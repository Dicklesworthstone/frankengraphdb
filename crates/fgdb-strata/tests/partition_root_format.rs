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
//! Block order is publication order. Visibility ranges may overlap because a later
//! tombstone repeats the old `created_at` of the version it retires; the later block
//! is the explicit precedence rule. Upper sequence frontiers never regress, while
//! gaps and overlapping lower bounds are both legal.

use fgdb_strata::root::{
    BlockRef, EdgeBirth, EdgeIdentityConflict, PartitionRoot, ROOT_FORMAT_V1, RootError,
    decode_root, encode_root, read_root, resolve_blocks, root_id, span_of,
};
use fgdb_strata::{AdjacencyEntry, block_id, encode_block};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
use std::cell::Cell;

const K_OID: [u8; 32] = [0x5a; 32];
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: fgdb_delta_types::RelationId = fgdb_delta_types::RelationId(1);

fn namespace() -> DatabaseSecurityNamespaceId {
    DatabaseSecurityNamespaceId([0x77; 32])
}

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
        src: fgdb_types::VId(src),
        relation: REL,
        dst: fgdb_types::VId(dst),
        eid: EId(eid),
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn identity_mismatch(expected: AdjacencyEntry, found: AdjacencyEntry) -> RootError {
    let birth = |entry: AdjacencyEntry| EdgeBirth {
        src: entry.src,
        relation: entry.relation,
        dst: entry.dst,
        created_at: entry.created_at,
    };
    RootError::EdgeIdentityMismatch {
        eid: expected.eid,
        conflict: Box::new(EdgeIdentityConflict {
            expected: birth(expected),
            found: birth(found),
        }),
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
        vertex_patches: vec![],
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
        vertex_patches: vec![],
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

/// OVERLAPPING RANGES ARE REQUIRED by tombstone supersede: the later statement of
/// one version repeats its original creation sequence and adds the retirement.
#[test]
fn overlapping_ranges_round_trip_in_publication_order() {
    let (id_a, bytes_a, span_a) = block(vec![entry(1, 2, 1, None)]);
    let (id_b, bytes_b, span_b) = block(vec![entry(1, 2, 1, Some(5))]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(5),
        blocks: vec![reference(id_a, span_a), reference(id_b, span_b)],
        vertex_patches: vec![],
    };
    let encoded = encode_root(&root).expect("truthful overlap is lawful");
    assert_eq!(decode_root(&encoded).expect("decodes"), root);

    let resolved = resolve_blocks(
        &K_OID,
        namespace(),
        &root,
        loader(vec![(id_a, bytes_a), (id_b, bytes_b)]),
    )
    .expect("both overlapping summaries match their blocks");
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&resolved, fgdb_types::VId(1), REL, CommitSeq(5))
            .expect("merges"),
        Vec::<fgdb_types::VId>::new(),
        "the later tombstone supplies the precedence rule"
    );
}

/// A later block's UPPER frontier cannot regress. Its lower bound may move back
/// under tombstone supersede, but rows were consumed in commit order.
#[test]
fn a_regressing_publication_frontier_is_refused() {
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
        vertex_patches: vec![],
    };
    assert_eq!(
        encode_root(&root),
        Err(RootError::BlockOrderRegression {
            earlier: 0,
            later: 1,
            earlier_last_seq: CommitSeq(5),
            later_last_seq: CommitSeq(1),
        })
    );
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
        vertex_patches: vec![],
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
        vertex_patches: vec![],
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
        vertex_patches: vec![],
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
        vertex_patches: vec![],
        ..inverted
    };
    assert_eq!(encode_root(&zero), Err(RootError::SequenceZero { at: 0 }));
}

