//! **The laws the spine owes** (`fgdb-j0vu`).
//!
//! The headline law is `open_write_read_drop_reopen_returns_the_same_graph`, and
//! its whole value is in what does NOT cross the drop. The `Database` is moved
//! into a scope and the scope ends; the only things that survive are the
//! directory path and the keys — exactly what a restarted process would hold.
//! Anything else surviving in a variable would be a way for the law to pass while
//! the durable path is broken, which is the failure mode a reopen test exists to
//! exclude.
//!
//! **WHAT WOULD MAKE THESE VACUOUS**, since a reopen suite is easy to write
//! green:
//!
//! - A database that never wrote anything reopens trivially. So the fixture
//!   commits across MULTIPLE batches, and asserts a non-empty answer, so an
//!   implementation returning "no neighbours" for everything fails.
//! - An implementation that answered from the writer's in-memory fold rather than
//!   from disk would pass a reopen test that reused the same process state.
//!   `Database::open` is given nothing but the path, and `rebuild` re-reads every
//!   block through the store, so there is no in-memory channel available.
//! - A read that ignored its relation or its source would still satisfy a
//!   one-vertex fixture. The controls below use a second relation and an isolated
//!   vertex, and both must come back empty.

use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, Database, DatabaseKeys, OpenError, WriteBatch, WriteError};
use fgdb_delta_types::RelationId;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::{Path, PathBuf};

const KNOWS: RelationId = RelationId(1);
const WORKS_WITH: RelationId = RelationId(2);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: [0x3c; 32],
    }
}

/// A scratch directory that does not yet exist, so `create` owns making it.
///
/// The pid is in the name because concurrent panes run this suite against one
/// `/tmp`, and a shared fixture path makes one pane's run fail on another's
/// leftovers. Nothing is ever removed here: cleaning up would mean a test
/// deleting directories, and this repo's rule 1 does not carve out an exception
/// for test code.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-spine-{}-{name}", std::process::id()))
}

fn under_lab<T: Send + 'static>(
    seed: u64,
    test: impl FnOnce(&CommitCx) -> T + Send + 'static,
) -> T {
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(&contexts.commit())
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

/// Commit the fixture history: three vertices, two `KNOWS` edges across two
/// separate batches, and one `WORKS_WITH` edge that must never answer a `KNOWS`
/// query.
fn write_fixture(cx: &CommitCx, dir: &Path) {
    let mut db = Database::create(cx, dir, keys()).expect("creates");

    let mut first = WriteBatch::new(KNOWS);
    first.create_vertex(VId(1), vec![], vec![]);
    first.create_vertex(VId(2), vec![], vec![]);
    first.create_vertex(VId(3), vec![], vec![]);
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, first).expect("the first batch commits");

    // A SECOND batch, so the fold has to survive across capsules rather than
    // being one encode of one in-memory list.
    let mut second = WriteBatch::new(KNOWS);
    second.add_edge(EId(11), VId(1), VId(3), vec![]);
    db.write(cx, second).expect("the second batch commits");

    let mut other = WriteBatch::new(WORKS_WITH);
    other.add_edge(EId(12), VId(1), VId(2), vec![]);
    db.write(cx, other).expect("the third batch commits");
}

/// **THE LIFECYCLE LAW: open, write, read, drop, reopen, read.**
#[test]
fn open_write_read_drop_reopen_returns_the_same_graph() {
    let dir = scratch("lifecycle");
    under_lab(71, move |cx| {
        let (before, frontier, root) = {
            let mut db = Database::create(cx, &dir, keys()).expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, batch).expect("commits");

            let mut second = WriteBatch::new(KNOWS);
            second.create_vertex(VId(3), vec![], vec![]);
            second.add_edge(EId(11), VId(1), VId(3), vec![]);
            db.write(cx, second).expect("commits");

            (
                db.neighbours(VId(1), KNOWS).expect("reads"),
                db.frontier(),
                db.partition_root(),
            )
            // `db` is dropped here: the coordinator, the store, the writer's
            // fold and the decoded blocks all go out of scope together.
        };

        assert_eq!(
            before,
            vec![VId(2), VId(3)],
            "the fixture must be non-trivial, or agreement after reopening is cheap"
        );

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).expect("reopens");
        assert_eq!(
            db.neighbours(VId(1), KNOWS).expect("reads"),
            before,
            "the reopened database must answer what the original did"
        );
        assert_eq!(db.frontier(), frontier, "and at the same frontier");
        assert_eq!(
            db.partition_root(),
            root,
            "and the rebuild is deterministic, so it republishes the same root"
        );
    });
}

