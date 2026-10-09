//! Checked inputs and shared numeric cells for a host-owned external reducer.
//!
//! This module owns query semantics, not scratch storage or byte admission.
//! The host reserves memory before retaining keys/cells, partitions complete
//! input rows, and evaluates completed groups in canonical key order before
//! ranking and pagination. Every input, reduction and delivery event must use
//! the SAME input cursor's meter.

use crate::algebra::{GlaOperator, GraphValue, GraphValueRow, MAX_PATTERN_VERTICES};
use crate::edge_stream::EdgeScanBuildError;
use crate::scan_stream::ScanKind;
use crate::stream::VertexScanBuildError;
use crate::stream::VertexScanEvent;
use crate::stream::aggregate::{Input, NumericState};
use crate::{
    GlaExecutionEvent, GqlQueryError, GraphAggregateError, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateRow, PreparedGraphAggregate,
};
use fgdb_types::CanonicalScalar;
use std::sync::Arc;

pub use crate::edge_stream::aggregate::{EdgeSpillAggregateCursor, EdgeSpillAggregatePlan};
pub use crate::edge_stream::{AsyncEdgeSpillAggregateCursor, AsyncEdgeSpillAggregatePlan};
pub use crate::stream::aggregate::{VertexSpillAggregateCursor, VertexSpillAggregatePlan};
pub use crate::stream::{AsyncVertexSpillAggregateCursor, AsyncVertexSpillAggregatePlan};

mod join;
pub use join::{AsyncEdgeJoinSpillAggregateCursor, AsyncEdgeJoinSpillAggregatePlan};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillAggregateBuildError {
    Unsupported,
    Vertex(VertexScanBuildError),
    Edge(EdgeScanBuildError),
}
impl core::fmt::Display for SpillAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported => {
                f.write_str("external aggregation requires COUNT/SUM/AVG (including DISTINCT arguments) or MIN/MAX, without collection")
            }
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for SpillAggregateBuildError {}

/// A source shape selected before opening a snapshot or reading input.
#[derive(Clone, Debug)]
pub enum SpillAggregatePlan {
    Vertex(VertexSpillAggregatePlan),
    Edge(EdgeSpillAggregatePlan),
}
impl SpillAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        let definition = SpillAggregateDefinition::compile(aggregate)?;
        if matches!(
            aggregate.input_pattern().plan().operators().first(),
            Some(GlaOperator::ScanEdges { .. })
        ) {
            EdgeSpillAggregatePlan::compile(definition)
                .map(Self::Edge)
                .map_err(SpillAggregateBuildError::Edge)
        } else {
            VertexSpillAggregatePlan::compile(definition)
                .map(Self::Vertex)
                .map_err(SpillAggregateBuildError::Vertex)
        }
    }

    pub fn definition(&self) -> &SpillAggregateDefinition {
        match self {
            Self::Vertex(plan) => plan.definition(),
            Self::Edge(plan) => plan.definition(),
        }
    }

    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Vertex(_) => ScanKind::Vertex,
            Self::Edge(_) => ScanKind::Edge,
        }
    }
}

/// A complete aggregate input admitted for asynchronous local source access.
/// The ordinary numeric definition and row evaluators own every semantic step.
/// Fixed-hop edge expansions use the native awaitable join driver. Probes,
/// OPTIONAL/variable-length expansion and relational input refuse before the
/// host opens storage. This is a physical source plan, not a result or authority.
#[derive(Clone, Debug)]
pub enum AsyncSpillAggregatePlan {
    Vertex(AsyncVertexSpillAggregatePlan),
    Edge(AsyncEdgeSpillAggregatePlan),
    Join(AsyncEdgeJoinSpillAggregatePlan),
}

