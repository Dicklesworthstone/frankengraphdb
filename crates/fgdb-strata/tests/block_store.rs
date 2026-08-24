//! Laws of the block store — where a partition stops needing the whole stream.
//!
//! **THE STORE'S ONE JOB IS THAT THE PATH IS NOT THE NAME.** A block's filename is
//! derived from its identity, and a read RE-DERIVES that identity from the bytes it
//! found. Every interesting law here is a way of asking whether the store trusts
//! its own layout: if it does, it returns whatever sits at the expected path, which
//! is the exact failure content-addressing exists to prevent and the one that is
//! silent.
//!
//! The store is the first thing in `fgdb-strata` that touches a disk. Writes use
//! `&CommitCx`, while every synchronous read accepts the shared sealed
//! `StorageReadCx` contract and runs under the caller's role restriction — the
//! doctrine-3 boundary a lab runtime swaps to inject filesystem faults.

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::RelationId;
use fgdb_strata::store::{BLOCK_DIR, BlockStore, BlockStoreCrashPoint, StoreError};
use fgdb_strata::{
    AdjacencyEntry, DeltaBlockVersion, PartitionRootVersion, block_id, encode_block,
};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CommitSeq, EId, VId};
use std::fs::OpenOptions;
use std::future::Future;
use std::path::PathBuf;

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const REL: RelationId = RelationId(1);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-blockstore-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    T: Send + 'static,
    Fut: Future<Output = T> + Send,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

fn entry(src: u128, dst: u128, created: u64) -> AdjacencyEntry {
    edge(
        src.wrapping_mul(1_000_000).wrapping_add(dst),
        src,
        dst,
        created,
        None,
    )
}

fn edge(eid: u128, src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn sample() -> Vec<u8> {
    encode_block(0, None, &[entry(1, 2, 1), entry(1, 3, 2)]).expect("encodes")
}

/// A stored block reads back as the same entries, under the identity the store
/// derived.
#[test]
fn a_stored_block_reads_back() {
    let dir = scratch_dir("roundtrip");
    under_lab(31, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let bytes = sample();
        let id = store.put(&cx, &bytes).await.expect("stores");

        assert_eq!(
            id.0,
            block_id(&K_OID, NAMESPACE, &bytes),
            "derived, not accepted"
        );
        assert!(store.contains(&cx, id).await);
        assert_eq!(
            store.get(&cx, id).await.expect("loads"),
            fgdb_strata::decode_block(&bytes).expect("decodes")
        );
        assert_eq!(store.get_bytes(&cx, id).await.expect("loads bytes"), bytes);
    });
}

/// A durable directory inode is not yet a durable `strata-blocks` name. Model
/// the loss arm with a second database directory rather than deleting the live
/// fixture: it contains only namespace entries that crossed the database-parent
/// barrier at the named instant.
#[test]
fn store_directory_creation_waits_for_database_directory_sync() {
    let working_dir = scratch_dir("store-dirent-working");
    let crash_image = scratch_dir("store-dirent-image");
    under_lab(39, move |cx| async move {
        let crashed = BlockStore::open_with_crash(
            &cx,
            &working_dir,
            K_OID,
            NAMESPACE,
            Some(BlockStoreCrashPoint::AfterStoreDirectorySyncBeforeDatabaseDirectorySync),
        )
        .await;
        assert!(
            crashed.is_err(),
            "open must not acknowledge before parent sync"
        );
        assert!(
            working_dir.join(BLOCK_DIR).is_dir(),
            "the directory inode exists in the working view"
        );
        assert!(
            !crash_image.join(BLOCK_DIR).exists(),
            "the legal loss-arm image has no unsynced directory entry"
        );

        let lost = BlockStore::open(&cx, &crash_image, K_OID, NAMESPACE)
            .await
            .expect("reopen loss-arm database");
        assert!(
            !lost
                .contains(&cx, DeltaBlockVersion(ObjectId([0x23; 32])))
                .await
        );

        // The other legal outcome is that the unsynced name survived. Reopen
        // re-establishes both directory barriers before any write is accepted.
        let survived = BlockStore::open(&cx, &working_dir, K_OID, NAMESPACE)
            .await
            .expect("reopen survival-arm database");
        let bytes = sample();
        let id = survived.put(&cx, &bytes).await.expect("store after reopen");
        assert_eq!(survived.get_bytes(&cx, id).await.expect("read"), bytes);
    });
}

/// Syncing a block inode does not publish its content-addressed name. The
/// working view proves the bytes exist; a second store is the legal crash image
/// in which the unsynced dirent was lost. Neither fixture is deleted or
/// rewritten to model the crash.
#[test]
fn block_creation_waits_for_store_directory_sync() {
    let working_dir = scratch_dir("block-dirent-working");
    let crash_image = scratch_dir("block-dirent-image");
    under_lab(40, move |cx| async move {
        let bytes = sample();
        let id = DeltaBlockVersion(block_id(&K_OID, NAMESPACE, &bytes));
        let store = BlockStore::open(&cx, &working_dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let crashed = store
            .put_with_crash(
                &cx,
                &bytes,
                Some(BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync),
            )
            .await;
        assert!(
            crashed.is_err(),
            "put must not acknowledge before directory sync"
        );
        assert!(
            store.path(id.0).is_file(),
            "the durable inode exists in the working view"
        );
        drop(store);

        let lost = BlockStore::open(&cx, &crash_image, K_OID, NAMESPACE)
            .await
            .expect("reopen loss-arm database");
        assert!(
            !lost.contains(&cx, id).await,
            "the legal loss arm cannot resolve an unsynced block name"
        );

        let survived = BlockStore::open(&cx, &working_dir, K_OID, NAMESPACE)
            .await
            .expect("reopen survival-arm database");
        assert_eq!(
            survived
                .get_bytes(&cx, id)
                .await
                .expect("surviving dirent reads"),
            bytes
        );
    });
}

/// A writer may die after its staging inode is complete. That inode is not a
/// block until publication: the canonical path must remain absent, and the next
/// publication permit owner must be able to reuse the staging slot and finish.
#[test]
fn a_staging_crash_never_exposes_partial_canonical_bytes() {
    let dir = scratch_dir("staging-before-publication");
    under_lab(43, move |cx| async move {
        let bytes = sample();
        let id = DeltaBlockVersion(block_id(&K_OID, NAMESPACE, &bytes));
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");

        let interrupted = store
            .put_with_crash(
                &cx,
                &bytes,
                Some(BlockStoreCrashPoint::AfterStagingFileSyncBeforePublication),
            )
            .await;
        assert!(
            interrupted.is_err(),
            "staging completion is not canonical publication"
        );
        assert!(
            !store.path(id.0).exists(),
            "no partial or empty canonical path may be visible"
        );

        assert_eq!(
            store.put(&cx, &bytes).await.expect("retry publishes"),
            id,
            "the next permit owner reuses the noncanonical staging slot"
        );
        assert_eq!(
            store.get_bytes(&cx, id).await.expect("published bytes"),
            bytes
        );
    });
}

/// Equal existing bytes are still namespace-uncertain after a reopen. The
/// idempotent path must re-enter the durability barrier instead of returning
/// merely because the bytes happen to be visible in the current kernel view.
#[test]
fn an_equal_existing_block_reestablishes_durability_after_reopen() {
    let dir = scratch_dir("idempotent-reopen-barrier");
    under_lab(44, move |cx| async move {
        let bytes = sample();
        let id = {
            let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
                .await
                .expect("opens");
            store.put(&cx, &bytes).await.expect("initial store")
        };
        let modified_at = std::fs::metadata(
            BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
                .await
                .expect("reopens")
                .path(id.0),
        )
        .and_then(|metadata| metadata.modified())
        .expect("mtime");

        let reopened = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("reopens again");
        let interrupted = reopened
            .put_with_crash(
                &cx,
                &bytes,
                Some(BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync),
            )
            .await;
        assert!(
            interrupted.is_err(),
            "equal existing bytes must not bypass the durability barrier"
        );
        assert_eq!(
            std::fs::metadata(reopened.path(id.0))
                .and_then(|metadata| metadata.modified())
                .expect("mtime after re-put"),
            modified_at,
            "re-establishing durability must not rewrite immutable bytes"
        );
        assert_eq!(reopened.get_bytes(&cx, id).await.expect("read"), bytes);
    });
}

/// **THE LOAD-BEARING LAW: the store does not trust its own path.**
///
/// The file at a block's expected path is replaced with a DIFFERENT lawful block.
/// A store that returned whatever sat there would hand back a valid-looking
/// partition made of the wrong data, with nothing anywhere reporting a problem —
/// and the bytes decode perfectly, so no format check would catch it either.
#[test]
fn bytes_at_the_right_path_that_are_the_wrong_block_are_refused() {
    let dir = scratch_dir("wrongblock");
    under_lab(32, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let mine = sample();
        let id = store.put(&cx, &mine).await.expect("stores");

        // A different, perfectly lawful block written over the path.
        let other = encode_block(0, None, &[entry(9, 9, 7)]).expect("encodes");
        assert_ne!(other, mine);
        std::fs::write(store.path(id.0), &other).expect("overwrite");

        let actual = block_id(&K_OID, NAMESPACE, &other);
        assert!(
            matches!(
                store.get(&cx, id).await,
                Err(StoreError::IdentityMismatch { expected, actual: got })
                    if expected == id.0 && got == actual
            ),
            "a store that trusts its layout returns the wrong partition silently"
        );
        // And the raw-bytes path enforces it too — a caller that skips decoding
        // must not thereby skip the identity check.
        assert!(matches!(
            store.get_bytes(&cx, id).await,
            Err(StoreError::IdentityMismatch { .. })
        ));
    });
}

