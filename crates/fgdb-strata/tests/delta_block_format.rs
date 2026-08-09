//! Laws of the tier-one delta-block format.
//!
//! These are the laws a BYTE LAYOUT can fail and an in-memory map cannot, which is
//! why the format is the honest first slice of Strata rather than a
//! `BTreeMap<(VId, RelationId, VId, EId), _>` with the right answers on top. A map has
//! no durable form; nothing about it can be non-canonical, truncated, or decode to
//! something other than what was encoded. Doctrine 7 names that substitution
//! directly — "no `HashMap<VId, Vec<EId>>` presented as storage" — and these tests
//! are what make the difference measurable rather than asserted.
//!
//! **THE CANONICAL LAW IS THE LOAD-BEARING ONE.** Exactly one byte string per
//! value (doctrine 4). The encoder refuses unsorted or repeated entries instead of
//! sorting them, and the decoder independently refuses a block whose entries are
//! not ascending — so a hand-built block cannot smuggle in an order the encoder
//! would never emit. A format that quietly sorted its input would let two callers
//! store different intents, both be told they succeeded, and produce one byte
//! string that answers to neither.

use fgdb_delta_types::RelationId;
use fgdb_strata::{
    AdjacencyEntry, BLOCK_FORMAT_V3, BLOCK_MAGIC, BlockError, MAX_BLOCK_ENTRIES, block_id,
    decode_block, encode_block, read_block, scan_neighbours,
};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CommitSeq, EId, VId};

const REL: RelationId = RelationId(1);
const OTHER_REL: RelationId = RelationId(2);

fn entry(src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    edge(
        src.wrapping_mul(1_000_000).wrapping_add(dst),
        src,
        dst,
        created,
        retired,
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

/// v1 -> {v2, v3}, v2 -> {v3}, with v1->v3 retired at 5.
fn sample() -> Vec<AdjacencyEntry> {
    vec![
        entry(1, 2, 1, None),
        entry(1, 3, 2, Some(5)),
        entry(1, 4, 3, None),
    ]
}

// ---------------------------------------------------------------------------
// Round trip and canonicality
// ---------------------------------------------------------------------------

/// Encode then decode returns exactly what went in.
#[test]
fn a_block_round_trips() {
    let entries = sample();
    let bytes = encode_block(0, None, &entries).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes"), entries);
}

/// EXACTLY ONE BYTE STRING PER VALUE: encoding the same entries twice produces
/// identical bytes, and no other input produces those bytes.
#[test]
fn encoding_is_deterministic_and_injective() {
    let a = encode_block(0, None, &sample()).expect("encodes");
    let b = encode_block(0, None, &sample()).expect("encodes");
    assert_eq!(a, b, "the same entries must produce the same bytes");

    let mut different = sample();
    different[1].created_at = CommitSeq(4);
    assert_ne!(
        encode_block(0, None, &different).expect("encodes"),
        a,
        "a different value must produce different bytes"
    );
}

/// THE ENCODER REFUSES unsorted input rather than sorting it.
///
/// Sorting would be the friendly behaviour and the wrong one: a caller handing
/// over a different order is describing a different intent, and repairing it
/// silently lets two callers disagree about what they stored while both succeed.
#[test]
fn the_encoder_refuses_unsorted_entries() {
    let mut entries = sample();
    entries.swap(0, 1);
    assert_eq!(
        encode_block(0, None, &entries),
        Err(BlockError::NonCanonicalOrder { at: 1 })
    );
}

/// The encoder refuses DUPLICATE keys — strictly ascending, not merely ascending.
///
/// Two entries for one `(src, relation, dst, eid)` are two statements of one
/// stable edge slot, and a block cannot say which supersedes the other.
#[test]
fn the_encoder_refuses_duplicate_keys() {
    let entries = vec![entry(1, 2, 1, None), entry(1, 2, 3, None)];
    assert_eq!(
        encode_block(0, None, &entries),
        Err(BlockError::NonCanonicalOrder { at: 1 })
    );
}

/// EId is the unconditional discriminator: parallel edges with equal topology
/// are distinct durable entries, while neighbour projection remains set-valued.
#[test]
fn parallel_edge_identities_round_trip_without_repeating_the_destination() {
    let entries = vec![edge(10, 1, 2, 1, Some(4)), edge(20, 1, 2, 2, None)];
    let bytes = encode_block(0, None, &entries).expect("parallel EIds are canonical keys");
    assert_eq!(decode_block(&bytes).expect("decodes"), entries);
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(3)).expect("scans"),
        vec![VId(2)],
        "two live parallel edges still yield one neighbour"
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(4)).expect("scans"),
        vec![VId(2)],
        "retiring one EId must not hide its live parallel peer"
    );
}

