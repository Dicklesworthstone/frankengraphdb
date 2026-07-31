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

/// Retirement and re-creation in ONE COMMIT can force two block boundaries with
/// equal upper frontiers. Equality is legal, and the version intervals still pick
/// exactly one live edge at the boundary.
#[test]
fn retirement_and_recreation_at_one_sequence_remain_rootable() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.seal(keys()).expect("seals the creation");
    w.apply(keys(), CommitSeq(6), &delete(10)).expect("retires");
    w.apply(keys(), CommitSeq(6), &create(11, 1, 2))
        .expect("re-creates at the same sequence");

    let (root, blocks) = w.publish(keys(), CommitSeq(6)).expect("publishes");
    assert_eq!(
        root.blocks
            .iter()
            .map(|block| (block.first_seq, block.last_seq))
            .collect::<Vec<_>>(),
        vec![
            (CommitSeq(1), CommitSeq(1)),
            (CommitSeq(1), CommitSeq(6)),
            (CommitSeq(6), CommitSeq(6)),
        ]
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

/// A KEY'S SECOND VERSION FORCES A SEAL, without the caller asking.
///
/// A block requires strictly ascending unique keys, so a re-creation cannot share
/// the pending run. The writer seals early rather than building a block the encoder
/// would refuse — the constraint the differential discovered in slice 4, honoured
/// here.
#[test]
fn a_re_creation_forces_a_seal() {
    let mut w = writer();
    w.apply(keys(), CommitSeq(1), &create(10, 1, 2))
        .expect("creates");
    w.apply(keys(), CommitSeq(3), &delete(10)).expect("deletes");
    assert_eq!(w.sealed().len(), 0, "nothing sealed yet");

    w.apply(keys(), CommitSeq(5), &create(11, 1, 2))
        .expect("re-creates");
    assert_eq!(
        w.sealed().len(),
        1,
        "the second version of (1,REL,2) must have forced a seal"
    );
    assert_eq!(w.pending_len(), 1, "and the new version is pending");

    let (root, blocks) = w.publish(keys(), CommitSeq(9)).expect("publishes");
    assert_eq!(blocks.len(), 2);
    assert_eq!(root.blocks.len(), 2);
    // Publication frontiers ascend; lower visibility bounds may overlap in other
    // histories because a tombstone repeats an old creation sequence.
    assert!(root.blocks[0].last_seq.0 <= root.blocks[1].last_seq.0);
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

    let sealed = w.seal(keys()).expect("seals").expect("a block");
    let entries = decode_block(&sealed.bytes).expect("decodes");
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
