//! Bounded variable-length routes over one authenticated vertex/incidence
//! source. The native walk cursor owns path semantics; the external sorter
//! owns complete-row ordering, DISTINCT and the final window.

use super::super::BufferedQueryRecord;
use super::*;
use fgdb_gql::edge_stream::{
    AsyncEdgeJoinSource, AsyncGraphWalkCursor, AsyncGraphWalkPlan, AsyncGraphWalkSource,
};
use fgdb_gql::stream::{AsyncVertexCandidate, AsyncVertexScanEvent};
use fgdb_strata::store::BufferedScanEvent;
use fgdb_types::EId;

impl<V: Vfs + Clone> AsyncGraphWalkSource for Source<'_, '_, V> {
    type RootRecord = BufferedQueryRecord;

    async fn next_root_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<Self::RootRecord>>, EdgeScanSourceError<Self::Error, C>>
    {
        // The combined driver loans the same authenticated partition to the
        // vertex merge and incidence reader. Isolates and zero-hop routes are
        // therefore real vertex candidates, never inferred from edge endpoints.
        let candidate = self
            .cx
            .with_restriction_async(self.scan.next_vertex_candidate(self.cx, &mut |event| {
                control(match event {
                    BufferedScanEvent::Work => AsyncVertexScanEvent::Work,
                    BufferedScanEvent::Identity(vid) => AsyncVertexScanEvent::Candidate(vid),
                })
            }))
            .erase_send()
            .await
            .map_err(|error| match error {
                BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
            })?;
        candidate
            .map(|candidate| {
                let record = candidate
                    .row
                    .map(|row| {
                        let scratch = self
                            .pool
                            .reserve(self.cx, 0)
                            .map_err(BufferedReadError::Memory)
                            .map_err(EdgeScanSourceError::Source)?;
                        Ok::<_, EdgeScanSourceError<Self::Error, C>>(BufferedQueryRecord {
                            row,
                            scratch: RefCell::new(scratch),
                        })
                    })
                    .transpose()?;
                Ok(AsyncVertexCandidate {
                    vid: candidate.vid,
                    record,
                })
            })
            .transpose()
    }

    fn root_evaluation_event(
        &self,
        root: &Self::RootRecord,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if event == GlaExecutionEvent::ScratchEntry {
            let bytes = 8 * core::mem::size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
            root.scratch.borrow_mut().grow(self.cx, bytes)?;
        }
        Ok(())
    }

    fn reserve_walk_output<C>(
        &self,
        root: &Self::RootRecord,
        records: &[Self::Record],
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Self::OutputGuard, EdgeScanSourceError<Self::Error, C>> {
        // Include every retained edge/endpoint and repeated path cell using the
        // existing join law. The root also needs its own bound: a zero-hop path
        // has no edge record from which to account its selected properties.
        let mut guard = self.reserve_join_output(records, columns, control)?;
        let mut bytes = 1024usize;
        for (_, value) in &root.row.props {
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
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        guard
            .grow(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(EdgeScanSourceError::Source)?;
        Ok(guard)
    }

    fn reserve_shortest_state<C>(
        &self,
        bytes: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<MemoryCharge, EdgeScanSourceError<Self::Error, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        self.pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(EdgeScanSourceError::Source)
    }

    fn reserve_shortest_output<C>(
        &self,
        root: &Self::RootRecord,
        records: &[Self::Record],
        hops: usize,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<MemoryCharge, EdgeScanSourceError<Self::Error, C>> {
        let mut guard = self.reserve_walk_output(root, records, columns, control)?;
        // ANY retains one final record, not one source record per path hop.
        // Reserve complete path/node-list/edge-list cells and encoding backing
        // independently of the breadth layer and every source record.
        let bytes = hops
            .checked_mul(core::mem::size_of::<GraphValue>().max(core::mem::size_of::<(EId, VId)>()))
            .and_then(|bytes| bytes.checked_mul(8))
            .and_then(|bytes| bytes.checked_mul(columns.max(1)))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        guard
            .grow(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(EdgeScanSourceError::Source)?;
        Ok(guard)
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Private ALL-occurrence input for the blocking native external sorter.
    /// The selected cut, source-work budget and MemoryPool cover root vertices,
    /// indexed incidence, retained route records and copied output together.
    /// No result-order or early-page promise is made by this traversal cursor.
    #[allow(clippy::type_complexity)]
    pub(crate) fn open_graph_walk_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncGraphWalkPlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncGraphWalkCursor<
            impl AsyncGraphWalkSource<
                Error = BufferedReadError,
                OutputGuard = MemoryCharge,
                TraversalGuard = MemoryCharge,
            > + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let (source, checkpoint) = self.edge_input_source(cx, as_of, EdgeRelation::Any)?;
        Ok(AsyncGraphWalkCursor::new(source, plan, policy, checkpoint))
    }
}
