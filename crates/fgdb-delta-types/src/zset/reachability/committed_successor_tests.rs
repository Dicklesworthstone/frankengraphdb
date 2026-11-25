use super::*;
use crate::{CoordinateEntry, DeltaRow, LogicalDeltaTemplate, SchemaEpoch};
use fgdb_types::{EId, MarkerRef, ObjectId};

const LIMBS: LimbLimit = LimbLimit::new(4);
fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn view() -> CommittedReachability {
    CommittedReachability::new(GraphId(1), BranchId(1), RelationId(1))
}
fn edge(id: u128, from: u128, to: u128) -> DeltaRow {
    DeltaRow::CreateEdge {
        eid: EId(id),
        birth_ordinal: id as u64,
        src: VId(from),
        relation: RelationId(1),
        dst: VId(to),
        canonical_key: None,
        props: vec![],
        valid_time: None,
    }
}
fn batch(at: u64, rows: Vec<DeltaRow>) -> LogicalDeltaBatch {
    let template = LogicalDeltaTemplate::build(
        ObjectId([1; 32]),
        [2; 32],
        vec![CoordinateEntry {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: RelationId(1),
            schema_epoch: SchemaEpoch(0),
            schema_transition: None,
            rows,
        }],
    )
    .unwrap();
    LogicalDeltaBatch::from_parts_for_test(
        template.coordinate_entries().to_vec(),
        [at as u8; 32],
        MarkerRef {
            marker_oid: ObjectId([at as u8; 32]),
            commit_seq: CommitSeq(at),
        },
        CommitSeq(at),
        CommitSeq(at),
    )
}

#[test]
fn live_and_indexed_recursive_ticks_are_identical_and_every_refusal_is_retryable() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    let first = batch(1, vec![edge(1, 1, 2), edge(2, 2, 3)]);
    let second = batch(2, vec![edge(3, 3, 1)]);
    let mut history = LocalDeltaBatchIndex::new();
    history.insert(first.clone()).unwrap();
    history.insert(second.clone()).unwrap();
    let build = || {
        let mut state = view();
        state
            .prepare_committed_successor(&cx, &first, LIMBS, &mut allow)
            .unwrap()
            .commit();
        state
    };
    let before = build();
    let mut indexed = view();
    indexed
        .prepare_next(&history, LIMBS, &mut allow)
        .unwrap()
        .unwrap()
        .commit();
    assert_eq!(indexed, before);
    let mut indexed_events = Vec::new();
    let expected = indexed
        .prepare_next(&history, LIMBS, &mut |e| {
            indexed_events.push(e);
            Ok::<_, usize>(())
        })
        .unwrap()
        .unwrap()
        .commit();
    let mut success = build();
    let mut live_events = Vec::new();
    let actual = success
        .prepare_committed_successor(&cx, &second, LIMBS, &mut |e| {
            live_events.push(e);
            Ok::<_, usize>(())
        })
        .unwrap()
        .commit();
    assert_eq!(expected, actual);
    assert_eq!(indexed_events, live_events);
    assert_eq!(indexed, success);
    assert_eq!(actual.len(), 6);
    for stop in 1..=live_events.len() {
        let mut state = build();
        let mut seen = 0;
        let error = state
            .prepare_committed_successor(&cx, &second, LIMBS, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err();
        match error {
            CommittedReachabilityError::Input(EdgeInputError::Delta(ZSetError::Control(at)))
            | CommittedReachabilityError::Reachability(ReachabilityError::Delta(
                ZSetError::Control(at),
            )) => assert_eq!(at, stop),
            other => panic!("unexpected refusal: {other:?}"),
        }
        assert_eq!(seen, stop);
        assert_eq!(state, before);
        let mut sink = state.snapshot(LIMBS, &mut allow).unwrap();
        {
            let pending = state
                .prepare_committed_successor(&cx, &second, LIMBS, &mut allow)
                .unwrap();
            assert!(
                sink.prepare_update(pending.delta(), LIMBS, &mut |_| Err(0))
                    .is_err()
            );
        }
        assert_eq!(state, before);
        let pending = state
            .prepare_committed_successor(&cx, &second, LIMBS, &mut allow)
            .unwrap();
        let output = sink
            .prepare_update(pending.delta(), LIMBS, &mut allow)
            .unwrap();
        output.commit();
        pending.commit();
        assert_eq!(state, success);
        assert_eq!(sink, state.snapshot(LIMBS, &mut allow).unwrap());
    }
    assert!(
        success
            .prepare_committed_successor(&cx, &second, LIMBS, &mut allow)
            .is_err()
    );
    assert!(
        success
            .prepare_next(&history, LIMBS, &mut allow)
            .unwrap()
            .is_none()
    );
}

