//! **Two-hop plans certify as two-hop plans**
//! (`fgdb-gql-two-hop-8pfw`, certificate slice).
//!
//! Growing the grammar grows what the certificate must distinguish. Three
//! inequalities pin the transcript at one shared sequence: the composed
//! statement is not the one-hop statement (a certificate that hashes only
//! the first hop's pattern collides here), and within the composed
//! statement `RETURN c` is not `RETURN a` (the projection stays in the
//! transcript at two hops, not just one). The same-statement determinism
//! control makes every inequality attributable to the plan alone, and the
//! executed answer rides along so a colliding certificate cannot hide
//! behind a working compose: if the digests collide while the answers
//! differ, the certificate is provably coarser than what it licenses.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const TWO_HOP_A: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a";
const ONE_HOP_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-two-hop-cert-{}-{name}", std::process::id()))
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

fn bind_rs() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

/// The composed fixture from the sibling suite: `1-[:R]->2-[:S]->4`,
/// `3-[:R]->2-[:S]->5`, and the dangling `1-[:R]->7`, so the two-hop
/// answer is `[4, 5]` while the one-hop answer is `[2, 7]`.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut r_batch = WriteBatch::new(R);
    for vid in [1u128, 2, 3, 4, 5, 7] {
        r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
    }
    r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    r_batch.add_edge(EId(11), VId(3), VId(2), vec![]);
    r_batch.add_edge(EId(12), VId(1), VId(7), vec![]);
    db.write(cx, r_batch).await.expect("R edges commit");
    let mut s_batch = WriteBatch::new(S);
    s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
    s_batch.add_edge(EId(21), VId(2), VId(5), vec![]);
    db.write(cx, s_batch).await.expect("S edges commit");
    db
}

/// Same db, same seq, same bind: the composed plan is not the one-hop
/// plan, the composed projection is in the transcript, the same statement
/// hashes identically — and the executed answers differ, so a collision
/// anywhere above is provably coarser than the behavior it certifies.
#[test]
fn two_hop_certificates_are_distinct_and_deterministic() {
    under_lab(0x2c_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("two-hop-cert");
        let db = seeded(cx, &dir).await;

        let two_hop_c = db
            .gql_plan_certificate(TWO_HOP_C, &bind_rs())
            .expect("two-hop RETURN c certifies");
        let two_hop_c_again = db
            .gql_plan_certificate(TWO_HOP_C, &bind_rs())
            .expect("two-hop RETURN c certifies again");
        let two_hop_a = db
            .gql_plan_certificate(TWO_HOP_A, &bind_rs())
            .expect("two-hop RETURN a certifies");
        let one_hop_b = db
            .gql_plan_certificate(ONE_HOP_B, &bind_rs())
            .expect("one-hop RETURN b certifies");

        assert_eq!(
            two_hop_c.snapshot_seq, one_hop_b.snapshot_seq,
            "no write happened: every digest difference below is the plan's"
        );
        assert_eq!(
            two_hop_c.digest, two_hop_c_again.digest,
            "determinism control: the same composed plan hashes identically"
        );
        assert_ne!(
            two_hop_c.digest, one_hop_b.digest,
            "the composed statement is not the one-hop statement — a \
             first-hop-only transcript collides here and can no longer say \
             which pattern it licensed"
        );
        assert_ne!(
            two_hop_c.digest, two_hop_a.digest,
            "the projection is in the transcript at two hops too: RETURN c \
             and RETURN a are different plans over one pattern"
        );

        // The behaviors the digests must keep apart, executed on the same
        // handle: composed [4, 5] versus one-hop [2, 7]. A collision above
        // while these differ is a certificate coarser than its behavior.
        assert_eq!(
            db.execute_gql(TWO_HOP_C, &bind_rs())
                .expect("two-hop RETURN c executes"),
            vec![VId(4), VId(5)]
        );
        assert_eq!(
            db.execute_gql(ONE_HOP_B, &bind_rs())
                .expect("one-hop RETURN b executes"),
            vec![VId(2), VId(7)]
        );
    });
}
