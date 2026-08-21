//! **Undirected `execute_gql_certified_at` names the asked sequence**
//! (`fgdb-w5-parsers-nje.3`).
//!
//! The undirected twin of `gql_exec_certified_at.rs`: the pinned call
//! answers the first epoch's incidence and stamps the CALLER'S `as_of` while
//! the live call answers the grown incidence under the live frontier — the
//! two certificates differ as wholes exactly through `snapshot_seq`
//! (statement and bind digests agree across the pair: same text, same
//! bind), so a certificate that stamped the live frontier on a pinned
//! answer, or collapsed the pair, fails loudly. The directed spelling at
//! the same pinned sequence keeps its own answer, so undirectedness cannot
//! hide behind the pin.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const UNDIRECTED_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const DIRECTED_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undir-cert-at-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts).await
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
fn undirected_certified_at_names_s1_while_live_names_the_frontier() {
    under_lab(0x0e_01, |contexts| async move {
        let commit = contexts.commit();
        let dir = scratch("epochs");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.expect("first epoch commits");
        let s1 = db.frontier().expect("healthy S1 frontier");
        let mut later = WriteBatch::new(R);
        later.create_vertex(VId(3), vec![], vec![]);
        later.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(&commit, later).await.expect("second epoch commits");
        let live_frontier = db.frontier().expect("healthy live frontier");

        // The pinned pass: first-epoch incidence, CALLER'S sequence stamped.
        let (pinned_rows, pinned_cert) = db
            .execute_gql_certified_at(UNDIRECTED_B, &bind_r(), s1)
            .expect("undirected certified MATCH executes at S1");
        assert_eq!(
            pinned_rows,
            vec![VId(1), VId(2)],
            "at S1 only the durable pair is incident, both ways"
        );
        assert_eq!(
            pinned_cert.snapshot_seq, s1,
            "the pinned certificate names the asked sequence, not the frontier"
        );

        // The live pass: grown incidence under the live frontier.
        let (live_rows, live_cert) = db
            .execute_gql_certified(UNDIRECTED_B, &bind_r())
            .expect("undirected certified MATCH executes live");
        assert_eq!(
            live_rows,
            vec![VId(1), VId(2), VId(3)],
            "the second epoch's incidence expands both ways live"
        );
        assert_eq!(
            live_cert.snapshot_seq, live_frontier,
            "the live certificate names the live frontier"
        );
        assert_ne!(
            pinned_cert, live_cert,
            "the two certificates must differ as wholes — the sequence is \
             the load-bearing field"
        );
        assert_ne!(pinned_cert.snapshot_seq, live_cert.snapshot_seq);
        // Same statement, same bind: those digests agree across the pair by
        // construction — the sequence alone separates the certificates.
        assert_eq!(pinned_cert.statement_digest, live_cert.statement_digest);
        assert_eq!(pinned_cert.bind_digest, live_cert.bind_digest);

        // The directed spelling at the same pinned sequence keeps its own
        // answer: undirectedness cannot hide behind the pin.
        let (directed_rows, directed_cert) = db
            .execute_gql_certified_at(DIRECTED_B, &bind_r(), s1)
            .expect("directed certified MATCH executes at S1");
        assert_eq!(
            directed_rows,
            vec![VId(2)],
            "directed RETURN b at S1 answers only the edge-flow destination"
        );
        assert_eq!(directed_cert.snapshot_seq, s1);
    });
}
