//! Laws of the block store — where a partition stops needing the whole stream.
//!
//! **THE STORE'S ONE JOB IS THAT THE PATH IS NOT THE NAME.** A block's filename is
//! derived from its identity, and a read RE-DERIVES that identity from the bytes it
//! found. Every interesting law here is a way of asking whether the store trusts
//! its own layout: if it does, it returns whatever sits at the expected path, which
//! is the exact failure content-addressing exists to prevent and the one that is
//! silent.
//!
//! The store is the first thing in `fgdb-strata` that touches a disk, so it is also
//! the first place `&CommitCx` appears — doctrine 3, and the boundary a lab runtime
//! swaps to inject fsync lies and torn writes.

use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::RelationId;
use fgdb_strata::store::{BlockStore, StoreError};
use fgdb_strata::{AdjacencyEntry, block_id, encode_block};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CommitSeq, VId};
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
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let id = store.put(cx, &bytes).expect("stores");

        assert_eq!(
            id,
            block_id(&K_OID, NAMESPACE, &bytes),
            "derived, not accepted"
        );
        assert!(store.contains(id));
        assert_eq!(
            store.get(id).expect("loads"),
            fgdb_strata::decode_block(&bytes).expect("decodes")
        );
        assert_eq!(store.get_bytes(id).expect("loads bytes"), bytes);
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
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        let mine = sample();
        let id = store.put(cx, &mine).expect("stores");

        // A different, perfectly lawful block written over the path.
        let other = encode_block(&[entry(9, 9, 7)]).expect("encodes");
        assert_ne!(other, mine);
        std::fs::write(store.path(id), &other).expect("overwrite");

        let actual = block_id(&K_OID, NAMESPACE, &other);
        assert!(
            matches!(
                store.get(id),
                Err(StoreError::IdentityMismatch { expected, actual: got })
                    if expected == id && got == actual
            ),
            "a store that trusts its layout returns the wrong partition silently"
        );
        // And the raw-bytes path enforces it too — a caller that skips decoding
        // must not thereby skip the identity check.
        assert!(matches!(
            store.get_bytes(id),
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
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        let id = store.put(cx, &sample()).expect("stores");

        let mut bytes = std::fs::read(store.path(id)).expect("read");
        let at = bytes.len() / 2;
        bytes[at] ^= 0x40;
        std::fs::write(store.path(id), &bytes).expect("write");

        assert!(matches!(
            store.get(id),
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
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
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
        assert_eq!(store.get_bytes(first).expect("loads"), bytes);
    });
}

/// A COLLISION IS REFUSED rather than overwritten.
///
/// Under a keyed 256-bit identity this is not a hash collision anyone will meet in
/// practice; it is a key or namespace mix-up, or a corrupted store. Both are worse
/// to overwrite than to refuse, and the refusal names the identity so an operator
/// can find the file.
#[test]
fn a_collision_is_refused() {
    let dir = scratch_dir("collision");
    under_lab(35, move |cx| {
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        let bytes = sample();
        let id = store.put(cx, &bytes).expect("stores");

        // Something else has taken this identity's path with different bytes.
        let other = encode_block(&[entry(4, 5, 6)]).expect("encodes");
        std::fs::write(store.path(id), &other).expect("plant");

        assert!(
            matches!(store.put(cx, &bytes), Err(StoreError::Collision { block_id }) if block_id == id),
            "a differing block at this identity must not be silently replaced"
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
        let mine = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        let theirs = BlockStore::open(&dir, [0x11; 32], NAMESPACE).expect("opens");
        let bytes = sample();

        let my_id = mine.put(cx, &bytes).expect("stores");
        let their_id = theirs.put(cx, &bytes).expect("stores");
        assert_ne!(my_id, their_id, "different keys, different objects");
        assert!(
            !mine.contains(their_id),
            "one key's store must not claim to hold another key's block, even \
             though the file is right there in the shared directory"
        );
        assert!(theirs.contains(their_id), "while its own store does");

        // Each store resolves its OWN identity and refuses the other's.
        assert_eq!(mine.get_bytes(my_id).expect("loads"), bytes);
        assert!(
            matches!(mine.get(their_id), Err(StoreError::IdentityMismatch { .. })),
            "the same bytes under another key are not this store's object"
        );
    });
}

/// A missing block is an IO failure naming the path, not a silent empty result.
#[test]
fn a_missing_block_is_an_error() {
    let dir = scratch_dir("missing");
    under_lab(37, move |_cx| {
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        let absent = ObjectId([0xab; 32]);
        assert!(!store.contains(absent));
        assert!(matches!(store.get(absent), Err(StoreError::Io(_))));
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
        let store = BlockStore::open(&dir, K_OID, NAMESPACE).expect("opens");
        // Stored honestly — the store derives the identity, so this IS the object
        // it names. It simply is not a block.
        let id = store
            .put(cx, b"this is not a strata block")
            .expect("stores");
        assert!(matches!(store.get(id), Err(StoreError::Malformed(_))));
        assert!(
            store.get_bytes(id).is_ok(),
            "and the raw path still returns them, since identity is all it claims"
        );
    });
}
