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
    create_with_props(eid, src, dst, vec![])
}

fn create_with_props(
    eid: u128,
    src: u128,
    dst: u128,
    props: Vec<(PropertyKeyId, CanonicalScalar)>,
) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(src),
        relation: REL,
        dst: VId(dst),
        canonical_key: None,
        props,
        valid_time: None,
    }
}

fn delete(eid: u128) -> DeltaRow {
    DeltaRow::DeleteEdge {
        eid: EId(eid),
        before_version: ObjectId([0u8; 32]),
    }
}

/// A bare vertex creation, so cascade fixtures satisfy the create-once law the
/// oracle and (since the vertex fold) this writer both enforce.
fn create_vertex_bare(vid: u128) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: 900 + vid as u64,
        labels: vec![],
        props: vec![],
        valid_time: None,
    }
}

fn seed_vertices(w: &mut BlockWriter, seq: u64, vids: &[u128]) {
    let seq = seq.max(1);
    for vid in vids {
        if !w.is_vertex_live(VId(*vid)) {
            w.apply(keys(), CommitSeq(seq), &create_vertex_bare(*vid))
                .expect("seed vertex");
        }
    }
}

fn apply_edge(
    w: &mut BlockWriter,
    seq: u64,
    eid: u128,
    src: u128,
    dst: u128,
) -> Result<(), WriteError> {
    seed_vertices(w, seq, &[src, dst]);
    w.apply(keys(), CommitSeq(seq), &create(eid, src, dst))
}

/// A CreateEdge whose endpoints the fold does not hold is refused
/// (fgdb-7g91). Format-invalid rows still fail format first.
#[test]
fn a_create_edge_with_a_missing_endpoint_is_refused() {
    let mut w = writer();
    assert_eq!(
        w.apply(keys(), CommitSeq(1), &create(10, 1, 2)),
        Err(WriteError::DanglingEndpoint {
            eid: EId(10),
            endpoint: VId(1)
        })
    );
    assert_eq!(w.pending_len(), 0, "the dangling row was never staged");
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("seeds src");
    assert_eq!(
        w.apply(keys(), CommitSeq(1), &create(10, 1, 2)),
        Err(WriteError::DanglingEndpoint {
            eid: EId(10),
            endpoint: VId(2)
        })
    );
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(2))
        .expect("seeds dst");
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("both endpoints live");
}

/// A creation and its retirement in one run become ONE entry with a finished
/// interval — the block carries the whole version, so nothing is superseded.
#[test]
fn a_create_and_delete_in_one_run_seal_as_one_finished_entry() {
    let mut w = writer();
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
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
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    w.seal(keys()).expect("seals the creation");
    w.apply(keys(), CommitSeq(6), &delete(10)).expect("retires");

    let (root, blocks, _patches) = w.publish(keys(), CommitSeq(6)).expect("publishes");
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
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    w.seal(keys()).expect("seals the creation");
    w.apply(keys(), CommitSeq(6), &delete(10)).expect("retires");
    apply_edge(&mut w, 6, 11, 1, 2).expect("creates a fresh identity at the same sequence");

    let (root, blocks, _patches) = w.publish(keys(), CommitSeq(6)).expect("publishes");
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
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    apply_edge(&mut w, 3, 11, 1, 2).expect("creates a parallel edge");
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

    let (root, blocks, _patches) = w.publish(keys(), CommitSeq(9)).expect("publishes");
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
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
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
        apply_edge(&mut w, 4, 10, 1, 2).is_ok(),
        "the refused future row must not consume sequence 5"
    );
}

/// THE CASCADE COMES FROM THE ROW, and every edge it names is retired.
#[test]
fn a_vertex_deletion_retires_its_declared_cascade() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    apply_edge(&mut w, 1, 11, 3, 1).expect("creates");
    apply_edge(&mut w, 1, 12, 2, 3).expect("creates");

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

/// A cascade that is not the live incident set is refused (fgdb-17ht).
/// An extra unknown eid is an overcount, not a licence to skip.
#[test]
fn a_cascade_naming_an_unknown_edge_is_refused() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
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
        Err(WriteError::CascadeImageMismatch {
            vid: VId(1),
            declared: vec![EId(10), EId(77)],
            actual: vec![EId(10)],
        })
    );
    w.apply(keys(), CommitSeq(4), &delete(10))
        .expect("the rejected cascade must leave its earlier edge live");
}

