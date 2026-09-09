//! Text preparation reaches real GLA, immutable history, and canonical overlays.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EdgeRecord, GqlError, MemVfs, ReadError, VertexRow, WriteBatch,
    WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const KNOWS: RelationId = RelationId(1);
const WORKS_AT: RelationId = RelationId(2);
const SHIPS: RelationId = RelationId(3);
const BACKS: RelationId = RelationId(4);
const PERSON: LabelId = LabelId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const NAME: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId((1_u128 << 96) + 5);
const TEXT: &str = "MATCH (person:Person)-[:KNOWS]->(friend)-[:WORKS_AT]->(company)-[:SHIPS]->(carrier), \
    (carrier)-[:BACKS]->(person) WHERE company.score >= $min AND person <> carrier \
    RETURN ALL person AS owner,company.name AS company,carrier";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb1; 32],
        DatabaseSecurityNamespaceId([0xb2; 32]),
        [0xb3; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "KNOWS") => Some(GraphSymbol::Relation(KNOWS)),
        (GraphSymbolKind::Relation, "WORKS_AT") => Some(GraphSymbol::Relation(WORKS_AT)),
        (GraphSymbolKind::Relation, "SHIPS") => Some(GraphSymbol::Relation(SHIPS)),
        (GraphSymbolKind::Relation, "BACKS") => Some(GraphSymbol::Relation(BACKS)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(NAME)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}
fn pattern(min: i64) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(TEXT, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new().with_int64("min", min).unwrap())
        .unwrap()
}
fn plain(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut vertices = WriteBatch::new(KNOWS);
    for vid in [VId(1), VId(2), VId(3), HIGH, VId(7)] {
        vertices.create_vertex(
            vid,
            if vid == VId(1) { vec![PERSON] } else { vec![] },
            vec![(
                SCORE,
                CanonicalScalar::Int(if vid == VId(3) { 90 } else { 10 }),
            )],
        );
    }
    vertices.set_vertex_property(
        VId(3),
        NAME,
        Some(CanonicalScalar::ucs_basic_text("Foundry").unwrap()),
    );
    db.write(cx, vertices).await.unwrap();
    let mut batches = Vec::new();
    for (eid, relation, src, dst) in [
        (10, KNOWS, VId(1), VId(2)),
        (11, KNOWS, VId(1), VId(2)),
        (20, WORKS_AT, VId(2), VId(3)),
        (30, SHIPS, VId(3), HIGH),
        (40, BACKS, HIGH, VId(1)),
    ] {
        let mut batch = WriteBatch::new(relation);
        batch.add_edge(EId(eid), src, dst, vec![]);
        batches.push(batch);
    }
    db.write_atomic(cx, batches).await.unwrap()
}
async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    seed(&mut db, cx).await;
    db
}

// Four concrete edge occurrences, looked up independently of text parsing,
// variable-slot scheduling, GLA traversal, or borrowed source admission.
fn oracle(vertices: &[VertexRow], edges: &[EdgeRecord], min: i64) -> Vec<Vec<GraphValue>> {
    let mut result = Vec::new();
    for a in edges.iter().filter(|edge| edge.entry.relation == KNOWS) {
        for b in edges
            .iter()
            .filter(|edge| edge.entry.relation == WORKS_AT && edge.entry.src == a.entry.dst)
        {
            for c in edges
                .iter()
                .filter(|edge| edge.entry.relation == SHIPS && edge.entry.src == b.entry.dst)
            {
                for d in edges.iter().filter(|edge| {
                    edge.entry.relation == BACKS
                        && edge.entry.src == c.entry.dst
                        && edge.entry.dst == a.entry.src
                }) {
                    let owner = vertices.iter().find(|row| row.vid == a.entry.src).unwrap();
                    let company = vertices.iter().find(|row| row.vid == b.entry.dst).unwrap();
                    if !owner.labels.contains(&PERSON) || owner.vid == d.entry.src {
                        continue;
                    }
                    if !company.props.iter().any(|(key, scalar)| {
                        *key == SCORE
                            && matches!(scalar, CanonicalScalar::Int(value) if *value >= min)
                    }) {
                        continue;
                    }
                    let name = company
                        .props
                        .iter()
                        .find(|(key, _)| *key == NAME)
                        .map(|(_, value)| value.clone())
                        .unwrap_or(CanonicalScalar::Null);
                    result.push(vec![
                        GraphValue::Vertex(owner.vid),
                        GraphValue::Scalar(name),
                        GraphValue::Vertex(c.entry.dst),
                    ]);
                }
            }
        }
    }
    result.sort();
    result
}

