use super::*;
use crate::{CoordinateEntry, DeltaRow, LocalDeltaBatchIndex, LogicalDeltaTemplate};
use crate::zset::reachability::committed::{CommittedReachability, CommittedReachabilityError};
use crate::zset::reachability::ReachabilityError;
use fgdb_types::{MarkerRef, ObjectId, VId};
use std::collections::{BTreeMap, BTreeSet};

const LIMBS: LimbLimit = LimbLimit::new(4);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn edge(id: u128, relation: RelationId, from: u128, to: u128) -> DeltaRow {
    DeltaRow::CreateEdge { eid: EId(id), birth_ordinal: id as u64,
        src: VId(from), relation, dst: VId(to), canonical_key: None,
        props: vec![], valid_time: None }
}
fn coordinate(relation: RelationId, rows: Vec<DeltaRow>) -> CoordinateEntry {
    CoordinateEntry { graph: GraphId(1), branch: BranchId(1), relation,
        schema_epoch: SchemaEpoch(0), schema_transition: None, rows }
}
fn batch(at: u64, coordinates: Vec<CoordinateEntry>) -> LogicalDeltaBatch {
    let template = LogicalDeltaTemplate::build(ObjectId([1; 32]), [2; 32], coordinates).unwrap();
    LogicalDeltaBatch::from_parts_for_test(template.coordinate_entries().to_vec(), [at as u8; 32],
        MarkerRef { marker_oid: ObjectId([at as u8; 32]), commit_seq: CommitSeq(at) },
        CommitSeq(at), CommitSeq(at))
}
fn initial() -> LogicalDeltaBatch {
    batch(1, vec![coordinate(R, vec![edge(1, R, 1, 2), edge(2, R, 1, 2), edge(3, R, 2, 3)]),
        coordinate(S, vec![edge(4, S, 3, 4)])])
}
fn baseline(
    anchor: &LogicalDeltaBatch,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), usize>,
) -> Result<EdgeSnapshot, SnapshotInputError<usize>> {
    let mut builder = EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), Some(anchor), control)?;
    // Include an empty relation's current epoch; no old delta needs replay.
    builder.record_epoch(RelationId(9), SchemaEpoch(7), control)?;
    for (id, relation, from, to) in [(1, R, 1, 2), (2, R, 1, 2), (3, R, 2, 3), (4, S, 3, 4)] {
        builder.insert(EId(id), (relation, VId(from), VId(to)), SchemaEpoch(0), LIMBS, control)?;
    }
    builder.finish(control)
}

#[test]
fn snapshot_baseline_preserves_identity_bags_then_retracts_cross_relation_cascades() {
    let anchor = initial();
    let snapshot = baseline(&anchor, &mut allow).unwrap();
    assert_eq!(snapshot.frontier(), CommitSeq(1));
    assert_eq!(snapshot.rows().weight(&(R, VId(1), VId(2))).unwrap().to_i128(), Some(2));
    let mut input = snapshot.into_input();
    let mut index = LocalDeltaBatchIndex::new();
    index.insert(anchor).unwrap();
    assert!(input.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
    index.insert(batch(2, vec![coordinate(R, vec![
        DeltaRow::DeleteVertex { vid: VId(2), before_version: ObjectId([8; 32]),
            sorted_retired_incident_edges: vec![EId(1), EId(2), EId(3)] },
        DeltaRow::DeleteEdge { eid: EId(1), before_version: ObjectId([8; 32]) },
    ]), coordinate(S, vec![
        DeltaRow::DeleteVertex { vid: VId(3), before_version: ObjectId([8; 32]),
            sorted_retired_incident_edges: vec![EId(3), EId(4)] },
    ])])).unwrap();
    let delta = input.prepare_next(&index, LIMBS, &mut allow).unwrap().unwrap().commit();
    assert_eq!(delta.weight(&(R, VId(1), VId(2))).unwrap().to_i128(), Some(-2));
    assert_eq!(input.edge_count(), 0);
    let mut changed = coordinate(RelationId(9), vec![]);
    changed.schema_epoch = SchemaEpoch(8);
    index.insert(batch(3, vec![changed])).unwrap();
    assert_eq!(input.prepare_next(&index, LIMBS, &mut allow).unwrap_err(), EdgeInputError::SchemaChanged);
    assert_eq!(input.frontier(), CommitSeq(2));
}

#[test]
fn missing_prefix_is_unneeded_and_boundary_identity_survives_but_forks_refuse() {
    let mut index = LocalDeltaBatchIndex::new();
    index.insert(initial()).unwrap();
    let anchor = batch(2, vec![coordinate(R, vec![])]);
    index.insert(anchor.clone()).unwrap();
    index.retire_prefix(CommitSeq(1)).unwrap();
    let mut input = baseline(&anchor, &mut allow).unwrap().into_input();
    assert!(input.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
    let before = input.snapshot(LIMBS, &mut allow).unwrap();
    for marker in [false, true] {
        let changed = LogicalDeltaBatch::from_parts_for_test(anchor.coordinate_entries().to_vec(),
            if marker { *anchor.source_template_digest() } else { [99; 32] },
            MarkerRef { marker_oid: if marker { ObjectId([99; 32]) } else { anchor.commit_marker_identity().marker_oid },
                commit_seq: CommitSeq(2) }, CommitSeq(2), CommitSeq(2));
        let fork = LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(1), CommitSeq(2),
            vec![(CommitSeq(2), changed)]);
        assert_eq!(input.prepare_next(&fork, LIMBS, &mut allow).unwrap_err(),
            EdgeInputError::HistoryChanged { at: CommitSeq(2) });
        assert_eq!(input.snapshot(LIMBS, &mut allow).unwrap(), before);
    }
    index.retire_prefix(CommitSeq(2)).unwrap();
    assert!(input.prepare_next(&index, LIMBS, &mut allow).unwrap().is_none());
    let unanchored = LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(2), CommitSeq(2), vec![]);
    assert_eq!(input.prepare_next(&unanchored, LIMBS, &mut allow).unwrap_err(),
        EdgeInputError::AnchorUnavailable { at: CommitSeq(2) });
}