/// An undercount would leave a dangling edge after the vertex is gone.
#[test]
fn a_cascade_undercount_is_refused_and_leaves_the_edge_live() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    assert_eq!(
        w.apply(
            keys(),
            CommitSeq(4),
            &DeltaRow::DeleteVertex {
                vid: VId(1),
                before_version: ObjectId([0u8; 32]),
                sorted_retired_incident_edges: vec![],
            },
        ),
        Err(WriteError::CascadeImageMismatch {
            vid: VId(1),
            declared: vec![],
            actual: vec![EId(10)],
        })
    );
    w.apply(keys(), CommitSeq(4), &delete(10))
        .expect("the undercount refusal left the incident edge live");
    assert!(
        w.is_vertex_live(VId(1)),
        "the undercount refusal left the vertex live"
    );
}

/// The stream contract makes cascade identities strictly ordered and unique. A
/// duplicate would become unknown only after its first retirement, so it must be
/// detected before either copy changes the writer.
#[test]
fn a_duplicate_cascade_edge_is_refused_atomically() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
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
        Err(WriteError::CascadeImageMismatch {
            vid: VId(1),
            declared: vec![EId(10), EId(10)],
            actual: vec![EId(10)],
        })
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
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates the earlier edge");
    apply_edge(&mut w, 4, 11, 3, 1).expect("creates the same-commit edge");

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
        apply_edge(&mut w, 1, 10, 1, 2).is_ok(),
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
    apply_edge(&mut w, 5, 10, 1, 2).expect("creates");
    seed_vertices(&mut w, 5, &[3]);
    assert_eq!(
        w.apply(keys(), CommitSeq(4), &create(11, 1, 3)),
        Err(WriteError::SequenceNotAdvancing {
            previous: CommitSeq(5),
            offered: CommitSeq(4),
        })
    );
    // The same sequence is fine: one commit carries many rows.
    assert!(apply_edge(&mut w, 5, 11, 1, 3).is_ok());
}

