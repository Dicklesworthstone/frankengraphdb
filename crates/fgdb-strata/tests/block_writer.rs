//! Laws of the tier-D writer.
//!
//! The writer is the piece that makes Strata part of the database rather than a
//! format sitting beside it: it consumes the same `DeltaRow`s the commit stream
//! carries and emits the blocks a partition root names.
//!
//! **THE LAWS HERE ARE ABOUT WHAT IT REFUSES AND WHAT IT IGNORES**, because those
//! are the two ways a fold silently produces a wrong partition. A delete it cannot
//! resolve means the stream is not being replayed from the beginning — skipping it
//! would leave an edge live forever with nothing looking broken. A row family it
//! does not hold must be ignored EXPLICITLY, not folded into adjacency because it
//! happens to mention a vertex.
//!
//! Whether the writer's answers are RIGHT is not asked here. That is the
//! differential's job (`fgdb-sim`), and it drives this same writer — so these laws
//! and that agreement are about one object, not two implementations of one idea.

use fgdb_delta_types::{DeltaRow, ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_strata::decode_block;
use fgdb_strata::root::RootError;
use fgdb_strata::writer::{BlockWriter, WriteError};
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{BranchId, CanonicalScalar, CommitSeq, EId, GraphId, VId};

const K_OID: [u8; 32] = [0x5a; 32];
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const REL: RelationId = RelationId(1);

fn keys() -> (&'static [u8; 32], DatabaseSecurityNamespaceId) {
    (&K_OID, DatabaseSecurityNamespaceId([0x77; 32]))
}

fn writer() -> BlockWriter {
    BlockWriter::new(GRAPH, BRANCH, 0)
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

fn delete(eid: u128) -> DeltaRow {
    DeltaRow::DeleteEdge {
        eid: EId(eid),
        before_version: ObjectId([0u8; 32]),
    }
}

/// A creation and its retirement in one run become ONE entry with a finished
/// interval — the block carries the whole version, so nothing is superseded.
#[test]
fn a_create_and_delete_in_one_run_seal_as_one_finished_entry() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(3), &delete(10)).expect("deletes");
    assert_eq!(w.pending_len(), 1, "one key, one entry");

    let sealed = w.seal(keys()).expect("seals").expect("a block");
    let entries = decode_block(&sealed.bytes).expect("decodes");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].created_at, CommitSeq(1));
    assert_eq!(entries[0].retired_at, Some(CommitSeq(3)));
    assert_eq!(
        (sealed.first_seq, sealed.last_seq),
        (CommitSeq(1), CommitSeq(3)),
        "the block's range must reach the retirement"
    );
}

/// A retirement after its creation was sealed is a TOMBSTONE SUPERSEDE, so the
/// later block truthfully repeats the version's old `created_at`. Its range must
/// therefore overlap the creation block's range; publication cannot reject the
/// writer's own representation.
#[test]
fn a_retirement_after_a_seal_publishes_an_encodable_root() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.seal(keys()).expect("seals the creation");
    w.apply(keys(), CommitSeq(6), &delete(10)).expect("retires");

    let (root, blocks) = w.publish(keys(), CommitSeq(6)).expect("publishes");
    assert_eq!(
        root.blocks
            .iter()
            .map(|block| (block.first_seq, block.last_seq))
            .collect::<Vec<_>>(),
        vec![(CommitSeq(1), CommitSeq(1)), (CommitSeq(1), CommitSeq(6)),],
        "the later tombstone repeats the original creation sequence"
    );

    let encoded = fgdb_strata::root::encode_root(&root).expect("the writer's root is lawful");
    assert_eq!(
        fgdb_strata::root::decode_root(&encoded).expect("decodes"),
        root
    );

    let owned = blocks.clone();
    let resolved = fgdb_strata::root::resolve_blocks(keys().0, keys().1, &root, move |wanted| {
        owned
            .iter()
            .find(|block| block.block_id == wanted)
            .map(|block| block.bytes.clone())
    })
    .expect("the overlapping ranges remain truthful");
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&resolved, VId(1), REL, CommitSeq(5))
            .expect("merges before retirement"),
        vec![VId(2)]
    );
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&resolved, VId(1), REL, CommitSeq(6))
            .expect("merges at retirement"),
        Vec::<VId>::new()
    );
}

