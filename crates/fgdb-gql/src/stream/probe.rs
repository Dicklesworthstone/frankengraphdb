//! The same compiled indexed probe kernel for a vertex-rooted outer row.
//! Adapt lookup/error/control vocabulary only; no second matching algorithm,
//! source scan, predicate evaluator, adjacency collection or per-probe budget.

use super::*;
use crate::edge_stream::{
    EdgeExpansionSourceError, EdgeScanError, EdgeScanRow, EdgeScanSource, EdgeScanSourceError,
    Probe,
};

pub(super) fn accepts<S: VertexScanSource, C>(
    probe: &Probe,
    vid: VId,
    source: &S,
    control: &mut impl FnMut(VertexScanEvent) -> ScanResult<(), S::Error, C>,
    record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
) -> ScanResult<bool, S::Error, C> {
    probe
        .accepts(
            &[Some(vid)],
            &Lookup(source),
            &mut |event| {
                control(vertex_event(event))
                    .map_err(|error| error.map_source(EdgeScanError::Source))
            },
            &mut || record().map_err(|error| error.map_source(EdgeScanError::Source)),
        )
        .map_err(|error| error.map_source(unpack))
}

fn vertex_event(event: GlaExecutionEvent) -> VertexScanEvent {
    match event {
        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => VertexScanEvent::Work,
    }
}
fn edge_event(event: VertexScanEvent) -> GlaExecutionEvent {
    match event {
        VertexScanEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
        VertexScanEvent::Work => GlaExecutionEvent::Work,
    }
}

fn source_error<E, C>(
    error: VertexScanSourceError<E, C>,
) -> EdgeScanSourceError<VertexScanError<E>, C> {
    match error {
        VertexScanSourceError::Source(error) => {
            EdgeScanSourceError::Source(VertexScanError::Source(error))
        }
        VertexScanSourceError::Control(error) => EdgeScanSourceError::Control(error),
    }
}
fn unavailable<E>() -> VertexScanError<E> {
    VertexScanError::Probe(EdgeScanError::ExpansionUnavailable)
}

struct Lookup<'a, S>(&'a S);
impl<S: VertexScanSource> EdgeScanSource for Lookup<'_, S> {
    type Error = VertexScanError<S::Error>;

    fn snapshot_seq(&self) -> CommitSeq {
        self.0.snapshot_seq()
    }

    fn next_edge<C>(
        &mut self,
        _control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeScanSourceError<Self::Error, C>> {
        // Probes enumerate vertices with caller-owned positions, not this
        // mutable outer-edge scan. Accidental calls must not fabricate EOF.
        Err(EdgeScanSourceError::Source(unavailable()))
    }

    fn next_probe_vertex<C>(
        &self,
        after: Option<VId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.0
            .next_probe_vertex(after, control)
            .map_err(|error| match error {
                EdgeExpansionSourceError::Unavailable => EdgeExpansionSourceError::Unavailable,
                EdgeExpansionSourceError::Read(error) => {
                    EdgeExpansionSourceError::Read(source_error(error))
                }
            })
    }

    fn edge<'a, C>(
        &'a self,
        eid: EId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EdgeScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        self.0
            .probe_edge(eid, control)
            .map_err(|error| match error {
                EdgeExpansionSourceError::Unavailable => EdgeScanSourceError::Source(unavailable()),
                EdgeExpansionSourceError::Read(error) => source_error(error),
            })
    }

    fn vertex<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRow<'a>>, EdgeScanSourceError<Self::Error, C>> {
        self.0
            .vertex(vid, &mut |event| control(edge_event(event)))
            .map_err(source_error)
    }

    fn vertex_record<'a, C>(
        &'a self,
        vid: VId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<VertexScanRecord<'a>>, EdgeScanSourceError<Self::Error, C>> {
        self.0
            .vertex_record(vid, &mut |event| control(edge_event(event)))
            .map_err(source_error)
    }

    fn vertex_property<'a, C>(
        &'a self,
        vid: VId,
        key: PropertyKeyId,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<Option<&'a CanonicalScalar>>, EdgeScanSourceError<Self::Error, C>> {
        self.0
            .vertex_property(vid, key, &mut |event| control(edge_event(event)))
            .map_err(source_error)
    }

    fn next_incident_edge<C>(
        &self,
        endpoint: VId,
        direction: crate::algebra::GlaDirection,
        after: Option<EId>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<EId>, EdgeExpansionSourceError<Self::Error, C>> {
        self.0
            .next_probe_edge(endpoint, direction, after, control)
            .map_err(|error| match error {
                EdgeExpansionSourceError::Unavailable => EdgeExpansionSourceError::Unavailable,
                EdgeExpansionSourceError::Read(error) => {
                    EdgeExpansionSourceError::Read(source_error(error))
                }
            })
    }
}

// Real source failures and outer controls keep their exact original variants.
// Only structural failures from the shared probe get the extra Probe context.
fn unpack<E>(error: EdgeScanError<VertexScanError<E>>) -> VertexScanError<E> {
    VertexScanError::Probe(match error {
        EdgeScanError::Source(error) => return error,
        EdgeScanError::Plan(error) => EdgeScanError::Plan(error),
        EdgeScanError::NonIncreasingIdentity => EdgeScanError::NonIncreasingIdentity,
        EdgeScanError::DanglingEndpoint => EdgeScanError::DanglingEndpoint,
        EdgeScanError::CounterExhausted => EdgeScanError::CounterExhausted,
        EdgeScanError::ExpansionUnavailable => EdgeScanError::ExpansionUnavailable,
        EdgeScanError::BoundEdgeUnavailable => EdgeScanError::BoundEdgeUnavailable,
    })
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod record_tests;
