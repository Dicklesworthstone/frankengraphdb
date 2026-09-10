//! Filtering/ranking of completed groups on actual snapshot and overlay reads.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateRow, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const TEXT: &str = "MATCH (owner)-[:R]->(mid)-[:S]->(item) \
    RETURN COUNT(*) AS paths, owner, SUM(item.p) AS total, COUNT(item.p) AS present \
    GROUP BY owner HAVING paths >= $minimum AND total IS NOT NULL \
    ORDER BY total DESC NULLS LAST, owner ASC SKIP $off LIMIT $take";
type Summary = (VId, u64, i128, u64);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 100_000)
}
fn arguments(minimum: i64, off: u64, take: u64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("minimum", minimum)
        .unwrap()
        .with_uint64("off", off)
        .unwrap()
        .with_uint64("take", take)
        .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut first = WriteBatch::new(R);
    for id in [1, 2, 3, 4, 5, 6, 7, 8, 90] {
        let props = match id {
            3 => vec![(P, CanonicalScalar::Int(7))],
            7 => vec![(P, CanonicalScalar::Int(50))],
            _ => vec![],
        };
        first.create_vertex(VId(id), vec![], props);
    }
    for (id, source, target) in [(10, 1, 2), (11, 1, 2), (12, 4, 5), (13, 6, 2)] {
        first.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    for (id, source, target) in [(20, 2, 3), (21, 2, 3), (22, 2, 8), (23, 5, 7)] {
        second.add_edge(EId(id), VId(source), VId(target), vec![]);
    }
    db.write(cx, second).await.unwrap()
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_integer().unwrap(),
                row.get(2).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}

// Materialize concrete path occurrences independently from ordinary owned
// storage rows, then use the standard full sort. No aggregate engine, GLA
// lowering, column-by-column query or bounded selection heap is reused.
fn oracle(
    vertices: &[VertexRow],
    edges: &[EdgeRecord],
    minimum: u64,
    off: usize,
    take: usize,
) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
    for left in edges.iter().filter(|edge| edge.entry.relation == R) {
        for right in edges
            .iter()
            .filter(|edge| edge.entry.relation == S && edge.entry.src == left.entry.dst)
        {
            let item = vertices
                .iter()
                .find(|row| row.vid == right.entry.dst)
                .unwrap();
            let value = item
                .props
                .iter()
                .find(|(key, _)| *key == P)
                .map(|(_, value)| value);
            let value = match value {
                None | Some(CanonicalScalar::Null) => None,
                Some(CanonicalScalar::Int(value)) => Some(*value),
                _ => panic!("noninteger in the independent fixture"),
            };
            groups.entry(left.entry.src).or_default().push(value);
        }
    }
    let mut rows: Vec<_> = groups
        .into_iter()
        .filter_map(|(key, values)| {
            let nonnull: Vec<_> = values.iter().filter_map(|value| *value).collect();
            (values.len() as u64 >= minimum && !nonnull.is_empty()).then(|| {
                (
                    key,
                    values.len() as u64,
                    nonnull.iter().map(|value| i128::from(*value)).sum::<i128>(),
                    nonnull.len() as u64,
                )
            })
        })
        .collect();
    rows.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
    rows.into_iter().skip(off).take(take).collect()
}