/// Damaged bytes are refused as an IDENTITY failure, not a parse failure.
///
/// The order matters for the diagnostic: "you fetched the wrong object" and "this
/// object is corrupt" send an operator to completely different places, and damage
/// makes the bytes a different object before it makes them unparseable.
#[test]
fn damaged_bytes_are_refused_by_identity_first() {
    let dir = scratch_dir("damaged");
    under_lab(33, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let id = store.put(&cx, &sample()).await.expect("stores");

        let mut bytes = std::fs::read(store.path(id.0)).expect("read");
        let at = bytes.len() / 2;
        bytes[at] ^= 0x40;
        std::fs::write(store.path(id.0), &bytes).expect("write");

        assert!(matches!(
            store.get(&cx, id).await,
            Err(StoreError::IdentityMismatch { .. })
        ));
    });
}

/// Storing the same bytes twice is a NO-OP, not an overwrite.
///
/// Blocks are immutable and content-addressed, so a repeat write has nothing to
/// change. Truncating and rewriting would take a durable object that is currently
/// readable and make it briefly absent, to replace it with what it already
/// contained — the hazard fgdb-capsule-no-overwrite-pysi names for capsules.
#[test]
fn storing_the_same_bytes_twice_is_a_no_op() {
    let dir = scratch_dir("idempotent");
    under_lab(34, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let bytes = sample();
        let first = store.put(&cx, &bytes).await.expect("stores");
        let modified_at = std::fs::metadata(store.path(first.0))
            .and_then(|m| m.modified())
            .expect("mtime");

        let second = store.put(&cx, &bytes).await.expect("stores again");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(store.path(first.0))
                .and_then(|m| m.modified())
                .expect("mtime"),
            modified_at,
            "the file must not have been rewritten"
        );
        assert_eq!(store.get_bytes(&cx, first).await.expect("loads"), bytes);
    });
}

/// DAMAGE IS REFUSED without being promoted to cryptographic evidence.
///
/// Bytes that derive a different identity from their canonical path are a key or
/// namespace mix-up, a nonconforming writer, or corruption. A true collision is
/// two different byte strings deriving the same keyed identity; conflating those
/// states would make an ordinary repair incident look like a cryptographic break.
#[test]
fn a_damaged_existing_file_is_not_misreported_as_a_collision() {
    let dir = scratch_dir("collision");
    under_lab(35, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let bytes = sample();
        let id = store.put(&cx, &bytes).await.expect("stores");

        // Something else has taken this identity's path with different bytes.
        let other = encode_block(0, None, &[entry(4, 5, 6)]).expect("encodes");
        std::fs::write(store.path(id.0), &other).expect("plant");

        let error = store
            .put(&cx, &bytes)
            .await
            .expect_err("damage must be refused");
        assert!(
            matches!(
                error,
                StoreError::DamagedExisting { expected, actual }
                    if expected == id.0 && actual == block_id(&K_OID, NAMESPACE, &other)
            ),
            "bytes whose own identity differs from their path are damage, not a hash collision: \
             {error:?}"
        );
        assert_eq!(
            std::fs::read(store.path(id.0)).expect("read"),
            other,
            "and the refusal left the existing file untouched"
        );
    });
}

/// THE IDENTITY IS SCOPED TO THE DATABASE, so two stores under different keys do
/// not see each other's blocks even in one directory.
///
/// Without this a block copied between databases would resolve under the wrong
/// key's root — the cross-database confusion `block_id`'s namespace binding exists
/// to prevent, here at the point where it actually matters.
#[test]
fn two_stores_with_different_keys_do_not_share_blocks() {
    let dir = scratch_dir("scoped");
    under_lab(36, move |cx| async move {
        let mine = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let theirs = BlockStore::open(&cx, &dir, [0x11; 32], NAMESPACE)
            .await
            .expect("opens");
        let bytes = sample();

        let my_id = mine.put(&cx, &bytes).await.expect("stores");
        let their_id = theirs.put(&cx, &bytes).await.expect("stores");
        assert_ne!(my_id, their_id, "different keys, different objects");
        assert!(
            !mine.contains(&cx, their_id).await,
            "one key's store must not claim to hold another key's block, even \
             though the file is right there in the shared directory"
        );
        assert!(
            theirs.contains(&cx, their_id).await,
            "while its own store does"
        );

        // Each store resolves its OWN identity and refuses the other's.
        assert_eq!(mine.get_bytes(&cx, my_id).await.expect("loads"), bytes);
        assert!(
            matches!(
                mine.get(&cx, their_id).await,
                Err(StoreError::IdentityMismatch { .. })
            ),
            "the same bytes under another key are not this store's object"
        );
    });
}

/// A missing block is an IO failure naming the path, not a silent empty result.
#[test]
fn a_missing_block_is_an_error() {
    let dir = scratch_dir("missing");
    under_lab(37, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let absent = DeltaBlockVersion(ObjectId([0xab; 32]));
        assert!(!store.contains(&cx, absent).await);
        assert!(matches!(
            store.get(&cx, absent).await,
            Err(StoreError::Io(_))
        ));
    });
}

