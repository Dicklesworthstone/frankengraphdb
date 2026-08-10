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
                db.frontier().expect("healthy frontier"),
                db.partition_root().expect("healthy root"),
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
        assert_eq!(
            db.frontier().expect("healthy reopened frontier"),
            frontier,
            "and at the same frontier"
        );
        assert_eq!(
            db.partition_root().expect("healthy reopened root"),
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

            let row = db
                .vertex(VId(1))
                .expect("reads")
                .expect("the written vertex is readable");
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
            let bare = db
                .vertex(VId(2))
                .expect("reads")
                .expect("the bare vertex is readable too");
            assert!(bare.labels.is_empty() && bare.props.is_empty());
            assert!(
                db.vertex(VId(99)).expect("reads").is_none(),
                "a vertex that was never created has no row"
            );
            row
        };

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("still readable"),
            before,
            "the reopened database must answer the same labels and properties"
        );
        assert!(db.vertex(VId(99)).expect("reads").is_none());
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
            assert!(
                db.vertex(VId(1)).expect("reads").is_none(),
                "the vertex row is retired"
            );
            assert!(db.neighbours(VId(1), KNOWS).expect("reads").is_empty());
            assert!(
                db.neighbours(VId(3), KNOWS).expect("reads").is_empty(),
                "the inbound edge 12 was cascade-retired too"
            );

            // Deleting the absent is a typed refusal, before anything durable.
            let frontier = db.frontier().expect("healthy frontier");
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
            assert_eq!(
                db.frontier().expect("healthy frontier after refusal"),
                frontier,
                "refusals consumed no sequence"
            );
        }

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert!(
            db.vertex(VId(1)).expect("reads").is_none(),
            "no resurrection across reopen"
        );
        assert!(db.neighbours(VId(3), KNOWS).expect("reads").is_empty());
        assert_eq!(
            db.vertex(VId(2))
                .expect("reads")
                .expect("undeleted vertex survives")
                .labels,
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
        assert!(db.vertex(VId(3)).expect("reads").is_none());
        assert!(
            db.vertex(VId(1)).expect("reads").is_some()
                && db.vertex(VId(2)).expect("reads").is_some(),
            "the surviving vertices are untouched"
        );
        drop(db);
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert!(
            db.vertex(VId(3)).expect("reads").is_none(),
            "and the fold is stable"
        );
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

            let row = db.vertex(VId(1)).expect("reads").expect("readable");
            assert_eq!(row.props, vec![(key, CanonicalScalar::Int(2))]);
            assert_eq!(row.labels, vec![LabelId(3), LabelId(5)]);

            // Unset the property and remove the original label.
            let mut clear = WriteBatch::new(KNOWS);
            clear.set_vertex_property(VId(1), key, None);
            clear.set_vertex_label(VId(1), LabelId(3), false);
            db.write(cx, clear).await.expect("clears commit");
            let row = db.vertex(VId(1)).expect("reads").expect("still readable");
            assert!(row.props.is_empty());
            assert_eq!(row.labels, vec![LabelId(5)]);

            // Updating the absent is a typed refusal, before anything durable.
            let frontier = db.frontier().expect("healthy frontier");
            let mut ghost = WriteBatch::new(KNOWS);
            ghost.set_vertex_property(VId(99), key, Some(CanonicalScalar::Int(3)));
            assert!(matches!(
                db.write(cx, ghost).await,
                Err(WriteError::UnknownVertex { vid: VId(99) })
            ));
            assert_eq!(
                db.frontier().expect("healthy frontier after refusal"),
                frontier,
                "the refusal consumed no sequence"
            );
        }

        // NOTHING crosses this line except `dir` and `keys()`.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        let row = db
            .vertex(VId(1))
            .expect("reads")
            .expect("the updated vertex survives");
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

        let row = db.vertex(VId(1)).expect("reads").expect("readable");
        assert_eq!(row.props, vec![(key, CanonicalScalar::Int(2))]);
        assert_eq!(row.labels, vec![LabelId(9)]);
        drop(db);
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("survives").props,
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
            db.partition_root().expect("healthy root")
        };
        let second = {
            let db = Database::open(cx, &dir, keys()).await.expect("reopens");
            db.partition_root().expect("healthy root")
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
            db.partition_root().expect("healthy empty root")
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
            db.frontier().expect("healthy frontier"),
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
                first.create_vertex(VId(1), vec![], vec![(PropertyKeyId(7), CanonicalScalar::Int(1))]);
                first.create_vertex(VId(2), vec![], vec![]);
                first.add_edge(EId(10), VId(1), VId(2), vec![(PropertyKeyId(7), CanonicalScalar::Int(1))]);
                db.write(cx, first).await.expect("the first batch commits");
                assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(2)]);

                // Creates AND updates in the crashed batch (fgdb-ls5b): the
                // retire + content-successor chains must be exactly as atomic
                // as the creations beside them — a crash can never leave a
                // retired statement without its successor.
                let mut second = WriteBatch::new(KNOWS);
                second.create_vertex(VId(3), vec![], vec![]);
                second.create_vertex(VId(4), vec![], vec![]);
                second.add_edge(EId(11), VId(1), VId(3), vec![]);
                second.add_edge(EId(12), VId(1), VId(4), vec![]);
                second.set_vertex_property(VId(1), PropertyKeyId(7), Some(CanonicalScalar::Int(2)));
                second.set_edge_property(EId(10), PropertyKeyId(7), Some(CanonicalScalar::Int(2)));
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
            // The updates share the batch's fate exactly: the answered rows
            // are the OLD content when the batch is absent and the NEW when
            // present — decided by the same adjacency answer, so a chain that
            // half-applied (retired without its successor, or vice versa)
            // cannot hide behind either verdict.
            let expected = if after == vec![VId(2)] { 1 } else { 2 };
            assert_eq!(
                db.vertex(VId(1))
                    .expect("reads")
                    .expect("survives")
                    .props,
                vec![(PropertyKeyId(7), CanonicalScalar::Int(expected))],
                "{point:?}: the vertex update must share the batch's fate"
            );
            assert_eq!(
                db.edge(EId(10)).expect("reads").expect("survives").props,
                vec![(PropertyKeyId(7), CanonicalScalar::Int(expected))],
                "{point:?}: the edge update must share the batch's fate"
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

/// A failure after D2 must not leave a callable handle whose derived snapshot
/// is one commit behind the authoritative stream (`fgdb-l96k`).
///
/// The fault is real filesystem behaviour, not a mock: after the first publish
/// we move `manifest.root` aside and put a directory at the same path. The
/// second commit therefore reaches D2, publishes its immutable Strata objects
/// and manifest, then fails when the root-slot store tries to open that path as
/// a file. Restoring the path before the third write is the important control:
/// without a poisoned-handle transition, that third write succeeds from the
/// stale fold and publishes a root that omits the already-durable second commit.
#[test]
fn post_d2_publication_failure_blocks_the_stale_handle_and_reopen_recovers() {
    let dir = scratch("post-d2-stale-handle");
    under_lab(1096, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");

        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        db.write(cx, first).await.expect("first commit publishes");

        let root = dir.join(fgdb_chronicle::store::ROOT_FILE_NAME);
        let saved_root = dir.join("manifest.root.before-post-d2-fault");
        let fault_artifact = dir.join("manifest.root.post-d2-fault");
        std::fs::rename(&root, &saved_root).expect("moves the valid root aside");
        std::fs::create_dir(&root).expect("puts a non-file at the root path");

        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(2), vec![], vec![]);
        let write_error = db
            .write(cx, second)
            .await
            .expect_err("root-slot publication must fail after the Chronicle commit is durable");
        assert!(
            matches!(
                &write_error,
                WriteError::CommittedNeedsRecovery { source, .. }
                    if matches!(**source, fgdb::RebuildError::Slot(_))
            ),
            "the Chronicle commit must be durable before root-slot publication fails: \
             {write_error:?}"
        );
        let WriteError::CommittedNeedsRecovery { recovery, .. } = write_error else {
            return;
        };
        assert_eq!(recovery.durable_frontier, CommitSeq(2));
        assert_eq!(recovery.published_frontier, CommitSeq(1));
        assert_eq!(
            recovery.failed_stage,
            fgdb::DerivedPublicationStage::PublishRootSlot
        );
        assert_eq!(
            db.state(),
            fgdb::DatabaseState::NeedsAuthoritativeRecovery(recovery)
        );
        let stale_state = db.state();
        assert!(matches!(
            db.compact(cx).await,
            Err(fgdb::RebuildError::HandleNotHealthy(found)) if found == stale_state
        ));
        let read_fence = db
            .vertex(VId(2))
            .expect_err("a post-D2 publication failure must fence reads");
        let fgdb::ReadError::RecoveryRequired(read_recovery) = read_fence else {
            return;
        };
        assert_eq!(read_recovery, recovery);

        // Restore the ordinary filesystem before probing the SAME handle. A
        // second error caused merely by the still-present fault would not prove
        // that stale state was fenced off.
        std::fs::rename(&root, &fault_artifact).expect("moves the fault aside");
        std::fs::rename(&saved_root, &root).expect("restores the valid root");

        let mut third = WriteBatch::new(KNOWS);
        third.create_vertex(VId(3), vec![], vec![]);
        let retry_fence = db
            .write(cx, third)
            .await
            .expect_err("a post-D2 publication failure must fence later writes");
        let WriteError::RecoveryRequired(write_recovery) = retry_fence else {
            return;
        };
        assert_eq!(write_recovery, recovery);
        let mut reopened = db
            .recover_authoritatively(cx)
            .await
            .expect("the positive recovery path rebuilds the durable second commit");
        assert!(matches!(
            reopened.state(),
            fgdb::DatabaseState::Healthy {
                published_frontier: CommitSeq(2)
            }
        ));
        assert!(reopened.vertex(VId(1)).expect("reads").is_some());
        assert!(reopened.vertex(VId(2)).expect("reads").is_some());
        assert!(reopened.vertex(VId(3)).expect("reads").is_none());

        let mut after_recovery = WriteBatch::new(KNOWS);
        after_recovery.create_vertex(VId(3), vec![], vec![]);
        reopened
            .write(cx, after_recovery)
            .await
            .expect("the recovered handle accepts the next write");
        assert!(reopened.vertex(VId(1)).expect("reads").is_some());
        assert!(reopened.vertex(VId(2)).expect("reads").is_some());
        assert!(reopened.vertex(VId(3)).expect("reads").is_some());
    });
}

