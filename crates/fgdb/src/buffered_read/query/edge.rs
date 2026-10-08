//! Native edge GQL over the authenticated buffered history merge. The source
//! owns I/O and resident reservations; the existing async edge operator owns
//! filtering, direction, capture, projection, ordering and pagination.

use super::{Cancel, Checkpoint};
use crate::{BufferedReadError, BufferedReadView, MemoryPool};
use asupersync::fs::Vfs;
use fgdb_gql::algebra::{
    EdgeRelation, GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValue, GraphValueRow, PreparedGraphPattern,
};
use fgdb_gql::edge_stream::{
    AsyncEdgeCandidate, AsyncEdgeScanCursor, AsyncEdgeScanEvent, AsyncEdgeScanPlan,
    AsyncEdgeScanRecord, AsyncEdgeScanSource, EdgeScanError, EdgeScanRow, EdgeScanSourceError,
    VertexScanRow,
};
use fgdb_gql::spill_aggregate::{AsyncEdgeSpillAggregateCursor, AsyncEdgeSpillAggregatePlan};
use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GqlQueryPolicy};
use fgdb_strata::store::BufferedScanError;
use fgdb_strata::tiered::edge_scan::{
    BufferedEdgeEndpoints, BufferedEdgeScan, BufferedEdgeScanEvent,
};
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::RefCell;

type EdgeQueryError = GqlQueryError<EdgeScanError<BufferedReadError>, Cancel>;
type CandidateResult<C> =
    Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<BufferedReadError, C>>;

struct Record {
    image: BufferedEdgeEndpoints,
    scratch: RefCell<MemoryCharge>,
}

impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        let edge = self.image.edge();
        EdgeScanRow {
            source: edge.entry.src,
            target: edge.entry.dst,
            relation: edge.entry.relation,
            properties: &edge.props,
        }
    }

    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        let row = if vid == self.image.edge().entry.src {
            self.image.source_vertex()
        } else if vid == self.image.edge().entry.dst {
            self.image.target_vertex()
        } else {
            return None;
        };
        // Storage admits both endpoints at the same cut. A missing endpoint
        // remains a typed source failure, never a NULL cell. Self-loops borrow
        // the one source image rather than copying a target.
        Some(VertexScanRow {
            labels: &row.labels,
            properties: &row.props,
        })
    }
}

struct Source<'view, 'q, V: Vfs> {
    scan: BufferedEdgeScan<'view, V>,
    cx: &'q QueryCx,
    pool: MemoryPool,
}

impl<V: Vfs + Clone> AsyncEdgeScanSource for Source<'_, '_, V> {
    type Error = BufferedReadError;
    type Record = Record;
    type OutputGuard = MemoryCharge;

    fn snapshot_seq(&self) -> CommitSeq {
        self.scan.snapshot_seq()
    }

    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> CandidateResult<C> {
        let relation = match relation {
            EdgeRelation::One(relation) => Some(relation),
            EdgeRelation::Any => None,
        };
        // Keep every awaitable source operation in the narrowed query context,
        // even though the public cursor is the ordinary native async operator.
        let candidate = self
            .cx
            .with_restriction_async(self.scan.next_with_endpoints(
                self.cx,
                relation,
                &mut |event| {
                    control(match event {
                        BufferedEdgeScanEvent::Work => AsyncEdgeScanEvent::Work,
                        BufferedEdgeScanEvent::Identity(eid) => AsyncEdgeScanEvent::Candidate(eid),
                    })
                },
            ))
            .await
            .map_err(|error| match error {
                BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
            })?;
        candidate
            .map(|candidate| {
                let record = candidate
                    .row
                    .map(|image| {
                        let charge = self
                            .pool
                            .reserve(self.cx, 0)
                            .map_err(BufferedReadError::Memory)
                            .map_err(EdgeScanSourceError::Source)?;
                        Ok::<_, EdgeScanSourceError<Self::Error, C>>(Record {
                            image,
                            scratch: RefCell::new(charge),
                        })
                    })
                    .transpose()?;
                Ok(AsyncEdgeCandidate {
                    eid: candidate.eid,
                    record,
                })
            })
            .transpose()
    }

