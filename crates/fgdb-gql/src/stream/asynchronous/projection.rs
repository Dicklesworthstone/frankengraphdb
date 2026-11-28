//! Computed vertex rows over the existing asynchronous source kernel.
//! The checked identity order, projection VM and native/host meters stay the
//! same owners as ordinary execution. Only the current record is retained.

use super::*;
use crate::algebra::GraphValueRow;
use crate::scan_stream::ScanError;
use crate::{GraphSetRowPull, PreparedGraphSet};
use fgdb_delta_types::ElementId;
use std::cell::RefCell;

type ProjectionResult<T, E, C> = Result<T, GqlQueryError<ScanError<E>, C>>;

/// A complete row-local projection with a proved increasing root-identity
/// order. The source and projection are paired privately; a caller cannot
/// replace the source with a differently paged or scoped definition.
///
/// Every property lookup is proved to address the current root. All source
/// predicates, public cells, private ranking cells and lazy branches contribute
/// to one bounded selected-property demand before the host opens its source.
/// Unsupported graph reads and key-set overflow refuse without materializing
/// a graph or selecting a complete-record fallback.
#[derive(Clone)]
pub struct AsyncVertexProjectionPlan {
    input: AsyncVertexScanPlan<GraphValueRow>,
    pull: GraphSetRowPull,
    properties: PropertySelection,
}

impl AsyncVertexProjectionPlan {
    pub fn compile(query: &PreparedGraphSet) -> Result<Self, VertexScanBuildError> {
        let refused = || VertexScanBuildError { operator: 0 };
        let pull = query.vertex_graph_row_pull().ok_or_else(refused)?;
        let input = AsyncVertexScanPlan::compile(pull.pattern().plan())?;
        // A final window belongs to the completed VM rows. The raw input must
        // reach every matching occurrence, including a projected LIMIT 0.
        if input.inner.offset != 0 || input.inner.count.is_some() {
            return Err(refused());
        }
        let mut properties = match input.property_demand() {
            VertexScanPropertyDemand::Topology => PropertySelection::EMPTY,
            VertexScanPropertyDemand::Properties(properties) => properties,
            VertexScanPropertyDemand::Complete => return Err(refused()),
        };
        let projected = pull.vertex_projection_properties().ok_or_else(refused)?;
        for &key in projected.keys() {
            properties.insert(key).map_err(|_| refused())?;
        }
        Ok(Self {
            input,
            pull,
            properties,
        })
    }

    /// Complete pre-authorization demand. Hosts must mask this bounded set
    /// with the same read permit used to admit the source identities.
    #[must_use]
    pub fn property_demand(&self) -> VertexScanPropertyDemand {
        if self.properties.keys().is_empty() {
            VertexScanPropertyDemand::Topology
        } else {
            VertexScanPropertyDemand::Properties(self.properties)
        }
    }
}

impl core::fmt::Debug for AsyncVertexProjectionPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncVertexProjectionPlan([REDACTED])")
    }
}

/// A fused computed-row cursor with one cumulative native policy. Source
/// records are private occurrences; only final selected rows spend ResultRows.
/// The ordinary VM evaluates each reached occurrence before pagination, so a
/// successful EOF also validates the suffix after LIMIT (including LIMIT 0).
/// Closing or dropping releases the unread suffix without evaluating it.
///
/// The source record and its allocation reservation remain alive during the
/// VM. Every event also reaches the source's evaluation_event before allocation;
/// reserve_output must retain that growing reservation with the delivered row.
/// Dropping a polled pending next future releases the source and permanently
/// fuses the cursor, just as the ordinary asynchronous source cursor does.
pub struct AsyncVertexProjectionCursor<S: AsyncVertexScanSource, F> {
    input: AsyncVertexScanCursor<S, F, GraphValueRow>,
    pull: GraphSetRowPull,
    ordinal: u64,
}