/// Once Chronicle has started appending the marker, an I/O error cannot say
/// whether the commit is absent or durable. The coordinator already poisons
/// itself at that instant; the integrated handle must propagate that fence to
/// reads as well as writes instead of presenting its old snapshot as current.
#[test]
fn an_unknown_commit_outcome_fences_reads_and_retries_until_reopen() {
    let dir = scratch("unknown-commit-outcome");
    under_lab(1097, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        db.write(cx, first).await.expect("first commit publishes");

        let mut uncertain = WriteBatch::new(KNOWS);
        uncertain.create_vertex(VId(2), vec![], vec![]);
        assert!(matches!(
            db.write_with_crash(cx, uncertain, Some(CrashPoint::AfterMarkerBeforeD2),)
                .await,
            Err(WriteError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(1),
                ..
            })
        ));
        assert_eq!(
            db.state(),
            fgdb::DatabaseState::CommitOutcomeUnknown {
                published_frontier: CommitSeq(1)
            }
        );
        let uncertain_state = db.state();
        assert!(matches!(
            db.compact(cx).await,
            Err(fgdb::RebuildError::HandleNotHealthy(found)) if found == uncertain_state
        ));
        assert!(matches!(
            db.neighbours(VId(1), KNOWS),
            Err(fgdb::ReadError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(1)
            })
        ));
        assert!(matches!(
            db.frontier(),
            Err(fgdb::ReadError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(1)
            })
        ));
        assert!(matches!(
            db.manifest(),
            Err(fgdb::ReadError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(1)
            })
        ));
        assert!(matches!(
            db.partition_root(),
            Err(fgdb::ReadError::CommitOutcomeUnknown {
                published_frontier: CommitSeq(1)
            })
        ));

        let mut retry = WriteBatch::new(KNOWS);
        retry.create_vertex(VId(3), vec![], vec![]);
        assert!(matches!(
            db.write(cx, retry).await,
            Err(WriteError::HandleCommitOutcomeUnknown {
                published_frontier: CommitSeq(1)
            })
        ));
        let mut reopened = db
            .recover_authoritatively(cx)
            .await
            .expect("authoritative recovery decides the marker outcome");
        assert!(matches!(
            reopened.state(),
            fgdb::DatabaseState::Healthy { .. }
        ));
        assert!(reopened.vertex(VId(1)).expect("reads").is_some());
        let mut after_recovery = WriteBatch::new(KNOWS);
        after_recovery.create_vertex(VId(3), vec![], vec![]);
        reopened
            .write(cx, after_recovery)
            .await
            .expect("a fresh authoritative handle accepts writes");
        assert!(reopened.vertex(VId(3)).expect("reads").is_some());
    });
}

