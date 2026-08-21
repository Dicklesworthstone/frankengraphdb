//! **Undirected two-hop `execute_gql_certified_at` names the asked sequence**
//! (`fgdb-gql-undir-2hop-7mrc`).
//!
//! The two-hop twin of `gql_undirected_certified_at.rs`: the pinned call
//! composes only the first epoch's continuations and stamps the CALLER'S
//! `as_of`, the live call includes the later continuation under the live
//! frontier, and the two certificates differ as wholes exactly through
//! `snapshot_seq` (statement and bind digests agree across the pair: same
//! text, same bind). The disconnected `:S` component is the leak control at
//! both sequences, and the directed spelling at the pinned sequence keeps
//! its own narrow answer.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const UNDIRECTED_C: &str = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
const DIRECTED_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undir-2hop-cert-at-{}-{name}", std::process::id()))
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

fn bind_rs() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

#[test]
fn undirected_two_hop_certified_at_names_s1_while_live_names_the_frontier() {
    under_lab(0x0f_01, |contexts| async move {
        let commit = contexts.commit();
        let dir = scratch("epochs");
        let mut db = Database::create(&commit, &dir, keys())
            .await
            .expect("creates");
        // First epoch: the path 1-[:R]->2-[:S]->4 plus the DISCONNECTED
        // 9-[:S]->8 component no R edge can reach.
        let mut r_seed = WriteBatch::new(R);
        for vid in [1u128, 2, 4, 9, 8] {
            r_seed.create_vertex(VId(vid), vec![], vec![]);
        }
        r_seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(&commit, r_seed).await.expect("R epoch commits");
        let mut s_seed = WriteBatch::new(S);
        s_seed.add_edge(EId(20), VId(2), VId(4), vec![]);
        s_seed.add_edge(EId(21), VId(9), VId(8), vec![]);
        db.write(&commit, s_seed).await.expect("S epoch commits");
        let s1 = db.frontier().expect("healthy S1 frontier");

        // Later epoch: a second continuation from the shared via.
        let mut later = WriteBatch::new(S);
        later.create_vertex(VId(5), vec![], vec![]);
        later.add_edge(EId(22), VId(2), VId(5), vec![]);
        db.write(&commit, later).await.expect("later continuation commits");
        let live_frontier = db.frontier().expect("healthy live frontier");

        // The pinned pass: only the first epoch's continuation composes.
        let (pinned_rows, pinned_cert) = db
            .execute_gql_certified_at(UNDIRECTED_C, &bind_rs(), s1)
            .expect("undirected two-hop certified MATCH executes at S1");
        assert!(
            pinned_rows.contains(&VId(4)),
            "the first epoch's continuation composes at S1: {pinned_rows:?}"
        );
        assert!(
            !pinned_rows.contains(&VId(5)),
            "the later continuation must be invisible at S1: {pinned_rows:?}"
        );
        assert!(
            !pinned_rows.contains(&VId(8)),
            "the disconnected :S component must not compose: {pinned_rows:?}"
        );
        assert_eq!(
            pinned_cert.snapshot_seq, s1,
            "the pinned certificate names the asked sequence, not the frontier"
        );

        // The live pass: the later continuation joins, the leak stays out.
        let (live_rows, live_cert) = db
            .execute_gql_certified(UNDIRECTED_C, &bind_rs())
            .expect("undirected two-hop certified MATCH executes live");
        assert!(
            live_rows.contains(&VId(4)) && live_rows.contains(&VId(5)),
            "both continuations compose live: {live_rows:?}"
        );
        assert!(
            !live_rows.contains(&VId(8)),
            "the disconnected :S component must not compose live: {live_rows:?}"
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

        // The directed spelling at the pinned sequence: still the narrow
        // edge-flow composition, so undirectedness cannot hide behind the pin.
        let (directed_rows, directed_cert) = db
            .execute_gql_certified_at(DIRECTED_C, &bind_rs(), s1)
            .expect("directed two-hop certified MATCH executes at S1");
        assert_eq!(
            directed_rows,
            vec![VId(4)],
            "directed RETURN c at S1 answers only the edge-flow composition"
        );
        assert_eq!(directed_cert.snapshot_seq, s1);
    });
}
