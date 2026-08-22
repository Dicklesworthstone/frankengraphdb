//! **The plan certificate sees the projection**
//! (`fgdb-gql-cert-proj-rtfq`).
//!
//! `RETURN a` and `RETURN b` are two different plans over one pattern: same
//! scan, different projected variable, different answers. A certificate
//! that hashes only pattern + relation + seq collides across them —
//! certifying "this plan answered" while unable to say WHICH of two
//! different answers it licensed. The row assertions ride along so a
//! colliding certificate cannot hide behind a working projection: if the
//! digests collide while the answers differ, the certificate is provably
//! coarser than the behavior it certifies.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const RETURN_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-cert-proj-{}-{name}", std::process::id()))
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

/// Two edges into one destination: source set `[1, 3]`, destination set
/// `[2]` — the two projections answer differently by construction.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.add_edge(EId(10), VId(3), VId(2), vec![]);
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Same db, same seq, same pattern, same bind — different projected
/// variable, different digest. Determinism control beside it: the same
/// statement twice is the same digest, so the inequality above is the
/// projection's and nothing else's.
#[test]
fn the_projection_is_in_the_certificate_transcript() {
    under_lab(0xcf_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("projection");
        let db = seeded(cx, &dir).await;

        let cert_a = db
            .gql_plan_certificate(RETURN_A, &bind_r())
            .expect("RETURN a certifies");
        let cert_a_again = db
            .gql_plan_certificate(RETURN_A, &bind_r())
            .expect("RETURN a certifies again");
        let cert_b = db
            .gql_plan_certificate(RETURN_B, &bind_r())
            .expect("RETURN b certifies");

        assert_eq!(
            cert_a.snapshot_seq, cert_b.snapshot_seq,
            "no write happened: any digest difference below is the plan's"
        );
        assert_eq!(
            cert_a.digest, cert_a_again.digest,
            "determinism control: the same plan hashes identically"
        );
        assert_ne!(
            cert_a.digest, cert_b.digest,
            "RETURN a and RETURN b are two plans; a certificate that hashes \
             pattern-without-projection collides here and can no longer say \
             which answer it licensed"
        );

        // The answers differ while the seq is shared — so if the digests
        // ever collide, the certificate is provably coarser than the
        // behavior it certifies.
        assert_eq!(
            db.execute_gql(RETURN_A, &bind_r())
                .expect("RETURN a executes"),
            vec![VId(1), VId(3)]
        );
        assert_eq!(
            db.execute_gql(RETURN_B, &bind_r())
                .expect("RETURN b executes"),
            vec![VId(2)]
        );
    });
}