impl AsyncSpillAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        let definition = SpillAggregateDefinition::compile(aggregate)?;
        if matches!(
            aggregate.input_pattern().plan().operators().first(),
            Some(GlaOperator::ScanEdges { .. })
        ) {
            // Select the access contract from the bound program, never by
            // retrying a rejected compiler or a failed storage operation.
            if aggregate.input_pattern().plan().operators().iter()
                .any(|operator| matches!(operator, GlaOperator::Expand { .. }))
            {
                AsyncEdgeJoinSpillAggregatePlan::from_definition(definition)
                    .map(Self::Join)
                    .map_err(SpillAggregateBuildError::Edge)
            } else {
                AsyncEdgeSpillAggregatePlan::compile(definition)
                    .map(Self::Edge)
                    .map_err(SpillAggregateBuildError::Edge)
            }
        } else {
            AsyncVertexSpillAggregatePlan::compile(definition)
                .map(Self::Vertex)
                .map_err(SpillAggregateBuildError::Vertex)
        }
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        match self {
            Self::Vertex(plan) => plan.definition(),
            Self::Edge(plan) => plan.definition(),
            Self::Join(plan) => plan.definition(),
        }
    }
    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Vertex(_) => ScanKind::Vertex,
            Self::Edge(_) | Self::Join(_) => ScanKind::Edge,
        }
    }
}

/// Sealed numeric aggregate definition, including completed-group clauses.
/// Clones share immutable compiler metadata; no source predicate is rewritten.
#[derive(Clone)]
pub struct SpillAggregateDefinition {
    pub(crate) aggregate: Arc<PreparedGraphAggregate>,
}
impl core::fmt::Debug for SpillAggregateDefinition {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillAggregateDefinition")
            .field("input_width", &self.input_width())
            .field("key_columns", &self.key_columns().len())
            .field("aggregate_columns", &self.aggregate_columns().len())
            .finish_non_exhaustive()
    }
}

/// One group's existing exact cells. A host must admit storage before creating
/// this state and before updating an extremum with an owned input payload.
pub struct SpillAggregateState {
    definition: SpillAggregateDefinition,
    cells: Vec<NumericState>,
}
impl core::fmt::Debug for SpillAggregateState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillAggregateState")
            .field("cells", &self.cells.len())
            .finish_non_exhaustive()
    }
}

