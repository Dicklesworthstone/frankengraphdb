use super::*;

fn indexed_baseline(
    index: &LocalDeltaBatchIndex,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), usize>,
) -> Result<EdgeSnapshot, SnapshotInputError<usize>> {
    let mut builder = EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), index, control)?;
    builder.record_epoch(RelationId(9), SchemaEpoch(7), control)?;
    for (id, relation, from, to) in [(1, R, 1, 2), (2, R, 1, 2), (3, R, 2, 3), (4, S, 3, 4)] {
        builder.insert(EId(id), (relation, VId(from), VId(to)), SchemaEpoch(0), LIMBS, control)?;
    }
    builder.finish(control)
}
fn recursive(
    index: &LocalDeltaBatchIndex,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), usize>,
) -> Result<CommittedReachability, usize> {
    let snapshot = indexed_baseline(index, control).map_err(|error| match error {
        SnapshotInputError::Input(EdgeInputError::Delta(ZSetError::Control(at))) => at,
        other => panic!("unexpected snapshot fixture refusal: {other:?}"),
    })?;
    CommittedReachability::from_snapshot(snapshot, R, LIMBS, control).map_err(|error| match error {
        CommittedReachabilityError::Reachability(ReachabilityError::Delta(ZSetError::Control(at))) => at,
        other => panic!("unexpected recursive fixture refusal: {other:?}"),
    })
}