/// `publish` is the producer boundary, so it must refuse a root whose declared
/// publication is below a block it names instead of returning an invalid value for
/// some later encoder to discover.
#[test]
fn publication_before_the_last_block_is_refused_by_the_writer() {
    let mut w = writer();
    apply_edge(&mut w, 5, 10, 1, 2).expect("creates");
    assert_eq!(
        w.publish(keys(), CommitSeq(4)),
        Err(WriteError::Root(RootError::BlockAfterPublication {
            at: 0,
            last_seq: CommitSeq(5),
            published_at: CommitSeq(4),
        }))
    );

    let mut boundary = writer();
    apply_edge(&mut boundary, 5, 10, 1, 2).expect("creates");
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
    let (root, blocks, _patches) = w.publish(keys(), CommitSeq(1)).expect("publishes");
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
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    apply_edge(&mut w, 2, 11, 1, 3).expect("creates");
    w.seal(keys()).expect("seals");
    apply_edge(&mut w, 4, 12, 2, 3).expect("creates");

    let (root, blocks, _patches) = w.publish(keys(), CommitSeq(9)).expect("publishes");
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
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    seed_vertices(&mut w, 1, &[3, 4]);

    assert_eq!(
        w.apply(keys(), CommitSeq(2), &create(10, 1, 3)),
        Err(WriteError::EdgeAlreadyLive { eid: EId(10) }),
        "a second create for a live edge is not a version, it is the stream lying"
    );
    // State-atomic: the refusal moved nothing, so the fold continues undamaged.
    apply_edge(&mut w, 3, 11, 1, 4).expect("the fold continues undamaged");

    // After a retirement the identity is absent but permanently spent.
    w.apply(keys(), CommitSeq(4), &delete(10)).expect("retires");
    assert_eq!(
        w.apply(keys(), CommitSeq(5), &create(10, 1, 3)),
        Err(WriteError::EdgeIdentitySpent { eid: EId(10) })
    );
    apply_edge(&mut w, 5, 12, 1, 3).expect("a fresh EId remains admissible");
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
    apply_edge(&mut w, 5, 10, 1, 2).expect("creates");
    w.apply(keys(), CommitSeq(5), &delete(10))
        .expect("the same-commit delete folds");
    assert_eq!(w.pending_len(), 0, "nothing remains of the folded edge");

    // The allocator slot stays spent even though the zero-length interval left
    // no block entry. A fresh identity at the same topology remains legal.
    assert_eq!(
        w.apply(keys(), CommitSeq(5), &create(10, 1, 2)),
        Err(WriteError::EdgeIdentitySpent { eid: EId(10) })
    );
    apply_edge(&mut w, 5, 11, 1, 2).expect("fresh identity in the same commit");
    apply_edge(&mut w, 6, 12, 2, 3).expect("creates");
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

/// The cascade contract is exact equality with the live incident set
/// (fgdb-17ht). A non-adjacent duplicate or an unsorted list is not that
/// set and fails BEFORE any member retires.
#[test]
fn a_non_adjacent_duplicate_or_unsorted_cascade_fails_before_any_mutation() {
    // Non-adjacent duplicate: [10, 20, 10] is not the incident set [10, 20].
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w, 1, 10, 1, 2).expect("creates");
    apply_edge(&mut w, 2, 20, 1, 3).expect("creates");
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
        Err(WriteError::CascadeImageMismatch {
            vid: VId(1),
            declared: vec![EId(10), EId(20), EId(10)],
            actual: vec![EId(10), EId(20)],
        })
    );
    // Atomic: both edges are still live and retirable afterward.
    w.apply(keys(), CommitSeq(4), &delete(10))
        .expect("the refusal left the edge live");
    w.apply(keys(), CommitSeq(4), &delete(20))
        .expect("the refusal left the other edge live");

    // Unsorted (no duplicate): [20, 10] is not the incident set [10, 20].
    let mut w2 = writer();
    w2.apply(keys(), CommitSeq(1), &create_vertex_bare(1))
        .expect("creates the vertex");
    apply_edge(&mut w2, 1, 10, 1, 2).expect("creates");
    apply_edge(&mut w2, 2, 20, 1, 3).expect("creates");
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
        Err(WriteError::CascadeImageMismatch {
            vid: VId(1),
            declared: vec![EId(20), EId(10)],
            actual: vec![EId(10), EId(20)],
        })
    );
    w2.apply(keys(), CommitSeq(4), &delete(20))
        .expect("the refusal left the writer usable");
}

// ---------------------------------------------------------------------------
// Vertex-row retirement (fgdb-w3-tier-d-ctj increment over fgdb-3xoi)
// ---------------------------------------------------------------------------

fn create_vertex_row(vid: u128, ordinal: u64) -> DeltaRow {
    DeltaRow::CreateVertex {
        vid: VId(vid),
        birth_ordinal: ordinal,
        labels: vec![LabelId(3), LabelId(5)],
        props: vec![(PropertyKeyId(7), CanonicalScalar::Int(1815))],
        valid_time: None,
    }
}

fn delete_vertex_row(vid: u128, cascade: Vec<EId>) -> DeltaRow {
    DeltaRow::DeleteVertex {
        vid: VId(vid),
        before_version: ObjectId([0u8; 32]),
        sorted_retired_incident_edges: cascade,
    }
}

/// A vertex retirement after its creation was sealed is a TOMBSTONE
/// SUPERSEDE, exactly like an edge's: the later patch restates the EXACT
/// birth — ordinal, labels, properties — with the interval closed, and the
/// merged answer flips from the full row to nothing at the retirement.
#[test]
fn a_deleted_vertex_is_invisible_and_its_tombstone_restates_the_birth() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_row(1, 0))
        .expect("creates");
    let first = w
        .seal_vertices(keys())
        .expect("seals")
        .expect("a patch exists");
    w.apply(keys(), CommitSeq(6), &delete_vertex_row(1, vec![]))
        .expect("retires");
    let second = w
        .seal_vertices(keys())
        .expect("seals the tombstone")
        .expect("a tombstone patch exists");

    let first_rows = fgdb_strata::vertex::decode_patch(&first.bytes).expect("decodes");
    let second_rows = fgdb_strata::vertex::decode_patch(&second.bytes).expect("decodes");
    assert_eq!(first_rows.len(), 1);
    assert_eq!(second_rows.len(), 1);
    let (birth, tombstone) = (&first_rows[0], &second_rows[0]);
    assert_eq!(birth.retired_at, None);
    assert_eq!(tombstone.retired_at, Some(CommitSeq(6)));
    let mut reopened_birth = tombstone.clone();
    reopened_birth.retired_at = None;
    assert_eq!(
        &reopened_birth, birth,
        "the tombstone must restate the exact birth"
    );

    let patches = vec![first_rows, second_rows];
    assert_eq!(
        fgdb_strata::vertex::merge_vertex(&patches, VId(1), CommitSeq(5))
            .expect("visible before the retirement")
            .labels,
        vec![LabelId(3), LabelId(5)]
    );
    assert!(
        fgdb_strata::vertex::merge_vertex(&patches, VId(1), CommitSeq(6)).is_none(),
        "half-open: the vertex is gone AT the retirement sequence"
    );
}

