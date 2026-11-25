//! Private masked source for the canonical native asynchronous operators.
//! Raw topology/history work is host-physical admission. A candidate enters
//! native and signed visible budgets only after same-cut capability masking.

use super::*;
use fgdb_delta_types::LabelId;
use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GlaDirection, GraphValue};
use fgdb_gql::edge_stream::{
    AsyncEdgeCandidate, AsyncEdgeJoinSource, AsyncEdgeScanEvent, AsyncEdgeScanRecord,
    AsyncEdgeScanSource, EdgeExpansionSourceError, EdgeScanSourceError,
};
use fgdb_gql::stream::{
    AsyncVertexCandidate, AsyncVertexScanEvent, AsyncVertexScanRecord, AsyncVertexScanSource,
};
use fgdb_strata::store::{BufferedScanError, BufferedVertexProperties, BufferedVertexScanState};
use fgdb_strata::tiered::edge_scan::{
    BufferedEdgeDirection, BufferedEdgeJoinScanState, BufferedEdgePropertiesEndpoints,
    BufferedEdgeScanState, BufferedEdgeTopologyEndpoints,
};
use fgdb_strata::tiered::memory::MemoryError;
use fgdb_types::EId;

pub(super) struct Source<'q, V: Vfs, C, P> {
    // Detached merge allocations drop before the backend returns to its owner.
    position: Option<P>,
    backend: Option<AuthorizedBufferedReadSource<V>>,
    returned: Returned<V>,
    cx: &'q QueryCx,
    execution: Shared<'q, C>,
    pool: MemoryPool,
    keys: PropertySelection,
    endpoint_keys: PropertySelection,
}

impl<'q, V: Vfs + Clone, C, P> Source<'q, V, C, P> {
    pub(super) fn new(
        backend: AuthorizedBufferedReadSource<V>,
        returned: Returned<V>,
        position: Option<P>,
        keys: PropertySelection,
        endpoint_keys: PropertySelection,
        cx: &'q QueryCx,
        execution: Shared<'q, C>,
    ) -> Self {
        let pool = backend.view.memory_pool().clone();
        Self {
            position,
            backend: Some(backend),
            returned,
            cx,
            execution,
            pool,
            keys,
            endpoint_keys,
        }
    }
}

impl<V: Vfs, C, P> Drop for Source<'_, V, C, P> {
    fn drop(&mut self) {
        self.position.take();
        if let Some(backend) = self.backend.take() {
            let previous = {
                let mut returned = self
                    .returned
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                returned.replace(backend)
            };
            // There is exactly one source. Do not run any backend destructor
            // while holding even this uncontended, callback-free return lock.
            debug_assert!(previous.is_none());
            drop(previous);
        }
    }
}

enum Control<C> {
    Query(QueryError),
    Native(C),
}

fn vertex_control<C>(
    error: BufferedScanError<Control<C>>,
) -> VertexScanSourceError<AuthorizedBufferedError, C> {
    match error {
        BufferedScanError::Read(error) => VertexScanSourceError::Source(error.into()),
        BufferedScanError::Control(Control::Query(error)) => {
            VertexScanSourceError::Source(error.into())
        }
        BufferedScanError::Control(Control::Native(error)) => VertexScanSourceError::Control(error),
    }
}
fn edge_control<C>(
    error: BufferedScanError<Control<C>>,
) -> EdgeScanSourceError<AuthorizedBufferedError, C> {
    match error {
        BufferedScanError::Read(error) => EdgeScanSourceError::Source(error.into()),
        BufferedScanError::Control(Control::Query(error)) => {
            EdgeScanSourceError::Source(error.into())
        }
        BufferedScanError::Control(Control::Native(error)) => EdgeScanSourceError::Control(error),
    }
}

/// Opaque affine accounting retained by AuthorizedBufferedQueryRow. The
/// record's evaluator grows the same reservation before allocating operands
/// and projected payloads; its last output owner refunds it.
pub struct OutputGuard {
    _copy: MemoryCharge,
    _evaluation: Arc<Mutex<MemoryCharge>>,
}

