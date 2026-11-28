//! Computed single-edge rows over the existing asynchronous source kernel.
//! The admitted record remains owned through projection and, when needed,
//! its second undirected orientation. No extra history lookup is introduced.

use super::*;
use crate::algebra::GraphValue;
use crate::scan_stream::ScanError;
use crate::{GraphSetRowPull, PreparedGraphSet};
use fgdb_delta_types::ElementId;
use std::cell::RefCell;

type ProjectionResult<T, E, C> = Result<T, GqlQueryError<ScanError<E>, C>>;

/// A checked computed projection over one ordered, identified edge source.
/// Directed roots require their EId order; undirected roots also require the
/// oriented source VId. The complete existing compiler proves that ordering,
/// including any private keys, before a source can be opened.
///
/// All native predicates and outer VM cells contribute bounded edge/endpoint
/// property demands. Direct bound element columns may read those fields;
/// indirect receivers, metadata, joins and unproved source order refuse.
#[derive(Clone)]
pub struct AsyncEdgeProjectionPlan {
    input: AsyncEdgeScanPlan,
    pull: GraphSetRowPull,
    properties: EdgeScanPropertySelection,
}

impl AsyncEdgeProjectionPlan {
    pub fn compile(query: &PreparedGraphSet) -> Result<Self, EdgeScanBuildError> {
        let refused = || EdgeScanBuildError { operator: 0 };
        let pull = query.edge_row_pull().ok_or_else(refused)?;
        let input = AsyncEdgeScanPlan::compile(pull.pattern().plan())?;
        // Pagination belongs to the completed VM rows. Successful completion
        // must still evaluate the full source suffix, including LIMIT zero.
        if input.inner.offset != 0 || input.inner.count.is_some() {
            return Err(refused());
        }
        let mut properties = input.selected_property_demand().ok_or_else(refused)?;
        let projected = pull.edge_projection_properties().ok_or_else(refused)?;
        for &key in projected.edge.keys() {
            properties.edge.insert(key).map_err(|_| refused())?;
        }
        for &key in projected.endpoints.keys() {
            properties.endpoints.insert(key).map_err(|_| refused())?;
        }
        Ok(Self {
            input,
            pull,
            properties,
        })
    }

    /// Complete pre-authorization demand. The host intersects both finite
    /// sets with the same permit used for edge and endpoint admission.
    #[must_use]
    pub fn selected_property_demand(&self) -> EdgeScanPropertySelection {
        self.properties
    }

    pub fn root_relation(&self) -> EdgeRelation {
        self.input.root_relation()
    }
    pub fn root_direction(&self) -> GlaDirection {
        self.input.root_direction()
    }
}

impl core::fmt::Debug for AsyncEdgeProjectionPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncEdgeProjectionPlan([REDACTED])")
    }
}

/// A fused computed-row cursor with the original source and cumulative meter.
/// Only selected final rows spend ResultRows. Every matching orientation runs
/// the VM before its final window; successful EOF validates the entire suffix.
/// Dropping or closing releases the unread suffix without claiming completion.
///
/// A non-self undirected edge keeps one admitted record between orientations.
/// Projection events grow that record's reservation, retained by each output
/// guard. A polled pending future owns both source and pending record, so its
/// cancellation or unwind cannot resume a partially consumed source prefix.
pub struct AsyncEdgeProjectionCursor<S: AsyncEdgeScanSource, F> {
    input: AsyncEdgeScanCursor<S, F>,
    pull: GraphSetRowPull,
    ordinal: u64,
}