/// Retirement and a fresh EId at the same destination in ONE COMMIT remain
/// rootable. The old EId and its successor are distinct canonical block keys, so
/// the tombstone and new edge can share one block without losing either meaning.
#[test]
fn retirement_and_a_fresh_identity_at_one_sequence_remain_rootable() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.seal(keys()).expect("seals the creation");
    w.apply(keys(), CommitSeq(6), &delete(10)).expect("retires");
    w.apply(keys(), CommitSeq(6), &create(11, 1, 2))
        .expect("creates a fresh identity at the same sequence");

    let (root, blocks) = w.publish(keys(), CommitSeq(6)).expect("publishes");
    assert_eq!(
        root.blocks
            .iter()
            .map(|block| (block.first_seq, block.last_seq))
            .collect::<Vec<_>>(),
        vec![(CommitSeq(1), CommitSeq(1)), (CommitSeq(1), CommitSeq(6)),]
    );
    fgdb_strata::root::encode_root(&root).expect("equal publication frontiers are lawful");

    let decoded = blocks
        .iter()
        .map(|block| decode_block(&block.bytes).expect("decodes"))
        .collect::<Vec<_>>();
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&decoded, VId(1), REL, CommitSeq(5))
            .expect("merges before replacement"),
        vec![VId(2)]
    );
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&decoded, VId(1), REL, CommitSeq(6))
            .expect("merges at replacement"),
        vec![VId(2)],
        "the old version retires exactly when its successor becomes visible"
    );
}

/// PARALLEL EIDS SHARE A BLOCK without collapsing one another.
///
/// `(src, relation, dst)` is not an edge identity. EId is the unconditional
/// discriminator, so two live edges at one destination are two lawful canonical
/// keys and do not force a seal merely because their topology is equal.
#[test]
fn fresh_parallel_identities_share_one_pending_block() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(3), &create(11, 1, 2))
        .expect("creates a parallel edge");
    assert_eq!(
        w.sealed().len(),
        0,
        "equal topology with distinct EIds must not force a seal"
    );
    assert_eq!(
        w.pending_len(),
        2,
        "both stable edge identities are pending"
    );

    let (root, blocks) = w.publish(keys(), CommitSeq(9)).expect("publishes");
    assert_eq!(blocks.len(), 1);
    assert_eq!(root.blocks.len(), 1);
    let decoded = decode_block(&blocks[0].bytes).expect("decodes");
    assert_eq!(decoded.len(), 2, "neither parallel EId was collapsed");
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&[decoded], VId(1), REL, CommitSeq(9)).expect("merges"),
        vec![VId(2)],
        "neighbour projection remains set-valued"
    );
}

/// A DELETE THE WRITER CANNOT RESOLVE IS REFUSED.
///
/// Its live-edge map is rebuilt by replaying the stream from the beginning, so an
/// unresolvable delete means the stream is not being replayed from the beginning,
/// or a row is missing. Skipping would leave the edge live forever and nothing
/// about the resulting partition would look wrong.
#[test]
fn a_delete_of_an_unknown_edge_is_refused() {
    let mut w = writer();
    assert_eq!(
        w.apply(keys(), CommitSeq(1), &delete(99)),
        Err(WriteError::UnknownEdge { eid: EId(99) })
    );
    // And a double delete is the same failure: the first consumed the version.
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(2), &delete(10)).expect("deletes");
    assert_eq!(
        w.apply(keys(), CommitSeq(3), &delete(10)),
        Err(WriteError::UnknownEdge { eid: EId(10) })
    );
}

/// Rejecting a row is not an observation of that row's sequence. The caller may
/// still need to supply an earlier missing prefix before retrying the refusal.
#[test]
fn a_refused_delete_does_not_advance_the_stream_frontier() {
    let mut w = writer();
    assert_eq!(
        w.apply(keys(), CommitSeq(5), &delete(99)),
        Err(WriteError::UnknownEdge { eid: EId(99) })
    );
    assert!(
        w.apply(keys(), CommitSeq(4), &create(10, 1, 2)).is_ok(),
        "the refused future row must not consume sequence 5"
    );
}