/// Created and deleted in one commit while the creation is still pending: the
/// durable image is NO row at all — the same fold as an edge's, for the same
/// empty-visibility-interval reason.
#[test]
fn a_same_commit_vertex_create_and_delete_folds_to_no_row() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(2), &create_vertex_row(1, 0))
        .expect("creates");
    w.apply(keys(), CommitSeq(2), &delete_vertex_row(1, vec![]))
        .expect("deletes in the same commit");
    assert_eq!(
        w.pending_vertex_len(),
        0,
        "the fold removed the pending row"
    );
    assert!(
        w.seal_vertices(keys()).expect("seal runs").is_none(),
        "nothing to seal: the vertex is visible on no snapshot"
    );
    // The identity stays permanently spent: no resurrection after the fold.
    assert_eq!(
        w.apply(keys(), CommitSeq(3), &create_vertex_row(1, 1)),
        Err(WriteError::VertexIdentitySpent { vid: VId(1) })
    );
}

/// A `DeleteVertex` for a vertex this fold never saw is refused BEFORE any
/// cascade member retires — a mid-cascade refusal would leave the writer
/// half-applied, which is the exact state the edge preflight exists to
/// prevent.
#[test]
fn deleting_an_unknown_vertex_is_refused_before_any_edge_retires() {
    let mut w = writer();
    apply_edge(&mut w, 1, 10, 1, 2).expect("an unrelated live edge");
    assert_eq!(
        w.apply(keys(), CommitSeq(2), &delete_vertex_row(9, vec![EId(10)])),
        Err(WriteError::UnknownVertex { vid: VId(9) })
    );
    // The cascade member did NOT retire: the edge still seals live.
    let sealed = w.seal(keys()).expect("seals").expect("a block");
    let entries = decode_block(&sealed.bytes).expect("decodes");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].retired_at, None, "the refusal was atomic");
}

/// A self-loop is incident once. DeleteVertex's cascade must equal that
/// singleton or apply refuses CascadeImageMismatch.
#[test]
fn a_self_loop_delete_cascades_the_edge_once() {
    let mut w = writer();
    apply_edge(&mut w, 1, 10, 1, 1).expect("creates a self-loop");
    w.apply(keys(), CommitSeq(2), &delete_vertex_row(1, vec![EId(10)]))
        .expect("self-loop cascade is a singleton");
    assert!(w.live_edge(EId(10)).is_none(), "the loop must retire");
    assert!(
        w.live_vertex_row(VId(1)).is_none(),
        "the vertex must retire"
    );
}

/// Early-seal during cascade retire must not leave the writer half-applied
/// when the incident set is past MAX_BLOCK_ENTRIES.
#[test]
fn a_cascade_past_the_entry_ceiling_retires_every_incident_edge() {
    let mut w = writer();
    seed_vertices(&mut w, 1, &[1, 2]);
    let count = usize::try_from(fgdb_strata::MAX_BLOCK_ENTRIES).expect("fits") + 1;
    let seq = CommitSeq(1);
    for n in 1..=count {
        w.apply(keys(), seq, &create(n as u128, 1, 2))
            .expect("creates an incident edge");
    }
    let cascade: Vec<EId> = (1..=count as u128).map(EId).collect();
    w.apply(keys(), CommitSeq(2), &delete_vertex_row(1, cascade))
        .expect("a 257-edge cascade must retire atomically");
    for n in 1..=count as u128 {
        assert!(
            w.live_edge(EId(n)).is_none(),
            "eid={n} must retire with the vertex"
        );
    }
    assert!(w.live_vertex_row(VId(1)).is_none());
    assert!(
        w.live_vertex_row(VId(2)).is_some(),
        "the other endpoint stays"
    );
}