/// Recovery is safe to call without first inspecting the diagnostic state.
/// A healthy handle must be returned directly: trying to open a replacement
/// before releasing its live coordinator would contend with its own writer
/// lease, while dropping and rebuilding would turn an idempotent operation
/// into unnecessary recovery I/O.
#[test]
fn authoritative_recovery_is_identity_for_a_healthy_handle() {
    let dir = scratch("healthy-authoritative-recovery");
    under_lab(1098, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");

        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        db.write(cx, first).await.expect("first commit publishes");

        let mut db = db
            .recover_authoritatively(cx)
            .await
            .expect("a healthy handle is already authoritative");
        assert!(matches!(
            db.state(),
            fgdb::DatabaseState::Healthy {
                published_frontier: CommitSeq(1)
            }
        ));
        assert!(db.vertex(VId(1)).expect("reads").is_some());

        let mut second = WriteBatch::new(KNOWS);
        second.create_vertex(VId(2), vec![], vec![]);
        assert_eq!(db.write(cx, second).await.expect("still writable").0, 2);
    });
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
        assert_eq!(
            db.frontier().expect("healthy fresh frontier"),
            fgdb_types::CommitSeq(0)
        );
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
        assert_eq!(db.frontier().expect("healthy reopened frontier"), s5);

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
            db.vertices()
                .expect("reads")
                .iter()
                .map(|row| row.vid)
                .collect::<Vec<_>>(),
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

/// **THE MANIFEST LAW (fgdb-63w2): every publish leaves a durable manifest
/// naming the current root, the identity re-derives across a reopen, and it
/// resolves through a FRESH store handle** — the exact object a root slot
/// carries, proven to work with nothing held in memory.
#[test]
fn every_publish_leaves_a_resolvable_manifest() {
    let dir = scratch("manifest");
    under_lab(89, move |cx| async move {
        let cx = &cx;
        let (manifest_after_writes, root_after_writes) = {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, batch).await.expect("commits");
            let manifest_first = db.manifest().expect("healthy first manifest");
            let mut second = WriteBatch::new(KNOWS);
            second.delete_edge(EId(10));
            db.write(cx, second).await.expect("commits");
            assert_ne!(
                db.manifest().expect("healthy second manifest"),
                manifest_first,
                "a new root means a new manifest — the binding is per publish"
            );
            (
                db.manifest().expect("healthy manifest"),
                db.partition_root().expect("healthy root"),
            )
        };

        // NOTHING crosses this line except `dir`, `keys()`, and the two
        // identities a root slot would durably hold.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.manifest().expect("healthy reopened manifest"),
            manifest_after_writes,
            "the rebuild re-derives the same manifest identity"
        );

        // The manifest resolves through a fresh store handle to the root the
        // database is actually serving from.
        drop(db);
        let store =
            fgdb_strata::store::BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("store opens");
        let resolved = store
            .resolve_manifest(cx, manifest_after_writes)
            .expect("the manifest resolves");
        assert_eq!(resolved.len(), 1, "one partition in the spine");
        let root_bytes = fgdb_strata::root::encode_root(&resolved[0].1).expect("re-encodes");
        assert_eq!(
            fgdb_strata::root::root_id(&K_OID, NAMESPACE, &root_bytes),
            root_after_writes.0,
            "the manifest names the root the database published last"
        );
    });
}