/// THE CASCADE COMES FROM THE ROW, and every edge it names is retired.
#[test]
fn a_vertex_deletion_retires_its_declared_cascade() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(1), &create(11, 3, 1))
        .expect("creates");
    w.apply(keys(), CommitSeq(1), &create(12, 2, 3))
        .expect("creates");

    w.apply(
        keys(),
        CommitSeq(4),
        &DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: ObjectId([0u8; 32]),
            sorted_retired_incident_edges: vec![EId(10), EId(11)],
        },
    )
    .expect("cascades");

    w.seal(keys())
        .expect("seals")
        .expect("at least one descriptor block");
    let entries: Vec<_> = w
        .sealed()
        .iter()
        .flat_map(|sealed| decode_block(&sealed.bytes).expect("decodes"))
        .collect();
    let retired: Vec<_> = entries.iter().filter(|e| e.retired_at.is_some()).collect();
    assert_eq!(retired.len(), 2, "both named edges are retired");
    assert!(
        entries
            .iter()
            .any(|e| e.src == VId(2) && e.dst == VId(3) && e.retired_at.is_none()),
        "the unrelated edge survives"
    );
}

/// A cascade naming an edge the writer never saw is refused, like any other
/// unresolvable delete — a cascade image is not a licence to skip.
#[test]
fn a_cascade_naming_an_unknown_edge_is_refused() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    assert_eq!(
        w.apply(
            keys(),
            CommitSeq(4),
            &DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(10), EId(77)],
            },
        ),
        Err(WriteError::UnknownEdge { eid: EId(77) })
    );
    w.apply(keys(), CommitSeq(4), &delete(10))
        .expect("the rejected cascade must leave its earlier edge live");
}

/// The stream contract makes cascade identities strictly ordered and unique. A
/// duplicate would become unknown only after its first retirement, so it must be
/// detected before either copy changes the writer.
#[test]
fn a_duplicate_cascade_edge_is_refused_atomically() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    assert_eq!(
        w.apply(
            keys(),
            CommitSeq(4),
            &DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(10), EId(10)],
            },
        ),
        Err(WriteError::UnknownEdge { eid: EId(10) })
    );
    w.apply(keys(), CommitSeq(4), &delete(10))
        .expect("the duplicate refusal must leave the edge live");
}

/// Every retirement in a cascade is validated before the first one lands, and
/// a member created in the cascade's own commit FOLDS to nothing (fgdb-zeay):
/// a vertex created and deleted with its incident edge in one commit is
/// visible on no snapshot, so the durable image retires the older edges and
/// carries no entry for the same-commit one.
#[test]
fn a_cascade_folds_its_same_commit_members_and_retires_the_rest() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates the earlier edge");
    w.apply(keys(), CommitSeq(4), &create(11, 3, 1))
        .expect("creates the same-commit edge");

    w.apply(
        keys(),
        CommitSeq(4),
        &DeltaRow::DeleteVertex {
            vid: VId(1),
            before_version: ObjectId([0u8; 32]),
            sorted_retired_incident_edges: vec![EId(10), EId(11)],
        },
    )
    .expect("the same-commit member folds instead of poisoning the cascade");

    w.seal(keys())
        .expect("seals")
        .expect("at least one descriptor block");
    let entries: Vec<_> = w
        .sealed()
        .iter()
        .flat_map(|sealed| decode_block(&sealed.bytes).expect("decodes"))
        .collect();
    assert_eq!(entries.len(), 1, "only the older edge stages a tombstone");
    assert_eq!(entries[0].dst, VId(2));
    assert_eq!(entries[0].created_at, CommitSeq(1));
    assert_eq!(entries[0].retired_at, Some(CommitSeq(4)));
}

/// A row that can never become a canonical block is refused before it changes the
/// pending map or consumes its sequence.
#[test]
fn an_invalid_entry_is_refused_before_writer_mutation() {
    let mut w = writer();
    assert_eq!(
        w.apply(keys(), CommitSeq(0), &create(10, 1, 2)),
        Err(WriteError::Block(fgdb_strata::BlockError::CreatedAtZero {
            at: 0,
        }))
    );
    assert_eq!(w.pending_len(), 0, "the invalid row was never staged");
    assert!(
        w.apply(keys(), CommitSeq(1), &create(10, 1, 2)).is_ok(),
        "the refused zero sequence did not advance the frontier"
    );
}

/// NON-ADJACENCY FAMILIES ARE IGNORED, and the law names them.
///
/// Vertex creation, labels and properties are real and belong to structures this
/// tier does not hold. Folding them into an adjacency block because they mention a
/// vertex would be worse than not storing them: the block would carry entries no
/// edge ever justified.
#[test]
fn non_adjacency_rows_produce_no_entries() {
    let mut w = writer();
    for row in [
        DeltaRow::CreateVertex {
            vid: VId(1),
            birth_ordinal: 1,
            labels: vec![LabelId(10)],
            props: vec![],
            valid_time: None,
        },
        DeltaRow::LabelMembership {
            vid: VId(1),
            label: LabelId(11),
            before: false,
            after: true,
        },
        DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PropertyKeyId(100),
            before: None,
            after: Some(CanonicalScalar::Int(1)),
        },
    ] {
        w.apply(keys(), CommitSeq(1), &row).expect("folds");
    }
    assert_eq!(
        w.pending_len(),
        0,
        "no adjacency was implied by any of them"
    );
    assert_eq!(w.seal(keys()).expect("seals"), None, "and nothing to seal");
}