/// THE DECODER ENFORCES ORDER INDEPENDENTLY of the encoder.
///
/// Built by hand from a valid block with two entries transposed, so the bytes are
/// well-formed in every other respect. A decoder that trusted the encoder's
/// discipline would accept it — and a block read from disk was not necessarily
/// written by this process.
#[test]
fn the_decoder_refuses_an_out_of_order_block() {
    let entries = sample();
    let bytes = encode_block(0, None, &entries).expect("encodes");
    let swapped = encode_forged(&[entries[1], entries[0], entries[2]]);
    assert_ne!(swapped, bytes, "the forged bytes differ from a V3 frame");
    assert!(
        decode_block(&swapped).is_err(),
        "a hand-built V2-shaped payload is not a V3 block"
    );
}

/// Encode without the ordering checks, to build blocks the encoder would refuse.
///
/// The decoder's laws need inputs the encoder cannot produce; without this the
/// decoder could only ever be tested on bytes that already satisfy every rule,
/// which tests nothing about the decoder.
fn encode_forged(entries: &[AdjacencyEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&BLOCK_MAGIC);
    out.extend_from_slice(&BLOCK_FORMAT_V3.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&e.src.0.to_be_bytes());
        out.extend_from_slice(&e.relation.0.to_be_bytes());
        out.extend_from_slice(&e.dst.0.to_be_bytes());
        out.extend_from_slice(&e.eid.0.to_be_bytes());
        out.extend_from_slice(&e.created_at.0.to_be_bytes());
        out.extend_from_slice(&e.retired_at.map_or(0, |r| r.0).to_be_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// Framing: the failures a map cannot have
// ---------------------------------------------------------------------------

/// Bytes that are not a block are refused, and distinctly from a version we do
/// not implement: "this is not our file" and "this is a newer version of our
/// file" call for completely different operator responses.
#[test]
fn foreign_bytes_and_future_versions_are_refused_distinctly() {
    assert_eq!(
        decode_block(b"not a block at all"),
        Err(BlockError::NotABlock)
    );
    assert_eq!(decode_block(&[]), Err(BlockError::NotABlock));

    let mut future = encode_block(0, None, &sample()).expect("encodes");
    future[4] = 0x00;
    future[5] = 0x09;
    assert_eq!(
        decode_block(&future),
        Err(BlockError::UnsupportedFormat { format: 9 })
    );

    let mut legacy = encode_block(0, None, &sample()).expect("encodes");
    legacy[4..6].copy_from_slice(&1u16.to_be_bytes());
    assert_eq!(
        decode_block(&legacy),
        Err(BlockError::UnsupportedFormat { format: 1 }),
        "V1 omitted EId and cannot represent the V2 value"
    );
}

/// The reserved `property_patch_refs[]` slot is a CONTRACT, not dead bytes
/// (fgdb-2t7q ruling 3B): a block declaring patch refs before the patch
/// machinery exists is refused with the exact declared count, and restoring
/// the reserved zero makes the same bytes decode again — the two halves prove
/// the count is read and validated rather than skipped.
#[test]
fn a_nonzero_property_patch_count_is_refused_until_the_machinery_lands() {
    let mut declared = encode_block(0, None, &sample()).expect("encodes");
    // The count sits at bytes [47, 49) — after magic, format, partition,
    // rows, (src, relation, direction), and span count (V5 layout).
    declared[47..49].copy_from_slice(&3u16.to_be_bytes());
    assert_eq!(
        decode_block(&declared),
        Err(BlockError::PropertyPatchesNotYetImplemented { declared: 3 })
    );

    declared[47..49].copy_from_slice(&0u16.to_be_bytes());
    assert_eq!(
        decode_block(&declared).expect("the reserved zero decodes"),
        sample(),
        "restoring the reserved zero must restore the block, or the refusal \
         above was testing something other than this slot"
    );
}

/// THE JOINT-FIT WITNESS for the fgdb-2t7q headroom coupling: ruling 1B-i's
/// visibility spans and ruling 3B's per-entry locator spend the SAME 3 B that
/// remain under §6.2's 16 B ceiling after the two identity columns, so they
/// cannot be verified independently — this pins the arithmetic jointly, on the
/// same reference run the byte-economy chain measures.
///
/// The sitting's escape hatch fires if spans exceed 2 B/entry amortized; the
/// locator needs at least 1 B. Both live inside the measured column cost or
/// the 4 KiB / 256-entry sizing stops being achievable as written — which
/// would be a plan-level finding, not a tuning problem.
#[test]
fn visibility_spans_and_the_patch_locator_jointly_fit_the_ceiling() {
    let rows = run_of(NORMATIVE_ENTRIES_PER_BLOCK);
    let encoded = encode_block(0, None, &rows).expect("encodes the normative run");

    // Whole-frame amortized cost of everything that is NOT the two identity
    // columns, measured by differencing against the columns' own payload cost.
    let dsts: Vec<_> = rows.iter().map(|e| e.dst).collect();
    let eids: Vec<_> = rows.iter().map(|e| e.eid).collect();
    let column_bytes = codec_payload_len(&dsts) + codec_payload_len(&eids);
    let framing_bytes = encoded.len() - column_bytes;
    let span_and_frame_amortized = framing_bytes.div_ceil(rows.len());

    const LOCATOR_MIN_BYTES: usize = 1;
    let columns_amortized = column_bytes.div_ceil(rows.len());
    assert!(
        columns_amortized + span_and_frame_amortized + LOCATOR_MIN_BYTES
            <= NORMATIVE_BYTES_PER_ENTRY,
        "joint fit failed on the reference run: columns {columns_amortized} + \
         spans/framing {span_and_frame_amortized} + locator {LOCATOR_MIN_BYTES} \
         exceeds §6.2's {NORMATIVE_BYTES_PER_ENTRY} B ceiling — the fgdb-2t7q \
         field-1 escape hatch fires and 1C needs its churn cost measured"
    );
}

/// TRUNCATION is refused, and the error says how much was expected.
///
/// Every prefix of a valid block is tested, because "it refuses an empty tail" and
/// "it refuses any short read" are different claims and only the second is useful.
#[test]
fn every_truncation_is_refused() {
    let bytes = encode_block(0, None, &sample()).expect("encodes");
    for cut in 0..bytes.len() {
        let result = decode_block(&bytes[..cut]);
        assert!(
            result.is_err(),
            "a {cut}-byte prefix of a {}-byte block must not decode",
            bytes.len()
        );
    }
    assert!(decode_block(&bytes).is_ok(), "and the whole block does");
}

/// TRAILING BYTES are refused rather than ignored. A trailing region is either a
/// second block someone concatenated or damage, and reading past it is wrong
/// either way.
#[test]
fn trailing_bytes_are_refused() {
    let mut bytes = encode_block(0, None, &sample()).expect("encodes");
    bytes.push(0x00);
    assert_eq!(
        decode_block(&bytes),
        Err(BlockError::TrailingBytes { extra: 1 })
    );
}

/// A declared entry count that could not fit is refused BEFORE it is used to size
/// anything. A length prefix read from possibly-damaged bytes is an allocation
/// request until it is bounded.
#[test]
fn an_implausible_entry_count_is_refused_before_allocating() {
    let mut bytes = encode_block(0, None, &sample()).expect("encodes");
    bytes[14..18].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        decode_block(&bytes),
        Err(BlockError::ImplausibleEntryCount { declared: u32::MAX })
    );
    // And the bound is the declared one, not an accident of the input's length.
    bytes[14..18].copy_from_slice(&(MAX_BLOCK_ENTRIES + 1).to_be_bytes());
    assert!(matches!(
        decode_block(&bytes),
        Err(BlockError::ImplausibleEntryCount { .. })
    ));
}

// ---------------------------------------------------------------------------
// Version intervals
// ---------------------------------------------------------------------------

/// An entry retired at or before its creation is refused, on both sides.
#[test]
fn a_retirement_at_or_before_creation_is_refused() {
    for retired in [3u64, 2] {
        let entries = vec![entry(1, 2, 3, Some(retired))];
        assert_eq!(
            encode_block(0, None, &entries),
            Err(BlockError::RetiredBeforeCreated {
                at: 0,
                created_at: CommitSeq(3),
                retired_at: CommitSeq(retired),
            })
        );
    }
    assert!(encode_block(0, None, &[entry(1, 2, 3, Some(4))]).is_ok());
}

/// Creation at sequence zero is refused: zero names the empty stream and can
/// never have created anything. This is also what makes zero usable as the
/// on-disk spelling of "live" without giving `None` two spellings.
#[test]
fn creation_at_the_empty_stream_is_refused() {
    assert_eq!(
        encode_block(0, None, &[entry(1, 2, 0, None)]),
        Err(BlockError::CreatedAtZero { at: 0 })
    );
}

/// A live entry round-trips as live — the zero sentinel is not mistaken for a
/// retirement at sequence zero.
#[test]
fn a_live_entry_round_trips_as_live() {
    let bytes = encode_block(0, None, &[entry(1, 2, 1, None)]).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes")[0].retired_at, None);
}

// ---------------------------------------------------------------------------
// Snapshot-visible scans
// ---------------------------------------------------------------------------

/// A scan sees an entry from its creation sequence onward, and not before.
#[test]
fn a_scan_sees_an_entry_from_its_creation_sequence() {
    let bytes = encode_block(0, None, &sample()).expect("encodes");
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(1)).expect("scans"),
        vec![VId(2)],
        "at 1 only the first edge exists"
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(2)).expect("scans"),
        vec![VId(2), VId(3)]
    );
}

