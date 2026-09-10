//! Aggregate text executes through the existing durable and overlay machinery.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphAggregateTextSlot, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const TEXT: &str = "MATCH (owner)-[:R]->(bridge)-[:S]->(item) WHERE bridge.n >= $floor \
    RETURN COUNT(*) AS paths,owner AS owner,SUM(item.n) AS total,COUNT(item.n) AS present,\
    COUNT(DISTINCT item.n) AS different,MIN(item.n) AS least,MAX(item.n) AS greatest \
    GROUP BY owner SKIP $offset LIMIT $take";
type Summary = (VId, u64, Option<i128>, u64, u64, Option<i64>, Option<i64>);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x45; 32],
        DatabaseSecurityNamespaceId([0x46; 32]),
        [0x47; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn args(floor: i64, offset: u64, take: u64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("floor", floor)
        .unwrap()
        .with_uint64("offset", offset)
        .unwrap()
        .with_uint64("take", take)
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 100_000)
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut entities = WriteBatch::new(RelationId(9));
    for id in 1..=6 {
        entities.create_vertex(
            VId(id),
            vec![],
            match id {
                2 => vec![(P, CanonicalScalar::Int(10))],
                3 => vec![(P, CanonicalScalar::Int(7))],
                _ => vec![],
            },
        );
    }
    let mut first = WriteBatch::new(R);
    for (eid, src) in [(10, 1), (11, 1), (12, 4)] {
        first.add_edge(EId(eid), VId(src), VId(2), vec![]);
    }
    let mut second = WriteBatch::new(S);
    for (eid, dst) in [(20, 3), (21, 3), (22, 5)] {
        second.add_edge(EId(eid), VId(2), VId(dst), vec![]);
    }
    db.write_atomic(cx, vec![entities, first, second])
        .await
        .unwrap()
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            let scalar = |at| match row
                .get(at)
                .unwrap()
                .as_value()
                .and_then(GraphValue::as_scalar)
            {
                Some(CanonicalScalar::Int(value)) => Some(*value),
                Some(CanonicalScalar::Null) => None,
                _ => panic!("fixture extremum type changed"),
            };
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_integer(),
                row.get(2).unwrap().as_count().unwrap(),
                row.get(3).unwrap().as_count().unwrap(),
                scalar(4),
                scalar(5),
            )
        })
        .collect()
}
// Independent join over ordinary storage rows, followed by ordinary grouping.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], floor: i64) -> Vec<Summary> {
    let property = |vid| {
        vertices
            .iter()
            .find(|row| row.vid == vid)
            .and_then(|row| row.props.iter().find(|(key, _)| *key == P))
            .and_then(|(_, value)| {
                if let CanonicalScalar::Int(value) = value {
                    Some(*value)
                } else {
                    None
                }
            })
    };
    let mut groups: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
    for first in edges.iter().filter(|edge| edge.entry.relation == R) {
        if !property(first.entry.dst).is_some_and(|value| value >= floor) {
            continue;
        }
        for second in edges
            .iter()
            .filter(|edge| edge.entry.relation == S && edge.entry.src == first.entry.dst)
        {
            groups
                .entry(first.entry.src)
                .or_default()
                .push(property(second.entry.dst));
        }
    }
    groups
        .into_iter()
        .map(|(owner, values)| {
            let ints: Vec<_> = values.iter().flatten().copied().collect();
            (
                owner,
                values.len() as u64,
                (!ints.is_empty()).then(|| ints.iter().map(|x| i128::from(*x)).sum()),
                ints.len() as u64,
                ints.iter().collect::<BTreeSet<_>>().len() as u64,
                ints.iter().min().copied(),
                ints.iter().max().copied(),
            )
        })
        .collect()
}

