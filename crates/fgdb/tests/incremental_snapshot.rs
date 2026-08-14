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

use asupersync::{Budget, cx::Cx, runtime::Runtime, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, WriteBatch};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
use std::path::PathBuf;

const KNOWS: RelationId = RelationId(1);

fn production_runtime() -> (Runtime, Cx) {
    let runtime = RuntimeBuilder::new().build().expect("production runtime");
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    (runtime, cx)
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

fn commit_one(
    runtime: &Runtime,
    commit: &fgdb_types::context::CommitCx,
    db: &mut Database,
    b: usize,
) {
    let mut batch = WriteBatch::new(KNOWS);
    if b == 0 {
        batch.create_vertex(VId(1), vec![], vec![]);
    }
    batch.create_vertex(VId(2000 + b as u128), vec![], vec![]);
    batch.add_edge(EId(b as u128 + 1), VId(1), VId(2000 + b as u128), vec![]);
    runtime.block_on(db.write(commit, batch)).expect("commit");
}

#[test]
fn the_incremental_snapshot_reopens_to_the_same_root() {
    const COMMITS: usize = 24;
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let dir = scratch("same-root");
    let mut db = runtime
        .block_on(Database::create(&commit, &dir, keys()))
        .expect("creates");
    for b in 0..COMMITS {
        commit_one(&runtime, &commit, &mut db, b);
    }
    let incremental_root = db.partition_root().expect("healthy root");
    let incremental_frontier = db.frontier().expect("healthy frontier");
    let incremental_neighbours = db.neighbours(VId(1), KNOWS).expect("reads");
    drop(db);

    let reopened = runtime
        .block_on(Database::open(&commit, &dir, keys()))
        .expect("reopens");
    assert_eq!(
        reopened.partition_root().expect("healthy reopened root"),
        incremental_root,
        "reopen's full rebuild derived a DIFFERENT partition root than the \
         incrementally maintained snapshot: the live path and the recovery \
         path disagree about the same commit stream"
    );
    assert_eq!(
        reopened.frontier().expect("healthy reopened frontier"),
        incremental_frontier
    );
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
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();

    let full_dir = scratch("control-full");
    let mut full = runtime
        .block_on(Database::create(&commit, &full_dir, keys()))
        .expect("creates");
    for b in 0..COMMITS {
        commit_one(&runtime, &commit, &mut full, b);
    }
    let full_root = full.partition_root().expect("healthy full root");
    drop(full);

    let short_dir = scratch("control-short");
    let mut short = runtime
        .block_on(Database::create(&commit, &short_dir, keys()))
        .expect("creates");
    for b in 0..COMMITS - 1 {
        commit_one(&runtime, &commit, &mut short, b);
    }
    let short_root = short.partition_root().expect("healthy short root");
    drop(short);

    assert_ne!(
        full_root, short_root,
        "a database missing a whole commit has the SAME root: root equality \
         distinguishes nothing and the equality law is vacuous"
    );
}

/// FGSV V2 restatement on the live path: a later commit tombs the birth and
/// publishes a content successor. Incremental snapshot carry-forward keeps
/// the birth patch and decodes the new one; reopen rebuilds both from the
/// stream. Root AND the version chain at every seq must agree — a root
/// match that answered the pre-update row at the frontier would hide the
/// exact bug `vertex_content_entry` is one merge away from.
#[test]
fn vertex_content_restatement_reopens_to_the_same_root_and_chain() {
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();
    let dir = scratch("vertex-restatement");
    let key = PropertyKeyId(7);
    let label = LabelId(3);

    let mut db = runtime
        .block_on(Database::create(&commit, &dir, keys()))
        .expect("creates");
    let mut birth = WriteBatch::new(KNOWS);
    birth.create_vertex(VId(1), vec![], vec![]);
    runtime.block_on(db.write(&commit, birth)).expect("birth");

    let mut restated = WriteBatch::new(KNOWS);
    restated.set_vertex_label(VId(1), label, true);
    restated.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(1)));
    runtime
        .block_on(db.write(&commit, restated))
        .expect("label+prop");

    let mut again = WriteBatch::new(KNOWS);
    again.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(9)));
    runtime.block_on(db.write(&commit, again)).expect("prop-2");

    let incremental_root = db.partition_root().expect("healthy root");
    let incremental_frontier = db.frontier().expect("healthy frontier");
    let incremental_now = db.vertex(VId(1)).expect("reads").expect("live");
    let incremental_at_birth = db
        .vertex_at(VId(1), CommitSeq(1))
        .expect("reads birth seq")
        .expect("visible at birth");
    let incremental_at_restatement = db
        .vertex_at(VId(1), CommitSeq(2))
        .expect("reads restatement seq")
        .expect("visible at restatement");
    let incremental_versions = db.element_versions().expect("healthy").clone();
    drop(db);

    let reopened = runtime
        .block_on(Database::open(&commit, &dir, keys()))
        .expect("reopens");
    assert_eq!(
        reopened.partition_root().expect("healthy reopened root"),
        incremental_root,
        "reopen rebuilt a different root after vertex restatement"
    );
    assert_eq!(
        reopened.frontier().expect("healthy reopened frontier"),
        incremental_frontier
    );
    let reopened_now = reopened.vertex(VId(1)).expect("reads").expect("live");
    assert_eq!(reopened_now.labels, vec![label]);
    assert_eq!(reopened_now.props, vec![(key, CanonicalScalar::Int(9))]);
    assert_eq!(
        reopened_now, incremental_now,
        "frontier vertex row diverged after restatement"
    );
    let reopened_at_birth = reopened
        .vertex_at(VId(1), CommitSeq(1))
        .expect("reads birth seq")
        .expect("visible at birth");
    assert!(reopened_at_birth.labels.is_empty());
    assert!(reopened_at_birth.props.is_empty());
    assert_eq!(
        reopened_at_birth, incremental_at_birth,
        "pre-update snapshot diverged after restatement"
    );
    let reopened_at_restatement = reopened
        .vertex_at(VId(1), CommitSeq(2))
        .expect("reads restatement seq")
        .expect("visible at restatement");
    assert_eq!(reopened_at_restatement.labels, vec![label]);
    assert_eq!(
        reopened_at_restatement.props,
        vec![(key, CanonicalScalar::Int(1))]
    );
    assert_eq!(
        reopened_at_restatement, incremental_at_restatement,
        "mid-chain restatement snapshot diverged"
    );
    assert_eq!(
        reopened.element_versions().expect("healthy"),
        &incremental_versions,
        "checkpoint-derived version heads drifted from the incremental fold \
         after vertex restatement — the graph answers hide this (fgdb-l96k)"
    );
    assert!(
        incremental_versions.contains_key(&ElementId::Vertex(VId(1))),
        "the restated vertex must still have a live version head"
    );
}

