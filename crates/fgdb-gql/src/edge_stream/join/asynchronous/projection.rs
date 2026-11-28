//! Computed rows over the existing asynchronous fixed-hop join kernel.
//! Each admitted occurrence keeps its complete binding frame until the shared
//! VM finishes. The independent output guard owns every copied VM payload.

use super::*;
use crate::algebra::GraphValue;
use crate::scan_stream::ScanError;
use crate::{GraphSetRowPull, PreparedGraphSet};
use fgdb_delta_types::ElementId;
use std::cell::RefCell;

type ProjectionResult<T, E, C> = Result<T, GqlQueryError<ScanError<E>, C>>;

/// A computed projection whose complete native fixed-hop input is already in
/// canonical traversal order. The ordinary join compiler proves the complete
/// identity prefix before any source is opened; the outer VM may retain its
/// ranking keys privately when the ALL projection has explicit identity order.
///
/// Every native instruction and outer VM cell contributes to bounded edge and
/// endpoint key unions. Direct current-binding element columns may read these
/// fields. Indirect receivers, metadata, native collection sources, terminal
/// OPTIONAL and unproved source order refuse without relaxing the native input
/// compiler. Ordinary computed output lists and maps keep their VM semantics.
#[derive(Clone)]
pub struct AsyncEdgeJoinProjectionPlan {
    input: AsyncEdgeJoinPlan,
    pull: GraphSetRowPull,
    properties: EdgeScanPropertySelection,
}