/// A second delete of the same vertex is `UnknownVertex`: retirement removed
/// it from the live map and the spent set forbids the re-create that could
/// make it deletable again.
#[test]
fn a_vertex_retirement_is_final() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create_vertex_row(1, 0))
        .expect("creates");
    w.apply(keys(), CommitSeq(2), &delete_vertex_row(1, vec![]))
        .expect("retires");
    assert_eq!(
        w.apply(keys(), CommitSeq(3), &delete_vertex_row(1, vec![])),
        Err(WriteError::UnknownVertex { vid: VId(1) })
    );
}

/// A format-ceiling seal must not turn a later same-seq content update into
/// an empty-interval tombstone (fgdb-aubf).
#[test]
fn a_same_commit_edge_update_after_the_entry_ceiling_restates_the_live_row() {
    let mut w = writer();
    let seq = CommitSeq(1);
    let key = PropertyKeyId(7);
    seed_vertices(&mut w, 1, &[1, 2]);
    let ceiling = usize::try_from(fgdb_strata::MAX_BLOCK_ENTRIES).expect("fits");
    for n in 1..=ceiling + 1 {
        w.apply(keys(), seq, &create(n as u128, 1, 2))
            .expect("creates up to and past the entry ceiling");
    }
    w.apply(
        keys(),
        seq,
        &DeltaRow::Property {
            elem: ElementId::Edge(EId(1)),
            property: key,
            before: None,
            after: Some(CanonicalScalar::Int(9)),
        },
    )
    .expect("same-seq update of a sealed creation must restate, not refuse");
    assert_eq!(
        w.live_edge_row(EId(1)).expect("still live"),
        vec![(key, CanonicalScalar::Int(9))]
    );
    let (root, blocks, _) = w
        .publish(keys(), seq)
        .expect("the restated oversized family must still publish");
    assert!(
        blocks.len() >= 2,
        "257 same-family entries cannot fit one block"
    );
    fgdb_strata::root::encode_root(&root).expect("the restated split root is lawful");
}

#[test]
fn a_same_commit_vertex_update_after_the_patch_ceiling_restates_the_live_row() {
    let mut w = writer();
    let seq = CommitSeq(1);
    let key = PropertyKeyId(3);
    let ceiling = usize::try_from(fgdb_strata::vertex::MAX_PATCH_ROWS).expect("fits");
    for n in 1..=ceiling + 1 {
        w.apply(keys(), seq, &create_vertex_bare(n as u128))
            .expect("creates up to and past the patch ceiling");
    }
    w.apply(
        keys(),
        seq,
        &DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: key,
            before: None,
            after: Some(CanonicalScalar::Int(4)),
        },
    )
    .expect("same-seq update of a sealed vertex creation must restate, not refuse");
    assert_eq!(
        w.live_vertex_row(VId(1)).expect("still live").props,
        vec![(key, CanonicalScalar::Int(4))]
    );
    let (root, _, patches) = w
        .publish(keys(), seq)
        .expect("the restated oversized vertex run must still publish");
    assert!(
        patches.len() >= 2,
        "257 pending vertex rows cannot fit one patch"
    );
    fgdb_strata::root::encode_root(&root).expect("the restated split root is lawful");
}

/// Early-seal at 256 must not freeze a live same-seq creation: delete can
/// only fold away while the row is still pending (fgdb-wlxe).
#[test]
fn a_same_commit_edge_delete_after_the_entry_ceiling_folds_away() {
    let mut w = writer();
    let seq = CommitSeq(1);
    seed_vertices(&mut w, 1, &[1, 2]);
    let ceiling = usize::try_from(fgdb_strata::MAX_BLOCK_ENTRIES).expect("fits");
    for n in 1..=ceiling + 1 {
        w.apply(keys(), seq, &create(n as u128, 1, 2))
            .expect("creates up to and past the entry ceiling");
    }
    w.apply(keys(), seq, &delete(1))
        .expect("same-seq delete of a ceiling-straddling creation must fold away");
    assert!(w.live_edge(EId(1)).is_none(), "eid=1 must not stay live");
}

