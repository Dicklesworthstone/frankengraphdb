//! Product regressions for one context-governed query, not two executions.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DerivedPublicationStage, GqlError, MemVfs, ReadError,
    RelationBind, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlEvidenceAuditError, GqlParameters,
    GqlPreparedResultArtifact, GqlQueryError, GqlQueryPolicy, PreparedGqlQuery,
    PreparedGqlTemplate,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const N: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa1; 32], DatabaseSecurityNamespaceId([0xa2; 32]), [0xa3; 32])
}

fn names() -> RelationBind {
    RelationBind::new().with_relation("R", R).with_relation("S", S)
        .with_label("L", L).with_property("n", N)
}

fn generous() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 10_000, 10_000)
}

async fn seeded(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut first = WriteBatch::new(R);
    for id in 1..=3 {
        first.create_vertex(VId(id), vec![L], vec![(N, CanonicalScalar::Int(7))]);
    }
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    first.add_edge(EId(11), VId(2), VId(2), vec![]);
    first.add_edge(EId(12), VId(1), VId(2), vec![]);
    db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(20), VId(2), VId(1), vec![]);
    second.add_edge(EId(21), VId(2), VId(3), vec![]);
    db.write(cx, second).await.unwrap();
    db
}

#[test]
fn governed_aliases_agree_with_expected_rows_across_every_product_surface() {
    let ((), report) = run_async_under_lab(0xc0a2_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txn_cx).unwrap();
        for (statement, expected) in [
            ("MATCH (a)-[:R]->(a) RETURN a", vec![VId(2)]),
            ("MATCH (a)<-[:R]-(a) RETURN a", vec![VId(2)]),
            ("MATCH (a)-[:R]-(a) RETURN a", vec![VId(2)]),
            ("MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a", vec![VId(1)]),
            ("MATCH (a)<-[:R]-(b)<-[:S]-(a) RETURN a", vec![VId(2)]),
            ("MATCH (a)-[:R]-(b)-[:S]-(a) RETURN a", vec![VId(1), VId(2)]),
        ] {
            let query = PreparedGqlQuery::prepare(statement, &names()).unwrap();
            assert_eq!(db.execute_prepared_query(&query).unwrap(), expected);
            let live = db.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap();
            assert_eq!(live.value, expected, "{statement}");
            assert_eq!(live.rows.snapshot_records, 5);
            assert_eq!(live.rows.result_rows, expected.len() as u64);
            assert_eq!(db.execute_prepared_query_governed_at(&query_cx, &query, basis, generous()).unwrap(), live);
            assert_eq!(pinned.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap(), live);
            assert_eq!(pinned.execute_prepared_query_governed_at(&query_cx, &query, basis, generous()).unwrap(), live);
            assert_eq!(txn.execute_prepared_query_governed(&db, &query_cx, &query, generous()).unwrap(), live);
        }

        let template = PreparedGqlTemplate::prepare(
            "MATCH (a)-[:R]->(a) WHERE a.n=$n RETURN a LIMIT$cap", &names(),
        ).unwrap();
        let query = template.bind_parameters(&GqlParameters::new().with_int64("n", 7).unwrap()
            .with_uint64("cap", 1).unwrap()).unwrap();
        assert_eq!(db.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap().value, vec![VId(2)]);
        let artifact = db.execute_prepared_query_artifact(&query).unwrap();
        assert_eq!(artifact.rows(), &[VId(2)]);
        let (_, certificate) = db.execute_prepared_gql_certified(query.plan()).unwrap();
        let wrong = GqlPreparedResultArtifact::new(&query, basis, certificate.digest, vec![VId(1), VId(2)]);
        assert!(matches!(db.audit_prepared_query_artifact(&query, &wrong.to_bytes()),
            Err(GqlEvidenceAuditError::ResultMismatch)));

        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(11));
        txn.write(&mut db, change).unwrap();
        assert!(txn.execute_prepared_query_governed(&db, &query_cx, &query, generous()).unwrap().value.is_empty());
        txn.commit(&mut db, &commit).await.unwrap();
        assert!(db.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap().value.is_empty());
        assert_eq!(db.execute_prepared_query_governed_at(&query_cx, &query, basis, generous()).unwrap().value, vec![VId(2)]);
        assert_eq!(pinned.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap().value, vec![VId(2)]);
        db.audit_prepared_query_artifact(&query, &artifact.to_bytes()).unwrap();
        db.compact(&commit).await.unwrap();
        assert_eq!(db.execute_prepared_query_governed_at(&query_cx, &query, basis, generous()).unwrap().value, vec![VId(2)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn four_dimensions_refuse_independently_with_exact_boundary_success() {
    let ((), report) = run_async_under_lab(0xc0a2_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let db = seeded(&commit).await;
        let query = PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN b", &names()).unwrap();
        let measured = db.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap();
        assert_eq!(measured.value, vec![VId(2)]);
        let exact = GqlQueryPolicy::new(5, 1, measured.evaluator.work_units, measured.evaluator.scratch_entries);
        assert_eq!(db.execute_prepared_query_governed(&query_cx, &query, exact).unwrap(), measured);
        assert!(matches!(db.execute_prepared_query_governed(&query_cx, &query,
            GqlQueryPolicy::new(4, 1, 10_000, 10_000)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords));
        assert!(matches!(db.execute_prepared_query_governed(&query_cx, &query,
            GqlQueryPolicy::new(5, 0, 10_000, 10_000)),
            Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows));
        assert!(matches!(db.execute_prepared_query_governed(&query_cx, &query,
            GqlQueryPolicy::new(5, 1, measured.evaluator.work_units - 1, 10_000)),
            Err(GqlQueryError::Evaluator(error)) if error.dimension == GlaLimitDimension::WorkUnits));
        assert!(matches!(db.execute_prepared_query_governed(&query_cx, &query,
            GqlQueryPolicy::new(5, 1, 10_000, measured.evaluator.scratch_entries - 1)),
            Err(GqlQueryError::Evaluator(error)) if error.dimension == GlaLimitDimension::ScratchEntries));
        let skipped = PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN b SKIP 1", &names()).unwrap();
        let empty = db.execute_prepared_query_governed(&query_cx, &skipped,
            GqlQueryPolicy::new(5, 0, 10_000, 10_000)).unwrap();
        assert!(empty.value.is_empty());
        assert_eq!(empty.rows.result_rows, 0);
        assert!(!format!("{measured:?}").contains("VId(2)"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn governed_refusal_preserves_label_witnesses_without_global_insert_fencing() {
    let ((), report) = run_async_under_lab(0xc0a2_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        for winner_kind in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            let query = PreparedGqlQuery::prepare("MATCH (a:L) RETURN a", &names()).unwrap();
            assert!(matches!(txn.execute_prepared_query_governed(&db, &query_cx, &query,
                GqlQueryPolicy::new(10, 10, 0, 100)), Err(GqlQueryError::Evaluator(_))));
            let mut winner = WriteBatch::new(R);
            winner.create_vertex(VId(77), if winner_kind == 1 { vec![L] } else { vec![] }, vec![]);
            db.write(&commit, winner).await.unwrap();
            if winner_kind == 2 {
                let mut label = WriteBatch::new(R);
                label.set_vertex_label(VId(77), L, true);
                db.write(&commit, label).await.unwrap();
            }
            let result = txn.commit(&mut db, &commit).await;
            if winner_kind == 0 {
                result.expect("unrelated unlabeled insert is not a scan phantom");
                assert!(db.vertex(VId(99)).unwrap().is_some());
            } else {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
                assert!(db.vertex(VId(99)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_sources_are_not_disguised_as_zero_budget_errors() {
    let ((), report) = run_async_under_lab(0xc0a2_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let query = PreparedGqlQuery::prepare("MATCH (a)-[:R]->(a) RETURN a", &names()).unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        let future = CommitSeq(db.frontier().unwrap().0 + 1);
        assert!(matches!(db.execute_prepared_query_governed_at(&query_cx, &query, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        assert!(matches!(pinned.execute_prepared_query_governed_at(&query_cx, &query, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        let txn = db.begin(&txn_cx).unwrap();
        assert!(matches!(txn.execute_prepared_query_governed(&foreign, &query_cx, &query, zero),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        txn.abort();
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(88), vec![], vec![]);
        assert!(matches!(db.write_with_publication_failure(&commit, winner,
            DerivedPublicationStage::FoldCommittedTemplate).await,
            Err(WriteError::CommittedNeedsRecovery { .. })));
        assert!(matches!(db.execute_prepared_query_governed(&query_cx, &query, zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::RecoveryRequired(_))))));
        assert_eq!(pinned.execute_prepared_query_governed(&query_cx, &query, generous()).unwrap().value, vec![VId(2)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn governed_artifact_replay_keeps_all_identity_and_staged_effect_checks() {
    use fgdb_gql::{GqlEvidenceLimitedAuditError, GqlEvidenceLimits};
    let ((), report) = run_async_under_lab(0xc0a2_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = seeded(&commit).await;
        let pinned = db.read_session().unwrap();
        let query = PreparedGqlQuery::prepare("MATCH (a)-[:R]->(a) RETURN a", &names()).unwrap();
        let artifact = db.execute_prepared_query_artifact(&query).unwrap();
        let bytes = artifact.to_bytes();
        let limits = GqlEvidenceLimits::DEFAULT_UNTRUSTED;
        let stop = GqlQueryPolicy::new(100, 100, 0, 100);
        assert!(matches!(db.audit_prepared_query_artifact_governed(&query_cx, &query, &bytes, limits, stop),
            Err(GqlEvidenceLimitedAuditError::Audit(GqlEvidenceAuditError::Execution(
                GqlQueryError::Evaluator(_)
            )))));
        assert_eq!(db.audit_prepared_query_artifact_governed(&query_cx, &query, &bytes, limits, generous()).unwrap().rows(), &[VId(2)]);
        assert_eq!(pinned.audit_prepared_query_artifact_governed(&query_cx, &query, &bytes, limits, generous()).unwrap().rows(), &[VId(2)]);
        let other = PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN b", &names()).unwrap();
        assert!(matches!(db.audit_prepared_query_artifact_governed(&query_cx, &other, &bytes, limits, stop),
            Err(GqlEvidenceLimitedAuditError::Audit(GqlEvidenceAuditError::InputMismatch))));
        let mut txn = db.begin(&txn_cx).unwrap();
        let overlay = txn.execute_prepared_query_overlay_artifact(&db, &query).unwrap();
        let overlay_bytes = overlay.to_bytes();
        assert!(matches!(txn.audit_prepared_query_overlay_artifact_governed(&db, &query_cx,
            &query, &overlay_bytes, limits, stop),
            Err(GqlEvidenceLimitedAuditError::Audit(GqlEvidenceAuditError::Execution(
                GqlQueryError::Evaluator(_)
            )))));
        assert_eq!(txn.audit_prepared_query_overlay_artifact_governed(&db, &query_cx,
            &query, &overlay_bytes, limits, generous()).unwrap().rows(), &[VId(2)]);
        let mut changed = WriteBatch::new(R);
        changed.set_vertex_property(VId(1), N, Some(CanonicalScalar::Int(8)));
        txn.write(&mut db, changed).unwrap();
        assert_eq!(txn.execute_prepared_query_governed(&db, &query_cx, &query, generous()).unwrap().value, vec![VId(2)]);
        assert!(matches!(txn.audit_prepared_query_overlay_artifact_governed(&db, &query_cx,
            &query, &overlay_bytes, limits, stop),
            Err(GqlEvidenceLimitedAuditError::Audit(GqlEvidenceAuditError::StagedEffectMismatch))));
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