/// The DECODER re-checks publication order independently, so hand-built bytes
/// cannot move the upper frontier backwards.
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
        vertex_patches: vec![],
    };
    let mut bytes = encode_root(&lawful).expect("encodes");
    // header(58) + the id(32) + first_seq(8) inside the first ref. Verified before
    // it is touched, because an offset slip here would silently patch a field this
    // law says nothing about.
    const HEADER: usize = 4 + 2 + 16 + 16 + 8 + 8 + 4;
    let first_last_seq = HEADER + 32 + 8;
    assert_eq!(
        u64::from_be_bytes(
            bytes[first_last_seq..first_last_seq + 8]
                .try_into()
                .expect("eight bytes")
        ),
        1,
        "the offset must land on the first block's last_seq"
    );
    bytes[first_last_seq..first_last_seq + 8].copy_from_slice(&3u64.to_be_bytes());
    assert_eq!(
        decode_root(&bytes),
        Err(RootError::BlockOrderRegression {
            earlier: 0,
            later: 1,
            earlier_last_seq: CommitSeq(3),
            later_last_seq: CommitSeq(2),
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

/// Resolution is a public admission boundary, not merely the second half of
/// `decode_root`. A caller can construct `PartitionRoot` directly, so the
/// resolver must reject an impossible publication order before trusting it to
/// choose block precedence or touching storage.
#[test]
fn resolution_refuses_an_unvalidated_publication_order_before_loading() {
    let (newer_id, newer_bytes, newer_span) = block(vec![entry(1, 2, 1, Some(9))]);
    let (older_id, older_bytes, older_span) = block(vec![entry(1, 2, 1, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(10),
        blocks: vec![
            reference(newer_id, newer_span),
            reference(older_id, older_span),
        ],
        vertex_patches: vec![],
    };
    let loads = Cell::new(0usize);
    let blocks = [(newer_id, newer_bytes), (older_id, older_bytes)];

    let result = resolve_blocks(&K_OID, namespace(), &root, |wanted| {
        loads.set(loads.get() + 1);
        blocks
            .iter()
            .find(|(id, _)| *id == wanted)
            .map(|(_, bytes)| bytes.clone())
    });

    assert_eq!(
        result,
        Err(RootError::BlockOrderRegression {
            earlier: 0,
            later: 1,
            earlier_last_seq: CommitSeq(9),
            later_last_seq: CommitSeq(1),
        })
    );
    assert_eq!(loads.get(), 0, "an invalid root must not reach storage");
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
        vertex_patches: vec![],
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
        vertex_patches: vec![],
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
        vertex_patches: vec![],
    };
    let bytes = encode_root(&root).expect("encodes");
    assert_eq!(decode_root(&bytes).expect("decodes"), root);
    assert_eq!(
        resolve_blocks(&K_OID, namespace(), &root, |_| None).expect("resolves"),
        Vec::<Vec<AdjacencyEntry>>::new()
    );
}

// ---------------------------------------------------------------------------
// Merging across blocks: tombstone supersede
// ---------------------------------------------------------------------------
//
// A block is immutable, so retiring an entry created in an EARLIER block cannot
// edit that block. The later block carries an entry for the same key whose
// interval states the retirement, and it supersedes. That choice is made in
// `merge_neighbours` and its rationale is recorded there; these laws are what
// hold the implementation to it.

/// A RETIREMENT IN A LATER BLOCK hides an edge created in an earlier one.
///
/// The whole point of the tombstone model, and unrepresentable within one block:
/// block A says the edge is live forever, block B says it ended at 5, and the
/// merged answer must be B's.
#[test]
fn a_later_block_retires_an_earlier_blocks_edge() {
    let early = vec![entry(1, 2, 1, None)];
    let late = vec![entry(1, 2, 1, Some(5))];

    assert_eq!(
        fgdb_strata::root::merge_neighbours(
            &[early.clone(), late.clone()],
            fgdb_types::VId(1),
            REL,
            CommitSeq(4)
        )
        .expect("merges"),
        vec![fgdb_types::VId(2)],
        "before the retirement it is still there"
    );
    assert_eq!(
        fgdb_strata::root::merge_neighbours(
            &[early.clone(), late.clone()],
            fgdb_types::VId(1),
            REL,
            CommitSeq(5)
        )
        .expect("merges"),
        Vec::<fgdb_types::VId>::new(),
        "at the retirement sequence it is gone — half-open, as within a block"
    );
    // And the earlier block ALONE still says it is live, so the merge is doing
    // the work rather than the block.
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&[early], fgdb_types::VId(1), REL, CommitSeq(5))
            .expect("merges"),
        vec![fgdb_types::VId(2)]
    );
}

/// A TOPOLOGY re-created under a fresh EId after retirement is visible again.
#[test]
fn a_re_creation_after_retirement_is_visible_again() {
    let blocks = vec![
        vec![edge(10, 1, 2, 1, Some(3))],
        vec![edge(20, 1, 2, 7, None)],
    ];
    for (as_of, expected) in [
        (1u64, vec![fgdb_types::VId(2)]),
        (3, Vec::new()),
        (6, Vec::new()),
        (7, vec![fgdb_types::VId(2)]),
    ] {
        assert_eq!(
            fgdb_strata::root::merge_neighbours(&blocks, fgdb_types::VId(1), REL, CommitSeq(as_of))
                .expect("merges"),
            expected,
            "at {as_of}"
        );
    }
}

/// SUPERSEDE IS PER EID IDENTITY: entries for the SAME stable edge identity
/// supersede by block order, and distinct EIds at one topology both survive.
///
/// The distinction is the whole model, and getting it wrong loses history. A
/// merge keyed on `dst` alone is wrong: it collapses distinct parallel EIds and a
/// later tombstone for one can erase the other. Keying by stable edge identity
/// preserves the tombstone while a fresh EId may later occupy the same topology.
#[test]
fn supersede_is_per_eid_identity_not_destination() {
    // SAME identity (both created at 1): the later block's retirement wins.
    let same = vec![vec![entry(1, 2, 1, None)], vec![entry(1, 2, 1, Some(4))]];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&same, fgdb_types::VId(1), REL, CommitSeq(5))
            .expect("merges"),
        Vec::<fgdb_types::VId>::new(),
        "a later block retiring the same version must win"
    );

    // DIFFERENT identities: both survive, and each answers for its own interval.
    let different = vec![
        vec![edge(10, 1, 2, 1, Some(3))],
        vec![edge(20, 1, 2, 7, None)],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&different, fgdb_types::VId(1), REL, CommitSeq(2))
            .expect("merges"),
        vec![fgdb_types::VId(2)],
        "the OLD version must still answer at a sequence when it was live"
    );
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&different, fgdb_types::VId(1), REL, CommitSeq(8))
            .expect("merges"),
        vec![fgdb_types::VId(2)],
        "and the new version answers at its own"
    );
}

