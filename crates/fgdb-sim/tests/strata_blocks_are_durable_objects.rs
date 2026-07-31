//! **A Strata block is a durable object like any other, and this proves it.**
//!
//! Doctrine 5 is unambiguous: "There is no mutable primary file. The only mutable
//! object in a database directory is `manifest.root`. Everything else is immutable,
//! content-addressed (`ObjectId = Trunc128(BLAKE3(...))`), and RaptorQ-erasure-coded.
//! No double-write journaling anywhere — RaptorQ heals torn/corrupt symbols."
//!
//! `fgdb-strata` derives a block identity but seals nothing: it produces bytes and
//! a name for them. That is a subset, not a substitute, only if the bytes actually
//! go through the same pipeline every other durable object does — otherwise
//! "blocks are durable objects" is a claim with nothing behind it, and Strata would
//! be the one tier exempt from the rule the plan says has no exceptions.
//!
//! So these laws take a block the tier-D writer produced, seal it with Chronicle's
//! capsule machinery, and require the whole §5.1 pipeline to hold over it:
//!
//!   * the sealed capsule's identity is the block's own content identity;
//!   * the container round-trips to bytes that still decode as a lawful block;
//!   * bit rot up to the repair budget HEALS, and past it fails closed;
//!   * healed bytes still decode to the same entries.
//!
//! **WHY THIS IS NOT THE BIT-ROT CAMPAIGN AGAIN.** That file
//! (`fgdb-chronicle/tests/capsule_bit_rot.rs`) proves the capsule layer heals its
//! own opaque payload. It never asks whether the healed bytes still MEAN anything,
//! because at that layer a payload is a byte string. Here the payload is a
//! structured durable format with its own decoder and its own laws, and the
//! question is whether erasure recovery returns something the format still accepts
//! — a heal that returned plausible bytes which no longer decoded would satisfy
//! every law in the capsule campaign and still have lost the partition.

use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile, decode_container, encode_container};
use fgdb_delta_types::{DeltaRow, RelationId};
use fgdb_strata::writer::BlockWriter;
use fgdb_strata::{block_id, decode_block};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, ObjectId, VId};

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

/// The object kind a Strata delta block is sealed under.
///
/// **IT IS NOT PART OF THE IDENTITY, and my first draft of this comment claimed it
/// was.** Plan L278 defines `ObjectId = BLAKE3_keyed(K_oid, "fgdb:logical:v1" ‖
/// namespace ‖ header ‖ payload)` — the kind is carried alongside, not hashed in.
/// Measured, then checked against the plan: chronicle is right and the comment was
/// wrong, which is the same defect shape this workspace keeps finding, so it is
/// corrected here rather than quietly deleted.
///
/// Type confusion is guarded a layer up instead, by
/// `IdentifiedObject::verifies_as_same_object`, which compares digest, KIND, length
/// and canonical plaintext before any deduplication or substitution. The law at the
/// bottom of this file pins both halves: equal identity, unequal object.
const STRATA_BLOCK_KIND: u16 = 0x0301;

fn keys() -> CapsuleKeys {
    CapsuleKeys {
        k_oid: K_OID,
        namespace: NAMESPACE,
        dek: [0x3c; 32],
        object_kind: STRATA_BLOCK_KIND,
        profile: CapsuleProfile::balanced(),
    }
}

fn edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
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

/// A block large enough to need many symbols, produced by the REAL writer.
///
/// Built by the writer rather than hand-assembled so the bytes under test are the
/// bytes the tier actually produces — sealing a fixture would prove the pipeline
/// works on something no partition contains.
fn block_bytes() -> (Vec<u8>, ObjectId) {
    let mut writer = BlockWriter::new(GRAPH, BRANCH, 0);
    let strata_keys: (&[u8; 32], DatabaseSecurityNamespaceId) = (&K_OID, NAMESPACE);
    for i in 0..400u128 {
        writer
            .apply(strata_keys, CommitSeq(i as u64 + 1), &edge(i + 1, 1, i + 2))
            .expect("the writer accepts the row");
    }
    let sealed = writer
        .seal(strata_keys)
        .expect("seals")
        .expect("a non-empty block");
    (sealed.bytes.clone(), sealed.block_id)
}

/// THE IDENTITY IS THE SAME OBJECT ID on both sides.
///
/// Strata derives a block's name with §5.1's `logical_object_id`; the capsule layer
/// derives a capsule's name the same way. If those disagreed, a partition root
/// naming a block could not be used to fetch the capsule holding it — the root
/// would name one object and the store another, which is the failure
/// content-addressing exists to make impossible.
#[test]
fn a_sealed_block_keeps_its_own_identity() {
    let (bytes, id) = block_bytes();
    let sealed = keys().seal(&bytes).expect("seals");
    assert_eq!(
        sealed.object_id, id,
        "the capsule's identity must be the block's identity"
    );
    assert_eq!(id, block_id(&K_OID, NAMESPACE, &bytes));
}

/// The container round-trips to bytes that still decode as a LAWFUL block.
#[test]
fn a_sealed_block_round_trips_through_the_container() {
    let (bytes, id) = block_bytes();
    let entries = decode_block(&bytes).expect("the block is lawful to begin with");

    let sealed = keys().seal(&bytes).expect("seals");
    let container = encode_container(&sealed);
    let (descriptor, symbols) = decode_container(&container).expect("the container decodes");
    let recovered = keys()
        .recover(&descriptor, &symbols, id)
        .expect("recovers to the requested identity");

    assert_eq!(recovered, bytes, "byte-for-byte");
    assert_eq!(
        decode_block(&recovered).expect("and still a lawful block"),
        entries
    );
}