/// The read is keyed on BOTH source and relation. Without these controls a read
/// that ignored either would still pass the law above.
#[test]
fn reads_are_keyed_on_source_and_relation() {
    let dir = scratch("keying");
    under_lab(72, move |cx| {
        write_fixture(cx, &dir);
        let db = Database::open(cx, &dir, keys()).expect("reopens");

        assert_eq!(
            db.neighbours(VId(1), KNOWS).expect("reads"),
            vec![VId(2), VId(3)]
        );
        assert_eq!(
            db.neighbours(VId(1), WORKS_WITH).expect("reads"),
            vec![VId(2)],
            "the other relation has its own single edge"
        );
        assert!(
            db.neighbours(VId(2), KNOWS).expect("reads").is_empty(),
            "vertex 2 is a destination, never a source"
        );
        assert!(
            db.neighbours(VId(99), KNOWS).expect("reads").is_empty(),
            "a vertex that was never created has no neighbours"
        );
    });
}

/// Reopening twice more must not drift: the rebuild is a function of the stream.
///
/// **THE EMPTY-PARTITION CONTROL IS NOT DECORATION.** Equality alone is satisfied
/// by an implementation that folds nothing at all, since a fold that always
/// produces the empty partition is perfectly deterministic — measured, not
/// supposed: a mutant that `continue`d past every template left this law GREEN
/// while both content laws went red. So this also pins the root away from the
/// root a database with no history publishes.
#[test]
fn repeated_reopens_publish_the_same_root() {
    let dir = scratch("determinism");
    let empty_dir = scratch("determinism-empty");
    under_lab(73, move |cx| {
        write_fixture(cx, &dir);

        let first = {
            let db = Database::open(cx, &dir, keys()).expect("reopens");
            db.partition_root()
        };
        let second = {
            let db = Database::open(cx, &dir, keys()).expect("reopens");
            db.partition_root()
        };
        assert_eq!(
            first, second,
            "the root is content-addressed and the fold is deterministic, so replaying \
             one stream twice must publish one identity"
        );

        let empty = {
            let db = Database::create(cx, &empty_dir, keys()).expect("creates");
            db.partition_root()
        };
        assert_ne!(
            first, empty,
            "a fold that dropped every row would publish the empty partition's root and \
             still satisfy the equality above"
        );
    });
}

/// **FAIL CLOSED.** A directory that is not a database must be refused by name,
/// never silently converted into an empty one.
#[test]
fn opening_a_directory_that_is_not_a_database_fails_closed() {
    let dir = scratch("foreign");
    under_lab(74, move |cx| {
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(dir.join("notes.txt"), b"not a database").expect("foreign file");

        let refusal = Database::open(cx, &dir, keys());
        assert!(
            matches!(&refusal, Err(OpenError::NotADatabase { missing, .. }) if *missing == "capsules"),
            "a foreign directory must be refused, and the refusal must name what is \
             missing: {refusal:?}"
        );

        // And the refusal must not have CREATED one on the way past.
        assert!(
            !dir.join("capsules").exists(),
            "a refused open must leave the directory as it found it"
        );
    });
}

#[test]
fn opening_a_path_that_does_not_exist_fails_closed() {
    let dir = scratch("absent");
    under_lab(75, move |cx| {
        let refusal = Database::open(cx, &dir, keys());
        assert!(
            matches!(&refusal, Err(OpenError::NotADatabase { .. })),
            "an absent path must be refused: {refusal:?}"
        );
        assert!(
            !dir.exists(),
            "a refused open must not create the directory"
        );
    });
}

#[test]
fn creating_over_an_existing_database_is_refused() {
    let dir = scratch("recreate");
    under_lab(76, move |cx| {
        write_fixture(cx, &dir);
        let refusal = Database::create(cx, &dir, keys());
        assert!(
            matches!(&refusal, Err(OpenError::AlreadyADatabase { .. })),
            "create must refuse an existing database: {refusal:?}"
        );
    });
}

