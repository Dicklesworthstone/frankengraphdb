//! **Undirected plans certify as undirected plans**
//! (`fgdb-w5-parsers-nje.2`, certificate slice).
//!
//! The direction is in the transcript: the undirected `RETURN b` and the
//! outgoing `RETURN b` are two plans over one relation, and their digests
//! must differ at one shared sequence. The same-statement determinism
//! control attributes the inequality to the direction alone, and the
//! executed answers ride along — `[1, 2, 3]` undirected vs `[2]` directed
//! — so a colliding certificate cannot hide behind a working directed
//! scan: it would be provably coarser than the two behaviors it claims to
//! license.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const UN_RETURN_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const OUT_RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undirected-cert-{}-{name}", std::process::id()))
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

/// Direction in the transcript, behavior riding along.
#[test]
fn the_undirected_certificate_differs_from_the_directed_one() {
    under_lab(0x3e_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("direction-in-transcript");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.create_vertex(VId(3), vec![], vec![]);
        seed.add_edge(EId(10), VId(3), VId(2), vec![]);
        seed.add_edge(EId(11), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");

        let undirected = db
            .gql_plan_certificate(UN_RETURN_B, &bind_r())
            .expect("undirected RETURN b certifies");
        let undirected_again = db
            .gql_plan_certificate(UN_RETURN_B, &bind_r())
            .expect("undirected RETURN b certifies again");
        let outgoing = db
            .gql_plan_certificate(OUT_RETURN_B, &bind_r())
            .expect("outgoing RETURN b certifies");

        assert_eq!(
            undirected.snapshot_seq, outgoing.snapshot_seq,
            "no write happened: any digest difference below is the plan's"
        );
        assert_eq!(
            undirected.digest, undirected_again.digest,
            "determinism control: the same undirected plan hashes identically"
        );
        assert_ne!(
            undirected.digest, outgoing.digest,
            "the direction is in the transcript — a direction-blind \
             certificate collides here and cannot say which scan it licensed"
        );

        // The behaviors the digests must keep apart, on the same handle.
        assert_eq!(
            db.execute_gql(UN_RETURN_B, &bind_r()).expect("undirected executes"),
            vec![VId(1), VId(2), VId(3)],
            "a colliding certificate cannot hide behind directed dests"
        );
        assert_eq!(
            db.execute_gql(OUT_RETURN_B, &bind_r()).expect("outgoing executes"),
            vec![VId(2)]
        );
    });
}