/// **THE EDGE UPDATE LAW (fgdb-ls5b): a live edge's properties change without
/// its identity or lifetime changing — durably as a retire + content
/// successor — so the frontier answers the new row while every pre-update
/// sequence keeps answering the row it always had.**
///
/// The as-of assertions are the law's teeth: a naive restatement model
/// (rewrite the row in place, last block wins) passes every frontier check
/// and silently leaks the new row into history.
#[test]
fn edge_property_updates_version_the_row_without_respending_the_identity() {
    let dir = scratch("edge-update");
    under_lab(83, move |cx| async move {
        let cx = &cx;
        let weight = PropertyKeyId(7);
        let label = PropertyKeyId(9);
        let text = CanonicalScalar::ucs_basic_text("close").expect("admissible");

        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(
            EId(10),
            VId(1),
            VId(2),
            vec![(weight, CanonicalScalar::Int(1))],
        );
        let s1 = db.write(cx, first).await.expect("commits");

        let mut second = WriteBatch::new(KNOWS);
        second.set_edge_property(EId(10), weight, Some(CanonicalScalar::Int(2)));
        second.set_edge_property(EId(10), label, Some(text.clone()));
        let s2 = db.write(cx, second).await.expect("commits");

        let mut third = WriteBatch::new(KNOWS);
        third.set_edge_property(EId(10), weight, None);
        let s3 = db.write(cx, third).await.expect("commits");

        // SAME-BATCH create + update folds to one durable statement carrying
        // the final row — the intermediate never existed on any snapshot.
        let mut fourth = WriteBatch::new(KNOWS);
        fourth.add_edge(
            EId(11),
            VId(2),
            VId(1),
            vec![(weight, CanonicalScalar::Int(7))],
        );
        fourth.set_edge_property(EId(11), weight, Some(CanonicalScalar::Int(8)));
        let s4 = db.write(cx, fourth).await.expect("commits");

        // Refusals: an unknown edge and a deleted edge both refuse.
        let mut ghost = WriteBatch::new(KNOWS);
        ghost.set_edge_property(EId(99), weight, Some(CanonicalScalar::Int(0)));
        assert!(matches!(
            db.write(cx, ghost).await,
            Err(fgdb::WriteError::UnknownEdge { .. })
        ));
        let mut gone = WriteBatch::new(KNOWS);
        gone.delete_edge(EId(11));
        db.write(cx, gone).await.expect("commits");
        let mut late = WriteBatch::new(KNOWS);
        late.set_edge_property(EId(11), weight, Some(CanonicalScalar::Int(9)));
        assert!(matches!(
            db.write(cx, late).await,
            Err(fgdb::WriteError::UnknownEdge { .. })
        ));
        drop(db);

        // NOTHING crosses this line except `dir`, `keys()`, the recorded
        // sequences, and the expected rows.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        let props_at = |as_of| {
            db.edge_at(EId(10), as_of)
                .expect("reads")
                .expect("alive at every probed sequence")
                .props
        };
        assert_eq!(props_at(s1), vec![(weight, CanonicalScalar::Int(1))]);
        assert_eq!(
            props_at(s2),
            vec![(weight, CanonicalScalar::Int(2)), (label, text.clone())],
            "the frontier of s2 answers the updated row"
        );
        assert_eq!(props_at(s3), vec![(label, text)], "the unset key is gone");
        let record = db.edge(EId(10)).expect("reads").expect("alive");
        assert_eq!(
            record.entry.retired_at, None,
            "updates never touch the lifetime the reader sees"
        );
        // Identity is not respent: topology and the neighbour answer are
        // stable across every content version.
        assert_eq!(
            db.neighbours_at(VId(1), KNOWS, s1).expect("reads"),
            db.neighbours_at(VId(1), KNOWS, s3).expect("reads"),
        );
        // The same-batch fold left exactly one durable statement.
        let folded = db
            .edge_at(EId(11), s4)
            .expect("reads")
            .expect("alive at s4");
        assert_eq!(folded.props, vec![(weight, CanonicalScalar::Int(8))]);
        assert_eq!(
            folded.entry.created_at, s4,
            "one statement, born at its commit"
        );
    });
}