/// A block that is NOT a lawful block is refused as malformed once its identity
/// checks out — the decoder's laws still apply to bytes the store vouches for.
///
/// Identity says "these are the bytes you asked for"; it says nothing about whether
/// they are a block. A store that stopped at the identity check would hand a caller
/// garbage it had personally certified.
#[test]
fn stored_bytes_that_are_not_a_block_are_refused_as_malformed() {
    let dir = scratch_dir("notablock");
    under_lab(38, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        // Stored honestly — the store derives the identity, so this IS the object
        // it names. It simply is not a block.
        let id = store
            .put(&cx, b"this is not a strata block")
            .await
            .expect("stores");
        assert!(matches!(
            store.get(&cx, id).await,
            Err(StoreError::Malformed(_))
        ));
        assert!(
            store.get_bytes(&cx, id).await.is_ok(),
            "and the raw path still returns them, since identity is all it claims"
        );
    });
}

// ---------------------------------------------------------------------------
// Reopening a partition from disk
// ---------------------------------------------------------------------------

use fgdb_delta_types::DeltaRow;
use fgdb_strata::root::{
    BlockRef, EdgeBirth, EdgeIdentityConflict, MAX_ENCODED_ROOT_BYTES, PartitionRoot, RootError,
    merge_neighbours, root_id as derive_root_id,
};
use fgdb_strata::writer::BlockWriter;
use fgdb_types::{BranchId, GraphId};

/// Plant a root exactly as a damaged/restored store can present it: at the
/// root transcript's own derived path, without asking the admission writer to
/// validate its block set first.  Roots and blocks now have distinct §5.1
/// headers, so the generic block writer is deliberately not a valid substitute.
fn plant_unadmitted_root(store: &BlockStore, root_bytes: &[u8]) -> PartitionRootVersion {
    let root_id = PartitionRootVersion(derive_root_id(&K_OID, NAMESPACE, root_bytes));
    std::fs::write(store.path(root_id.0), root_bytes).expect("plants raw root object");
    root_id
}

fn create(eid: u128, src: u128, dst: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}

fn create_vertex(vid: u128) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: 900 + vid as u64,
        labels: vec![],
        props: vec![],
        valid_time: None,
    }
}

fn seed_triangle(writer: &mut BlockWriter, keys: (&[u8; 32], DatabaseSecurityNamespaceId)) {
    for vid in [1u128, 2, 3] {
        if !writer.is_vertex_live(VId(vid)) {
            writer
                .apply(keys, CommitSeq(1), &create_vertex(vid))
                .expect("seed vertex");
        }
    }
}

fn assert_identity_conflict<T>(
    result: Result<T, StoreError>,
    eid: EId,
    expected: EdgeBirth,
    found: EdgeBirth,
) {
    let actual = result.err().and_then(|error| match error {
        StoreError::MalformedRoot(RootError::EdgeIdentityMismatch {
            eid: actual,
            conflict,
        }) => Some((actual, *conflict)),
        _ => None,
    });
    assert_eq!(
        actual,
        Some((eid, EdgeIdentityConflict { expected, found })),
        "root admission must retain both incompatible births"
    );
}

/// A root is admitted only after every named block proves the range the root
/// would authenticate. Otherwise a caller with commit authority could persist
/// a keyed, content-addressed lie and a later snapshot reader could treat its
/// understated `first_seq` as permission to skip relevant history.
#[test]
fn putting_a_root_refuses_a_false_block_range_before_publication() {
    let dir = scratch_dir("root-range-admission");
    under_lab(44, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let bytes = sample();
        let block_id = store.put(&cx, &bytes).await.expect("stores block");
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(3),
            blocks: vec![BlockRef {
                block_id: block_id.0,
                first_seq: CommitSeq(2),
                last_seq: CommitSeq(2),
            }],
            vertex_patches: vec![],
        };
        let root_bytes = fgdb_strata::root::encode_root(&root).expect("encodes root");
        let root_id = derive_root_id(&K_OID, NAMESPACE, &root_bytes);

        assert!(matches!(
            store.put_root(&cx, &root).await,
            Err(StoreError::MalformedRoot(RootError::BlockRangeMismatch {
                at: 0,
                declared: (CommitSeq(2), CommitSeq(2)),
                actual: (CommitSeq(1), CommitSeq(2)),
            }))
        ));
        assert!(
            !store.path(root_id).exists(),
            "a root that failed admission must not acquire a canonical path"
        );
    });
}

/// Root admission proves cross-block EId continuity before publication or before
/// minting a token that may skip future block I/O. Otherwise an early snapshot
/// could answer from the first birth while the conflicting later birth remained
/// hidden behind a truthful future range.
#[test]
fn root_admission_refuses_eid_reuse_in_a_future_block() {
    let dir = scratch_dir("root-eid-admission");
    under_lab(50, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let early = edge(10, 1, 2, 1, Some(3));
        let future = edge(10, 1, 2, 5, None);
        let early_bytes = encode_block(0, None, &[early]).expect("early block encodes");
        let early_id = store
            .put(&cx, &early_bytes)
            .await
            .expect("stores early block");
        // The future block LINKS the early one lawfully (V6), so the refusal
        // under test stays the identity law rather than the chain law.
        let future_bytes = encode_block(
            0,
            Some(fgdb_strata::DeltaBlockVersion(early_id.0)),
            &[future],
        )
        .expect("future block encodes");
        let future_id = store
            .put(&cx, &future_bytes)
            .await
            .expect("stores future block");
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(6),
            blocks: vec![
                BlockRef {
                    block_id: early_id.0,
                    first_seq: CommitSeq(1),
                    last_seq: CommitSeq(3),
                },
                BlockRef {
                    block_id: future_id.0,
                    first_seq: CommitSeq(5),
                    last_seq: CommitSeq(5),
                },
            ],
            vertex_patches: vec![],
        };
        let expected = EdgeBirth {
            src: VId(1),
            relation: REL,
            dst: VId(2),
            created_at: CommitSeq(1),
        };
        let found = EdgeBirth {
            created_at: CommitSeq(5),
            ..expected
        };

        let root_bytes = fgdb_strata::root::encode_root(&root).expect("root encodes");
        let root_id = derive_root_id(&K_OID, NAMESPACE, &root_bytes);
        assert_identity_conflict(store.put_root(&cx, &root).await, EId(10), expected, found);
        assert!(
            !store.path(root_id).exists(),
            "failed admission must not publish the malformed root"
        );

        // Plant the same well-formed root bytes through its own raw identity path
        // to model a pre-existing or restored store. Fresh selective reopen reads
        // every block before minting its skip token, even though sequence 2 would
        // retain only the early block.
        let planted_id = plant_unadmitted_root(&store, &root_bytes);
        assert_identity_conflict(
            store.reopen_at(&cx, planted_id, CommitSeq(2)).await,
            EId(10),
            expected,
            found,
        );
    });
}

/// A structurally valid root is not publishable before every named block is
/// present. Admission and canonical naming are one ordered operation.
#[test]
fn putting_a_root_requires_every_named_block_before_publication() {
    let dir = scratch_dir("root-block-presence-admission");
    under_lab(46, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(2),
            blocks: vec![BlockRef {
                block_id: ObjectId([0x61; 32]),
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            }],
            vertex_patches: vec![],
        };
        let root_bytes = fgdb_strata::root::encode_root(&root).expect("encodes root");
        let root_id = derive_root_id(&K_OID, NAMESPACE, &root_bytes);

        assert!(matches!(
            store.put_root(&cx, &root).await,
            Err(StoreError::RootBlockLoad { at: 0, error })
                if matches!(*error, StoreError::Io(ref io)
                    if io.kind() == std::io::ErrorKind::NotFound)
        ));
        assert!(
            !store.path(root_id).exists(),
            "an incomplete root must not acquire a canonical path"
        );
    });
}

