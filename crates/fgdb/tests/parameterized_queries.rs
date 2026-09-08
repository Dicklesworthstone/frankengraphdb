//! Numeric parameter bindings must use the ordinary product execution paths.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, MemVfs, ReadError, RelationBind, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{BudgetedGqlError, GlaExecutionError, GlaExecutionLimits, GqlEvidenceAuditError, GqlExecutionBudget, GqlParameters, PreparedGqlTemplate};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(2);
const N: PropertyKeyId = PropertyKeyId(3);

fn bind() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_label("L", L).with_property("n", N)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x81; 32], DatabaseSecurityNamespaceId([0x82; 32]), [0x83; 32])
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.expect("database");
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(VId(id), vec![L], vec![(N, CanonicalScalar::Int(id as i64))]);
    }
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch.add_edge(EId(11), VId(2), VId(3), vec![]);
    batch.add_edge(EId(12), VId(2), VId(4), vec![]);
    db.write(cx, batch).await.expect("seed");
    db
}

fn template() -> PreparedGqlTemplate {
    PreparedGqlTemplate::prepare(
        "MATCH (a)-[:R]->(b) WHERE b.n>=$min RETURN b SKIP$skip LIMIT$cap", &bind(),
    ).expect("prepare reusable template")
}

fn parameters(min: i64, skip: u64, cap: u64) -> GqlParameters {
    GqlParameters::new().with_int64("min", min).unwrap()
        .with_uint64("skip", skip).unwrap().with_uint64("cap", cap).unwrap()
}

