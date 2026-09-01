//! Aligned input + plan evidence from one bounded GQL execution
//! (`fgdb-gate-genesis-lce.2`).
//!
//! The combined surface binds once, executes once through the shared snapshot
//! kernel, and only then returns the existing input and plan certificates.
//! Both evidence values must name the exact sequence that produced the rows.
//! Neither certificate claims to attest those rows.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, ReadError, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
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

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-aligned-certificates-{}-{name}",
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

#[test]
fn one_execution_aligns_both_certificate_layers_across_surfaces() {
    under_lab(0x1c_e2, |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, scratch("alignment"), keys())
            .await
            .expect("creates");
        let bind = bind_r();
        let plan = db.prepare_gql_plan(QUERY, &bind).expect("binds once");

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        let first_seq = db.write(cx, first).await.expect("first commit lands");
        let old = db.read_session().expect("pins first generation");

        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(11), VId(1), VId(3), vec![]);
        let second_seq = db.write(cx, second).await.expect("second commit lands");
        let current = db.read_session().expect("pins second generation");

        let (historical_rows, historical_input, historical_plan) = db
            .execute_gql_with_certificates_at(QUERY, &bind, first_seq)
            .expect("historical aligned execution succeeds");
        assert_eq!(historical_rows, vec![VId(2)]);
        assert_eq!(historical_input.snapshot_seq, first_seq);
        assert_eq!(historical_plan.snapshot_seq, first_seq);
        assert!(historical_input.verifies_at(QUERY, &bind, first_seq));
        assert!(historical_plan.verifies_at(&plan, first_seq));

        let (separate_rows, separate_input) = db
            .execute_gql_certified_at(QUERY, &bind, first_seq)
            .expect("separate input certificate succeeds");
        let (prepared_rows, separate_plan) = db
            .execute_prepared_gql_certified_at(&plan, first_seq)
            .expect("separate plan certificate succeeds");
        assert_eq!(historical_rows, separate_rows);
        assert_eq!(historical_rows, prepared_rows);
        assert_eq!(historical_input, separate_input);
        assert_eq!(historical_plan, separate_plan);

        let (old_rows, old_input, old_plan) = old
            .execute_gql_with_certificates(QUERY, &bind)
            .expect("old session aligns evidence");
        assert_eq!(old_rows, historical_rows);
        assert_eq!(old_input, historical_input);
        assert_eq!(old_plan, historical_plan);

        let (replayed_rows, replayed_input, replayed_plan) = current
            .execute_gql_with_certificates_at(QUERY, &bind, first_seq)
            .expect("current session replays first generation");
        assert_eq!(replayed_rows, historical_rows);
        assert_eq!(replayed_input, historical_input);
        assert_eq!(replayed_plan, historical_plan);

        let (live_rows, live_input, live_plan) = db
            .execute_gql_with_certificates(QUERY, &bind)
            .expect("live aligned execution succeeds");
        assert_eq!(live_rows, vec![VId(2), VId(3)]);
        assert_eq!(live_input.snapshot_seq, second_seq);
        assert_eq!(live_plan.snapshot_seq, second_seq);
        assert!(live_input.verifies_at(QUERY, &bind, second_seq));
        assert!(live_plan.verifies_at(&plan, second_seq));
        assert_ne!(live_input.snapshot_seq, historical_input.snapshot_seq);
        assert_ne!(live_plan.digest, historical_plan.digest);
    });
}

#[test]
fn refused_or_unbound_execution_returns_no_evidence_tuple() {
    under_lab(0x1c_e3, |cx| async move {
        let db = Database::create(&cx, scratch("refusals"), keys())
            .await
            .expect("creates");
        let frontier = db.frontier().expect("healthy frontier");
        let future = frontier
            .checked_successor()
            .expect("the test frontier has a successor");

        let future_error = db
            .execute_gql_with_certificates_at(QUERY, &bind_r(), future)
            .expect_err("future execution must refuse before evidence exists");
        assert!(matches!(
            future_error,
            GqlError::Read(ReadError::BeyondFrontier {
                asked,
                frontier: seen,
            }) if asked == future && seen == frontier
        ));

        let bind_error = db
            .execute_gql_with_certificates(QUERY, &RelationBind::new())
            .expect_err("unbound execution must refuse before evidence exists");
        assert!(matches!(bind_error, GqlError::Bind(_)));

        let parse_error = db
            .execute_gql_with_certificates("MATCH (a) RETURN a", &bind_r())
            .expect_err("off-grammar execution must refuse before evidence exists");
        assert!(matches!(parse_error, GqlError::Parse(_)));
    });
}