#[test]
fn long_text_property_bags_agree_on_all_five_read_surfaces() {
    let ((), report) = run_async_under_lab(0x7e87_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let at = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let query = pattern(80);
        let expected = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 80);
        assert_eq!(expected.len(), 2);
        assert_eq!(query.columns(), &["owner", "company", "carrier"]);
        for run in [
            db.execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap(),
            db.execute_graph_pattern_governed_at(&cx, &query, at, policy())
                .unwrap(),
            pinned
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap(),
            pinned
                .execute_graph_pattern_governed_at(&cx, &query, at, policy())
                .unwrap(),
            txn.execute_graph_pattern_governed(&db, &cx, &query, policy())
                .unwrap(),
        ] {
            assert_eq!(plain(&run.value), expected);
            assert_eq!(run.rows.snapshot_records, 5);
            assert_eq!(run.rows.result_rows, 2);
        }
        txn.abort();
        let no_match = pattern(100);
        assert!(
            db.execute_graph_pattern_governed(&cx, &no_match, policy())
                .unwrap()
                .value
                .is_empty()
        );
        let distinct_text = TEXT.replace("RETURN ALL", "RETURN DISTINCT");
        let distinct = PreparedGraphText::prepare(&distinct_text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new().with_int64("min", 80).unwrap())
            .unwrap();
        assert_eq!(
            db.execute_graph_pattern_governed(&cx, &distinct, policy())
                .unwrap()
                .value
                .len(),
            1
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn text_query_staging_nulls_ensures_and_history_survive_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x7e87_0002, |root| async move {
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
        let query = pattern(80);
        let before = oracle(&db.vertices().unwrap(), &db.edges().unwrap(), 80);
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut staged = WriteBatch::new(KNOWS);
        staged.delete_edge(EId(10));
        staged.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        staged.set_vertex_property(VId(3), NAME, None);
        txn.write(&mut db, staged).unwrap();
        assert!(txn.edge(&db, EId(999)).unwrap().is_none());
        let expected = oracle(&txn.vertices(&db).unwrap(), &txn.edges(&db).unwrap(), 80);
        assert_eq!(expected.len(), 1);
        let overlay = txn
            .execute_graph_pattern_governed(&db, &cx, &query, policy())
            .unwrap();
        assert_eq!(plain(&overlay.value), expected);
        assert!(overlay.value[0].get(1).unwrap().is_null());
        let exact = GqlQueryPolicy::new(
            overlay.rows.snapshot_records,
            overlay.rows.result_rows,
            overlay.evaluator.work_units,
            overlay.evaluator.scratch_entries,
        );
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &query, exact)
                .unwrap(),
            overlay
        );
        assert_eq!(
            plain(
                &db.execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        txn.commit(&mut db, &commit).await.unwrap();
        db.compact(&commit).await.unwrap();
        assert_eq!(
            plain(
                &db.execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        drop(db);
        let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            expected
        );
        assert_eq!(
            plain(
                &reopened
                    .execute_graph_pattern_governed_at(&cx, &query, basis, policy())
                    .unwrap()
                    .value
            ),
            before
        );
        assert_eq!(
            plain(
                &pinned
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap()
                    .value
            ),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn text_queries_keep_resource_and_source_error_precedence() {
    let ((), report) = run_async_under_lab(0x7e87_0003, |root| async move {
        // Cancel the query task while keeping the lab supervisor live, so the
        // query's error-precedence assertions can finish and be joined.
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let commit = contexts.commit();
                let cx = contexts.query();
                let txn_cx = contexts.txn();
                let mut db = seeded(&commit).await;
                let foreign = seeded(&commit).await;
                let query = pattern(80);
                let full = db
                    .execute_graph_pattern_governed(&cx, &query, policy())
                    .unwrap();
                let exact = GqlQueryPolicy::new(
                    full.rows.snapshot_records,
                    full.rows.result_rows,
                    full.evaluator.work_units,
                    full.evaluator.scratch_entries,
                );
                assert_eq!(
                    db.execute_graph_pattern_governed(&cx, &query, exact)
                        .unwrap(),
                    full
                );
                for cap in [
                    GqlQueryPolicy::new(4, 2, 1_000_000, 1_000_000),
                    GqlQueryPolicy::new(5, 1, 1_000_000, 1_000_000),
                ] {
                    assert!(matches!(
                        db.execute_graph_pattern_governed(&cx, &query, cap),
                        Err(GqlQueryError::Rows(_))
                    ));
                }
                for cap in [
                    GqlQueryPolicy::new(5, 2, full.evaluator.work_units - 1, 1_000_000),
                    GqlQueryPolicy::new(5, 2, 1_000_000, full.evaluator.scratch_entries - 1),
                ] {
                    assert!(matches!(
                        db.execute_graph_pattern_governed(&cx, &query, cap),
                        Err(GqlQueryError::Evaluator(_))
                    ));
                }
                let txn = db.begin(&txn_cx).unwrap();
                let future = CommitSeq(db.frontier().unwrap().0 + 1);
                let zero = GqlQueryPolicy::new(0, 0, 0, 0);
                root.cancel_with(
                    asupersync::CancelKind::User,
                    Some("text query cancellation"),
                );
                assert!(matches!(
                    db.execute_graph_pattern_governed(&cx, &query, policy()),
                    Err(GqlQueryError::Interrupted(_))
                ));
                assert!(matches!(
                    db.execute_graph_pattern_governed_at(&cx, &query, future, zero),
                    Err(GqlQueryError::Source(GqlError::Read(
                        ReadError::BeyondFrontier { .. }
                    )))
                ));
                assert!(matches!(
                    txn.execute_graph_pattern_governed(&foreign, &cx, &query, zero),
                    Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
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
fn refusal_keeps_unprojected_dependencies_and_disjoint_commits_remain_possible() {
    let ((), report) = run_async_under_lab(0x7e87_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txn_cx = contexts.txn();
        for conflicting in [false, true] {
            let mut db = seeded(&commit).await;
            let query = pattern(80);
            let old = db
                .execute_graph_pattern_governed(&cx, &query, policy())
                .unwrap()
                .value;
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut output = WriteBatch::new(KNOWS);
            output.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, output).unwrap();
            assert!(matches!(
                txn.execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &query,
                    GqlQueryPolicy::new(100, 0, 1_000_000, 1_000_000)
                ),
                Err(GqlQueryError::Rows(_))
            ));
            let mut winner = WriteBatch::new(KNOWS);
            winner.set_vertex_property(
                if conflicting { VId(2) } else { VId(7) },
                SCORE,
                Some(CanonicalScalar::Int(21)),
            );
            db.write(&commit, winner).await.unwrap();
            // No owned full-table oracle reads here: they would add unrelated
            // dependencies and invalidate the deliberate disjoint control.
            assert_eq!(
                txn.execute_graph_pattern_governed(&db, &cx, &query, policy())
                    .unwrap()
                    .value,
                old
            );
            let frontier = db.frontier().unwrap();
            let result = txn.commit(&mut db, &commit).await;
            if conflicting {
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
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