impl<S: AsyncEdgeScanSource, F> AsyncEdgeProjectionCursor<S, F> {
    pub fn new(
        source: S,
        plan: AsyncEdgeProjectionPlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: AsyncEdgeScanCursor::new(source, plan.input, policy, checkpoint),
            pull: plan.pull,
            ordinal: 0,
        }
    }

    pub fn snapshot_seq(&self) -> CommitSeq {
        self.input.snapshot_seq()
    }
    pub fn state(&self) -> EdgeScanState {
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

    #[allow(clippy::should_implement_trait)]
    pub fn next<C>(
        &mut self,
    ) -> impl core::future::Future<
        Output = Option<ProjectionResult<AsyncEdgeScanOutput<S::OutputGuard>, S::Error, C>>,
    > + Send
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        self.next_inner::<C>()
    }

    async fn next_inner<C>(
        &mut self,
    ) -> Option<ProjectionResult<AsyncEdgeScanOutput<S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.input.state != EdgeScanState::Open {
            return None;
        }
        self.input.state = EdgeScanState::Failed;
        let mut source = self
            .input
            .source
            .take()
            .expect("open cursor owns its source");
        let mut pending = self.input.pending.take();
        match self.advance::<C>(&mut source, &mut pending).await {
            Ok(Some(output)) => {
                self.input.source = Some(source);
                self.input.pending = pending;
                self.input.state = EdgeScanState::Open;
                Some(Ok(output))
            }
            Ok(None) => {
                self.input.state = EdgeScanState::Exhausted;
                None
            }
            Err(error) => Some(Err(error)),
        }
    }

    async fn advance<C>(
        &mut self,
        source: &mut S,
        pending: &mut Option<(EId, S::Record)>,
    ) -> ProjectionResult<Option<AsyncEdgeScanOutput<S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        loop {
            let Some((output, record, eid, reverse)) = self
                .input
                .advance_record::<false, C>(source, pending)
                .await
                .map_err(lift)?
            else {
                return Ok(None);
            };
            let ordinal = self.ordinal;
            let next_ordinal = ordinal.checked_add(1).ok_or_else(|| {
                GqlQueryError::Source(ScanError::Edge(EdgeScanError::CounterExhausted))
            })?;
            let next_result = if self.pull.selects_row(ordinal) {
                Some(
                    self.input
                        .meter
                        .increment(
                            GqlBudgetDimension::ResultRows,
                            self.input.meter.rows.result_rows,
                        )
                        .map_err(lift)?,
                )
            } else {
                None
            };
            let (row, guard) = output.into_parts();
            let projected = {
                // These callbacks are mutually exclusive; no RefCell borrow
                // or borrowed property survives an event or an await.
                let meter = RefCell::new(&mut self.input.meter);
                let charge = |event: GlaExecutionEvent| -> ProjectionResult<(), S::Error, C> {
                    let event = match event {
                        GlaExecutionEvent::ResultRow => GlaExecutionEvent::Work,
                        event => event,
                    };
                    meter.borrow_mut().event(event).map_err(lift)?;
                    source.evaluation_event(&record, event).map_err(|error| {
                        GqlQueryError::Source(ScanError::Edge(EdgeScanError::Source(error)))
                    })
                };
                self.pull.project_with_properties(
                    &row,
                    ordinal,
                    &mut |event| charge(event),
                    &mut |element, key| {
                        charge(GlaExecutionEvent::Work)?;
                        let edge = record.edge();
                        let properties = match element {
                            ElementId::Edge(requested) if requested == eid => edge.properties,
                            ElementId::Vertex(vid) if vid == edge.source || vid == edge.target => {
                                record
                                    .vertex(vid)
                                    .ok_or_else(|| {
                                        GqlQueryError::Source(ScanError::Edge(
                                            EdgeScanError::DanglingEndpoint,
                                        ))
                                    })?
                                    .properties
                            }
                            _ => {
                                return Err(GqlQueryError::Source(ScanError::Edge(
                                    EdgeScanError::Plan(EdgeScanBuildError { operator: 0 }),
                                )));
                            }
                        };
                        for (field, value) in properties {
                            charge(GlaExecutionEvent::Work)?;
                            if *field == key {
                                // Canonical copy admission precedes clone;
                                // the VM separately admits its owned conversion.
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
            if let Some(next) = next_result {
                row_event(
                    &mut self.input.meter,
                    source,
                    &record,
                    GlaExecutionEvent::ResultRow,
                )
                .map_err(lift)?;
                self.input.meter.rows.result_rows = next;
                if reverse {
                    *pending = Some((eid, record));
                }
                return Ok(Some(AsyncEdgeScanOutput {
                    row: projected,
                    guard,
                }));
            }
            if reverse {
                *pending = Some((eid, record));
            }
            // A skipped or suffix row is discarded before advancing. The
            // parked record, when any, supplies its remaining orientation.
        }
    }
}

impl<S: AsyncEdgeScanSource, F> core::fmt::Debug for AsyncEdgeProjectionCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeProjectionCursor")
            .field("input", &self.input)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

fn lift<E, C>(error: GqlQueryError<EdgeScanError<E>, C>) -> GqlQueryError<ScanError<E>, C> {
    error.map_source(ScanError::Edge)
}