/// Root decoding has a much smaller structural ceiling than a full block. The
/// store must apply that exact ceiling before materializing bytes, not first
/// allocate up to the block-format maximum and let the root decoder complain.
#[test]
fn a_root_read_uses_the_root_formats_exact_byte_ceiling() {
    let dir = scratch_dir("root-byte-ceiling");
    under_lab(45, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let id = PartitionRootVersion(ObjectId([0x51; 32]));
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(store.path(id.0))
            .expect("creates sparse planted root");
        file.set_len(MAX_ENCODED_ROOT_BYTES as u64 + 1)
            .expect("extends sparse planted root");

        assert!(matches!(
            store.get_root(&cx, id).await,
            Err(StoreError::ObjectTooLarge { limit, observed })
                if limit == MAX_ENCODED_ROOT_BYTES as u64
                    && observed == MAX_ENCODED_ROOT_BYTES as u64 + 1
        ));
    });
}

/// **THE PAYOFF: a partition reopens from disk with no stream replay.**
///
/// The writer produces blocks and a root; all of it goes into the store; and a
/// FRESH store handle — as a restarted process would have — is given nothing but
/// the root identity and reconstructs the whole partition, answering the same
/// adjacency queries. Until this existed a partition could only be produced by
/// folding the entire commit history, which is correct and is not a storage tier.
#[test]
fn a_partition_reopens_from_disk_with_no_stream_replay() {
    let dir = scratch_dir("reopen");
    under_lab(41, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates");
        writer
            .apply(strata_keys, CommitSeq(2), &create(11, 1, 3))
            .expect("creates");
        writer.seal(strata_keys).expect("seals");
        writer
            .apply(strata_keys, CommitSeq(4), &create(12, 2, 3))
            .expect("creates");
        let (root, blocks, patches) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");
        assert!(blocks.len() >= 2, "the fixture spans more than one block");

        let root_id = {
            let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
                .await
                .expect("opens");
            for block in &blocks {
                store.put(&cx, &block.bytes).await.expect("stores a block");
            }
            for patch in &patches {
                store
                    .put_patch(&cx, &patch.bytes)
                    .await
                    .expect("stores a vertex patch");
            }
            store.put_root(&cx, &root).await.expect("stores the root")
        };
        let encoded_root = fgdb_strata::root::encode_root(&root).expect("encodes root");
        assert_eq!(
            root_id.0,
            derive_root_id(&K_OID, NAMESPACE, &encoded_root),
            "root publication and root verification share one authoritative transcript"
        );

        // A FRESH handle, holding nothing but the root identity.
        let reopened = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("reopens");
        let (loaded_root, loaded_blocks, _block_props, _patches) = reopened
            .reopen(&cx, root_id)
            .await
            .expect("reopens the partition");

        assert_eq!(loaded_root, root, "the root came back exactly");
        assert_eq!(loaded_blocks.len(), blocks.len());
        assert_eq!(
            merge_neighbours(&loaded_blocks, VId(1), REL, CommitSeq(9)).expect("merges"),
            vec![VId(2), VId(3)],
            "and the partition answers the same adjacency it did in memory"
        );
        assert_eq!(
            merge_neighbours(&loaded_blocks, VId(2), REL, CommitSeq(9)).expect("merges"),
            vec![VId(3)]
        );
    });
}

/// A fresh selective reopen proves every range but retains only blocks that can
/// affect the requested snapshot. This is the bounded-memory first-open path;
/// the returned admission token supplies the actual I/O skip on later reads.
#[test]
fn selective_reopen_retains_only_blocks_visible_at_the_snapshot() {
    let dir = scratch_dir("selective-reopen");
    under_lab(47, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates the first block");
        writer.seal(strata_keys).expect("seals the first block");
        writer
            .apply(strata_keys, CommitSeq(7), &create(11, 1, 3))
            .expect("creates the future block");
        let (root, blocks, patches) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");
        assert_eq!(blocks.len(), 2, "the fixture needs one skippable block");

        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        for block in &blocks {
            store.put(&cx, &block.bytes).await.expect("stores block");
        }
        for patch in &patches {
            store
                .put_patch(&cx, &patch.bytes)
                .await
                .expect("stores a vertex patch");
        }
        let root_id = store.put_root(&cx, &root).await.expect("stores root");

        let (admitted, visible, _patches) = store
            .reopen_at(&cx, root_id, CommitSeq(3))
            .await
            .expect("selectively reopens");
        assert_eq!(admitted.root_id(), root_id);
        assert_eq!(admitted.root(), &root);
        assert_eq!(visible.len(), 1, "the block beginning at 7 is not retained");
        assert_eq!(
            merge_neighbours(&visible, VId(1), REL, CommitSeq(3)).expect("merges"),
            vec![VId(2)]
        );
    });
}

/// Fresh selective reopen cannot use a root's lower bounds until the actual
/// blocks have proved them. Otherwise a forged high `first_seq` could hide live
/// history from every early snapshot.
#[test]
fn fresh_selective_reopen_refuses_an_unproved_skip_range() {
    let dir = scratch_dir("selective-reopen-range-admission");
    under_lab(49, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let bytes = sample();
        let block_id = store.put(&cx, &bytes).await.expect("stores block");
        let lying = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(9),
            blocks: vec![BlockRef {
                block_id: block_id.0,
                first_seq: CommitSeq(7),
                last_seq: CommitSeq(7),
            }],
            vertex_patches: vec![],
        };
        let root_bytes = fgdb_strata::root::encode_root(&lying).expect("encodes root");
        let root_id = plant_unadmitted_root(&store, &root_bytes);

        assert!(matches!(
            store.reopen_at(&cx, root_id, CommitSeq(3)).await,
            Err(StoreError::MalformedRoot(RootError::BlockRangeMismatch {
                at: 0,
                declared: (CommitSeq(7), CommitSeq(7)),
                actual: (CommitSeq(1), CommitSeq(2)),
            }))
        ));
    });
}

/// Once every range has been admitted, a later snapshot read loads exactly its
/// candidate blocks. Damage planted in a future-only block is therefore outside
/// an earlier read, but is still detected when that block becomes relevant.
#[test]
fn an_admitted_root_skips_future_block_io_on_reuse() {
    let dir = scratch_dir("admitted-root-skip");
    under_lab(48, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates the first block");
        writer.seal(strata_keys).expect("seals the first block");
        writer
            .apply(strata_keys, CommitSeq(7), &create(11, 1, 3))
            .expect("creates the future block");
        let (root, blocks, patches) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");
        assert_eq!(blocks.len(), 2, "the fixture needs one future block");

        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        for block in &blocks {
            store.put(&cx, &block.bytes).await.expect("stores block");
        }
        for patch in &patches {
            store
                .put_patch(&cx, &patch.bytes)
                .await
                .expect("stores a vertex patch");
        }
        let root_id = store.put_root(&cx, &root).await.expect("stores root");
        let admitted = store
            .admit_root(&cx, root_id)
            .await
            .expect("admits every range");
        let future_block_id = root
            .blocks
            .get(1)
            .map(|reference| reference.block_id)
            .expect("the writer published the future block reference");

        std::fs::write(store.path(future_block_id), b"damaged after admission")
            .expect("plants later damage");

        let early = admitted
            .resolve_blocks_at(&cx, CommitSeq(3))
            .await
            .expect("future-only damage is skipped");
        assert_eq!(early.len(), 1);
        assert_eq!(
            merge_neighbours(&early, VId(1), REL, CommitSeq(3)).expect("merges"),
            vec![VId(2)]
        );
        assert!(matches!(
            admitted.resolve_blocks_at(&cx, CommitSeq(7)).await,
            Err(StoreError::RootBlockLoad { at: 1, error })
                if matches!(*error, StoreError::IdentityMismatch { .. })
        ));
    });
}