impl core::fmt::Debug for OutputGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedBufferedOutputGuard([REDACTED])")
    }
}

impl OutputGuard {
    pub(super) fn into_parts(self) -> (MemoryCharge, Arc<Mutex<MemoryCharge>>) {
        (self._copy, self._evaluation)
    }
}

pub(super) struct VertexRecord {
    labels: Vec<LabelId>,
    fields: BufferedVertexProperties,
    scratch: Arc<Mutex<MemoryCharge>>,
}
impl AsyncVertexScanRecord for VertexRecord {
    fn as_row(&self) -> VertexScanRow<'_> {
        VertexScanRow {
            labels: &self.labels,
            properties: self.fields.properties(),
        }
    }
}

pub(super) struct EdgeRecord {
    source_labels: Vec<LabelId>,
    target_labels: Vec<LabelId>,
    fields: BufferedEdgePropertiesEndpoints,
    scratch: Arc<Mutex<MemoryCharge>>,
}
impl AsyncEdgeScanRecord for EdgeRecord {
    fn edge(&self) -> EdgeScanRow<'_> {
        let edge = self.fields.edge();
        EdgeScanRow {
            source: edge.entry().src,
            target: edge.entry().dst,
            relation: edge.entry().relation,
            properties: edge.properties(),
        }
    }
    fn vertex(&self, vid: VId) -> Option<VertexScanRow<'_>> {
        let edge = self.fields.edge().entry();
        let (labels, properties) = if vid == edge.src {
            (&self.source_labels, self.fields.source_properties())
        } else if vid == edge.dst {
            (&self.target_labels, self.fields.target_properties())
        } else {
            return None;
        };
        Some(VertexScanRow { labels, properties })
    }
}

fn labels<C: FnMut() -> u64>(
    original: &[LabelId],
    cx: &QueryCx,
    execution: &Shared<'_, C>,
    charge: &mut MemoryCharge,
) -> Result<Vec<LabelId>, AuthorizedBufferedError> {
    let count = {
        let live = execution.borrow();
        original
            .iter()
            .filter(|label| live.permit.predicates().allows_label(**label))
            .count()
    };
    let bytes = count
        .checked_mul(size_of::<LabelId>())
        .ok_or(BufferedReadError::SizeOverflow)?;
    charge.grow(cx, bytes).map_err(BufferedReadError::Memory)?;
    let mut result = Vec::new();
    result.try_reserve_exact(count).map_err(|_| {
        BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
    })?;
    if result
        .capacity()
        .checked_mul(size_of::<LabelId>())
        .is_none_or(|capacity| capacity > bytes)
    {
        return Err(BufferedReadError::SizeOverflow.into());
    }
    let live = execution.borrow();
    result.extend(
        original
            .iter()
            .copied()
            .filter(|label| live.permit.predicates().allows_label(*label)),
    );
    Ok(result)
}

