//! Pinned resident element reads for completed external aggregate output.
//! The input cursor remains the sole record/work/scratch/result meter. A
//! retained view keeps its exact generation alive after that cursor releases
//! its source; neither a second query nor a replacement allowance is opened.

use super::*;
use crate::gql_exec::property_maps::{self, MetadataRecord};
use crate::gql_exec::source::{self, SourceEvent};
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::{
    GlaExecutionEvent, GraphElementMetadata, GraphElementMetadataError, GraphElementReader,
};
use fgdb_types::CanonicalPropertyValue;
use std::cell::RefCell;

struct ResidentInput<'q> {
    input: Box<dyn GroupInput + 'q>,
    view: EmbeddedReadView,
}

pub(in super::super) fn retain_element_reader<'q>(
    input: Box<dyn GroupInput + 'q>,
    view: EmbeddedReadView,
) -> Box<dyn GroupInput + 'q> {
    Box::new(ResidentInput { input, view })
}

impl GroupInput for ResidentInput<'_> {
    fn next_input(&mut self) -> crate::SendFuture<'_, Result<Option<SpoolRow>>> {
        self.input.next_input()
    }
    fn charge(&mut self, event: VertexScanEvent) -> core::result::Result<(), ExecutionError> {
        self.input.charge(event)
    }
    fn finish_result(&mut self) -> core::result::Result<(), ExecutionError> {
        self.input.finish_result()
    }
    fn output_view(&self) -> Option<EmbeddedReadView> {
        Some(self.view.clone())
    }
    fn admit_output_record(&mut self) -> core::result::Result<(), ExecutionError> {
        self.input.admit_output_record()
    }
    fn native_control(&mut self, event: VertexScanEvent) -> Result<()> {
        self.input.native_control(event)
    }
    fn exhausted(&self) -> bool {
        self.input.exhausted()
    }
    fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    fn kind(&self) -> ScanKind {
        self.input.kind()
    }
    fn row_stats(&self) -> GqlExecutionStats {
        self.input.row_stats()
    }
    fn evaluator_stats(&self) -> GlaExecutionStats {
        self.input.evaluator_stats()
    }
}

struct ProjectionControl<'a> {
    input: &'a mut dyn GroupInput,
    charge: &'a mut MemoryCharge,
    cx: &'a QueryCx,
}
impl ProjectionControl<'_> {
    fn event(
        &mut self,
        event: VertexScanEvent,
    ) -> core::result::Result<(), ControlledExecutionError> {
        self.input.controlled_charge(event)?;
        if event == VertexScanEvent::ScratchEntry {
            // The same per-allocation grant as ordinary computed output:
            // payload, vector growth and its encoded-envelope overlap remain
            // owned until this group's complete frame has been written.
            let bytes = 8 * size_of::<GraphValue>()
                .max(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES);
            self.charge.grow(self.cx, bytes).map_err(|error| {
                GqlQueryError::Interrupted(SpillError::Memory(error).into())
            })?;
        }
        Ok(())
    }

    fn source_event(
        &mut self,
        event: SourceEvent,
    ) -> core::result::Result<(), ControlledExecutionError> {
        self.event(match event {
            SourceEvent::Work | SourceEvent::SnapshotRecord => VertexScanEvent::Work,
            SourceEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
        })
    }

    fn record(&mut self) -> core::result::Result<(), ControlledExecutionError> {
        // Cancellation and the original logical record ceiling precede any
        // history/key/payload observation, including absent and repeated IDs.
        self.event(VertexScanEvent::Work)?;
        self.input
            .admit_output_record()
            .map_err(execute_error)
            .map_err(GqlQueryError::Interrupted)
    }
}

struct Elements<'s, 'a> {
    view: EmbeddedReadView,
    as_of: CommitSeq,
    control: &'s RefCell<ProjectionControl<'a>>,
}
impl Elements<'_, '_> {
    fn visible_record<E>(
        &self,
        element: ElementId,
        control: &mut impl FnMut(SourceEvent) -> core::result::Result<(), E>,
    ) -> core::result::Result<Option<MetadataRecord<'_>>, E> {
        let snapshot = &self.view.snapshot;
        Ok(match element {
            ElementId::Vertex(id) => source::find_vertex(snapshot, id, self.as_of, control)?
                .map(MetadataRecord::Vertex),
            ElementId::Edge(id) => {
                let Some((block, row)) = snapshot.adjacency_index().statement_at_controlled(
                    &snapshot.blocks,
                    id,
                    self.as_of,
                    &mut |_| control(SourceEvent::Work),
                )? else {
                    return Ok(None);
                };
                Some(MetadataRecord::Edge(
                    snapshot.blocks[block][row].relation,
                    source::edge_properties_at(&snapshot.block_props, block, row),
                ))
            }
        })
    }
}

