//! Edge-rooted transactions must supply the same independent vertex domain as
//! native snapshot queries, including isolates, overlays and insertion gaps.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaOperator, GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphSymbol,
    GraphSymbolKind, PreparedGraphAggregate, PreparedGraphText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, EmbeddedTxnCompletion,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const ROOT: LabelId = LabelId(1);
const TAG: LabelId = LabelId(2);
const EMPTY: LabelId = LabelId(3);
const P: PropertyKeyId = PropertyKeyId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Label, "Tag") => Some(GraphSymbol::Label(TAG)),
        (GraphSymbolKind::Label, "Empty") => Some(GraphSymbol::Label(EMPTY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

// A comma product reads edges beside an independent vertex domain. The planner
// roots it at ScanVertices (11dadc0c), so it reads edges through Expand.
fn independent(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    let pattern = query(text);
    assert!(pattern.plan().reads_edges(), "{text}");
    assert!(
        pattern
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::ScanVertices)),
        "{text}"
    );
    pattern
}

// An uncorrelated EXISTS keeps the edge root, so it must exercise the
// edge-root source's independent vertex domain.
fn edge_rooted(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    let pattern = independent(text);
    assert!(
        pattern.plan().scans_edges(),
        "must exercise the edge-root source: {text}"
    );
    pattern
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000)
}

fn cells(rows: &[GraphValueRow]) -> Vec<Vec<GraphValue>> {
    rows.iter().map(|row| row.values().to_vec()).collect()
}

fn integers(values: &[i64]) -> Vec<Vec<GraphValue>> {
    values
        .iter()
        .map(|value| vec![GraphValue::Scalar(CanonicalScalar::Int(*value))])
        .collect()
}

fn read_conflict(result: Result<EmbeddedTxnCompletion, WriteTxnError>) -> bool {
    matches!(
        result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01",
            ..
        }))
    )
}

async fn fixture(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0xa5; 32],
        DatabaseSecurityNamespaceId([0xa6; 32]),
        [0xa7; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![ROOT], vec![(P, CanonicalScalar::Int(10))]);
    seed.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(20))]);
    for id in [3, 4] {
        seed.create_vertex(
            VId(id),
            vec![TAG],
            vec![(P, CanonicalScalar::Int(id as i64 * 10))],
        );
    }
    // Parallel relationships must retain bag multiplicity. V3/V4 are isolates.
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.unwrap();
    db
}