/// Rows must arrive in commit order: the writer is a fold over an ordered stream,
/// and out-of-order input would put entries in a block whose declared range no
/// longer bounds them.
#[test]
fn rows_must_arrive_in_commit_order() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(5), &create(10, 1, 2))
        .expect("creates");
    assert_eq!(
        w.apply(keys(), CommitSeq(4), &create(11, 1, 3)),
        Err(WriteError::SequenceNotAdvancing {
            previous: CommitSeq(5),
            offered: CommitSeq(4),
        })
    );
    // The same sequence is fine: one commit carries many rows.
    assert!(w.apply(keys(), CommitSeq(5), &create(11, 1, 3)).is_ok());
}

/// `publish` is the producer boundary, so it must refuse a root whose declared
/// publication is below a block it names instead of returning an invalid value for
/// some later encoder to discover.
#[test]
fn publication_before_the_last_block_is_refused_by_the_writer() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(5), &create(10, 1, 2))
        .expect("creates");
    assert_eq!(
        w.publish(keys(), CommitSeq(4)),
        Err(WriteError::Root(RootError::BlockAfterPublication {
            at: 0,
            last_seq: CommitSeq(5),
            published_at: CommitSeq(4),
        }))
    );

    let mut boundary = writer();
    boundary
        .apply(keys(), CommitSeq(5), &create(10, 1, 2))
        .expect("creates");
    assert!(
        boundary.publish(keys(), CommitSeq(5)).is_ok(),
        "publication at the exact upper frontier is legal"
    );
}

/// Sealing an empty run is a no-op rather than an empty block — an empty block
/// carries no information and a root naming one would be describing nothing.
#[test]
fn sealing_nothing_produces_no_block() {
    let mut w = writer();
    assert_eq!(w.seal(keys()).expect("seals"), None);
    let (root, blocks) = w.publish(keys(), CommitSeq(1)).expect("publishes");
    assert!(blocks.is_empty() && root.blocks.is_empty());
}

/// A published root is LAWFUL: it decodes, its publication frontiers ascend, and
/// every block it names resolves against the bytes the writer produced.
///
/// This is the writer's contract with the root format — the two were built
/// separately and this is the only place they are required to agree.
#[test]
fn a_published_root_resolves_against_the_writers_own_blocks() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(2), &create(11, 1, 3))
        .expect("creates");
    w.seal(keys()).expect("seals");
    w.apply(keys(), CommitSeq(4), &create(12, 2, 3))
        .expect("creates");

    let (root, blocks) = w.publish(keys(), CommitSeq(9)).expect("publishes");
    let encoded = fgdb_strata::root::encode_root(&root).expect("the root is lawful");
    assert_eq!(
        fgdb_strata::root::decode_root(&encoded).expect("decodes"),
        root
    );

    let owned = blocks.clone();
    let resolved = fgdb_strata::root::resolve_blocks(keys().0, keys().1, &root, move |wanted| {
        owned
            .iter()
            .find(|b| b.block_id == wanted)
            .map(|b| b.bytes.clone())
    })
    .expect("every named block resolves and matches its declared range");
    assert_eq!(resolved.len(), 2);
}

