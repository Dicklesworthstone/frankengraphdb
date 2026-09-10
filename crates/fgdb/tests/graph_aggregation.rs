//! Product aggregation over the actual durable and canonical overlay sources.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphPatternBuilder, GraphValue, GraphValueRow, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate,
    GraphAggregateError, GraphAggregateRow, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Summary = (VId, u64, u64, u64, Option<i128>, Option<i64>, Option<i64>);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}
fn child() -> PreparedGraphPattern<GraphValueRow> {
    let mut b = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] {
        b.vertex(name).unwrap();
    }
    b.edge("a", R, GlaDirection::Forward, "b").unwrap();
    b.edge("b", S, GlaDirection::Forward, "c").unwrap();
    b.prepare_values(
        &[
            GraphColumn::vertex("owner", "a"),
            GraphColumn::vertex("via", "b"),
            GraphColumn::property("amount", "c", P),
        ],
        0,
        None,
    )
    .unwrap()
    .with_duplicates()
}
fn aggregate(grouped: bool) -> PreparedGraphAggregate {
    PreparedGraphAggregate::prepare(
        child(),
        if grouped { &[0] } else { &[] },
        &[
            GraphAggregate::count_rows("paths"),
            GraphAggregate::count("present", 2),
            GraphAggregate::count_distinct("unique", 2),
            GraphAggregate::sum_int("sum", 2),
            GraphAggregate::min("min", 2),
            GraphAggregate::max("max", 2),
        ],
        0,
        None,
    )
    .unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut first = WriteBatch::new(R);
    for id in 1..=6 {
        first.create_vertex(
            VId(id),
            vec![],
            if id == 3 {
                vec![(P, CanonicalScalar::Int(7))]
            } else {
                vec![]
            },
        );
    }
    for (eid, owner) in [(10, 1), (11, 1), (12, 4)] {
        first.add_edge(EId(eid), VId(owner), VId(2), vec![]);
    }
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    for (eid, destination) in [(20, 3), (21, 3), (22, 5)] {
        second.add_edge(EId(eid), VId(2), VId(destination), vec![]);
    }
    db.write(cx, second).await.unwrap()
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            let extreme = |at| match row
                .get(at)
                .unwrap()
                .as_value()
                .and_then(GraphValue::as_scalar)
            {
                Some(CanonicalScalar::Int(value)) => Some(*value),
                Some(CanonicalScalar::Null) => None,
                other => panic!("unexpected fixture aggregate: {other:?}"),
            };
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_count().unwrap(),
                row.get(2).unwrap().as_count().unwrap(),
                row.get(3).unwrap().as_integer(),
                extreme(4),
                extreme(5),
            )
        })
        .collect()
}

// Independently materialize concrete qualifying pairs of ordinary owned rows.
// No GLA/compiler/aggregate state or column-by-column query results are used.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord]) -> Vec<Summary> {
    let mut groups: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
    for first in edges.iter().filter(|edge| edge.entry.relation == R) {
        for second in edges.iter().filter(|edge| edge.entry.relation == S) {
            if first.entry.dst != second.entry.src {
                continue;
            }
            let row = vertices
                .iter()
                .find(|row| row.vid == second.entry.dst)
                .unwrap();
            let value = row
                .props
                .iter()
                .find(|(key, _)| *key == P)
                .map(|(_, value)| value);
            let value = match value {
                None | Some(CanonicalScalar::Null) => None,
                Some(CanonicalScalar::Int(value)) => Some(*value),
                _ => panic!("noninteger in oracle fixture"),
            };
            groups.entry(first.entry.src).or_default().push(value);
        }
    }
    groups
        .into_iter()
        .map(|(key, values)| {
            let nonnull: Vec<_> = values.iter().filter_map(|value| *value).collect();
            (
                key,
                values.len() as u64,
                nonnull.len() as u64,
                nonnull.iter().copied().collect::<BTreeSet<_>>().len() as u64,
                (!nonnull.is_empty()).then(|| nonnull.iter().map(|value| i128::from(*value)).sum()),
                nonnull.iter().min().copied(),
                nonnull.iter().max().copied(),
            )
        })
        .collect()
}