/// A root naming a block the store does not hold is refused, naming the block's
/// position — a partial partition is not a partition.
#[test]
fn reopening_with_a_missing_block_is_refused() {
    let dir = scratch_dir("reopen-missing");
    under_lab(42, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates");
        writer.seal(strata_keys).expect("seals");
        writer
            .apply(strata_keys, CommitSeq(4), &create(11, 1, 3))
            .expect("creates");
        let (root, blocks, _patches) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");

        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        // Store only the FIRST block. `put_root` itself rejects this incomplete
        // publication; the root's raw identity path models a damaged/restored
        // store that still reaches read.
        store.put(&cx, &blocks[0].bytes).await.expect("stores");
        let root_bytes = fgdb_strata::root::encode_root(&root).expect("encodes root");
        let root_id = plant_unadmitted_root(&store, &root_bytes);

        assert!(
            matches!(
                store.reopen(&cx, root_id).await,
                Err(StoreError::RootBlockLoad { at: 1, error })
                    if matches!(*error, StoreError::Io(ref io)
                        if io.kind() == std::io::ErrorKind::NotFound)
            ),
            "a missing stored block must retain both its position and I/O diagnosis"
        );
        // The root itself is still perfectly readable — the failure is about the
        // partition, not about the root object, and the two are worth telling apart.
        assert_eq!(
            store
                .get_root(&cx, root_id)
                .await
                .expect("the root is fine"),
            root
        );
    });
}

// ---------------------------------------------------------------------------
// The receipted publish path (fgdb-gieu)
// ---------------------------------------------------------------------------

use fgdb_strata::store::PublishReceipts;

fn delete(eid: u128) -> DeltaRow {
    DeltaRow::DeleteEdge {
        eid: EId(eid),
        before_version: ObjectId([0; 32]),
    }
}

/// **The receipted path derives the identical publication.** One writer
/// lineage, two stores: every commit's root id must agree between the plain
/// path and the receipted path — including across a seal boundary and a
/// cross-block tombstone, the shapes that exercise the receipts' cumulative
/// edge-history state. This is the differential that makes the memoisation
/// claim falsifiable: a receipt that skipped work the slow path needed would
/// eventually publish a different root or refuse a lawful one.
#[test]
fn the_receipted_path_publishes_the_same_roots_as_the_plain_path() {
    let dir_plain = scratch_dir("receipts-differential-plain");
    let dir_receipted = scratch_dir("receipts-differential-receipted");
    under_lab(51, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let plain = BlockStore::open(&cx, &dir_plain, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let receipted = BlockStore::open(&cx, &dir_receipted, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let mut receipts = PublishReceipts::new();
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);

        let rows: [DeltaRow; 4] = [
            create(10, 1, 2),
            create(11, 1, 3),
            delete(10),
            create(12, 2, 3),
        ];
        for (b, row) in rows.iter().enumerate() {
            let seq = CommitSeq(b as u64 + 1);
            writer.apply(strata_keys, seq, row).expect("applies");
            if b == 0 {
                // Force a sealed prefix so the later tombstone restates a birth
                // ACROSS blocks, the case the cumulative validator must accept.
                writer.seal(strata_keys).expect("seals");
            }
            let (root, blocks, patches) =
                writer.clone().publish(strata_keys, seq).expect("publishes");

            for block in &blocks {
                plain.put(&cx, &block.bytes).await.expect("plain put");
            }
            for patch in &patches {
                plain
                    .put_patch(&cx, &patch.bytes)
                    .await
                    .expect("plain put_patch");
            }
            let plain_root = plain.put_root(&cx, &root).await.expect("plain put_root");

            for block in &blocks {
                receipted
                    .put_verified(&cx, &block.bytes, None, &mut receipts)
                    .await
                    .expect("put_verified");
            }
            for patch in &patches {
                receipted
                    .put_patch_verified(&cx, &patch.bytes, &mut receipts)
                    .await
                    .expect("put_patch_verified");
            }
            let receipted_root = receipted
                .put_root_verified(&cx, &root, &mut receipts)
                .await
                .expect("put_root_verified");

            assert_eq!(
                receipted_root, plain_root,
                "commit {seq:?}: the receipted path published a different root"
            );
        }
    });
}

/// **A receipt really is a filesystem skip, and it is scoped to what this
/// session proved.** Damage planted over a receipted block's file makes the
/// plain idempotent put refuse (`DamagedExisting`) — it re-reads the file —
/// while the receipted put returns the identity without touching the inode:
/// the receipt records that THIS process already made those exact bytes
/// durable, and content addressing makes re-deriving that from a disk that
/// can only agree or be damaged pure re-verification. The damage is not
/// thereby forgiven: any path that actually reads the block (`get`, reopen,
/// recovery) still refuses it by identity, exactly as the planted-damage laws
/// above pin.
#[test]
fn a_receipted_block_is_admitted_without_rereading_its_file() {
    let dir = scratch_dir("receipts-skip");
    under_lab(52, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let mut receipts = PublishReceipts::new();
        let bytes = sample();
        let id = store
            .put_verified(&cx, &bytes, None, &mut receipts)
            .await
            .expect("earns the receipt");
        assert!(receipts.holds(id));

        let other = encode_block(0, None, &[entry(9, 9, 7)]).expect("encodes");
        std::fs::write(store.path(id.0), &other).expect("plants damage");

        assert!(
            matches!(
                store.put(&cx, &bytes).await,
                Err(StoreError::DamagedExisting { .. })
            ),
            "the plain path re-reads the file and must see the damage"
        );
        assert_eq!(
            store
                .put_verified(&cx, &bytes, None, &mut receipts)
                .await
                .expect("the receipt answers without the file"),
            id,
            "a receipted identity resolves from the session's own proof"
        );
        assert!(
            matches!(
                store.get(&cx, id).await,
                Err(StoreError::IdentityMismatch { .. })
            ),
            "and every reading path still refuses the damaged bytes"
        );
    });
}

/// A root that lies about a receipted block's range gets no benefit from the
/// receipt: the span mismatch routes that reference back through full disk
/// admission, which refuses it with exactly the diagnostic the plain path
/// gives. The memoised fast path must never be the wider one.
#[test]
fn a_root_lying_about_a_receipted_range_is_still_refused() {
    let dir = scratch_dir("receipts-range-lie");
    under_lab(53, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let mut receipts = PublishReceipts::new();
        let bytes = sample();
        let block_id = store
            .put_verified(&cx, &bytes, None, &mut receipts)
            .await
            .expect("stores block");
        let lying = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(3),
            blocks: vec![BlockRef {
                block_id: block_id.0,
                first_seq: CommitSeq(2),
                last_seq: CommitSeq(2),
            }],
            vertex_patches: vec![],
        };
        let root_bytes = fgdb_strata::root::encode_root(&lying).expect("encodes root");
        let root_id = derive_root_id(&K_OID, NAMESPACE, &root_bytes);

        assert!(matches!(
            store.put_root_verified(&cx, &lying, &mut receipts).await,
            Err(StoreError::MalformedRoot(RootError::BlockRangeMismatch {
                at: 0,
                declared: (CommitSeq(2), CommitSeq(2)),
                actual: (CommitSeq(1), CommitSeq(2)),
            }))
        ));
        assert!(
            !store.path(root_id).exists(),
            "a refused root must not acquire a canonical path"
        );
    });
}

