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
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, CommitSeq, EId, VId};
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

/// Commit the fixture history: three vertices, two `KNOWS` edges across two
/// separate batches, and one `WORKS_WITH` edge that must never answer a `KNOWS`
/// query.
async fn write_fixture(cx: &CommitCx, dir: &Path) {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");

    let mut first = WriteBatch::new(KNOWS);
    first.create_vertex(VId(1), vec![], vec![]);
    first.create_vertex(VId(2), vec![], vec![]);
    first.create_vertex(VId(3), vec![], vec![]);
    first.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, first).await.expect("the first batch commits");

    // A SECOND batch, so the fold has to survive across capsules rather than
    // being one encode of one in-memory list.
    let mut second = WriteBatch::new(KNOWS);
    second.add_edge(EId(11), VId(1), VId(3), vec![]);
    db.write(cx, second)
        .await
        .expect("the second batch commits");

    let mut other = WriteBatch::new(WORKS_WITH);
    other.add_edge(EId(12), VId(1), VId(2), vec![]);
    db.write(cx, other).await.expect("the third batch commits");
}

/// **THE LIFECYCLE LAW: open, write, read, drop, reopen, read.**
#[test]
fn open_write_read_drop_reopen_returns_the_same_graph() {
    let dir = scratch("lifecycle");
    under_lab(71, move |cx| async move {
        let cx = &cx;
        let (before, frontier, root) = {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, batch).await.expect("commits");

            let mut second = WriteBatch::new(KNOWS);
            second.create_vertex(VId(3), vec![], vec![]);
            second.add_edge(EId(11), VId(1), VId(3), vec![]);
            db.write(cx, second).await.expect("commits");

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
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
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

/// **THE VERTEX LAW (fgdb-3xoi): written labels and properties are readable,
/// and a reopen answers the same.** Until this landed, tier D was adjacency
/// only and a committed vertex's labels/properties were durable, oracle-
/// materialized, and unreadable — so this is the law that makes
/// [`Database::vertex`] real rather than decorative.
#[test]
fn written_labels_and_properties_are_readable_and_survive_a_reopen() {
    let dir = scratch("vertex-props");
    under_lab(76, move |cx| async move {
        let cx = &cx;
        let name_key = PropertyKeyId(7);
        let born_key = PropertyKeyId(9);
        let before = {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(
                VId(1),
                vec![LabelId(3), LabelId(5)],
                vec![
                    (
                        name_key,
                        CanonicalScalar::ucs_basic_text("ada").expect("admissible"),
                    ),
                    (born_key, CanonicalScalar::Int(1815)),
                ],
            );
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, batch).await.expect("commits");

            let row = db.vertex(VId(1)).expect("the written vertex is readable");
            assert_eq!(row.labels, vec![LabelId(3), LabelId(5)]);
            assert_eq!(
                row.props,
                vec![
                    (
                        name_key,
                        CanonicalScalar::ucs_basic_text("ada").expect("admissible")
                    ),
                    (born_key, CanonicalScalar::Int(1815)),
                ]
            );
            let bare = db.vertex(VId(2)).expect("the bare vertex is readable too");
            assert!(bare.labels.is_empty() && bare.props.is_empty());
            assert!(
                db.vertex(VId(99)).is_none(),
                "a vertex that was never created has no row"
            );
            row
        };

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("still readable"),
            before,
            "the reopened database must answer the same labels and properties"
        );
        assert!(db.vertex(VId(99)).is_none());
    });
}

/// **THE DELETE LAW (fgdb-p3ok): deletes take effect, cascade, and survive a
/// reopen — with every before-image derived by the engine.**
///
/// The image derivation itself is proven against the oracle by the
/// differential (`fgdb-sim/tests/spine_differential.rs`), whose replay path
/// REFUSES a wrong `before_version` or cascade; this law pins the engine-side
/// lifecycle: reads flip at the delete, nothing resurrects across a reopen,
/// and deleting the absent is a typed refusal with no durable trace.
#[test]
fn deletes_take_effect_cascade_and_survive_a_reopen() {
    let dir = scratch("deletes");
    under_lab(77, move |cx| async move {
        let cx = &cx;
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.create_vertex(VId(3), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            batch.add_edge(EId(11), VId(1), VId(3), vec![]);
            batch.add_edge(EId(12), VId(3), VId(1), vec![]);
            db.write(cx, batch).await.expect("commits");

            // Delete one edge: only that edge goes.
            let mut drop_edge = WriteBatch::new(KNOWS);
            drop_edge.delete_edge(EId(10));
            db.write(cx, drop_edge).await.expect("edge delete commits");
            assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(3)]);

            // Delete a vertex: the engine derives the cascade — BOTH
            // directions — and the vertex row goes with its edges.
            let mut drop_vertex = WriteBatch::new(KNOWS);
            drop_vertex.delete_vertex(VId(1));
            db.write(cx, drop_vertex)
                .await
                .expect("vertex delete commits");
            assert!(db.vertex(VId(1)).is_none(), "the vertex row is retired");
            assert!(db.neighbours(VId(1), KNOWS).expect("reads").is_empty());
            assert!(
                db.neighbours(VId(3), KNOWS).expect("reads").is_empty(),
                "the inbound edge 12 was cascade-retired too"
            );

            // Deleting the absent is a typed refusal, before anything durable.
            let frontier = db.frontier();
            let mut again = WriteBatch::new(KNOWS);
            again.delete_vertex(VId(1));
            assert!(matches!(
                db.write(cx, again).await,
                Err(WriteError::UnknownVertex { vid: VId(1) })
            ));
            let mut ghost = WriteBatch::new(KNOWS);
            ghost.delete_edge(EId(10));
            assert!(matches!(
                db.write(cx, ghost).await,
                Err(WriteError::UnknownEdge { eid: EId(10) })
            ));
            assert_eq!(db.frontier(), frontier, "refusals consumed no sequence");
        }

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert!(db.vertex(VId(1)).is_none(), "no resurrection across reopen");
        assert!(db.neighbours(VId(3), KNOWS).expect("reads").is_empty());
        assert_eq!(
            db.vertex(VId(2)).expect("undeleted vertex survives").labels,
            vec![]
        );
    });
}