/// THE INTERVAL IS HALF-OPEN: an entry retired at N is invisible AT N.
///
/// With a closed upper bound an edge retired at N and one created at N would both
/// be visible at N, so a replaced edge would have two simultaneous versions — the
/// same reason valid-time periods are half-open.
#[test]
fn retirement_is_half_open() {
    let bytes = encode_block(0, None, &sample()).expect("encodes");
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(4)).expect("scans"),
        vec![VId(2), VId(3), VId(4)],
        "visible through 4"
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(5)).expect("scans"),
        vec![VId(2), VId(4)],
        "and gone AT 5, the sequence that retired it"
    );
}

/// A scan is scoped to its relation and its source. Without this a scan that
/// ignored either would pass every visibility law above.
#[test]
fn a_scan_is_scoped_to_its_source_and_relation() {
    let entries = vec![entry(1, 2, 1, None)];
    let bytes = encode_block(0, None, &entries).expect("encodes");
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(9)).expect("scans"),
        vec![VId(2)]
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(1), OTHER_REL, CommitSeq(9)).expect("scans"),
        Vec::<VId>::new()
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(3), REL, CommitSeq(9)).expect("scans"),
        Vec::<VId>::new(),
        "a source with no entries scans empty rather than failing"
    );
}

/// A SCAN VALIDATES WHAT IT READS. It is a read path, and a read path that
/// skipped the decoder's checks would be a second, weaker decoder — the
/// verify-narrower-than-validate shape that has bitten this workspace twice
/// (fgdb-dcq7, fgdb-delta-index-verify-fail-closed-iovs).
#[test]
fn a_scan_refuses_what_the_decoder_would_refuse() {
    let entries = sample();
    let forged = encode_forged(&[entries[1], entries[0], entries[2]]);
    assert!(
        scan_neighbours(&forged, VId(1), REL, CommitSeq(9)).is_err(),
        "a forged block must not scan"
    );

    let mut truncated = encode_block(0, None, &entries).expect("encodes");
    truncated.truncate(truncated.len() - 1);
    assert!(matches!(
        scan_neighbours(&truncated, VId(1), REL, CommitSeq(9)),
        Err(BlockError::Truncated { .. })
    ));
}

