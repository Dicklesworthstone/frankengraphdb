//! Native edge GQL over the authenticated buffered history merge. The source
//! owns I/O and resident reservations; the existing async edge operator owns
//! filtering, direction, capture, projection, ordering and pagination.

mod join;
mod walk;

use super::{BufferedVertexSeeds, Cancel, Checkpoint};
use crate::{BufferedReadError, BufferedReadView, MemoryPool};
use asupersync::fs::Vfs;
use fgdb_gql::algebra::{
    EdgeRelation, GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaDirection, GraphValue, GraphValueRow,
    PreparedGraphPattern,
};
use fgdb_gql::edge_stream::{
    AsyncEdgeCandidate, AsyncEdgeScanCursor, AsyncEdgeScanEvent, AsyncEdgeScanPlan,
    AsyncEdgeScanRecord, AsyncEdgeScanSource, EdgeScanError, EdgeScanPropertyDemand, EdgeScanRow,
    EdgeScanSourceError, VertexScanRow,
};
use fgdb_gql::spill_aggregate::{AsyncEdgeSpillAggregateCursor, AsyncEdgeSpillAggregatePlan};
use fgdb_gql::stream::{AsyncVertexProbeCursor, AsyncVertexProbePlan, VertexScanError};
use fgdb_gql::{GlaExecutionEvent, GqlQueryError, GqlQueryPolicy};
use fgdb_strata::store::BufferedScanError;
use fgdb_strata::tiered::edge_scan::{
    BufferedEdgeCandidate, BufferedEdgeDirection, BufferedEdgeEndpoints, BufferedEdgeJoinScan,
    BufferedEdgePropertiesEndpoints, BufferedEdgeScanEvent, BufferedEdgeTopologyEndpoints,
};
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::{CommitSeq, QueryCx, VId};
use std::cell::RefCell;

type EdgeQueryError = GqlQueryError<EdgeScanError<BufferedReadError>, Cancel>;
type CandidateResult<C> =
    Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<BufferedReadError, C>>;
type VertexProbeResult<S, C> =
    Result<AsyncVertexProbeCursor<S, C, GraphValueRow>, super::BufferedQueryError>;

struct Record {
    image: Image,
    scratch: RefCell<MemoryCharge>,
}

// Only the compiled single-edge evaluator may authorize this projection.
// Joins, probes and walks retain complete records for their downstream reads.
enum Image {
    Complete(BufferedEdgeEndpoints),
    Topology(BufferedEdgeTopologyEndpoints),
    Property(BufferedEdgePropertiesEndpoints),
}

impl From<BufferedEdgeEndpoints> for Image {
    fn from(image: BufferedEdgeEndpoints) -> Self {
        Self::Complete(image)
    }
}

impl From<BufferedEdgeTopologyEndpoints> for Image {
    fn from(image: BufferedEdgeTopologyEndpoints) -> Self {
        Self::Topology(image)
    }
}

impl From<BufferedEdgePropertiesEndpoints> for Image {
    fn from(image: BufferedEdgePropertiesEndpoints) -> Self {
        Self::Property(image)
    }
}

impl AsyncEdgeScanRecord for Record {
    fn edge(&self) -> EdgeScanRow<'_> {
        match &self.image {
            Image::Complete(image) => {
                let edge = image.edge();
                EdgeScanRow {
                    source: edge.entry.src,
                    target: edge.entry.dst,
                    relation: edge.entry.relation,
                    properties: &edge.props,
                }
            }
            Image::Topology(image) => {
                let edge = image.edge().entry();
                EdgeScanRow {
                    source: edge.src,
                    target: edge.dst,
                    relation: edge.relation,
                    properties: &[],
                }
            }
            Image::Property(image) => {
                let edge = image.edge();
                EdgeScanRow {
                    source: edge.entry().src,
                    target: edge.entry().dst,
                    relation: edge.entry().relation,
                    properties: edge.properties(),
                }
            }
        }
    }

    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        // Storage admits both endpoints at the same cut. A missing endpoint
        // remains a typed source failure, never a NULL cell. Self-loops borrow
        // the one source image rather than copying a target.
        match &self.image {
            Image::Complete(image) => {
                let row = if vid == image.edge().entry.src {
                    image.source_vertex()
                } else if vid == image.edge().entry.dst {
                    image.target_vertex()
                } else {
                    return None;
                };
                Some(VertexScanRow {
                    labels: &row.labels,
                    properties: &row.props,
                })
            }
            Image::Topology(image) => {
                let row = if vid == image.edge().entry().src {
                    image.source_vertex()
                } else if vid == image.edge().entry().dst {
                    image.target_vertex()
                } else {
                    return None;
                };
                Some(VertexScanRow {
                    labels: row.labels(),
                    properties: &[],
                })
            }
            Image::Property(image) => {
                let row = if vid == image.edge().entry().src {
                    image.source_vertex()
                } else if vid == image.edge().entry().dst {
                    image.target_vertex()
                } else {
                    return None;
                };
                Some(VertexScanRow {
                    labels: row.labels(),
                    properties: &[],
                })
            }
        }
    }
}