/// fgdb-3usp / fgdb-s50d — a re-create of a LIVE edge is refused, never overwritten:
/// overwriting the live map would strand the first version, retired by
/// nothing, answering every future snapshot. Retirement makes the edge absent,
/// but its allocator slot remains permanently spent; only a fresh EId is legal.
#[test]
fn a_double_create_is_refused_and_retirement_does_not_re_admit_the_eid() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");

    assert_eq!(
        w.apply(keys(), CommitSeq(2), &create(10, 1, 3)),
        Err(WriteError::EdgeAlreadyLive { eid: EId(10) }),
        "a second create for a live edge is not a version, it is the stream lying"
    );
    // State-atomic: the refusal moved nothing, so the fold continues undamaged.
    w.apply(keys(), CommitSeq(3), &create(11, 1, 4))
        .expect("the fold continues undamaged");

    // After a retirement the identity is absent but permanently spent.
    w.apply(keys(), CommitSeq(4), &delete(10)).expect("retires");
    assert_eq!(
        w.apply(keys(), CommitSeq(5), &create(10, 1, 3)),
        Err(WriteError::EdgeIdentitySpent { eid: EId(10) })
    );
    w.apply(keys(), CommitSeq(5), &create(12, 1, 3))
        .expect("a fresh EId remains admissible");
    let sealed = w.seal(keys()).expect("seals").expect("a block");
    let entries = decode_block(&sealed.bytes).expect("decodes");
    let live: Vec<_> = entries.iter().filter(|e| e.retired_at.is_none()).collect();
    assert_eq!(live.len(), 2);
    assert!(
        live.iter()
            .any(|e| e.eid == EId(12) && e.dst == VId(3) && e.created_at == CommitSeq(5)),
        "the fresh parallel identity answers"
    );
    assert!(
        !live.iter().any(|e| e.dst == VId(2)),
        "the retired first version answers nothing — the stranded-v1 defect"
    );
}

/// fgdb-zeay — created and deleted in ONE commit, an edge is visible on no
/// snapshot, so its durable image is no entry at all: the pending creation
/// folds away instead of poisoning the seal with an empty interval.
#[test]
fn a_same_commit_create_and_delete_folds_to_no_entry() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(5), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(5), &delete(10))
        .expect("the same-commit delete folds");
    assert_eq!(w.pending_len(), 0, "nothing remains of the folded edge");

    // The allocator slot stays spent even though the zero-length interval left
    // no block entry. A fresh identity at the same topology remains legal.
    assert_eq!(
        w.apply(keys(), CommitSeq(5), &create(10, 1, 2)),
        Err(WriteError::EdgeIdentitySpent { eid: EId(10) })
    );
    w.apply(keys(), CommitSeq(5), &create(11, 1, 2))
        .expect("fresh identity in the same commit");
    w.apply(keys(), CommitSeq(6), &create(12, 2, 3))
        .expect("creates");
    w.seal(keys())
        .expect("seals")
        .expect("at least one descriptor block");
    let entries: Vec<_> = w
        .sealed()
        .iter()
        .flat_map(|sealed| decode_block(&sealed.bytes).expect("decodes"))
        .collect();
    assert_eq!(entries.len(), 2, "the folded pair left nothing behind");
    assert!(
        entries.iter().all(|e| e.retired_at.is_none()),
        "no tombstones: the fold means there was never anything to retire"
    );
    assert!(
        entries
            .iter()
            .any(|e| e.dst == VId(2) && e.created_at == CommitSeq(5)),
        "the fresh edge survives under its own identity"
    );
}

/// The cascade contract is strict ascending-unique, and the preflight enforces
/// ALL of it: a non-adjacent duplicate or an unsorted list fails BEFORE any
/// member retires — never mid-loop, half-applied.
#[test]
fn a_non_adjacent_duplicate_or_unsorted_cascade_fails_before_any_mutation() {
    // Non-adjacent duplicate: [10, 20, 10] is an order violation at the
    // second 10 (20 > 10), caught at preflight.
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(2), &create(20, 1, 3))
        .expect("creates");
    assert_eq!(
        w.apply(
            keys(),
            CommitSeq(4),
            &DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(10), EId(20), EId(10)],
            },
        ),
        Err(WriteError::CascadeOrderViolation {
            previous: EId(20),
            found: EId(10),
        })
    );
    // Atomic: both edges are still live and retirable afterward.
    w.apply(keys(), CommitSeq(4), &delete(10))
        .expect("the refusal left the edge live");
    w.apply(keys(), CommitSeq(4), &delete(20))
        .expect("the refusal left the other edge live");

    // Unsorted (no duplicate): [20, 10] is the same contract breach.
    let mut w2 = writer();
    w2.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w2.apply(keys(), CommitSeq(2), &create(20, 1, 3))
        .expect("creates");
    assert_eq!(
        w2.apply(
            keys(),
            CommitSeq(4),
            &DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![EId(20), EId(10)],
            },
        ),
        Err(WriteError::CascadeOrderViolation {
            previous: EId(20),
            found: EId(10),
        })
    );
    w2.apply(keys(), CommitSeq(4), &delete(20))
        .expect("the refusal left the writer usable");
}
