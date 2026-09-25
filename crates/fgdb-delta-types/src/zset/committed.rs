//! Chronicle-batch adapter for an incremental edge relation.
//!
//! This is a derived input arrangement, NOT another graph store, an alternate
//! commit log, or a durable view scheduler. Feed it the authenticated window
//! returned by `Database::delta_index()`. It retains edge identities solely to
//! translate edge/vertex deletion before-images into exact signed tuples.
//! Properties, labels and valid-time filtering are not part of this projection.
//! Schema/constraint changes refuse rather than silently changing its meaning.
//!
//! Every tick consumes one WHOLE committed batch, including all of its relation
//! coordinates. The input arrangement and consumed marker advance only when the
//! prepared guard commits. Prepare downstream operators/sinks first; dropping
//! any guard leaves this input retryable at the same sequence. Publication has
//! no recoverable callbacks, with the parent Z-set allocation/panic boundary.
//!
//! A retained marker AND template digest anchor continuation to the previously
//! consumed history. Fresh authoritative baselines use [`snapshot`]; no
//! arbitrary cursor setter or import into an existing input exists.
//! Retirement keeps one exact boundary identity, without retaining its rows.
//! Older cursors still need an authoritative baseline; no retention lease or
//! cryptographic chain proof is invented here.
//! Marker/digest identity checks are not cryptographic verification of hostile
//! payloads: authentication and complete cascade/version validation belong to
//! the source's Chronicle/Strata apply path, not to this projection.

pub mod snapshot;

use super::{ZSet, ZSetError, ZSetEvent, event};
use crate::{
    DELTA_FORMAT_V1, DeltaRow, INDEX_FORMAT_V1, IndexError, LimbLimit, LocalDeltaBatchIndex,
    LogicalDeltaBatch, RelationId, SchemaEpoch, ZWeight,
};
use fgdb_types::{BranchId, CommitCx, CommitSeq, EId, GraphId, MarkerRef, VId};
use std::collections::{BTreeMap, btree_map::Entry};

/// Parallel edges consolidate to a bag weight; their EIds remain in the input
/// arrangement so removing one edge retracts exactly one occurrence.
pub type EdgeTuple = (RelationId, VId, VId);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgeInputError<E> {
    Index(IndexError),
    Delta(ZSetError<E>),
    MissingBatch { at: CommitSeq },
    UnsupportedBatchFormat { found: u16 },
    AnchorUnavailable { at: CommitSeq },
    HistoryChanged { at: CommitSeq },
    SchemaChanged,
    DuplicateEdge,
    UnknownEdge,
    WrongRelation,
    NonIncidentCascade,
}