struct Source<'view, 'q, V: Vfs> {
    scan: BufferedEdgeJoinScan<'view, V>,
    vertex_seeds: Option<BufferedVertexSeeds>,
    edge_seed_direction: Option<BufferedEdgeDirection>,
    property_demand: EdgeScanPropertyDemand,
    cx: &'q QueryCx,
    pool: MemoryPool,
}

impl<V: Vfs> Source<'_, '_, V> {
    // Root and routed incidence readers already admitted the same EId and
    // endpoint histories. Retain their typed reservations; never copy a record.
    fn admit_record<C, T: Into<Image>>(
        &self,
        candidate: Option<BufferedEdgeCandidate<T>>,
    ) -> CandidateResult<C> {
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
                        Ok::<_, EdgeScanSourceError<BufferedReadError, C>>(Record {
                            image: image.into(),
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
        if let Some(direction) = self.edge_seed_direction.take() {
            let seeds = self
                .vertex_seeds
                .take()
                .expect("selected edge direction owns its complete vertex domain");
            self.cx
                .with_restriction_async(self.scan.select_root_vertices_with(
                    self.cx,
                    seeds.as_slice(),
                    direction,
                    &mut |event| {
                        control(match event {
                            BufferedEdgeScanEvent::Work => AsyncEdgeScanEvent::Work,
                            BufferedEdgeScanEvent::Identity(eid) => {
                                AsyncEdgeScanEvent::Candidate(eid)
                            }
                        })
                    },
                ))
                .erase_send()
                .await
                .map_err(|error| match error {
                    BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                    BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
                })?;
            // Compact selected positions now own the root restriction. Release
            // the bounded seed batch before admitting full edge/endpoints.
            drop(seeds);
        }
        // An alternation reads every relation's candidates; the async cursor
        // admits only its members through `EdgeRelation::matches`.
        let relation = relation.one();
        if self.property_demand == EdgeScanPropertyDemand::Topology {
            let candidate = self
                .cx
                .with_restriction_async(self.scan.next_topology_with_endpoints(
                    self.cx,
                    relation,
                    &mut |event| {
                        control(match event {
                            BufferedEdgeScanEvent::Work => AsyncEdgeScanEvent::Work,
                            BufferedEdgeScanEvent::Identity(eid) => {
                                AsyncEdgeScanEvent::Candidate(eid)
                            }
                        })
                    },
                ))
                .erase_send()
                .await
                .map_err(|error| match error {
                    BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                    BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
                })?;
            return self.admit_record(candidate);
        }
        if let EdgeScanPropertyDemand::Properties(selection) = self.property_demand {
            let candidate = self
                .cx
                .with_restriction_async(self.scan.next_properties_with_endpoints(
                    self.cx,
                    relation,
                    selection,
                    &mut |event| {
                        control(match event {
                            BufferedEdgeScanEvent::Work => AsyncEdgeScanEvent::Work,
                            BufferedEdgeScanEvent::Identity(eid) => {
                                AsyncEdgeScanEvent::Candidate(eid)
                            }
                        })
                    },
                ))
                .erase_send()
                .await
                .map_err(|error| match error {
                    BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                    BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
                })?;
            return self.admit_record(candidate);
        }
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
            .erase_send()
            .await
            .map_err(|error| match error {
                BufferedScanError::Read(error) => EdgeScanSourceError::Source(error),
                BufferedScanError::Control(error) => EdgeScanSourceError::Control(error),
            })?;
        self.admit_record(candidate)
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
        let edge = record.edge();
        let source = record
            .vertex(edge.source)
            .expect("admitted source endpoint");
        let target = (edge.source != edge.target).then(|| {
            record
                .vertex(edge.target)
                .expect("admitted target endpoint")
        });
        let mut bytes = 1024usize;
        for (_, value) in edge
            .properties
            .iter()
            .chain(source.properties.iter())
            .chain(target.into_iter().flat_map(|row| row.properties.iter()))
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

fn seed_direction(direction: GlaDirection) -> BufferedEdgeDirection {
    match direction {
        GlaDirection::Forward => BufferedEdgeDirection::Outgoing,
        GlaDirection::Reverse => BufferedEdgeDirection::Incoming,
        GlaDirection::Undirected => BufferedEdgeDirection::Undirected,
    }
}

impl<V: Vfs + Clone> BufferedReadView<V> {
    /// Stream a vertex-rooted query with correlated EXISTS/NOT EXISTS through
    /// the same cold root/incidence source used by bounded graph traversal.
    /// The ordinary vertex scan remains a separate local-only capability.
    pub fn stream_graph_probes_governed<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> VertexProbeResult<
        impl fgdb_gql::edge_stream::AsyncGraphWalkSource<
            Error = BufferedReadError,
            OutputGuard = MemoryCharge,
        > + use<'view, 'q, V>,
        impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
    > {
        let as_of = self.frontier();
        self.stream_graph_probes_governed_at(cx, pattern, as_of, policy)
    }

    /// The same indexed semi/anti source at one exact historical cut. A first
    /// witness finishes EXISTS; NOT EXISTS requires successful exhaustion.
    /// Read, cancellation, quota and arithmetic errors never establish absence.
    pub fn stream_graph_probes_governed_at<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> VertexProbeResult<
        impl fgdb_gql::edge_stream::AsyncGraphWalkSource<
            Error = BufferedReadError,
            OutputGuard = MemoryCharge,
        > + use<'view, 'q, V>,
        impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
    > {
        let plan = AsyncVertexProbePlan::compile(pattern.plan())
            .map_err(|error| GqlQueryError::Source(VertexScanError::Plan(error)))?;
        self.open_vertex_probe_input(cx, plan, as_of, policy)
    }

    pub(crate) fn open_vertex_probe_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncVertexProbePlan<GraphValueRow>,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> VertexProbeResult<
        impl fgdb_gql::edge_stream::AsyncGraphWalkSource<
            Error = BufferedReadError,
            OutputGuard = MemoryCharge,
        > + use<'view, 'q, V>,
        impl FnMut() -> Result<(), Cancel> + Send + use<'view, 'q, V>,
    > {
        let (source, checkpoint) = self
            .edge_input_source(cx, as_of, EdgeRelation::Any)
            .map_err(|error| {
                error.map_source(|error| match error {
                    EdgeScanError::Source(source) => VertexScanError::Source(source),
                    error => VertexScanError::Probe(error),
                })
            })?;
        Ok(AsyncVertexProbeCursor::new(
            source, plan, policy, checkpoint,
        ))
    }

    /// Execute the existing ordered single-edge GLA profile through buffered
    /// storage. RETURN begins with the edge and oriented source identities.
    /// Directed, incoming, undirected, typed/Any relations, local vertex/edge
    /// predicates and properties, one-edge captures, DISTINCT/ALL and SKIP/LIMIT
    /// retain the ordinary operator semantics. Unsupported joins, probes and
    /// orderings refuse before a candidate is read; no eager fallback exists.
    ///
    /// When the compiled plan proves that no property is observed, the source
    /// retains edge topology and same-cut endpoint identities and labels only.
    /// Such reads skip independent edge-property sidecars and do not decode
    /// endpoint properties. A plan that reads one edge property key additionally
    /// admits only that selected field; unrelated edge and endpoint properties
    /// are not materialized. Missing keys remain NULL to the ordinary evaluator.
    /// Hidden ordering and aggregate input cells participate in the proof.
    /// Multiple edge keys, endpoint properties, joins, probes and unrecognized
    /// programs retain complete records. Selected native base images keep their
    /// existing complete-image admission bounds.
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
        let (mut source, checkpoint) = self.edge_input_source(cx, as_of, plan.root_relation())?;
        source.property_demand = plan.property_demand();
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
        let (mut source, checkpoint) = self.edge_input_source(cx, as_of, plan.root_relation())?;
        source.property_demand = plan.property_demand();
        Ok(AsyncEdgeSpillAggregateCursor::new(
            source, plan, policy, checkpoint,
        ))
    }

    /// Select the first edge's slot-zero vertex domain without changing native
    /// edge orientation, bag multiplicity, projection or external ordering.
    pub(crate) fn open_seeded_edge_input<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        plan: AsyncEdgeScanPlan,
        seeds: BufferedVertexSeeds,
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
        let (mut source, checkpoint) = self.edge_input_source(cx, as_of, plan.root_relation())?;
        source.property_demand = plan.property_demand();
        source.edge_seed_direction = Some(seed_direction(plan.root_direction()));
        source.vertex_seeds = Some(seeds);
        Ok(AsyncEdgeScanCursor::new(source, plan, policy, checkpoint))
    }

    fn edge_input_source<'view, 'q>(
        &'view mut self,
        cx: &'q QueryCx,
        as_of: CommitSeq,
        relation: EdgeRelation,
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
        // Root admission already proved every block's immutable relation and
        // complete histories. Select its descriptor family before seeding the
        // root heap, so unrelated topology/properties spend no payload I/O or
        // candidate quota. Any/alternation keep the native per-row predicate;
        // None from one() must never narrow a type alternation to one member.
        // This restricts only the root; nested incidence requests remain free
        // to select their own relation at the same admitted snapshot.
        let scan = match relation.one() {
            Some(relation) => self.partition.edge_scan_relation(cx, as_of, relation),
            None => self.partition.edge_scan(cx, as_of),
        }
        .map_err(source_error)?;
        let source = Source {
            scan: scan.into_join_scan(),
            vertex_seeds: None,
            edge_seed_direction: None,
            property_demand: EdgeScanPropertyDemand::Complete,
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
