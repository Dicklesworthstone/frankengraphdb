//! **Incoming vs outbound plan certificates differ**
//! (`fgdb-gql-incoming-1qei`).
//!
//! The arrow's spelling is plan identity: at ONE database and ONE sequence,
//! the incoming and outbound spellings of the same surface text mint
//! different plan-certificate digests, and the two projections of the
//! incoming spelling differ from each other — while re-minting the same
//! statement is byte-identical. The crossing fixture keeps the scans
//! honestly apart (incoming `RETURN a` answers the dest, outbound `RETURN a`
//! the sources), so a digest collision cannot hide behind a working scan.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const OUTBOUND_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const INCOMING_A: &str = "MATCH (a)<-[:R]-(b) RETURN a";
const INCOMING_B: &str = "MATCH (a)<-[:R]-(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-gql-incoming-cert-{}-{name}",
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

/// The crossing fixture: `1-[:R]->2` and `3-[:R]->2`, so incoming `RETURN a`
/// (the shared dest) answers `[2]` while outbound `RETURN a` (the sources)
/// answers `[1, 3]`.
fn seed_crossing() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch.add_edge(EId(11), VId(3), VId(2), vec![]);
    batch
}

#[test]
fn incoming_and_outbound_certificates_differ_at_one_sequence() {
    let dir = scratch("cert-identity");
    under_lab(0xab_01, move |cx| async move {
        let mut db = Database::create(&cx, &dir, keys())
            .await
            .expect("creates the database");
        db.write(&cx, seed_crossing())
            .await
            .expect("seeds the crossing fixture");
        let frontier = db.frontier().expect("healthy frontier");

        let outbound_a = db
            .gql_plan_certificate(OUTBOUND_A, &bind_r())
            .expect("outbound RETURN a certifies");
        let incoming_a = db
            .gql_plan_certificate(INCOMING_A, &bind_r())
            .expect("incoming RETURN a certifies");
        let incoming_b = db
            .gql_plan_certificate(INCOMING_B, &bind_r())
            .expect("incoming RETURN b certifies");

        // One database, one sequence: every certificate names the same
        // snapshot, so ONLY the plan can separate the digests.
        assert_eq!(outbound_a.snapshot_seq, frontier);
        assert_eq!(incoming_a.snapshot_seq, frontier);
        assert_eq!(incoming_b.snapshot_seq, frontier);
        assert_ne!(
            incoming_a.digest, outbound_a.digest,
            "the arrow's spelling is plan identity: incoming and outbound \
             RETURN a must not collide"
        );
        assert_ne!(
            incoming_a.digest, incoming_b.digest,
            "the projection is plan identity: incoming RETURN a and RETURN b \
             must not collide"
        );
        assert_eq!(
            db.gql_plan_certificate(INCOMING_A, &bind_r())
                .expect("incoming RETURN a re-certifies"),
            incoming_a,
            "the same statement at the same sequence re-mints byte-identically"
        );

        // The scan cross-check: a colliding certificate cannot hide behind a
        // working scan, because the two spellings answer DIFFERENT rows on
        // this fixture.
        assert_eq!(
            db.execute_gql(INCOMING_A, &bind_r())
                .expect("incoming RETURN a executes"),
            vec![VId(2)],
            "incoming RETURN a answers the shared destination"
        );
        assert_eq!(
            db.execute_gql(OUTBOUND_A, &bind_r())
                .expect("outbound RETURN a executes"),
            vec![VId(1), VId(3)],
            "outbound RETURN a answers the sources"
        );
    });
}