fn scratch(cx: &QueryCx, charge: &Arc<Mutex<MemoryCharge>>) -> Result<(), AuthorizedBufferedError> {
    let bytes = 8 * size_of::<GraphValue>().max(GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
    charge
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .grow(cx, bytes)
        .map_err(BufferedReadError::Memory)?;
    Ok(())
}

impl<V: Vfs + Clone, Clock: FnMut() -> u64 + Send> AsyncVertexScanSource
    for Source<'_, V, Clock, BufferedVertexScanState>
{
    type Error = AuthorizedBufferedError;
    type Record = VertexRecord;
    type OutputGuard = OutputGuard;

    fn snapshot_seq(&self) -> CommitSeq {
        self.backend.as_ref().expect("source owns backend").at
    }

    async fn next_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<VertexRecord>>, VertexScanSourceError<Self::Error, C>>
    {
        loop {
            let Some(state) = self.position.take() else {
                return Ok(None);
            };
            let backend = self.backend.as_mut().expect("source owns backend");
            let mut scan = backend
                .view
                .partition_mut()
                .resume_vertex_scan(self.cx, state)
                .map_err(|error| VertexScanSourceError::Source(error.into()))?;
            let execution = &self.execution;
            let mut admit = |vid, original: &[LabelId]| {
                {
                    let mut live = execution.borrow_mut();
                    live.poll_fresh().map_err(Control::Query)?;
                    if !live.permit.predicates().allows_vertex(original) {
                        return Ok(false);
                    }
                    live.root_node(vid).map_err(Control::Query)?;
                }
                control(AsyncVertexScanEvent::Candidate(vid)).map_err(Control::Native)?;
                Ok(true)
            };
            let mut observe = |_| {
                execution
                    .borrow_mut()
                    .reserve_work(0)
                    .map_err(Control::Query)
            };
            let result = self
                .cx
                .with_restriction_async(scan.next_admitted_properties_candidate_with(
                    self.cx,
                    self.keys,
                    &mut admit,
                    &mut observe,
                ))
                .erase_send()
                .await;
            self.position = Some(scan.into_state());
            let Some(candidate) = result.map_err(vertex_control)? else {
                return Ok(None);
            };
            let Some(fields) = candidate.row else {
                continue;
            };
            let mut charge = self
                .pool
                .reserve(self.cx, 0)
                .map_err(BufferedReadError::Memory)
                .map_err(|error| VertexScanSourceError::Source(error.into()))?;
            let labels = labels(fields.labels(), self.cx, execution, &mut charge)
                .map_err(VertexScanSourceError::Source)?;
            return Ok(Some(AsyncVertexCandidate {
                vid: candidate.vid,
                record: Some(VertexRecord {
                    labels,
                    fields,
                    scratch: Arc::new(Mutex::new(charge)),
                }),
            }));
        }
    }

    fn evaluation_event(
        &self,
        record: &VertexRecord,
        event: VertexScanEvent,
    ) -> Result<(), Self::Error> {
        if event == VertexScanEvent::ScratchEntry {
            scratch(self.cx, &record.scratch)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &VertexRecord,
        columns: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<OutputGuard, VertexScanSourceError<Self::Error, C>> {
        // Native controls precede every payload measurement, while the shared
        // signed checkpoint remains the sole logical work owner.
        let mut bytes = 1024usize;
        for (_, value) in record.fields.properties() {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            let payload = value.canonical_encoded_len().map_err(|_| {
                VertexScanSourceError::Source(BufferedReadError::SizeOverflow.into())
            })?;
            bytes = payload
                .checked_mul(4)
                .and_then(|n| n.checked_add(256))
                .and_then(|n| bytes.checked_add(n))
                .ok_or_else(|| {
                    VertexScanSourceError::Source(BufferedReadError::SizeOverflow.into())
                })?;
        }
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or_else(|| VertexScanSourceError::Source(BufferedReadError::SizeOverflow.into()))?;
        control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
        let copy = self
            .pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(|error| VertexScanSourceError::Source(error.into()))?;
        Ok(OutputGuard {
            _copy: copy,
            _evaluation: record.scratch.clone(),
        })
    }
}

// Root and child edges pass the same relation and BOTH endpoint checks before
// any visible candidate/node debit or selected property decoding.
fn admit_edge<C, Clock: FnMut() -> u64>(
    execution: &Shared<'_, Clock>,
    relation: EdgeRelation,
    row: &BufferedEdgeTopologyEndpoints,
    control: &mut impl FnMut(AsyncEdgeScanEvent) -> Result<(), C>,
) -> Result<bool, Control<C>> {
    let edge = row.edge().entry();
    {
        let mut live = execution.borrow_mut();
        live.poll_fresh().map_err(Control::Query)?;
        let scope = live.permit.predicates();
        // Set and Any may use a broader physical route. Nonmembers never
        // become logical candidates, even when otherwise visible.
        if !relation.matches(edge.relation)
            || !scope.allows_relation(edge.relation)
            || !scope.allows_vertex(row.source_vertex().labels())
            || !scope.allows_vertex(row.target_vertex().labels())
        {
            return Ok(false);
        }
        // This edge source owns fresh endpoint records. A previous vertex
        // leaf in the same external execution may leave pull_root behind,
        // but its record is no longer borrowed and grants no node credit.
        live.node().map_err(Control::Query)?;
        if edge.dst != edge.src {
            live.node().map_err(Control::Query)?;
        }
    }
    control(AsyncEdgeScanEvent::Candidate(edge.eid)).map_err(Control::Native)?;
    Ok(true)
}

impl<V: Vfs + Clone, Clock: FnMut() -> u64 + Send, P> Source<'_, V, Clock, P> {
    fn retain_edge(
        &self,
        fields: BufferedEdgePropertiesEndpoints,
    ) -> Result<EdgeRecord, AuthorizedBufferedError> {
        let mut charge = self
            .pool
            .reserve(self.cx, 0)
            .map_err(BufferedReadError::Memory)?;
        let source_labels = labels(
            fields.source_vertex().labels(),
            self.cx,
            &self.execution,
            &mut charge,
        )?;
        let target_labels = if fields.edge().entry().src == fields.edge().entry().dst {
            Vec::new()
        } else {
            labels(
                fields.target_vertex().labels(),
                self.cx,
                &self.execution,
                &mut charge,
            )?
        };
        Ok(EdgeRecord {
            source_labels,
            target_labels,
            fields,
            scratch: Arc::new(Mutex::new(charge)),
        })
    }
}

impl<V: Vfs + Clone, Clock: FnMut() -> u64 + Send> AsyncEdgeScanSource
    for Source<'_, V, Clock, BufferedEdgeScanState>
{
    type Error = AuthorizedBufferedError;
    type Record = EdgeRecord;
    type OutputGuard = OutputGuard;

    fn snapshot_seq(&self) -> CommitSeq {
        self.backend.as_ref().expect("source owns backend").at
    }

    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<EdgeRecord>>, EdgeScanSourceError<Self::Error, C>> {
        if excludes_all_relations(relation, self.execution.borrow().permit.predicates()) {
            return Ok(None);
        }
        loop {
            let Some(state) = self.position.take() else {
                return Ok(None);
            };
            let backend = self.backend.as_mut().expect("source owns backend");
            let mut scan = backend
                .view
                .partition_mut()
                .resume_edge_scan(self.cx, state)
                .map_err(|error| EdgeScanSourceError::Source(error.into()))?;
            let execution = &self.execution;
            let mut admit = |row: &BufferedEdgeTopologyEndpoints| {
                admit_edge(execution, relation, row, control)
            };
            let mut observe = |_| {
                execution
                    .borrow_mut()
                    .reserve_work(0)
                    .map_err(Control::Query)
            };
            let result = self
                .cx
                .with_restriction_async(scan.next_admitted_properties_and_endpoint_properties(
                    self.cx,
                    relation.one(),
                    self.keys,
                    self.endpoint_keys,
                    &mut admit,
                    &mut observe,
                ))
                .erase_send()
                .await;
            self.position = Some(scan.into_state());
            let Some(candidate) = result.map_err(edge_control)? else {
                return Ok(None);
            };
            let Some(fields) = candidate.row else {
                continue;
            };
            return Ok(Some(AsyncEdgeCandidate {
                eid: candidate.eid,
                record: Some(self.retain_edge(fields).map_err(EdgeScanSourceError::Source)?),
            }));
        }
    }

    fn evaluation_event(
        &self,
        record: &EdgeRecord,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if event == GlaExecutionEvent::ScratchEntry {
            scratch(self.cx, &record.scratch)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &EdgeRecord,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<OutputGuard, EdgeScanSourceError<Self::Error, C>> {
        let mut bytes = 1024usize;
        let edge = record.fields.edge();
        let target_properties = if edge.entry().src == edge.entry().dst {
            &[][..]
        } else {
            record.fields.target_properties()
        };
        for (_, value) in edge
            .properties()
            .iter()
            .chain(record.fields.source_properties())
            .chain(target_properties)
        {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            let payload = value
                .canonical_encoded_len()
                .map_err(|_| EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into()))?;
            bytes = payload
                .checked_mul(4)
                .and_then(|n| n.checked_add(256))
                .and_then(|n| bytes.checked_add(n))
                .ok_or_else(|| {
                    EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into())
                })?;
        }
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or_else(|| EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into()))?;
        control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
        let copy = self
            .pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(|error| EdgeScanSourceError::Source(error.into()))?;
        Ok(OutputGuard {
            _copy: copy,
            _evaluation: record.scratch.clone(),
        })
    }
}

// Join roots use the same admission as single-edge scans. The detached join
// state additionally retains its independently positioned incidence owner, so
// the final root candidate can still expand after the root reaches EOF.
impl<V: Vfs + Clone, Clock: FnMut() -> u64 + Send> AsyncEdgeScanSource
    for Source<'_, V, Clock, BufferedEdgeJoinScanState>
{
    type Error = AuthorizedBufferedError;
    type Record = EdgeRecord;
    type OutputGuard = OutputGuard;

    fn snapshot_seq(&self) -> CommitSeq {
        self.backend.as_ref().expect("source owns backend").at
    }

    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<EdgeRecord>>, EdgeScanSourceError<Self::Error, C>> {
        if excludes_all_relations(relation, self.execution.borrow().permit.predicates()) {
            return Ok(None);
        }
        loop {
            let Some(state) = self.position.take() else {
                return Ok(None);
            };
            let backend = self.backend.as_mut().expect("source owns backend");
            let mut scan = backend
                .view
                .partition_mut()
                .resume_edge_join_scan(self.cx, state)
                .map_err(|error| EdgeScanSourceError::Source(error.into()))?;
            let execution = &self.execution;
            let mut admit = |row: &BufferedEdgeTopologyEndpoints| {
                admit_edge(execution, relation, row, control)
            };
            let mut observe = |_| {
                execution
                    .borrow_mut()
                    .reserve_work(0)
                    .map_err(Control::Query)
            };
            let result = self
                .cx
                .with_restriction_async(scan.next_admitted_properties_and_endpoint_properties(
                    self.cx,
                    relation.one(),
                    self.keys,
                    self.endpoint_keys,
                    &mut admit,
                    &mut observe,
                ))
                .erase_send()
                .await;
            self.position = Some(scan.into_state());
            let Some(candidate) = result.map_err(edge_control)? else {
                return Ok(None);
            };
            let Some(fields) = candidate.row else {
                continue;
            };
            return Ok(Some(AsyncEdgeCandidate {
                eid: candidate.eid,
                record: Some(self.retain_edge(fields).map_err(EdgeScanSourceError::Source)?),
            }));
        }
    }

    fn evaluation_event(
        &self,
        record: &EdgeRecord,
        event: GlaExecutionEvent,
    ) -> Result<(), Self::Error> {
        if event == GlaExecutionEvent::ScratchEntry {
            scratch(self.cx, &record.scratch)?;
        }
        Ok(())
    }

    fn reserve_output<C>(
        &self,
        record: &EdgeRecord,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<OutputGuard, EdgeScanSourceError<Self::Error, C>> {
        self.reserve_join_output(std::slice::from_ref(record), columns, control)
    }
}

impl<V: Vfs + Clone, Clock: FnMut() -> u64 + Send> AsyncEdgeJoinSource
    for Source<'_, V, Clock, BufferedEdgeJoinScanState>
{
    type TraversalGuard = MemoryCharge;

    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        relation: EdgeRelation,
        direction: GlaDirection,
        mut after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<
        Option<AsyncEdgeCandidate<EdgeRecord>>,
        EdgeExpansionSourceError<AuthorizedBufferedError, C>,
    > {
        // A denied child selector needs no index or endpoint observation.
        // Every endpoint supplied by the native join came from an admitted
        // outer record; each child still rechecks both endpoints at this cut.
        if excludes_all_relations(relation, self.execution.borrow().permit.predicates()) {
            return Ok(None);
        }
        let direction = match direction {
            GlaDirection::Forward => BufferedEdgeDirection::Outgoing,
            GlaDirection::Reverse => BufferedEdgeDirection::Incoming,
            GlaDirection::Undirected => BufferedEdgeDirection::Undirected,
        };
        loop {
            let Some(state) = self.position.take() else {
                return Ok(None);
            };
            let backend = self.backend.as_mut().expect("source owns backend");
            let mut scan = backend
                .view
                .partition_mut()
                .resume_edge_join_scan(self.cx, state)
                .map_err(|error| {
                    EdgeExpansionSourceError::Read(EdgeScanSourceError::Source(error.into()))
                })?;
            let execution = &self.execution;
            let mut admit = |row: &BufferedEdgeTopologyEndpoints| {
                admit_edge(execution, relation, row, control)
            };
            let mut observe = |_| {
                execution
                    .borrow_mut()
                    .reserve_work(0)
                    .map_err(Control::Query)
            };
            let result = self
                .cx
                .with_restriction_async(
                    scan.next_incident_admitted_properties_and_endpoint_properties(
                        self.cx,
                        endpoint,
                        relation.one(),
                        direction,
                        after,
                        self.keys,
                        self.endpoint_keys,
                        &mut admit,
                        &mut observe,
                    ),
                )
                .erase_send()
                .await;
            self.position = Some(scan.into_state());
            let Some(candidate) = result
                .map_err(edge_control)
                .map_err(EdgeExpansionSourceError::Read)?
            else {
                return Ok(None);
            };
            // Hidden or retired candidates advance only this private physical
            // successor. They emit no native event, debit no visible allowance
            // and cannot multiply the outer binding.
            after = Some(candidate.eid);
            let Some(fields) = candidate.row else {
                continue;
            };
            let record = self
                .retain_edge(fields)
                .map_err(EdgeScanSourceError::Source)
                .map_err(EdgeExpansionSourceError::Read)?;
            return Ok(Some(AsyncEdgeCandidate {
                eid: candidate.eid,
                record: Some(record),
            }));
        }
    }

    fn reserve_traversal<C>(
        &self,
        hops: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<MemoryCharge, EdgeScanSourceError<AuthorizedBufferedError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        // One source record per active hop plus native binding/choice/position
        // vectors and their headers. Payload and directory owners stay separate.
        let bytes = size_of::<EdgeRecord>()
            .checked_add(512)
            .and_then(|slot| slot.checked_mul(hops.saturating_add(1)))
            .and_then(|bytes| bytes.checked_add(2048))
            .ok_or_else(|| EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into()))?;
        self.pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(|error| EdgeScanSourceError::Source(error.into()))
    }

    fn reserve_join_output<C>(
        &self,
        records: &[EdgeRecord],
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<OutputGuard, EdgeScanSourceError<AuthorizedBufferedError, C>> {
        // Every retained hop can contribute selected fields to every output
        // column. Sum their payloads before projection, then keep an independent
        // owner: a popped traversal record cannot refund a delivered value.
        let mut bytes = 1024usize;
        for record in records {
            control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
            bytes = bytes
                .checked_add(1024)
                .ok_or_else(|| {
                    EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into())
                })?;
            let edge = record.fields.edge();
            let target = if edge.entry().src == edge.entry().dst {
                &[][..]
            } else {
                record.fields.target_properties()
            };
            for (_, value) in edge
                .properties()
                .iter()
                .chain(record.fields.source_properties())
                .chain(target)
            {
                control(GlaExecutionEvent::Work).map_err(EdgeScanSourceError::Control)?;
                let payload = value.canonical_encoded_len().map_err(|_| {
                    EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into())
                })?;
                bytes = payload
                    .checked_mul(4)
                    .and_then(|payload| payload.checked_add(256))
                    .and_then(|payload| bytes.checked_add(payload))
                    .ok_or_else(|| {
                        EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into())
                    })?;
            }
        }
        let bytes = bytes
            .checked_mul(columns.max(1))
            .ok_or_else(|| EdgeScanSourceError::Source(BufferedReadError::SizeOverflow.into()))?;
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)?;
        let copy = self
            .pool
            .reserve(self.cx, bytes)
            .map_err(BufferedReadError::Memory)
            .map_err(|error| EdgeScanSourceError::Source(error.into()))?;
        let evaluation = self
            .pool
            .reserve(self.cx, 0)
            .map_err(BufferedReadError::Memory)
            .map_err(|error| EdgeScanSourceError::Source(error.into()))?;
        Ok(OutputGuard {
            _copy: copy,
            _evaluation: Arc::new(Mutex::new(evaluation)),
        })
    }

    fn join_output_event(
        &self,
        guard: &mut OutputGuard,
        event: GlaExecutionEvent,
    ) -> Result<(), AuthorizedBufferedError> {
        if event == GlaExecutionEvent::ScratchEntry {
            scratch(self.cx, &guard._evaluation)?;
        }
        Ok(())
    }
}

