//! Exact ordered-result evidence for the bounded GQL execution surface
//! (`fgdb-gate-genesis-lce.2`).
//!
//! Input and plan certificates already name one successful execution. This
//! suite closes the next evidence layer: the plan certificate must bind the
//! exact ordered rows, remain replayable at an older sequence after the live
//! graph advances, and never be minted for a refused future read.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, ReadError, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const QUERY: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-gql-result-digest-{}-{name}",
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
fn ordered_result_digest_replays_after_the_live_graph_advances() {
    under_lab(0xc3_01, |cx| async move {
        let dir = scratch("historical-replay");
        let mut db = Database::create(&cx, &dir, keys()).await.expect("creates");
        let bind = bind_r();
        let plan = db.prepare_gql_plan(QUERY, &bind).expect("statement binds");

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        let first_seq = db.write(&cx, first).await.expect("first commit lands");

        let (first_rows, first_input, first_plan, first_result) = db
            .execute_gql_with_result_digest_at(QUERY, &bind, first_seq)
            .expect("historical result evidence is issued");
        assert_eq!(first_rows, vec![VId(2)]);
        assert!(first_input.verifies_at(QUERY, &bind, first_seq));
        assert!(first_plan.verifies_at(&plan, first_seq));
        assert!(first_plan.verifies_result_digest(&first_rows, first_result));
        assert!(!first_plan.verifies_result_digest(&[VId(3)], first_result));
        let (prepared_first_rows, prepared_first_plan, prepared_first_result) = db
            .execute_prepared_gql_with_result_digest_at(&plan, first_seq)
            .expect("prepared historical evidence is issued");
        assert_eq!(prepared_first_rows, first_rows);
        assert_eq!(prepared_first_plan, first_plan);
        assert_eq!(prepared_first_result, first_result);

        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(11), VId(1), VId(3), vec![]);
        let second_seq = db.write(&cx, second).await.expect("second commit lands");

        let (live_rows, live_input, live_plan, live_result) = db
            .execute_gql_with_result_digest(QUERY, &bind)
            .expect("live result evidence is issued");
        assert_eq!(live_rows, vec![VId(2), VId(3)]);
        assert!(live_input.verifies_at(QUERY, &bind, second_seq));
        assert!(live_plan.verifies_at(&plan, second_seq));
        assert!(live_plan.verifies_result_digest(&live_rows, live_result));
        assert_ne!(first_result, live_result);
        let (prepared_live_rows, prepared_live_plan, prepared_live_result) = db
            .execute_prepared_gql_with_result_digest(&plan)
            .expect("prepared live evidence is issued");
        assert_eq!(prepared_live_rows, live_rows);
        assert_eq!(prepared_live_plan, live_plan);
        assert_eq!(prepared_live_result, live_result);

        let (replayed_rows, replayed_input, replayed_plan, replayed_result) = db
            .execute_gql_with_result_digest_at(QUERY, &bind, first_seq)
            .expect("old result replays after advancement");
        assert_eq!(replayed_rows, first_rows);
        assert_eq!(replayed_input, first_input);
        assert_eq!(replayed_plan, first_plan);
        assert_eq!(replayed_result, first_result);

        let view = db.read_session().expect("pins the live generation");
        let (view_rows, view_input, view_plan, view_result) = view
            .execute_gql_with_result_digest_at(QUERY, &bind, first_seq)
            .expect("read view uses the same historical evidence path");
        assert_eq!(view_rows, first_rows);
        assert_eq!(view_input, first_input);
        assert_eq!(view_plan, first_plan);
        assert_eq!(view_result, first_result);
        let (view_prepared_rows, view_prepared_plan, view_prepared_result) = view
            .execute_prepared_gql_with_result_digest_at(&plan, first_seq)
            .expect("prepared read-view evidence uses the same historical path");
        assert_eq!(view_prepared_rows, first_rows);
        assert_eq!(view_prepared_plan, first_plan);
        assert_eq!(view_prepared_result, first_result);

        let future = second_seq
            .checked_successor()
            .expect("test frontier has a successor");
        let error = db
            .execute_gql_with_result_digest_at(QUERY, &bind, future)
            .expect_err("future reads mint no evidence");
        assert!(matches!(
            error,
            GqlError::Read(ReadError::BeyondFrontier { asked, frontier })
                if asked == future && frontier == second_seq
        ));
    });
}