/// **THE ORDER-SENSITIVITY REFUSAL (fgdb-kokz, found by the generated
/// differential, seed 11): a batch whose meaning depends on submission order
/// refuses BEFORE any byte is durable.** The durable template's row order is
/// canonical byte order, so update-then-delete of one element — directly or
/// through a cascade — and a double touch of one exact field would commit
/// bytes that replay in a different order than the caller meant, and the
/// refusal would otherwise arrive AFTER the irreversible commit, poisoning
/// the database against its own reopen.
#[test]
fn order_sensitive_batches_refuse_before_anything_is_durable() {
    let dir = scratch("order-sensitive");
    under_lab(91, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(EId(10), VId(1), VId(2), vec![]);
        let s1 = db.write(cx, first).await.expect("commits");

        // Update-then-delete, both element kinds — including the cascade
        // route, where the delete reaches the updated edge through a vertex.
        let mut vertex_case = WriteBatch::new(KNOWS);
        vertex_case.set_vertex_property(VId(1), key, None);
        vertex_case.delete_vertex(VId(1));
        assert!(matches!(
            db.write(cx, vertex_case).await,
            Err(fgdb::WriteError::OrderSensitiveBatch { .. })
        ));
        let mut edge_case = WriteBatch::new(KNOWS);
        edge_case.set_edge_property(EId(10), key, Some(CanonicalScalar::Int(1)));
        edge_case.delete_edge(EId(10));
        assert!(matches!(
            db.write(cx, edge_case).await,
            Err(fgdb::WriteError::OrderSensitiveBatch { .. })
        ));
        let mut cascade_case = WriteBatch::new(KNOWS);
        cascade_case.set_edge_property(EId(10), key, Some(CanonicalScalar::Int(1)));
        cascade_case.delete_vertex(VId(2));
        assert!(matches!(
            db.write(cx, cascade_case).await,
            Err(fgdb::WriteError::OrderSensitiveBatch { .. })
        ));
        // A double touch of one exact field; a DIFFERENT field commutes and
        // stays lawful — the control that keeps this a refusal about order,
        // not about updates.
        let mut double = WriteBatch::new(KNOWS);
        double.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(1)));
        double.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(2)));
        assert!(matches!(
            db.write(cx, double).await,
            Err(fgdb::WriteError::OrderSensitiveBatch { .. })
        ));
        let mut commuting = WriteBatch::new(KNOWS);
        commuting.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(1)));
        commuting.set_vertex_property(VId(1), PropertyKeyId(9), Some(CanonicalScalar::Int(2)));
        let s2 = db
            .write(cx, commuting)
            .await
            .expect("distinct fields commute");

        // Identity reuse refusals, also pre-commit (the fold's spent law).
        let mut recreate = WriteBatch::new(KNOWS);
        recreate.create_vertex(VId(1), vec![], vec![]);
        assert!(matches!(
            db.write(cx, recreate).await,
            Err(fgdb::WriteError::AlreadyLive { .. })
        ));
        drop(db);

        // NOTHING refused above became durable: the reopened database sits at
        // exactly the two acknowledged commits.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(db.frontier().expect("healthy reopened frontier"), s2);
        assert!(db.vertex_at(VId(1), s1).expect("reads").is_some());
    });
}

/// **THE REVERSE READ LAW (fgdb-x164): `in_neighbours` answers the sources
/// arriving at a vertex — with the direction controls that catch a face
/// accidentally answering the forward merge.** A self-loop appears on both
/// faces; an asymmetric pair appears on exactly one each; deletes and
/// history behave as everywhere else.
#[test]
fn in_neighbours_answers_sources_and_respects_direction() {
    let dir = scratch("in-neighbours");
    under_lab(87, move |cx| async move {
        let cx = &cx;
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(3), VId(2), vec![]);
        batch.add_edge(EId(12), VId(2), VId(2), vec![]); // self-loop
        let s1 = db.write(cx, batch).await.expect("commits");
        let mut second = WriteBatch::new(KNOWS);
        second.delete_edge(EId(11));
        let s2 = db.write(cx, second).await.expect("commits");
        drop(db);

        // NOTHING crosses this line except `dir`, `keys()`, and sequences.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.in_neighbours_at(VId(2), KNOWS, s1).expect("reads"),
            vec![VId(1), VId(2), VId(3)],
            "every arriving source, self-loop included"
        );
        assert_eq!(
            db.in_neighbours(VId(2), KNOWS).expect("reads"),
            vec![VId(1), VId(2)],
            "the deleted arrival is gone at the frontier"
        );
        assert_eq!(
            db.in_neighbours(VId(1), KNOWS).expect("reads"),
            vec![],
            "nothing arrives at the pure source — the face that answered the \
             forward merge here would say [2]"
        );
        assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(2)]);
        assert_eq!(
            db.in_neighbours(VId(2), WORKS_WITH).expect("reads"),
            vec![],
            "keyed on relation, as every read is"
        );
        let future = CommitSeq(s2.0 + 1);
        assert!(matches!(
            db.in_neighbours_at(VId(2), KNOWS, future),
            Err(fgdb::ReadError::BeyondFrontier { .. })
        ));
    });
}

