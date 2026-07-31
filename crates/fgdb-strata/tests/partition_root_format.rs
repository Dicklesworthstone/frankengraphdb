//! Laws of the partition root — the object that says which blocks a partition is
//! made of.
//!
//! **THE ROOT IS ONLY WORTH HAVING IF ITS SUMMARY CAN BE TRUSTED.** Its whole
//! performance argument is that a reader can skip a block without decoding it,
//! based on the range the root declares. That means a root which UNDERSTATES a
//! range is the dangerous case: the reader skips a block that mattered, gets a
//! wrong answer, and nothing about the block itself looks wrong. So `resolve_blocks`
//! checks two different things, and the tests below separate them — that the bytes
//! are the block the root named (identity), and that the block spans what the root
//! said (range). Either check alone leaves a lie the other would catch.
//!
//! Ranges are ascending and NON-OVERLAPPING because two blocks claiming one
//! sequence would make a merge ambiguous — a reader assembling state at that
//! sequence would have two sources and no rule to choose. Gaps are fine: a
//! partition that received no commits over a stretch of the stream has none.

use fgdb_strata::root::{
    BlockRef, PartitionRoot, ROOT_FORMAT_V1, RootError, decode_root, encode_root, read_root,
    resolve_blocks, root_id, span_of,
};
use fgdb_strata::{AdjacencyEntry, block_id, encode_block};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, GraphId};

const K_OID: [u8; 32] = [0x5a; 32];
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: fgdb_delta_types::RelationId = fgdb_delta_types::RelationId(1);

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([0x77; 32])
}