/// A create and its delete in ONE batch: the engine images the version the
/// batch prefix just minted, and the durable fold leaves no trace of the
/// element — the same-commit fold, end to end through the public surface.
#[test]
fn a_same_batch_create_and_delete_leaves_no_element() {
    let dir = scratch("same-batch-delete");
    under_lab(78, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.delete_edge(EId(10));
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.delete_vertex(VId(3));
        db.write(cx, batch).await.expect("commits");

        assert!(db.neighbours(VId(1), KNOWS).expect("reads").is_empty());
        assert!(db.vertex(VId(3)).is_none());
        assert!(
            db.vertex(VId(1)).is_some() && db.vertex(VId(2)).is_some(),
            "the surviving vertices are untouched"
        );
        drop(db);
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert!(db.vertex(VId(3)).is_none(), "and the fold is stable");
    });
}

/// **THE UPDATE LAW (fgdb-stb6): labels and properties CHANGE, the change is
/// readable, and a reopen answers the updated state.** The before-images the
/// durable rows carry are engine-derived and oracle-validated by the
/// differential; this law pins the engine-side lifecycle.
#[test]
fn label_and_property_updates_are_readable_and_survive_a_reopen() {
    let dir = scratch("updates");
    under_lab(79, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(
                VId(1),
                vec![LabelId(3)],
                vec![(key, CanonicalScalar::Int(1))],
            );
            db.write(cx, batch).await.expect("commits");

            // Change a property, add a label, in separate commits.
            let mut update = WriteBatch::new(KNOWS);
            update.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(2)));
            db.write(cx, update).await.expect("property update commits");
            let mut label = WriteBatch::new(KNOWS);
            label.set_vertex_label(VId(1), LabelId(5), true);
            db.write(cx, label).await.expect("label update commits");

            let row = db.vertex(VId(1)).expect("readable");
            assert_eq!(row.props, vec![(key, CanonicalScalar::Int(2))]);
            assert_eq!(row.labels, vec![LabelId(3), LabelId(5)]);

            // Unset the property and remove the original label.
            let mut clear = WriteBatch::new(KNOWS);
            clear.set_vertex_property(VId(1), key, None);
            clear.set_vertex_label(VId(1), LabelId(3), false);
            db.write(cx, clear).await.expect("clears commit");
            let row = db.vertex(VId(1)).expect("still readable");
            assert!(row.props.is_empty());
            assert_eq!(row.labels, vec![LabelId(5)]);

            // Updating the absent is a typed refusal, before anything durable.
            let frontier = db.frontier();
            let mut ghost = WriteBatch::new(KNOWS);
            ghost.set_vertex_property(VId(99), key, Some(CanonicalScalar::Int(3)));
            assert!(matches!(
                db.write(cx, ghost).await,
                Err(WriteError::UnknownVertex { vid: VId(99) })
            ));
            assert_eq!(db.frontier(), frontier, "the refusal consumed no sequence");
        }

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        let row = db.vertex(VId(1)).expect("the updated vertex survives");
        assert!(row.props.is_empty());
        assert_eq!(row.labels, vec![LabelId(5)]);
    });
}