#[test]
fn creating_in_a_non_empty_foreign_directory_is_refused() {
    let dir = scratch("occupied");
    under_lab(77, move |cx| {
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(dir.join("someone_elses.txt"), b"hello").expect("foreign file");
        let refusal = Database::create(cx, &dir, keys());
        assert!(
            matches!(&refusal, Err(OpenError::NotEmpty { .. })),
            "create must refuse a non-empty foreign directory: {refusal:?}"
        );
    });
}

/// An empty batch consumes a sequence and publishes a marker for nothing.
#[test]
fn an_empty_batch_is_refused() {
    let dir = scratch("empty");
    under_lab(78, move |cx| {
        let mut db = Database::create(cx, &dir, keys()).expect("creates");
        let refusal = db.write(cx, WriteBatch::new(KNOWS));
        assert!(
            matches!(&refusal, Err(WriteError::EmptyBatch)),
            "an empty batch must be refused: {refusal:?}"
        );
        assert_eq!(
            db.frontier(),
            fgdb_types::CommitSeq(0),
            "and it must not have consumed a sequence"
        );
    });
}

/// **THE CRASH-POINT MATRIX: all or nothing, at every instant of the protocol.**
///
/// A crash between a write and the reopen must leave the WHOLE batch or NONE of
/// it. The second batch therefore carries TWO edges, which is what makes
/// partiality detectable at all: with one edge, "all" and "nothing" are the only
/// reachable answers and the law would hold for free. With two, a protocol that
/// made half a batch durable answers `[2, 3]` or `[2, 4]`, and both are refused
/// below.
///
/// Every crash point Chronicle defines is exercised, not a sample — the whole
/// value of a crash matrix is that no instant is left unasked.
///
/// **ONE INSTANT DOES NOT EXIST ON THIS PATH, AND THE LAW SAYS SO EXACTLY
/// RATHER THAN TOLERATING IT.** `AfterCapsuleDirectorySyncBeforeParentDirectorySync`
/// is guarded by `capsule_directory_parent_sync_pending`
/// (`fgdb-chronicle/src/commit.rs:615`), which is set only when the capsule
/// directory was newly created — the FIRST commit into a fresh database. The
/// second batch is not that, so injecting there is a no-op and the write
/// completes. Discovered by this law failing, then read in the source rather
/// than assumed.
///
/// The fired set is therefore PINNED as an exact set, not waved through with an
/// `if err`. A blanket "either outcome is fine" would keep passing if crash
/// injection silently became a no-op everywhere, which is the one regression a
/// crash matrix exists to catch. `a_crash_on_the_first_commit_covers_the_parent_directory_instant`
/// below covers the remaining instant where it does exist, so the matrix is total.
#[test]
fn a_crash_at_any_protocol_instant_leaves_the_whole_batch_or_none() {
    let points = [
        CrashPoint::BeforeCapsule,
        CrashPoint::AfterCapsuleBeforeD1,
        CrashPoint::AfterCapsuleFileSyncBeforeDirectorySync,
        CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync,
        CrashPoint::AfterD1,
        CrashPoint::AfterMarkerBeforeD2,
        CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
    ];
    let mut fired = Vec::new();
    for (index, point) in points.into_iter().enumerate() {
        let dir = scratch(&format!("crash-{index}"));
        fired.push((point, under_lab(80 + index as u64, move |cx| {
            let crashed;
            // A durable first batch, so the law is about the SECOND one and a
            // reopen that lost everything cannot pass by accident.
            {
                let mut db = Database::create(cx, &dir, keys()).expect("creates");
                let mut first = WriteBatch::new(KNOWS);
                first.create_vertex(VId(1), vec![], vec![]);
                first.create_vertex(VId(2), vec![], vec![]);
                first.add_edge(EId(10), VId(1), VId(2), vec![]);
                db.write(cx, first).expect("the first batch commits");
                assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(2)]);

                let mut second = WriteBatch::new(KNOWS);
                second.create_vertex(VId(3), vec![], vec![]);
                second.create_vertex(VId(4), vec![], vec![]);
                second.add_edge(EId(11), VId(1), VId(3), vec![]);
                second.add_edge(EId(12), VId(1), VId(4), vec![]);
                crashed = db.write_with_crash(cx, second, Some(point)).is_err();
                // The process dies here. Nothing republishes, nothing cleans up.
            }

            let reopened = Database::open(cx, &dir, keys());
            assert!(
                reopened.is_ok(),
                "{point:?}: a crashed database must still reopen: {reopened:?}"
            );
            let db = reopened.expect("reopens");
            let after = db.neighbours(VId(1), KNOWS).expect("reads");
            assert!(
                after == vec![VId(2)] || after == vec![VId(2), VId(3), VId(4)],
                "{point:?}: the second batch must be wholly absent or wholly present, got {after:?}"
            );
            // The first batch is durable at every instant of the second's
            // protocol: a crash may lose the batch in flight and must never
            // damage what was already acknowledged.
            assert!(
                after.contains(&VId(2)),
                "{point:?}: the previously acknowledged batch must survive, got {after:?}"
            );
            // A write that reported success must be wholly present: "all or
            // nothing" allows nothing only when the write did not complete.
            if !crashed {
                assert_eq!(
                    after,
                    vec![VId(2), VId(3), VId(4)],
                    "{point:?}: this instant did not fire, so the completed write must be whole"
                );
            }
            crashed
        })));
    }

    let actually_fired: Vec<CrashPoint> = fired
        .iter()
        .filter(|(_, crashed)| *crashed)
        .map(|(point, _)| *point)
        .collect();
    assert_eq!(
        actually_fired,
        vec![
            CrashPoint::BeforeCapsule,
            CrashPoint::AfterCapsuleBeforeD1,
            CrashPoint::AfterCapsuleFileSyncBeforeDirectorySync,
            CrashPoint::AfterD1,
            CrashPoint::AfterMarkerBeforeD2,
            CrashPoint::AfterMarkerFileSyncBeforeDirectorySync,
        ],
        "exactly six of the seven instants exist on a NON-first commit; \
         AfterCapsuleDirectorySyncBeforeParentDirectorySync is guarded by \
         capsule_directory_parent_sync_pending. If this set shrinks, crash \
         injection has silently become a no-op and every law above went vacuous \
         with it"
    );
}