#[test]
fn edge_rooted_products_include_isolates_and_charge_both_domains_once() {
    let ((), report) = run_async_under_lab(0x6973_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut db = fixture(&cx).await;
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        for (text, expected) in [
            (
                "MATCH (a)-[:R]->(b), (n) RETURN n.p",
                vec![10, 10, 20, 20, 30, 30, 40, 40],
            ),
            (
                "MATCH (a:Root)-[:R]->(b), (n:Tag) RETURN n.p",
                vec![30, 30, 40, 40],
            ),
            (
                "MATCH (a)-[:R]->(b), (n) RETURN DISTINCT n.p SKIP 1 LIMIT 2",
                vec![20, 30],
            ),
        ] {
            let pattern = independent(text);
            let actual = txn
                .execute_graph_pattern_governed(&db, &qcx, &pattern, policy())
                .unwrap();
            let native = pinned
                .execute_graph_pattern_governed(&qcx, &pattern, policy())
                .unwrap();
            assert_eq!(cells(&actual.value), integers(&expected), "{text}");
            assert_eq!(actual.value, native.value);
            assert_eq!(
                actual.rows.snapshot_records, 6,
                "four vertices plus two edges, once"
            );
            assert_eq!(actual.rows.snapshot_records, native.rows.snapshot_records);
        }
        let head = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, &cx).await,
            Ok(EmbeddedTxnCompletion::ReadClosed { .. })
        ));
        assert_eq!(db.frontier().unwrap(), head);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_vertex_overlay_applies_properties_labels_births_and_retirement() {
    let ((), report) = run_async_under_lab(0x6973_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut db = fixture(&cx).await;
        let pinned = db.read_session().unwrap();
        let head = db.frontier().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(33)));
        edit.set_vertex_label(VId(3), TAG, false);
        edit.delete_vertex(VId(4));
        edit.create_vertex(VId(5), vec![TAG], vec![(P, CanonicalScalar::Int(50))]);
        txn.write(&mut db, edit).unwrap();
        let all = independent("MATCH (a)-[:R]->(b), (n) RETURN n.p");
        let tagged = independent("MATCH (a:Root)-[:R]->(b), (n:Tag) RETURN n.p");
        let staged = txn
            .execute_graph_pattern_governed(&db, &qcx, &all, policy())
            .unwrap();
        assert_eq!(
            cells(&staged.value),
            integers(&[10, 10, 20, 20, 33, 33, 50, 50])
        );
        assert_eq!(staged.rows.snapshot_records, 6);
        let selected = txn
            .execute_graph_pattern_governed(&db, &qcx, &tagged, policy())
            .unwrap();
        assert_eq!(cells(&selected.value), integers(&[50, 50]));
        assert_eq!(db.frontier().unwrap(), head);
        assert!(db.vertex(VId(5)).unwrap().is_none());
        assert!(db.vertex(VId(4)).unwrap().is_some());
        assert_eq!(
            txn.commit(&mut db, &cx).await.unwrap(),
            CommitSeq(head.0 + 1)
        );
        assert_eq!(db.delta_since(head).unwrap().count(), 1);
        assert_eq!(
            db.execute_graph_pattern_governed(&qcx, &all, policy())
                .unwrap()
                .value,
            staged.value
        );
        assert_eq!(
            cells(
                &pinned
                    .execute_graph_pattern_governed(&qcx, &tagged, policy())
                    .unwrap()
                    .value
            ),
            integers(&[30, 30, 40, 40])
        );
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_independent_results_and_zero_pages_keep_insertion_witnesses_after_rollback() {
    let ((), report) = run_async_under_lab(0x6973_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        for text in [
            "MATCH (a:Root)-[:R]->(b), (n:Empty) RETURN n.p",
            "MATCH (a:Root)-[:R]->(b), (n) RETURN n.p LIMIT 0",
        ] {
            let mut db = fixture(&cx).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "before-read").unwrap();
            let pattern = independent(text);
            assert!(
                txn.execute_graph_pattern_governed(&db, &qcx, &pattern, policy())
                    .unwrap()
                    .value
                    .is_empty()
            );
            txn.rollback_to_savepoint(&db, "before-read").unwrap();
            let mut winner = WriteBatch::new(R);
            winner.create_vertex(VId(9), vec![EMPTY], vec![(P, CanonicalScalar::Int(90))]);
            db.write(&cx, winner).await.unwrap();
            let head = db.frontier().unwrap();
            assert!(read_conflict(txn.finish(&mut db, &cx).await), "{text}");
            assert_eq!(db.frontier().unwrap(), head);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_quota_counts_isolates_before_returning_even_one_result() {
    let ((), report) = run_async_under_lab(0x6973_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut db = fixture(&cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        let pattern = independent("MATCH (a)-[:R]->(b), (n) RETURN n.p LIMIT 1");
        let small = GqlQueryPolicy::new(5, 1, 1_000_000, 1_000_000);
        let failure = txn.execute_graph_pattern_governed(&db, &qcx, &pattern, small);
        assert!(matches!(failure, Err(GqlQueryError::Rows(error))
            if error.dimension == GqlBudgetDimension::SnapshotRecords
                && error.limit == 5 && error.observed == 6));
        let exact = GqlQueryPolicy::new(6, 1, 1_000_000, 1_000_000);
        let result = txn
            .execute_graph_pattern_governed(&db, &qcx, &pattern, exact)
            .unwrap();
        assert_eq!(cells(&result.value), integers(&[10]));
        assert_eq!(result.rows.snapshot_records, 6);
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(9), vec![], vec![]);
        db.write(&cx, winner).await.unwrap();
        assert!(read_conflict(txn.finish(&mut db, &cx).await));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_rows_stay_at_the_original_snapshot_after_a_writer_advances() {
    let ((), report) = run_async_under_lab(0x6973_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut db = fixture(&cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(31)));
        winner.create_vertex(VId(9), vec![TAG], vec![(P, CanonicalScalar::Int(90))]);
        db.write(&cx, winner).await.unwrap();
        let pattern = independent("MATCH (a)-[:R]->(b), (n:Tag) RETURN n.p");
        let historic = txn
            .execute_graph_pattern_governed(&db, &qcx, &pattern, policy())
            .unwrap();
        assert_eq!(cells(&historic.value), integers(&[30, 30, 40, 40]));
        assert_eq!(historic.rows.snapshot_records, 6);
        assert_eq!(
            cells(
                &db.execute_graph_pattern_governed(&qcx, &pattern, policy())
                    .unwrap()
                    .value
            ),
            integers(&[31, 31, 40, 40, 90, 90])
        );
        assert!(read_conflict(txn.finish(&mut db, &cx).await));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_and_independent_probes_consume_the_same_complete_vertex_domain() {
    let ((), report) = run_async_under_lab(0x6973_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut db = fixture(&cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        let input = independent("MATCH (a)-[:R]->(b), (n:Tag) RETURN n.p").with_duplicates();
        let aggregate = PreparedGraphAggregate::prepare(
            input,
            &[],
            &[
                GraphAggregate::count_rows("count"),
                GraphAggregate::sum_int("total", 0),
            ],
            0,
            None,
        )
        .unwrap();
        let result = txn
            .execute_graph_aggregate_governed(&db, &qcx, &aggregate, policy())
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(4));
        assert_eq!(result.value[0].get(1).unwrap().as_integer(), Some(140));
        assert_eq!(result.rows.snapshot_records, 6);
        assert_eq!(
            result.value,
            db.execute_graph_aggregate_governed(&qcx, &aggregate, policy())
                .unwrap()
                .value
        );
        for (text, expected) in [
            (
                "MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (n:Tag) } RETURN a.p",
                vec![10, 10],
            ),
            (
                "MATCH (a)-[:R]->(b) WHERE NOT EXISTS { MATCH (n:Tag) } RETURN a.p",
                vec![],
            ),
            (
                "MATCH (a)-[:R]->(b) WHERE NOT EXISTS { MATCH (n:Empty) } RETURN a.p",
                vec![10, 10],
            ),
        ] {
            let pattern = edge_rooted(text);
            let actual = txn
                .execute_graph_pattern_governed(&db, &qcx, &pattern, policy())
                .unwrap();
            assert_eq!(cells(&actual.value), integers(&expected), "{text}");
            assert_eq!(
                actual.value,
                db.execute_graph_pattern_governed(&qcx, &pattern, policy())
                    .unwrap()
                    .value
            );
            assert_eq!(actual.rows.snapshot_records, 6);
        }
        txn.finish(&mut db, &cx).await.unwrap();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn connected_edge_queries_do_not_gain_an_unrelated_vertex_scan_witness() {
    let ((), report) = run_async_under_lab(0x6973_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let qcx = contexts.query();
        let mut db = fixture(&cx).await;
        let mut txn = db.begin(&txcx).unwrap();
        let pattern = query("MATCH (a:Root)-[:R]->(b) RETURN a.p");
        assert!(pattern.plan().scans_edges());
        assert!(
            !pattern
                .plan()
                .operators()
                .iter()
                .any(|op| matches!(op, GlaOperator::ScanVertices))
        );
        let result = txn
            .execute_graph_pattern_governed(&db, &qcx, &pattern, policy())
            .unwrap();
        assert_eq!(cells(&result.value), integers(&[10, 10]));
        assert_eq!(
            result.rows.snapshot_records, 2,
            "only the original edge domain is charged"
        );
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(9), vec![TAG], vec![]);
        db.write(&cx, winner).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &cx).await,
            Ok(EmbeddedTxnCompletion::ReadClosed { .. })
        ));
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
