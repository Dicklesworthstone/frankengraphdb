//! Grouped edge statistics use the actual native dispatch and pinned database.
//! No replacement graph source or test-only aggregate execution path is used.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryError, QueryResult, QueryValue,
    WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphAggregateTextSlot, RelationBind,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
        .with_property("p", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn seed() -> Vec<WriteBatch> {
    let mut first = WriteBatch::new(R);
    for id in [0, 1, 2, 99, u128::MAX] {
        first.create_vertex(
            VId(id),
            vec![],
            vec![(
                P,
                if id == 1 {
                    CanonicalScalar::Null
                } else {
                    CanonicalScalar::Int(2)
                },
            )],
        );
    }
    for (eid, a, b, w) in [
        (1, 0, 1, Some(5)),
        (2, 0, 1, Some(5)),
        (3, 2, 0, Some(-2)),
        (4, 1, 1, None),
        (5, u128::MAX, 0, Some(7)),
        (6, 0, 0, Some(i64::MAX)),
    ] {
        first.add_edge(
            EId(eid),
            VId(a),
            VId(b),
            w.into_iter()
                .map(|w| (P, CanonicalScalar::Int(w)))
                .collect(),
        );
    }
    let mut second = WriteBatch::new(S);
    for (eid, a, b, w) in [
        (10, 1, 2, Some(3)),
        (11, 1, 1, None),
        (12, 0, u128::MAX, Some(4)),
    ] {
        second.add_edge(
            EId(eid),
            VId(a),
            VId(b),
            w.into_iter()
                .map(|w| (P, CanonicalScalar::Int(w)))
                .collect(),
        );
    }
    vec![first, second]
}
fn definition(shape: usize, direction: usize) -> String {
    let edge = |name, relation, end| match direction {
        0 => format!("-[{name}:{relation}]->({end})"),
        1 => format!("<-[{name}:{relation}]-({end})"),
        _ => format!("-[{name}:{relation}]-({end})"),
    };
    let mut pattern = format!("(a){}", edge("r", "R", "b"));
    if shape > 0 {
        pattern.push_str(&edge("s", "S", "c"));
    }
    if shape > 1 {
        pattern.push_str(&edge("t", "R", "a"));
    }
    let end = if shape == 0 { "b" } else { "c" };
    format!(
        "MATCH {pattern} RETURN COUNT(*) AS n,{end} AS destination,SUM(r.p) AS total,{end} AS again,AVG(r.p) AS mean,COUNT(DISTINCT r.p) AS unique_values,MIN(r) AS first_edge GROUP BY {end}"
    )
}
fn flatten(row: &GraphAggregateRow, slots: &[GraphAggregateTextSlot]) -> Vec<QueryValue> {
    slots
        .iter()
        .map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => QueryValue::Value(row.keys()[at].clone()),
            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
        })
        .collect()
}
fn eager_rows(result: QueryResult) -> (Vec<String>, Vec<Vec<QueryValue>>) {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("query returned write");
    };
    (columns, rows)
}

