use super::*;
use crate::{CoordinateEntry, DeltaRow, LogicalDeltaTemplate, SchemaEpoch};
use crate::zset::committed::snapshot::EdgeSnapshotBuilder;
use fgdb_types::{EId, MarkerRef, ObjectId};

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn create(id: u128, relation: u64, a: u128, b: u128) -> DeltaRow {
    DeltaRow::CreateEdge { eid: EId(id), birth_ordinal: id as u64, src: VId(a),
        relation: RelationId(relation), dst: VId(b), canonical_key: None,
        props: vec![], valid_time: None }
}
fn delete(id: u128) -> DeltaRow {
    DeltaRow::DeleteEdge { eid: EId(id), before_version: ObjectId([8; 32]) }
}
fn coordinate(relation: u64, rows: Vec<DeltaRow>) -> CoordinateEntry {
    CoordinateEntry { graph: GraphId(1), branch: BranchId(1), relation: RelationId(relation),
        schema_epoch: SchemaEpoch(0), schema_transition: None, rows }
}
// Test-shaped decoded batches prove algebra composition, not authentication.
// The database integration suite obtains input from real Chronicle publication.
fn batch(seq: u64, entries: Vec<CoordinateEntry>) -> LogicalDeltaBatch {
    let template = LogicalDeltaTemplate::build(ObjectId([1; 32]), [2; 32], entries).unwrap();
    LogicalDeltaBatch::from_parts_for_test(template.coordinate_entries().to_vec(), [seq as u8; 32],
        MarkerRef { marker_oid: ObjectId([seq as u8; 32]), commit_seq: CommitSeq(seq) },
        CommitSeq(seq), CommitSeq(seq))
}
fn empty(quantifier: TriangleQuantifier) -> CommittedTriangles {
    CommittedTriangles::new(GraphId(1), BranchId(1), RelationId(1), quantifier)
}
fn advance(state: &mut CommittedTriangles, index: &LocalDeltaBatchIndex) -> ZSet<(VId, VId, VId)> {
    state.prepare_next(index, LIMBS, &mut allow).unwrap().unwrap().commit()
}
fn seed(quantifier: TriangleQuantifier) -> (CommittedTriangles, LocalDeltaBatchIndex) {
    let mut index = LocalDeltaBatchIndex::new();
    index.insert(batch(1, vec![coordinate(1, vec![create(10, 1, 1, 2), create(11, 1, 2, 1),
        create(12, 1, 2, 3), create(13, 1, 3, 1), create(14, 1, 1, 1)]),
        coordinate(2, vec![create(20, 2, 1, 3)])])).unwrap();
    let mut state = empty(quantifier);
    advance(&mut state, &index);
    (state, index)
}
fn count(state: &CommittedTriangles) -> i128 { state.total().to_i128().unwrap() }
fn one(delta: &ZSet<(VId, VId, VId)>, weight: i128) {
    assert_eq!(delta.len(), usize::from(weight != 0));
    assert_eq!(delta.weight(&(VId(1), VId(2), VId(3))).map(ZWeight::to_i128),
        if weight == 0 { None } else { Some(Some(weight)) });
}