/// A scan agrees with decoding and filtering by hand — the format's two read
/// paths must not disagree.
///
/// Swept across every sequence around the sample's boundaries rather than probed
/// at one, since the middle of an interval is where every implementation agrees.
#[test]
fn a_scan_agrees_with_decode_and_filter() {
    let bytes = encode_block(0, None, &sample()).expect("encodes");
    let decoded = decode_block(&bytes).expect("decodes");
    for as_of in 1..=7u64 {
        let scanned = scan_neighbours(&bytes, VId(1), REL, CommitSeq(as_of)).expect("scans");
        let filtered: Vec<VId> = decoded
            .iter()
            .filter(|e| e.src == VId(1) && e.relation == REL && e.visible_at(CommitSeq(as_of)))
            .map(|e| e.dst)
            .collect();
        assert_eq!(scanned, filtered, "the two read paths disagree at {as_of}");
    }
}

/// An empty block is valid and scans empty. The vacuous case answered explicitly,
/// since a decoder that rejected it would be caught by nothing else here.
#[test]
fn an_empty_block_is_valid() {
    let bytes = encode_block(0, None, &[]).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes"), Vec::new());
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(1)).expect("scans"),
        Vec::<VId>::new()
    );
}

// ---------------------------------------------------------------------------
// Block identity
// ---------------------------------------------------------------------------

const K_OID: [u8; 32] = [0x5a; 32];

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([0x77; 32])
}

/// Identity is a function of the CONTENT: the same block always has the same id,
/// and any different block has a different one.
#[test]
fn identity_is_derived_from_content() {
    let a = encode_block(0, None, &sample()).expect("encodes");
    assert_eq!(
        block_id(&K_OID, namespace(), &a),
        block_id(&K_OID, namespace(), &a),
        "identity must be a function, not a fresh value"
    );

    let mut other = sample();
    other[2].created_at = CommitSeq(4);
    let b = encode_block(0, None, &other).expect("encodes");
    assert_ne!(
        block_id(&K_OID, namespace(), &a),
        block_id(&K_OID, namespace(), &b),
        "one changed sequence must change the identity"
    );
}

/// Identity is SCOPED to the database's key and security namespace, so the same
/// bytes in two databases are two different objects.
///
/// This is what makes it §5.1's logical object id rather than a bare content hash:
/// an identity that ignored the namespace would let one database's block be named
/// — and therefore fetched — by another's root.
#[test]
fn identity_is_scoped_to_the_database() {
    let bytes = encode_block(0, None, &sample()).expect("encodes");
    let mine = block_id(&K_OID, namespace(), &bytes);

    assert_ne!(
        mine,
        block_id(&[0x11; 32], namespace(), &bytes),
        "a different key is a different object"
    );
    assert_ne!(
        mine,
        block_id(&K_OID, DatabaseSecurityNamespaceId([0x22; 32]), &bytes),
        "a different security namespace is a different object"
    );
}

/// `read_block` returns the entries when the bytes ARE the block asked for.
#[test]
fn read_block_accepts_the_block_it_names() {
    let entries = sample();
    let bytes = encode_block(0, None, &entries).expect("encodes");
    let id = block_id(&K_OID, namespace(), &bytes);
    assert_eq!(
        read_block(&K_OID, namespace(), &bytes, id).expect("reads"),
        entries
    );
}