impl SpillAggregateDefinition {
    fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        if aggregate.input_pattern().columns().len() > MAX_PATTERN_VERTICES
            || aggregate
                .input_projection()
                .is_some_and(|projection| projection.len() > MAX_PATTERN_VERTICES)
            || aggregate
                .evaluation_key_columns()
                .len()
                .saturating_add(aggregate.evaluation_aggregate_columns().len())
                > MAX_PATTERN_VERTICES
            || !aggregate.aggregates().iter().all(|spec| {
                matches!(
                    spec.function(),
                    GraphAggregateFunction::CountRows
                        | GraphAggregateFunction::Count
                        | GraphAggregateFunction::CountDistinct
                        | GraphAggregateFunction::SumInt
                        | GraphAggregateFunction::SumIntDistinct
                        | GraphAggregateFunction::AverageInt
                        | GraphAggregateFunction::AverageIntDistinct
                        | GraphAggregateFunction::Min
                        | GraphAggregateFunction::Max
                )
            })
        {
            return Err(SpillAggregateBuildError::Unsupported);
        }
        let aggregate = aggregate
            .prepare_streamed_output()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        Ok(Self {
            aggregate: Arc::new(aggregate),
        })
    }

    pub fn input_width(&self) -> usize {
        self.aggregate.input_projection().map_or_else(
            || self.aggregate.input_pattern().columns().len(),
            |projection| projection.len(),
        )
    }
    pub fn group_key_columns(&self) -> &[usize] {
        self.aggregate.group_key_columns()
    }
    pub fn key_columns(&self) -> &[String] {
        self.aggregate.key_columns()
    }
    pub fn aggregate_columns(&self) -> &[String] {
        self.aggregate.aggregate_columns()
    }

    /// Full private schemas. Hidden keys and clause-only summaries remain in
    /// spill frames until ranking finishes; output schemas never identify them.
    pub fn evaluation_key_columns(&self) -> &[String] {
        self.aggregate.evaluation_key_columns()
    }
    pub fn evaluation_aggregate_columns(&self) -> &[String] {
        self.aggregate.evaluation_aggregate_columns()
    }
    pub fn has_output_stage(&self) -> bool {
        self.aggregate.has_streamed_output_stage()
    }
    pub fn has_computed_output(&self) -> bool {
        self.aggregate.output_projection().is_some()
    }
    pub fn has_distinct_output(&self) -> bool {
        self.aggregate.incremental_output_is_distinct()
    }
    /// DISTINCT observes the final visible tuple before ranking or pagination,
    /// even when that tuple only hides or repeats ordinary group columns.
    pub fn has_precomputed_output(&self) -> bool {
        self.has_computed_output() || self.has_distinct_output()
    }
    pub fn ordering(&self) -> &[GraphAggregateOrder] {
        self.aggregate.ordering()
    }
    pub fn result_window(&self) -> (u64, Option<u64>) {
        self.aggregate.incremental_result_window()
    }

    /// Conservative number of copies of any input payload in late projection.
    /// Numeric summaries are copied once; key projection may repeat keys.
    /// Computed outputs instead require their evaluator's per-allocation byte
    /// admission before the host retains the projected scratch envelope.
    pub fn output_payload_copies(&self) -> usize {
        self.key_columns().len().max(1)
    }

    fn check_complete<E, C>(
        &self,
        row: &GraphAggregateRow,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        if row.keys().len() != self.evaluation_key_columns().len()
            || row.values().len() != self.evaluation_aggregate_columns().len()
        {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        Ok(())
    }

    /// Evaluate all HAVING operands on one complete group, even with LIMIT 0.
    /// The host visits groups in canonical key order before any rank/window.
    pub fn qualifies_output<E, C>(
        &self,
        row: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<bool, GqlQueryError<GraphAggregateError<E>, C>> {
        self.check_complete(row)?;
        self.aggregate
            .qualifies_streamed_output(row, &mut |event| control(result_event(event)))
    }

    /// Apply the checked visible projection to a HAVING-qualified group. A host
    /// with computed outputs MUST call this for every qualified group before
    /// ranking or pagination, and retain the projected result without running
    /// its expressions again. Full rows with no transform move without copies.
    /// This neither evaluates HAVING again nor charges a delivered result row.
    pub fn project_output<E, C>(
        &self,
        row: GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<GraphAggregateRow, GqlQueryError<GraphAggregateError<E>, C>> {
        self.check_complete(&row)?;
        if self.aggregate.transforms_streamed_columns() {
            self.aggregate
                .project_complete_output(&row, &mut |event| control(result_event(event)))
        } else {
            Ok(row)
        }
    }

    /// The SAME exact comparator used by completed in-memory groups. Unsigned
    /// counts, wide signed sums and rational averages are never narrowed or
    /// compared by their serialization tags. Full ascending keys break ties.
    pub fn compare_output<E, C>(
        &self,
        left: &GraphAggregateRow,
        right: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::cmp::Ordering, GqlQueryError<GraphAggregateError<E>, C>> {
        self.check_complete(left)?;
        self.check_complete(right)?;
        left.compare_incremental_order(right, self.ordering(), &mut |event| {
            control(result_event(event))
        })?
        .ok_or(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput,
        ))
    }

    /// Compare only projected visible cells for output DISTINCT. Equal values
    /// retain their original representation; compare_output() separately picks
    /// the first fully ranked group as each equivalence class's representative.
    pub fn compare_projected_output<E, C>(
        &self,
        left: &GraphAggregateRow,
        right: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::cmp::Ordering, GqlQueryError<GraphAggregateError<E>, C>> {
        if left.keys().len() != self.key_columns().len()
            || left.values().len() != self.aggregate_columns().len()
            || right.keys().len() != self.key_columns().len()
            || right.values().len() != self.aggregate_columns().len()
        {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        left.compare_incremental_distinct(right, &mut |event| control(result_event(event)))?
            .ok_or(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ))
    }

    /// Cells that may retain one variable-size argument value. COUNT/SUM/AVG
    /// own only the fixed storage covered by state_resident_bytes().
    pub fn extremum_count(&self) -> usize {
        self.aggregate
            .aggregates()
            .iter()
            .filter(|spec| {
                matches!(
                    spec.function(),
                    GraphAggregateFunction::Min | GraphAggregateFunction::Max
                )
            })
            .count()
    }

    /// Fixed retained cell storage, including possible binary64 promotion.
    /// The host separately reserves keys, variable extremum payloads, allocator
    /// overhead and transient decoded/projected rows before taking ownership.
    pub fn state_resident_bytes(&self) -> usize {
        core::mem::size_of::<SpillAggregateState>()
            .saturating_add(
                self.aggregate
                    .aggregates()
                    .len()
                    .saturating_mul(core::mem::size_of::<NumericState>()),
            )
            .saturating_add(
                self.aggregate
                    .aggregates()
                    .iter()
                    .filter(|spec| {
                        matches!(
                            spec.function(),
                            GraphAggregateFunction::SumInt
                                | GraphAggregateFunction::SumIntDistinct
                                | GraphAggregateFunction::AverageInt
                                | GraphAggregateFunction::AverageIntDistinct
                        )
                    })
                    .count()
                    .saturating_mul(core::mem::size_of::<fgdb_types::ExactBinary64Sum>()),
            )
    }

    /// Validate every numeric argument while the original source is drained.
    /// Partition order must never select a different first domain error.
    pub fn validate_input<E, C>(
        &self,
        row: &GraphValueRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        if row.values().len() != self.input_width() {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        for (aggregate, spec) in self.aggregate.aggregates().iter().enumerate() {
            control(VertexScanEvent::Work)?;
            let error = match spec.function() {
                GraphAggregateFunction::SumInt | GraphAggregateFunction::SumIntDistinct => {
                    GraphAggregateError::NonIntegerSum { aggregate }
                }
                GraphAggregateFunction::AverageInt | GraphAggregateFunction::AverageIntDistinct => {
                    GraphAggregateError::NonIntegerAverage { aggregate }
                }
                _ => continue,
            };
            let Some(column) = spec.argument_column() else {
                return Err(GqlQueryError::Source(
                    GraphAggregateError::InvalidReductionInput,
                ));
            };
            if !matches!(
                &row.values()[column],
                GraphValue::Scalar(
                    CanonicalScalar::Null | CanonicalScalar::Int(_) | CanonicalScalar::Float(_)
                )
            ) {
                return Err(GqlQueryError::Source(error));
            }
        }
        Ok(())
    }

    pub fn new_state<E, C>(
        &self,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<SpillAggregateState, GqlQueryError<GraphAggregateError<E>, C>> {
        control(VertexScanEvent::ScratchEntry)?;
        for _ in self.aggregate.aggregates() {
            control(VertexScanEvent::ScratchEntry)?;
        }
        let mut cells = Vec::with_capacity(self.aggregate.aggregates().len());
        for spec in self.aggregate.aggregates() {
            // The external host proves uniqueness. Retaining native DISTINCT
            // support sets here would make one large group unbounded again.
            cells.push(NumericState::new_governed(
                plain_function(spec.function()),
                control,
            )?);
        }
        Ok(SpillAggregateState {
            definition: self.clone(),
            cells,
        })
    }

    /// Add every occurrence to ordinary cells. DISTINCT cells are populated
    /// separately by update_distinct_argument after external canonical dedup.
    pub fn update<E, C>(
        &self,
        state: &mut SpillAggregateState,
        row: &GraphValueRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        if !Arc::ptr_eq(&self.aggregate, &state.definition.aggregate) {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        self.validate_input(row, control)?;
        for (aggregate, (spec, cell)) in self
            .aggregate
            .aggregates()
            .iter()
            .zip(&mut state.cells)
            .enumerate()
        {
            control(VertexScanEvent::Work)?;
            if distinct_argument(spec.function()) {
                continue;
            }
            let input = spec.argument_column().map_or(Input::Identity, |column| {
                Input::from_value(&row.values()[column])
            });
            cell.update_governed(input, aggregate, control)?;
        }
        Ok(())
    }

    /// Input columns requiring one external uniqueness pass each. Functions
    /// sharing an argument share a pass; hidden HAVING/order cells participate.
    pub fn distinct_argument_columns(&self) -> impl Iterator<Item = usize> + '_ {
        self.aggregate
            .aggregates()
            .iter()
            .enumerate()
            .filter_map(|(at, spec)| {
                if !distinct_argument(spec.function()) {
                    return None;
                }
                let column = spec.argument_column()?;
                (!self.aggregate.aggregates()[..at].iter().any(|previous| {
                    distinct_argument(previous.function())
                        && previous.argument_column() == Some(column)
                }))
                .then_some(column)
            })
    }

    /// Add one typed (group key, argument) equivalence class to every DISTINCT
    /// cell using this column. The host must first externally sort/deduplicate
    /// those classes. NULL is ignored by the same native plain numeric kernel.
    /// No canonical value or resident membership set is retained here.
    pub fn update_distinct_argument<E, C>(
        &self,
        state: &mut SpillAggregateState,
        row: &GraphValueRow,
        column: usize,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        if !Arc::ptr_eq(&self.aggregate, &state.definition.aggregate)
            || !self
                .distinct_argument_columns()
                .any(|argument| argument == column)
        {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        self.validate_input(row, control)?;
        for (aggregate, (spec, cell)) in self
            .aggregate
            .aggregates()
            .iter()
            .zip(&mut state.cells)
            .enumerate()
        {
            control(VertexScanEvent::Work)?;
            if distinct_argument(spec.function()) && spec.argument_column() == Some(column) {
                cell.update_governed(Input::from_value(&row.values()[column]), aggregate, control)?;
            }
        }
        Ok(())
    }

    pub fn finish<E, C>(
        &self,
        keys: Vec<GraphValue>,
        state: SpillAggregateState,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<GraphAggregateRow, GqlQueryError<GraphAggregateError<E>, C>> {
        if keys.len() != self.group_key_columns().len()
            || !Arc::ptr_eq(&self.aggregate, &state.definition.aggregate)
        {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        control(VertexScanEvent::ScratchEntry)?;
        for _ in &state.cells {
            control(VertexScanEvent::ScratchEntry)?;
        }
        let mut values = Vec::with_capacity(state.cells.len());
        for cell in state.cells {
            values.push(cell.finish_governed(control)?);
        }
        Ok(GraphAggregateRow::from_group_values(keys, values))
    }
}

fn distinct_argument(function: GraphAggregateFunction) -> bool {
    matches!(
        function,
        GraphAggregateFunction::CountDistinct
            | GraphAggregateFunction::SumIntDistinct
            | GraphAggregateFunction::AverageIntDistinct
    )
}

fn plain_function(function: GraphAggregateFunction) -> GraphAggregateFunction {
    match function {
        GraphAggregateFunction::CountDistinct => GraphAggregateFunction::Count,
        GraphAggregateFunction::SumIntDistinct => GraphAggregateFunction::SumInt,
        GraphAggregateFunction::AverageIntDistinct => GraphAggregateFunction::AverageInt,
        other => other,
    }
}

fn result_event(event: GlaExecutionEvent) -> VertexScanEvent {
    match event {
        GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
        // ResultRows belong to the external host's final selected-row stage.
        GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => VertexScanEvent::Work,
    }
}