#[test]
fn simultaneous_changes_parallel_orientations_and_cross_relation_cascades_are_exact() {
    for q in [TriangleQuantifier::All, TriangleQuantifier::Distinct] {
        let (mut state, mut index) = seed(q);
        let initial = if q == TriangleQuantifier::All { 2 } else { 1 };
        assert_eq!(count(&state), initial);
        one(&state.snapshot(LIMBS, &mut allow).unwrap(), initial);
        assert_eq!(state.relation(), RelationId(1));
        assert_eq!(state.quantifier(), q);
        index.insert(batch(2, vec![coordinate(1, vec![delete(10), create(15, 1, 2, 3),
            create(16, 1, 1, 3)])])).unwrap();
        let next = if q == TriangleQuantifier::All { 4 } else { 1 };
        let pending = state.prepare_next(&index, LIMBS, &mut allow).unwrap().unwrap();
        assert_eq!(pending.total().to_i128(), Some(next));
        assert_eq!(pending.commit_seq(), CommitSeq(2));
        one(&pending.commit(), next - initial);
        assert_eq!(count(&state), next);
        index.insert(batch(3, vec![coordinate(2, vec![DeltaRow::DeleteVertex {
            vid: VId(3), before_version: ObjectId([8; 32]),
            sorted_retired_incident_edges: vec![EId(12), EId(13), EId(15), EId(16), EId(20)],
        }]), coordinate(1, vec![delete(12)])])).unwrap();
        one(&advance(&mut state, &index), -next);
        assert_eq!(count(&state), 0);
        assert_eq!(state.input.edge_count(), 2); // reverse edge plus self-loop
        assert_eq!(state.frontier(), CommitSeq(3));
        assert!(state.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
    }
}

#[test]
fn zero_derivative_ticks_still_publish_identity_and_history_changes() {
    let (mut state, mut index) = seed(TriangleQuantifier::All);
    index.insert(batch(2, vec![coordinate(1, vec![delete(10), create(99, 1, 1, 2)])])).unwrap();
    assert!(advance(&mut state, &index).is_empty());
    assert_eq!(state.frontier(), CommitSeq(2));
    let mut foreign = coordinate(1, vec![create(100, 1, 2, 3)]);
    foreign.graph = GraphId(2);
    index.insert(batch(3, vec![foreign])).unwrap();
    assert!(advance(&mut state, &index).is_empty());
    assert_eq!(state.frontier(), CommitSeq(3));
    index.insert(batch(4, vec![coordinate(1, vec![delete(99)])])).unwrap();
    one(&advance(&mut state, &index), -1);
    assert_eq!(count(&state), 1);
    // Unknown identity in an UNSELECTED relation still invalidates the input.
    index.insert(batch(5, vec![coordinate(2, vec![delete(900)])])).unwrap();
    assert!(matches!(state.prepare_next(&index, LIMBS, &mut allow),
        Err(CommittedTrianglesError::Input(_))));
    assert_eq!(state.frontier(), CommitSeq(4));
    assert_eq!(count(&state), 1);
}

#[test]
fn every_control_refusal_and_downstream_drop_preserve_input_triangles_and_total() {
    let (mut state, mut index) = seed(TriangleQuantifier::All);
    let before = seed(TriangleQuantifier::All).0;
    index.insert(batch(2, vec![coordinate(1, vec![delete(10), create(15, 1, 2, 3),
        create(16, 1, 1, 3)])])).unwrap();
    let mut calls = 0usize;
    {
        let pending = state.prepare_next(&index, LIMBS, &mut |_| { calls += 1; Ok::<_, usize>(()) })
            .unwrap().unwrap();
        assert_eq!(pending.total().to_i128(), Some(4));
        one(pending.delta(), 2);
        // Model a refusing downstream sink after complete operator preparation.
        drop(pending);
    }
    assert!(calls > 0);
    assert_eq!(state, before);
    for stop in 1..=calls {
        let mut visited = 0;
        let refused = state.prepare_next(&index, LIMBS, &mut |_| {
            visited += 1;
            if visited == stop { Err(stop) } else { Ok(()) }
        });
        assert!(refused.is_err(), "control boundary {stop} did not refuse");
        drop(refused);
        assert_eq!(visited, stop);
        assert_eq!(state, before, "partial publication at {stop}");
    }
    one(&advance(&mut state, &index), 2);
    assert_eq!(count(&state), 4);
}

#[test]
fn snapshot_bootstrap_uses_current_edges_then_accepts_the_same_successor() {
    let (_, mut index) = seed(TriangleQuantifier::All);
    // Source cut has only three selected live sides, not the old bag at seq 1.
    index.insert(batch(2, vec![coordinate(1, vec![delete(10)])])).unwrap();
    let mut builder = EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), &index, &mut allow).unwrap();
    for (eid, relation, a, b) in [(11, 1, 2, 1), (12, 1, 2, 3), (13, 1, 3, 1),
        (14, 1, 1, 1), (20, 2, 1, 3)] {
        builder.insert(EId(eid), (RelationId(relation), VId(a), VId(b)), SchemaEpoch(0), LIMBS, &mut allow).unwrap();
    }
    let mut rebuilt = CommittedTriangles::from_snapshot(builder.finish(&mut allow).unwrap(),
        RelationId(1), TriangleQuantifier::All, LIMBS, &mut allow).unwrap();
    assert_eq!(rebuilt.frontier(), CommitSeq(2));
    assert_eq!(count(&rebuilt), 1);
    let (mut replayed, _) = seed(TriangleQuantifier::All);
    advance(&mut replayed, &index);
    assert_eq!(rebuilt.snapshot(LIMBS, &mut allow).unwrap(), replayed.snapshot(LIMBS, &mut allow).unwrap());
    index.insert(batch(3, vec![coordinate(1, vec![delete(12)])])).unwrap();
    assert_eq!(advance(&mut rebuilt, &index), advance(&mut replayed, &index));
    assert_eq!(count(&rebuilt), 0);
    assert_eq!(rebuilt.frontier(), CommitSeq(3));
}

#[test]
fn caught_up_calls_reject_a_substituted_history_anchor_without_state_change() {
    let (mut state, _) = seed(TriangleQuantifier::Distinct);
    let before = seed(TriangleQuantifier::Distinct).0;
    let mut fork = LocalDeltaBatchIndex::new();
    let changed = batch(1, vec![coordinate(1, vec![create(500, 1, 9, 10)])]);
    // Same seq/marker spelling, different decoded template identity.
    // Construct via the same supported fixture API instead of mutating internals.
    let changed = LogicalDeltaBatch::from_parts_for_test(changed.coordinate_entries().to_vec(), [77; 32],
        MarkerRef { marker_oid: ObjectId([1; 32]), commit_seq: CommitSeq(1) },
        CommitSeq(1), CommitSeq(1));
    fork.insert(changed).unwrap();
    assert!(matches!(state.prepare_next(&fork, LIMBS, &mut allow),
        Err(CommittedTrianglesError::Input(_))));
    assert_eq!(state, before);
}