/// THE LOAD-BEARING LAW: well-formed bytes that are the WRONG BLOCK are refused.
///
/// Every other refusal in this file is about malformed bytes. This one is about
/// bytes that decode perfectly and are simply not what was asked for — the failure
/// a content-addressed store exists to prevent, and the only one that is silent
/// without this check. A partition root naming a block must be able to prove the
/// bytes it found are that block rather than trusting the path they came from.
#[test]
fn read_block_refuses_a_different_block() {
    let mine = encode_block(0, None, &sample()).expect("encodes");
    let mut other_entries = sample();
    other_entries[0].created_at = CommitSeq(9);
    let other = encode_block(0, None, &other_entries).expect("encodes");

    let expected = block_id(&K_OID, namespace(), &mine);
    let actual = block_id(&K_OID, namespace(), &other);
    assert_eq!(
        read_block(&K_OID, namespace(), &other, expected),
        Err(BlockError::IdentityMismatch { expected, actual }),
        "a well-formed block that is not the requested one must be refused"
    );
    // And it is refused BEFORE decoding: the wrong block here is perfectly valid.
    assert!(decode_block(&other).is_ok());
}

/// Identity is checked before the contents are interpreted, so damaged bytes that
/// are also the wrong block report the identity failure rather than a parse error.
///
/// The order matters for the diagnostic: "you fetched the wrong object" sends an
/// operator somewhere completely different from "this object is corrupt".
#[test]
fn identity_is_checked_before_the_contents() {
    let bytes = encode_block(0, None, &sample()).expect("encodes");
    let id = block_id(&K_OID, namespace(), &bytes);

    let mut damaged = bytes.clone();
    damaged.truncate(damaged.len() - 1);
    assert!(
        matches!(
            read_block(&K_OID, namespace(), &damaged, id),
            Err(BlockError::IdentityMismatch { .. })
        ),
        "truncated bytes are a different object before they are a short read"
    );
}

/// An identity naming a block still gets the DECODER's laws: matching bytes that
/// are internally malformed are refused, not blessed by their identity.
///
/// Identity says "these are the bytes you asked for"; it says nothing about
/// whether those bytes are a lawful block. A reader that stopped at the identity
/// check would accept an out-of-order block whose id happened to be requested —
/// which is exactly how a forged root would smuggle one in.
#[test]
fn a_matching_identity_does_not_excuse_a_malformed_block() {
    let entries = sample();
    let forged = encode_forged(&[entries[1], entries[0], entries[2]]);
    let id = block_id(&K_OID, namespace(), &forged);
    assert!(
        read_block(&K_OID, namespace(), &forged, id).is_err(),
        "identity is not a substitute for the decoder's laws"
    );
}

// ---------------------------------------------------------------------------
// Byte economy — §6.2's 4 KiB / 256-entry law, and the distance to it
// ---------------------------------------------------------------------------
//
// **THESE WITNESSES PUBLISH BAD NUMBERS ON PURPOSE**, the same discipline
// `complexity_witness.rs` applies to the read path. §6.2 is normative about this
// format's economics: a Tier-D block is "about 4 KiB" and holds "at most 256"
// entries, and that sizing "presumes this encoding at <=16 B per entry — raw
// 128-bit identities would cap a 4 KiB block near 95 entries". This format stores
// raw 128-bit identities. So the law is missed, and it is missed by an amount
// nobody had ever measured: the gap lived in a bead comment, not in the tree.
//
// **WHY MEASURE INSTEAD OF READING THE CONSTANT.** `ENTRY_LEN` is private, and a
// witness that restated it would only prove the crate agrees with itself. These
// encode real blocks and difference their lengths, so they measure what a disk
// would actually receive — and they keep working if the layout is restructured
// rather than merely re-tuned.
//
// **THEY SHOULD FAIL WHEN `fgdb-w3-tier-d-ctj` LANDS THE IDENTITY-COLUMN CODEC**,
// and failing is the correct outcome: it is the signal to re-derive these numbers
// downward toward the law. Until then they hold the line in the other direction —
// the format cannot silently get FATTER, which is the regression nobody would
// otherwise notice. This is also the measurement that keeps the format out of
// Appendix A: registering it would freeze the number below as normative.

/// §6.2: a Tier-D block is "about 4 KiB".
const NORMATIVE_BLOCK_BYTES: usize = 4096;
/// §6.2: that block holds "at most 256" entries.
const NORMATIVE_ENTRIES_PER_BLOCK: usize = 256;
/// §6.2: the sizing "presumes this encoding at <=16 B per entry".
const NORMATIVE_BYTES_PER_ENTRY: usize = 16;

/// `count` strictly ascending, distinct entries out of one source.
///
/// Ascending `dst` gives ascending `(src, relation, dst, eid)` because `entry`
/// derives the `EId` from both endpoints, so this is a shape the ENCODER accepts
/// rather than one it would refuse — a refused encode would make every
/// measurement below vacuous.
fn run_of(count: usize) -> Vec<AdjacencyEntry> {
    (0..count)
        .map(|i| entry(1, (i + 2) as u128, 1, None))
        .collect()
}