/// Fresh receipts degrade to one full admission, not to weaker checking: a
/// session that inherits a store populated by someone else re-reads every
/// referenced block from disk on its first publication, earns the receipts,
/// and only then skips.
#[test]
fn fresh_receipts_fall_back_to_full_disk_admission() {
    let dir = scratch_dir("receipts-fallback");
    under_lab(54, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates");
        writer.seal(strata_keys).expect("seals");
        writer
            .apply(strata_keys, CommitSeq(4), &create(11, 1, 3))
            .expect("creates");
        let (root, blocks, patches) = writer
            .publish(strata_keys, CommitSeq(4))
            .expect("publishes");
        assert_eq!(blocks.len(), 2, "the fixture needs a sealed prefix");

        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        for block in &blocks {
            store.put(&cx, &block.bytes).await.expect("plain put");
        }
        for patch in &patches {
            store
                .put_patch(&cx, &patch.bytes)
                .await
                .expect("plain put_patch");
        }
        let expected = store.put_root(&cx, &root).await.expect("plain put_root");

        // A NEW session over the same store: no receipts, so admission must
        // come from disk — and must succeed, seeding every receipt.
        let mut receipts = PublishReceipts::new();
        let republished = store
            .put_root_verified(&cx, &root, &mut receipts)
            .await
            .expect("full-admission fallback");
        assert_eq!(republished, expected);
        for block in &blocks {
            assert!(
                receipts.holds(DeltaBlockVersion(block.block_id)),
                "the fallback earns the receipt it verified"
            );
        }
    });
}

/// The cross-block EId law survives memoisation — and fires EARLIER. The
/// receipted path validates each block's entries as the receipt is earned, so
/// a future block reusing a spent EId is refused at `put_verified` time,
/// before any root could even name it. The plain path refuses the same defect
/// at `put_root`; both refuse, neither publishes.
#[test]
fn receipted_puts_refuse_eid_reuse_as_the_receipt_is_earned() {
    let dir = scratch_dir("receipts-eid-reuse");
    under_lab(55, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let mut receipts = PublishReceipts::new();
        let early = edge(10, 1, 2, 1, Some(3));
        let future = edge(10, 1, 2, 5, None);
        let early_bytes = encode_block(0, None, &[early]).expect("encodes");
        let future_bytes = encode_block(0, None, &[future]).expect("encodes");

        store
            .put_verified(&cx, &early_bytes, None, &mut receipts)
            .await
            .expect("the first birth is lawful");
        assert!(matches!(
            store
                .put_verified(&cx, &future_bytes, None, &mut receipts)
                .await,
            Err(StoreError::MalformedRoot(RootError::EdgeIdentityMismatch {
                eid: EId(10),
                ..
            }))
        ));
    });
}

/// A root is subject to the same identity rule as a block: bytes at its path that
/// are a DIFFERENT root are refused.
#[test]
fn a_root_at_the_wrong_identity_is_refused() {
    let dir = scratch_dir("reopen-wrongroot");
    under_lab(43, move |cx| async move {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        seed_triangle(&mut writer, strata_keys);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates");
        let (root, blocks, patches) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");

        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        for block in &blocks {
            store.put(&cx, &block.bytes).await.expect("stores block");
        }
        for patch in &patches {
            store
                .put_patch(&cx, &patch.bytes)
                .await
                .expect("stores a vertex patch");
        }
        let root_id = store.put_root(&cx, &root).await.expect("stores");

        // A different lawful root written over the path.
        let other = fgdb_strata::root::PartitionRoot {
            published_at: CommitSeq(11),
            ..root
        };
        let other_bytes = fgdb_strata::root::encode_root(&other).expect("encodes");
        std::fs::write(store.path(root_id.0), &other_bytes).expect("plant");

        assert!(matches!(
            store.get_root(&cx, root_id).await,
            Err(StoreError::MalformedRoot(
                RootError::IdentityMismatch { .. }
            ))
        ));
    });
}

// ---------------------------------------------------------------------------
// Block-hosted edge property patches (fgdb-yqor increment 2)
// ---------------------------------------------------------------------------