/// **THE ROOT SLOT LAW (fgdb-ge6a, PLAIN opener ruling): every publish
/// advances the one mutable object in the directory to name the current
/// manifest; open continues, heals a lagging slot forward, and refuses a
/// slot the stream cannot account for or that is not this database's.**
#[test]
fn the_root_slot_names_the_current_manifest_and_open_reconciles_it() {
    let dir = scratch("root-slot");
    under_lab(95, move |cx| async move {
        let cx = &cx;
        let (manifest_first, manifest_second) = {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(VId(1), vec![], vec![]);
            db.write(cx, first).await.expect("commits");
            let manifest_first = db.manifest().expect("healthy first manifest");
            let mut second = WriteBatch::new(KNOWS);
            second.create_vertex(VId(2), vec![], vec![]);
            db.write(cx, second).await.expect("commits");
            (
                manifest_first,
                db.manifest().expect("healthy second manifest"),
            )
        };

        // The slot is durably CURRENT with nothing held in memory: a fresh
        // RootStore selects a slot naming the last manifest.
        let slot_store = fgdb_chronicle::RootStore::new(&dir);
        let published = slot_store.current(cx).await.expect("a slot exists");
        assert_eq!(published.root_manifest_oid, manifest_second.0.0);
        let generation_before = published.slot_generation;
        assert!(generation_before >= 3, "create + two writes each published");

        // Reopen continues the generation — no spurious heal.
        {
            let db = Database::open(cx, &dir, keys()).await.expect("reopens");
            assert_eq!(
                db.manifest().expect("healthy reopened manifest"),
                manifest_second
            );
        }
        let after_reopen = slot_store.current(cx).await.expect("still selects");
        assert_eq!(
            after_reopen.slot_generation, generation_before,
            "an agreeing slot is continued, not republished"
        );

        // A LAGGING slot — the crash window's exact shape: a newer generation
        // naming the previous (resolvable) manifest. Open heals it forward.
        let mut lagging = after_reopen.clone();
        lagging.slot_generation += 1;
        lagging.root_manifest_oid = manifest_first.0.0;
        slot_store
            .publish_evidenced(cx, &lagging)
            .await
            .expect("publishes the lag shape");
        {
            let db = Database::open(cx, &dir, keys())
                .await
                .expect("heals and opens");
            assert_eq!(
                db.manifest().expect("healthy healed manifest"),
                manifest_second
            );
        }
        let healed = slot_store.current(cx).await.expect("selects");
        assert_eq!(healed.root_manifest_oid, manifest_second.0.0);
        assert_eq!(healed.slot_generation, lagging.slot_generation + 1);

        // A slot naming a manifest the stream cannot account for: refused.
        let mut phantom = healed.clone();
        phantom.slot_generation += 1;
        phantom.root_manifest_oid = [0xEE; 32];
        slot_store
            .publish_evidenced(cx, &phantom)
            .await
            .expect("publishes the phantom shape");
        assert!(matches!(
            Database::open(cx, &dir, keys()).await,
            Err(fgdb::OpenError::SlotDisagreesWithStream { .. })
        ));

        // A slot whose identity tuple is not this database's: refused as
        // foreign even though it points at the right manifest.
        let mut foreign = healed.clone();
        foreign.slot_generation = phantom.slot_generation + 1;
        foreign.database_security_namespace_id = [0x01; 32];
        slot_store
            .publish_evidenced(cx, &foreign)
            .await
            .expect("publishes the foreign shape");
        assert!(matches!(
            Database::open(cx, &dir, keys()).await,
            Err(fgdb::OpenError::ForeignSlot { .. })
        ));
    });
}

/// A content-addressed checkpoint is authentic BYTES, not proof that those
/// bytes project this Chronicle prefix. Two databases under the same keys and
/// namespace can each produce a perfectly resolvable sequence-1 manifest. A
/// slot transplant must therefore be refused even though every object-level
/// identity check succeeds; accepting it would replace committed history A
/// with unrelated history B without changing Chronicle at all.
#[test]
fn a_resolvable_checkpoint_from_a_divergent_history_is_refused() {
    let primary = scratch("checkpoint-primary");
    let divergent = scratch("checkpoint-divergent");
    under_lab(101, move |cx| async move {
        let cx = &cx;
        let (primary_manifest, primary_root) = {
            let mut db = Database::create(cx, &primary, keys())
                .await
                .expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            db.write(cx, batch).await.expect("commits primary history");
            (
                db.manifest().expect("healthy primary manifest"),
                db.partition_root().expect("healthy primary root"),
            )
        };
        let (divergent_manifest, divergent_root) = {
            let mut db = Database::create(cx, &divergent, keys())
                .await
                .expect("creates divergent database");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(999), vec![], vec![]);
            db.write(cx, batch)
                .await
                .expect("commits divergent history");
            (
                db.manifest().expect("healthy divergent manifest"),
                db.partition_root().expect("healthy divergent root"),
            )
        };
        assert_ne!(primary_manifest, divergent_manifest);
        assert_ne!(primary_root, divergent_root);

        // Copy the complete immutable closure, preserving every real identity.
        // The destination can resolve and admit this manifest; only its absent
        // Chronicle-prefix binding makes it unlawful here.
        let source = fgdb_strata::store::BlockStore::open(cx, &divergent, K_OID, NAMESPACE)
            .expect("opens divergent object store");
        let destination = fgdb_strata::store::BlockStore::open(cx, &primary, K_OID, NAMESPACE)
            .expect("opens primary object store");
        let root = source
            .get_root(cx, divergent_root)
            .expect("reads divergent root");
        for reference in &root.blocks {
            let bytes = source
                .get_bytes(cx, fgdb_strata::DeltaBlockVersion(reference.block_id))
                .expect("reads divergent block");
            let stored = destination.put(cx, &bytes).expect("copies divergent block");
            assert_eq!(stored.0, reference.block_id);
        }
        for reference in &root.vertex_patches {
            let bytes = source
                .get_patch_bytes(
                    cx,
                    fgdb_strata::vertex::VertexPatchVersion(reference.patch_id),
                )
                .expect("reads divergent vertex patch");
            let stored = destination
                .put_patch(cx, &bytes)
                .expect("copies divergent vertex patch");
            assert_eq!(stored.0, reference.patch_id);
        }
        let stored_root = destination
            .put_root(cx, &root)
            .expect("admits divergent root");
        assert_eq!(stored_root, divergent_root);
        // The transplant carries the divergent history's OWN record verbatim —
        // including its published_chain_hash (fgdb-90hw), which was validly
        // produced by THAT history's chain. The refusal below is therefore the
        // binding law itself: the primary's recovered chain hashes differently
        // at the same sequence, and no amount of structural validity survives
        // the one comparison.
        let divergent_records: Vec<_> = source
            .resolve_manifest(cx, divergent_manifest)
            .expect("resolves the divergent manifest in its own store")
            .into_iter()
            .map(|(record, _)| record)
            .collect();
        let stored_manifest = destination
            .put_manifest(cx, &divergent_records)
            .expect("copies divergent manifest");
        assert_eq!(stored_manifest, divergent_manifest);

        let slot_store = fgdb_chronicle::RootStore::new(&primary);
        let mut transplanted = slot_store.current(cx).await.expect("selects current slot");
        transplanted.slot_generation += 1;
        transplanted.root_manifest_oid = divergent_manifest.0.0;
        slot_store
            .publish_evidenced(cx, &transplanted)
            .await
            .expect("publishes a structurally valid transplant");

        assert!(matches!(
            Database::open(cx, &primary, keys()).await,
            Err(fgdb::OpenError::SlotDisagreesWithStream { .. })
        ));
    });
}