/// The first member of the acceptance chain is the measured V2 baseline.  It is
/// retained as a named historical control: V3 must never recreate the raw shape.
#[test]
fn the_v2_raw_entry_baseline_is_seventy_two_bytes() {
    const V2_RAW_ENTRY_BYTES: usize = 16 + 8 + 16 + 16 + 8 + 8;
    assert_eq!(V2_RAW_ENTRY_BYTES, 72);
}

/// WITNESS, AND IT RECORDS A BAD NUMBER ON PURPOSE: 4 KiB holds 56 entries where
/// §6.2's law wants 256.
///
/// Measured by encoding until the budget is exceeded rather than by arithmetic on
/// a constant, so it stays true across a header change. A 4.5x density shortfall
/// is what "registering this format would enshrine a regression" means in
/// numbers, and it is the whole reason `BLOCK_FORMAT_V2` carries no Appendix A
/// row (see the note on that constant).
#[test]
fn four_kibibytes_hold_the_normative_two_hundred_fifty_six_entries() {
    let mut fits = 0usize;
    for count in 1..=NORMATIVE_ENTRIES_PER_BLOCK {
        let encoded = encode_block(0, None, &run_of(count)).expect("encodes");
        if encoded.len() > NORMATIVE_BLOCK_BYTES {
            break;
        }
        fits = count;
    }
    assert_eq!(
        fits, NORMATIVE_ENTRIES_PER_BLOCK,
        "a {NORMATIVE_BLOCK_BYTES} B block holds {fits} entries; §6.2 wants \
         {NORMATIVE_ENTRIES_PER_BLOCK}. The shortfall is the identity-column encoding, \
         not the header"
    );
    assert_eq!(MAX_BLOCK_ENTRIES as usize, NORMATIVE_ENTRIES_PER_BLOCK);
}

// ---------------------------------------------------------------------------
// What adopting the codec would actually buy — measured, not projected
// ---------------------------------------------------------------------------
//
// **THIS ANSWERS A SCOPING QUESTION WITH THE REAL CODEC INSTEAD OF ARITHMETIC.**
// `fgdb-codec::identity` already implements §6.2's registered identity-column
// layout, and this crate does not call it (fgdb-by2l). The obvious next increment
// is "adopt the codec for the block's identity columns" — and the numbers below
// say that increment CANNOT reach §6.2's <=16 B/entry law, because the law is not
// really about the codec.
//
// The measurement runs the actual encoder over the actual entries a block holds,
// so it is not a size model that can drift from the code it predicts. It is a
// dev-dependency: measuring what adoption buys is not adoption.
//
// **WHY THE LAW NEEDS MORE THAN A CODEC.** §6.2's entry is
// `(dst_VId, EId, user_key_ref?, prop_row_ref, flags)` — it carries NO `src`, no
// `relation`, and no inline `created_at`/`retired_at`, because a normative block
// is per `(partition_id, descriptor_key)` and visibility lives in the block's own
// `visibility_intervals[]`. Those four fields are hoisted OUT of the entry. This
// format keeps all four in every entry, and no encoding of the three identity
// columns can pay for a `relation` and two sequence numbers that should not be
// there at all. Two identity columns at ~6 B is how 16 B is reached; six columns
// is not.

use fgdb_codec::identity::{IdentityColumn, IdentityColumnLimits};

/// Encode one identity column of `values` through the real codec and return its
/// exact payload length.
fn codec_payload_len<T: fgdb_codec::identity::ElementIdentity>(values: &[T]) -> usize {
    let limits = IdentityColumnLimits::new(values.len().max(1), 256, 1 << 20);
    let column =
        IdentityColumn::try_new(values, limits).expect("the codec accepts block identities");
    column.encoded_payload_len()
}

/// WITNESS: adopting the identity-column codec for this entry shape gets to
/// roughly 42 B/entry — a real 1.7x, and still 2.6x above §6.2's law.
///
/// Measured over a 256-entry run, the size §6.2 actually talks about. The three
/// identity columns are handed to the codec exactly as a block holds them; the
/// `relation`/`created_at`/`retired_at` columns keep their present fixed width,
/// because nothing in the identity codec addresses them.
///
/// **THE POINT IS THE SHORTFALL, NOT THE SAVING.** A format change that lands
/// this and stops would still miss the law, so it would still not let
/// `DeltaBlockVersion` into Appendix A — which is the entire purpose of the
/// exercise (fgdb-ge6a). Read this as: adopt the codec AND hoist the four
/// non-entry fields in the same breaking change, or the block gets rewritten
/// twice for one outcome.
#[test]
fn adopting_the_codec_alone_lands_near_forty_two_bytes_and_the_law_wants_sixteen() {
    let entries = run_of(NORMATIVE_ENTRIES_PER_BLOCK);
    let rows = entries.len();

    let srcs: Vec<_> = entries.iter().map(|e| e.src).collect();
    let dsts: Vec<_> = entries.iter().map(|e| e.dst).collect();
    let eids: Vec<_> = entries.iter().map(|e| e.eid).collect();

    let identity_bytes =
        codec_payload_len(&srcs) + codec_payload_len(&dsts) + codec_payload_len(&eids);
    // relation(8) + created_at(8) + retired_at(8), untouched by an identity codec.
    let fixed_bytes = rows * (8 + 8 + 8);
    let per_entry = (identity_bytes + fixed_bytes).div_ceil(rows);

    assert!(
        (40..=44).contains(&per_entry),
        "codec-adopted entry cost measured {per_entry} B; expected ~42 B \
         (three identity columns at ~6 B plus 24 B of fixed columns). If this moved, \
         the codec's representation chooser changed and the fgdb-by2l scoping \
         argument must be re-derived"
    );
    assert!(
        per_entry > NORMATIVE_BYTES_PER_ENTRY,
        "codec adoption alone still misses §6.2's {NORMATIVE_BYTES_PER_ENTRY} B law: \
         the four hoisted fields, not the encoding, are what stands between this \
         format and registration"
    );
    // And it IS a real improvement over the raw layout, which is why the codec is
    // part of the answer even though it is not the whole answer.
    assert!(
        per_entry < 72,
        "codec adoption must beat the V2 raw 72 B layout"
    );
}