/// fgdb-ghgt — EId is a permanently spent stable identity, not merely a merge
/// discriminator. Individually canonical blocks must not be able to change its
/// topology or introduce a later second birth. Both histories used to return a
/// plausible neighbour answer because merge filtered by adjacency before it
/// compared the cross-block identity.
#[test]
fn eid_topology_drift_and_nonoverlapping_rebirth_are_refused() {
    let source_drift = vec![
        vec![edge(10, 1, 2, 1, None)],
        vec![edge(10, 9, 2, 1, Some(4))],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&source_drift, VId(1), REL, CommitSeq(2),),
        Err(identity_mismatch(
            edge(10, 1, 2, 1, None),
            edge(10, 9, 2, 1, Some(4)),
        )),
    );

    let mut changed_relation = edge(10, 1, 2, 1, Some(4));
    changed_relation.relation = fgdb_delta_types::RelationId(9);
    let relation_drift = vec![vec![edge(10, 1, 2, 1, None)], vec![changed_relation]];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&relation_drift, VId(1), REL, CommitSeq(2),),
        Err(identity_mismatch(edge(10, 1, 2, 1, None), changed_relation,)),
    );

    let topology_drift = vec![
        vec![edge(10, 1, 2, 1, None)],
        vec![edge(10, 1, 3, 1, Some(4))],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&topology_drift, VId(1), REL, CommitSeq(2),),
        Err(identity_mismatch(
            edge(10, 1, 2, 1, None),
            edge(10, 1, 3, 1, Some(4)),
        )),
    );

    let nonoverlapping_rebirth = vec![
        vec![edge(10, 1, 2, 1, Some(3))],
        vec![edge(10, 1, 2, 5, None)],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&nonoverlapping_rebirth, VId(1), REL, CommitSeq(4),),
        Err(identity_mismatch(
            edge(10, 1, 2, 1, Some(3)),
            edge(10, 1, 2, 5, None),
        )),
    );
}

/// Once an exact EId birth is retired, later blocks may neither resurrect it nor
/// move its death. Last-block-wins is a precedence rule for a lawful tombstone,
/// not permission to rewrite an edge's lifetime.
#[test]
fn eid_retirement_is_irreversible_and_immutable() {
    let resurrection = vec![
        vec![edge(10, 1, 2, 1, Some(4))],
        vec![edge(10, 1, 2, 1, None)],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&resurrection, VId(1), REL, CommitSeq(5)),
        Err(fgdb_strata::root::RootError::EdgeRetirementMismatch {
            eid: EId(10),
            expected: Some(CommitSeq(4)),
            found: None,
        })
    );

    let retimed = vec![
        vec![edge(10, 1, 2, 1, None)],
        vec![edge(10, 1, 2, 1, Some(4))],
        vec![edge(10, 1, 2, 1, Some(5))],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&retimed, VId(1), REL, CommitSeq(2)),
        Err(fgdb_strata::root::RootError::EdgeRetirementMismatch {
            eid: EId(10),
            expected: Some(CommitSeq(4)),
            found: Some(CommitSeq(5)),
        })
    );
}