/// **THE CHECKPOINT-SELECTED OPEN EQUIVALENCE LAW (fgdb-ge6a): opening through
/// the slot's verified manifest and opening by folding the whole stream are
/// indistinguishable —
/// same root, same manifest, same frontier, same answers, and the SAME next
/// root after one more write.** The last clause is the sharp one: it proves
/// the derived writer state (live maps, chains, versions, birth ordinals)
/// equals the folded one, because publication is deterministic in exactly
/// that state.
#[test]
fn checkpoint_selected_open_is_indistinguishable_from_the_stream_fold() {
    let dir = scratch("fast-open");
    under_lab(99, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut first = WriteBatch::new(KNOWS);
            first.create_vertex(VId(1), vec![LabelId(3)], vec![]);
            first.create_vertex(VId(2), vec![], vec![(key, CanonicalScalar::Int(1))]);
            first.add_edge(
                EId(10),
                VId(1),
                VId(2),
                vec![(key, CanonicalScalar::Int(5))],
            );
            db.write(cx, first).await.expect("commits");
            let mut second = WriteBatch::new(KNOWS);
            second.create_vertex(VId(3), vec![], vec![]);
            second.add_edge(EId(11), VId(3), VId(1), vec![]);
            second.set_edge_property(EId(10), key, Some(CanonicalScalar::Int(6)));
            second.set_vertex_property(VId(2), key, None);
            db.write(cx, second).await.expect("commits");
            let mut third = WriteBatch::new(KNOWS);
            third.delete_edge(EId(11));
            third.delete_vertex(VId(3));
            db.write(cx, third).await.expect("commits");
        }

        // Both open paths, sequentially (the lease is single-writer), each
        // interrogated identically.
        let selected = Database::open(cx, &dir, keys())
            .await
            .expect("checkpoint-selected open succeeds");
        let selected_state = (
            selected.partition_root().expect("healthy selected root"),
            selected.manifest().expect("healthy selected manifest"),
            selected.frontier().expect("healthy selected frontier"),
            selected.vertices().expect("reads"),
            selected.edges().expect("reads"),
            selected.element_versions().expect("reads").clone(),
        );
        drop(selected);
        let slow = Database::open_rebuilding(cx, &dir, keys())
            .await
            .expect("rebuild opens");
        assert_eq!(
            (
                slow.partition_root().expect("healthy rebuilt root"),
                slow.manifest().expect("healthy rebuilt manifest"),
                slow.frontier().expect("healthy rebuilt frontier"),
                slow.vertices().expect("reads"),
                slow.edges().expect("reads"),
                slow.element_versions().expect("reads").clone(),
            ),
            selected_state,
            "the two open paths must be indistinguishable — including the v3 \
             element-version heads, which the graph answers cannot witness \
             (an updated element answers from its final row alone, so a head \
             that chained through the wrong statements stays invisible to \
             every scan comparison)"
        );
        drop(slow);

        // One more write after EACH path — the roots must match, which pins
        // the derived writer state itself, not just the published artifacts.
        let mut selected = Database::open(cx, &dir, keys())
            .await
            .expect("checkpoint-selected open succeeds");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(4), vec![], vec![(key, CanonicalScalar::Int(9))]);
        batch.add_edge(EId(12), VId(4), VId(1), vec![]);
        batch.set_vertex_label(VId(1), LabelId(3), false);
        selected
            .write(cx, batch)
            .await
            .expect("commits after checkpoint-selected open");
        let root_after_selected = selected.partition_root().expect("healthy selected root");
        let scans_after_selected = (
            selected.vertices().expect("reads"),
            selected.edges().expect("reads"),
            selected.element_versions().expect("reads").clone(),
        );
        drop(selected);

        // The write above advanced durable history, so the rebuild control
        // now folds THAT stream — its post-open state must equal what the
        // checkpoint-selected session already answered.
        let slow = Database::open_rebuilding(cx, &dir, keys())
            .await
            .expect("rebuild reopens");
        assert_eq!(
            slow.partition_root().expect("healthy rebuilt root"),
            root_after_selected
        );
        assert_eq!(
            (
                slow.vertices().expect("reads"),
                slow.edges().expect("reads"),
                slow.element_versions().expect("reads").clone(),
            ),
            scans_after_selected,
            "a write through the checkpoint-selected session is the same write \
             — and its statement chains landed on the same v3 heads"
        );
    });
}