fn entry(src: u128, dst: u128, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: fgdb_types::VId(src),
        relation: REL,
        dst: fgdb_types::VId(dst),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

/// A block plus its identity and the span it actually covers.
fn block(entries: Vec<AdjacencyEntry>) -> (ObjectId, Vec<u8>, (CommitSeq, CommitSeq)) {
    let bytes = encode_block(&entries).expect("encodes");
    let id = block_id(&K_OID, namespace(), &bytes);
    let span = span_of(&entries).expect("non-empty");
    (id, bytes, span)
}

fn reference(id: ObjectId, span: (CommitSeq, CommitSeq)) -> BlockRef {
    BlockRef {
        block_id: id,
        first_seq: span.0,
        last_seq: span.1,
    }
}

/// Two blocks with disjoint ascending ranges, and a root over them.
fn sample() -> (PartitionRoot, Vec<(ObjectId, Vec<u8>)>) {
    let (id_a, bytes_a, span_a) = block(vec![entry(1, 2, 1, None), entry(1, 3, 2, None)]);
    let (id_b, bytes_b, span_b) = block(vec![entry(2, 3, 5, None), entry(2, 4, 6, Some(7))]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![reference(id_a, span_a), reference(id_b, span_b)],
    };
    (root, vec![(id_a, bytes_a), (id_b, bytes_b)])
}

fn loader(blocks: Vec<(ObjectId, Vec<u8>)>) -> impl FnMut(ObjectId) -> Option<Vec<u8>> {
    move |wanted| {
        blocks
            .iter()
            .find(|(id, _)| *id == wanted)
            .map(|(_, bytes)| bytes.clone())
    }
}

// ---------------------------------------------------------------------------
// Round trip, canonicality, framing
// ---------------------------------------------------------------------------

#[test]
fn a_root_round_trips() {
    let (root, _) = sample();
    let bytes = encode_root(&root).expect("encodes");
    assert_eq!(decode_root(&bytes).expect("decodes"), root);
}

/// EVERY HEADER FIELD SURVIVES THE ROUND TRIP, checked field by field with
/// distinct values.
///
/// A header of same-width neighbours is exactly where an offset slip hides: the
/// first draft of this decoder read `published_at` out of the partition field and
/// the block count eight bytes early, and a round trip using zeros or equal values
/// for those fields would have passed anyway.
#[test]
fn every_header_field_round_trips_distinctly() {
    let (_, blocks) = sample();
    let (id, bytes_a, span) = block(vec![entry(1, 2, 3, None)]);
    let _ = (blocks, bytes_a);
    let root = PartitionRoot {
        graph: GraphId(0x1111_2222_3333_4444_5555_6666_7777_8888),
        branch: BranchId(0x9999_aaaa_bbbb_cccc_dddd_eeee_ffff_0000),
        partition: 0x0102_0304_0506_0708,
        published_at: CommitSeq(0x1122_3344_5566_7788),
        blocks: vec![reference(id, span)],
    };
    let decoded = decode_root(&encode_root(&root).expect("encodes")).expect("decodes");
    assert_eq!(decoded.graph, root.graph);
    assert_eq!(decoded.branch, root.branch);
    assert_eq!(decoded.partition, root.partition);
    assert_eq!(decoded.published_at, root.published_at);
    assert_eq!(decoded.blocks, root.blocks);
}

#[test]
fn foreign_bytes_and_future_versions_are_refused_distinctly() {
    assert_eq!(decode_root(b"nope"), Err(RootError::NotARoot));
    let (root, _) = sample();
    let mut future = encode_root(&root).expect("encodes");
    future[4] = 0;
    future[5] = 7;
    assert_eq!(
        decode_root(&future),
        Err(RootError::UnsupportedFormat { format: 7 })
    );
    assert_eq!(ROOT_FORMAT_V1, 1);
}

#[test]
fn every_truncation_and_any_trailing_byte_is_refused() {
    let (root, _) = sample();
    let bytes = encode_root(&root).expect("encodes");
    for cut in 0..bytes.len() {
        assert!(
            decode_root(&bytes[..cut]).is_err(),
            "a {cut}-byte prefix must not decode"
        );
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert_eq!(
        decode_root(&extra),
        Err(RootError::TrailingBytes { extra: 1 })
    );
    assert!(decode_root(&bytes).is_ok());
}

// ---------------------------------------------------------------------------
// Range laws
// ---------------------------------------------------------------------------

/// OVERLAPPING RANGES ARE REFUSED — the ambiguity is made unrepresentable rather
/// than resolved at read time.
#[test]
fn overlapping_ranges_are_refused() {
    let (id_a, _, _) = block(vec![entry(1, 2, 1, None), entry(1, 3, 5, None)]);
    let (id_b, _, _) = block(vec![entry(2, 3, 4, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![
            reference(id_a, (CommitSeq(1), CommitSeq(5))),
            // Starts at 4, inside the previous block's 1..5.
            reference(id_b, (CommitSeq(4), CommitSeq(6))),
        ],
    };
    assert_eq!(
        encode_root(&root),
        Err(RootError::OverlappingRanges {
            earlier: 0,
            later: 1
        })
    );
}

/// Blocks must ASCEND. A descending pair is caught by the same rule, since the
/// later block must start strictly after the earlier one ended.
#[test]
fn descending_blocks_are_refused() {
    let (id_a, _, _) = block(vec![entry(1, 2, 5, None)]);
    let (id_b, _, _) = block(vec![entry(2, 3, 1, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![
            reference(id_a, (CommitSeq(5), CommitSeq(5))),
            reference(id_b, (CommitSeq(1), CommitSeq(1))),
        ],
    };
    assert!(matches!(
        encode_root(&root),
        Err(RootError::OverlappingRanges { .. })
    ));
}

/// A GAP between blocks is ALLOWED. Without this law the overlap rule could be
/// implemented as "contiguous", which would make a partition that received no
/// commits over a stretch of the stream unrepresentable.
#[test]
fn a_gap_between_blocks_is_allowed() {
    let (id_a, _, _) = block(vec![entry(1, 2, 1, None)]);
    let (id_b, _, _) = block(vec![entry(2, 3, 40, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(99),
        blocks: vec![
            reference(id_a, (CommitSeq(1), CommitSeq(1))),
            reference(id_b, (CommitSeq(40), CommitSeq(40))),
        ],
    };
    assert!(encode_root(&root).is_ok(), "gaps are legal");
}

/// A block reaching PAST the root's publication is refused: the root is written
/// after the blocks it names, so a block claiming a later sequence means either
/// the root is stale or the range is a lie.
#[test]
fn a_block_after_publication_is_refused() {
    let (id, _, _) = block(vec![entry(1, 2, 1, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(3),
        blocks: vec![reference(id, (CommitSeq(1), CommitSeq(4)))],
    };
    assert_eq!(
        encode_root(&root),
        Err(RootError::BlockAfterPublication {
            at: 0,
            last_seq: CommitSeq(4),
            published_at: CommitSeq(3),
        })
    );
    // The boundary itself is legal: a root published at the sequence its last
    // block reaches is the ordinary case.
    let ok = PartitionRoot {
        published_at: CommitSeq(4),
        ..root
    };
    assert!(encode_root(&ok).is_ok());
}

/// An inverted range and a zero sequence are both refused.
#[test]
fn inverted_and_zero_ranges_are_refused() {
    let (id, _, _) = block(vec![entry(1, 2, 1, None)]);
    let inverted = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![reference(id, (CommitSeq(5), CommitSeq(3)))],
    };
    assert_eq!(
        encode_root(&inverted),
        Err(RootError::InvertedRange {
            at: 0,
            first_seq: CommitSeq(5),
            last_seq: CommitSeq(3),
        })
    );
    let zero = PartitionRoot {
        blocks: vec![reference(id, (CommitSeq(0), CommitSeq(3)))],
        ..inverted
    };
    assert_eq!(encode_root(&zero), Err(RootError::SequenceZero { at: 0 }));
}

/// The DECODER re-checks the range laws independently, so a hand-built root cannot
/// smuggle in an overlap the encoder would never emit.
#[test]
fn the_decoder_re_checks_the_range_laws() {
    let (id_a, _, _) = block(vec![entry(1, 2, 1, None)]);
    let (id_b, _, _) = block(vec![entry(2, 3, 2, None)]);
    let lawful = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![
            reference(id_a, (CommitSeq(1), CommitSeq(1))),
            reference(id_b, (CommitSeq(2), CommitSeq(2))),
        ],
    };
    let mut bytes = encode_root(&lawful).expect("encodes");
    // header(58) + one ref(48) + the id(32) inside the second ref. Verified before
    // it is touched, because an offset slip here would silently patch a field this
    // law says nothing about — and the first version of this test did exactly that,
    // rewriting the FIRST block's first_seq to the value it already held.
    const HEADER: usize = 4 + 2 + 16 + 16 + 8 + 8 + 4;
    const REF: usize = 32 + 8 + 8;
    let second_first_seq = HEADER + REF + 32;
    assert_eq!(
        u64::from_be_bytes(
            bytes[second_first_seq..second_first_seq + 8]
                .try_into()
                .expect("eight bytes")
        ),
        2,
        "the offset must land on the second block's first_seq"
    );
    bytes[second_first_seq..second_first_seq + 8].copy_from_slice(&1u64.to_be_bytes());
    assert_eq!(
        decode_root(&bytes),
        Err(RootError::OverlappingRanges {
            earlier: 0,
            later: 1
        })
    );
}

// ---------------------------------------------------------------------------
// Identity and resolution
// ---------------------------------------------------------------------------

#[test]
fn a_root_has_a_derived_identity_and_read_root_enforces_it() {
    let (root, _) = sample();
    let bytes = encode_root(&root).expect("encodes");
    let id = root_id(&K_OID, namespace(), &bytes);
    assert_eq!(
        read_root(&K_OID, namespace(), &bytes, id).expect("reads"),
        root
    );

    let other = PartitionRoot {
        published_at: CommitSeq(10),
        ..root
    };
    let other_bytes = encode_root(&other).expect("encodes");
    let actual = root_id(&K_OID, namespace(), &other_bytes);
    assert_eq!(
        read_root(&K_OID, namespace(), &other_bytes, id),
        Err(RootError::IdentityMismatch {
            expected: id,
            actual
        })
    );
}

/// Resolving a well-formed root loads every block and proves each one.
#[test]
fn resolving_loads_every_named_block() {
    let (root, blocks) = sample();
    let loaded = resolve_blocks(&K_OID, namespace(), &root, loader(blocks)).expect("resolves");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].len(), 2);
    assert_eq!(loaded[1].len(), 2);
}

/// A block the loader cannot supply is a typed failure naming its position.
#[test]
fn a_missing_block_is_reported_with_its_position() {
    let (root, blocks) = sample();
    let only_first = vec![blocks[0].clone()];
    assert!(matches!(
        resolve_blocks(&K_OID, namespace(), &root, loader(only_first)),
        Err(RootError::Block { at: 1, .. })
    ));
}

/// THE LOAD-BEARING RESOLUTION LAW: a root that UNDERSTATES a block's range is
/// refused.
///
/// This is the lie that identity alone cannot catch. The block is exactly the block
/// the root names — its bytes hash to the declared id — and the root simply says it
/// covers less than it does. A reader trusting that summary would skip it at a
/// sequence where it mattered and return a wrong answer with nothing looking
/// broken.
#[test]
fn a_root_that_understates_a_block_range_is_refused() {
    let entries = vec![entry(1, 2, 1, None), entry(1, 3, 8, None)];
    let (id, bytes, span) = block(entries);
    assert_eq!(span, (CommitSeq(1), CommitSeq(8)));

    let lying = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        // Claims the block stops at 2, though it reaches 8.
        blocks: vec![reference(id, (CommitSeq(1), CommitSeq(2)))],
    };
    assert_eq!(
        resolve_blocks(&K_OID, namespace(), &lying, loader(vec![(id, bytes)])),
        Err(RootError::BlockRangeMismatch {
            at: 0,
            declared: (CommitSeq(1), CommitSeq(2)),
            actual: (CommitSeq(1), CommitSeq(8)),
        })
    );
}

/// A RETIREMENT extends a block's span, because a reader deciding whether to skip
/// the block at that sequence needs to know the retirement is in there.
#[test]
fn a_retirement_extends_the_span() {
    let entries = vec![entry(1, 2, 3, Some(9))];
    assert_eq!(
        span_of(&entries),
        Some((CommitSeq(3), CommitSeq(9))),
        "the span must reach the retirement, not stop at the creation"
    );
    assert_eq!(span_of(&[]), None, "an empty block spans nothing");
}

/// Resolution ALSO enforces block identity: bytes that are a different block are
/// refused even when the declared range happens to fit.
#[test]
fn resolution_enforces_block_identity_too() {
    let (id_a, _, span_a) = block(vec![entry(1, 2, 1, None)]);
    let (id_b, bytes_b, _) = block(vec![entry(9, 9, 1, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![reference(id_a, span_a)],
    };
    // The loader hands back a DIFFERENT block for the requested identity.
    let swapped = move |_wanted| Some(bytes_b.clone());
    assert!(
        matches!(
            resolve_blocks(&K_OID, namespace(), &root, swapped),
            Err(RootError::Block { at: 0, .. })
        ),
        "a store returning the wrong bytes must be caught, whatever the range says"
    );
    assert_ne!(id_a, id_b);
}

/// An empty root is valid and resolves to nothing — a partition that exists and
/// holds no blocks yet.
#[test]
fn an_empty_root_is_valid() {
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(1),
        blocks: Vec::new(),
    };
    let bytes = encode_root(&root).expect("encodes");
    assert_eq!(decode_root(&bytes).expect("decodes"), root);
    assert_eq!(
        resolve_blocks(&K_OID, namespace(), &root, |_| None).expect("resolves"),
        Vec::<Vec<AdjacencyEntry>>::new()
    );
}