    fn evaluation_event(
        &self,
        record: &Record,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if event == GlaExecutionEvent::ScratchEntry {
            // Same payload/value-cell reservation as the buffered vertex lane.
            let bytes = 8 * core::mem::size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
            record.scratch.borrow_mut().grow(self.cx, bytes)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &Record,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<MemoryCharge, EdgeScanSourceError<Self::Error, C>> {
        // The physical compiler admits only this edge, its endpoints, their
        // scalar properties and a single-edge path. Include all source payloads
        // and fixed path/row framing per column, even for repeated projections.
        let image = &record.image;
        let edge = image.edge();
        let target = (edge.entry.src != edge.entry.dst).then(|| image.target_vertex());
        let mut bytes = 1024usize;
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
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or(EdgeScanSourceError::Source(BufferedReadError::SizeOverflow))?;
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        self.pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(EdgeScanSourceError::Source)
    }
}

fn source_error(error: BufferedReadError) -> EdgeQueryError {
    GqlQueryError::Source(EdgeScanError::Source(error))
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Execute the existing ordered single-edge GLA profile through buffered
    /// storage. RETURN begins with the edge and oriented source identities.
    /// Directed, incoming, undirected, typed/Any relations, local vertex/edge
    /// predicates and properties, one-edge captures, DISTINCT/ALL and SKIP/LIMIT
    /// retain the ordinary operator semantics. Unsupported joins, probes and
    /// orderings refuse before a candidate is read; no eager fallback exists.
    ///
    /// The returned native async cursor owns its source privately. Its ordinary
    /// next/state/close/statistics API applies unchanged. Each output carries a
    /// MemoryCharge independently of the cursor and view; callers extracting
    /// output.into_parts() must retain the guard alongside the row. A late
    /// failure does not retract prior rows; successful completion requires EOF
    /// or an Exhausted state, not merely a delivered prefix.
    ///
    /// Initial root/history admission and Chronicle recovery retain their
    /// existing bounds. Endpoint descriptor lookup is bounds-pruned, not an
    /// indexed O(1) point read. Preparation/catalog/parameter objects and
    /// caller-created copies remain outside the view's MemoryPool. This is an
    /// owner-level API, not a Warden session or a resumable server cursor.
    pub fn stream_graph_edges_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeScanCursor<
            impl AsyncEdgeScanSource<Error = BufferedReadError, OutputGuard = MemoryCharge>
            + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let frontier = self.frontier();
        self.stream_graph_edges_governed_at(cx, pattern, frontier, policy)
    }

    /// Same physical operator at an exact retained sequence, never silently
    /// replaced by the view frontier. All source/evaluator work shares policy.
    /// Preparation is not borrowed by the returned cursor; only the view and
    /// query context are retained, with the same ownership rules as the live cut.
    pub fn stream_graph_edges_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeScanCursor<
            impl AsyncEdgeScanSource<Error = BufferedReadError, OutputGuard = MemoryCharge>
            + use<'view, 'q, V>,
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
        let plan = AsyncEdgeScanPlan::compile(pattern.plan())
            .map_err(|error| GqlQueryError::Source(EdgeScanError::Plan(error)))?;
        self.open_edge_input(cx, plan, as_of, policy)
    }

    /// The same authenticated source and guarded native cursor, with local
    /// projection admitted for an external operator instead of direct output.
    pub(crate) fn open_edge_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeScanPlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeScanCursor<
            impl AsyncEdgeScanSource<Error = BufferedReadError, OutputGuard = MemoryCharge>
            + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let (source, checkpoint) = self.edge_input_source(cx, as_of)?;
        Ok(AsyncEdgeScanCursor::new(source, plan, policy, checkpoint))
    }

    pub(crate) fn open_edge_aggregate_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeSpillAggregatePlan,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> Result<
        AsyncEdgeSpillAggregateCursor<
            impl AsyncEdgeScanSource<Error = BufferedReadError, OutputGuard = MemoryCharge>
            + use<'view, 'q, V>,
            impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
        >,
        EdgeQueryError,
    > {
        let (source, checkpoint) = self.edge_input_source(cx, as_of)?;
        Ok(AsyncEdgeSpillAggregateCursor::new(
            source, plan, policy, checkpoint,
        ))
    }

    fn edge_input_source<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        as_of: CommitSeq,
    ) -> Result<(Source<'view, 'q, V>, Checkpoint<'q>), EdgeQueryError> {
        if as_of > self.frontier() {
            return Err(source_error(BufferedReadError::BeyondPublication {
                requested: as_of,
                publication: self.frontier(),
            }));
        }
        cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
        let pool = self.memory_pool().clone();
        let metadata = pool
            .reserve(cx, 1024)
            .map_err(BufferedReadError::Memory)
            .map_err(source_error)?;
        let source = Source {
            scan: self.partition.edge_scan(cx, as_of).map_err(source_error)?,
            cx,
            pool,
        };
        // The meter outlives source close/exhaustion. Keep the cursor's fixed
        // reservation there until cursor drop, never refund it on a row pull.
        let checkpoint: Checkpoint<'q> = Box::new(move || {
            let _retained = &metadata;
            cx.with_restriction(|| cx.checkpoint())
        });
        Ok((source, checkpoint))
    }
}
