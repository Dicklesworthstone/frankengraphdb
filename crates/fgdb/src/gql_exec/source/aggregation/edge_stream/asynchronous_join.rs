//! Awaitable native joins over the resident snapshot's real persistent indexes.
//! No second graph or adjacency table is built. Source callbacks below are
//! synchronous indexed reads exposed as ready futures; disk-faulting sources
//! implement the same GQL contract separately rather than pretending to fit RAM.

use super::*;
use fgdb_gql::algebra::{EdgeRelation, GlaDirection};
use fgdb_gql::edge_stream::{
    AsyncEdgeCandidate, AsyncEdgeJoinCursor, AsyncEdgeJoinPlan, AsyncEdgeJoinSource,
    AsyncEdgeScanEvent, AsyncEdgeScanRecord, AsyncEdgeScanSource, AsyncIncidentCandidateResult,
    EdgeExpansionSourceError, EdgeScanRecord, VertexScanRecord,
};
use fgdb_gql::stream::VertexScanEvent;

struct Record {
    edge: EdgeScanRecord<'static>,
    source: Option<VertexScanRecord<'static>>,
    target: Option<VertexScanRecord<'static>>,
}
impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        self.edge.as_row()
    }
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        let edge = self.edge.as_row();
        if vid == edge.source {
            self.source.as_ref().map(VertexScanRecord::as_row)
        } else if vid == edge.target {
            self.target.as_ref().map(VertexScanRecord::as_row)
        } else {
            None
        }
    }
}

struct Source<'q>(SnapshotEdgeSource<'q>);
fn source_event(event: GlaExecutionEvent) -> AsyncEdgeScanEvent {
    match event {
        GlaExecutionEvent::ScratchEntry => AsyncEdgeScanEvent::ScratchEntry,
        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => AsyncEdgeScanEvent::Work,
    }
}
fn vertex_event(event: VertexScanEvent) -> AsyncEdgeScanEvent {
    match event {
        VertexScanEvent::ScratchEntry => AsyncEdgeScanEvent::ScratchEntry,
        VertexScanEvent::Work => AsyncEdgeScanEvent::Work,
    }
}
impl Source<'_> {
    // Admission happens before any history resolution or owned field copy.
    // Both endpoint reads use the same source and cut, including retirement.
    // Owned native record constructors debit every copied field/payload unit;
    // subsequent stage evaluation borrows these records without repeated reads.
    fn capture<C>(
        &self,
        eid: EId,
        relation: EdgeRelation,
        control: &mut impl FnMut(AsyncEdgeScanEvent) -> Result<(), C>,
    ) -> Result<AsyncEdgeCandidate<Record>, EdgeScanSourceError<ReadError, C>> {
        control(AsyncEdgeScanEvent::Candidate(eid)).map_err(EdgeScanSourceError::Control)?;
        let Some(edge) = self
            .0
            .edge(eid, &mut |event| control(source_event(event)))?
        else {
            return Ok(AsyncEdgeCandidate { eid, record: None });
        };
        if !relation.matches(edge.relation) {
            return Ok(AsyncEdgeCandidate { eid, record: None });
        }
        let source = self
            .0
            .vertex(edge.source, &mut |event| control(source_event(event)))?
            .map(|row| {
                VertexScanRecord::copy_masked(row, |_| true, |_| true, &mut |event| {
                    control(vertex_event(event))
                })
            })
            .transpose()
            .map_err(EdgeScanSourceError::Control)?;
        let target = if edge.source == edge.target {
            None
        } else {
            self.0
                .vertex(edge.target, &mut |event| control(source_event(event)))?
                .map(|row| {
                    VertexScanRecord::copy_masked(row, |_| true, |_| true, &mut |event| {
                        control(vertex_event(event))
                    })
                })
                .transpose()
                .map_err(EdgeScanSourceError::Control)?
        };
        let edge =
            EdgeScanRecord::copy_masked(edge, |_| true, &mut |event| control(source_event(event)))
                .map_err(EdgeScanSourceError::Control)?;
        // A missing endpoint stays absent; the ordinary async driver emits
        // DanglingEndpoint, never projects it as a NULL optional binding.
        Ok(AsyncEdgeCandidate {
            eid,
            record: Some(Record {
                edge,
                source,
                target,
            }),
        })
    }
}
impl AsyncEdgeScanSource for Source<'_> {
    type Error = ReadError;
    type Record = Record;
    type OutputGuard = ();
    fn snapshot_seq(&self) -> CommitSeq {
        self.0.as_of
    }

    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<ReadError, C>> {
        let id = self
            .0
            .next_edge_for_relation(relation, &mut |event| control(source_event(event)))?;
        id.map(|eid| self.capture(eid, relation, control))
            .transpose()
    }

    fn reserve_output<C>(
        &self,
        _: &Record,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<ReadError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
    fn evaluation_event(&self, _: &Record, _: GlaExecutionEvent) -> Result<(), ReadError> {
        // The GQL meter owns logical work/scratch; this resident source has no
        // physical MemoryPool. Explicit unit guards must not imply byte bounds.
        Ok(())
    }
}
impl AsyncEdgeJoinSource for Source<'_> {
    type TraversalGuard = ();
    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        relation: EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> AsyncIncidentCandidateResult<Record, ReadError, C> {
        let id = self.0.next_incident_edge_for_relation(
            endpoint,
            relation,
            direction,
            after,
            &mut |event| control(source_event(event)),
        )?;
        id.map(|eid| self.capture(eid, relation, control))
            .transpose()
            .map_err(EdgeExpansionSourceError::Read)
    }
    fn reserve_traversal<C>(
        &self,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<ReadError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
    fn reserve_join_output<C>(
        &self,
        _: &[Record],
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<ReadError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
}

fn open<'q>(
    view: EmbeddedReadView,
    cx: &'q QueryCx,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
) -> Result<
    AsyncEdgeJoinCursor<
        impl AsyncEdgeJoinSource<Error = ReadError, OutputGuard = (), TraversalGuard = ()> + use<'q>,
        impl FnMut() -> Result<(), Cancel> + Send + use<'q>,
    >,
    StreamError,