#[test]
fn full_retirement_preserves_the_same_baseline_then_cross_relation_cascades() {
    let anchor = initial();
    let mut index = LocalDeltaBatchIndex::new();
    index.insert(anchor.clone()).unwrap();
    let legacy = baseline(&anchor, &mut allow).unwrap().into_input();
    assert_eq!(indexed_baseline(&index, &mut allow).unwrap().into_input(), legacy);
    index.retire_prefix(CommitSeq(1)).unwrap();
    assert!(index.is_empty());
    let mut current = indexed_baseline(&index, &mut allow).unwrap().into_input();
    assert_eq!(current, legacy);
    assert!(current.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
    let mut closure = recursive(&index, &mut allow).unwrap();
    assert_eq!(closure.pairs().collect::<BTreeSet<_>>(),
        BTreeSet::from([(VId(1), VId(2)), (VId(1), VId(3)), (VId(2), VId(3))]));
    index.insert(batch(2, vec![coordinate(R, vec![
        DeltaRow::DeleteVertex { vid: VId(2), before_version: ObjectId([8; 32]),
            sorted_retired_incident_edges: vec![EId(1), EId(2), EId(3)] },
        DeltaRow::DeleteEdge { eid: EId(1), before_version: ObjectId([8; 32]) },
    ]), coordinate(S, vec![
        DeltaRow::DeleteVertex { vid: VId(3), before_version: ObjectId([8; 32]),
            sorted_retired_incident_edges: vec![EId(3), EId(4)] },
    ])])).unwrap();
    current.prepare_next(&index, LIMBS, &mut allow).unwrap().unwrap().commit();
    closure.prepare_next(&index, LIMBS, &mut allow).unwrap().unwrap().commit();
    assert_eq!(current.edge_count(), 0);
    assert!(closure.pairs().next().is_none());
    index.retire_prefix(CommitSeq(2)).unwrap();
    assert!(closure.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
    let mut behind = legacy;
    assert!(matches!(behind.prepare_next(&index, LIMBS, &mut allow),
        Err(EdgeInputError::Index(crate::IndexError::CursorRetired { .. }))));
    let mut changed = coordinate(RelationId(9), vec![]);
    changed.schema_epoch = SchemaEpoch(8);
    index.insert(batch(3, vec![changed])).unwrap();
    assert_eq!(current.prepare_next(&index, LIMBS, &mut allow).unwrap_err(), EdgeInputError::SchemaChanged);
    assert_eq!(current.frontier(), CommitSeq(2));
}

#[test]
fn every_retired_index_bootstrap_checkpoint_refuses_privately_and_retries() {
    let mut index = LocalDeltaBatchIndex::new();
    index.insert(initial()).unwrap();
    index.retire_prefix(CommitSeq(1)).unwrap();
    let before = index.clone();
    let mut calls = 0;
    let expected = recursive(&index, &mut |_| { calls += 1; Ok(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert_eq!(recursive(&index, &mut |_| {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }).unwrap_err(), stop);
        assert_eq!(seen, stop);
        assert_eq!(index, before);
        assert_eq!(recursive(&index, &mut allow).unwrap(), expected);
    }
    let mut builder = EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), &index, &mut allow).unwrap();
    assert!(builder.record_epoch(R, SchemaEpoch(0), &mut |_| Err(17_usize)).is_err());
    assert!(matches!(builder.finish(&mut allow), Err(SnapshotInputError::Refused)));
}

#[test]
fn index_constructor_never_invents_origin_data_or_missing_anchor_evidence() {
    let origin = LocalDeltaBatchIndex::new();
    let empty = EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), &origin, &mut allow).unwrap()
        .finish(&mut allow).unwrap();
    assert_eq!(empty.frontier(), CommitSeq::ORIGIN);
    assert!(empty.rows().is_empty());
    let mut nonempty = EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), &origin, &mut allow).unwrap();
    assert_eq!(nonempty.insert(EId(1), (R, VId(1), VId(2)), SchemaEpoch(0), LIMBS, &mut allow),
        Err(SnapshotInputError::NonEmptyOrigin));
    let mut bare = LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(1), CommitSeq(1), vec![]);
    bare.retire_prefix(CommitSeq(1)).unwrap();
    assert_eq!(EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), &bare, &mut allow).unwrap_err(),
        SnapshotInputError::Input(EdgeInputError::AnchorUnavailable { at: CommitSeq(1) }));
    for marker in [false, true] {
        let malformed = LogicalDeltaBatch::from_parts_for_test(vec![], [1; 32],
            MarkerRef { marker_oid: ObjectId([1; 32]), commit_seq: CommitSeq(if marker { 2 } else { 1 }) },
            CommitSeq(1), CommitSeq(if marker { 1 } else { 2 }));
        let index = LocalDeltaBatchIndex::from_parts_for_test(CommitSeq::ORIGIN, CommitSeq(1),
            vec![(CommitSeq(1), malformed)]);
        assert!(matches!(EdgeSnapshotBuilder::from_index(GraphId(1), BranchId(1), &index, &mut allow),
            Err(SnapshotInputError::Input(EdgeInputError::Index(_)))));
    }
}

#[test]
fn retired_anchor_compares_marker_and_template_independently() {
    let anchor = initial();
    let mut original = LocalDeltaBatchIndex::new();
    original.insert(anchor.clone()).unwrap();
    original.retire_prefix(CommitSeq(1)).unwrap();
    let mut state = indexed_baseline(&original, &mut allow).unwrap().into_input();
    for marker in [false, true] {
        let changed = LogicalDeltaBatch::from_parts_for_test(anchor.coordinate_entries().to_vec(),
            if marker { *anchor.source_template_digest() } else { [99; 32] },
            MarkerRef { marker_oid: if marker { ObjectId([99; 32]) } else { anchor.commit_marker_identity().marker_oid },
                commit_seq: CommitSeq(1) }, CommitSeq(1), CommitSeq(1));
        let mut fork = LocalDeltaBatchIndex::new();
        fork.insert(changed).unwrap();
        fork.retire_prefix(CommitSeq(1)).unwrap();
        assert_eq!(state.prepare_next(&fork, LIMBS, &mut allow).unwrap_err(),
            EdgeInputError::HistoryChanged { at: CommitSeq(1) });
        assert_eq!(state, indexed_baseline(&original, &mut allow).unwrap().into_input());
    }
}
