//! DISTINCT is over completed visible tuples, before the ranked result page.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphAggregateTextSlot, GraphAggregateValue, GraphExactAverage, RelationBind,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::cmp::Ordering;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
type Edges = BTreeMap<u128, (u128, u128, Option<i64>)>;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("p", P)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn edges(cut: u64) -> Edges {
    let mut edges = Edges::from([
        (0, (0, 1, Some(2))),
        (1, (0, 1, Some(2))),
        (2, (0, 2, Some(1))),
        (3, (0, 2, Some(3))),
        (4, (0, 3, None)),
        (5, (0, 4, None)),
        (6, (0, 5, Some(1))),
        (7, (0, 5, Some(2))),
        (8, (0, 5, Some(3))),
        (u128::MAX, (0, 0, Some(6))),
    ]);
    if cut >= 2 {
        edges.remove(&0);
        edges.get_mut(&3).unwrap().2 = Some(7);
        edges.remove(&4);
    }
    if cut >= 3 {
        edges.insert(20, (5, u128::MAX, Some(4)));
        edges.insert(21, (2, 1, None));
        edges.get_mut(&6).unwrap().2 = Some(5);
    }
    edges
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in [0, 1, 2, 3, 4, 5, u128::MAX] {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    for (id, (a, b, value)) in edges(1) {
        batch.add_edge(
            EId(id),
            VId(a),
            VId(b),
            value
                .map(|v| vec![(P, CanonicalScalar::Int(v))])
                .unwrap_or_default(),
        );
    }
    batch
}
fn edit(cut: u64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    if cut == 2 {
        batch.delete_edge(EId(0));
        batch.set_edge_property(EId(3), P, Some(CanonicalScalar::Int(7)));
        batch.delete_vertex(VId(3)); // Existing native cascade retires edge 4.
    } else {
        batch.add_edge(
            EId(20),
            VId(5),
            VId(u128::MAX),
            vec![(P, CanonicalScalar::Int(4))],
        );
        batch.add_edge(EId(21), VId(2), VId(1), vec![]);
        batch.set_edge_property(EId(6), P, Some(CanonicalScalar::Int(5)));
    }
    batch
}
fn args(cut: u64, skip: u64, limit: u64) -> GqlParameters {
    GqlParameters::new()
        .with_uint64("cut", cut)
        .unwrap()
        .with_uint64("skip", skip)
        .unwrap()
        .with_uint64("limit", limit)
        .unwrap()
}
fn statement(direction: usize, aggregate: usize, descending: bool) -> String {
    let pattern = match direction {
        0 => "(a)-[r:R]->(b)",
        1 => "(a)<-[r:R]-(b)",
        _ => "(a)-[r:R]-(b)",
    };
    let aggregate = ["AVG(r.p)", "SUM(r.p)", "COUNT(*)", "COUNT(DISTINCT r.p)"][aggregate];
    let order = if descending { "DESC" } else { "ASC" };
    format!(
        "MATCH {pattern} FOR SYSTEM_TIME AS OF SEQ $cut RETURN DISTINCT {aggregate} AS value GROUP BY b ORDER BY value {order} NULLS LAST SKIP $skip LIMIT $limit"
    )
}
fn projected(
    row: &GraphAggregateRow,
    slots: &[GraphAggregateTextSlot],
) -> Vec<GraphAggregateValue> {
    slots
        .iter()
        .map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => {
                GraphAggregateValue::Value(row.keys()[at].clone())
            }
            GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
        })
        .collect()
}
// Independent occurrence arithmetic and rational comparison. Neither the
// batch engine, streamed comparator nor DISTINCT normalization is the oracle.
fn oracle(
    cut: u64,
    direction: usize,
    aggregate: usize,
    descending: bool,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphAggregateValue>> {
    let mut groups = BTreeMap::<u128, Vec<Option<i64>>>::new();
    for (_, (a, b, value)) in edges(cut) {
        match direction {
            1 => groups.entry(a).or_default().push(value),
            2 => {
                groups.entry(b).or_default().push(value);
                if a != b {
                    groups.entry(a).or_default().push(value);
                }
            }
            _ => groups.entry(b).or_default().push(value),
        }
    }
    let null = || GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|(key, values)| {
            let nonnull: Vec<_> = values.iter().flatten().copied().collect();
            let total: i128 = nonnull.iter().map(|v| i128::from(*v)).sum();
            let (rank, value) = match aggregate {
                0 if !nonnull.is_empty() => (
                    Some((total, nonnull.len() as i128)),
                    GraphAggregateValue::Average(
                        GraphExactAverage::new(total, nonnull.len() as u64).unwrap(),
                    ),
                ),
                1 if !nonnull.is_empty() => (Some((total, 1)), GraphAggregateValue::Integer(total)),
                2 => (
                    Some((values.len() as i128, 1)),
                    GraphAggregateValue::Count(values.len() as u64),
                ),
                3 => {
                    let count = nonnull
                        .into_iter()
                        .collect::<std::collections::BTreeSet<_>>()
                        .len();
                    (
                        Some((count as i128, 1)),
                        GraphAggregateValue::Count(count as u64),
                    )
                }
                _ => (None, null()),
            };
            (key, rank, value)
        })
        .collect();
    let numeric = |a: Option<(i128, i128)>, b: Option<(i128, i128)>| match (a, b) {
        (None, None) => Ordering::Equal,
        (None, _) => Ordering::Greater,
        (_, None) => Ordering::Less,
        (Some((a, da)), Some((b, db))) => {
            let order = (a * db).cmp(&(b * da));
            if descending { order.reverse() } else { order }
        }
    };
    rows.sort_by(|a, b| numeric(a.1, b.1).then_with(|| a.0.cmp(&b.0)));
    rows.dedup_by(|a, b| numeric(a.1, b.1) == Ordering::Equal);
    rows.into_iter()
        .skip(skip)
        .take(limit)
        .map(|(_, _, value)| vec![value])
        .collect()
}