/// **BIT ROT UP TO THE REPAIR BUDGET HEALS, AND THE HEALED BYTES STILL DECODE.**
///
/// The law the capsule campaign cannot state. There, a payload is an opaque byte
/// string and "it healed" means the bytes came back. Here the payload is a
/// structured format, so the question is whether recovery returns something the
/// decoder still accepts — a heal producing plausible bytes that no longer decoded
/// would pass every capsule law and still have lost the partition.
#[test]
fn a_rotted_block_heals_and_still_decodes() {
    let (bytes, id) = block_bytes();
    let entries = decode_block(&bytes).expect("lawful");
    let sealed = keys().seal(&bytes).expect("seals");
    let mut container = encode_container(&sealed);

    let (_, symbols) = decode_container(&container).expect("decodes");
    let budget = CapsuleProfile::balanced().erasure_budget();
    assert!(
        symbols.len() > budget,
        "the block must span more symbols than the budget, or 'within budget' and \
         'the whole object' are the same test"
    );
    // Rot exactly the budget's worth, each in a different symbol, located by
    // content rather than by a computed offset.
    for symbol in symbols.iter().take(budget) {
        let at = container
            .windows(symbol.len())
            .position(|w| w == symbol.as_slice())
            .expect("each symbol appears verbatim");
        container[at + symbol.len() / 2] ^= 0x40;
    }

    let (descriptor, damaged) = decode_container(&container).expect("still parses");
    let healed = keys()
        .recover(&descriptor, &damaged, id)
        .expect("heals within the repair budget");
    assert_eq!(healed, bytes, "healed byte-for-byte");
    assert_eq!(
        decode_block(&healed).expect("and the healed bytes are still a lawful block"),
        entries,
        "same entries, not merely the same length"
    );
}

/// One symbol past the budget FAILS CLOSED — a block is not exempt from the rule
/// that beyond overhead recovery refuses rather than returning partial bytes.
#[test]
fn a_block_rotted_beyond_the_budget_fails_closed() {
    let (bytes, id) = block_bytes();
    let sealed = keys().seal(&bytes).expect("seals");
    let mut container = encode_container(&sealed);

    let (_, symbols) = decode_container(&container).expect("decodes");
    let over = CapsuleProfile::balanced().erasure_budget() + 1;
    for symbol in symbols.iter().take(over) {
        let at = container
            .windows(symbol.len())
            .position(|w| w == symbol.as_slice())
            .expect("each symbol appears verbatim");
        container[at + symbol.len() / 2] ^= 0x40;
    }

    let (descriptor, damaged) = decode_container(&container).expect("still parses");
    assert!(
        keys().recover(&descriptor, &damaged, id).is_err(),
        "beyond the budget a block must fail closed, not return usable-looking bytes"
    );
}

/// A BLOCK AND A COMMIT CAPSULE WITH IDENTICAL BYTES SHARE AN OBJECT ID, and the
/// substitution guard is what keeps them apart.
///
/// This is the design plan L278 specifies, not a gap: the identity binds key,
/// namespace, header and payload — not the kind — so content-addressing
/// deduplicates identical bytes across kinds, and `verifies_as_same_object`
/// refuses to SUBSTITUTE one for the other by comparing the kind explicitly.
/// Asserting equality here rather than difference is deliberate: a later change
/// that folded the kind into the digest would silently break deduplication, and
/// this law is what would notice.
///
/// The practical separation for Strata is elsewhere and stronger: a block begins
/// with `FGSB` and a delta template does not, so no block's bytes can equal a
/// template's however the identity is computed.
#[test]
fn identity_ignores_object_kind_and_the_substitution_guard_does_not() {
    let (bytes, _) = block_bytes();
    let as_block = keys().seal(&bytes).expect("seals").object_id;
    let as_other = CapsuleKeys {
        object_kind: fgdb_sim::CAPSULE_OBJECT_KIND,
        ..keys()
    }
    .seal(&bytes)
    .expect("seals")
    .object_id;
    assert_eq!(
        as_block, as_other,
        "plan L278 does not bind object_kind into the identity"
    );

    // And the guard that DOES separate them, at substitution time.
    let block = fgdb_chronicle::identity::IdentifiedObject::new(
        &K_OID,
        NAMESPACE,
        STRATA_BLOCK_KIND,
        &[],
        &bytes,
    );
    let other = fgdb_chronicle::identity::IdentifiedObject::new(
        &K_OID,
        NAMESPACE,
        fgdb_sim::CAPSULE_OBJECT_KIND,
        &[],
        &bytes,
    );
    assert_eq!(block.object_id(), other.object_id());
    assert!(
        !block.verifies_as_same_object(&other),
        "identical bytes of a different KIND must not be substitutable"
    );
    assert!(
        block.verifies_as_same_object(&fgdb_chronicle::identity::IdentifiedObject::new(
            &K_OID,
            NAMESPACE,
            STRATA_BLOCK_KIND,
            &[],
            &bytes,
        )),
        "and the same kind still is, or the guard refuses everything"
    );
}
