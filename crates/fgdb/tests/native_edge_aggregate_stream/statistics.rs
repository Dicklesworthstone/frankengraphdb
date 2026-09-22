//! The native facade must expose the extended typed operator without fallback.
use super::*;
use fgdb_gql::GraphExactAverage;

const STATISTICS: &str = "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(*) AS total,\
    COUNT(DISTINCT r.score) AS distinct_values,SUM(DISTINCT r.score) AS distinct_sum,\
    AVG(r.score) AS average,AVG(DISTINCT r.score) AS distinct_average,\
    MIN(r) AS first_edge,MAX(r) AS last_edge,COUNT(DISTINCT a) AS roots";

#[test]
fn native_statistics_match_eager_across_parameters_directions_and_captured_paths() {
    let ((), report) = run_async_under_lab(0xa66e_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        for text in [
            STATISTICS,
            "MATCH (a)<-[r:R]-(b) RETURN AVG(DISTINCT r.score) AS average,COUNT(DISTINCT r) AS edges,MIN(r.score) AS lo,MAX(r.score) AS hi",
            "MATCH (a)-[r:R]-(b) RETURN AVG(r.score) AS average,SUM(DISTINCT r.score) AS distinct_sum,COUNT(DISTINCT a) AS vertices",
            "MATCH p=(a)-[:R]->(b)-[:R]->(c) RETURN COUNT(DISTINCT p) AS paths,MIN(p) AS first,MAX(p) AS last",
            "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) WHERE c.score>0 } RETURN AVG(DISTINCT r.score) AS average,MIN(r) AS first",
        ] {
            let args = GqlParameters::new();
            let QueryResult::Rows { columns, rows } =
                db.query(&cx, text, &args, symbols(), wide()).unwrap()
            else {
                panic!("expected read rows");
            };
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &args, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            assert_eq!(cursor.columns(), columns);
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            assert_eq!(
                rows,
                vec![cursor.next().unwrap().unwrap().values().to_vec()]
            );
            assert!(cursor.next().is_none());
        }
        let text = "MATCH (a)-[r:R]->(b) WHERE r.score>=$floor RETURN AVG(DISTINCT r.score) AS mean,SUM(DISTINCT r.score) AS sum,COUNT(DISTINCT r.score) AS count";
        let args = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        for (floor, count, total) in [(-100, 4, Some(0)), (0, 2, Some(6)), (100, 0, None)] {
            let args = GqlParameters::new().with_int64("floor", floor).unwrap();
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            let row = cursor.next().unwrap().unwrap();
            assert_eq!(row.values()[1].as_integer(), total);
            assert_eq!(row.values()[2].as_count(), Some(count));
            assert_eq!(
                row.values()[0].as_average(),
                total.and_then(|sum| GraphExactAverage::new(sum, count))
            );
            if count == 0 {
                assert!(row.values()[0].is_null());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_history_and_unique_value_state_survive_writer_compaction_and_drop() {
    let ((), report) = run_async_under_lab(0xa66e_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = fgdb::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $cut RETURN AVG(DISTINCT r.score) AS mean,SUM(DISTINCT r.score) AS sum,MAX(r) AS last";
        let args = GqlParameters::new().with_uint64("cut", 1).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut old = prepared
            .stream_aggregate_in_view(&view, &cx, &args, wide())
            .unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Int(5)));
        let latest = db.write(&commit, edit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for cut in 0..=latest.0 {
            let args = GqlParameters::new().with_uint64("cut", cut).unwrap();
            let QueryResult::Rows { rows, .. } = prepared.execute(&db, &cx, &args, wide()).unwrap()
            else {
                panic!("expected read rows");
            };
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(
                rows,
                vec![cursor.next().unwrap().unwrap().values().to_vec()]
            );
        }
        let future = GqlParameters::new().with_uint64("cut", latest.0).unwrap();
        assert!(matches!(
            prepared.stream_aggregate_in_view(&view, &cx, &future, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::EdgeAggregateStream(_))
        ));
        let mut current = prepared
            .stream_aggregate(&db, &cx, &future, wide())
            .unwrap();
        drop(view);
        drop(db);
        drop(args);
        drop(future);
        drop(prepared);
        assert_eq!(old.row_stats().snapshot_records, 0);
        let row = old.next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_average(), GraphExactAverage::new(0, 4));
        assert_eq!(row.values()[1].as_integer(), Some(0));
        assert_eq!(
            row.values()[2].as_value(),
            Some(&GraphValue::Edge(EId(u128::MAX)))
        );
        let row = current.next().unwrap().unwrap();
        assert_eq!(row.values()[0].as_average(), GraphExactAverage::new(4, 3));
        assert_eq!(row.values()[1].as_integer(), Some(4));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn extended_statistics_preserve_exact_quotas_typed_late_failures_and_nondraining_close() {
    let ((), report) = run_async_under_lab(0xa66e_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(STATISTICS, &args, symbols()).unwrap();
        let mut baseline = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let result = baseline.next().unwrap().unwrap();
        let records = baseline.row_stats().snapshot_records;
        let work = baseline.evaluator_stats();
        let exact = GqlQueryPolicy::new(records, 1, work.work_units, work.scratch_entries);
        assert_eq!(
            prepared
                .stream_aggregate(&db, &cx, &args, exact)
                .unwrap()
                .next()
                .unwrap()
                .unwrap(),
            result
        );
        for p in [
            GqlQueryPolicy::new(records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, work.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, work.scratch_entries - 1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, p).unwrap();
            assert!(cursor.next().unwrap().is_err());
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        cursor.close();
        assert!(cursor.next().is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.evaluator_stats().work_units, 0);
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(
            EId(u128::MAX),
            P,
            Some(CanonicalScalar::ucs_basic_text("private invalid number").unwrap()),
        );
        db.write(&commit, edit).await.unwrap();
        let text =
            "MATCH (a)-[r:R]->(b) RETURN COUNT(DISTINCT r) AS count,AVG(DISTINCT r.score) AS mean";
        let mut cursor = db
            .query_aggregate_stream(&cx, text, &args, symbols(), wide())
            .unwrap();
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerAverage { aggregate: 1 }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(!format!("{cursor:?}").contains("private invalid"));
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