> {
    view.snapshot.check_frontier(as_of).map_err(source_error)?;
    let plan = AsyncEdgeJoinPlan::compile(pattern.plan())
        .map_err(|error| GqlQueryError::Source(EdgeScanError::Plan(error)))?;
    cx.with_restriction(|| cx.checkpoint())
        .map_err(GqlQueryError::Interrupted)?;
    let source = Source(view.edge_scan_source(cx, as_of).map_err(source_error)?);
    Ok(AsyncEdgeJoinCursor::new(source, plan, policy, move || {
        cx.with_restriction(|| cx.checkpoint())
    }))
}

impl<V: Vfs + Clone> Database<V> {
    /// Awaitable fixed-hop joins over the same admitted resident MVCC source as
    /// stream_graph_edges_governed. Opening scans nothing; source callbacks use
    /// real persistent incidence/history indexes, not a graph copy or rescan.
    ///
    /// The cursor retains only QueryCx and its immutable read generation, never
    /// the database handle or prepared argument. Later writes/compaction cannot
    /// move its cut. Each selected edge/endpoints record is copied under logical
    /// scratch admission, then borrowed through the existing GLA join evaluator.
    ///
    /// This is an OWNER-level resident read, not Warden authorization or cold
    /// storage. Unit output/traversal guards establish NO physical memory bound.
    /// Probes, OPTIONAL, variable-length expansion and nonidentity order refuse.
    /// Errors/drop/close release the active traversal without draining. Earlier
    /// rows remain delivered; successful completion requires Exhausted.
    pub fn stream_graph_edge_joins_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = ReadError, OutputGuard = (), TraversalGuard = ()>
            + use<'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'q, V>,
        >,
        StreamError,
    > {
        let view = self.read_session().map_err(source_error)?;
        let as_of = view.frontier();
        open(view, cx, pattern, as_of, policy)
    }

    /// Identical source and evaluator at one exact retained CommitSeq.
    pub fn stream_graph_edge_joins_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = ReadError, OutputGuard = (), TraversalGuard = ()>
            + use<'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'q, V>,
        >,
        StreamError,
    > {
        open(
            self.read_session().map_err(source_error)?,
            cx,
            pattern,
            as_of,
            policy,
        )
    }
}
impl EmbeddedReadView {
    /// The same async join driver pinned to this immutable view's frontier.
    pub fn stream_graph_edge_joins_governed<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = ReadError, OutputGuard = (), TraversalGuard = ()> + use<'q>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'q>,
        >,
        StreamError,
    > {
        open(self.clone(), cx, pattern, self.frontier(), policy)
    }
    pub fn stream_graph_edge_joins_governed_at<'q>(
        &self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = ReadError, OutputGuard = (), TraversalGuard = ()> + use<'q>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'q>,
        >,
        StreamError,
    > {
        open(self.clone(), cx, pattern, as_of, policy)
    }
}

#[cfg(test)]
mod tests;
