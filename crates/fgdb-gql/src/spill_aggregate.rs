//! Checked inputs and shared numeric cells for a host-owned external reducer.
//!
//! This module owns query semantics, not scratch storage or byte admission.
//! The host reserves memory before retaining keys/cells, partitions complete
//! input rows, and evaluates completed groups in canonical key order before
//! ranking and pagination. Every input, reduction and delivery event must use
//! the SAME input cursor's meter.

use crate::algebra::{GlaOperator, GraphValue, GraphValueRow, MAX_PATTERN_VERTICES};
use crate::edge_stream::EdgeScanBuildError;
use crate::edge_stream::aggregate::pipeline::InputPipeline;
use crate::scan_stream::ScanKind;
use crate::spill_set::{AsyncSpillSetPlan, SpillSetBuildError};
use crate::stream::VertexScanBuildError;
use crate::stream::VertexScanEvent;
use crate::stream::aggregate::{Input, NumericState};
use crate::{
    GlaExecutionEvent, GqlQueryError, GraphAggregateError, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateRow, PreparedGraphAggregate, PreparedGraphSetAggregate,
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
    Relation(SpillSetBuildError),
}
impl core::fmt::Display for SpillAggregateBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported => {
                f.write_str("external aggregation requires COUNT/SUM/AVG, MIN/MAX, or COLLECT over a complete relational input")
            }
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
            Self::Relation(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for SpillAggregateBuildError {}

/// A source shape selected before opening a snapshot or reading input.
/// Single-source ALL projection/filter WITH chains run once per binding before
/// partitioning. Intermediate DISTINCT, ordering, windows, expansion and other
/// non-row-local relational operators still refuse; the graph leaf is checked
/// separately. Numeric group/DISTINCT support uses the existing external host,
/// not an intermediate input table. Source and scalar-VM transient allocations
/// retain their existing admission contracts; this is not a whole-query RSS cap.
#[derive(Clone, Debug)]
pub enum SpillAggregatePlan {
    Vertex(VertexSpillAggregatePlan),
    Edge(EdgeSpillAggregatePlan),
}
impl SpillAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        let definition = SpillAggregateDefinition::compile_row_local(aggregate)?;
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
/// Fixed-hop edge expansions and correlated fixed-hop/copy-only probes use the
/// native awaitable join driver. Independent/nested probes and OPTIONAL or
/// variable-length expansion refuse before the host opens storage.
/// Relational inputs retain every projection, filter, UNWIND, binary set,
/// DISTINCT, ordering and window barrier before reduction. Values inputs use the
/// native singleton without a graph source. Direct graph collections also use
/// the complete relation owner, after native occurrence-order normalization;
/// their graph rows are never reduced in a direct cursor's physical order.
/// This plan contains no result or authority.
#[derive(Clone, Debug)]
pub enum AsyncSpillAggregatePlan {
    Vertex(AsyncVertexSpillAggregatePlan),
    Edge(AsyncEdgeSpillAggregatePlan),
    Join(AsyncEdgeJoinSpillAggregatePlan),
    Relation {
        input: Box<AsyncSpillSetPlan>,
        definition: SpillAggregateDefinition,
    },
}

impl AsyncSpillAggregatePlan {
    pub fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        Self::compile_sources(aggregate, false)
    }

    /// Compile for the buffered host's additional bounded path source. Walk
    /// occurrences complete through the ordinary external relation before
    /// exact reduction; the physical traversal order never becomes COLLECT's
    /// order. Unsupported walk definitions still refuse before source access.
    pub fn compile_with_bounded_walks(
        aggregate: &PreparedGraphAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        Self::compile_sources(aggregate, true)
    }

    fn compile_sources(
        aggregate: &PreparedGraphAggregate,
        bounded_walks: bool,
    ) -> Result<Self, SpillAggregateBuildError> {
        if aggregate.input_relation().is_none()
            && aggregate.aggregates().iter().any(|spec| {
                matches!(
                    spec.function(),
                    GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
                )
            })
        {
            // Native graph aggregation does not generally visit bindings in
            // aggregate-argument order. Reuse the proved normalization that
            // carries its original occurrence order as private leading cells.
            // The complete ordered relation then owns source admission and
            // every computed expression before the reducer sees any rows.
            // Selecting a direct cursor, or sorting only the collected value,
            // would change COLLECT and first-occurrence COLLECT DISTINCT.
            let relation = aggregate
                .incremental_collection_relation()
                .ok_or(SpillAggregateBuildError::Unsupported)?;
            return Self::compile_relation_sources(&relation, bounded_walks);
        }
        if let Some(relation) = aggregate.input_relation() {
            // The complete relation, never just its graph leaf, owns the rows
            // summarized by this definition. Admit both halves before I/O.
            let input = if bounded_walks {
                AsyncSpillSetPlan::compile_with_bounded_walks(relation)
            } else {
                AsyncSpillSetPlan::compile(relation)
            }
            .map_err(SpillAggregateBuildError::Relation)?;
            let physical = aggregate
                .prepare_complete_group_output()
                .ok_or(SpillAggregateBuildError::Unsupported)?;
            let definition = SpillAggregateDefinition::from_physical(physical)?;
            return Ok(Self::Relation {
                input: Box::new(input),
                definition,
            });
        }
        if bounded_walks
            && matches!(
                aggregate.input_pattern().plan().operators().first(),
                Some(GlaOperator::ScanVertices)
            )
            && aggregate
                .input_pattern()
                .plan()
                .operators()
                .iter()
                .any(|operator| matches!(operator, GlaOperator::VarLengthExpand { .. }))
        {
            let relation = aggregate
                .complete_group_relation()
                .ok_or(SpillAggregateBuildError::Unsupported)?;
            return Self::compile_relation_sources(&relation, true);
        }
        let definition = SpillAggregateDefinition::compile(aggregate)?;
        if matches!(
            aggregate.input_pattern().plan().operators().first(),
            Some(GlaOperator::ScanEdges { .. })
        ) {
            // Select the access contract from the bound program, never by
            // retrying a rejected compiler or a failed storage operation.
            // A copy-only probe needs the join driver despite having no Expand.
            if aggregate
                .input_pattern()
                .plan()
                .operators()
                .iter()
                .any(|operator| {
                    matches!(
                        operator,
                        GlaOperator::Expand { .. } | GlaOperator::Probe { .. }
                    )
                })
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

    /// Compile the complete source-aware relation and its native numeric group
    /// definition. Zero-source Values/UNWIND pipelines require this owner;
    /// graph-backed and binary inputs retain the same stages and result clauses.
    /// Every descendant is admitted before I/O, including beneath LIMIT zero.
    /// COLLECT and COLLECT DISTINCT retain the complete relation's occurrence
    /// order. Each collected value must fit the host's memory and row limits;
    /// the host must admit every native allocation event before retaining it.
    /// The private summary never becomes an executable single-graph source.
    pub fn compile_relation(
        aggregate: &PreparedGraphSetAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        Self::compile_relation_sources(aggregate, false)
    }

    /// Source-aware counterpart of `compile_with_bounded_walks`; every child
    /// barrier and collection occurrence law remains owned by the relation.
    pub fn compile_relation_with_bounded_walks(
        aggregate: &PreparedGraphSetAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        Self::compile_relation_sources(aggregate, true)
    }

    fn compile_relation_sources(
        aggregate: &PreparedGraphSetAggregate,
        bounded_walks: bool,
    ) -> Result<Self, SpillAggregateBuildError> {
        let input = if bounded_walks {
            AsyncSpillSetPlan::compile_with_bounded_walks(aggregate.input())
        } else {
            AsyncSpillSetPlan::compile(aggregate.input())
        }
        .map_err(SpillAggregateBuildError::Relation)?;
        let definition = SpillAggregateDefinition::from_relation(aggregate)?;
        Ok(Self::Relation {
            input: Box::new(input),
            definition,
        })
    }

    /// Admit a terminal summary with exactly one procedure in its complete
    /// relational input. The procedure cannot occur inside any Apply body.
    /// Ordinary compilation still refuses procedures; a graph-only definition
    /// or a relation without a procedure also refuses this explicit capability.
    ///
    /// The host must admit the original procedure shape before opening storage
    /// and evaluate arguments only when that source is reached. Every input
    /// barrier and completed-group clause remains native. This returns exact
    /// aggregate cells, including wide sums and rational averages, without
    /// converting them into the ordinary relational row domain.
    pub fn compile_with_single_procedure(
        aggregate: &PreparedGraphAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        let relation = aggregate
            .input_relation()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        let input = AsyncSpillSetPlan::compile_with_single_procedure(relation)
            .map_err(SpillAggregateBuildError::Relation)?;
        let physical = aggregate
            .prepare_complete_group_output()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        let definition = SpillAggregateDefinition::from_physical(physical)?;
        Ok(Self::Relation {
            input: Box::new(input),
            definition,
        })
    }

    /// Admit the source-aware terminal aggregate owner with exactly one
    /// non-correlated procedure. The same complete-input and native value
    /// contracts as `compile_with_single_procedure` apply. COLLECT and COLLECT
    /// DISTINCT retain the input's occurrence order and existing value bounds;
    /// each collection still needs the host's per-allocation memory admission.
    pub fn compile_relation_with_single_procedure(
        aggregate: &PreparedGraphSetAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        let input = AsyncSpillSetPlan::compile_with_single_procedure(aggregate.input())
            .map_err(SpillAggregateBuildError::Relation)?;
        let definition = SpillAggregateDefinition::from_relation(aggregate)?;
        Ok(Self::Relation {
            input: Box::new(input),
            definition,
        })
    }

    /// Admit a terminal summary over one or more independent procedure
    /// sources. Every procedure remains outside Apply bodies and executes in
    /// original child order. The host must admit all registered source shapes
    /// before I/O and share cumulative allowances across every input and the
    /// terminal reducer. Arguments stay lazy at their reached sources.
    ///
    /// Native completed-group clauses and exact aggregate domains are the
    /// same as `compile_with_single_procedure`; ordinary and single-source
    /// compilation retain their existing refusal boundaries.
    pub fn compile_with_independent_procedures(
        aggregate: &PreparedGraphAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        let relation = aggregate
            .input_relation()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        let input = AsyncSpillSetPlan::compile_with_independent_procedures(relation)
            .map_err(SpillAggregateBuildError::Relation)?;
        let physical = aggregate
            .prepare_complete_group_output()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        let definition = SpillAggregateDefinition::from_physical(physical)?;
        Ok(Self::Relation {
            input: Box::new(input),
            definition,
        })
    }

    /// Admit the complete source-aware aggregate input with independent
    /// procedure sources. All-call host admission and cumulative accounting
    /// match `compile_with_independent_procedures`. COLLECT keeps native input
    /// occurrence order and every existing per-value memory and row bound.
    pub fn compile_relation_with_independent_procedures(
        aggregate: &PreparedGraphSetAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        let input = AsyncSpillSetPlan::compile_with_independent_procedures(aggregate.input())
            .map_err(SpillAggregateBuildError::Relation)?;
        let definition = SpillAggregateDefinition::from_relation(aggregate)?;
        Ok(Self::Relation {
            input: Box::new(input),
            definition,
        })
    }

    pub fn definition(&self) -> &SpillAggregateDefinition {
        match self {
            Self::Vertex(plan) => plan.definition(),
            Self::Edge(plan) => plan.definition(),
            Self::Join(plan) => plan.definition(),
            Self::Relation { definition, .. } => definition,
        }
    }
    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Vertex(_) => ScanKind::Vertex,
            Self::Edge(_) | Self::Join(_) => ScanKind::Edge,
            Self::Relation { input, .. } => input.source().kind(),
        }
    }
}

