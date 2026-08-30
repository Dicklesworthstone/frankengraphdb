//! `:memory:` acceptance for the embedded spine (README row: the embedded
//! API — `Database::open(":memory:")`).
//!
//! What these tests PROVE, and how:
//!
//! - A memory database is not a mock. [`Database::<MemVfs>::open_memory`]
//!   funnels into the same [`Database::create_with_vfs`] law a disk database
//!   obeys, and every write below goes through Chronicle's real two-fsync
//!   commit protocol and Strata's real content-addressed publication — the
//!   [`MemVfs`] under it simply holds the bytes in RAM over a sparse shadow
//!   namespace (see `fgdb::memvfs` module docs for why the shadow exists).
//! - Reads answer from the published tier-D blocks after the full
//!   encode → address → fsync-barrier → decode round trip; `compact` runs
//!   the real republish; GQL MATCH runs the pinned parser→binder→executor
//!   pipeline.
//! - The durability CONTRACT differs by design, and the tests pin the
//!   contract, not a weaker substitute: a memory database is private to its
//!   [`MemVfs`], so a fresh `open_memory` is a fresh empty graph, and
//!   dropping the handle drops the database. What survives is exactly what a
//!   retained handle can reach — reopen goes through `open_with_vfs` over a
//!   caller-held `MemVfs` clone and recovers the fold from the in-memory
//!   commit stream, with Chronicle's writer lease still refusing a second
//!   live open.
//!
//! There is no test-only commit path, no in-memory shortcut, and no bypassed
//! barrier in this file: the same protocol code that serves a disk database
//! serves every assertion below.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, OpenError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// The one bind every test uses: the statement's `R` resolves to
/// `RelationId(1)` exactly as the disk-backed spine tests do.
fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// One `WriteBatch` building the canonical two-vertex, one-edge graph.
fn one_edge_batch() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: Future<Output = T> + Send + 'static,
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

/// The full spine over memory: open, commit through the real two-fsync
/// protocol, answer point reads and pinned GQL MATCH from the published
/// blocks, compact (a real forced republish), and answer identically after.
#[test]
fn memory_database_commits_through_the_real_protocol_and_answers_reads() {
    under_lab(0x4D_A1, |cx| async move {
        let cx = &cx;
        let mut db = Database::<MemVfs>::open_memory(cx, keys())
            .await
            .expect("opens a fresh in-memory database");

        db.write(cx, one_edge_batch())
            .await
            .expect("commits through the real two-fsync path over MemVfs");

        assert_eq!(
            db.neighbours(VId(1), R).expect("neighbours"),
            vec![VId(2)],
            "adjacency answers from the published tier-D blocks"
        );
        let vertex = db.vertex(VId(1)).expect("vertex read");
        assert!(vertex.is_some(), "the created vertex is readable");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("pinned MATCH executes"),
            vec![VId(2)]
        );

        // Compaction is the real republish, over the same memory filesystem.
        db.compact(cx).await.expect("compacts");
        assert_eq!(
            db.neighbours(VId(1), R).expect("neighbours after compact"),
            vec![VId(2)]
        );
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("MATCH after compact"),
            vec![VId(2)]
        );
    });
}

/// Memory databases are private and mortal: two concurrently-alive
/// `open_memory` databases are independent graphs, and a dropped handle's
/// graph is gone — a fresh open is empty, never a resurrection.
#[test]
fn memory_databases_are_private_and_lost_on_drop() {
    under_lab(0x4D_A2, |cx| async move {
        let cx = &cx;
        let mut first = Database::<MemVfs>::open_memory(cx, keys())
            .await
            .expect("opens");
        first.write(cx, one_edge_batch()).await.expect("commits");
        assert_eq!(
            first.execute_gql(PINNED, &bind_r()).expect("MATCH"),
            vec![VId(2)]
        );

        // A second, simultaneously-alive memory database shares nothing:
        // its own namespace, its own lease, its own empty graph.
        let second = Database::<MemVfs>::open_memory(cx, keys())
            .await
            .expect("opens an independent database");
        assert_eq!(
            second.execute_gql(PINNED, &bind_r()).expect("MATCH"),
            Vec::<VId>::new(),
            "the private graph of another handle is not visible"
        );

        // The contract: lost on drop. No path, no recovery, no residue.
        drop(first);
        let third = Database::<MemVfs>::open_memory(cx, keys())
            .await
            .expect("opens");
        assert_eq!(
            third.execute_gql(PINNED, &bind_r()).expect("MATCH"),
            Vec::<VId>::new(),
            "a fresh memory database cannot have a prior history"
        );
        assert!(
            third.vertex(VId(1)).expect("vertex read").is_none(),
            "the dropped database's vertices are gone"
        );
    });
}

/// The reopen story memory databases DO have: retain a clone of the `MemVfs`
/// and reopen through `open_with_vfs`. Recovery runs the real protocol —
/// Chronicle recovers the marker chain from the in-memory commit log and
/// rebuilds from capsules — while the single-writer lease still refuses a
/// second live handle, exactly as it does on disk.
#[test]
fn retained_memvfs_reopens_and_writer_lease_still_governs() {
    under_lab(0x4D_A3, |cx| async move {
        let cx = &cx;
        let vfs = MemVfs::new().expect("memory filesystem");
        let dir = vfs.database_dir();

        let mut db = Database::<MemVfs>::create_with_vfs(cx, vfs.clone(), dir.clone(), keys())
            .await
            .expect("creates through the standard creation law");
        db.write(cx, one_edge_batch()).await.expect("commits");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r()).expect("MATCH"),
            vec![VId(2)]
        );

        // While the first handle lives, the writer lease refuses a second
        // open — process-liveness authority works over the shadow too.
        let refused = Database::<MemVfs>::open_with_vfs(cx, vfs.clone(), dir.clone(), keys()).await;
        assert!(
            matches!(
                refused,
                Err(OpenError::Commit(
                    fgdb_chronicle::commit::CommitError::WriterAlreadyOpen
                ))
            ),
            "second live open must be refused by the writer lease, got {refused:?}"
        );

        // The handle drops; the retained MemVfs keeps the bytes. Reopen
        // recovers the fold from the in-memory commit stream and answers
        // identically.
        drop(db);
        let mut db = Database::<MemVfs>::open_with_vfs(cx, vfs, dir, keys())
            .await
            .expect("reopens through the retained handle");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("MATCH after reopen"),
            vec![VId(2)],
            "the committed graph survives the handle via its MemVfs"
        );
        assert_eq!(
            db.neighbours(VId(1), R).expect("neighbours after reopen"),
            vec![VId(2)]
        );

        // A reopened memory database keeps taking real commits.
        let mut second_batch = WriteBatch::new(R);
        second_batch.create_vertex(VId(3), vec![], vec![]);
        second_batch.add_edge(EId(11), VId(2), VId(3), vec![]);
        db.write(cx, second_batch)
            .await
            .expect("commits after reopen");
        assert_eq!(
            db.neighbours(VId(2), R).expect("fresh adjacency"),
            vec![VId(3)]
        );
    });
}
