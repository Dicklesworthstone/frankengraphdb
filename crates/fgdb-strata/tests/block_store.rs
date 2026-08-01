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
use fgdb_strata::{AdjacencyEntry, block_id, encode_block};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CommitSeq, VId};
use std::fs::OpenOptions;
use std::path::PathBuf;

const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const REL: RelationId = RelationId(1);

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fgdb-blockstore-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
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
        report.invariant_violations.is_empty(),
        "lab invariant violation: {report:?}"
    );
    output
}

fn entry(src: u128, dst: u128, created: u64) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        created_at: CommitSeq(created),
        retired_at: None,
    }
}

fn sample() -> Vec<u8> {
    encode_block(&[entry(1, 2, 1), entry(1, 3, 2)]).expect("encodes")
}

/// A stored block reads back as the same entries, under the identity the store
/// derived.
#[test]
fn a_stored_block_reads_back() {
    let dir = scratch_dir("roundtrip");
    under_lab(31, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let id = store.put(cx, &bytes).expect("stores");

        assert_eq!(
            id,
            block_id(&K_OID, NAMESPACE, &bytes),
            "derived, not accepted"
        );
        assert!(store.contains(cx, id));
        assert_eq!(
            store.get(cx, id).expect("loads"),
            fgdb_strata::decode_block(&bytes).expect("decodes")
        );
        assert_eq!(store.get_bytes(cx, id).expect("loads bytes"), bytes);
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
    under_lab(39, move |cx| {
        let crashed = BlockStore::open_with_crash(
            cx,
            &working_dir,
            K_OID,
            NAMESPACE,
            Some(BlockStoreCrashPoint::AfterStoreDirectorySyncBeforeDatabaseDirectorySync),
        );
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

        let lost =
            BlockStore::open(cx, &crash_image, K_OID, NAMESPACE).expect("reopen loss-arm database");
        assert!(!lost.contains(cx, ObjectId([0x23; 32])));

        // The other legal outcome is that the unsynced name survived. Reopen
        // re-establishes both directory barriers before any write is accepted.
        let survived = BlockStore::open(cx, &working_dir, K_OID, NAMESPACE)
            .expect("reopen survival-arm database");
        let bytes = sample();
        let id = survived.put(cx, &bytes).expect("store after reopen");
        assert_eq!(survived.get_bytes(cx, id).expect("read"), bytes);
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
    under_lab(40, move |cx| {
        let bytes = sample();
        let id = block_id(&K_OID, NAMESPACE, &bytes);
        let store = BlockStore::open(cx, &working_dir, K_OID, NAMESPACE).expect("opens");
        let crashed = store.put_with_crash(
            cx,
            &bytes,
            Some(BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync),
        );
        assert!(
            crashed.is_err(),
            "put must not acknowledge before directory sync"
        );
        assert!(
            store.path(id).is_file(),
            "the durable inode exists in the working view"
        );
        drop(store);

        let lost =
            BlockStore::open(cx, &crash_image, K_OID, NAMESPACE).expect("reopen loss-arm database");
        assert!(
            !lost.contains(cx, id),
            "the legal loss arm cannot resolve an unsynced block name"
        );

        let survived = BlockStore::open(cx, &working_dir, K_OID, NAMESPACE)
            .expect("reopen survival-arm database");
        assert_eq!(
            survived.get_bytes(cx, id).expect("surviving dirent reads"),
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
    under_lab(43, move |cx| {
        let bytes = sample();
        let id = block_id(&K_OID, NAMESPACE, &bytes);
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");

        let interrupted = store.put_with_crash(
            cx,
            &bytes,
            Some(BlockStoreCrashPoint::AfterStagingFileSyncBeforePublication),
        );
        assert!(
            interrupted.is_err(),
            "staging completion is not canonical publication"
        );
        assert!(
            !store.path(id).exists(),
            "no partial or empty canonical path may be visible"
        );

        assert_eq!(
            store.put(cx, &bytes).expect("retry publishes"),
            id,
            "the next permit owner reuses the noncanonical staging slot"
        );
        assert_eq!(store.get_bytes(cx, id).expect("published bytes"), bytes);
    });
}

/// Equal existing bytes are still namespace-uncertain after a reopen. The
/// idempotent path must re-enter the durability barrier instead of returning
/// merely because the bytes happen to be visible in the current kernel view.
#[test]
fn an_equal_existing_block_reestablishes_durability_after_reopen() {
    let dir = scratch_dir("idempotent-reopen-barrier");
    under_lab(44, move |cx| {
        let bytes = sample();
        let id = {
            let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
            store.put(cx, &bytes).expect("initial store")
        };
        let modified_at = std::fs::metadata(
            BlockStore::open(cx, &dir, K_OID, NAMESPACE)
                .expect("reopens")
                .path(id),
        )
        .and_then(|metadata| metadata.modified())
        .expect("mtime");

        let reopened = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("reopens again");
        let interrupted = reopened.put_with_crash(
            cx,
            &bytes,
            Some(BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync),
        );
        assert!(
            interrupted.is_err(),
            "equal existing bytes must not bypass the durability barrier"
        );
        assert_eq!(
            std::fs::metadata(reopened.path(id))
                .and_then(|metadata| metadata.modified())
                .expect("mtime after re-put"),
            modified_at,
            "re-establishing durability must not rewrite immutable bytes"
        );
        assert_eq!(reopened.get_bytes(cx, id).expect("read"), bytes);
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
    under_lab(32, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let mine = sample();
        let id = store.put(cx, &mine).expect("stores");

        // A different, perfectly lawful block written over the path.
        let other = encode_block(&[entry(9, 9, 7)]).expect("encodes");
        assert_ne!(other, mine);
        std::fs::write(store.path(id), &other).expect("overwrite");

        let actual = block_id(&K_OID, NAMESPACE, &other);
        assert!(
            matches!(
                store.get(cx, id),
                Err(StoreError::IdentityMismatch { expected, actual: got })
                    if expected == id && got == actual
            ),
            "a store that trusts its layout returns the wrong partition silently"
        );
        // And the raw-bytes path enforces it too — a caller that skips decoding
        // must not thereby skip the identity check.
        assert!(matches!(
            store.get_bytes(cx, id),
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
    under_lab(33, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let id = store.put(cx, &sample()).expect("stores");

        let mut bytes = std::fs::read(store.path(id)).expect("read");
        let at = bytes.len() / 2;
        bytes[at] ^= 0x40;
        std::fs::write(store.path(id), &bytes).expect("write");

        assert!(matches!(
            store.get(cx, id),
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
    under_lab(34, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let first = store.put(cx, &bytes).expect("stores");
        let modified_at = std::fs::metadata(store.path(first))
            .and_then(|m| m.modified())
            .expect("mtime");

        let second = store.put(cx, &bytes).expect("stores again");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(store.path(first))
                .and_then(|m| m.modified())
                .expect("mtime"),
            modified_at,
            "the file must not have been rewritten"
        );
        assert_eq!(store.get_bytes(cx, first).expect("loads"), bytes);
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
    under_lab(35, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let id = store.put(cx, &bytes).expect("stores");

        // Something else has taken this identity's path with different bytes.
        let other = encode_block(&[entry(4, 5, 6)]).expect("encodes");
        std::fs::write(store.path(id), &other).expect("plant");

        let error = store.put(cx, &bytes).expect_err("damage must be refused");
        assert!(
            matches!(
                error,
                StoreError::DamagedExisting { expected, actual }
                    if expected == id && actual == block_id(&K_OID, NAMESPACE, &other)
            ),
            "bytes whose own identity differs from their path are damage, not a hash collision: \
             {error:?}"
        );
        assert_eq!(
            std::fs::read(store.path(id)).expect("read"),
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
    under_lab(36, move |cx| {
        let mine = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let theirs = BlockStore::open(cx, &dir, [0x11; 32], NAMESPACE).expect("opens");
        let bytes = sample();

        let my_id = mine.put(cx, &bytes).expect("stores");
        let their_id = theirs.put(cx, &bytes).expect("stores");
        assert_ne!(my_id, their_id, "different keys, different objects");
        assert!(
            !mine.contains(cx, their_id),
            "one key's store must not claim to hold another key's block, even \
             though the file is right there in the shared directory"
        );
        assert!(theirs.contains(cx, their_id), "while its own store does");

        // Each store resolves its OWN identity and refuses the other's.
        assert_eq!(mine.get_bytes(cx, my_id).expect("loads"), bytes);
        assert!(
            matches!(
                mine.get(cx, their_id),
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
    under_lab(37, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let absent = ObjectId([0xab; 32]);
        assert!(!store.contains(cx, absent));
        assert!(matches!(store.get(cx, absent), Err(StoreError::Io(_))));
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
    under_lab(38, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        // Stored honestly — the store derives the identity, so this IS the object
        // it names. It simply is not a block.
        let id = store
            .put(cx, b"this is not a strata block")
            .expect("stores");
        assert!(matches!(store.get(cx, id), Err(StoreError::Malformed(_))));
        assert!(
            store.get_bytes(cx, id).is_ok(),
            "and the raw path still returns them, since identity is all it claims"
        );
    });
}

// ---------------------------------------------------------------------------
// Reopening a partition from disk
// ---------------------------------------------------------------------------

use fgdb_delta_types::DeltaRow;
use fgdb_strata::root::{
    BlockRef, MAX_ENCODED_ROOT_BYTES, PartitionRoot, RootError, merge_neighbours,
    root_id as derive_root_id,
};
use fgdb_strata::writer::BlockWriter;
use fgdb_types::{BranchId, EId, GraphId};

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

/// A root is admitted only after every named block proves the range the root
/// would authenticate. Otherwise a caller with commit authority could persist
/// a keyed, content-addressed lie and a later snapshot reader could treat its
/// understated `first_seq` as permission to skip relevant history.
#[test]
fn putting_a_root_refuses_a_false_block_range_before_publication() {
    let dir = scratch_dir("root-range-admission");
    under_lab(44, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let block_id = store.put(cx, &bytes).expect("stores block");
        let root = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(3),
            blocks: vec![BlockRef {
                block_id,
                first_seq: CommitSeq(2),
                last_seq: CommitSeq(2),
            }],
        };
        let root_bytes = fgdb_strata::root::encode_root(&root).expect("encodes root");
        let root_id = derive_root_id(&K_OID, NAMESPACE, &root_bytes);

        assert!(matches!(
            store.put_root(cx, &root),
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

/// A structurally valid root is not publishable before every named block is
/// present. Admission and canonical naming are one ordered operation.
#[test]
fn putting_a_root_requires_every_named_block_before_publication() {
    let dir = scratch_dir("root-block-presence-admission");
    under_lab(46, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
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
        };
        let root_bytes = fgdb_strata::root::encode_root(&root).expect("encodes root");
        let root_id = derive_root_id(&K_OID, NAMESPACE, &root_bytes);

        assert!(matches!(
            store.put_root(cx, &root),
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
    under_lab(45, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let id = ObjectId([0x51; 32]);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(store.path(id))
            .expect("creates sparse planted root");
        file.set_len(MAX_ENCODED_ROOT_BYTES as u64 + 1)
            .expect("extends sparse planted root");

        assert!(matches!(
            store.get_root(cx, id),
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
    under_lab(41, move |cx| {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
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
        let (root, blocks) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");
        assert!(blocks.len() >= 2, "the fixture spans more than one block");

        let root_id = {
            let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
            for block in &blocks {
                store.put(cx, &block.bytes).expect("stores a block");
            }
            store.put_root(cx, &root).expect("stores the root")
        };
        let encoded_root = fgdb_strata::root::encode_root(&root).expect("encodes root");
        assert_eq!(
            root_id,
            derive_root_id(&K_OID, NAMESPACE, &encoded_root),
            "root publication and root verification share one authoritative transcript"
        );

        // A FRESH handle, holding nothing but the root identity.
        let reopened = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("reopens");
        let (loaded_root, loaded_blocks) =
            reopened.reopen(cx, root_id).expect("reopens the partition");

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
    under_lab(47, move |cx| {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates the first block");
        writer.seal(strata_keys).expect("seals the first block");
        writer
            .apply(strata_keys, CommitSeq(7), &create(11, 1, 3))
            .expect("creates the future block");
        let (root, blocks) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");
        assert_eq!(blocks.len(), 2, "the fixture needs one skippable block");

        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        for block in &blocks {
            store.put(cx, &block.bytes).expect("stores block");
        }
        let root_id = store.put_root(cx, &root).expect("stores root");

        let (admitted, visible) = store
            .reopen_at(cx, root_id, CommitSeq(3))
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
    under_lab(49, move |cx| {
        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let block_id = store.put(cx, &bytes).expect("stores block");
        let lying = PartitionRoot {
            graph: GraphId(1),
            branch: BranchId(1),
            partition: 0,
            published_at: CommitSeq(9),
            blocks: vec![BlockRef {
                block_id,
                first_seq: CommitSeq(7),
                last_seq: CommitSeq(7),
            }],
        };
        let root_bytes = fgdb_strata::root::encode_root(&lying).expect("encodes root");
        let root_id = store
            .put(cx, &root_bytes)
            .expect("plants an unadmitted root");

        assert!(matches!(
            store.reopen_at(cx, root_id, CommitSeq(3)),
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
    under_lab(48, move |cx| {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates the first block");
        writer.seal(strata_keys).expect("seals the first block");
        writer
            .apply(strata_keys, CommitSeq(7), &create(11, 1, 3))
            .expect("creates the future block");
        let (root, blocks) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");
        assert_eq!(blocks.len(), 2, "the fixture needs one future block");

        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        for block in &blocks {
            store.put(cx, &block.bytes).expect("stores block");
        }
        let root_id = store.put_root(cx, &root).expect("stores root");
        let admitted = store.admit_root(cx, root_id).expect("admits every range");
        let future_block_id = root
            .blocks
            .get(1)
            .map(|reference| reference.block_id)
            .expect("the writer published the future block reference");

        std::fs::write(store.path(future_block_id), b"damaged after admission")
            .expect("plants later damage");

        let early = admitted
            .resolve_blocks_at(cx, CommitSeq(3))
            .expect("future-only damage is skipped");
        assert_eq!(early.len(), 1);
        assert_eq!(
            merge_neighbours(&early, VId(1), REL, CommitSeq(3)).expect("merges"),
            vec![VId(2)]
        );
        assert!(matches!(
            admitted.resolve_blocks_at(cx, CommitSeq(7)),
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
    under_lab(42, move |cx| {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates");
        writer.seal(strata_keys).expect("seals");
        writer
            .apply(strata_keys, CommitSeq(4), &create(11, 1, 3))
            .expect("creates");
        let (root, blocks) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");

        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        // Store the root bytes through the generic object path and only the
        // FIRST block. `put_root` itself rejects this incomplete publication;
        // the raw path models a damaged/restored store that still reaches read.
        store.put(cx, &blocks[0].bytes).expect("stores");
        let root_bytes = fgdb_strata::root::encode_root(&root).expect("encodes root");
        let root_id = store.put(cx, &root_bytes).expect("plants root object");

        assert!(
            matches!(
                store.reopen(cx, root_id),
                Err(StoreError::RootBlockLoad { at: 1, error })
                    if matches!(*error, StoreError::Io(ref io)
                        if io.kind() == std::io::ErrorKind::NotFound)
            ),
            "a missing stored block must retain both its position and I/O diagnosis"
        );
        // The root itself is still perfectly readable — the failure is about the
        // partition, not about the root object, and the two are worth telling apart.
        assert_eq!(store.get_root(cx, root_id).expect("the root is fine"), root);
    });
}

/// A root is subject to the same identity rule as a block: bytes at its path that
/// are a DIFFERENT root are refused.
#[test]
fn a_root_at_the_wrong_identity_is_refused() {
    let dir = scratch_dir("reopen-wrongroot");
    under_lab(43, move |cx| {
        let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
        let mut writer = BlockWriter::new(GraphId(1), BranchId(1), 0);
        writer
            .apply(strata_keys, CommitSeq(1), &create(10, 1, 2))
            .expect("creates");
        let (root, blocks) = writer
            .publish(strata_keys, CommitSeq(9))
            .expect("publishes");

        let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
        for block in blocks {
            store.put(cx, &block.bytes).expect("stores block");
        }
        let root_id = store.put_root(cx, &root).expect("stores");

        // A different lawful root written over the path.
        let other = fgdb_strata::root::PartitionRoot {
            published_at: CommitSeq(11),
            ..root
        };
        let other_bytes = fgdb_strata::root::encode_root(&other).expect("encodes");
        std::fs::write(store.path(root_id), &other_bytes).expect("plant");

        assert!(matches!(
            store.get_root(cx, root_id),
            Err(StoreError::MalformedRoot(
                RootError::IdentityMismatch { .. }
            ))
        ));
    });
}
