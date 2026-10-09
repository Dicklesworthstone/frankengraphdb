//! Cold fixed-hop GQL joins over the existing routed incidence reader. Storage
//! resolves histories and owns fields; the native join driver owns semantics.

use super::*;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::edge_stream::{
    AsyncEdgeJoinCursor, AsyncEdgeJoinPlan, AsyncEdgeJoinSource, EdgeExpansionSourceError,
};
use fgdb_strata::tiered::edge_scan::BufferedEdgeDirection;
use fgdb_types::EId;

impl<V: Vfs + Clone> AsyncEdgeJoinSource for Source<'_, '_, V> {
    type TraversalGuard = MemoryCharge;

    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        relation: EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeExpansionSourceError<BufferedReadError, C>> {
        let relation = match relation {
            EdgeRelation::One(relation) => Some(relation),
            EdgeRelation::Any => None,
        };
        let direction = match direction {
            GlaDirection::Forward => BufferedEdgeDirection::Outgoing,
            GlaDirection::Reverse => BufferedEdgeDirection::Incoming,
            GlaDirection::Undirected => BufferedEdgeDirection::Undirected,
        };
        let candidate = self.cx.with_restriction_async(
            self.scan.next_incident_with_endpoints(
                self.cx, endpoint, relation, direction, after, &mut |event| {
                    control(match event {
                        BufferedEdgeScanEvent::Work => AsyncEdgeScanEvent::Work,
                        BufferedEdgeScanEvent::Identity(eid) => AsyncEdgeScanEvent::Candidate(eid),
                    })
                },
            ),
        ).await.map_err(|error| EdgeExpansionSourceError::Read(match error {
            BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
            BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
        }))?;
        // History, fields and their charges move together. The independent
        // successor position never replaces or advances the root scan position.
        self.admit_record(candidate).map_err(EdgeExpansionSourceError::Read)
    }

    fn reserve_traversal<C>(
        &self,
        hops: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<MemoryCharge, EdgeScanSourceError<BufferedReadError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        // Record slots plus Choice, binding and successor vectors, including
        // their headers and one pending undirected record. Source payloads and
        // routed block directories carry their own existing pool reservations.
        let bytes = core::mem::size_of::<Record>().checked_add(512)
            .and_then(|slot| slot.checked_mul(hops.saturating_add(1)))
            .and_then(|bytes| bytes.checked_add(2048))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        self.pool.reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory).map_err(EdgeScanSourceError::Source)
    }

    fn reserve_join_output<C>(
        &self,
        records: &[Record],
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<MemoryCharge, EdgeScanSourceError<BufferedReadError, C>> {
        // A projected path can contain every hop, and any source field may be
        // repeated in every output column. Reserve the SUM, not the largest
        // record, and keep it independently of the popped traversal records.
        let mut bytes = 1024usize;
        for record in records {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            bytes = bytes.checked_add(1024)
                .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
            let image = &record.image;
            let edge = image.edge();
            let target = (edge.entry.src != edge.entry.dst).then(|| image.target_vertex());
            for (_, value) in edge.props.iter()
                .chain(image.source_vertex().props.iter())
                .chain(target.into_iter().flat_map(|row| row.props.iter()))
            {
                control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
                let payload = value.canonical_encoded_len()
                    .map_err(|_| EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
                bytes = payload.checked_mul(4).and_then(|payload| payload.checked_add(256))
                    .and_then(|payload| bytes.checked_add(payload))
                    .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
            }
        }
        let bytes = bytes.checked_mul(columns.max(1))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.pool.reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory).map_err(EdgeScanSourceError::Source)
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Stream native fixed-hop chains, branches and identity-constrained cycles
    /// from cold authenticated histories. The output identity prefix is root
    /// edge, oriented root source, then each appended edge in traversal order.
    /// Predicates, paths, multiplicity, DISTINCT and pagination are evaluated by
    /// the existing AsyncEdgeJoinCursor, not a second graph interpreter.
    ///
    /// One bounded source record per active hop is retained. Routed incidence
    /// directories, decode workspaces, traversal and copied results all use this
    /// view's MemoryPool. The first nested lookup builds resident block routing;
    /// it can refuse on memory/work limits. Later successors fault only routed
    /// blocks. This is not a persistent index or an optimal sequential-I/O claim.
    ///
    /// Close, error and dropped pending pulls release traversal without draining
    /// input. Delivered rows keep independent guards; callers extracting pairs
    /// must retain the guard with the row. Exhaustion establishes completion.
    /// The view/context are borrowed, but the prepared definition is not.
    /// Initial admission/recovery retain their existing bounds. Owner-level API:
    /// no Warden grant, durable result lease, OPTIONAL/probe or unbounded walk.
    pub fn stream_graph_edge_joins_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = BufferedReadError, OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge> + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let frontier = self.frontier();
        self.stream_graph_edge_joins_governed_at(cx, pattern, frontier, policy)
    }

    /// Execute at the exact retained cut; a future cut refuses before source
    /// construction. No root, child or endpoint silently uses a newer frontier.
    pub fn stream_graph_edge_joins_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = BufferedReadError, OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge> + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        if as_of > self.frontier() {
            return Err(source_error(BufferedReadError::BeyondPublication {
                requested: as_of,
                publication: self.frontier(),
            }));
        }
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let plan = AsyncEdgeJoinPlan::compile(pattern.plan())
            .map_err(|error| GqlQueryError::Source(EdgeScanError::Plan(error)))?;
        self.open_edge_join_input(cx, plan, as_of, policy)
    }

    // Also serves the existing external ordering consumer. Only a fully checked
    // native join plan can enter; this never retries a failed source as eager.
    pub(crate) fn open_edge_join_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeJoinPlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<Error = BufferedReadError, OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge> + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let (source, checkpoint) = self.edge_input_source(cx, as_of)?;
        Ok(AsyncEdgeJoinCursor::new(source, plan, policy, checkpoint))
    }
}

#[cfg(test)]
mod tests;
