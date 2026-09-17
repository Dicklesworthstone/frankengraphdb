//! Native MATCH maps and computed correlations across canonical database surfaces.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, VertexRow, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow, GraphMutationError,
    GraphMutationPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText,
    PreparedGraphMutation, PreparedGraphMutationText, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const OWNER: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const N: PropertyKeyId = PropertyKeyId(1);
const FLAG: PropertyKeyId = PropertyKeyId(2);
const MAP_HEAD: &str = "MATCH (a:Owner {flag:TRUE,n:10}) \
    OPTIONAL MATCH (a)-[:R]->(b {n:$target})";
const CAPTURE_HEAD: &str = "MATCH (a:Owner {flag:TRUE}) \
    OPTIONAL MATCH (b) WHERE b.n = a.n + $delta";
type Pair = (VId, Option<VId>);
type Records = Vec<(VId, Vec<LabelId>, Vec<(PropertyKeyId, CanonicalScalar)>)>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 1_000_000)
}

fn mutation_policy() -> GraphMutationPolicy {
    GraphMutationPolicy::new(wide(), 1_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(N)),
        (GraphSymbolKind::Property, "flag") => Some(GraphSymbol::Property(FLAG)),
        _ => None,
    }
}

fn argument(name: &str, value: i64) -> GqlParameters {
    GqlParameters::new().with_int64(name, value).unwrap()
}

fn pattern(head: &str, arguments: &GqlParameters) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(&format!("{head} RETURN a,b ORDER BY a,b"), symbols)
        .unwrap()
        .bind_parameters(arguments)
        .unwrap()
}

fn mutation() -> PreparedGraphMutation {
    PreparedGraphMutationText::prepare(
        "MATCH (a:Owner {flag:TRUE}) MATCH (b) \
         WHERE b.n = a.n + $delta SET b.n=b.n+5",
        R,
        symbols,
    )
    .unwrap()
    .bind_parameters(&argument("delta", 1))
    .unwrap()
}

fn pairs(rows: &[GraphValueRow]) -> Vec<Pair> {
    rows.iter()
        .map(|row| {
            (
                row.get(0).unwrap().as_vertex().unwrap(),
                row.get(1).unwrap().as_vertex(),
            )
        })
        .collect()
}

fn counts(rows: &[GraphAggregateRow]) -> Vec<(VId, u64)> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}

fn records(rows: &[VertexRow]) -> Records {
    rows.iter()
        .map(|row| (row.vid, row.labels.clone(), row.props.clone()))
        .collect()
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, flag) in [(1, true), (4, false)] {
        batch.create_vertex(
            VId(id),
            vec![OWNER],
            vec![(N, CanonicalScalar::Int(10)), (FLAG, CanonicalScalar::Bool(flag))],
        );
    }
    for id in [2, 3] {
        batch.create_vertex(VId(id), vec![], vec![(N, CanonicalScalar::Int(11))]);
    }
    batch.create_vertex(VId(5), vec![], vec![(N, CanonicalScalar::Null)]);
    batch.create_vertex(VId(6), vec![], vec![]);
    for (id, destination) in [(101, 2), (102, 2), (103, 3)] {
        batch.add_edge(EId(id), VId(1), VId(destination), vec![]);
    }
    db.write(cx, batch).await.unwrap()
}