impl<E: core::fmt::Display> core::fmt::Display for EdgeInputError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Index(error) => error.fmt(f),
            Self::Delta(error) => error.fmt(f),
            Self::MissingBatch { at } => write!(f, "missing committed batch at {at:?}"),
            Self::UnsupportedBatchFormat { found } => write!(f, "unsupported delta format {found}"),
            Self::AnchorUnavailable { at } => write!(f, "delta anchor unavailable at {at:?}"),
            Self::HistoryChanged { at } => write!(f, "delta history changed at {at:?}"),
            Self::SchemaChanged => {
                f.write_str("edge input requires rebaseline after schema change")
            }
            Self::DuplicateEdge => f.write_str("duplicate edge in committed input"),
            Self::UnknownEdge => f.write_str("unknown edge in committed deletion"),
            Self::WrongRelation => f.write_str("edge and coordinate relations disagree"),
            Self::NonIncidentCascade => {
                f.write_str("cascade edge is not incident to deleted vertex")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for EdgeInputError<E> {}
impl<E> From<ZSetError<E>> for EdgeInputError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E> From<IndexError> for EdgeInputError<E> {
    fn from(error: IndexError) -> Self {
        Self::Index(error)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Anchor {
    marker: MarkerRef,
    template: [u8; 32],
}
impl Anchor {
    fn of(batch: &LogicalDeltaBatch) -> Self {
        Self {
            marker: batch.commit_marker_identity(),
            template: *batch.source_template_digest(),
        }
    }
}

/// Fixed graph/branch topology input starting at the stream origin. Ordinary
/// ticks visit only their batch and touched EIds, not all retained edges/history.
/// This type does not retain vertex properties or expose graph-query execution.
#[derive(PartialEq, Eq)]
pub struct CommittedEdgeInput {
    graph: GraphId,
    branch: BranchId,
    anchor: Option<Anchor>,
    edges: BTreeMap<EId, EdgeTuple>,
    epochs: BTreeMap<RelationId, SchemaEpoch>,
}

impl core::fmt::Debug for CommittedEdgeInput {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CommittedEdgeInput")
            .field("frontier", &self.frontier())
            .field("edge_count", &self.edges.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

impl CommittedEdgeInput {
    pub fn new(graph: GraphId, branch: BranchId) -> Self {
        Self {
            graph,
            branch,
            anchor: None,
            edges: BTreeMap::new(),
            epochs: BTreeMap::new(),
        }
    }

    pub fn frontier(&self) -> CommitSeq {
        self.anchor
            .map_or(CommitSeq::ORIGIN, |anchor| anchor.marker.commit_seq)
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Explicit arrangement export for a snapshot/audit. Normal ticks never
    /// call it. The caller's logical work/scratch and promoted-weight limits
    /// govern this materialization just like any other Z-set value operation.
    pub fn snapshot<E>(
        &self,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<EdgeTuple>, EdgeInputError<E>> {
        let mut output = ZSet::new();
        for &tuple in self.edges.values() {
            output.accumulate(tuple, ZWeight::ONE, limbs, control)?;
        }
        Ok(output)
    }

    /// Prepare exactly the next sequence, or `None` after checking a caught-up
    /// source. A batch outside this graph/branch still advances the global
    /// sequence with an empty delta. Never filter the stream before this method.
    ///
    /// The returned delta is tentative until committed. Apply it to downstream
    /// prepared guards, then publish all guards without intervening fallible
    /// work. A zero-output tick must also commit, or later deletion state and
    /// history tracking would remain stale.
    pub fn prepare_next<E>(
        &mut self,
        index: &LocalDeltaBatchIndex,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Option<EdgeInputUpdate<'_>>, EdgeInputError<E>> {
        event(control, ZSetEvent::Work)?;
        let after = self.frontier();
        if index.format() != INDEX_FORMAT_V1 {
            return Err(IndexError::UnsupportedFormat {
                format: index.format(),
            }
            .into());
        }
        // The window's own API owns future/retired cursor semantics. Do not
        // clamp, skip a missing batch, or trust a filtered iterator as a cursor.
        let _suffix = index.since(after)?;
        if let Some(anchor) = self.anchor
            && checked_anchor(index, after)? != anchor
        {
            return Err(EdgeInputError::HistoryChanged { at: after });
        }
        if after == index.frontier() {
            return Ok(None);
        }
        let next = after.checked_successor().map_err(IndexError::from)?;
        let batch = checked_batch(index, next)?;
        self.prepare_batch(batch, limbs, control).map(Some)
    }

    /// Consume the next batch from the SAME live, authenticated commit source.
    /// The caller must be its commit-purpose publisher and must deliver every
    /// whole global batch in order. This avoids cloning a history window for a
    /// post-commit hook; it uses the identical envelope and topology kernel as
    /// `prepare_next`. Duplicates, gaps and malformed envelopes refuse before
    /// staging. There is no independent acknowledgement or cursor setter.
    ///
    /// `CommitCx` is an authority witness, NOT a cryptographic ancestry proof.
    /// The publisher owns source continuity and authentication, as with
    /// `CommittedMarker::attest`. Detached/query consumers must use
    /// `prepare_next`, which still verifies the retained marker/template anchor
    /// on every call. A later indexed call also verifies anchors established by
    /// this method. Do not use this entrypoint to switch histories.
    ///
    /// A query-purpose caller cannot select this publication lane:
    /// ```compile_fail,E0308
    /// use fgdb_delta_types::{LimbLimit, LogicalDeltaBatch};
    /// use fgdb_delta_types::zset::committed::CommittedEdgeInput;
    /// use fgdb_types::QueryCx;
    /// fn refuse(input: &mut CommittedEdgeInput, batch: &LogicalDeltaBatch, cx: &QueryCx) {
    ///     let _ = input.prepare_committed_successor(
    ///         cx, batch, LimbLimit::new(4), &mut |_| Ok::<_, ()>(()));
    /// }
    /// ```
    pub fn prepare_committed_successor<E>(
        &mut self,
        _cx: &CommitCx,
        batch: &LogicalDeltaBatch,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<EdgeInputUpdate<'_>, EdgeInputError<E>> {
        event(control, ZSetEvent::Work)?;
        let next = self
            .frontier()
            .checked_successor()
            .map_err(IndexError::from)?;
        validate_batch(batch, next)?;
        self.prepare_batch(batch, limbs, control)
    }

    fn prepare_batch<E>(
        &mut self,
        batch: &LogicalDeltaBatch,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<EdgeInputUpdate<'_>, EdgeInputError<E>> {
        let anchor = Anchor::of(batch);
        let mut created = BTreeMap::new();
        let mut removed = BTreeMap::new();
        let mut epochs = BTreeMap::new();
        let mut delta = ZSet::new();

        // Coordinates are canonically sorted, not an execution schedule. Read
        // every creation before processing deletion/cascade references, so a
        // cross-relation cascade may name an edge created later in that order.
        for coordinate in batch.coordinate_entries() {
            event(control, ZSetEvent::Work)?;
            if coordinate.graph != self.graph || coordinate.branch != self.branch {
                continue;
            }
            if coordinate.schema_transition.is_some() {
                return Err(EdgeInputError::SchemaChanged);
            }
            let known = epochs
                .get(&coordinate.relation)
                .or_else(|| self.epochs.get(&coordinate.relation));
            match known {
                Some(epoch) if *epoch != coordinate.schema_epoch => {
                    return Err(EdgeInputError::SchemaChanged);
                }
                Some(_) => {}
                None => {
                    event(control, ZSetEvent::ScratchEntry)?;
                    event(control, ZSetEvent::ScratchEntry)?;
                    epochs.insert(coordinate.relation, coordinate.schema_epoch);
                }
            }
            for row in &coordinate.rows {
                event(control, ZSetEvent::Work)?;
                match row {
                    DeltaRow::CreateEdge {
                        eid,
                        src,
                        relation,
                        dst,
                        ..
                    } => {
                        if *relation != coordinate.relation {
                            return Err(EdgeInputError::WrongRelation);
                        }
                        if self.edges.contains_key(eid) || created.contains_key(eid) {
                            return Err(EdgeInputError::DuplicateEdge);
                        }
                        // Reserve staging plus eventual retained identity before
                        // either grows; a same-tick deletion may leave it unused.
                        event(control, ZSetEvent::ScratchEntry)?;
                        event(control, ZSetEvent::ScratchEntry)?;
                        let tuple = (*relation, *src, *dst);
                        created.insert(*eid, tuple);
                        delta.accumulate(tuple, ZWeight::ONE, limbs, control)?;
                    }
                    DeltaRow::Schema { .. } | DeltaRow::Constraint { .. } => {
                        return Err(EdgeInputError::SchemaChanged);
                    }
                    DeltaRow::CreateVertex { .. }
                    | DeltaRow::DeleteVertex { .. }
                    | DeltaRow::DeleteEdge { .. }
                    | DeltaRow::LabelMembership { .. }
                    | DeltaRow::Property { .. }
                    | DeltaRow::ValidTime { .. }
                    | DeltaRow::Counter { .. }
                    | DeltaRow::Escrow { .. }
                    | DeltaRow::Sketch { .. } => {}
                }
            }
        }
        for coordinate in batch.coordinate_entries() {
            event(control, ZSetEvent::Work)?;
            if coordinate.graph != self.graph || coordinate.branch != self.branch {
                continue;
            }
            for row in &coordinate.rows {
                event(control, ZSetEvent::Work)?;
                match row {
                    DeltaRow::DeleteEdge { eid, .. } => {
                        let tuple = created
                            .get(eid)
                            .or_else(|| self.edges.get(eid))
                            .ok_or(EdgeInputError::UnknownEdge)?;
                        if tuple.0 != coordinate.relation {
                            return Err(EdgeInputError::WrongRelation);
                        }
                        remove_once(*eid, *tuple, &mut removed, &mut delta, limbs, control)?;
                    }
                    DeltaRow::DeleteVertex {
                        vid,
                        sorted_retired_incident_edges,
                        ..
                    } => {
                        // Cascade before-images can cross relation coordinates.
                        // An explicit deletion or another endpoint's cascade can
                        // name the same EId: its occurrence retracts only once.
                        for eid in sorted_retired_incident_edges {
                            event(control, ZSetEvent::Work)?;
                            let tuple = created
                                .get(eid)
                                .or_else(|| self.edges.get(eid))
                                .ok_or(EdgeInputError::UnknownEdge)?;
                            if tuple.1 != *vid && tuple.2 != *vid {
                                return Err(EdgeInputError::NonIncidentCascade);
                            }
                            remove_once(*eid, *tuple, &mut removed, &mut delta, limbs, control)?;
                        }
                    }
                    _ => {}
                }
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(EdgeInputUpdate {
            owner: self,
            anchor,
            created,
            removed,
            epochs,
            delta,
        })
    }
}

/// Retention preserves exactly one boundary identity, not an arbitrary older
/// checkpoint. `since(after)` still refuses before this when any delta is lost.
fn checked_anchor<E>(
    index: &LocalDeltaBatchIndex,
    at: CommitSeq,
) -> Result<Anchor, EdgeInputError<E>> {
    if index.get(at).is_some() {
        return Ok(Anchor::of(checked_batch(index, at)?));
    }
    if at == index.retained_after_commit_seq()
        && let Some((format, marker, template)) = index.retired_boundary_identity()
    {
        if format != DELTA_FORMAT_V1 {
            return Err(EdgeInputError::UnsupportedBatchFormat { found: format });
        }
        if marker.commit_seq != at {
            return Err(IndexError::WrongMarker {
                batch_commit_seq: at,
                marker_commit_seq: marker.commit_seq,
            }
            .into());
        }
        return Ok(Anchor { marker, template });
    }
    Err(EdgeInputError::AnchorUnavailable { at })
}

fn checked_batch<E>(
    index: &LocalDeltaBatchIndex,
    at: CommitSeq,
) -> Result<&LogicalDeltaBatch, EdgeInputError<E>> {
    let batch = index.get(at).ok_or(EdgeInputError::MissingBatch { at })?;
    validate_batch(batch, at)?;
    Ok(batch)
}

fn validate_batch<E>(batch: &LogicalDeltaBatch, at: CommitSeq) -> Result<(), EdgeInputError<E>> {
    if batch.format() != DELTA_FORMAT_V1 {
        return Err(EdgeInputError::UnsupportedBatchFormat {
            found: batch.format(),
        });
    }
    if batch.commit_seq() != at {
        return Err(IndexError::WrongEntryKey {
            stored: at,
            batch: batch.commit_seq(),
        }
        .into());
    }
    if batch.commit_marker_identity().commit_seq != at {
        return Err(IndexError::WrongMarker {
            batch_commit_seq: at,
            marker_commit_seq: batch.commit_marker_identity().commit_seq,
        }
        .into());
    }
    if batch.frontier() != at {
        return Err(IndexError::WrongFrontier {
            commit_seq: at,
            frontier: batch.frontier(),
        }
        .into());
    }
    Ok(())
}

fn remove_once<E>(
    eid: EId,
    tuple: EdgeTuple,
    removed: &mut BTreeMap<EId, EdgeTuple>,
    delta: &mut ZSet<EdgeTuple>,
    limbs: LimbLimit,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), EdgeInputError<E>> {
    if let Entry::Vacant(entry) = removed.entry(eid) {
        event(control, ZSetEvent::ScratchEntry)?;
        entry.insert(tuple);
        delta.accumulate(tuple, ZWeight::from_i128(-1), limbs, control)?;
    }
    Ok(())
}

/// Pending input transition. No `Clone`, arbitrary sequence setter, or separate
/// acknowledgement exists: input state and its committed provenance move once.
#[must_use = "dropping an input update aborts it"]
pub struct EdgeInputUpdate<'a> {
    owner: &'a mut CommittedEdgeInput,
    anchor: Anchor,
    created: BTreeMap<EId, EdgeTuple>,
    removed: BTreeMap<EId, EdgeTuple>,
    epochs: BTreeMap<RelationId, SchemaEpoch>,
    delta: ZSet<EdgeTuple>,
}
impl EdgeInputUpdate<'_> {
    pub fn delta(&self) -> &ZSet<EdgeTuple> {
        &self.delta
    }
    pub fn commit_seq(&self) -> CommitSeq {
        self.anchor.marker.commit_seq
    }

    pub fn commit(self) -> ZSet<EdgeTuple> {
        let Self {
            owner,
            anchor,
            created,
            removed,
            epochs,
            delta,
        } = self;
        for (eid, tuple) in created {
            if !removed.contains_key(&eid) {
                owner.edges.insert(eid, tuple);
            }
        }
        for eid in removed.keys() {
            owner.edges.remove(eid);
        }
        owner.epochs.extend(epochs);
        owner.anchor = Some(anchor);
        delta
    }
}
impl core::fmt::Debug for EdgeInputUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EdgeInputUpdate")
            .field("commit_seq", &self.commit_seq())
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    include!("committed_successor_tests.rs");

    use super::*;
    use crate::{CoordinateEntry, LogicalDeltaTemplate};
    use fgdb_types::ObjectId;

    const LIMBS: LimbLimit = LimbLimit::new(16);
    fn allow(_: ZSetEvent) -> Result<(), usize> {
        Ok(())
    }
    fn input() -> CommittedEdgeInput {
        CommittedEdgeInput::new(GraphId(1), BranchId(1))
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
    fn cascade(vid: u128, edges: &[u128]) -> DeltaRow {
        DeltaRow::DeleteVertex {
            vid: VId(vid),
            before_version: ObjectId([8; 32]),
            sorted_retired_incident_edges: edges.iter().copied().map(EId).collect(),
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
    // Deliberately decoded/test-shaped batches exercise envelope checks without
    // manufacturing commit-purpose authority. Real attestation is exercised by
    // the database integration tests using its actual committed delta index.
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
    fn advance(state: &mut CommittedEdgeInput, index: &LocalDeltaBatchIndex) -> ZSet<EdgeTuple> {
        state
            .prepare_next(index, LIMBS, &mut allow)
            .unwrap()
            .unwrap()
            .commit()
    }
    fn seed() -> (CommittedEdgeInput, LocalDeltaBatchIndex) {
        let mut index = LocalDeltaBatchIndex::new();
        index
            .insert(batch(
                1,
                vec![
                    coordinate(1, vec![create(1, 1, 1, 2), create(2, 1, 1, 2)]),
                    coordinate(2, vec![create(3, 2, 2, 3)]),
                ],
            ))
            .unwrap();
        let mut state = input();
        advance(&mut state, &index);
        (state, index)
    }
    fn plain(value: &ZSet<EdgeTuple>) -> BTreeMap<EdgeTuple, i128> {
        value
            .iter()
            .map(|(tuple, weight)| (*tuple, weight.to_i128().unwrap()))
            .collect()
    }

    #[test]
    fn whole_batch_cascades_cross_relations_and_retract_shared_edges_once() {
        let (mut state, mut index) = seed();
        let initial = state.snapshot(LIMBS, &mut allow).unwrap();
        // A later coordinate creates edge 4; the earlier coordinate's cascade
        // names it. Two endpoint cascades and an explicit delete overlap.
        index
            .insert(batch(
                2,
                vec![
                    coordinate(
                        1,
                        vec![cascade(1, &[1, 2]), cascade(2, &[1, 2, 3, 4]), delete(1)],
                    ),
                    coordinate(2, vec![create(4, 2, 2, 9)]),
                ],
            ))
            .unwrap();
        let delta = advance(&mut state, &index);
        assert!(initial.plus(&delta, LIMBS, &mut allow).unwrap().is_empty());
        assert_eq!(state.edge_count(), 0);
        assert_eq!(state.frontier(), CommitSeq(2));
        assert!(
            state
                .prepare_next(&index, LIMBS, &mut allow)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn silent_identity_replacement_and_unrelated_commits_still_advance() {
        let mut state = input();
        let mut index = LocalDeltaBatchIndex::new();
        index
            .insert(batch(1, vec![coordinate(1, vec![create(1, 1, 2, 3)])]))
            .unwrap();
        advance(&mut state, &index);
        index
            .insert(batch(
                2,
                vec![coordinate(1, vec![delete(1), create(2, 1, 2, 3)])],
            ))
            .unwrap();
        assert!(advance(&mut state, &index).is_empty());
        assert!(!state.edges.contains_key(&EId(1)));
        assert!(state.edges.contains_key(&EId(2)));
        let mut foreign_graph = coordinate(1, vec![create(3, 1, 2, 3)]);
        foreign_graph.graph = GraphId(2);
        let mut foreign_branch = coordinate(1, vec![create(4, 1, 2, 3)]);
        foreign_branch.branch = BranchId(2);
        index
            .insert(batch(3, vec![foreign_graph, foreign_branch]))
            .unwrap();
        assert!(advance(&mut state, &index).is_empty());
        assert_eq!(state.frontier(), CommitSeq(3));
        index
            .insert(batch(4, vec![coordinate(1, vec![delete(2)])]))
            .unwrap();
        assert_eq!(
            plain(&advance(&mut state, &index)),
            BTreeMap::from([((RelationId(1), VId(2), VId(3)), -1)])
        );
        assert_eq!(state.edge_count(), 0);
    }

    #[test]
    fn every_tick_boundary_and_dropped_downstream_preparation_is_retryable() {
        let (mut success, mut index) = seed();
        index
            .insert(batch(
                2,
                vec![
                    coordinate(1, vec![delete(1), create(4, 1, 3, 4)]),
                    coordinate(2, vec![cascade(2, &[2, 3])]),
                ],
            ))
            .unwrap();
        let mut events = Vec::new();
        let wanted = success
            .prepare_next(&index, LIMBS, &mut |event| {
                events.push(event);
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap()
            .commit();
        for stop in 1..=events.len() {
            let (mut state, _) = seed();
            let mut seen = 0;
            assert_eq!(
                state
                    .prepare_next(&index, LIMBS, &mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    })
                    .unwrap_err(),
                EdgeInputError::Delta(ZSetError::Control(stop))
            );
            assert_eq!(seen, stop);
            assert_eq!(state, seed().0);
            assert_eq!(advance(&mut state, &index), wanted);
            assert_eq!(state, success);
        }
        let (mut state, _) = seed();
        let pending = state
            .prepare_next(&index, LIMBS, &mut allow)
            .unwrap()
            .unwrap();
        assert_eq!(pending.delta(), &wanted);
        assert_eq!(pending.commit_seq(), CommitSeq(2));
        drop(pending);
        assert_eq!(state, seed().0);
        assert_eq!(advance(&mut state, &index), wanted);
    }

    #[test]
    fn index_gaps_wrong_envelopes_forks_and_retired_anchors_fail_closed() {
        let (mut state, index) = seed();
        let original = index.get(CommitSeq(1)).unwrap();
        for change_marker in [false, true] {
            let changed = LogicalDeltaBatch::from_parts_for_test(
                original.coordinate_entries().to_vec(),
                if change_marker {
                    *original.source_template_digest()
                } else {
                    [99; 32]
                },
                MarkerRef {
                    marker_oid: if change_marker {
                        ObjectId([99; 32])
                    } else {
                        original.commit_marker_identity().marker_oid
                    },
                    commit_seq: CommitSeq(1),
                },
                CommitSeq(1),
                CommitSeq(1),
            );
            let fork = LocalDeltaBatchIndex::from_parts_for_test(
                CommitSeq(0),
                CommitSeq(1),
                vec![(CommitSeq(1), changed)],
            );
            assert_eq!(
                state.prepare_next(&fork, LIMBS, &mut allow).unwrap_err(),
                EdgeInputError::HistoryChanged { at: CommitSeq(1) }
            );
            assert_eq!(state, seed().0);
        }
        let mut retired = index.clone();
        retired.retire_prefix(CommitSeq(1)).unwrap();
        // Exact boundary identity survives retirement; a bare decoded floor
        // remains unanchored and cannot be promoted into evidence by a no-op.
        assert!(
            state
                .prepare_next(&retired, LIMBS, &mut allow)
                .unwrap()
                .is_none()
        );
        let unanchored =
            LocalDeltaBatchIndex::from_parts_for_test(CommitSeq(1), CommitSeq(1), vec![]);
        assert_eq!(
            state
                .prepare_next(&unanchored, LIMBS, &mut allow)
                .unwrap_err(),
            EdgeInputError::AnchorUnavailable { at: CommitSeq(1) }
        );
        assert!(matches!(
            input().prepare_next(&retired, LIMBS, &mut allow),
            Err(EdgeInputError::Index(IndexError::CursorRetired { .. }))
        ));
        assert!(matches!(
            state.prepare_next(&LocalDeltaBatchIndex::new(), LIMBS, &mut allow),
            Err(EdgeInputError::Index(IndexError::BeyondFrontier { .. }))
        ));
        let two = batch(2, vec![coordinate(1, vec![])]);
        let missing = LocalDeltaBatchIndex::from_parts_for_test(
            CommitSeq(0),
            CommitSeq(2),
            vec![(CommitSeq(2), two.clone())],
        );
        assert_eq!(
            input()
                .prepare_next(&missing, LIMBS, &mut allow)
                .unwrap_err(),
            EdgeInputError::MissingBatch { at: CommitSeq(1) }
        );
        let wrong_key = LocalDeltaBatchIndex::from_parts_for_test(
            CommitSeq(0),
            CommitSeq(1),
            vec![(CommitSeq(1), two)],
        );
        assert!(matches!(
            input().prepare_next(&wrong_key, LIMBS, &mut allow),
            Err(EdgeInputError::Index(IndexError::WrongEntryKey { .. }))
        ));
        for wrong_marker in [false, true] {
            let bad = LogicalDeltaBatch::from_parts_for_test(
                vec![],
                [0; 32],
                MarkerRef {
                    marker_oid: ObjectId([0; 32]),
                    commit_seq: CommitSeq(if wrong_marker { 2 } else { 1 }),
                },
                CommitSeq(1),
                CommitSeq(if wrong_marker { 1 } else { 2 }),
            );
            let corrupt = LocalDeltaBatchIndex::from_parts_for_test(
                CommitSeq(0),
                CommitSeq(1),
                vec![(CommitSeq(1), bad)],
            );
            let failure = input()
                .prepare_next(&corrupt, LIMBS, &mut allow)
                .unwrap_err();
            assert!(matches!(
                failure,
                EdgeInputError::Index(IndexError::WrongMarker { .. })
                    | EdgeInputError::Index(IndexError::WrongFrontier { .. })
            ));
        }
    }

    #[test]
    fn schema_and_inconsistent_edge_effects_never_publish_a_partial_tick() {
        for (row, wanted) in [
            (create(1, 1, 3, 4), EdgeInputError::DuplicateEdge),
            (delete(99), EdgeInputError::UnknownEdge),
            (delete(3), EdgeInputError::WrongRelation),
            (cascade(99, &[1]), EdgeInputError::NonIncidentCascade),
            (
                DeltaRow::Schema {
                    transition_oid: ObjectId([1; 32]),
                    before_epoch: SchemaEpoch(0),
                    after_epoch: SchemaEpoch(1),
                },
                EdgeInputError::SchemaChanged,
            ),
        ] {
            let (mut state, mut index) = seed();
            index
                .insert(batch(
                    2,
                    vec![coordinate(1, vec![create(88, 1, 8, 8), row])],
                ))
                .unwrap();
            assert_eq!(
                state.prepare_next(&index, LIMBS, &mut allow).unwrap_err(),
                wanted
            );
            assert_eq!(state, seed().0);
        }
        for explicit_transition in [false, true] {
            let (mut state, mut index) = seed();
            let mut changed = coordinate(1, vec![create(4, 1, 4, 4)]);
            if explicit_transition {
                changed.schema_transition = Some(ObjectId([2; 32]));
            } else {
                changed.schema_epoch = SchemaEpoch(1);
            }
            index.insert(batch(2, vec![changed])).unwrap();
            assert_eq!(
                state.prepare_next(&index, LIMBS, &mut allow).unwrap_err(),
                EdgeInputError::SchemaChanged
            );
            assert_eq!(state, seed().0);
        }
    }

    #[test]
    fn point_updates_do_not_scan_unaffected_edges_or_history() {
        let mut measurements = Vec::new();
        for unrelated in [0, 1000] {
            let mut index = LocalDeltaBatchIndex::new();
            let rows = (1..=unrelated + 1)
                .map(|id| create(id, 1, id, id + 1))
                .collect();
            index.insert(batch(1, vec![coordinate(1, rows)])).unwrap();
            let mut state = input();
            advance(&mut state, &index);
            index
                .insert(batch(2, vec![coordinate(1, vec![delete(1)])]))
                .unwrap();
            let mut events = Vec::new();
            let delta = state
                .prepare_next(&index, LIMBS, &mut |event| {
                    events.push(event);
                    Ok::<_, usize>(())
                })
                .unwrap()
                .unwrap()
                .commit();
            measurements.push((delta, events));
        }
        assert_eq!(measurements[0], measurements[1]);
    }

    #[test]
    fn seeded_identity_histories_match_independent_full_bag_recomputation() {
        let mut state = input();
        let mut index = LocalDeltaBatchIndex::new();
        let mut identities = BTreeMap::<u128, (u64, u128, u128)>::new();
        let mut integrated = ZSet::new();
        let mut random = 11_u64;
        let mut fresh = 0_u128;
        for seq in 1..=300 {
            let mut rows = BTreeMap::<u64, Vec<DeltaRow>>::new();
            for _ in 0..4 {
                random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
                if random.is_multiple_of(3) && !identities.is_empty() {
                    let at = (random as usize) % identities.len();
                    let (&eid, &(relation, _, _)) = identities.iter().nth(at).unwrap();
                    identities.remove(&eid);
                    rows.entry(relation).or_default().push(delete(eid));
                } else {
                    fresh += 1;
                    let (relation, source, target) = (
                        1 + random % 2,
                        u128::from((random >> 8) % 5),
                        u128::from((random >> 16) % 5),
                    );
                    identities.insert(fresh, (relation, source, target));
                    rows.entry(relation)
                        .or_default()
                        .push(create(fresh, relation, source, target));
                }
            }
            index
                .insert(batch(
                    seq,
                    rows.into_iter()
                        .map(|(relation, rows)| coordinate(relation, rows))
                        .collect(),
                ))
                .unwrap();
            integrated
                .integrate(&advance(&mut state, &index), LIMBS, &mut allow)
                .unwrap();
            let mut expected = BTreeMap::<EdgeTuple, i128>::new();
            for &(relation, source, target) in identities.values() {
                *expected
                    .entry((RelationId(relation), VId(source), VId(target)))
                    .or_default() += 1;
            }
            assert_eq!(plain(&integrated), expected, "sequence {seq}");
            assert_eq!(plain(&state.snapshot(LIMBS, &mut allow).unwrap()), expected);
            assert_eq!(state.edge_count(), identities.len());
            assert_eq!(state.frontier(), CommitSeq(seq));
        }
    }
}