/// The writer seals an FGSP patch beside any block whose entries carry
/// properties, the store admits the pair through the root, and a reopen
/// re-proves the joint bijection — while a root over a propertied block whose
/// patch was never stored FAILS CLOSED at the unreachable patch.
#[test]
fn a_propertied_block_admits_only_with_its_patch_reachable() {
    let dir = scratch_dir("propertied-admission");
    under_lab(0x9a_01, move |cx| async move {
        use fgdb_delta_types::{DeltaRow, PropertyKeyId};
        use fgdb_strata::writer::BlockWriter;
        use fgdb_types::CanonicalScalar;
        use fgdb_types::ids::{BranchId, GraphId};

        let keys = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        for vid in [1u128, 2, 3] {
            writer
                .apply(
                    keys,
                    CommitSeq(1),
                    &DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: vid as u64,
                        labels: vec![],
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("vertex folds");
        }
        writer
            .apply(
                keys,
                CommitSeq(2),
                &DeltaRow::CreateEdge {
                    eid: EId(10),
                    birth_ordinal: 10,
                    src: VId(1),
                    relation: REL,
                    dst: VId(2),
                    canonical_key: None,
                    props: vec![(PropertyKeyId(7), CanonicalScalar::Int(1815))],
                    valid_time: None,
                },
            )
            .expect("propertied edge folds");
        writer
            .apply(
                keys,
                CommitSeq(2),
                &DeltaRow::CreateEdge {
                    eid: EId(11),
                    birth_ordinal: 11,
                    src: VId(1),
                    relation: REL,
                    dst: VId(3),
                    canonical_key: None,
                    props: vec![],
                    valid_time: None,
                },
            )
            .expect("bare edge folds");
        let (root, blocks, patches) = writer.publish(keys, CommitSeq(2)).expect("publishes");

        let propertied = blocks
            .iter()
            .find(|block| block.property_patch.is_some())
            .expect("the propertied entry forced a hosted patch");
        let hosted = propertied.property_patch.as_ref().expect("present");
        let rows = fgdb_strata::edge_props::decode_property_patch(&hosted.bytes)
            .expect("the sealed patch decodes");
        assert_eq!(
            rows,
            vec![vec![(PropertyKeyId(7), CanonicalScalar::Int(1815))]],
            "the patch carries exactly the propertied entry's list"
        );

        // WITHOUT the patch stored, the root is refused at the unreachable
        // patch — reachability is root -> block -> patch.
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("store opens");
        for block in &blocks {
            store
                .put(&cx, &block.bytes)
                .await
                .expect("stores the block");
        }
        for patch in &patches {
            store
                .put_patch(&cx, &patch.bytes)
                .await
                .expect("stores vertex patch");
        }
        assert!(
            matches!(
                store.put_root(&cx, &root).await,
                Err(StoreError::BlockPatchLoad { .. })
            ),
            "an unreachable hosted patch must refuse the root"
        );

        // With the patch durable, admission and a fresh reopen both hold.
        store
            .put_edge_property_patch(&cx, &hosted.bytes)
            .await
            .expect("stores the hosted patch");
        let root_id = store.put_root(&cx, &root).await.expect("root admits");
        let (_, reopened_blocks, _block_props, _) =
            store.reopen(&cx, root_id).await.expect("reopens");
        let total: usize = reopened_blocks.iter().map(Vec::len).sum();
        assert_eq!(total, 2, "both edges survive the round trip");
    });
}

/// A tombstone RESTATES the properties: after a delete, the superseding
/// statement's block hosts a patch carrying the same list, so pre-retirement
/// snapshots keep answering them.
#[test]
fn a_tombstone_restates_the_properties_in_its_own_patch() {
    let dir = scratch_dir("tombstone-props");
    under_lab(0x9a_02, move |cx| async move {
        use fgdb_delta_types::{DeltaRow, PropertyKeyId};
        use fgdb_strata::writer::BlockWriter;
        use fgdb_types::CanonicalScalar;
        use fgdb_types::ids::{BranchId, GraphId};

        let keys = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        for vid in [1u128, 2] {
            writer
                .apply(
                    keys,
                    CommitSeq(1),
                    &DeltaRow::CreateVertex {
                        vid: VId(vid),
                        birth_ordinal: vid as u64,
                        labels: vec![],
                        props: vec![],
                        valid_time: None,
                    },
                )
                .expect("vertex folds");
        }
        writer
            .apply(
                keys,
                CommitSeq(2),
                &DeltaRow::CreateEdge {
                    eid: EId(10),
                    birth_ordinal: 10,
                    src: VId(1),
                    relation: REL,
                    dst: VId(2),
                    canonical_key: None,
                    props: vec![(PropertyKeyId(7), CanonicalScalar::Int(1))],
                    valid_time: None,
                },
            )
            .expect("creates");
        writer.seal(keys).expect("seals the creation");
        writer
            .apply(
                keys,
                CommitSeq(5),
                &DeltaRow::DeleteEdge {
                    eid: EId(10),
                    before_version: ObjectId([0u8; 32]),
                },
            )
            .expect("retires");
        let (root, blocks, patches) = writer.publish(keys, CommitSeq(5)).expect("publishes");

        let tombstone_block = blocks
            .iter()
            .find(|block| {
                fgdb_strata::decode_block(&block.bytes)
                    .expect("decodes")
                    .iter()
                    .any(|entry| entry.retired_at == Some(CommitSeq(5)))
            })
            .expect("the tombstone sealed into its own block");
        let hosted = tombstone_block
            .property_patch
            .as_ref()
            .expect("the tombstone RESTATES the properties");
        assert_eq!(
            fgdb_strata::edge_props::decode_property_patch(&hosted.bytes).expect("decodes"),
            vec![vec![(PropertyKeyId(7), CanonicalScalar::Int(1))]],
        );

        // Full round trip: store everything, admit, reopen.
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("store opens");
        for block in &blocks {
            if let Some(patch) = &block.property_patch {
                store
                    .put_edge_property_patch(&cx, &patch.bytes)
                    .await
                    .expect("stores hosted patch");
            }
            store.put(&cx, &block.bytes).await.expect("stores block");
        }
        for patch in &patches {
            store
                .put_patch(&cx, &patch.bytes)
                .await
                .expect("stores vertex patch");
        }
        let root_id = store.put_root(&cx, &root).await.expect("root admits");
        store
            .reopen(&cx, root_id)
            .await
            .expect("the history reopens whole");
    });
}

/// **THE TRANSPLANT LAW (V5, fgdb-da6b): a root refuses a block whose durable
/// `partition_id` is another partition's.** Without this, moving one
/// content-addressed file between partition directories would silently merge
/// a foreign partition's history — well-formed, correctly addressed, and
/// wrong.
#[test]
fn a_root_refuses_a_block_transplanted_from_another_partition() {
    let dir = scratch_dir("partition-transplant");
    under_lab(52, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let foreign = encode_block(9, None, &[entry(1, 2, 1), entry(1, 3, 2)]).expect("encodes");
        let block_id = store.put(&cx, &foreign).await.expect("stores block");
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(3),
            blocks: vec![BlockRef {
                block_id: block_id.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(2),
            }],
            vertex_patches: vec![],
        };
        assert!(matches!(
            store.put_root(&cx, &root).await,
            Err(StoreError::MalformedRoot(
                RootError::BlockPartitionMismatch {
                    at: 0,
                    root_partition: 0,
                    block_partition: 9,
                }
            )),
        ));

        // CONTROL: the same content at the root's own partition admits — the
        // refusal above is the partition law firing, not fixture debris.
        let home = encode_block(0, None, &[entry(1, 2, 1), entry(1, 3, 2)]).expect("encodes");
        let home_id = store.put(&cx, &home).await.expect("stores block");
        let root = PartitionRoot {
            blocks: vec![BlockRef {
                block_id: home_id.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(2),
            }],
            ..root
        };
        store
            .put_root(&cx, &root)
            .await
            .expect("the home block admits");
    });
}

/// **THE JOINT DIGEST LAW (V5): a propertied block whose declared digest
/// disowns its own rows is refused at admission**, where block and patch are
/// first in hand together — the same seam as the row-count bijection. The
/// splice targets the digest field itself: bytes the encoder cannot emit.
#[test]
fn admission_refuses_a_propertied_block_with_a_forged_digest() {
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_strata::edge_props::{encode_property_patch, property_patch_id};
    use fgdb_types::CanonicalScalar;

    let dir = scratch_dir("forged-joint-digest");
    under_lab(53, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let rows = vec![vec![(PropertyKeyId(7), CanonicalScalar::Int(1815))]];
        let patch_bytes = encode_property_patch(&rows).expect("encodes");
        let patch_id = property_patch_id(&K_OID, NAMESPACE, &patch_bytes);
        let entries = vec![entry(1, 2, 1), entry(1, 3, 2)];
        let mut bytes =
            fgdb_strata::encode_block_with_properties(0, None, &entries, patch_id, &[1, 0], &rows)
                .expect("encodes");
        bytes[49] ^= 0x01; // forge the digest; content addressing follows the bytes
        store
            .put_edge_property_patch(&cx, &patch_bytes)
            .await
            .expect("patch stores");
        let block_id = store.put(&cx, &bytes).await.expect("block stores");
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(3),
            blocks: vec![BlockRef {
                block_id: block_id.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(2),
            }],
            vertex_patches: vec![],
        };
        assert!(matches!(
            store.put_root(&cx, &root).await,
            Err(StoreError::Malformed(
                fgdb_strata::BlockError::LogicalDigestMismatch { .. }
            )),
        ));
    });
}

