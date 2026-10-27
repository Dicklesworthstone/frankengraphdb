//! Cold fixed-hop GQL joins over the existing routed incidence reader. Storage
//! resolves histories and owns fields; the native join driver owns semantics.

use super::*;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::edge_stream::{
    AsyncEdgeJoinCursor, AsyncEdgeJoinPlan, AsyncEdgeJoinSource, EdgeExpansionSourceError,
};
use fgdb_gql::spill_aggregate::{
    AsyncEdgeJoinSpillAggregateCursor, AsyncEdgeJoinSpillAggregatePlan,
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
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeExpansionSourceError<BufferedReadError, C>>
    {
        // An alternation reads every relation's incidence; the async join
        // admits only its members through `EdgeRelation::matches`.
        let relation = relation.one();
        let direction = match direction {
            GlaDirection::Forward => BufferedEdgeDirection::Outgoing,
            GlaDirection::Reverse => BufferedEdgeDirection::Incoming,
            GlaDirection::Undirected => BufferedEdgeDirection::Undirected,
        };
        let candidate = self
            .cx
            .with_restriction_async(self.scan.next_incident_retained_with_endpoints(
                self.cx,
                endpoint,
                relation,
                direction,
                after,
                &mut |event| {
                    control(match event {
                        BufferedEdgeScanEvent::Work => AsyncEdgeScanEvent::Work,
                        BufferedEdgeScanEvent::Identity(eid) => AsyncEdgeScanEvent::Candidate(eid),
                    })
                },
            ))
            .erase_send()
            .await
            .map_err(|error| {
                EdgeExpansionSourceError::Read(match error {
                    BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                    BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
                })
            })?;
        // Repeated siblings reuse one charged incidence merge only when their
        // complete request continues the preceding historical candidate. A
        // different hop, backtrack or restart remains an independent seek;
        // the source replaces its old continuation before allocating another.
        // History, fields and charges still move together, without advancing
        // the root or caching projected/filtered query answers.
        self.admit_record(candidate)
            .map_err(EdgeExpansionSourceError::Read)
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
        let bytes = core::mem::size_of::<Record>()
            .checked_add(512)
            .and_then(|slot| slot.checked_mul(hops.saturating_add(1)))
            .and_then(|bytes| bytes.checked_add(2048))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        self.pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(EdgeScanSourceError::Source)
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
            bytes = bytes
                .checked_add(1024)
                .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
            let image = &record.image;
            let edge = image.edge();
            let target = (edge.entry.src != edge.entry.dst).then(|| image.target_vertex());
            for (_, value) in edge
                .props
                .iter()
                .chain(image.source_vertex().props.iter())
                .chain(target.into_iter().flat_map(|row| row.props.iter()))
            {
                control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
                let payload = value
                    .canonical_encoded_len()
                    .map_err(|_| EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
                bytes = payload
                    .checked_mul(4)
                    .and_then(|payload| payload.checked_add(256))
                    .and_then(|payload| bytes.checked_add(payload))
                    .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
            }
        }
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(EdgeScanSourceError::Source)
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Stream native fixed-hop chains, branches and identity-constrained cycles
    /// from cold authenticated histories. The output identity prefix is root
    /// edge, oriented root source, then each appended edge in traversal order.
    /// Predicates, paths, multiplicity, DISTINCT and pagination are evaluated by
    /// the existing AsyncEdgeJoinCursor, not a second graph interpreter.
    /// Correlated EXISTS/NOT EXISTS scopes use the same incidence source and
    /// native probe predicates. One witness ends a positive search; absence
    /// requires exhaustion. Child records never multiply or escape the outer
    /// result. Each active probe has its own definition-bounded frame charge;
    /// histories, decoding and predicates share the outer cumulative policy.
    ///
    /// One bounded source record per active hop is retained. Authenticated block
    /// directories, decode workspaces, traversal and copied results all use this
    /// view's MemoryPool. Nested lookups seek shared descriptor metadata without
    /// reading graph payloads to build routing. Broad incoming ranges can still
    /// select many blocks and refuse on memory/work limits. Consecutive successors
    /// retain one charged merge heap and bounded decoded block, instead of
    /// reseeding for every candidate. Root advances release that continuation;
    /// different incidences or positions replace it before reseeding. Deep
    /// alternating traversals may still reseed. This is not a per-hop cache,
    /// persistent index or optimal sequential-I/O claim. Source refusals remain
    /// terminal, never retried with another physical implementation.
    ///
    /// Close, error and dropped pending pulls release traversal without draining
    /// input. Delivered rows keep independent guards; callers extracting pairs
    /// must retain the guard with the row. Exhaustion establishes completion.
    /// The view/context are borrowed, but the prepared definition is not.
    /// Initial admission/recovery retain their existing bounds. Owner-level API:
    /// no Warden grant, durable result lease, OPTIONAL, independent probe scans,
    /// nested probes or variable-length probe/outer traversal. These profiles
    /// refuse during physical preparation, never as an empty neighborhood.
    // The cursor's source and checkpoint are opaque, and stable Rust cannot
    // name an opaque type in an alias, so this and the three join openers
    // below spell the type out (as the resident join entry points do).
    #[allow(clippy::type_complexity)]
    pub fn stream_graph_edge_joins_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<
                Error = BufferedReadError,
                OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge,
            > + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let frontier = self.frontier();
        self.stream_graph_edge_joins_governed_at(cx, pattern, frontier, policy)
    }

    /// Execute at the exact retained cut; a future cut refuses before source
    /// construction. No root, child or endpoint silently uses a newer frontier.
    #[allow(clippy::type_complexity)]
    pub fn stream_graph_edge_joins_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<
                Error = BufferedReadError,
                OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge,
            > + use<'view, 'q, V>,
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
    #[allow(clippy::type_complexity)]
    pub(crate) fn open_edge_join_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeJoinPlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinCursor<
            impl AsyncEdgeJoinSource<
                Error = BufferedReadError,
                OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge,
            > + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let (source, checkpoint) = self.edge_input_source(cx, as_of, plan.root_relation())?;
        Ok(AsyncEdgeJoinCursor::new(source, plan, policy, checkpoint))
    }

    // Numeric reducers consume private occurrences, not delivered join rows.
    // Preserve the same routed source, cut, QueryCx and resident reservations;
    // only the sealed native aggregate-input plan selects its accounting mode.
    #[allow(clippy::type_complexity)]
    pub(crate) fn open_edge_join_aggregate_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeJoinSpillAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeJoinSpillAggregateCursor<
            impl AsyncEdgeJoinSource<
                Error = BufferedReadError,
                OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge,
            > + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let (source, checkpoint) = self.edge_input_source(cx, as_of, plan.root_relation())?;
        Ok(AsyncEdgeJoinSpillAggregateCursor::new(
            source, plan, policy, checkpoint,
        ))
    }
}

#[cfg(test)]
mod tests;
