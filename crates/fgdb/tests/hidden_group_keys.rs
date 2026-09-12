//! Hidden grouping properties are read dependencies in every embedded posture.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const OWNER: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x75; 32], DatabaseSecurityNamespaceId([0x76; 32]), [0x77; 32])
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn template() -> PreparedGraphAggregateText {
    PreparedGraphAggregateText::prepare(
        "MATCH (a:Owner) RETURN DISTINCT COUNT(*) AS n GROUP BY a.p \
         HAVING a.p >= $floor ORDER BY a.p LIMIT $take", |kind, name| {
            match (kind, name) {
                (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(OWNER)),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
                _ => None,
            }
        }).unwrap()
}
fn args(floor: i64, take: u64) -> GqlParameters {
    GqlParameters::new().with_int64("floor", floor).unwrap().with_uint64("take", take).unwrap()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, value) in [1, 1, 2, 3, 3].into_iter().enumerate() {
        batch.create_vertex(VId(id as u128), vec![OWNER], vec![(P, CanonicalScalar::Int(value))]);
    }
    db.write(cx, batch).await.unwrap()
}
fn counts(rows: &[GraphAggregateRow]) -> Vec<u64> {
    rows.iter().map(|row| {
        assert!(row.keys().is_empty());
        assert_eq!(row.values().len(), 1);
        row.get(0).unwrap().as_count().unwrap()
    }).collect()
}

#[test]
fn hidden_grouping_and_distinct_share_live_pinned_staged_and_reopened_history() {
    let ((), report) = run_async_under_lab(0x1dde_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap(); let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let basis = seed(&mut db, &commit).await;
        let pinned = db.read_session().unwrap();
        let definition = template();
        let query = definition.bind_parameters(&args(1, 10)).unwrap();
        let frozen = query.canonical_bytes();
        assert!(query.key_columns().is_empty());
        assert_eq!(query.evaluation_key_columns().len(), 1);
        let measured = db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap();
        assert_eq!(counts(&measured.value), vec![2, 1]);
        let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, 2,
            measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_graph_aggregate_governed(&cx, &query, exact).unwrap(), measured);
        assert_eq!(counts(&db.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap().value), vec![2, 1]);
        assert_eq!(counts(&pinned.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), vec![2, 1]);
        assert_eq!(counts(&pinned.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap().value), vec![2, 1]);
        let mut txn = db.begin(&txn_cx).unwrap();
        assert_eq!(counts(&txn.execute_graph_aggregate_governed(&db, &cx, &query, wide()).unwrap().value), vec![2, 1]);
        let mut changes = WriteBatch::new(R);
        changes.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(1)));
        txn.write(&mut db, changes).unwrap();
        // Three vertices now share hidden key 1, two share hidden key 3.
        assert_eq!(counts(&txn.execute_graph_aggregate_governed(&db, &cx, &query, wide()).unwrap().value), vec![3, 2]);
        assert_eq!(counts(&db.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), vec![2, 1]);
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap(); drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(counts(&reopened.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), vec![3, 2]);
        assert_eq!(counts(&reopened.execute_graph_aggregate_governed_at(&cx, &query, basis, wide()).unwrap().value), vec![2, 1]);
        assert_eq!(counts(&pinned.execute_graph_aggregate_governed(&cx, &query, wide()).unwrap().value), vec![2, 1]);
        let changed = definition.bind_parameters(&args(2, 10)).unwrap();
        assert_ne!(changed.canonical_bytes(), frozen);
        assert_eq!(query.canonical_bytes(), frozen);
        assert_eq!(counts(&reopened.execute_graph_aggregate_governed(&cx, &changed, wide()).unwrap().value), vec![2]);
        assert_eq!(counts(&reopened.execute_graph_aggregate_governed_at(&cx, &changed, basis, wide()).unwrap().value), vec![1, 2]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_group_key_dependencies_survive_distinct_output_refusal_and_limit_zero() {
    let ((), report) = run_async_under_lab(0x1dde_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit(); let txn_cx = contexts.txn();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit).await;
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(777), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            let query = template().bind_parameters(&args(1, u64::from(mode != 2))).unwrap();
            let result = txn.execute_graph_aggregate_governed(&db, &cx, &query,
                GqlQueryPolicy::new(1000, u64::from(mode != 1), 1_000_000, 1_000_000));
            match mode {
                0 => assert_eq!(counts(&result.unwrap().value), vec![2]),
                1 => assert!(matches!(result, Err(GqlQueryError::Rows(_)))),
                _ => assert!(result.unwrap().value.is_empty()),
            }
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(2)));
            db.write(&commit, winner).await.unwrap();
            let frontier = db.frontier().unwrap();
            // No further transaction read can restore a lost hidden-key
            // dependency before its immediate attempted write commit.
            assert!(matches!(txn.commit(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins { .. }))));
            assert_eq!(db.frontier().unwrap(), frontier);
            assert!(db.vertex(VId(777)).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
