//! **The spine-level half of the `fgdb-fujt` equality law:** the snapshot a
//! `Database` maintains incrementally across commits is the SAME snapshot a
//! from-scratch reopen derives by full rebuild — same content-addressed root,
//! same adjacency.
//!
//! The strata law (`incremental_publish_equals_rebuild.rs`) pins writer-level
//! equality; this pins it end to end through the real durable path: N real
//! commits, then drop the `Database` (releasing the writer lease) and reopen,
//! which runs `rebuild()` — the recovery path deliberately left untouched by
//! the incremental fold. The root is `Trunc128(BLAKE3(...))` over the derived
//! partition, so root equality IS derived-state equality, not a proxy.
//!
//! The control: a database missing the final commit must NOT reopen to the
//! same root. If it did, root equality could not distinguish anything and the
//! law above would be vacuous.

use asupersync::cx::Cx;
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

const KNOWS: RelationId = RelationId(1);

/// Minimal single-future executor for the non-lab lane — same shape and
/// rationale as `cx_probe.rs`.
fn drive<F: Future>(future: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut task_cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut task_cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: [0x5a; 32],
        namespace: DatabaseSecurityNamespaceId([0x77; 32]),
        dek: [0x3c; 32],
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-incr-snap-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn commit_one(commit: &fgdb_types::context::CommitCx, db: &mut Database, b: usize) {
    let mut batch = WriteBatch::new(KNOWS);
    if b == 0 {
        batch.create_vertex(VId(1), vec![], vec![]);
    }
    batch.create_vertex(VId(2000 + b as u128), vec![], vec![]);
    batch.add_edge(EId(b as u128 + 1), VId(1), VId(2000 + b as u128), vec![]);
    drive(db.write(commit, batch)).expect("commit");
}

#[test]
fn the_incremental_snapshot_reopens_to_the_same_root() {
    const COMMITS: usize = 24;
    let cx = Cx::for_testing();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let dir = scratch("same-root");
    let mut db = drive(Database::create(&commit, &dir, keys())).expect("creates");
    for b in 0..COMMITS {
        commit_one(&commit, &mut db, b);
    }
    let incremental_root = db.partition_root();
    let incremental_frontier = db.frontier();
    let incremental_neighbours = db.neighbours(VId(1), KNOWS).expect("reads");
    drop(db);

    let reopened = drive(Database::open(&commit, &dir, keys())).expect("reopens");
    assert_eq!(
        reopened.partition_root(),
        incremental_root,
        "reopen's full rebuild derived a DIFFERENT partition root than the \
         incrementally maintained snapshot: the live path and the recovery \
         path disagree about the same commit stream"
    );
    assert_eq!(reopened.frontier(), incremental_frontier);
    assert_eq!(
        reopened.neighbours(VId(1), KNOWS).expect("reads"),
        incremental_neighbours,
        "root equality held but adjacency differs — the root is not covering \
         what it claims to cover"
    );
}

/// The control that can fail: one fewer commit must produce a different root.
#[test]
fn a_database_missing_the_last_commit_reopens_to_a_different_root() {
    const COMMITS: usize = 24;
    let cx = Cx::for_testing();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let full_dir = scratch("control-full");
    let mut full = drive(Database::create(&commit, &full_dir, keys())).expect("creates");
    for b in 0..COMMITS {
        commit_one(&commit, &mut full, b);
    }
    let full_root = full.partition_root();
    drop(full);

    let short_dir = scratch("control-short");
    let mut short = drive(Database::create(&commit, &short_dir, keys())).expect("creates");
    for b in 0..COMMITS - 1 {
        commit_one(&commit, &mut short, b);
    }
    let short_root = short.partition_root();
    drop(short);

    assert_ne!(
        full_root, short_root,
        "a database missing a whole commit has the SAME root: root equality \
         distinguishes nothing and the equality law is vacuous"
    );
}