#[test]
fn distinct_pages_follow_full_group_semantics_across_history_directions_and_exact_statistics() {
    let ((), report) = run_async_under_lab(0xd157_7101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, seed()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let text = statement(0, 0, true);
        let parameters = args(1, 0, 10);
        let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols()).unwrap();
        let mut paused = prepared
            .stream_aggregate(&db, &cx, &parameters, wide())
            .unwrap();
        let slots = paused.output_slots().to_vec();
        let first = projected(&paused.next().unwrap().unwrap(), &slots);
        drop(prepared);
        drop(parameters);
        for cut in [2, 3] {
            db.write(&commit, edit(cut)).await.unwrap();
        }
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(paused.snapshot_seq(), CommitSeq(1));
        let mut old = vec![first];
        old.extend(paused.by_ref().map(|row| projected(&row.unwrap(), &slots)));
        assert_eq!(old, oracle(1, 0, 0, true, 0, 10));
        assert_eq!(paused.state(), VertexScanState::Exhausted);
        assert!(
            pinned
                .query_aggregate_stream(&cx, &text, &args(2, 0, 10), symbols(), wide())
                .is_err()
        );
        for cut in 1..=3 {
            for direction in 0..3 {
                for aggregate in 0..4 {
                    for descending in [false, true] {
                        for (skip, limit) in [(0, 0), (0, 1), (1, 2), (0, 10)] {
                            let text = statement(direction, aggregate, descending);
                            let args = args(cut, skip, limit);
                            let expected = oracle(
                                cut,
                                direction,
                                aggregate,
                                descending,
                                skip as usize,
                                limit as usize,
                            );
                            let mut cursor = db
                                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                                .unwrap();
                            assert_eq!(cursor.kind(), ScanKind::Edge);
                            assert_eq!(cursor.row_stats().snapshot_records, 0);
                            let layout = cursor.output_slots().to_vec();
                            let actual: Vec<_> = cursor
                                .by_ref()
                                .map(|row| projected(&row.unwrap(), &layout))
                                .collect();
                            assert_eq!(actual, expected, "{text} at {cut}");
                            assert_eq!(cursor.row_stats().result_rows, expected.len() as u64);
                            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
                            assert!(cursor.next().is_none());
                            let QueryResult::Rows { rows, .. } =
                                db.query(&cx, &text, &args, symbols(), wide()).unwrap()
                            else {
                                panic!("read expected")
                            };
                            assert_eq!(rows, expected);
                        }
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_support_and_rank_prefix_do_not_spend_selected_row_quota_and_close_does_not_drain() {
    let ((), report) = run_async_under_lab(0xd157_7102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(0), vec![], vec![]);
        for id in 1..=256 {
            batch.create_vertex(VId(id), vec![], vec![]);
            batch.add_edge(
                EId(id),
                VId(0),
                VId(id),
                vec![(P, CanonicalScalar::Int(if id == 256 { -1 } else { 7 }))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) RETURN DISTINCT SUM(r.p) AS total GROUP BY b ORDER BY total DESC SKIP 1 LIMIT 1";
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let expected = full.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].values(), &[GraphAggregateValue::Integer(-1)]);
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries);
        assert_eq!(
            prepared
                .stream_aggregate(&db, &cx, &args, exact)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected
        );
        for policy in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &args, policy).unwrap();
            assert!(cursor.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut unopened = prepared.stream_aggregate(&db, &cx, &args, exact).unwrap();
        unopened.close();
        assert!(unopened.next().is_none());
        assert_eq!(unopened.row_stats().snapshot_records, 0);
        let text =
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT SUM(r.p) AS total GROUP BY b ORDER BY total DESC";
        let mut cursor = db
            .query_aggregate_stream(&cx, text, &args, symbols(), wide())
            .unwrap();
        cursor.next().unwrap().unwrap();
        let usage = (cursor.row_stats(), cursor.evaluator_stats());
        cursor.close();
        cursor.close();
        assert!(cursor.next().is_none());
        assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), usage);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_native_layouts_keep_hidden_ranks_repeated_cells_and_concrete_representatives() {
    let ((), report) = run_async_under_lab(0xd157_7103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n GROUP BY b ORDER BY SUM(r.p) DESC NULLS LAST",
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT AVG(r.p) AS first,AVG(r.p) AS again GROUP BY b ORDER BY first DESC",
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT b AS first,b AS again,COUNT(*) AS n GROUP BY b ORDER BY n DESC",
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*)+1 AS n GROUP BY b ORDER BY SUM(r.p) DESC LIMIT 3",
            "MATCH p=(a)-[r:R]->(b) RETURN DISTINCT MIN(p) AS path,COUNT(DISTINCT r) AS n GROUP BY b ORDER BY n DESC",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN DISTINCT AVG(DISTINCT r.p) AS average GROUP BY c ORDER BY average DESC",
            "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:R]->(c) } RETURN DISTINCT COUNT(*) AS n GROUP BY b ORDER BY n DESC",
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n GROUP BY b HAVING n>=2 ORDER BY n DESC",
        ] {
            let QueryResult::Rows { columns, rows } =
                db.query(&cx, text, &args, symbols(), wide()).unwrap()
            else {
                panic!("read expected")
            };
            let mut cursor = db
                .query_aggregate_stream(&cx, text, &args, symbols(), wide())
                .unwrap();
            assert_eq!(cursor.columns(), columns);
            let slots = cursor.output_slots().to_vec();
            assert_eq!(
                cursor
                    .by_ref()
                    .map(|row| projected(&row.unwrap(), &slots))
                    .collect::<Vec<_>>(),
                rows,
                "{text}"
            );
        }
        // Both hidden groups produce numeric 2, but one is a COUNT and the
        // other an integer SUM. The winning hidden rank chooses the variant.
        let mut empty = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..3 {
            batch.create_vertex(VId(id), vec![], vec![]);
        }
        batch.add_edge(EId(1), VId(0), VId(1), vec![]);
        batch.add_edge(EId(2), VId(0), VId(1), vec![]);
        batch.add_edge(EId(3), VId(0), VId(2), vec![(P, CanonicalScalar::Int(2))]);
        empty.write(&commit, batch).await.unwrap();
        for (direction, winner) in [
            ("ASC", GraphAggregateValue::Integer(2)),
            ("DESC", GraphAggregateValue::Count(2)),
        ] {
            let text = format!(
                "MATCH (a)-[r:R]->(b) RETURN DISTINCT COALESCE(SUM(r.p),COUNT(*)) AS chosen GROUP BY b ORDER BY COUNT(*) {direction}"
            );
            let rows = empty
                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values(), &[winner]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_pages_and_duplicate_losers_cannot_hide_source_or_expression_errors() {
    let ((), report) = run_async_under_lab(0xd157_7104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let args = GqlParameters::new();
        let rows = db
            .query_aggregate_stream(
                &cx,
                "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n",
                &args,
                symbols(),
                wide(),
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values(), &[GraphAggregateValue::Count(0)]);
        let grouped =
            "MATCH (a)-[r:R]->(b) RETURN DISTINCT COUNT(*) AS n GROUP BY b ORDER BY n DESC";
        assert!(
            db.query_aggregate_stream(&cx, grouped, &args, symbols(), wide())
                .unwrap()
                .next()
                .is_none()
        );
        db.write(&commit, seed()).await.unwrap();
        for limit in [0, 1] {
            let text = format!(
                "MATCH (a)-[r:R]->(b) RETURN DISTINCT 1/(COUNT(*)-1) AS n GROUP BY b ORDER BY COUNT(*) DESC LIMIT {limit}"
            );
            let mut cursor = db
                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                .unwrap();
            assert!(matches!(
                cursor.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { .. }
                )))
            ));
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        let mut bad = WriteBatch::new(R);
        bad.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, bad).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) RETURN DISTINCT SUM(r.p) AS total GROUP BY b ORDER BY total LIMIT 0";
        let mut cursor = db
            .query_aggregate_stream(&cx, text, &args, symbols(), wide())
            .unwrap();
        assert!(matches!(
            cursor.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerSum { .. }
            )))
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert!(cursor.next().is_none());
        let future = statement(0, 0, true);
        assert!(
            db.query_aggregate_stream(&cx, &future, &crate::args(3, 0, 0), symbols(), wide())
                .is_err()
        );
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