#[test]
fn every_builder_checkpoint_refuses_without_an_exportable_partial_baseline() {
    let anchor = initial();
    let mut calls = 0;
    let complete = baseline(&anchor, &mut |_| { calls += 1; Ok(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert_eq!(baseline(&anchor, &mut |_| {
            seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
        }).unwrap_err(), SnapshotInputError::Input(EdgeInputError::Delta(ZSetError::Control(stop))));
        assert_eq!(seen, stop);
        let retried = baseline(&anchor, &mut allow).unwrap();
        assert_eq!(retried.rows(), complete.rows());
        assert_eq!(retried.frontier(), complete.frontier());
    }
    let mut builder = EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), Some(&anchor), &mut allow).unwrap();
    assert!(builder.insert(EId(1), (R, VId(1), VId(2)), SchemaEpoch(0), LIMBS, &mut |_| Err(7)).is_err());
    assert_eq!(builder.record_epoch(R, SchemaEpoch(0), &mut allow), Err(SnapshotInputError::Refused));
    assert!(matches!(builder.finish(&mut allow), Err(SnapshotInputError::Refused)));
}

#[test]
fn origin_duplicates_inconsistent_schemas_and_malformed_anchors_fail_closed() {
    let origin = EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), None, &mut allow).unwrap()
        .finish(&mut allow).unwrap();
    assert_eq!(origin.frontier(), CommitSeq::ORIGIN);
    assert!(origin.rows().is_empty());
    let mut builder = EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), None, &mut allow).unwrap();
    assert_eq!(builder.insert(EId(1), (R, VId(1), VId(2)), SchemaEpoch(0), LIMBS, &mut allow),
        Err(SnapshotInputError::NonEmptyOrigin));
    let anchor = initial();
    for duplicate in [false, true] {
        let mut builder = EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), Some(&anchor), &mut allow).unwrap();
        builder.insert(EId(1), (R, VId(1), VId(2)), SchemaEpoch(0), LIMBS, &mut allow).unwrap();
        let error = if duplicate {
            builder.insert(EId(1), (R, VId(1), VId(2)), SchemaEpoch(0), LIMBS, &mut allow)
        } else {
            builder.record_epoch(R, SchemaEpoch(1), &mut allow)
        }.unwrap_err();
        assert_eq!(error, SnapshotInputError::Input(if duplicate { EdgeInputError::DuplicateEdge }
            else { EdgeInputError::SchemaChanged }));
        assert!(matches!(builder.finish(&mut allow), Err(SnapshotInputError::Refused)));
    }
    let zero = batch(0, vec![]);
    assert!(matches!(EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), Some(&zero), &mut allow),
        Err(SnapshotInputError::OriginAnchor)));
    for marker in [false, true] {
        let bad = LogicalDeltaBatch::from_parts_for_test(vec![], [1; 32],
            MarkerRef { marker_oid: ObjectId([1; 32]), commit_seq: CommitSeq(if marker { 2 } else { 1 }) },
            CommitSeq(1), CommitSeq(if marker { 1 } else { 2 }));
        assert!(matches!(EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), Some(&bad), &mut allow),
            Err(SnapshotInputError::Input(EdgeInputError::Index(_)))));
    }
}

