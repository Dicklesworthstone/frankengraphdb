//! Pinned embedded read sessions and reusable bounded GQL plans
//! (`fgdb-w10-embedded-54r.1`).
//!
//! `Database::read_session` returns the existing immutable
//! `EmbeddedReadView`, and `prepare_gql_plan` exposes the executor-ready
//! `BoundPlan` as the honest reusable prepared form. One plan can therefore
//! execute repeatedly across differently pinned sessions without reparsing or
//! rebinding. This remains a read-only embedded subset, not the final
//! authorized and parameterized W10 session protocol.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const SCORE: PropertyKeyId = PropertyKeyId(7);
const QUERY: &str =
    "MATCH (a:Person)-[:R]->(b) WHERE b.score >= 7 RETURN b SKIP 1 LIMIT 1";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn bind() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_label("Person", PERSON)
        .with_property("score", SCORE)
}

/// Pid-qualified because concurrent panes share the host temp directory. The
/// test never removes a directory; each named case owns a fresh path.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-read-session-{}-{name}", std::process::id()))
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
fn one_prepared_plan_reuses_the_exact_kernel_across_pinned_sessions() {
    under_lab(0x54_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("prepared-reuse");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");

        let mut first = WriteBatch::new(R);
        first.create_vertex(VId(1), vec![PERSON], vec![]);
        first.create_vertex(
            VId(2),
            vec![],
            vec![(SCORE, CanonicalScalar::Int(7))],
        );
        first.create_vertex(
            VId(4),
            vec![],
            vec![(SCORE, CanonicalScalar::Int(9))],
        );
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        first.add_edge(EId(12), VId(1), VId(4), vec![]);
        let first_seq = db.write(cx, first).await.expect("first generation commits");

        let old = db.read_session().expect("pins first generation");
        let old_clone = old.clone();
        assert_eq!(old.frontier(), first_seq);
        assert!(
            old.shares_decoded_state_with(&old_clone),
            "cloning a read session must share the decoded immutable generation"
        );

        let prepared = old
            .prepare_gql_plan(QUERY, &bind())
            .expect("parses and binds once");
        assert_eq!(
            prepared,
            db.prepare_gql_plan(QUERY, &bind())
                .expect("the same inputs prepare the same plan")
        );

        let direct_old = db
            .execute_gql_at(QUERY, &bind(), first_seq)
            .expect("direct historical path executes");
        assert_eq!(direct_old, vec![VId(4)]);
        assert_eq!(old.execute_gql(QUERY, &bind()).unwrap(), direct_old);
        assert_eq!(old.execute_prepared_gql(&prepared).unwrap(), direct_old);
        assert_eq!(old.execute_prepared_gql(&prepared).unwrap(), direct_old);
        assert_eq!(db.execute_prepared_gql(&prepared).unwrap(), direct_old);
        let (live_old_rows, live_old_certificate) = db
            .execute_prepared_gql_certified(&prepared)
            .expect("live prepared execution certifies");
        assert_eq!(live_old_rows, direct_old);
        assert!(live_old_certificate.verifies_at(&prepared, first_seq));

        let (old_rows, old_certificate) = old
            .execute_prepared_gql_certified(&prepared)
            .expect("certifies old session execution");
        assert_eq!(old_rows, direct_old);
        assert_eq!(old_certificate.snapshot_seq, first_seq);
        assert!(old_certificate.verifies_at(&prepared, first_seq));
        assert_eq!(
            old.prepared_gql_plan_certificate(&prepared),
            db.gql_plan_certificate_at(QUERY, &bind(), first_seq)
                .expect("direct old plan certificate")
        );

        let old_root = old.partition_root();
        let old_manifest = old.manifest();
        let mut successor = WriteBatch::new(R);
        successor.create_vertex(
            VId(3),
            vec![],
            vec![(SCORE, CanonicalScalar::Int(8))],
        );
        successor.add_edge(EId(11), VId(1), VId(3), vec![]);
        let successor_seq = db
            .write(cx, successor)
            .await
            .expect("successor generation commits");
        let current = db.read_session().expect("pins successor generation");

        assert_eq!(old.frontier(), first_seq);
        assert_eq!(old.partition_root(), old_root);
        assert_eq!(old.manifest(), old_manifest);
        assert_eq!(old.execute_prepared_gql(&prepared).unwrap(), vec![VId(4)]);
        assert_eq!(current.frontier(), successor_seq);
        assert_ne!(current.partition_root(), old_root);
        assert_ne!(current.manifest(), old_manifest);
        assert_eq!(
            current.execute_prepared_gql(&prepared).unwrap(),
            vec![VId(3)]
        );
        assert_eq!(db.execute_gql(QUERY, &bind()).unwrap(), vec![VId(3)]);
        assert_eq!(db.execute_prepared_gql(&prepared).unwrap(), vec![VId(3)]);

        let (_, current_certificate) = current
            .execute_prepared_gql_certified(&prepared)
            .expect("certifies current session execution");
        assert_eq!(current_certificate.snapshot_seq, successor_seq);
        assert!(current_certificate.verifies_at(&prepared, successor_seq));
        assert_ne!(current_certificate.digest, old_certificate.digest);
        assert_ne!(current_certificate.snapshot_seq, old_certificate.snapshot_seq);
    });
}

#[test]
fn preparation_preserves_parse_and_bind_refusals() {
    under_lab(0x54_02, |cx| async move {
        let dir = scratch("prepare-refusals");
        let db = Database::create(&cx, &dir, keys()).await.expect("creates");

        let parse = db
            .prepare_gql_plan("MATCH (a) RETURN a", &bind())
            .expect_err("unlabeled node-only query is outside the bounded grammar");
        assert!(matches!(parse, GqlError::Parse(_)), "got {parse:?}");

        let unbound = db
            .prepare_gql_plan(
                "MATCH (a)-[:MISSING]->(b) RETURN b",
                &RelationBind::new(),
            )
            .expect_err("unknown relation must remain a bind refusal");
        assert!(matches!(unbound, GqlError::Bind(_)), "got {unbound:?}");
    });
}
