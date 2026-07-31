//! Laws of the tier-one delta-block format.
//!
//! These are the laws a BYTE LAYOUT can fail and an in-memory map cannot, which is
//! why the format is the honest first slice of Strata rather than a
//! `BTreeMap<(VId, RelationId, VId), _>` with the right answers on top. A map has
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
    AdjacencyEntry, BLOCK_FORMAT_V1, BLOCK_MAGIC, BlockError, MAX_BLOCK_ENTRIES, decode_block,
    encode_block, scan_neighbours,
};
use fgdb_types::{CommitSeq, VId};

const REL: RelationId = RelationId(1);
const OTHER_REL: RelationId = RelationId(2);

fn entry(src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

/// v1 -> {v2, v3}, v2 -> {v3}, with v1->v3 retired at 5.
fn sample() -> Vec<AdjacencyEntry> {
    vec![
        entry(1, 2, 1, None),
        entry(1, 3, 2, Some(5)),
        entry(2, 3, 3, None),
    ]
}

// ---------------------------------------------------------------------------
// Round trip and canonicality
// ---------------------------------------------------------------------------

/// Encode then decode returns exactly what went in.
#[test]
fn a_block_round_trips() {
    let entries = sample();
    let bytes = encode_block(&entries).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes"), entries);
}

/// EXACTLY ONE BYTE STRING PER VALUE: encoding the same entries twice produces
/// identical bytes, and no other input produces those bytes.
#[test]
fn encoding_is_deterministic_and_injective() {
    let a = encode_block(&sample()).expect("encodes");
    let b = encode_block(&sample()).expect("encodes");
    assert_eq!(a, b, "the same entries must produce the same bytes");

    let mut different = sample();
    different[1].created_at = CommitSeq(4);
    assert_ne!(
        encode_block(&different).expect("encodes"),
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
        encode_block(&entries),
        Err(BlockError::NonCanonicalOrder { at: 1 })
    );
}

/// The encoder refuses DUPLICATE keys — strictly ascending, not merely ascending.
///
/// Two entries for one `(src, relation, dst)` are two versions of one slot, and a
/// block cannot say which is current: that is a merge, and merging belongs to the
/// tier machinery that does not exist yet.
#[test]
fn the_encoder_refuses_duplicate_keys() {
    let entries = vec![entry(1, 2, 1, None), entry(1, 2, 3, None)];
    assert_eq!(
        encode_block(&entries),
        Err(BlockError::NonCanonicalOrder { at: 1 })
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
    let bytes = encode_block(&entries).expect("encodes");
    let swapped = encode_forged(&[entries[1], entries[0], entries[2]]);
    assert_eq!(swapped.len(), bytes.len(), "same shape, different order");
    assert_eq!(
        decode_block(&swapped),
        Err(BlockError::NonCanonicalOrder { at: 1 })
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
    out.extend_from_slice(&BLOCK_FORMAT_V1.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&e.src.0.to_be_bytes());
        out.extend_from_slice(&e.relation.0.to_be_bytes());
        out.extend_from_slice(&e.dst.0.to_be_bytes());
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

    let mut future = encode_block(&sample()).expect("encodes");
    future[4] = 0x00;
    future[5] = 0x09;
    assert_eq!(
        decode_block(&future),
        Err(BlockError::UnsupportedFormat { format: 9 })
    );
}

/// TRUNCATION is refused, and the error says how much was expected.
///
/// Every prefix of a valid block is tested, because "it refuses an empty tail" and
/// "it refuses any short read" are different claims and only the second is useful.
#[test]
fn every_truncation_is_refused() {
    let bytes = encode_block(&sample()).expect("encodes");
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
    let mut bytes = encode_block(&sample()).expect("encodes");
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
    let mut bytes = encode_block(&sample()).expect("encodes");
    bytes[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(
        decode_block(&bytes),
        Err(BlockError::ImplausibleEntryCount { declared: u32::MAX })
    );
    // And the bound is the declared one, not an accident of the input's length.
    bytes[6..10].copy_from_slice(&(MAX_BLOCK_ENTRIES + 1).to_be_bytes());
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
            encode_block(&entries),
            Err(BlockError::RetiredBeforeCreated {
                at: 0,
                created_at: CommitSeq(3),
                retired_at: CommitSeq(retired),
            })
        );
    }
    assert!(encode_block(&[entry(1, 2, 3, Some(4))]).is_ok());
}

/// Creation at sequence zero is refused: zero names the empty stream and can
/// never have created anything. This is also what makes zero usable as the
/// on-disk spelling of "live" without giving `None` two spellings.
#[test]
fn creation_at_the_empty_stream_is_refused() {
    assert_eq!(
        encode_block(&[entry(1, 2, 0, None)]),
        Err(BlockError::CreatedAtZero { at: 0 })
    );
}

/// A live entry round-trips as live — the zero sentinel is not mistaken for a
/// retirement at sequence zero.
#[test]
fn a_live_entry_round_trips_as_live() {
    let bytes = encode_block(&[entry(1, 2, 1, None)]).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes")[0].retired_at, None);
}

// ---------------------------------------------------------------------------
// Snapshot-visible scans
// ---------------------------------------------------------------------------

/// A scan sees an entry from its creation sequence onward, and not before.
#[test]
fn a_scan_sees_an_entry_from_its_creation_sequence() {
    let bytes = encode_block(&sample()).expect("encodes");
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
    let bytes = encode_block(&sample()).expect("encodes");
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(4)).expect("scans"),
        vec![VId(2), VId(3)],
        "visible through 4"
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(5)).expect("scans"),
        vec![VId(2)],
        "and gone AT 5, the sequence that retired it"
    );
}

/// A scan is scoped to its relation and its source. Without this a scan that
/// ignored either would pass every visibility law above.
#[test]
fn a_scan_is_scoped_to_its_source_and_relation() {
    let entries = vec![
        entry(1, 2, 1, None),
        AdjacencyEntry {
            src: VId(1),
            relation: OTHER_REL,
            dst: VId(9),
            created_at: CommitSeq(1),
            retired_at: None,
        },
        entry(2, 3, 1, None),
    ];
    let bytes = encode_block(&entries).expect("encodes");
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(9)).expect("scans"),
        vec![VId(2)]
    );
    assert_eq!(
        scan_neighbours(&bytes, VId(1), OTHER_REL, CommitSeq(9)).expect("scans"),
        vec![VId(9)]
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
    assert_eq!(
        scan_neighbours(&forged, VId(1), REL, CommitSeq(9)),
        Err(BlockError::NonCanonicalOrder { at: 1 }),
        "an out-of-order block must not scan"
    );

    let mut truncated = encode_block(&entries).expect("encodes");
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
    let bytes = encode_block(&sample()).expect("encodes");
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
    let bytes = encode_block(&[]).expect("encodes");
    assert_eq!(decode_block(&bytes).expect("decodes"), Vec::new());
    assert_eq!(
        scan_neighbours(&bytes, VId(1), REL, CommitSeq(1)).expect("scans"),
        Vec::<VId>::new()
    );
}