/// **THE CHAIN LAW AT ADMISSION (V6, fgdb-4391): a family's blocks must link
/// in exactly the root's publication order.** A missing link and a forged
/// link are both refused; the lawful chain is the control. The chain IS
/// publication order restricted to one family, so FG-INV-03's finite /
/// acyclic / newer-first properties hold by construction once this law does.
#[test]
fn a_root_refuses_a_broken_or_forged_family_chain() {
    use fgdb_strata::DeltaBlockVersion;

    let dir = scratch_dir("family-chain");
    under_lab(54, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let first_bytes = encode_block(0, None, &[edge(10, 1, 2, 1, None)]).expect("encodes");
        let first_id = store.put(&cx, &first_bytes).await.expect("stores");
        let reference = |id: fgdb_strata::DeltaBlockVersion, first: u64, last: u64| BlockRef {
            block_id: id.0,
            first_seq: CommitSeq(first),
            last_seq: CommitSeq(last),
        };
        let root_of = |blocks: Vec<BlockRef>| PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(6),
            blocks,
            vertex_patches: vec![],
        };

        // A second family block that LINKS NOTHING: the chain skips.
        let unlinked = encode_block(0, None, &[edge(11, 1, 3, 5, None)]).expect("encodes");
        let unlinked_id = store.put(&cx, &unlinked).await.expect("stores");
        assert!(matches!(
            store
                .put_root(
                    &cx,
                    &root_of(vec![
                        reference(first_id, 1, 1),
                        reference(unlinked_id, 5, 5),
                    ])
                )
                .await,
            Err(StoreError::MalformedRoot(RootError::BlockChainMismatch {
                at: 1,
                declared: None,
                expected: Some(_),
            })),
        ));

        // A second family block that links a FOREIGN identity.
        let forged = encode_block(
            0,
            Some(DeltaBlockVersion(ObjectId([0xee; 32]))),
            &[edge(11, 1, 3, 5, None)],
        )
        .expect("encodes");
        let forged_id = store.put(&cx, &forged).await.expect("stores");
        assert!(matches!(
            store
                .put_root(
                    &cx,
                    &root_of(vec![reference(first_id, 1, 1), reference(forged_id, 5, 5),])
                )
                .await,
            Err(StoreError::MalformedRoot(RootError::BlockChainMismatch {
                at: 1,
                ..
            })),
        ));

        // CONTROL: the lawful link admits.
        let chained = encode_block(0, Some(first_id), &[edge(11, 1, 3, 5, None)]).expect("encodes");
        let chained_id = store.put(&cx, &chained).await.expect("stores");
        store
            .put_root(
                &cx,
                &root_of(vec![reference(first_id, 1, 1), reference(chained_id, 5, 5)]),
            )
            .await
            .expect("the lawful chain admits");

        // And a DIFFERENT family in between does not break the chain: the
        // law is family-scoped, not adjacency-scoped.
        let other_family = encode_block(0, None, &[edge(20, 2, 4, 3, None)]).expect("encodes");
        let other_id = store.put(&cx, &other_family).await.expect("stores");
        store
            .put_root(
                &cx,
                &root_of(vec![
                    reference(first_id, 1, 1),
                    reference(other_id, 3, 3),
                    reference(chained_id, 5, 5),
                ]),
            )
            .await
            .expect("an interleaved foreign family leaves the chain lawful");
    });
}

/// **THE EDGE CHAIN LAWS AT ADMISSION (fgdb-ls5b): a content-version
/// successor must begin exactly where its predecessor retired and carry the
/// same topology.** An overlap is aliasing, a topology change is a different
/// edge wearing a spent EId — both refuse; the lawful chain is the control,
/// and history answers each version in its own interval.
#[test]
fn edge_content_chains_admit_contiguously_and_refuse_aliasing_and_drift() {
    let dir = scratch_dir("edge-chains");
    under_lab(57, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        let reference = |id: fgdb_strata::DeltaBlockVersion, first: u64, last: u64| BlockRef {
            block_id: id.0,
            first_seq: CommitSeq(first),
            last_seq: CommitSeq(last),
        };
        let root_of = |blocks: Vec<BlockRef>| PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(9),
            blocks,
            vertex_patches: vec![],
        };

        // The predecessor statement: eid 10, alive [1, 5).
        let first_bytes = encode_block(0, None, &[edge(10, 1, 2, 1, Some(5))]).expect("encodes");
        let first_id = store.put(&cx, &first_bytes).await.expect("stores");

        // ALIASING: a successor that begins inside the predecessor's life.
        // Its span reaches PAST the predecessor's frontier so the root-order
        // law stays satisfied and the refusal under test is the chain law.
        let overlap =
            encode_block(0, Some(first_id), &[edge(10, 1, 2, 3, Some(9))]).expect("frame-lawful");
        let overlap_id = store.put(&cx, &overlap).await.expect("stores");
        assert!(matches!(
            store
                .put_root(
                    &cx,
                    &root_of(vec![reference(first_id, 1, 5), reference(overlap_id, 3, 9),])
                )
                .await,
            Err(StoreError::MalformedRoot(RootError::EdgeIdentityMismatch {
                eid: EId(10),
                ..
            })),
        ));

        // TOPOLOGY DRIFT: contiguous, but the successor moved the edge.
        let drift =
            encode_block(0, Some(first_id), &[edge(10, 1, 3, 5, None)]).expect("frame-lawful");
        let drift_id = store.put(&cx, &drift).await.expect("stores");
        assert!(matches!(
            store
                .put_root(
                    &cx,
                    &root_of(vec![reference(first_id, 1, 5), reference(drift_id, 5, 5),])
                )
                .await,
            Err(StoreError::MalformedRoot(RootError::EdgeIdentityMismatch {
                eid: EId(10),
                ..
            })),
        ));

        // CONTROL: the contiguous same-topology successor admits, and each
        // version answers in its own interval.
        let lawful = encode_block(0, Some(first_id), &[edge(10, 1, 2, 5, None)]).expect("encodes");
        let lawful_id = store.put(&cx, &lawful).await.expect("stores");
        store
            .put_root(
                &cx,
                &root_of(vec![reference(first_id, 1, 5), reference(lawful_id, 5, 5)]),
            )
            .await
            .expect("the lawful chain admits");
        let history = vec![
            vec![edge(10, 1, 2, 1, Some(5))],
            vec![edge(10, 1, 2, 5, None)],
        ];
        for (as_of, expect_created) in [(1u64, 1u64), (4, 1), (5, 5), (9, 5)] {
            let answered =
                merge_neighbours(&history, VId(1), REL, CommitSeq(as_of)).expect("merges");
            assert_eq!(answered, vec![VId(2)], "one edge, every content version");
            let statement = fgdb_strata::root::merge_edge(&history, EId(10), CommitSeq(as_of))
                .expect("merges")
                .expect("visible at every probed sequence");
            assert_eq!(
                statement.created_at,
                CommitSeq(expect_created),
                "as_of {as_of} answers its own content version"
            );
        }
    });
}

/// fgdb-a7sz regression: roots admit against THEIR OWN format ceiling
/// (MAX_ENCODED_ROOT_BYTES), not the block-derived object bound. A root whose
/// reference enumeration lawfully outgrew 16 KiB -- here 400 refs x 48 B --
/// used to be refused at publication AFTER its commit marker was durable, and
/// rebuild re-offered the same root on every later open: the directory
/// admitted no recovery. The store now applies each family's own ceiling.
#[test]
fn a_root_lawful_under_its_own_format_ceiling_is_admitted() {
    let dir = scratch_dir("root-format-ceiling-admission");
    under_lab(7, move |cx| async move {
        let store = BlockStore::open(&cx, &dir, K_OID, NAMESPACE)
            .await
            .expect("opens");
        const REFS: usize = 400;
        let mut refs = Vec::with_capacity(REFS);
        for k in 0..REFS {
            let bytes = encode_block(0, None, &[entry(1_000 + k as u128, 500_000 + k as u128, 1)])
                .expect("encodes");
            let stored = store.put(&cx, &bytes).await.expect("stores block");
            refs.push(fgdb_strata::root::BlockRef {
                block_id: stored.0,
                first_seq: CommitSeq(1),
                last_seq: CommitSeq(1),
            });
        }
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(2),
            blocks: refs,
            vertex_patches: vec![],
        };
        let encoded = fgdb_strata::root::encode_root(&root).expect("encodes root");
        assert!(
            encoded.len() > 16_384,
            "fixture regressed: {} bytes no longer exercises the seam",
            encoded.len()
        );
        assert!(encoded.len() <= fgdb_strata::root::MAX_ENCODED_ROOT_BYTES);

        let root_id = store
            .put_root(&cx, &root)
            .await
            .expect("a format-lawful root is admitted");
        let read_back = store.get_root(&cx, root_id).await.expect("reads back");
        assert_eq!(read_back.blocks.len(), REFS);
    });
}