#[test]
fn native_maps_and_captures_survive_staging_compaction_reopen_and_pinned_history() {
    let ((), report) = run_async_under_lab(0xca97_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        let mapped = pattern(MAP_HEAD, &argument("target", 11));
        let captured = pattern(CAPTURE_HEAD, &argument("delta", 1));
        let map_rows = vec![
            (VId(1), Some(VId(2))),
            (VId(1), Some(VId(2))),
            (VId(1), Some(VId(3))),
        ];
        let capture_rows = vec![(VId(1), Some(VId(2))), (VId(1), Some(VId(3)))];
        let aggregate = PreparedGraphAggregateText::prepare(
            &format!("{MAP_HEAD} RETURN a,COUNT(b) AS hits GROUP BY a ORDER BY a"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&argument("target", 11))
        .unwrap();
        let frozen = captured.canonical_bytes();
        for (plan, expected) in [(&mapped, &map_rows), (&captured, &capture_rows)] {
            for result in [
                db.execute_graph_pattern_governed(&query, plan, wide()).unwrap(),
                db.execute_graph_pattern_governed_at(&query, plan, basis, wide())
                    .unwrap(),
                pinned.execute_graph_pattern_governed(&query, plan, wide()).unwrap(),
                pinned.execute_graph_pattern_governed_at(&query, plan, basis, wide())
                    .unwrap(),
                txn.execute_graph_pattern_governed(&db, &query, plan, wide()).unwrap(),
            ] {
                assert_eq!(&pairs(&result.value), expected);
            }
        }
        assert_eq!(
            counts(&db.execute_graph_aggregate_governed(&query, &aggregate, wide()).unwrap().value),
            vec![(VId(1), 3)]
        );
        let selected = txn
            .execute_graph_mutation_governed(&mut db, &query, &mutation(), mutation_policy())
            .unwrap();
        assert_eq!((selected.selection.result_rows, selected.effects), (2, 2));
        let missing = vec![(VId(1), None)];
        for (plan, expected) in [(&mapped, &map_rows), (&captured, &capture_rows)] {
            assert_eq!(
                pairs(&txn.execute_graph_pattern_governed(&db, &query, plan, wide()).unwrap().value),
                missing
            );
            assert_eq!(
                &pairs(&db.execute_graph_pattern_governed(&query, plan, wide()).unwrap().value),
                expected
            );
        }
        assert_eq!(
            counts(&txn.execute_graph_aggregate_governed(&db, &query, &aggregate, wide()).unwrap().value),
            vec![(VId(1), 0)]
        );
        let moved = pattern(CAPTURE_HEAD, &argument("delta", 6));
        assert_eq!(
            pairs(&txn.execute_graph_pattern_governed(&db, &query, &moved, wide()).unwrap().value),
            capture_rows
        );
        assert_eq!(db.frontier().unwrap(), basis);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (plan, expected) in [(&mapped, &map_rows), (&captured, &capture_rows)] {
            assert_eq!(
                pairs(&reopened.execute_graph_pattern_governed(&query, plan, wide()).unwrap().value),
                missing
            );
            assert_eq!(
                &pairs(&reopened.execute_graph_pattern_governed_at(&query, plan, basis, wide()).unwrap().value),
                expected
            );
            assert_eq!(
                &pairs(&pinned.execute_graph_pattern_governed(&query, plan, wide()).unwrap().value),
                expected
            );
        }
        assert_eq!(
            counts(&reopened.execute_graph_aggregate_governed(&query, &aggregate, wide()).unwrap().value),
            vec![(VId(1), 0)]
        );
        assert_eq!(captured.canonical_bytes(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn map_selected_correlated_writes_refuse_atomically_and_can_retry() {
    let ((), report) = run_async_under_lab(0xca97_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let mut txn = db.begin(&contexts.txn()).unwrap();
        let mut prefix = WriteBatch::new(R);
        prefix.create_vertex(VId(777), vec![], vec![(N, CanonicalScalar::Int(123))]);
        txn.write(&mut db, prefix).unwrap();
        let before = records(&txn.vertices(&db).unwrap());
        let plan = mutation();
        let mut small = mutation_policy();
        small.max_effects = 1;
        assert!(matches!(
            txn.execute_graph_mutation_governed(&mut db, &query, &plan, small),
            Err(GqlQueryError::Source(GraphMutationError::EffectLimit { .. }))
        ));
        assert_eq!(records(&txn.vertices(&db).unwrap()), before);
        assert_eq!(db.frontier().unwrap(), basis);
        let result = txn
            .execute_graph_mutation_governed(&mut db, &query, &plan, mutation_policy())
            .unwrap();
        assert_eq!((result.selection.result_rows, result.effects), (2, 2));
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.vertex(VId(777)).unwrap().is_some());
        for id in [2, 3] {
            let row = db.vertex(VId(id)).unwrap().unwrap();
            assert_eq!(row.props, vec![(N, CanonicalScalar::Int(16))]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn captured_and_rejected_map_properties_remain_transaction_conflicts() {
    let ((), report) = run_async_under_lab(0xca97_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        for change in 0..2 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&contexts.txn()).unwrap();
            let plan = pattern(CAPTURE_HEAD, &argument("delta", 1));
            txn.execute_graph_pattern_governed(&db, &query, &plan, wide()).unwrap();
            let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, staged).unwrap();
            let mut winner = WriteBatch::new(R);
            if change == 0 {
                winner.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(12)));
            } else {
                winner.set_vertex_property(VId(4), FLAG, Some(CanonicalScalar::Bool(true)));
            }
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            assert!(matches!(
                txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(777)).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