/// Edge content successor then clear: the retiring statement must keep its
/// pre-update row (fgdb-yqor). Incremental publish seals the birth, then a
/// later patch tombs it; reopen must reconstruct the same chain.
#[test]
fn edge_content_restatement_reopens_to_the_same_root_and_chain() {
    let (runtime, cx) = production_runtime();
    let commit = PurposeContexts::narrow_runtime_root(&cx).commit();
    let dir = scratch("edge-restatement");
    let key = PropertyKeyId(7);

    let mut db = runtime
        .block_on(Database::create(&commit, &dir, keys()))
        .expect("creates");
    let mut birth = WriteBatch::new(KNOWS);
    birth.create_vertex(VId(1), vec![], vec![]);
    birth.create_vertex(VId(2), vec![], vec![]);
    birth.add_edge(EId(10), VId(1), VId(2), vec![]);
    runtime.block_on(db.write(&commit, birth)).expect("birth");

    let mut set = WriteBatch::new(KNOWS);
    set.set_edge_property(EId(10), key, Some(CanonicalScalar::Int(4)));
    runtime.block_on(db.write(&commit, set)).expect("set");

    let mut clear = WriteBatch::new(KNOWS);
    clear.set_edge_property(EId(10), key, None);
    runtime.block_on(db.write(&commit, clear)).expect("clear");

    let incremental_root = db.partition_root().expect("healthy root");
    let incremental_frontier = db.frontier().expect("healthy frontier");
    let incremental_now = db.edge(EId(10)).expect("reads").expect("live");
    let incremental_at_set = db
        .edge_at(EId(10), CommitSeq(2))
        .expect("reads set seq")
        .expect("visible at set");
    let incremental_versions = db.element_versions().expect("healthy").clone();
    drop(db);

    let reopened = runtime
        .block_on(Database::open(&commit, &dir, keys()))
        .expect("reopens");
    assert_eq!(
        reopened.partition_root().expect("healthy reopened root"),
        incremental_root,
        "reopen rebuilt a different root after edge restatement"
    );
    assert_eq!(
        reopened.frontier().expect("healthy reopened frontier"),
        incremental_frontier
    );
    let reopened_now = reopened.edge(EId(10)).expect("reads").expect("live");
    assert!(reopened_now.props.is_empty());
    assert_eq!(
        reopened_now, incremental_now,
        "frontier edge row diverged after clear"
    );
    let reopened_at_set = reopened
        .edge_at(EId(10), CommitSeq(2))
        .expect("reads set seq")
        .expect("visible at set");
    assert_eq!(reopened_at_set.props, vec![(key, CanonicalScalar::Int(4))]);
    assert_eq!(
        reopened_at_set, incremental_at_set,
        "pre-clear snapshot diverged after restatement"
    );
    assert_eq!(
        reopened.element_versions().expect("healthy"),
        &incremental_versions,
        "checkpoint-derived version heads drifted from the incremental fold \
         after edge restatement — the graph answers hide this (fgdb-l96k)"
    );
}