fn read_error(element: ElementId, error: ReadError) -> ControlledExecutionError {
    GqlQueryError::Source(fgdb_gql::GraphAggregateError::Source(match element {
        ElementId::Vertex(_) => {
            ScanError::Vertex(fgdb_gql::stream::VertexScanError::Source(error))
        }
        ElementId::Edge(_) => {
            ScanError::Edge(fgdb_gql::edge_stream::EdgeScanError::Source(error))
        }
    }))
}

impl GraphElementReader<ControlledExecutionError> for Elements<'_, '_> {
    fn element_property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> core::result::Result<Option<CanonicalPropertyValue>, ControlledExecutionError> {
        // Release this borrow before a lookup invokes its control callback.
        self.control.borrow_mut().record()?;
        let mut control = |event| self.control.borrow_mut().source_event(event);
        let Some(record) = self.visible_record(element, &mut control)? else {
            return Ok(None);
        };
        let props = match record {
            MetadataRecord::Vertex(row) => row.props.as_slice(),
            MetadataRecord::Edge(_, props) => props,
        };
        let (mut low, mut high) = (0, props.len());
        while low < high {
            control(SourceEvent::Work)?;
            let middle = low + (high - low) / 2;
            match props[middle].0.cmp(&key) {
                Ordering::Less => low = middle + 1,
                Ordering::Greater => high = middle,
                Ordering::Equal => {
                    let value = &props[middle].1;
                    let heap = value
                        .owned_heap_bytes_controlled(&mut || control(SourceEvent::Work))?
                        .ok_or_else(|| {
                            GqlQueryError::Interrupted(SpillError::SizeOverflow.into())
                        })?;
                    let bytes = size_of::<CanonicalPropertyValue>()
                        .checked_add(heap)
                        .ok_or_else(|| {
                            GqlQueryError::Interrupted(SpillError::SizeOverflow.into())
                        })?;
                    // Reserve the selected canonical value BEFORE cloning.
                    // Unrelated properties and complete records stay borrowed.
                    for _ in 0..bytes.div_ceil(
                        fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES,
                    ) {
                        control(SourceEvent::ScratchEntry)?;
                    }
                    return Ok(Some(value.clone()));
                }
            }
        }
        Ok(None)
    }

    fn element_metadata(
        &mut self,
        element: ElementId,
        kind: GraphElementMetadata,
        catalog: &fgdb_gql::ReverseSymbolCatalog,
        control: &mut dyn FnMut(GlaExecutionEvent) -> core::result::Result<(), ()>,
    ) -> Option<
        core::result::Result<
            Option<GraphValue>,
            GraphElementMetadataError<ControlledExecutionError>,
        >,
    > {
        Some((|| {
            // This source grant has no live RefCell borrow when the supplied
            // evaluator control begins reserving metadata allocations.
            self.control
                .borrow_mut()
                .record()
                .map_err(GraphElementMetadataError::Source)?;
            let mut observe = |event| {
                control(match event {
                    SourceEvent::Work | SourceEvent::SnapshotRecord => GlaExecutionEvent::Work,
                    SourceEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
                })
                .map_err(|()| GraphElementMetadataError::Interrupted)
            };
            observe(SourceEvent::Work)?;
            let Some(record) = self.visible_record(element, &mut observe)? else {
                return Ok(None);
            };
            property_maps::metadata(
                record,
                kind,
                catalog,
                |_| true,
                |_| true,
                &mut observe,
                &|error| GraphElementMetadataError::Source(read_error(element, error)),
            )
            .map(Some)
        })())
    }
}

pub(super) fn project(
    definition: &SpillAggregateDefinition,
    input: &mut dyn GroupInput,
    row: GraphAggregateRow,
    charge: &mut MemoryCharge,
    cx: &QueryCx,
    view: EmbeddedReadView,
) -> core::result::Result<
    (GraphAggregateRow, Option<GraphAggregateRow>),
    ControlledExecutionError,
> {
    let as_of = input.snapshot_seq();
    let control = RefCell::new(ProjectionControl { input, charge, cx });
    let mut elements = Elements {
        view,
        as_of,
        control: &control,
    };
    definition.project_output_with_rank_with_elements(
        row,
        &mut elements,
        &mut |event| control.borrow_mut().event(event),
    )
}