impl<S: AsyncVertexScanSource, F> AsyncVertexProjectionCursor<S, F> {
    pub fn new(
        source: S,
        plan: AsyncVertexProjectionPlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: AsyncVertexScanCursor::new(source, plan.input, policy, checkpoint),
            pull: plan.pull,
            ordinal: 0,
        }
    }

    pub fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    pub fn state(&self) -> VertexScanState {
        self.input.state()
    }
    pub fn row_stats(&self) -> GqlExecutionStats {
        self.input.row_stats()
    }
    pub fn evaluator_stats(&self) -> GlaExecutionStats {
        self.input.evaluator_stats()
    }
    pub fn close(&mut self) {
        self.input.close();
    }

    pub async fn next<C>(
        &mut self,
    ) -> Option<ProjectionResult<AsyncVertexScanOutput<GraphValueRow, S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.input.state != VertexScanState::Open {
            return None;
        }
        // The polled future exclusively owns the source through every await.
        // Only a complete row restores it; cancellation/unwind cannot resume
        // from a partially consumed occurrence and claim successful completion.
        self.input.state = VertexScanState::Failed;
        let mut source = self
            .input
            .source
            .take()
            .expect("open cursor owns its source");
        match self.advance::<C>(&mut source).await {
            Ok(Some(output)) => {
                self.input.source = Some(source);
                self.input.state = VertexScanState::Open;
                Some(Ok(output))
            }
            Ok(None) => {
                self.input.state = VertexScanState::Exhausted;
                None
            }
            Err(error) => Some(Err(error)),
        }
    }

    async fn advance<C>(
        &mut self,
        source: &mut S,
    ) -> ProjectionResult<Option<AsyncVertexScanOutput<GraphValueRow, S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        loop {
            let Some((output, record, vid)) = self
                .input
                .advance_record::<false, C>(source)
                .await
                .map_err(lift)?
            else {
                return Ok(None);
            };
            let ordinal = self.ordinal;
            let next_ordinal = ordinal.checked_add(1).ok_or_else(|| {
                GqlQueryError::Source(ScanError::Vertex(VertexScanError::CounterExhausted))
            })?;
            let selected = self.pull.selects_row(ordinal);
            if selected {
                self.input.meter.next_result_count().map_err(lift)?;
            }
            let (row, guard) = output.into_parts();
            let projected = {
                // VM events and element reads are mutually exclusive callbacks.
                // Neither mutable meter borrow survives an event or any await.
                let meter = RefCell::new(&mut self.input.meter);
                let charge = |event: GlaExecutionEvent| -> ProjectionResult<(), S::Error, C> {
                    let event = match event {
                        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                            VertexScanEvent::Work
                        }
                    };
                    meter.borrow_mut().event(event).map_err(lift)?;
                    source.evaluation_event(&record, event).map_err(|error| {
                        GqlQueryError::Source(ScanError::Vertex(VertexScanError::Source(error)))
                    })
                };
                self.pull.project_with_properties(
                    &row,
                    ordinal,
                    &mut |event| charge(event),
                    &mut |element, key| {
                        charge(GlaExecutionEvent::Work)?;
                        // This is an invariant check on the compiled receiver,
                        // never a lookup into a second graph or an absent value.
                        if element != ElementId::Vertex(vid) {
                            return Err(GqlQueryError::Source(ScanError::Vertex(
                                VertexScanError::Plan(VertexScanBuildError { operator: 0 }),
                            )));
                        }
                        for (field, value) in record.as_row().properties {
                            charge(GlaExecutionEvent::Work)?;
                            if *field == key {
                                // Admission covers the full canonical copy
                                // BEFORE cloning. The VM separately accounts
                                // for conversion into its owned output value.
                                GraphValue::charge_property_value(value, &mut |event| {
                                    charge(event)
                                })?;
                                return Ok(Some(value.clone()));
                            }
                        }
                        Ok(None)
                    },
                    |column, error| {
                        GqlQueryError::Source(ScanError::Projection {
                            row: ordinal,
                            column,
                            error,
                        })
                    },
                )?
            };
            self.ordinal = next_ordinal;
            if selected {
                self.input.meter.emit().map_err(lift)?;
                return Ok(Some(AsyncVertexScanOutput {
                    row: projected,
                    guard,
                }));
            }
            // Drop the skipped/suffix row, guard and retained record before
            // the next source await. The output page never shortens evaluation.
        }
    }
}

impl<S: AsyncVertexScanSource, F> core::fmt::Debug for AsyncVertexProjectionCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncVertexProjectionCursor")
            .field("input", &self.input)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

fn lift<E, C>(error: GqlQueryError<VertexScanError<E>, C>) -> GqlQueryError<ScanError<E>, C> {
    error.map_source(ScanError::Vertex)
}