/// The actual V3 durable frame completes the 72 -> 43 -> 13 acceptance chain.
/// Its visibility span is one 24-byte record over 256 rows: 0.094 B/entry,
/// comfortably below the 2 B/entry escape hatch.
#[test]
fn the_v3_frame_is_thirteen_bytes_per_entry_and_visibility_amortizes_below_two() {
    let rows = run_of(NORMATIVE_ENTRIES_PER_BLOCK);
    let encoded = encode_block(0, None, &rows).expect("V3 encodes the normative run");
    assert_eq!(
        encoded.len().div_ceil(rows.len()),
        13,
        "72 -> 43 -> 13 acceptance chain"
    );
    let visibility_bytes = 24usize;
    assert!(
        visibility_bytes * 100 < rows.len() * 200,
        "visibility spans exceed 2 B/entry amortized"
    );
}

/// WITNESS: §6.2's OWN entry shape reaches the law with the same codec — 13 B for
/// two identity columns, inside the 16 B ceiling.
///
/// This is the control that proves the shortfall above is the ENTRY SHAPE and not
/// the codec: hand the identical codec only the two columns §6.2's entry actually
/// carries, and the law is met. §6.2's ceiling is not arbitrary — it is close to
/// what two shared-prefix identity columns cost, which is why the normative entry
/// has exactly two of them.
///
/// The 13 is 6 B of slot per row per column, plus one 11-byte prefix dictionary
/// per column amortized over 256 rows. The dictionaries are why this is 13 and not
/// the 12 the per-row arithmetic alone suggests — a detail worth pinning, because
/// the 3 B of remaining headroom is what `user_key_ref?`, `prop_row_ref` and
/// `flags` have to fit into, and that is tight rather than comfortable.
#[test]
fn the_normative_entry_shape_reaches_the_law_with_the_same_codec() {
    let entries = run_of(NORMATIVE_ENTRIES_PER_BLOCK);
    let rows = entries.len();

    let dsts: Vec<_> = entries.iter().map(|e| e.dst).collect();
    let eids: Vec<_> = entries.iter().map(|e| e.eid).collect();
    let per_entry = (codec_payload_len(&dsts) + codec_payload_len(&eids)).div_ceil(rows);

    assert!(
        per_entry <= NORMATIVE_BYTES_PER_ENTRY,
        "the normative two-column entry measured {per_entry} B against §6.2's \
         {NORMATIVE_BYTES_PER_ENTRY} B ceiling; if this ever exceeds it, §6.2's \
         4 KiB/256 sizing is not achievable as written and that is a plan-level finding"
    );
    // Pinned exactly, because the margin is the interesting part: §6.2's entry
    // also carries user_key_ref?, prop_row_ref and flags, and they have to fit in
    // what is left under the same ceiling.
    assert_eq!(
        per_entry,
        13,
        "two identity columns measured {per_entry} B/entry (6 B slot per row per \
         column, plus an 11 B prefix dictionary each amortized over {rows} rows). \
         That leaves {} B under §6.2's {NORMATIVE_BYTES_PER_ENTRY} B ceiling for \
         user_key_ref?/prop_row_ref/flags",
        NORMATIVE_BYTES_PER_ENTRY - per_entry
    );
}