impl AsyncEdgeJoinProjectionPlan {
    pub fn compile(query: &PreparedGraphSet) -> Result<Self, EdgeScanBuildError> {
        let refused = || EdgeScanBuildError { operator: 0 };
        let pull = query.edge_row_pull().ok_or_else(refused)?;
        let input = AsyncEdgeJoinPlan::compile(pull.pattern().plan())?;
        // Only the final computed rows own pagination. All matching input,
        // including a skipped prefix and LIMIT-zero suffix, must reach the VM.
        // Optional-local records do not belong to the ordinary binding frame.
        if input.inner.offset != 0 || input.inner.count.is_some() || input.optional.is_some() {
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

    /// Complete pre-authorization demand across all held bindings. The host
    /// intersects both finite sets with its edge/endpoint admission permit.
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

impl core::fmt::Debug for AsyncEdgeJoinProjectionPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AsyncEdgeJoinProjectionPlan([REDACTED])")
    }
}

/// Fused computed-row execution over the native fixed-hop cursor and its
/// original cumulative meter. Only selected final rows spend ResultRows;
/// successful EOF evaluates every matching occurrence, including the suffix.
///
/// Projection borrows only the current retained frame, before its next resume
/// can pop a child. Its copies grow the independent join output guard, which
/// remains charged after every traversal record and the source are dropped.
/// A pending pull owns the source and frame; cancellation or unwind releases
/// both and cannot resume a partially consumed source prefix.
pub struct AsyncEdgeJoinProjectionCursor<S: AsyncEdgeJoinSource, F> {
    input: AsyncEdgeJoinCursor<S, F>,
    pull: GraphSetRowPull,
    ordinal: u64,
}

impl<S: AsyncEdgeJoinSource, F> AsyncEdgeJoinProjectionCursor<S, F> {
    pub fn new(
        source: S,
        plan: AsyncEdgeJoinProjectionPlan,
        policy: GqlQueryPolicy,
        checkpoint: F,
    ) -> Self {
        Self {
            input: AsyncEdgeJoinCursor::new(source, plan.input, policy, checkpoint),
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
        Output = Option<ProjectionResult<AsyncEdgeJoinOutput<S::OutputGuard>, S::Error, C>>,
    > + Send
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        self.next_inner::<C>()
    }

    async fn next_inner<C>(
        &mut self,
    ) -> Option<ProjectionResult<AsyncEdgeJoinOutput<S::OutputGuard>, S::Error, C>>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        if self.input.state != EdgeScanState::Open {
            return None;
        }
        self.input.state = EdgeScanState::Failed;
        let mut source = self.input.source.take().expect("open cursor owns source");
        // Keep the source alive until every borrowed-frame record is dropped.
        let mut frame = self.input.frame.take();
        match self.advance::<C>(&mut source, &mut frame).await {
            Ok(Some(output)) => {
                self.input.source = Some(source);
                self.input.frame = frame;
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
        frame: &mut Option<Frame<S::Record, S::TraversalGuard>>,
    ) -> ProjectionResult<Option<AsyncEdgeJoinOutput<S::OutputGuard>>, S::Error, C>
    where
        F: FnMut() -> Result<(), C> + Send,
        C: Send,
    {
        loop {
            let Some(output) = self
                .input
                .advance::<false, C>(source, frame)
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
            let (row, mut guard) = output.into_parts();
            let projected = {
                let frame = frame.as_ref().expect("a yielded join owns its bindings");
                let local = FrameSource::<_, S::Error> {
                    records: &frame.records,
                    choices: &frame.traversal.choices,
                    seq: self.input.seq,
                    error: core::marker::PhantomData,
                };
                // The VM and property reader call this synchronously. Neither
                // a RefCell borrow nor a field loan crosses an event or await.
                let owners = RefCell::new((&mut self.input.meter, &mut guard));
                let charge = |event: GlaExecutionEvent| -> ProjectionResult<(), S::Error, C> {
                    let event = match event {
                        GlaExecutionEvent::ResultRow => GlaExecutionEvent::Work,
                        event => event,
                    };
                    let mut owners = owners.borrow_mut();
                    owners.0.event(event).map_err(lift)?;
                    source.join_output_event(owners.1, event).map_err(|error| {
                        GqlQueryError::Source(ScanError::Edge(EdgeScanError::Source(error)))
                    })
                };
                self.pull.project_with_properties(
                    &row,
                    ordinal,
                    &mut |event| charge(event),
                    &mut |element, key| {
                        let properties = match element {
                            ElementId::Edge(eid) => {
                                flatten_read(local.edge(eid, &mut |event| charge(event)))?
                                    .ok_or_else(|| {
                                        GqlQueryError::Source(ScanError::Edge(
                                            EdgeScanError::BoundEdgeUnavailable,
                                        ))
                                    })?
                                    .properties
                            }
                            ElementId::Vertex(vid) => {
                                flatten_read(local.vertex(vid, &mut |event| charge(event)))?
                                    .ok_or_else(|| {
                                        GqlQueryError::Source(ScanError::Edge(
                                            EdgeScanError::DanglingEndpoint,
                                        ))
                                    })?
                                    .properties
                            }
                        };
                        for (field, value) in properties {
                            charge(GlaExecutionEvent::Work)?;
                            if *field == key {
                                // Admit the canonical copy BEFORE clone; the
                                // VM separately admits its owned conversion.
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
                self.input
                    .meter
                    .event(GlaExecutionEvent::ResultRow)
                    .map_err(lift)?;
                source
                    .join_output_event(&mut guard, GlaExecutionEvent::ResultRow)
                    .map_err(|error| {
                        GqlQueryError::Source(ScanError::Edge(EdgeScanError::Source(error)))
                    })?;
                self.input.meter.rows.result_rows = next;
                return Ok(Some(AsyncEdgeJoinOutput {
                    row: projected,
                    guard,
                }));
            }
            // The complete private occurrence and its copies drop before the
            // next native advance resumes/pops the traversal's final binding.
        }
    }
}

impl<S: AsyncEdgeJoinSource, F> core::fmt::Debug for AsyncEdgeJoinProjectionCursor<S, F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncEdgeJoinProjectionCursor")
            .field("input", &self.input)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

fn lift<E, C>(error: GqlQueryError<EdgeScanError<E>, C>) -> GqlQueryError<ScanError<E>, C> {
    error.map_source(ScanError::Edge)
}

fn flatten_read<T, E, C>(
    result: Result<T, EdgeScanSourceError<E, GqlQueryError<ScanError<E>, C>>>,
) -> ProjectionResult<T, E, C> {
    result.map_err(|error| match error {
        EdgeScanSourceError::Source(error) => {
            GqlQueryError::Source(ScanError::Edge(EdgeScanError::Source(error)))
        }
        EdgeScanSourceError::Control(error) => error,
    })
}