/// **CHECKPOINT AUTHENTICATION IS READ-ONLY OVER DERIVED OBJECTS.** Opening a
/// healthy database must authenticate Chronicle membership and decode the
/// selected Strata checkpoint without republishing the replay used for that
/// comparison. Besides wasting an inode+directory fsync for every immutable
/// object, republishing would require write access to objects whose content is
/// already final.
///
/// This is deliberately not a read-only-database claim: the coordinator and
/// root slot remain writable, and a later commit would correctly fail once it
/// needed to publish a new Strata object. It pins only the sharper law that
/// checkpoint selection itself does not write the objects it is verifying.
#[test]
fn checkpoint_authentication_does_not_republish_immutable_objects() {
    let dir = scratch("checkpoint-read-only-objects");
    under_lab(1099, move |cx| async move {
        let cx = &cx;
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(KNOWS);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, batch).await.expect("commits");
        }

        let object_dir = dir.join(fgdb_strata::store::BLOCK_DIR);
        let mut protected = 0usize;
        for entry in std::fs::read_dir(&object_dir).expect("lists derived objects") {
            let entry = entry.expect("reads directory entry");
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let metadata = entry.metadata().expect("reads object metadata");
            if !metadata.is_file() {
                continue;
            }
            let mut permissions = metadata.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(entry.path(), permissions)
                .expect("makes the immutable object read-only");
            protected += 1;
        }
        assert!(
            protected > 0,
            "the fixture must protect real Strata objects"
        );

        let db = Database::open(cx, &dir, keys())
            .await
            .expect("checkpoint authentication only reads immutable objects");
        assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(2)]);
    });
}

/// **THE DURABLE COMPACTION LAW (fgdb-ge6a): consolidation shrinks the
/// partition, changes NO answer at ANY committed sequence, and — because
/// open prefers the manifest — the compacted generation is what a reopen
/// actually lands on.** The rebuild face remains the authoritative recovery
/// and re-derives the uncompacted layout with identical answers.
#[test]
fn compaction_is_durable_and_answer_preserving_at_every_sequence() {
    let dir = scratch("durable-compact");
    under_lab(103, move |cx| async move {
        let cx = &cx;
        let key = PropertyKeyId(7);
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        // Churn: creates, updates, deletes — the shapes consolidation folds.
        let mut epochs = Vec::new();
        let mut first = WriteBatch::new(KNOWS);
        first.create_vertex(VId(1), vec![], vec![]);
        first.create_vertex(VId(2), vec![], vec![]);
        first.add_edge(
            EId(10),
            VId(1),
            VId(2),
            vec![(key, CanonicalScalar::Int(1))],
        );
        first.add_edge(EId(11), VId(1), VId(2), vec![]);
        epochs.push(db.write(cx, first).await.expect("commits"));
        let mut second = WriteBatch::new(KNOWS);
        second.set_edge_property(EId(10), key, Some(CanonicalScalar::Int(2)));
        second.delete_edge(EId(11));
        second.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(3)));
        epochs.push(db.write(cx, second).await.expect("commits"));
        let mut third = WriteBatch::new(KNOWS);
        third.add_edge(EId(12), VId(2), VId(1), vec![]);
        third.set_vertex_label(VId(1), LabelId(3), true);
        third.set_vertex_property(VId(2), key, Some(CanonicalScalar::Int(4)));
        epochs.push(db.write(cx, third).await.expect("commits"));

        let blocks_before = db.partition_root().expect("healthy pre-compact root");
        let answers = |db: &Database, epochs: &[CommitSeq]| {
            epochs
                .iter()
                .map(|as_of| {
                    (
                        db.vertices_at(*as_of).expect("reads"),
                        db.edges_at(*as_of).expect("reads"),
                        db.neighbours_at(VId(1), KNOWS, *as_of).expect("reads"),
                        db.in_neighbours_at(VId(1), KNOWS, *as_of).expect("reads"),
                    )
                })
                .collect::<Vec<_>>()
        };
        let before = answers(&db, &epochs);

        let patches_before = db.partition_root().expect("healthy pre-compact root");
        let _ = patches_before;
        db.compact(cx).await.expect("compacts");
        assert_ne!(
            db.partition_root().expect("healthy compacted root"),
            blocks_before,
            "consolidation published a replacement generation"
        );
        // Vertex churn built restatement chains across three per-commit
        // patches; consolidation collapses them into one canonical patch.
        let resolved = {
            let store =
                fgdb_strata::store::BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
            store
                .get_root(cx, db.partition_root().expect("healthy compacted root"))
                .expect("the compacted root resolves")
        };
        assert_eq!(
            resolved.vertex_patches.len(),
            1,
            "three per-commit patches consolidated into one"
        );
        assert_eq!(
            answers(&db, &epochs),
            before,
            "consolidation must not move ANY answer at ANY sequence"
        );
        let compacted_root = db.partition_root().expect("healthy compacted root");
        let compacted_manifest = db.manifest().expect("healthy compacted manifest");
        drop(db);

        // SURVIVAL: the fast reopen lands on the compacted generation.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.partition_root().expect("healthy reopened root"),
            compacted_root
        );
        assert_eq!(
            db.manifest().expect("healthy reopened manifest"),
            compacted_manifest
        );
        assert_eq!(answers(&db, &epochs), before);
        // And a write on top of the compacted generation composes.
        let mut db = db;
        let mut fourth = WriteBatch::new(KNOWS);
        fourth.add_edge(EId(13), VId(1), VId(2), vec![]);
        epochs.push(db.write(cx, fourth).await.expect("commits"));
        assert_eq!(db.neighbours(VId(1), KNOWS).expect("reads"), vec![VId(2)],);
        drop(db);

        // AUTHORITATIVE RECOVERY: the stream fold re-derives its own layout
        // — different root, identical answers (doctrine 5).
        let slow = Database::open_rebuilding(cx, &dir, keys())
            .await
            .expect("rebuild opens");
        assert_eq!(
            answers(&slow, &epochs[..3]),
            before,
            "the rebuilt layout answers identically at every epoch"
        );
    });
}