/// A create and its updates in ONE batch: the engine derives each row's
/// before-image against the batch prefix, and the durable fold shows only the
/// final content — the intermediate never existed on any snapshot.
#[test]
fn same_batch_create_and_update_shows_only_the_final_content() {
    let dir = scratch("same-batch-update");
    under_lab(80, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![(key, CanonicalScalar::Int(1))]);
        batch.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(2)));
        batch.set_vertex_label(VId(1), LabelId(9), true);
        db.write(cx, batch).await.expect("commits");

        let row = db.vertex(VId(1)).expect("readable");
        assert_eq!(row.props, vec![(key, CanonicalScalar::Int(2))]);
        assert_eq!(row.labels, vec![LabelId(9)]);
        drop(db);
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("survives").props,
            vec![(key, CanonicalScalar::Int(2))]
        );
    });
}

/// **THE EDGE-LOOKUP LAW: an edge answers by identity — endpoints, relation,
/// lifetime, and properties — flips at its deletion, and survives a reopen.**
///
/// The parallel twin doubles as the properties control: it is propertyless, so
/// a read that answered properties by position instead of by the locator
/// column would hand the twin its sibling's row.
#[test]
fn edge_lookups_answer_by_identity_and_survive_a_reopen() {
    let dir = scratch("edge-lookup");
    under_lab(81, move |cx| async move {
        let cx = &cx;
        let since = PropertyKeyId(7);
        let props = vec![(since, CanonicalScalar::Int(2019))];
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), props.clone());
            batch.add_edge(EId(11), VId(1), VId(2), vec![]); // parallel twin
            batch.add_edge(
                EId(12),
                VId(2),
                VId(1),
                vec![(since, CanonicalScalar::Int(2024))],
            );
            db.write(cx, batch).await.expect("commits");

            let edge = db.edge(EId(10)).expect("reads").expect("exists");
            assert_eq!(
                (edge.entry.src, edge.entry.relation, edge.entry.dst),
                (VId(1), KNOWS, VId(2))
            );
            assert!(edge.entry.retired_at.is_none());
            assert_eq!(edge.props, props, "the durable patch answers the props");
            assert!(
                db.edge(EId(99)).expect("reads").is_none(),
                "an edge that was never created has no version"
            );

            let mut drop_one = WriteBatch::new(KNOWS);
            drop_one.delete_edge(EId(10));
            db.write(cx, drop_one).await.expect("commits");
            assert!(
                db.edge(EId(10)).expect("reads").is_none(),
                "the deleted parallel edge is gone by identity"
            );
            assert!(
                db.edge(EId(11)).expect("reads").is_some(),
                "its twin survives — the lookup is keyed on EId, not endpoints"
            );
        }

        // NOTHING crosses this line except `dir`, `keys()`, and the expected
        // property row.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert!(db.edge(EId(10)).expect("reads").is_none());
        let twin = db.edge(EId(11)).expect("reads").expect("survives");
        assert_eq!((twin.entry.src, twin.entry.dst), (VId(1), VId(2)));
        assert_eq!(
            twin.props,
            vec![],
            "the propertyless twin owns no row, even beside a propertied sibling"
        );
        let survivor = db.edge(EId(12)).expect("reads").expect("survives");
        assert_eq!(
            survivor.props,
            vec![(since, CanonicalScalar::Int(2024))],
            "edge properties survive a from-scratch reopen"
        );
    });
}