#[test]
fn bindings_flow_through_live_historical_pinned_staged_budgeted_and_limited_queries() {
    let ((), report) = run_async_under_lab(0x7061_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let template = template();
        let wide = template.bind_parameters(&parameters(2, 0, 10)).unwrap();
        let page = template.bind_parameters(&parameters(2, 1, 1)).unwrap();
        assert_eq!(db.execute_prepared_query(&wide).unwrap(), vec![VId(2), VId(3), VId(4)]);
        assert_eq!(db.execute_prepared_query(&page).unwrap(), vec![VId(3)]);
        assert_eq!(pinned.execute_prepared_query(&page).unwrap(), vec![VId(3)]);
        let budgeted = db.execute_prepared_query_budgeted(&page, GqlExecutionBudget::new(3, 1)).unwrap();
        assert_eq!(budgeted.value, vec![VId(3)]);
        assert_eq!(budgeted.stats.snapshot_records, 3);
        assert_eq!(budgeted.stats.result_rows, 1);
        assert!(matches!(db.execute_prepared_query_budgeted(&wide, GqlExecutionBudget::new(3, 2)),
            Err(BudgetedGqlError::Budget(_))));
        let limits = GlaExecutionLimits::new(10_000, 10_000);
        assert_eq!(db.execute_prepared_query_limited(&page, limits).unwrap().value, vec![VId(3)]);
        assert_eq!(pinned.execute_prepared_query_limited(&page, limits).unwrap().value, vec![VId(3)]);
        let mut txn = db.begin(&txn_cx).unwrap();
        let high = template.bind_parameters(&parameters(7, 0, 10)).unwrap();
        assert!(txn.execute_prepared_query(&db, &high).unwrap().is_empty());
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(2), N, Some(CanonicalScalar::Int(9)));
        staged.create_vertex(VId(5), vec![L], vec![(N, CanonicalScalar::Int(8))]);
        staged.add_edge(EId(13), VId(2), VId(5), vec![]);
        txn.write(&mut db, staged).unwrap();
        assert_eq!(txn.execute_prepared_query(&db, &high).unwrap(), vec![VId(2), VId(5)]);
        assert_eq!(txn.execute_prepared_query_budgeted(&db, &high, GqlExecutionBudget::new(4, 2)).unwrap().value,
            vec![VId(2), VId(5)]);
        assert_eq!(txn.execute_prepared_query_limited(&db, &high, limits).unwrap().value, vec![VId(2), VId(5)]);
        assert!(db.execute_prepared_query(&high).unwrap().is_empty());
        let published = txn.commit(&mut db, &cx).await.unwrap();
        assert_eq!(db.execute_prepared_query(&high).unwrap(), vec![VId(2), VId(5)]);
        assert!(db.execute_prepared_query_at(&high, basis).unwrap().is_empty());
        assert!(pinned.execute_prepared_query(&high).unwrap().is_empty());
        assert!(matches!(pinned.execute_prepared_query_limited_at(&high, published, GlaExecutionLimits::new(0, 0)),
            Err(GlaExecutionError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        assert!(matches!(txn.execute_prepared_query(&db, &high), Err(WriteTxnError::Finished)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn evidence_replays_concrete_values_and_rejects_other_bindings_even_with_identical_rows() {
    let ((), report) = run_async_under_lab(0x7061_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let template = template();
        let low = template.bind_parameters(&parameters(-10, 0, 10)).unwrap();
        let equivalent_rows = template.bind_parameters(&parameters(-9, 0, 10)).unwrap();
        assert_eq!(db.execute_prepared_query(&low).unwrap(), db.execute_prepared_query(&equivalent_rows).unwrap());
        let artifact = db.execute_prepared_query_artifact(&low).unwrap();
        let bytes = artifact.to_bytes();
        assert_eq!(db.audit_prepared_query_artifact(&low, &bytes).unwrap().rows(), artifact.rows());
        assert!(matches!(db.audit_prepared_query_artifact(&equivalent_rows, &bytes), Err(GqlEvidenceAuditError::InputMismatch)));
        let rebuilt = fgdb_gql::PreparedGqlQuery::prepare(low.statement(), low.bind()).unwrap();
        assert_eq!(rebuilt, low);
        assert_eq!(db.audit_prepared_query_artifact(&rebuilt, &bytes).unwrap().rows(), artifact.rows());
        db.compact(&cx).await.unwrap();
        assert_eq!(db.audit_prepared_query_artifact(&low, &bytes).unwrap().rows(), artifact.rows());
        let mut txn = db.begin(&txn_cx).unwrap();
        let overlay = txn.execute_prepared_query_overlay_artifact(&db, &low).unwrap();
        let overlay_bytes = overlay.to_bytes();
        let mut cursor = txn.open_untrusted_prepared_query_overlay_artifact_cursor(&db, &low, &overlay_bytes).unwrap();
        assert_eq!(cursor.next_page(1).unwrap().rows(), &[VId(2)]);
        let checkpoint = cursor.checkpoint_token().unwrap().to_bytes();
        let mut resumed = txn.resume_untrusted_prepared_query_overlay_artifact_cursor(
            &db, &low, &overlay_bytes, &checkpoint).unwrap();
        assert_eq!(resumed.next_page(2).unwrap().rows(), &[VId(3), VId(4)]);
        assert!(txn.resume_untrusted_prepared_query_overlay_artifact_cursor(
            &db, &equivalent_rows, &overlay_bytes, &checkpoint).is_err());
        let mut staged = WriteBatch::new(R);
        staged.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(55)));
        txn.write(&mut db, staged).unwrap();
        assert!(matches!(txn.audit_prepared_query_overlay_artifact(&db, &low, &overlay_bytes),
            Err(GqlEvidenceAuditError::StagedEffectMismatch)));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameter_queries_preserve_owner_fences_and_empty_scan_conflicts() {
    let ((), report) = run_async_under_lab(0x7061_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txn_cx = contexts.txn();
        let mut db = seeded(&cx).await;
        let foreign = seeded(&cx).await;
        let query = template().bind_parameters(&parameters(1000, 0, 1)).unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        assert!(matches!(txn.execute_prepared_query(&foreign, &query), Err(WriteTxnError::WrongDatabase)));
        assert!(txn.execute_prepared_query(&db, &query).unwrap().is_empty());
        let mut staged = WriteBatch::new(R);
        staged.create_vertex(VId(99), vec![], vec![]);
        txn.write(&mut db, staged).unwrap();
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(6), vec![], vec![]);
        winner.create_vertex(VId(7), vec![L], vec![(N, CanonicalScalar::Int(1001))]);
        winner.add_edge(EId(14), VId(6), VId(7), vec![]);
        db.write(&cx, winner).await.unwrap();
        assert_eq!(db.execute_prepared_query(&query).unwrap(), vec![VId(7)]);
        let frontier = db.frontier().unwrap();
        assert!(matches!(txn.commit(&mut db, &cx).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins { law: "FG-LAW-FCW-READ-01", .. }))));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert!(db.vertex(VId(99)).unwrap().is_none());
        assert!(matches!(db.execute_prepared_query_at(&query, CommitSeq(frontier.0 + 1)),
            Err(GqlError::Read(ReadError::BeyondFrontier { .. }))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