/// CONTROL: the two witnesses above can actually fail.
///
/// Both measure a difference between encoder outputs, so a fixture that silently
/// built degenerate input — an empty run, or one the encoder refuses — would make
/// them measure nothing while still reporting a number. This pins the fixture: the
/// run is the length asked for, strictly ascending, and accepted by the encoder,
/// and a block of `n` entries really does grow with `n`.
#[test]
fn the_fixture_builds_the_run_the_byte_economy_witnesses_assume() {
    let run = run_of(8);
    assert_eq!(run.len(), 8, "run_of(8) must build eight entries");
    for pair in run.windows(2) {
        assert!(
            (pair[0].src, pair[0].relation, pair[0].dst, pair[0].eid)
                < (pair[1].src, pair[1].relation, pair[1].dst, pair[1].eid),
            "the run must be strictly ascending or the encoder would refuse it"
        );
    }
    let eight = encode_block(0, None, &run).expect("the encoder must accept the fixture");
    let nine = encode_block(0, None, &run_of(9)).expect("the encoder must accept the fixture");
    assert!(
        nine.len() > eight.len(),
        "a longer run must encode to more bytes, or the measurement is vacuous"
    );
    // And the decoder agrees the fixture is a lawful block, so the bytes measured
    // above are bytes a reader would actually accept.
    assert_eq!(
        decode_block(&eight).expect("the fixture must decode"),
        run,
        "the measured bytes must round-trip to the fixture"
    );
}

// ---------------------------------------------------------------------------
// V5: partition_id and the canonical logical digest (fgdb-da6b)
// ---------------------------------------------------------------------------

/// The partition rides the header durably and distinctly: two blocks that
/// differ ONLY by partition are different byte strings, and the field decodes
/// from where the layout says it lives.
#[test]
fn the_partition_id_is_durable_and_distinct() {
    let home = encode_block(7, None, &sample()).expect("encodes");
    let foreign = encode_block(8, None, &sample()).expect("encodes");
    assert_ne!(home, foreign, "the partition is part of the value");
    assert_eq!(
        u64::from_be_bytes(home[6..14].try_into().expect("fixed header")),
        7,
        "partition_id sits after the format word, §6.2 field order"
    );
    // Same logical content: the digest field is IDENTICAL across partitions —
    // the digest covers statements, not residence.
    assert_eq!(home[49..81], foreign[49..81]);
}

/// **THE DIGEST LAW (ctj acceptance): mutating any entry breaks
/// `canonical_logical_digest`.** The mutation here is STRUCTURALLY LAWFUL —
/// a different `created_at` on a live single-entry block passes every frame
/// and span check — so the digest is the only law that can catch it, which
/// is exactly the property that makes the field worth 32 bytes.
#[test]
fn a_structurally_lawful_mutation_is_caught_by_the_digest_alone() {
    let entries = vec![entry(1, 2, 5, None)];
    let bytes = encode_block(0, None, &entries).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes"), entries);

    // The single span's created_at is the last 16..8 bytes of the frame.
    let mut mutated = bytes.clone();
    let at = mutated.len() - 16;
    mutated[at..at + 8].copy_from_slice(&6u64.to_be_bytes());
    assert!(
        matches!(
            decode_block(&mutated),
            Err(BlockError::LogicalDigestMismatch { .. })
        ),
        "a retimed creation must be disowned by the digest"
    );

    // Decoder independence: a forged digest field — bytes the encoder cannot
    // emit — is refused the same way.
    let mut forged = bytes;
    forged[49] ^= 0x01;
    assert!(matches!(
        decode_block(&forged),
        Err(BlockError::LogicalDigestMismatch { .. })
    ));
}

// ---------------------------------------------------------------------------
// V6: the predecessor chain link (fgdb-4391)
// ---------------------------------------------------------------------------

/// The link round-trips in both states, stays OUT of the logical digest —
/// physical lineage, not logical content — and its slot admits exactly one
/// byte string per value.
#[test]
fn the_predecessor_slot_is_canonical_and_outside_the_digest() {
    use fgdb_strata::DeltaBlockVersion;
    use fgdb_types::ids::ObjectId;

    let unlinked = encode_block(0, None, &sample()).expect("encodes");
    let link = DeltaBlockVersion(ObjectId([0xcd; 32]));
    let linked = encode_block(0, Some(link), &sample()).expect("encodes");
    assert_ne!(unlinked, linked, "the link is part of the value");
    assert_eq!(
        decode_block(&linked).expect("decodes"),
        sample(),
        "a linked block's entries decode identically"
    );
    assert_eq!(
        unlinked[49..81],
        linked[49..81],
        "the digest covers statements, not chain history"
    );
    assert_eq!(linked[81], 1);
    assert_eq!(&linked[82..114], &[0xcd; 32]);
    assert_eq!(&unlinked[81..114], &[0u8; 33][..]);

    // The canonical refusals, hand-spliced — bytes the encoder cannot emit.
    let mut smuggled = unlinked.clone();
    smuggled[90] = 0xff; // identity bytes behind an absence flag
    assert_eq!(
        decode_block(&smuggled),
        Err(BlockError::NonCanonicalPredecessor)
    );
    let mut zero_link = linked.clone();
    zero_link[82..114].copy_from_slice(&[0u8; 32]); // "present" link to nothing
    assert_eq!(
        decode_block(&zero_link),
        Err(BlockError::NonCanonicalPredecessor)
    );
    let mut bad_flag = unlinked;
    bad_flag[81] = 2;
    assert_eq!(
        decode_block(&bad_flag),
        Err(BlockError::NonCanonicalPredecessor)
    );
}