#[test]
fn aggregate_text_reuses_all_five_sources_with_exact_limits_and_group_pagination() {
    let ((), report) = run_async_under_lab(0xa670_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let template = PreparedGraphAggregateText::prepare(TEXT, symbols).unwrap();
        assert_eq!(
            template.output_slots()[0],
            GraphAggregateTextSlot::Aggregate(0)
        );
        assert_eq!(
            template.output_slots()[1],
            GraphAggregateTextSlot::GroupKey(0)
        );
        let query = template.bind_parameters(&args(0, 0, 100)).unwrap();
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 0);
        assert_eq!(
            expected,
            vec![
                (VId(1), 6, Some(28), 4, 1, Some(7), Some(7)),
                (VId(4), 3, Some(14), 2, 1, Some(7), Some(7))
            ]
        );
        for rows in [
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
            assert_eq!(plain(&rows), expected);
        }
        let full = db
            .execute_graph_aggregate_governed(&cx, &query, policy())
            .unwrap();
        let exact = GqlQueryPolicy::new(
            full.rows.snapshot_records,
            full.rows.result_rows,
            full.evaluator.work_units,
            full.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &query, exact)
                .unwrap(),
            full
        );
        let page = template.bind_parameters(&args(0, 1, 1)).unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_aggregate_governed(&cx, &page, policy())
                    .unwrap()
                    .value
            ),
            expected[1..]
        );
        let zero = template.bind_parameters(&args(0, 0, 0)).unwrap();
        assert!(
            db.execute_graph_aggregate_governed(&cx, &zero, policy())
                .unwrap()
                .value
                .is_empty()
        );
        assert!(matches!(
            db.execute_graph_aggregate_governed(
                &cx,
                &zero,
                GqlQueryPolicy::new(5, 0, 1_000_000, 100_000)
            ),
            Err(GqlQueryError::Rows(_))
        ));
        let filtered = template.bind_parameters(&args(11, 0, 100)).unwrap();
        assert!(
            db.execute_graph_aggregate_governed(&cx, &filtered, policy())
                .unwrap()
                .value
                .is_empty()
        );
        assert_eq!(
            db.execute_graph_aggregate_governed(&cx, &query, policy())
                .unwrap()
                .value,
            full.value
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_changes_reopen_and_history_keep_the_same_text_summary() {
    let ((), report) = run_async_under_lab(0xa670_0002, |root| async move {
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
        let template = PreparedGraphAggregateText::prepare(TEXT, symbols).unwrap();
        let query = template.bind_parameters(&args(0, 0, 100)).unwrap();
        let before = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 0);
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(10));
        changes.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        changes.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(-2)));
        changes.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(3)));
        txn.write(&mut db, changes).unwrap();
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), 0);
        assert_eq!(
            expected,
            vec![
                (VId(1), 3, Some(-1), 3, 2, Some(-2), Some(3)),
                (VId(4), 3, Some(-1), 3, 2, Some(-2), Some(3))
            ]
        );
        assert_eq!(
            plain(
                &txn.execute_graph_aggregate_governed(&db, &cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            plain(
                &db.execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
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
            plain(
                &reopened
                    .execute_graph_aggregate_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        assert_eq!(
            plain(
                &pinned
                    .execute_graph_aggregate_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_output_preserves_unprojected_dependencies_without_fencing_disjoint_writes() {
    let ((), report) = run_async_under_lab(0xa670_0003, |root| async move {
        // Cancel the query child, keeping the lab supervisor available to join it.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let txn_cx = contexts.txn();
                for conflict in [false, true] {
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit).await;
                    let template = PreparedGraphAggregateText::prepare(TEXT, symbols).unwrap();
                    let query = template.bind_parameters(&args(0, 0, 0)).unwrap();
                    let mut txn = db.begin(&txn_cx).unwrap();
                    let mut staged = WriteBatch::new(R);
                    staged.create_vertex(VId(99), vec![], vec![]);
                    txn.write(&mut db, staged).unwrap();
                    assert!(
                        txn.execute_graph_aggregate_governed(&db, &cx, &query, policy())
                            .unwrap()
                            .value
                            .is_empty()
                    );
                    let mut winner = WriteBatch::new(R);
                    winner.set_vertex_property(
                        if conflict { VId(2) } else { VId(6) },
                        P,
                        Some(CanonicalScalar::Int(11)),
                    );
                    db.write(&commit, winner).await.unwrap();
                    let frontier = db.frontier().unwrap();
                    let result = txn.commit(&mut db, &commit).await;
                    if conflict {
                        assert!(matches!(
                            result,
                            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                                law: "FG-LAW-FCW-READ-01",
                                ..
                            }))
                        ));
                        assert_eq!(db.frontier().unwrap(), frontier);
                        assert!(db.vertex(VId(99)).unwrap().is_none());
                    } else {
                        result.unwrap();
                        assert!(db.vertex(VId(99)).unwrap().is_some());
                    }
                }
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit).await;
                let foreign = Database::open_memory(&commit, keys()).await.unwrap();
                let txn = db.begin(&txn_cx).unwrap();
                let query = PreparedGraphAggregateText::prepare(TEXT, symbols)
                    .unwrap()
                    .bind_parameters(&args(0, 0, 100))
                    .unwrap();
                root.cancel_with(CancelKind::User, Some("aggregate text authority control"));
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
            })
            .expect("lab query task can be spawned");
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok(), "the supervisor remains live");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