#[test]
fn a_same_commit_vertex_delete_after_the_patch_ceiling_folds_away() {
    let mut w = writer();
    let seq = CommitSeq(1);
    let ceiling = usize::try_from(fgdb_strata::vertex::MAX_PATCH_ROWS).expect("fits");
    for n in 1..=ceiling + 1 {
        w.apply(keys(), seq, &create_vertex_bare(n as u128))
            .expect("creates up to and past the patch ceiling");
    }
    w.apply(keys(), seq, &delete_vertex_row(1, vec![]))
        .expect("same-seq delete of a ceiling-straddling vertex must fold away");
    assert!(
        w.live_vertex_row(VId(1)).is_none(),
        "vid=1 must not stay live"
    );
}

/// Early-seal skip lets one family grow past MAX_BLOCK_ENTRIES. publish/seal
/// must split that run into conforming blocks (fgdb-otcw); encode_block
/// refuses a 257-entry family.
#[test]
fn a_same_commit_family_past_the_entry_ceiling_publishes_conforming_blocks() {
    let mut w = writer();
    let seq = CommitSeq(1);
    seed_vertices(&mut w, 1, &[1, 2]);
    let ceiling = usize::try_from(fgdb_strata::MAX_BLOCK_ENTRIES).expect("fits");
    let count = ceiling + 1;
    for n in 1..=count {
        w.apply(keys(), seq, &create(n as u128, 1, 2))
            .expect("creates up to and past the entry ceiling");
    }
    let (root, blocks, _patches) = w
        .publish(keys(), seq)
        .expect("publish must split the oversized family");
    assert_eq!(
        blocks.len(),
        2,
        "one family of {} propertyless entries is two blocks",
        count
    );
    assert_eq!(root.blocks.len(), 2);
    let mut eids: Vec<EId> = blocks
        .iter()
        .flat_map(|block| decode_block(&block.bytes).expect("each chunk decodes"))
        .map(|entry| entry.eid)
        .collect();
    eids.sort();
    assert_eq!(
        eids,
        (1..=count as u128).map(EId).collect::<Vec<_>>(),
        "every created edge must survive the split"
    );
    fgdb_strata::root::encode_root(&root).expect("the split root is lawful");
}

/// The property-patch ceiling is 255 (u8 locators). 256 same-family
/// propertied edges must split or encode_property_patch / u8::try_from
/// refuse the chunk (fgdb-hc04).
#[test]
fn a_same_commit_propertied_family_past_the_patch_row_ceiling_publishes() {
    let mut w = writer();
    let seq = CommitSeq(1);
    seed_vertices(&mut w, 1, &[1, 2]);
    let ceiling = usize::try_from(fgdb_strata::edge_props::MAX_PROPERTY_PATCH_ROWS).expect("fits");
    let count = ceiling + 1;
    let key = PropertyKeyId(7);
    for n in 1..=count {
        w.apply(
            keys(),
            seq,
            &create_with_props(n as u128, 1, 2, vec![(key, CanonicalScalar::Int(n as i64))]),
        )
        .expect("creates up to and past the property-patch ceiling");
    }
    let (root, blocks, _patches) = w
        .publish(keys(), seq)
        .expect("publish must split the oversized propertied family");
    assert_eq!(
        blocks.len(),
        2,
        "one family of {} propertied entries is two blocks",
        count
    );
    assert_eq!(root.blocks.len(), 2);
    let mut eids: Vec<EId> = blocks
        .iter()
        .flat_map(|block| decode_block(&block.bytes).expect("each chunk decodes"))
        .map(|entry| entry.eid)
        .collect();
    eids.sort();
    assert_eq!(
        eids,
        (1..=count as u128).map(EId).collect::<Vec<_>>(),
        "every propertied edge must survive the split"
    );
    fgdb_strata::root::encode_root(&root).expect("the split root is lawful");
}