/// The read is keyed on BOTH source and relation. Without these controls a read
/// that ignored either would still pass the law above.
#[test]
fn reads_are_keyed_on_source_and_relation() {
    let dir = scratch("keying");
    under_lab(72, move |cx| async move {
        let cx = &cx;
        write_fixture(cx, &dir).await;
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");

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
    under_lab(73, move |cx| async move {
        let cx = &cx;
        write_fixture(cx, &dir).await;

        let first = {
            let db = Database::open(cx, &dir, keys()).await.expect("reopens");
            db.partition_root()
        };
        let second = {
            let db = Database::open(cx, &dir, keys()).await.expect("reopens");
            db.partition_root()
        };
        assert_eq!(
            first, second,
            "the root is content-addressed and the fold is deterministic, so replaying \
             one stream twice must publish one identity"
        );

        let empty = {
            let db = Database::create(cx, &empty_dir, keys())
                .await
                .expect("creates");
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
    under_lab(74, move |cx| async move {
        let cx = &cx;
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(dir.join("notes.txt"), b"not a database").expect("foreign file");

        let refusal = Database::open(cx, &dir, keys()).await;
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
    under_lab(75, move |cx| async move {
        let cx = &cx;
        let refusal = Database::open(cx, &dir, keys()).await;
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
    under_lab(76, move |cx| async move {
        let cx = &cx;
        write_fixture(cx, &dir).await;
        let refusal = Database::create(cx, &dir, keys()).await;
        assert!(
            matches!(&refusal, Err(OpenError::AlreadyADatabase { .. })),
            "create must refuse an existing database: {refusal:?}"
        );
    });
}

#[test]
fn creating_in_a_non_empty_foreign_directory_is_refused() {
    let dir = scratch("occupied");
    under_lab(77, move |cx| async move {
        let cx = &cx;
        std::fs::create_dir_all(&dir).expect("scratch dir");
        std::fs::write(dir.join("someone_elses.txt"), b"hello").expect("foreign file");
        let refusal = Database::create(cx, &dir, keys()).await;
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
    under_lab(78, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let refusal = db.write(cx, WriteBatch::new(KNOWS)).await;
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
        fired.push((point, under_lab(80 + index as u64, move |cx| async move {
            let cx = &cx;
            let crashed;
            // A durable first batch, so the law is about the SECOND one and a
            // reopen that lost everything cannot pass by accident.
            {
                let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
                let mut first = WriteBatch::new(KNOWS);
                first.create_vertex(VId(1), vec![], vec![]);
                first.create_vertex(VId(2), vec![], vec![]);
                first.add_edge(EId(10), VId(1), VId(2), vec![]);
                db.write(cx, first).await.expect("the first batch commits");
                assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(2)]);

                let mut second = WriteBatch::new(KNOWS);
                second.create_vertex(VId(3), vec![], vec![]);
                second.create_vertex(VId(4), vec![], vec![]);
                second.add_edge(EId(11), VId(1), VId(3), vec![]);
                second.add_edge(EId(12), VId(1), VId(4), vec![]);
                crashed = db.write_with_crash(cx, second, Some(point)).await.is_err();
                // The process dies here. Nothing republishes, nothing cleans up.
            }

            let reopened = Database::open(cx, &dir, keys()).await;
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
    under_lab(90, move |cx| async move {
        let cx = &cx;
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut only = WriteBatch::new(KNOWS);
            only.create_vertex(VId(1), vec![], vec![]);
            only.create_vertex(VId(2), vec![], vec![]);
            only.add_edge(EId(10), VId(1), VId(2), vec![]);
            let crashed = db
                .write_with_crash(
                    cx,
                    only,
                    Some(CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync),
                )
                .await;
            assert!(
                crashed.is_err(),
                "on the FIRST commit this instant exists and must fire: {crashed:?}"
            );
        }

        let reopened = Database::open(cx, &dir, keys()).await;
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
    under_lab(79, move |cx| async move {
        let cx = &cx;
        {
            Database::create(cx, &dir, keys()).await.expect("creates");
        }
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert!(db.neighbours(VId(1), KNOWS).expect("reads").is_empty());
        assert_eq!(db.frontier(), fgdb_types::CommitSeq(0));
    });
}

/// **THE TIME-TRAVEL LAW (fgdb-90jx): every read family answers AS OF any
/// committed sequence, each answer flips at exactly its boundary commit, the
/// future is a typed refusal, and history survives a reopen.**
#[test]
fn reads_answer_as_of_any_committed_sequence() {
    let dir = scratch("time-travel");
    under_lab(93, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        let props = vec![(key, CanonicalScalar::Int(1815))];

        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), props.clone());
        let s1 = db.write(cx, first).await.expect("commits");
        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(11), VId(1), VId(3), vec![]);
        let s2 = db.write(cx, second).await.expect("commits");
        let mut third = WriteBatch::new(KNOWS);
        third.delete_edge(EId(10));
        let s3 = db.write(cx, third).await.expect("commits");
        let mut fourth = WriteBatch::new(KNOWS);
        fourth.set_vertex_label(VId(3), LabelId(9), true);
        let s4 = db.write(cx, fourth).await.expect("commits");
        let mut fifth = WriteBatch::new(KNOWS);
        fifth.delete_vertex(VId(3)); // cascades EId(11)
        let s5 = db.write(cx, fifth).await.expect("commits");
        drop(db);

        // NOTHING crosses this line except `dir`, `keys()`, the recorded
        // sequences, and the expected values — history is read back durable.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(db.frontier(), s5);

        // Neighbours flip at every boundary.
        let hood = |as_of| db.neighbours_at(VId(1), KNOWS, as_of).expect("reads");
        assert_eq!(hood(s1), vec![VId(2)]);
        assert_eq!(hood(s2), vec![VId(2), VId(3)]);
        assert_eq!(hood(s3), vec![VId(3)], "e10's deletion is visible at s3");
        assert_eq!(hood(s4), vec![VId(3)]);
        assert_eq!(hood(s5), vec![], "the cascade retired e11 at s5");

        // The deleted edge answers its whole life — props included — and
        // nothing after.
        for alive in [s1, s2] {
            let record = db.edge_at(EId(10), alive).expect("reads").expect("alive");
            assert_eq!(record.props, props, "history answers the durable row");
        }
        for dead in [s3, s4, s5] {
            assert!(db.edge_at(EId(10), dead).expect("reads").is_none());
        }

        // The vertex chain: absent, bare, labeled, gone.
        assert!(db.vertex_at(VId(3), s1).expect("reads").is_none());
        for bare in [s2, s3] {
            let row = db.vertex_at(VId(3), bare).expect("reads").expect("exists");
            assert_eq!(row.labels, vec![], "the label lands at s4, not before");
        }
        let labeled = db.vertex_at(VId(3), s4).expect("reads").expect("exists");
        assert_eq!(labeled.labels, vec![LabelId(9)]);
        assert!(db.vertex_at(VId(3), s5).expect("reads").is_none());

        // The future is a refusal, not a clamp — for every family.
        let future = CommitSeq(s5.0 + 1);
        assert!(matches!(
            db.neighbours_at(VId(1), KNOWS, future),
            Err(fgdb::ReadError::BeyondFrontier { asked, frontier }) if asked == future && frontier == s5
        ));
        assert!(matches!(
            db.edge_at(EId(10), future),
            Err(fgdb::ReadError::BeyondFrontier { .. })
        ));
        assert!(matches!(
            db.vertex_at(VId(3), future),
            Err(fgdb::ReadError::BeyondFrontier { .. })
        ));
    });
}

/// **THE ENUMERATION LAW (fgdb-9k5w): the whole graph scans at any committed
/// sequence, in canonical ascending-identity order, agreeing element-for-
/// element with the point lookups — and the future is the same refusal.**
///
/// Enumeration is what a query layer starts from, so the sharp edge is
/// TOTALITY: nothing visible may be missing, nothing retired may appear, and
/// nothing may disagree with the point lookup that serves the same identity.
#[test]
fn the_graph_enumerates_at_every_committed_sequence() {
    let dir = scratch("enumeration");
    under_lab(97, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        let props = vec![(key, CanonicalScalar::Int(1815))];

        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), props.clone());
        let s1 = db.write(cx, first).await.expect("commits");
        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(3), vec![], vec![]);
        second.add_edge(EId(11), VId(3), VId(1), vec![]);
        let s2 = db.write(cx, second).await.expect("commits");
        let mut third = WriteBatch::new(KNOWS);
        third.delete_edge(EId(10));
        third.delete_vertex(VId(3)); // cascades EId(11)
        let s3 = db.write(cx, third).await.expect("commits");
        drop(db);

        // NOTHING crosses this line except `dir`, `keys()`, the recorded
        // sequences, and the expected values.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");

        // Totality and order at each epoch.
        let vids = |as_of| -> Vec<VId> {
            db.vertices_at(as_of)
                .expect("reads")
                .iter()
                .map(|row| row.vid)
                .collect()
        };
        let eids = |as_of| -> Vec<EId> {
            db.edges_at(as_of)
                .expect("reads")
                .iter()
                .map(|record| record.entry.eid)
                .collect()
        };
        assert_eq!(vids(s1), vec![VId(1), VId(2)]);
        assert_eq!(eids(s1), vec![EId(10)]);
        assert_eq!(vids(s2), vec![VId(1), VId(2), VId(3)]);
        assert_eq!(eids(s2), vec![EId(10), EId(11)]);
        assert_eq!(vids(s3), vec![VId(1), VId(2)], "the delete drops out");
        assert_eq!(
            eids(s3),
            vec![],
            "both the deletion and its cascade drop out"
        );

        // The frontier faces answer the last epoch.
        assert_eq!(
            db.vertices().iter().map(|row| row.vid).collect::<Vec<_>>(),
            vids(s3)
        );
        assert_eq!(db.edges().expect("reads").len(), 0);

        // Element-for-element agreement with the point lookups, every epoch.
        for as_of in [s1, s2, s3] {
            for row in db.vertices_at(as_of).expect("reads") {
                assert_eq!(
                    db.vertex_at(row.vid, as_of).expect("reads"),
                    Some(row),
                    "enumeration and point lookup disagree at {as_of:?}"
                );
            }
            for record in db.edges_at(as_of).expect("reads") {
                assert_eq!(
                    db.edge_at(record.entry.eid, as_of).expect("reads"),
                    Some(record),
                    "enumeration and point lookup disagree at {as_of:?}"
                );
            }
        }
        // The scanned property row is the durable one.
        let scanned = db.edges_at(s2).expect("reads");
        assert_eq!(scanned[0].props, props);

        // The future refuses for both scan families.
        let future = CommitSeq(s3.0 + 1);
        assert!(matches!(
            db.vertices_at(future),
            Err(fgdb::ReadError::BeyondFrontier { .. })
        ));
        assert!(matches!(
            db.edges_at(future),
            Err(fgdb::ReadError::BeyondFrontier { .. })
        ));
    });
}
