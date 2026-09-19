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