/// The vertex half of fgdb-otcw: seal_vertices must chunk at MAX_PATCH_ROWS
/// rather than handing encode_patch a 257-row run.
#[test]
fn a_same_commit_vertex_run_past_the_patch_ceiling_publishes_conforming_patches() {
    let mut w = writer();
    let seq = CommitSeq(1);
    let ceiling = usize::try_from(fgdb_strata::vertex::MAX_PATCH_ROWS).expect("fits");
    let count = ceiling + 1;
    for n in 1..=count {
        w.apply(keys(), seq, &create_vertex_bare(n as u128))
            .expect("creates up to and past the patch ceiling");
    }
    let (root, _blocks, patches) = w
        .publish(keys(), seq)
        .expect("publish must split the oversized vertex run");
    assert_eq!(
        patches.len(),
        2,
        "a {}-row pending map is two patches",
        count
    );
    assert_eq!(root.vertex_patches.len(), 2);
    let mut vids: Vec<VId> = patches
        .iter()
        .flat_map(|patch| {
            fgdb_strata::vertex::decode_patch(&patch.bytes).expect("each chunk decodes")
        })
        .map(|row| row.vid)
        .collect();
    vids.sort();
    assert_eq!(
        vids,
        (1..=count as u128).map(VId).collect::<Vec<_>>(),
        "every created vertex must survive the split"
    );
    fgdb_strata::root::encode_root(&root).expect("the split root is lawful");
}

/// An explicit mid-commit seal freezes the creation. A later same-seq
/// property restatement must not make a same-seq delete look foldable:
/// folding would drop the restatement and leave the sealed live row as a
/// ghost (the aubf restatement path).
#[test]
fn a_same_commit_delete_after_explicit_seal_and_restatement_is_refused() {
    let mut w = writer();
    let seq = CommitSeq(1);
    seed_vertices(&mut w, 1, &[1, 2]);
    w.apply(keys(), seq, &create(10, 1, 2)).expect("creates");
    w.seal(keys()).expect("explicit mid-commit seal");
    w.apply(
        keys(),
        seq,
        &DeltaRow::Property {
            elem: ElementId::Edge(EId(10)),
            property: PropertyKeyId(7),
            before: None,
            after: Some(CanonicalScalar::Int(9)),
        },
    )
    .expect("restates the already-sealed same-seq creation");
    assert_eq!(
        w.apply(keys(), seq, &delete(10)),
        Err(WriteError::Block(
            fgdb_strata::BlockError::RetiredBeforeCreated {
                at: 0,
                created_at: seq,
                retired_at: seq,
            }
        )),
        "the sealed creation cannot fold away; empty interval is refused"
    );
    assert!(
        w.live_edge(EId(10)).is_some(),
        "the refused delete must leave the restated edge live"
    );
    let (root, blocks, _) = w.publish(keys(), seq).expect("publishes the restatement");
    fgdb_strata::root::encode_root(&root).expect("lawful");
    let decoded: Vec<Vec<_>> = blocks
        .iter()
        .map(|block| decode_block(&block.bytes).expect("decodes"))
        .collect();
    assert_eq!(
        fgdb_strata::root::merge_neighbours(&decoded, VId(1), REL, seq).expect("merged"),
        vec![VId(2)],
        "last-wins keeps the restated live edge; the refused delete did not erase it"
    );
}

#[test]
fn a_same_commit_vertex_delete_after_explicit_seal_and_restatement_is_refused() {
    let mut w = writer();
    let seq = CommitSeq(1);
    w.apply(keys(), seq, &create_vertex_bare(1))
        .expect("creates");
    w.seal_vertices(keys()).expect("explicit mid-commit seal");
    w.apply(
        keys(),
        seq,
        &DeltaRow::Property {
            elem: ElementId::Vertex(VId(1)),
            property: PropertyKeyId(3),
            before: None,
            after: Some(CanonicalScalar::Int(4)),
        },
    )
    .expect("restates the already-sealed same-seq vertex");
    assert_eq!(
        w.apply(keys(), seq, &delete_vertex_row(1, vec![])),
        Err(WriteError::Patch(
            fgdb_strata::vertex::VertexPatchError::RetiredBeforeCreated {
                at: 0,
                created_at: seq,
                retired_at: seq,
            }
        )),
        "the sealed vertex cannot fold away; empty interval is refused"
    );
    assert!(
        w.live_vertex_row(VId(1)).is_some(),
        "the refused delete must leave the restated vertex live"
    );
}