/// Sealed aggregate definition, including completed-group clauses.
/// Clones share immutable compiler metadata; no source predicate is rewritten.
#[derive(Clone)]
pub struct SpillAggregateDefinition {
    pub(crate) aggregate: Arc<PreparedGraphAggregate>,
    // Address the stored projected row through the existing exact comparator.
    // Lower once at preparation, never allocate sort metadata per comparison.
    projected_ordering: Arc<[GraphAggregateOrder]>,
    // Only the synchronous source owner installs this program. The async
    // relational owner already produces complete rows and MUST NOT replay it.
    // Shared metadata, never a retained match bag or per-group support set.
    input_pipeline: Option<Arc<InputPipeline>>,
    collections: bool,
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
/// Collection profiles additionally admit every allocation event and retain
/// that credit through finalization; fixed state bytes cannot cover a list.
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
    /// Share complete-group admission between terminal summaries and an
    /// intermediate relational aggregate. Each caller separately compiles the
    /// entire real input; this method never creates or admits a graph source.
    pub(crate) fn from_relation(
        aggregate: &PreparedGraphSetAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        let physical = aggregate
            .prepare_spill_output()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        Self::from_physical(physical)
    }

    fn compile(aggregate: &PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        let physical = aggregate
            .prepare_streamed_output()
            .ok_or(SpillAggregateBuildError::Unsupported)?;
        Self::from_physical(physical)
    }