#[test]
fn all_five_surfaces_share_the_summary_and_one_combined_allowance() {
    let ((), report) = run_async_under_lab(0xa660_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let aggregate = aggregate(true);
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap());
        assert_eq!(
            expected,
            vec![
                (VId(1), 6, 4, 1, Some(28), Some(7), Some(7)),
                (VId(4), 3, 2, 1, Some(14), Some(7), Some(7))
            ]
        );
        for rows in [
            db.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap()
                .value,
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap()
                .value,
            pinned
                .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap()
                .value,
            pinned
                .execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap()
                .value,
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
                .unwrap()
                .value,
        ] {
            assert_eq!(plain(&rows), expected);
        }
        let full = db
            .execute_graph_aggregate_governed(&cx, &aggregate, policy())
            .unwrap();
        assert_eq!(full.rows.snapshot_records, 6);
        assert_eq!(full.rows.result_rows, 2);
        let exact = GqlQueryPolicy::new(
            6,
            2,
            full.evaluator.work_units,
            full.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &aggregate, exact)
                .unwrap(),
            full
        );
        for cap in [
            GqlQueryPolicy::new(6, 2, full.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(6, 2, u64::MAX, full.evaluator.scratch_entries - 1),
        ] {
            assert!(
                matches!(db.execute_graph_aggregate_governed(&cx, &aggregate, cap),
                Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1)
            );
        }
        assert!(
            matches!(db.execute_graph_aggregate_governed(&cx, &aggregate,
            GqlQueryPolicy::new(6, 1, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 2)
        );
        assert!(
            matches!(db.execute_graph_aggregate_governed(&cx, &aggregate,
            GqlQueryPolicy::new(5, 2, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == 6)
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn canonical_staging_and_durable_history_keep_exact_counts_and_values() {
    let ((), report) = run_async_under_lab(0xa660_0002, |root| async move {
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
        let aggregate = aggregate(true);
        let before = db
            .execute_graph_aggregate_governed(&cx, &aggregate, policy())
            .unwrap()
            .value;
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.delete_edge(EId(10));
        stage.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        stage.add_edge(EId(13), VId(6), VId(2), vec![]);
        stage.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-2)));
        stage.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(3)));
        txn.write(&mut db, stage).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap());
        let staged = txn
            .execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
            .unwrap();
        assert_eq!(plain(&staged.value), expected);
        assert_ne!(staged.value, before);
        let exact = GqlQueryPolicy::new(
            staged.rows.snapshot_records,
            staged.rows.result_rows,
            staged.evaluator.work_units,
            staged.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_aggregate_governed(&db, &cx, &aggregate, exact)
                .unwrap(),
            staged
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap()
                .value,
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_aggregate_governed(&cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            db.execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap()
                .value,
            before
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            reopened
                .execute_graph_aggregate_governed_at(&cx, &aggregate, basis, policy())
                .unwrap()
                .value,
            before
        );
        assert_eq!(
            pinned
                .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap()
                .value,
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_count_and_refused_group_output_preserve_read_dependencies() {
    let ((), report) = run_async_under_lab(0xa660_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for empty in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            if !empty {
                seed(&mut db, &commit).await;
            }
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            let aggregate = aggregate(false);
            if empty {
                let zero = txn
                    .execute_graph_aggregate_governed(&db, &cx, &aggregate, policy())
                    .unwrap();
                assert_eq!(zero.value.len(), 1);
                assert_eq!(zero.value[0].get(0).unwrap().as_count(), Some(0));
                seed(&mut db, &commit).await;
            } else {
                assert!(matches!(
                    txn.execute_graph_aggregate_governed(
                        &db,
                        &cx,
                        &aggregate,
                        GqlQueryPolicy::new(100, 0, 1_000_000, 100_000)
                    ),
                    Err(GqlQueryError::Rows(_))
                ));
                // The middle vertex is not a group key or aggregate argument.
                let mut winner = WriteBatch::new(R);
                winner.set_vertex_property(VId(2), PropertyKeyId(9), Some(CanonicalScalar::Int(9)));
                db.write(&commit, winner).await.unwrap();
            }
            let frontier = db.frontier().unwrap();
            assert!(matches!(
                txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(99)).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn data_owner_snapshot_and_cancellation_errors_keep_their_domains() {
    let ((), report) = run_async_under_lab(0xa660_0004, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let txn_cx = contexts.txn();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let foreign = Database::open_memory(&commit, keys()).await.unwrap();
                let aggregate = aggregate(false);
                let mut wrong_type = WriteBatch::new(R);
                wrong_type.set_vertex_property(VId(3), P, Some(CanonicalScalar::Bool(true)));
                db.write(&commit, wrong_type).await.unwrap();
                assert!(matches!(
                    db.execute_graph_aggregate_governed(&cx, &aggregate, policy()),
                    Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum {
                        aggregate: 3
                    }))
                ));
                let txn = db.begin(&txn_cx).unwrap();
                root.cancel_with(CancelKind::User, Some("aggregate authority regression"));
                assert!(matches!(
                    txn.execute_graph_aggregate_governed(&foreign, &cx, &aggregate, policy()),
                    Err(GqlQueryError::Source(GraphAggregateError::Source(
                        WriteTxnError::WrongDatabase
                    )))
                ));
                let future = CommitSeq(db.frontier().unwrap().0 + 1);
                assert!(matches!(
                    db.execute_graph_aggregate_governed_at(&cx, &aggregate, future, policy()),
                    Err(GqlQueryError::Source(GraphAggregateError::Source(
                        GqlError::Read(ReadError::BeyondFrontier { .. })
                    )))
                ));
                assert!(matches!(
                    db.execute_graph_aggregate_governed(&cx, &aggregate, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
                txn.abort();
            })
            .expect("lab query task can be spawned");
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameterized_connected_text_feeds_aggregation_without_materializing_matches() {
    let ((), report) = run_async_under_lab(0xa660_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let template = PreparedGraphText::prepare(
            "MATCH (a)-[:R]->(b)-[:S]->(c) WHERE c.amount >= $min RETURN ALL a AS owner, c.amount AS amount",
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
                (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
                (GraphSymbolKind::Property, "amount") => Some(GraphSymbol::Property(P)),
                _ => None,
            },
        ).unwrap();
        for (minimum, expected_count, expected_sum) in [(0, 6, Some(42)), (10, 0, None)] {
            let input = template
                .bind_parameters(&GqlParameters::new().with_int64("min", minimum).unwrap())
                .unwrap();
            let aggregate = PreparedGraphAggregate::prepare(
                input,
                &[],
                &[
                    GraphAggregate::count_rows("paths"),
                    GraphAggregate::sum_int("total", 1),
                ],
                0,
                None,
            )
            .unwrap();
            let result = db
                .execute_graph_aggregate_governed(&cx, &aggregate, policy())
                .unwrap();
            assert_eq!(result.value.len(), 1);
            assert_eq!(
                result.value[0].get(0).unwrap().as_count(),
                Some(expected_count)
            );
            assert_eq!(result.value[0].get(1).unwrap().as_integer(), expected_sum);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