// Preserve cold source failures without extending or laundering QueryError's
// resident storage sum; all native structural/budget failures use its owners.
fn edge_failure(
    error: EdgeScanError<AuthorizedBufferedError>,
) -> Result<EdgeScanError<QueryError>, AuthorizedBufferedError> {
    Ok(match error {
        EdgeScanError::Source(error) => return Err(error),
        EdgeScanError::Plan(error) => EdgeScanError::Plan(error),
        EdgeScanError::NonIncreasingIdentity => EdgeScanError::NonIncreasingIdentity,
        EdgeScanError::InvalidCandidateAdmission => EdgeScanError::InvalidCandidateAdmission,
        EdgeScanError::DanglingEndpoint => EdgeScanError::DanglingEndpoint,
        EdgeScanError::CounterExhausted => EdgeScanError::CounterExhausted,
        EdgeScanError::ExpansionUnavailable => EdgeScanError::ExpansionUnavailable,
        EdgeScanError::BoundEdgeUnavailable => EdgeScanError::BoundEdgeUnavailable,
    })
}

pub(super) fn vertex_error(
    error: GqlQueryError<VertexScanError<AuthorizedBufferedError>, QueryError>,
) -> AuthorizedBufferedError {
    let error = match error {
        GqlQueryError::Interrupted(error) => return error.into(),
        GqlQueryError::Source(VertexScanError::Source(error)) => return error,
        GqlQueryError::Source(VertexScanError::Plan(error)) => {
            GqlQueryError::Source(VertexScanError::Plan(error))
        }
        GqlQueryError::Source(VertexScanError::NonIncreasingIdentity) => {
            GqlQueryError::Source(VertexScanError::NonIncreasingIdentity)
        }
        GqlQueryError::Source(VertexScanError::InvalidCandidateAdmission) => {
            GqlQueryError::Source(VertexScanError::InvalidCandidateAdmission)
        }
        GqlQueryError::Source(VertexScanError::CounterExhausted) => {
            GqlQueryError::Source(VertexScanError::CounterExhausted)
        }
        GqlQueryError::Source(VertexScanError::Probe(error)) => match edge_failure(error) {
            Ok(error) => GqlQueryError::Source(VertexScanError::Probe(error)),
            Err(error) => return error,
        },
        GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
        GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
        GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
        GqlQueryError::Data(error) => GqlQueryError::Data(error),
    };
    scan_error(error).into()
}

pub(super) fn edge_error(
    error: GqlQueryError<EdgeScanError<AuthorizedBufferedError>, QueryError>,
) -> AuthorizedBufferedError {
    let error = match error {
        GqlQueryError::Interrupted(error) => return error.into(),
        GqlQueryError::Source(error) => match edge_failure(error) {
            Ok(error) => GqlQueryError::Source(error),
            Err(error) => return error,
        },
        GqlQueryError::Rows(error) => GqlQueryError::Rows(error),
        GqlQueryError::Evaluator(error) => GqlQueryError::Evaluator(error),
        GqlQueryError::IdentifiedEdgesRequired => GqlQueryError::IdentifiedEdgesRequired,
        GqlQueryError::Data(error) => GqlQueryError::Data(error),
    };
    super::super::aggregate::edge::row_error(error).into()
}
