//! Refusal and cross-surface laws for plan-only GQL certificates
//! (`fgdb-w4-g1-txn-core-qpmg.22`).
//!
//! The certificate surface may bind a live or historical plan, but it may not
//! clamp a future `as_of`, mint evidence from an unbound statement, or diverge
//! from the plan certificate returned by prepared execution at the same
//! frontier.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, ReadError, RelationBind};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x77; 32]),
        [0x3c; 32],
    )
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-plan-certificate-refusal-{}-{name}",
        std::process::id()
    ))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

#[test]
fn future_plan_certificate_is_a_typed_beyond_frontier_refusal() {
    under_lab(0x9c_b1, |cx| async move {
        let db = Database::create(&cx, scratch("future"), keys())
            .await
            .expect("creates");
        let frontier = db.frontier().expect("healthy frontier");
        let future = frontier.checked_successor().expect("genesis has a successor");

        let err = db
            .gql_plan_certificate_at(QUERY, &bind_r(), future)
            .expect_err("future certificate must be refused");
        assert!(matches!(
            err,
            GqlError::Read(ReadError::BeyondFrontier {
                asked,
                frontier: seen,
            }) if asked == future && seen == frontier
        ));
    });
}

#[test]
fn unbound_plan_certificate_is_a_typed_bind_refusal() {
    under_lab(0x9c_b2, |cx| async move {
        let db = Database::create(&cx, scratch("unbound"), keys())
            .await
            .expect("creates");
        let err = db
            .gql_plan_certificate(QUERY, &RelationBind::new())
            .expect_err("unbound relation must be refused");
        assert!(matches!(err, GqlError::Bind(_)));
    });
}

#[test]
fn live_plan_only_and_prepared_execution_certificates_are_identical() {
    under_lab(0x9c_b3, |cx| async move {
        let db = Database::create(&cx, scratch("cross-surface"), keys())
            .await
            .expect("creates");
        let bind = bind_r();
        let plan = db
            .prepare_gql_plan(QUERY, &bind)
            .expect("the statement binds");
        let plan_only = db
            .gql_plan_certificate(QUERY, &bind)
            .expect("plan-only certificate is issued");
        let (rows, executing) = db
            .execute_prepared_gql_certified(&plan)
            .expect("prepared execution succeeds");

        assert!(rows.is_empty(), "the fresh graph has no matches");
        assert_eq!(plan_only, executing);
        assert!(plan_only.verifies_at(&plan, db.frontier().expect("healthy frontier")));
    });
}