#[test]
fn grouped_native_layout_and_statistics_survive_writes_compaction_reopen_and_pinned_history() {
    let ((), report) = run_async_under_lab(0xe66e_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write_atomic(&commit, seed()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let mut cases = Vec::new();
        for shape in 0..3 {
            for direction in 0..3 {
                let text = definition(shape, direction);
                let args = GqlParameters::new();
                let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
                let (columns, expected) =
                    eager_rows(prepared.execute(&db, &cx, &args, wide()).unwrap());
                let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
                fn send(_: &impl Send) {}
                send(&cursor);
                assert_eq!(cursor.kind(), ScanKind::Edge);
                assert_eq!(cursor.columns(), columns);
                assert_eq!(cursor.key_columns(), &["destination"]);
                assert_eq!(
                    cursor.output_slots()[1],
                    GraphAggregateTextSlot::GroupKey(0)
                );
                assert_eq!(
                    cursor.output_slots()[3],
                    GraphAggregateTextSlot::GroupKey(0)
                );
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                let slots = cursor.output_slots().to_vec();
                let first = cursor.next().transpose().unwrap();
                if let Some(first) = &first {
                    assert_eq!(flatten(first, &slots), expected[0]);
                }
                // Neither text/template/arguments nor database is borrowed by the cursor.
                drop(prepared);
                drop(args);
                cases.push((text, slots, expected, first, cursor));
            }
        }
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(1), P, Some(CanonicalScalar::Int(-9)));
        edit.delete_edge(EId(2));
        edit.add_edge(
            EId(100),
            VId(u128::MAX),
            VId(1),
            vec![(P, CanonicalScalar::Int(2))],
        );
        db.write(&commit, edit).await.unwrap();
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (text, slots, expected, first, mut cursor) in cases {
            let mut old: Vec<_> = first.iter().map(|row| flatten(row, &slots)).collect();
            old.extend(cursor.by_ref().map(|row| flatten(&row.unwrap(), &slots)));
            assert_eq!(old, expected);
            assert_eq!(cursor.snapshot_seq(), basis);
            assert_eq!(cursor.row_stats().result_rows, old.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            let args = GqlParameters::new();
            let mut from_pin = pinned
                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                .unwrap();
            assert_eq!(
                from_pin
                    .by_ref()
                    .map(|row| flatten(&row.unwrap(), &slots))
                    .collect::<Vec<_>>(),
                expected
            );
            let historical =
                text.replacen(" RETURN ", " FOR SYSTEM_TIME AS OF SEQ $seq RETURN ", 1);
            for seq in 0..=3 {
                let args = GqlParameters::new().with_uint64("seq", seq).unwrap();
                let (names, expected) = eager_rows(
                    db.query(&cx, &historical, &args, symbols(), wide())
                        .unwrap(),
                );
                let mut stream = db
                    .query_aggregate_stream(&cx, &historical, &args, symbols(), wide())
                    .unwrap();
                assert_eq!(stream.snapshot_seq(), CommitSeq(seq));
                assert_eq!(stream.columns(), names);
                let layout = stream.output_slots().to_vec();
                assert_eq!(
                    stream
                        .by_ref()
                        .map(|row| flatten(&row.unwrap(), &layout))
                        .collect::<Vec<_>>(),
                    expected
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn grouped_native_budgets_late_data_refusals_and_nondraining_close_keep_original_boundaries() {
    let ((), report) = run_async_under_lab(0xe66e_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write_atomic(&commit, seed()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let text = definition(0, 0);
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert!(expected.len() > 1);
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        assert_eq!(
            prepared
                .stream_aggregate(&db, &cx, &args, exact)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            expected
        );
        for p in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1000, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, r.result_rows - 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(1000, 1000, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, p).unwrap();
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("quota became a different outcome: {other:?}"),
                }
            }
            assert_eq!(prefix, expected[..prefix.len()]);
            assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        for pulled in [false, true] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
            if pulled {
                cursor.next().unwrap().unwrap();
            }
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            cursor.close();
            cursor.close();
            assert!(cursor.next().is_none());
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
            if !pulled {
                assert_eq!(stats.0.snapshot_records, 0);
                assert_eq!(stats.1.work_units, 0);
            }
        }
        let temporal = text.replacen(" RETURN ", " FOR SYSTEM_TIME AS OF SEQ $seq RETURN ", 1);
        let future = GqlParameters::new()
            .with_uint64("seq", basis.0 + 1)
            .unwrap();
        assert!(matches!(
            pinned.query_aggregate_stream(
                &cx,
                &temporal,
                &future,
                symbols(),
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(QueryError::EdgeAggregateStream(GqlQueryError::Source(_)))
        ));
        let mut invalid = WriteBatch::new(R);
        invalid.set_edge_property(
            EId(6),
            P,
            Some(CanonicalScalar::ucs_basic_text("private invalid sum").unwrap()),
        );
        db.write(&commit, invalid).await.unwrap();
        let mut failed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        assert!(matches!(
            failed.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { aggregate: 1 }
            )))
        ));
        assert_eq!(failed.row_stats().result_rows, 0);
        assert!(failed.next().is_none());
        assert!(!format!("{failed:?}").contains("private invalid sum"));
        assert_eq!(
            prepared
                .stream_aggregate_in_view(&pinned, &cx, &args, wide())
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_group_count_not_input_occurrences_controls_output_allowance() {
    let ((), report) = run_async_under_lab(0xe66e_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..4 {
            batch.create_vertex(VId(id), vec![], vec![]);
        }
        for id in 0..4096 {
            batch.add_edge(
                EId(id),
                VId(0),
                VId(id % 4),
                vec![(P, CanonicalScalar::Int(i64::MAX))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let q = "MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n,SUM(r.p) AS total,AVG(r.p) AS mean,COUNT(DISTINCT r.p) AS unique_values GROUP BY b";
        let mut cursor = db
            .query_aggregate_stream(
                &cx,
                q,
                &GqlParameters::new(),
                symbols(),
                GqlQueryPolicy::new(4096, 4, 10_000_000, 10_000_000),
            )
            .unwrap();
        for id in 0..4 {
            let row = cursor.next().unwrap().unwrap();
            assert_eq!(
                row.keys(),
                &[fgdb_gql::algebra::GraphValue::Vertex(VId(id))]
            );
            assert_eq!(row.values()[0].as_count(), Some(1024));
            assert_eq!(
                row.values()[1].as_integer(),
                Some(1024 * i128::from(i64::MAX))
            );
            assert_eq!(
                row.values()[2],
                QueryValue::Average(
                    fgdb_gql::GraphExactAverage::new(i128::from(i64::MAX), 1).unwrap()
                )
            );
            assert_eq!(row.values()[3].as_count(), Some(1));
            assert_eq!(cursor.row_stats().result_rows, id as u64 + 1);
        }
        assert_eq!(cursor.row_stats().snapshot_records, 4096);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