    fn compile_row_local(
        aggregate: &PreparedGraphAggregate,
    ) -> Result<Self, SpillAggregateBuildError> {
        let (physical, pipeline) =
            InputPipeline::prepare(aggregate).ok_or(SpillAggregateBuildError::Unsupported)?;
        let mut definition = Self::from_physical(physical)?;
        definition.input_pipeline = pipeline.map(Arc::new);
        Ok(definition)
    }

    /// Source-only transformation. The host validates and partitions its final
    /// row, and all later reduction/DISTINCT/output passes consume that row
    /// unchanged. Never call this when replaying an external partition or when
    /// a complete relational input owner has already evaluated these stages.
    /// A filtered binding is not EOF; the source adapter must continue scanning.
    pub(crate) fn evaluate_input<E, C>(
        &self,
        row: GraphValueRow,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Option<GraphValueRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        match &self.input_pipeline {
            Some(pipeline) => pipeline.evaluate(row, control),
            None => self
                .aggregate
                .evaluate_streamed_input(row, control)
                .map(Some),
        }
    }

    fn from_physical(aggregate: PreparedGraphAggregate) -> Result<Self, SpillAggregateBuildError> {
        // Relational owners derive their schema from the real completed input,
        // including zero-column Values. Graph admission metadata is irrelevant
        // to this branch and must never manufacture a source or input width.
        let input_width = aggregate.input_relation().map_or_else(
            || aggregate.input_pattern().columns().len(),
            |relation| relation.columns().len(),
        );
        let collections = aggregate.aggregates().iter().any(|spec| {
            matches!(
                spec.function(),
                GraphAggregateFunction::Collect | GraphAggregateFunction::CollectDistinct
            )
        });
        // Direct graph cursors have separate visitation-order proofs. Only a
        // complete relational owner establishes the order this profile needs;
        // do not silently sort collected values or select a graph-only leaf.
        if aggregate.computed_order_requires_element_reads()
            || (collections && aggregate.input_relation().is_none())
            || input_width > MAX_PATTERN_VERTICES
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
                        | GraphAggregateFunction::Collect
                        | GraphAggregateFunction::CollectDistinct
                )
            })
        {
            return Err(SpillAggregateBuildError::Unsupported);
        }
        let projected_ordering = aggregate
            .projected_ordering()
            .iter()
            .map(|order| GraphAggregateOrder {
                column: crate::GraphAggregateColumn::Aggregate(order.column),
                descending: order.descending,
                nulls: if order.nulls_first {
                    crate::GraphNullPlacement::First
                } else {
                    crate::GraphNullPlacement::Last
                },
            })
            .collect();
        Ok(Self {
            aggregate: Arc::new(aggregate),
            projected_ordering,
            input_pipeline: None,
            collections,
        })
    }

    /// Variable-size collection cells require allocation-event admission in
    /// addition to state_resident_bytes(). The same guard must cover retained
    /// membership, collected payloads, finalization and envelope-copy overlap.
    pub fn has_collections(&self) -> bool {
        self.collections
    }

    pub fn input_width(&self) -> usize {
        if let Some(relation) = self.aggregate.input_relation() {
            return relation.columns().len();
        }
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
    /// Evaluation-column ordering only. Use has_ordering() to decide whether
    /// an external rank pass is required, including projected-result ordering.
    pub fn ordering(&self) -> &[GraphAggregateOrder] {
        self.aggregate.ordering()
    }
    pub fn has_ordering(&self) -> bool {
        !self.ordering().is_empty() || self.has_projected_ordering()
    }
    /// Requires the already-evaluated visible row alongside its complete group.
    /// compare_output() deliberately refuses this profile; use
    /// compare_precomputed_output() without re-evaluating output expressions.
    pub fn has_projected_ordering(&self) -> bool {
        !self.projected_ordering.is_empty()
    }
    pub fn computed_rank_width(&self) -> usize {
        self.aggregate.computed_ordering().len()
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

    fn check_projected<E, C>(
        &self,
        row: &GraphAggregateRow,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        if row.keys().len() != self.key_columns().len()
            || row.values().len() != self.aggregate_columns().len()
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

    /// Evaluate visible output and private rank cells once, before pagination.
    /// The host retains them as separate scratch rows so DISTINCT and delivery
    /// continue to observe only the visible tuple.
    pub fn project_output_with_rank<E, C>(
        &self,
        row: GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<
        (GraphAggregateRow, Option<GraphAggregateRow>),
        GqlQueryError<GraphAggregateError<E>, C>,
    > {
        if self.computed_rank_width() == 0 {
            return self.project_output(row, control).map(|row| (row, None));
        }
        self.check_complete(&row)?;
        let mut govern = |event| control(result_event(event));
        let projected = self.aggregate.project_complete_output(&row, &mut govern)?;
        let rank = self.aggregate.project_complete_order_with_elements(
            &row,
            &projected,
            &mut crate::set_ops::no_element_reads,
            &mut govern,
        )?;
        Ok((projected, Some(rank)))
    }

    /// The SAME exact comparator used by completed in-memory groups. Unsigned
    /// counts, wide signed sums and rational averages are never narrowed or
    /// compared by their serialization tags. Full ascending keys break ties.
    /// A projected-order definition requires compare_precomputed_output();
    /// complete cells alone cannot determine its rank and are refused.
    pub fn compare_output<E, C>(
        &self,
        left: &GraphAggregateRow,
        right: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::cmp::Ordering, GqlQueryError<GraphAggregateError<E>, C>> {
        if self.has_projected_ordering() {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        self.check_complete(left)?;
        self.check_complete(right)?;
        left.compare_incremental_order(right, self.ordering(), &mut |event| {
            control(result_event(event))
        })?
        .ok_or(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput,
        ))
    }

    /// Rank retained output cells, then the complete ascending grouping key.
    /// The host must pair each complete group with the output it evaluated
    /// once after HAVING. This checks all four schemas before any decisive
    /// comparison; it does not certify provenance or re-evaluate expressions.
    /// Both domains use the existing exact comparator, with no temporary row,
    /// scalar cast, floating approximation or allocation during comparison.
    /// Ordinary evaluation ordering remains unchanged when no projected order
    /// was selected. DISTINCT equality is still compare_projected_output().
    pub fn compare_precomputed_output<E, C>(
        &self,
        left: &GraphAggregateRow,
        left_projected: &GraphAggregateRow,
        right: &GraphAggregateRow,
        right_projected: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::cmp::Ordering, GqlQueryError<GraphAggregateError<E>, C>> {
        if self.computed_rank_width() != 0 {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        self.check_complete(left)?;
        self.check_complete(right)?;
        self.check_projected(left_projected)?;
        self.check_projected(right_projected)?;
        if self.has_projected_ordering() {
            // A computed output has no grouping-key prefix. Thus an equal
            // projected comparison reaches the ORIGINAL complete keys below,
            // never a hidden or noninjective projected key as a tiebreak.
            let order = left_projected
                .compare_incremental_order(
                    right_projected,
                    &self.projected_ordering,
                    &mut |event| control(result_event(event)),
                )?
                .ok_or(GqlQueryError::Source(
                    GraphAggregateError::InvalidReductionInput,
                ))?;
            if order != core::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        left.compare_incremental_order(right, self.ordering(), &mut |event| {
            control(result_event(event))
        })?
        .ok_or(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput,
        ))
    }

    /// Rank an already evaluated private tuple, then complete ascending keys.
    /// Visible output is checked and compared separately for DISTINCT. No
    /// expression runs inside this comparator and all rank schemas are checked
    /// before an early decisive key can hide a malformed later cell.
    pub fn compare_precomputed_rank<E, C>(
        &self,
        left: &GraphAggregateRow,
        left_rank: &GraphAggregateRow,
        right: &GraphAggregateRow,
        right_rank: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::cmp::Ordering, GqlQueryError<GraphAggregateError<E>, C>> {
        self.check_complete(left)?;
        self.check_complete(right)?;
        let width = self.computed_rank_width();
        if width == 0
            || !left_rank.keys().is_empty()
            || !right_rank.keys().is_empty()
            || left_rank.values().len() != width
            || right_rank.values().len() != width
        {
            return Err(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        let rank = left_rank
            .compare_incremental_order(right_rank, &self.projected_ordering, &mut |event| {
                control(result_event(event))
            })?
            .ok_or(GqlQueryError::Source(
                GraphAggregateError::InvalidReductionInput,
            ))?;
        if rank != core::cmp::Ordering::Equal {
            return Ok(rank);
        }
        left.compare_incremental_order(right, self.ordering(), &mut |event| {
            control(result_event(event))
        })?
        .ok_or(GqlQueryError::Source(
            GraphAggregateError::InvalidReductionInput,
        ))
    }

    /// Compare only projected visible cells for output DISTINCT. Equal values
    /// retain their original representation; the rank comparator separately
    /// picks the first fully ranked group as each class's representative.
    pub fn compare_projected_output<E, C>(
        &self,
        left: &GraphAggregateRow,
        right: &GraphAggregateRow,
        control: &mut impl FnMut(
            VertexScanEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<core::cmp::Ordering, GqlQueryError<GraphAggregateError<E>, C>> {
        self.check_projected(left)?;
        self.check_projected(right)?;
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
    /// Collections require additional per-allocation admission, not a bound
    /// derived from the largest individual input row.
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
            // Numeric DISTINCT uses external uniqueness passes. Collection
            // DISTINCT must preserve first occurrence, so it keeps the native
            // membership owner under the host's per-allocation byte admission.
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

    /// Add every occurrence to ordinary and collection-DISTINCT cells. Numeric
    /// DISTINCT cells alone are populated by update_distinct_argument. Sorting
    /// a COLLECT DISTINCT argument here would destroy first-occurrence order.
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

#[cfg(test)]
mod input_tests;
#[cfg(test)]
mod procedure_tests;