/// The instant the matrix above cannot reach: the capsule directory is created
/// by the FIRST commit, so only that commit can crash between making it durable
/// and making its entry in the database directory durable.
///
/// Nothing was written before it, so the all-or-nothing law here is "nothing" —
/// and the database must still open afterwards rather than being left in a state
/// only a repair tool could read.
#[test]
fn a_crash_on_the_first_commit_covers_the_parent_directory_instant() {
    let dir = scratch("crash-first-commit");
    under_lab(90, move |cx| {
        {
            let mut db = Database::create(cx, &dir, keys()).expect("creates");
            let mut only = WriteBatch::new(KNOWS);
            only.create_vertex(VId(1), vec![], vec![]);
            only.create_vertex(VId(2), vec![], vec![]);
            only.add_edge(EId(10), VId(1), VId(2), vec![]);
            let crashed = db.write_with_crash(
                cx,
                only,
                Some(CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync),
            );
            assert!(
                crashed.is_err(),
                "on the FIRST commit this instant exists and must fire: {crashed:?}"
            );
        }

        let reopened = Database::open(cx, &dir, keys());
        assert!(
            reopened.is_ok(),
            "a database crashed on its first commit must still reopen: {reopened:?}"
        );
        let db = reopened.expect("reopens");
        assert!(
            db.neighbours(VId(1), KNOWS).expect("reads").is_empty(),
            "the crash was before D2, so no commit is in the stream and the graph is empty"
        );
    });
}

/// A database created and never written reopens as an empty graph rather than
/// failing — the boundary between "not a database" and "a database with nothing
/// in it".
#[test]
fn a_created_but_unwritten_database_reopens_empty() {
    let dir = scratch("fresh");
    under_lab(79, move |cx| {
        {
            Database::create(cx, &dir, keys()).expect("creates");
        }
        let db = Database::open(cx, &dir, keys()).expect("reopens");
        assert!(db.neighbours(VId(1), KNOWS).expect("reads").is_empty());
        assert_eq!(db.frontier(), fgdb_types::CommitSeq(0));
    });
}
