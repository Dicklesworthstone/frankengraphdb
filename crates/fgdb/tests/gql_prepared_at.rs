//! Historical prepared-query and pinned-session parity laws
//! (`fgdb-w10-embedded-54r.1`).
//!
//! One executor-ready `BoundPlan` must cross live, historical, and immutable
//! session surfaces without reparsing. Every certified face must name the exact
//! sequence that actually produced its rows, and future reads must retain the
//! ordinary typed frontier refusal with no evidence value.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, ReadError, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CommitSeq, EId, VId};
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
    std::env::temp_dir().join(format!("fgdb-prepared-at-{}-{name}", std::process::id()))
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
fn one_plan_crosses_live_historical_and_pinned_session_surfaces() {
    under_lab(0x54_11, |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, scratch("matrix"), keys())
            .await
            .expect("creates");
        let bind = bind_r();
        let plan = db
            .prepare_gql_plan(QUERY, &bind)
            .expect("parses and binds once");

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

        assert_eq!(db.execute_prepared_gql_at(&plan, first_seq).unwrap(), vec![VId(2)]);
        assert_eq!(db.execute_prepared_gql(&plan).unwrap(), vec![VId(2), VId(3)]);
        assert_eq!(old.execute_prepared_gql(&plan).unwrap(), vec![VId(2)]);
        assert_eq!(current.execute_prepared_gql(&plan).unwrap(), vec![VId(2), VId(3)]);
        assert_eq!(
            current.execute_prepared_gql_at(&plan, first_seq).unwrap(),
            vec![VId(2)]
        );
        assert_eq!(
            old.execute_prepared_gql_at(&plan, CommitSeq::ORIGIN)
                .unwrap(),
            Vec::<VId>::new()
        );

        let (historical_rows, historical_plan_certificate) = db
            .execute_prepared_gql_certified_at(&plan, first_seq)
            .expect("historical prepared execution certifies");
        assert_eq!(historical_rows, vec![VId(2)]);
        assert!(historical_plan_certificate.verifies_at(&plan, first_seq));

        let (session_rows, session_plan_certificate) = current
            .execute_prepared_gql_certified_at(&plan, first_seq)
            .expect("current session replays retained history");
        assert_eq!(session_rows, historical_rows);
        assert_eq!(session_plan_certificate, historical_plan_certificate);

        let (old_rows, old_input_certificate) = old
            .execute_gql_certified(QUERY, &bind)
            .expect("pinned text execution certifies");
        assert_eq!(old_rows, vec![VId(2)]);
        assert!(old_input_certificate.verifies_at(QUERY, &bind, first_seq));

        let (origin_rows, origin_input_certificate) = current
            .execute_gql_certified_at(QUERY, &bind, CommitSeq::ORIGIN)
            .expect("retained origin executes and certifies");
        assert!(origin_rows.is_empty());
        assert!(origin_input_certificate.verifies_at(QUERY, &bind, CommitSeq::ORIGIN));

        let future = second_seq
            .checked_successor()
            .expect("the test frontier has a successor");
        let live_err = db
            .execute_prepared_gql_certified_at(&plan, future)
            .expect_err("future live read must refuse before certification");
        assert!(matches!(
            live_err,
            GqlError::Read(ReadError::BeyondFrontier {
                asked,
                frontier,
            }) if asked == future && frontier == second_seq
        ));

        let old_err = old
            .execute_prepared_gql_certified_at(&plan, second_seq)
            .expect_err("an old session cannot observe a later generation");
        assert!(matches!(
            old_err,
            GqlError::Read(ReadError::BeyondFrontier {
                asked,
                frontier,
            }) if asked == second_seq && frontier == first_seq
        ));
    });
}