/// ANY SECOND BIRTH OF ONE EID IS REFUSED, not only an overlapping one.
///
/// The allocator slot is permanently spent at first creation. Whether a chosen
/// snapshot happens to intersect both intervals cannot decide if the durable
/// history is lawful.
#[test]
fn a_second_birth_of_one_eid_is_refused_at_every_snapshot() {
    let overlapping = vec![
        vec![edge(10, 1, 2, 1, Some(9))],
        vec![edge(10, 1, 2, 5, None)],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&overlapping, VId(1), REL, CommitSeq(2)),
        Err(identity_mismatch(
            edge(10, 1, 2, 1, Some(9)),
            edge(10, 1, 2, 5, None),
        ))
    );
}

/// Parallel EIds are not overlapping versions. Retiring one must leave the
/// other visible even when creation and tombstone statements cross block cuts.
#[test]
fn parallel_edges_survive_cross_block_merge_and_individual_retirement() {
    let blocks = vec![
        vec![edge(10, 1, 2, 1, None)],
        vec![edge(20, 1, 2, 2, None)],
        vec![edge(10, 1, 2, 1, Some(4))],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&blocks, fgdb_types::VId(1), REL, CommitSeq(3))
            .expect("parallel edges merge"),
        vec![fgdb_types::VId(2)],
        "two live EIds project to one neighbour"
    );
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&blocks, fgdb_types::VId(1), REL, CommitSeq(4))
            .expect("one retirement does not hide its peer"),
        vec![fgdb_types::VId(2)]
    );
}

/// THE SKIP RULE IS SOUND: dropping every block whose `first_seq` exceeds the
/// snapshot gives the IDENTICAL answer, not merely a faster one.
///
/// This is the root's whole payoff — carrying ranges is only worth it if a reader
/// may act on them. Swept across every sequence in and around the fixture rather
/// than probed once, because a skip rule that is wrong at exactly one boundary is
/// the way this fails.
#[test]
fn skipping_blocks_above_the_snapshot_gives_the_same_answer() {
    let (id_a, _, span_a) = block(vec![entry(1, 2, 1, None), entry(1, 3, 2, None)]);
    let (id_b, _, span_b) = block(vec![entry(1, 2, 1, Some(6)), entry(1, 4, 7, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![reference(id_a, span_a), reference(id_b, span_b)],
        vertex_patches: vec![],
    };
    let all = vec![
        vec![entry(1, 2, 1, None), entry(1, 3, 2, None)],
        vec![entry(1, 2, 1, Some(6)), entry(1, 4, 7, None)],
    ];

    for as_of in 1..=9u64 {
        let full =
            fgdb_strata::root::merge_neighbours(&all, fgdb_types::VId(1), REL, CommitSeq(as_of))
                .expect("merges");
        let kept: Vec<Vec<AdjacencyEntry>> =
            fgdb_strata::root::blocks_visible_at(&root, CommitSeq(as_of))
                .into_iter()
                .map(|index| all[index].clone())
                .collect();
        let skipped =
            fgdb_strata::root::merge_neighbours(&kept, fgdb_types::VId(1), REL, CommitSeq(as_of))
                .expect("merges");
        assert_eq!(full, skipped, "skipping changed the answer at {as_of}");
    }
}

/// The skip rule actually SKIPS something — otherwise the law above would hold
/// vacuously by reading every block every time.
#[test]
fn the_skip_rule_is_not_vacuous() {
    let (id_a, _, span_a) = block(vec![entry(1, 2, 1, None)]);
    let (id_b, _, span_b) = block(vec![entry(1, 3, 7, None)]);
    let root = PartitionRoot {
        graph: GRAPH,
        branch: BRANCH,
        partition: 0,
        published_at: CommitSeq(9),
        blocks: vec![reference(id_a, span_a), reference(id_b, span_b)],
        vertex_patches: vec![],
    };
    assert_eq!(
        fgdb_strata::root::blocks_visible_at(&root, CommitSeq(3)),
        vec![0],
        "the block that starts at 7 is skipped at snapshot 3"
    );
    assert_eq!(
        fgdb_strata::root::blocks_visible_at(&root, CommitSeq(7)),
        vec![0, 1],
        "and read once the snapshot reaches it"
    );
}

/// A merge over disjoint keys is a union, and it is scoped to its source and
/// relation like a single-block scan is.
#[test]
fn a_merge_unions_disjoint_keys_and_stays_scoped() {
    let blocks = vec![
        vec![entry(1, 2, 1, None), entry(2, 9, 1, None)],
        vec![entry(1, 3, 2, None)],
    ];
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&blocks, fgdb_types::VId(1), REL, CommitSeq(9))
            .expect("merges"),
        vec![fgdb_types::VId(2), fgdb_types::VId(3)]
    );
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&blocks, fgdb_types::VId(2), REL, CommitSeq(9))
            .expect("merges"),
        vec![fgdb_types::VId(9)],
        "another source's edges are not in this answer"
    );
}
