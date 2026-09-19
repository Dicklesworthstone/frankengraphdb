use super::*;
use crate::{
    CoordinateEntry, DeltaRow, LogicalDeltaBatch, LogicalDeltaTemplate, SchemaEpoch, ZWeight,
};
use fgdb_types::{EId, MarkerRef, ObjectId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn view() -> CommittedReachability {
    CommittedReachability::new(GraphId(1), BranchId(1), RelationId(1))
}
fn create(eid: u128, relation: u64, source: u128, target: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(eid),
        birth_ordinal: eid as u64,
        src: VId(source),
        relation: RelationId(relation),
        dst: VId(target),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}
fn delete(eid: u128) -> DeltaRow {
    DeltaRow::DeleteEdge {
        eid: EId(eid),
        before_version: ObjectId([8; 32]),
    }
}
fn coordinate(relation: u64, rows: Vec<DeltaRow>) -> CoordinateEntry {
    CoordinateEntry {
        graph: GraphId(1),
        branch: BranchId(1),
        relation: RelationId(relation),
        schema_epoch: SchemaEpoch(0),
        schema_transition: None,
        rows,
    }
}
// Decoded/test-shaped batches exercise composition, not commit attestation.
// The executable database witness obtains batches from the real durable path.
fn batch(seq: u64, entries: Vec<CoordinateEntry>) -> LogicalDeltaBatch {
    let template = LogicalDeltaTemplate::build(ObjectId([1; 32]), [2; 32], entries).unwrap();
    LogicalDeltaBatch::from_parts_for_test(
        template.coordinate_entries().to_vec(),
        [seq as u8; 32],
        MarkerRef {
            marker_oid: ObjectId([seq as u8; 32]),
            commit_seq: CommitSeq(seq),
        },
        CommitSeq(seq),
        CommitSeq(seq),
    )
}
fn advance(view: &mut CommittedReachability, index: &LocalDeltaBatchIndex) -> ZSet<(VId, VId)> {
    view.prepare_next(index, LIMBS, &mut allow)
        .unwrap()
        .unwrap()
        .commit()
}
fn plain(delta: &ZSet<(VId, VId)>) -> BTreeMap<(u128, u128), i128> {
    delta
        .iter()
        .map(|((s, t), w)| ((s.0, t.0), w.to_i128().unwrap()))
        .collect()
}
fn seed() -> (CommittedReachability, LocalDeltaBatchIndex) {
    let mut index = LocalDeltaBatchIndex::new();
    index
        .insert(batch(
            1,
            vec![
                coordinate(
                    1,
                    vec![
                        create(10, 1, 1, 2),
                        create(11, 1, 1, 2),
                        create(12, 1, 2, 3),
                    ],
                ),
                coordinate(2, vec![create(20, 2, 3, 4)]),
            ],
        ))
        .unwrap();
    let mut state = view();
    advance(&mut state, &index);
    (state, index)
}

#[test]
fn whole_batches_preserve_parallel_counts_and_scope_recursive_paths_by_relation() {
    let (mut state, mut index) = seed();
    assert_eq!(state.relation(), RelationId(1));
    assert!(state.contains(VId(1), VId(3)));
    assert!(!state.contains(VId(1), VId(4)));
    index
        .insert(batch(
            2,
            vec![coordinate(1, vec![delete(10), create(13, 1, 2, 4)])],
        ))
        .unwrap();
    assert_eq!(
        plain(&advance(&mut state, &index)),
        BTreeMap::from([((1, 4), 1), ((2, 4), 1)])
    );
    assert!(state.contains(VId(1), VId(2)));
    // One cross-relation cascade and overlapping explicit deletion retire
    // every selected edge once; the unrelated edge is still input state.
    index
        .insert(batch(
            3,
            vec![
                coordinate(
                    2,
                    vec![DeltaRow::DeleteVertex {
                        vid: VId(2),
                        before_version: ObjectId([8; 32]),
                        sorted_retired_incident_edges: vec![EId(11), EId(12), EId(13)],
                    }],
                ),
                coordinate(1, vec![delete(11)]),
            ],
        ))
        .unwrap();
    let delta = advance(&mut state, &index);
    assert_eq!(delta.len(), 5);
    assert!(
        delta
            .iter()
            .all(|(_, weight)| weight == &ZWeight::from_i128(-1))
    );
    assert!(state.pairs().next().is_none());
    assert_eq!(state.input.edge_count(), 1);
    assert_eq!(state.frontier(), CommitSeq(3));
}

#[test]
fn cycle_insertion_and_deletion_publish_the_exact_fixed_point() {
    let mut index = LocalDeltaBatchIndex::new();
    index
        .insert(batch(
            1,
            vec![coordinate(1, vec![create(1, 1, 1, 2), create(2, 1, 2, 3)])],
        ))
        .unwrap();
    let mut state = view();
    assert_eq!(advance(&mut state, &index).len(), 3);
    index
        .insert(batch(2, vec![coordinate(1, vec![create(3, 1, 3, 1)])]))
        .unwrap();
    assert_eq!(advance(&mut state, &index).len(), 6);
    for s in 1..=3 {
        for t in 1..=3 {
            assert!(state.contains(VId(s), VId(t)));
        }
    }
    index
        .insert(batch(3, vec![coordinate(1, vec![delete(1)])]))
        .unwrap();
    let delta = advance(&mut state, &index);
    assert_eq!(delta.len(), 6);
    assert!(
        delta
            .iter()
            .all(|(_, weight)| weight == &ZWeight::from_i128(-1))
    );
    assert_eq!(
        state.pairs().collect::<Vec<_>>(),
        vec![(VId(2), VId(1)), (VId(2), VId(3)), (VId(3), VId(1))]
    );
}

#[test]
fn empty_ticks_track_replaced_edge_ids_and_unrelated_global_history() {
    let mut state = view();
    let mut index = LocalDeltaBatchIndex::new();
    index
        .insert(batch(1, vec![coordinate(1, vec![create(1, 1, 1, 2)])]))
        .unwrap();
    advance(&mut state, &index);
    index
        .insert(batch(
            2,
            vec![coordinate(1, vec![delete(1), create(2, 1, 1, 2)])],
        ))
        .unwrap();
    assert!(advance(&mut state, &index).is_empty());
    assert_eq!(state.frontier(), CommitSeq(2));
    let mut foreign_graph = coordinate(1, vec![create(3, 1, 2, 3)]);
    foreign_graph.graph = GraphId(2);
    let mut foreign_branch = coordinate(1, vec![create(4, 1, 2, 4)]);
    foreign_branch.branch = BranchId(2);
    index
        .insert(batch(3, vec![foreign_graph, foreign_branch]))
        .unwrap();
    assert!(advance(&mut state, &index).is_empty());
    assert_eq!(state.frontier(), CommitSeq(3));
    assert_eq!(state.pairs().count(), 1);
    index
        .insert(batch(4, vec![coordinate(1, vec![delete(2)])]))
        .unwrap();
    assert_eq!(
        plain(&advance(&mut state, &index)),
        BTreeMap::from([((1, 2), -1)])
    );
    assert!(state.pairs().next().is_none());
    assert!(
        state
            .prepare_next(&index, LIMBS, &mut allow)
            .unwrap()
            .is_none()
    );
}

#[test]
fn every_refusal_in_input_projection_recursion_and_final_admission_is_retryable() {
    let (before, mut index) = seed();
    index
        .insert(batch(
            2,
            vec![coordinate(
                1,
                vec![delete(10), delete(11), create(13, 1, 3, 1)],
            )],
        ))
        .unwrap();
    let mut success = seed().0;
    let mut total = 0;
    let wanted = success
        .prepare_next(&index, LIMBS, &mut |_| {
            total += 1;
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap()
        .commit();
    for stop in 1..=total {
        let mut state = seed().0;
        let mut seen = 0;
        let error = state
            .prepare_next(&index, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err();
        match error {
            CommittedReachabilityError::Input(EdgeInputError::Delta(ZSetError::Control(n)))
            | CommittedReachabilityError::Reachability(ReachabilityError::Delta(
                ZSetError::Control(n),
            )) => assert_eq!(n, stop),
            other => panic!("unexpected refusal: {other:?}"),
        }
        assert_eq!(seen, stop);
        assert_eq!(state, before);
        assert_eq!(advance(&mut state, &index), wanted);
        assert_eq!(state, success);
    }
    let mut state = seed().0;
    let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
    {
        let pending = state
            .prepare_next(&index, LIMBS, &mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(pending.commit_seq(), CommitSeq(2));
        assert_eq!(pending.delta(), &wanted);
        assert!(
            sink.prepare_update(pending.delta(), LIMBS, &mut |_| Err(17))
                .is_err()
        );
        // Drop aborts both the input frontier and recursive arrangements.
    }
    assert_eq!(state, before);
    assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
    let pending = state
        .prepare_next(&index, LIMBS, &mut allow)
        .unwrap()
        .unwrap();
    let output = sink
        .prepare_update(pending.delta(), LIMBS, &mut allow)
        .unwrap();
    output.commit();
    pending.commit();
    assert_eq!(state, success);
    assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
}

#[test]
fn caught_up_history_forks_and_schema_changes_do_not_rebind_the_view() {
    let (mut state, index) = seed();
    let before = seed().0;
    let original = index.get(CommitSeq(1)).unwrap();
    let mut fork = LocalDeltaBatchIndex::new();
    fork.insert(LogicalDeltaBatch::from_parts_for_test(
        original.coordinate_entries().to_vec(),
        [99; 32],
        original.commit_marker_identity(),
        CommitSeq(1),
        CommitSeq(1),
    ))
    .unwrap();
    assert_eq!(
        state.prepare_next(&fork, LIMBS, &mut allow).unwrap_err(),
        CommittedReachabilityError::Input(EdgeInputError::HistoryChanged { at: CommitSeq(1) })
    );
    assert_eq!(state, before);
    // Even a schema change to another input relation cannot be hidden by
    // filtering the stream before the authoritative input adapter sees it.
    let mut changed = index;
    let mut entry = coordinate(2, vec![create(21, 2, 4, 5)]);
    entry.schema_epoch = SchemaEpoch(1);
    changed.insert(batch(2, vec![entry])).unwrap();
    assert_eq!(
        state.prepare_next(&changed, LIMBS, &mut allow).unwrap_err(),
        CommittedReachabilityError::Input(EdgeInputError::SchemaChanged)
    );
    assert_eq!(state, before);
}
