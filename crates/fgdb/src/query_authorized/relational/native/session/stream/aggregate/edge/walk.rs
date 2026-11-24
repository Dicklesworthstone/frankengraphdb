//! Awaitable variable paths over the existing resident, scoped source.
//! Only admitted records on the native traversal survive a pull. No raw
//! metadata, independent permit, graph copy or adjacency cache escapes here.

use super::*;
use fgdb_gql::algebra::GlaDirection;
use fgdb_gql::edge_stream::{
    AsyncEdgeCandidate, AsyncEdgeJoinSource, AsyncEdgeScanEvent, AsyncEdgeScanRecord,
    AsyncEdgeScanSource, AsyncGraphWalkSource, AsyncIncidentCandidateResult,
};
use fgdb_gql::stream::{AsyncVertexCandidate, AsyncVertexScanEvent, AsyncVertexScanRecord};

pub(super) struct Record {
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
pub(super) struct RootRecord(VertexScanRecord<'static>);
impl AsyncVertexScanRecord for RootRecord {
    fn as_row(&self) -> VertexScanRow<'_> {
        self.0.as_row()
    }
}

fn source_event(event: GlaExecutionEvent) -> AsyncEdgeScanEvent {
    match event {
        GlaExecutionEvent::ScratchEntry => AsyncEdgeScanEvent::ScratchEntry,
        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => AsyncEdgeScanEvent::Work,
    }
}
fn vertex_event(event: VertexScanEvent) -> AsyncVertexScanEvent {
    match event {
        VertexScanEvent::ScratchEntry => AsyncVertexScanEvent::ScratchEntry,
        VertexScanEvent::Work => AsyncVertexScanEvent::Work,
    }
}

impl<E, V, Clock: FnMut() -> u64> Source<'_, E, V, Clock>
where
    E: EdgeScanSource<Error = ReadError>,
    V: VertexScanSource<Error = ReadError>,
{
    fn walk_vertex<C>(
        &self,
        vid: VId,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'static>>, EdgeScanSourceError<QueryError, C>> {
        let Some(row) = self.vertices.admitted_vertex(vid, control)? else {
            return Ok(None);
        };
        VertexScanRecord::copy_masked(
            row,
            |label| {
                self.vertices
                    .execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_label(label)
            },
            |key| {
                self.vertices
                    .execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_property(key)
            },
            control,
        )
        .map(Some)
        .map_err(EdgeScanSourceError::Control)
    }

    fn walk_capture<C>(
        &self,
        eid: EId,
        relation: EdgeRelation,
        control: &mut impl FnMut(AsyncEdgeScanEvent) -> Result<(), C>,
    ) -> Result<AsyncEdgeCandidate<Record>, EdgeScanSourceError<QueryError, C>> {
        // Enumeration already admitted relation and BOTH endpoints. Hidden
        // histories spend neither candidate nor signed node/work allowances.
        // The exact same immutable winner is checked before copying fields.
        control(AsyncEdgeScanEvent::Candidate(eid)).map_err(EdgeScanSourceError::Control)?;
        let Some(edge) = self.visible(eid, relation)? else {
            return Ok(AsyncEdgeCandidate { eid, record: None });
        };
        let source = self.walk_vertex(edge.source, &mut |event| {
            control(source_event(edge_event(event)))
        })?;
        let target = if edge.source == edge.target {
            None
        } else {
            self.walk_vertex(edge.target, &mut |event| {
                control(source_event(edge_event(event)))
            })?
        };
        let edge = EdgeScanRecord::copy_masked(
            edge,
            |key| {
                self.vertices
                    .execution
                    .borrow()
                    .permit
                    .predicates()
                    .allows_property(key)
            },
            &mut |event| control(source_event(event)),
        )
        .map_err(EdgeScanSourceError::Control)?;
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

impl<E, V, Clock: FnMut() -> u64 + Send> AsyncEdgeScanSource for Source<'_, E, V, Clock>
where
    E: EdgeScanSource<Error = ReadError> + Send,
    V: VertexScanSource<Error = ReadError> + Send,
{
    type Error = QueryError;
    type Record = Record;
    type OutputGuard = ();

    fn snapshot_seq(&self) -> CommitSeq {
        self.edges.snapshot_seq()
    }
    async fn next_candidate<C: Send>(
        &mut self,
        relation: EdgeRelation,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncEdgeCandidate<Record>>, EdgeScanSourceError<QueryError, C>> {
        let id =
            self.next_edge_for_relation(relation, &mut |event| control(source_event(event)))?;
        id.map(|eid| self.walk_capture(eid, relation, control))
            .transpose()
    }
    fn reserve_output<C>(
        &self,
        _: &Record,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
    fn evaluation_event(&self, _: &Record, _: GlaExecutionEvent) -> Result<(), QueryError> {
        // The native meter charges every allocation before this callback.
        // Explicit unit guards make no physical resident-memory claim.
        self.vertices.execution.borrow_mut().poll()
    }
}

impl<E, V, Clock: FnMut() -> u64 + Send> AsyncEdgeJoinSource for Source<'_, E, V, Clock>
where
    E: EdgeScanSource<Error = ReadError> + Send,
    V: VertexScanSource<Error = ReadError> + Send,
{
    type TraversalGuard = ();

    async fn next_incident_candidate<C: Send>(
        &mut self,
        endpoint: VId,
        relation: EdgeRelation,
        direction: GlaDirection,
        after: Option<EId>,
        control: &mut (impl FnMut(AsyncEdgeScanEvent) -> Result<(), C> + Send),
    ) -> AsyncIncidentCandidateResult<Record, QueryError, C> {
        let id = self.next_incident_edge_for_relation(
            endpoint,
            relation,
            direction,
            after,
            &mut |event| control(source_event(event)),
        )?;
        id.map(|eid| self.walk_capture(eid, relation, control))
            .transpose()
            .map_err(EdgeExpansionSourceError::Read)
    }
    fn reserve_traversal<C>(
        &self,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
    fn reserve_join_output<C>(
        &self,
        _: &[Record],
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
    fn join_output_event(&self, _: &mut (), _: GlaExecutionEvent) -> Result<(), QueryError> {
        self.vertices.execution.borrow_mut().poll()
    }
}

impl<E, V, Clock: FnMut() -> u64 + Send> AsyncGraphWalkSource for Source<'_, E, V, Clock>
where
    E: EdgeScanSource<Error = ReadError> + Send,
    V: VertexScanSource<Error = ReadError> + Send,
{
    type RootRecord = RootRecord;

    async fn next_root_candidate<C: Send>(
        &mut self,
        control: &mut (impl FnMut(AsyncVertexScanEvent) -> Result<(), C> + Send),
    ) -> Result<Option<AsyncVertexCandidate<RootRecord>>, EdgeScanSourceError<QueryError, C>> {
        let Some(vid) = self
            .vertices
            .next_vertex(&mut |event| control(vertex_event(event)))?
        else {
            return Ok(None);
        };
        control(AsyncVertexScanEvent::Candidate(vid)).map_err(EdgeScanSourceError::Control)?;
        let record = self
            .walk_vertex(vid, &mut |event| control(vertex_event(event)))?
            .map(RootRecord);
        Ok(Some(AsyncVertexCandidate { vid, record }))
    }
    fn root_evaluation_event(
        &self,
        _: &RootRecord,
        _: GlaExecutionEvent,
    ) -> Result<(), QueryError> {
        self.vertices.execution.borrow_mut().poll()
    }
    fn reserve_walk_output<C>(
        &self,
        _: &RootRecord,
        records: &[Record],
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        self.reserve_join_output(records, columns, control)
    }
    fn reserve_shortest_state<C>(
        &self,
        _: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        control(GlaExecutionEvent::ScratchEntry).map_err(EdgeScanSourceError::Control)
    }
    fn reserve_shortest_output<C>(
        &self,
        _: &RootRecord,
        records: &[Record],
        _: usize,
        columns: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<(), EdgeScanSourceError<QueryError, C>> {
        self.reserve_join_output(records, columns, control)
    }
    fn release_shortest_temporaries(&self, _: &RootRecord, _: &[Record]) {}
}