fn closure(edges: &BTreeMap<EId, EdgeTuple>, relation: RelationId) -> BTreeSet<(VId, VId)> {
    let mut pairs: BTreeSet<_> = edges.values().filter(|tuple| tuple.0 == relation)
        .map(|tuple| (tuple.1, tuple.2)).collect();
    let vertices: BTreeSet<_> = pairs.iter().flat_map(|&(a, b)| [a, b]).collect();
    for middle in &vertices {
        for source in &vertices {
            for destination in &vertices {
                if pairs.contains(&(*source, *middle)) && pairs.contains(&(*middle, *destination)) {
                    pairs.insert((*source, *destination));
                }
            }
        }
    }
    pairs
}

#[test]
fn rebased_recursive_views_match_replay_and_independent_closure_through_future_deletes() {
    let mut index = LocalDeltaBatchIndex::new();
    let mut identities = BTreeMap::<EId, EdgeTuple>::new();
    let mut replay = CommittedReachability::new(GraphId(1), BranchId(1), R);
    let mut baseline_view: Option<CommittedReachability> = None;
    let mut random = 13_u64;
    let mut fresh = 0_u128;
    for seq in 1..=160 {
        let mut r = Vec::new();
        let mut s = Vec::new();
        for _ in 0..3 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let (relation, row) = if random % 3 == 0 && !identities.is_empty() {
                let (&id, &tuple) = identities.iter().nth(random as usize % identities.len()).unwrap();
                identities.remove(&id);
                (tuple.0, DeltaRow::DeleteEdge { eid: id, before_version: ObjectId([8; 32]) })
            } else {
                fresh += 1;
                let relation = if random & 1 == 0 { R } else { S };
                // Preserve 128-bit identities, including in cycles and self pairs.
                let wide = 1_u128 << 100;
                let from = wide + u128::from((random >> 8) % 5);
                let to = wide + u128::from((random >> 16) % 5);
                identities.insert(EId(fresh), (relation, VId(from), VId(to)));
                (relation, edge(fresh, relation, from, to))
            };
            if relation == R { r.push(row); } else { s.push(row); }
        }
        let next = batch(seq, vec![coordinate(R, r), coordinate(S, s)]);
        index.insert(next.clone()).unwrap();
        replay.prepare_next(&index, LIMBS, &mut allow).unwrap().unwrap().commit();
        if let Some(view) = &mut baseline_view {
            view.prepare_next(&index, LIMBS, &mut allow).unwrap().unwrap().commit();
            assert_eq!(view.pairs().collect::<BTreeSet<_>>(), closure(&identities, R));
        }
        let mut builder = EdgeSnapshotBuilder::new(GraphId(1), BranchId(1), Some(&next), &mut allow).unwrap();
        for relation in [R, S] { builder.record_epoch(relation, SchemaEpoch(0), &mut allow).unwrap(); }
        for (&id, &tuple) in &identities {
            builder.insert(id, tuple, SchemaEpoch(0), LIMBS, &mut allow).unwrap();
        }
        let view = CommittedReachability::from_snapshot(builder.finish(&mut allow).unwrap(), R, LIMBS, &mut allow).unwrap();
        assert_eq!(view.pairs().collect::<BTreeSet<_>>(), closure(&identities, R));
        assert_eq!(view, replay);
        baseline_view = Some(view);
        // Only the current anchor remains; no reconstructed prefix is needed.
        if seq > 1 { index.retire_prefix(CommitSeq(seq - 1)).unwrap(); }
    }
}

#[test]
fn every_recursive_baseline_checkpoint_is_private_and_retryable() {
    let anchor = initial();
    let mut calls = 0;
    let expected = CommittedReachability::from_snapshot(baseline(&anchor, &mut allow).unwrap(),
        R, LIMBS, &mut |_| { calls += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        let error = CommittedReachability::from_snapshot(baseline(&anchor, &mut allow).unwrap(),
            R, LIMBS, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }).unwrap_err();
        assert_eq!(error, CommittedReachabilityError::Reachability(
            ReachabilityError::Delta(ZSetError::Control(stop))));
        assert_eq!(seen, stop);
        let actual = CommittedReachability::from_snapshot(baseline(&anchor, &mut allow).unwrap(),
            R, LIMBS, &mut allow).unwrap();
        assert_eq!(actual, expected);
    }
}

#[path = "retired_tests.rs"]
mod retired_tests;