#[test]
fn all_five_sources_rank_before_slicing_and_use_one_combined_policy() {
    let ((), report) = run_async_under_lab(0xa680_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let template = PreparedGraphAggregateText::prepare(TEXT, symbols).unwrap();
        let query = template.bind_parameters(&arguments(1, 0, 2)).unwrap();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 1, 0, 2);
        assert_eq!(expected, vec![(VId(4), 1, 50, 1), (VId(1), 6, 28, 4)]);
        for value in [
            db.execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value,
            db.execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                .unwrap()
                .value,
            pinned
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value,
            pinned
                .execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                .unwrap()
                .value,
            txn.execute_graph_aggregate_governed(&db, &cx, &query, policy())
                .unwrap()
                .value,
        ] {
            assert_eq!(plain(&value), expected);
        }
        let full = db
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap();
        assert_eq!(full.rows.snapshot_records, 8);
        let exact = GqlQueryPolicy::new(
            8,
            2,
            full.evaluator.work_units,
            full.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &query, exact)
                .unwrap(),
            full
        );
        for cap in [
            GqlQueryPolicy::new(8, 2, full.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(8, 2, u64::MAX, full.evaluator.scratch_entries - 1),
        ] {
            assert!(
                matches!(db.execute_graph_aggregate_governed(&cx, &query, cap),
                Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1)
            );
        }
        assert!(
            matches!(db.execute_graph_aggregate_governed(&cx, &query, GqlQueryPolicy::new(8, 1, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 2)
        );
        let page = template.bind_parameters(&arguments(2, 1, 1)).unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_aggregate_governed(&cx, &page, policy())
                    .unwrap()
                    .value
            ),
            vec![(VId(6), 3, 14, 2)]
        );
        let zero = template.bind_parameters(&arguments(999, 0, 1)).unwrap();
        assert!(
            db.execute_graph_aggregate_governed(&cx, &zero, policy())
                .unwrap()
                .value
                .is_empty()
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn canonical_staging_changes_rank_while_history_and_pinned_generations_keep_old_order() {
    let ((), report) = run_async_under_lab(0xa680_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
            .unwrap()
            .bind_parameters(&arguments(1, 0, 2))
            .unwrap();
        let old = db
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap()
            .value;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.delete_edge(EId(10));
        stage.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        stage.add_edge(EId(14), VId(6), VId(2), vec![]);
        stage.set_vertex_property(VId(7), P, Some(CanonicalScalar::Int(1)));
        txn.write(&mut db, stage).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(
            &txn.vertices(&db).unwrap(),
            &txn.edges(&db).unwrap(),
            1,
            0,
            2,
        );
        assert_eq!(expected, vec![(VId(6), 6, 28, 4), (VId(1), 3, 14, 2)]);
        let staged = txn
            .execute_graph_aggregate_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(plain(&staged.value), expected);
        let exact = GqlQueryPolicy::new(
            staged.rows.snapshot_records,
            staged.rows.result_rows,
            staged.evaluator.work_units,
            staged.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_aggregate_governed(&db, &cx, &query, exact)
                .unwrap(),
            staged
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value,
            old
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            reopened
                .execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                .unwrap()
                .value,
            old
        );
        assert_eq!(
            pinned
                .execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value,
            old
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_and_unselected_groups_keep_dependencies_but_disjoint_writes_remain_admissible() {
    let ((), report) = run_async_under_lab(0xa680_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for minimum in [1, 999] {
            for conflict in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
                    .unwrap()
                    .bind_parameters(&arguments(minimum, 0, 1))
                    .unwrap();
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(R);
                stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let result = txn
                    .execute_graph_aggregate_governed(&db, &cx, &query, policy())
                    .unwrap();
                if minimum == 1 {
                    assert_eq!(result.value[0].keys()[0].as_vertex(), Some(VId(4)));
                } else {
                    assert!(result.value.is_empty());
                }
                // item 3 belongs only to groups that were not returned. Changing
                // it can affect a later ranking even though top-1 came from 4.
                let mut winner = WriteBatch::new(R);
                winner.set_vertex_property(
                    if conflict { VId(3) } else { VId(90) },
                    P,
                    Some(CanonicalScalar::Int(1000)),
                );
                db.write(&commit, winner).await.unwrap();
                let frontier = db.frontier().unwrap();
                let committed = txn.commit(&mut db, &commit).await;
                if conflict {
                    assert!(matches!(
                        committed,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert_eq!(db.frontier().unwrap(), frontier);
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                } else {
                    committed.unwrap();
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ranked_empty_counts_and_owner_future_cancellation_errors_retain_their_meaning() {
    let ((), report) = run_async_under_lab(0xa680_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let query = PreparedGraphAggregateText::prepare(
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n HAVING n > 0 ORDER BY n DESC",
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(
            db.execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value
                .is_empty()
        );
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, stage).unwrap();
        assert!(
            txn.execute_graph_aggregate_governed(&db, &cx, &query, policy())
                .unwrap()
                .value
                .is_empty()
        );
        seed(&mut db, &commit).await;
        assert!(matches!(
            txn.commit(&mut db, &commit).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        root.cancel_with(CancelKind::User, Some("ranked aggregate error ordering"));
        assert!(matches!(
            txn.execute_graph_aggregate_governed(&foreign, &cx, &query, policy()),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                WriteTxnError::WrongDatabase
            )))
        ));
        let future = CommitSeq(db.frontier().unwrap().0 + 1);
        assert!(matches!(
            db.execute_graph_aggregate_governed_at(&cx, &query, future, policy()),
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                GqlError::Read(ReadError::BeyondFrontier { .. })
            )))
        ));
        assert!(matches!(
            db.execute_graph_aggregate_governed(&cx, &query, policy()),
            Err(GqlQueryError::Interrupted(_))
        ));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