#[test]
fn catalog_checkpoint_keeps_the_raw_anchor_and_needs_fresh_compatibility_for_epoch_drift() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let contexts = fgdb_types::context::PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.commit();
    let first = batch(1, vec![edge(1, 1, 2), edge(2, 2, 3)]);
    let transition_oid = ObjectId([42; 32]);
    let catalog = LogicalDeltaBatch::from_parts_for_test(
        vec![CoordinateEntry {
            graph: GraphId(1),
            branch: BranchId(1),
            relation: RelationId(1),
            schema_epoch: SchemaEpoch(0),
            schema_transition: Some(transition_oid),
            rows: vec![DeltaRow::Schema {
                transition_oid,
                before_epoch: SchemaEpoch(0),
                after_epoch: SchemaEpoch(1),
            }],
        }],
        [2; 32],
        MarkerRef {
            marker_oid: ObjectId([2; 32]),
            commit_seq: CommitSeq(2),
        },
        CommitSeq(2),
        CommitSeq(2),
    );
    let mut state = view();
    state
        .prepare_committed_successor(&cx, &first, LIMBS, &mut allow)
        .unwrap()
        .commit();
    let rows = state.snapshot(LIMBS, &mut allow).unwrap();
    let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
        &cx,
        &catalog,
        GraphId(1),
        BranchId(1),
        SchemaEpoch(1),
    );
    assert!(
        state
            .prepare_catalog_successor(&cx, &proof, LIMBS, &mut allow)
            .unwrap()
            .commit()
            .is_empty()
    );
    let bytes = state.encode_checkpoint(&mut allow).unwrap();
    let (mut restored, restored_rows) = CommittedReachability::decode_checkpoint(
        &bytes,
        GraphId(1),
        BranchId(1),
        RelationId(1),
        LIMBS,
        &mut allow,
    )
    .unwrap();
    assert_eq!(restored, state);
    assert_eq!(restored_rows, rows);
    assert_eq!(restored.encode_checkpoint(&mut allow).unwrap(), bytes);
    let mut index = LocalDeltaBatchIndex::new();
    index.insert(first.clone()).unwrap();
    index.insert(catalog.clone()).unwrap();
    assert!(
        restored
            .prepare_next(&index, LIMBS, &mut allow)
            .unwrap()
            .is_none()
    );
    let forged = LogicalDeltaBatch::from_parts_for_test(
        catalog.coordinate_entries().to_vec(),
        [99; 32],
        catalog.commit_marker_identity(),
        CommitSeq(2),
        CommitSeq(2),
    );
    let changed = LocalDeltaBatchIndex::from_parts_for_test(
        CommitSeq::ORIGIN,
        CommitSeq(2),
        vec![(CommitSeq(1), first), (CommitSeq(2), forged)],
    );
    assert_eq!(
        restored
            .prepare_next(&changed, LIMBS, &mut allow)
            .unwrap_err(),
        CommittedReachabilityError::Input(EdgeInputError::HistoryChanged { at: CommitSeq(2) })
    );
    let candidate = batch(3, vec![edge(3, 3, 4)]);
    let mut entries = candidate.coordinate_entries().to_vec();
    entries[0].schema_epoch = SchemaEpoch(1);
    let next = LogicalDeltaBatch::from_parts_for_test(
        entries,
        *candidate.source_template_digest(),
        candidate.commit_marker_identity(),
        CommitSeq(3),
        CommitSeq(3),
    );
    index.insert(next.clone()).unwrap();
    assert_eq!(
        restored
            .prepare_next(&index, LIMBS, &mut allow)
            .unwrap_err(),
        CommittedReachabilityError::Input(EdgeInputError::SchemaChanged)
    );
    assert_eq!(restored, state);
    let proof = CommittedSchemaCompatibility::attest_name_only_catalog(
        &cx,
        &next,
        GraphId(1),
        BranchId(1),
        SchemaEpoch(1),
    );
    let delta = restored
        .prepare_catalog_successor(&cx, &proof, LIMBS, &mut allow)
        .unwrap()
        .commit();
    assert_eq!(delta.len(), 3);
    for source in [1, 2, 3] {
        assert!(restored.contains(VId(source), VId(4)));
    }
    assert_eq!(restored.frontier(), CommitSeq(3));
}
