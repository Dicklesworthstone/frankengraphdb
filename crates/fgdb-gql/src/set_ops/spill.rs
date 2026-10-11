//! Physical admission for external relations. Do not fuse across a stage:
//! every child's complete evaluation and selected row order precede its
//! parent's operation, including when a later stage requests LIMIT zero.

pub use super::aggregate::SpillSetAggregateRowError;
pub use super::merge::SpillSetMergeStep;
use super::{
    GraphSetColumnType, GraphSetExecutionError, GraphSetOperation, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, PreparedGraphSet, SetNode, filter::RowPredicate, merge,
    projection,
};
use crate::algebra::{GlaOperator, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use crate::edge_stream::{AsyncEdgeJoinPlan, AsyncEdgeScanPlan, EdgeScanBuildError};
use crate::scan_stream::{ScanKind, ScanSortTail};
use crate::spill_aggregate::SpillAggregateDefinition;
use crate::stream::{AsyncVertexScanPlan, VertexScanBuildError};
use crate::{GlaExecutionEvent, GraphAggregateError, GraphAggregateRow};
use core::convert::Infallible;

/// A refusal found before any graph source or scratch writer is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillSetBuildError {
    /// Scope, selection, projection, UNWIND, numeric grouping and binary set
    /// operations over native graph or singleton leaves are admitted. This
    /// ordinal is the depth from the root, including across binary operands.
    Unsupported {
        depth: usize,
    },
    Vertex(VertexScanBuildError),
    Edge(EdgeScanBuildError),
}
impl core::fmt::Display for SpillSetBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported { depth } => {
                write!(
                    f,
                    "external relation has an unsupported node at depth {depth}"
                )
            }
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for SpillSetBuildError {}

/// The ordinary async sort-input compiler owns all graph predicates, captures
/// and projection. Values owns the native singleton and opens no graph source.
/// Binary owns both complete operand plans and their checked set semantics.
/// Aggregate owns one complete input and the existing exact numeric reducer.
/// This is a fixed source choice, never a fallback strategy.
#[derive(Clone, Debug)]
pub enum AsyncSpillSetSourcePlan {
    Values(SpillSetValuesPlan),
    Vertex(AsyncVertexScanPlan<GraphValueRow>),
    Edge(AsyncEdgeScanPlan),
    Join(AsyncEdgeJoinPlan),
    Binary(SpillSetBinaryPlan),
    Aggregate(SpillSetAggregatePlan),
}
impl AsyncSpillSetSourcePlan {
    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Values(_) => ScanKind::Values,
            Self::Vertex(_) => ScanKind::Vertex,
            Self::Edge(_) | Self::Join(_) => ScanKind::Edge,
            Self::Binary(_) => ScanKind::Set,
            Self::Aggregate(aggregate) => aggregate.input().source().kind(),
        }
    }
}

/// A compiler-owned intermediate numeric aggregate over a complete relation.
///
/// Finish the entire input, including every child grouping, DISTINCT, set
/// operation, order and page, before invoking definition(). The ordinary
/// external reducer owns all numeric and argument-DISTINCT semantics. Visit
/// its completed groups in native canonical key order, then convert EVERY
/// group to the ordinary row domain before starting the enclosing plan's
/// first stage. That Identity stage owns this node's ORDER BY/SKIP/LIMIT;
/// a later LIMIT zero cannot conceal a conversion or child-evaluation error.
///
/// Counts and integer sums require an exact Int64 representation. Exact
/// rational averages refuse even when their denominator is one; floating
/// results, extrema and NULL retain their canonical native values. No numeric
/// coercion, collection reducer or graph source is synthesized here.
#[derive(Clone)]
pub struct SpillSetAggregatePlan {
    input: Box<AsyncSpillSetPlan>,
    definition: SpillAggregateDefinition,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
}

impl core::fmt::Debug for SpillSetAggregatePlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillSetAggregatePlan")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl SpillSetAggregatePlan {
    pub fn input(&self) -> &AsyncSpillSetPlan {
        &self.input
    }
    pub fn definition(&self) -> &SpillAggregateDefinition {
        &self.definition
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    /// The SAME checked conversion as native materialized grouping. The host
    /// retains the authenticated group's reservation, charges its cumulative
    /// query meter and admits memory on ScratchEntry BEFORE this call owns a
    /// value. Retain the new reservation through all uses of the returned row,
    /// including awaited encoding/appends. No final ResultRow is charged.
    ///
    /// A value/control failure invalidates the entire tentative result. Do not
    /// publish a prefix or retry with a different aggregate or scalar domain.
    pub fn convert_row_with_control<E>(
        &self,
        row: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetAggregateRowError<E>> {
        if row.keys().len() != self.definition.key_columns().len()
            || row.values().len() != self.definition.aggregate_columns().len()
        {
            return Err(SpillSetAggregateRowError::Value(
                GraphAggregateError::InvalidReductionInput,
            ));
        }
        super::aggregate::convert_row(row, control)
    }

    /// Complete the native conversion barrier once after every group succeeds,
    /// including when a keyed input has no groups. This final Work checkpoint
    /// precedes every downstream expression, filter, ordering and page.
    pub fn finish_conversion_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), SpillSetAggregateRowError<E>> {
        super::aggregate::finish_conversion(control)
    }
}

/// A compiler-owned binary relation, including both complete child plans.
///
/// Execute the complete left child, then the complete right child, even if
/// either returns no rows or a parent asks for LIMIT zero. Only after both
/// children succeed, sort each completed child by canonical whole-row order
/// (NULL first). DISTINCT additionally deduplicates EACH child before merging;
/// output-only deduplication does not implement EXCEPT DISTINCT. Operand order
/// and pagination belong to the child and must run before this canonical sort.
///
/// The merge emits canonical whole-row order. Its enclosing plan's first stage
/// then applies the binary node's own ORDER BY/SKIP/LIMIT before any parent
/// stage. Column names belong to the left child; position and value domains of
/// both children were checked by the native relational binder.
#[derive(Clone)]
pub struct SpillSetBinaryPlan {
    left: Box<AsyncSpillSetPlan>,
    right: Box<AsyncSpillSetPlan>,
    operation: GraphSetOperation,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
}

impl core::fmt::Debug for SpillSetBinaryPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillSetBinaryPlan")
            .field("operation", &self.operation)
            .field("quantifier", &self.quantifier)
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl SpillSetBinaryPlan {
    pub fn left(&self) -> &AsyncSpillSetPlan {
        &self.left
    }
    pub fn right(&self) -> &AsyncSpillSetPlan {
        &self.right
    }
    pub fn operation(&self) -> GraphSetOperation {
        self.operation
    }
    pub fn quantifier(&self) -> GraphSetQuantifier {
        self.quantifier
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    /// Decide one step using the SAME transition as the native resident merge.
    /// The caller owns at most one current head per sorted child and may use
    /// canonical encoded rows without allocating a decoded copy. The comparator
    /// must implement complete native canonical row ordering, including value
    /// type identity and NULL equality, and charge its own work through control.
    /// An explicit ORDER BY comparator or raw framing-byte order is invalid.
    ///
    /// This charges Work before comparison/decision, never ScratchEntry or a
    /// final ResultRow. The host uses its original cumulative query allowance,
    /// reserves payload bytes before pulling heads, and retains each reservation
    /// through any awaited append. A refusal is terminal for the host's tentative
    /// merge; no output handle may escape. Follow a returned advance exactly once
    /// and call again until Done, including the unselected suffix of either child.
    pub fn next_with_control<T: ?Sized, E, C, F>(
        &self,
        left: Option<&T>,
        right: Option<&T>,
        control: &mut C,
        compare: &mut F,
    ) -> Result<SpillSetMergeStep, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        F: FnMut(&T, &T, &mut C) -> Result<core::cmp::Ordering, E>,
    {
        merge::next_step(
            left,
            right,
            self.operation,
            self.quantifier,
            control,
            compare,
        )
    }
}

/// A compiler-owned native singleton source with one zero-column occurrence.
/// Starting it performs no graph access or source-record accounting. The host
/// separately admits its private input occurrence before encoding or appending.
#[derive(Clone, Debug)]
pub struct SpillSetValuesPlan {
    _private: (),
}
impl SpillSetValuesPlan {
    /// Charge the existing native Values work and retention before constructing
    /// its identity tuple. These events belong to the SAME cumulative allowance
    /// used by every following stage; no final ResultRow is charged here.
    ///
    /// The host retains the returned cursor's reservation through the yielded
    /// row's last use, including awaited encoding/appends. The consuming cursor
    /// emits exactly one unit row and then exhausts. Its first stage still owns
    /// the Values node's page; a later LIMIT zero never skips this source.
    pub fn start<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<SpillSetRows, SpillSetRowError<E>> {
        control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        Ok(SpillSetRows {
            state: StageRows::One(Some(GraphValueRow::unit())),
        })
    }
}

#[derive(Clone)]
struct GraphSourceMetadata {
    pattern: PreparedGraphPattern<GraphValueRow>,
    tail: ScanSortTail,
}

/// A bound source, binary relation or grouping followed by native row barriers.
///
/// First consume the complete source and apply its source_tail(), when present,
/// including hidden-column removal. Values emits one zero-column identity tuple
/// and has no graph pattern or scan tail. Binary executes both complete child
/// plans and merges them according to its native contract, also without a graph
/// pattern or scan tail. Aggregate reduces its complete child and converts all
/// groups through the native row-domain boundary. Then execute stages() in order. The first graph stage
/// canonicalizes visible source tuples as native Set admission does; a graph
/// leaf's ORDER BY/SKIP/LIMIT runs BEFORE this canonicalization.
/// No source, expression, clause or unsupported descendant is erased even when
/// the final count is zero. Bound plans contain no storage or query authority.
#[derive(Clone)]
pub struct AsyncSpillSetPlan {
    source: AsyncSpillSetSourcePlan,
    graph: Option<GraphSourceMetadata>,
    stages: Vec<SpillSetStage>,
}
impl AsyncSpillSetPlan {
    pub fn compile(query: &PreparedGraphSet) -> Result<Self, SpillSetBuildError> {
        Self::compile_at(query, 0)
    }

    fn compile_at(query: &PreparedGraphSet, depth: usize) -> Result<Self, SpillSetBuildError> {
        let mut stages = Vec::new();
        let pattern = match compile_stages(query, depth, &mut stages)? {
            CompiledSource::Values => {
                return Ok(Self {
                    source: AsyncSpillSetSourcePlan::Values(SpillSetValuesPlan { _private: () }),
                    graph: None,
                    stages,
                });
            }
            CompiledSource::Binary(binary) => {
                return Ok(Self {
                    source: AsyncSpillSetSourcePlan::Binary(binary),
                    graph: None,
                    stages,
                });
            }
            CompiledSource::Aggregate(aggregate) => {
                return Ok(Self {
                    source: AsyncSpillSetSourcePlan::Aggregate(aggregate),
                    graph: None,
                    stages,
                });
            }
            CompiledSource::Pattern(pattern) => pattern,
        };
        let (source, source_tail) = if matches!(
            pattern.plan().operators().first(),
            Some(GlaOperator::ScanEdges { .. })
        ) {
            // Choose the source contract from the complete native program,
            // never by retrying a failed single-edge plan or storage operation.
            if pattern
                .plan()
                .operators()
                .iter()
                .any(|op| matches!(op, GlaOperator::Expand { .. }))
            {
                let (plan, tail) = AsyncEdgeJoinPlan::compile_sort_input(pattern.plan())
                    .map_err(SpillSetBuildError::Edge)?;
                (AsyncSpillSetSourcePlan::Join(plan), tail)
            } else {
                let (plan, tail) = AsyncEdgeScanPlan::compile_sort_input(pattern.plan())
                    .map_err(SpillSetBuildError::Edge)?;
                (AsyncSpillSetSourcePlan::Edge(plan), tail)
            }
        } else {
            let (plan, tail) = AsyncVertexScanPlan::compile_sort_input(pattern.plan())
                .map_err(SpillSetBuildError::Vertex)?;
            (AsyncSpillSetSourcePlan::Vertex(plan), tail)
        };
        Ok(Self {
            source,
            graph: Some(GraphSourceMetadata {
                pattern: pattern.clone(),
                tail: source_tail,
            }),
            stages,
        })
    }

    pub fn source_pattern(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        self.graph.as_ref().map(|graph| &graph.pattern)
    }
    pub fn source(&self) -> &AsyncSpillSetSourcePlan {
        &self.source
    }
    pub fn source_columns(&self) -> &[String] {
        match &self.source {
            AsyncSpillSetSourcePlan::Binary(binary) => binary.columns(),
            AsyncSpillSetSourcePlan::Aggregate(aggregate) => aggregate.columns(),
            _ => self
                .graph
                .as_ref()
                .map_or(&[], |graph| graph.pattern.columns()),
        }
    }
    pub fn source_tail(&self) -> Option<&ScanSortTail> {
        self.graph.as_ref().map(|graph| &graph.tail)
    }
    pub fn stages(&self) -> &[SpillSetStage] {
        &self.stages
    }
    /// Total bounded physical barriers, including both binary descendants and
    /// their two canonicalization passes plus merge, and every aggregate's
    /// input, reduction, canonical group order and checked row conversion.
    /// Internal aggregate partitions/argument support have their own finite
    /// limits; aggregate_count() gives their number of independent reducers.
    /// This admits append-only metadata, never larger byte or work allowances.
    pub fn stage_count(&self) -> usize {
        let descendants = match &self.source {
            AsyncSpillSetSourcePlan::Binary(binary) => binary
                .left()
                .stage_count()
                .saturating_add(binary.right().stage_count())
                .saturating_add(3),
            AsyncSpillSetSourcePlan::Aggregate(aggregate) => {
                aggregate.input().stage_count().saturating_add(3)
            }
            _ => 0,
        };
        self.stages.len().saturating_add(descendants)
    }
    /// Exact number of intermediate numeric reducers, including all binary
    /// descendants. Hosts reserve each reducer's partition/DISTINCT append
    /// envelope without refreshing the cumulative query's disk or work budget.
    pub fn aggregate_count(&self) -> usize {
        match &self.source {
            AsyncSpillSetSourcePlan::Aggregate(aggregate) => {
                aggregate.input().aggregate_count().saturating_add(1)
            }
            AsyncSpillSetSourcePlan::Binary(binary) => binary
                .left()
                .aggregate_count()
                .saturating_add(binary.right().aggregate_count()),
            _ => 0,
        }
    }
    pub fn columns(&self) -> &[String] {
        self.stages
            .last()
            .expect("every admitted source leaf is a stage")
            .columns()
    }
}
impl core::fmt::Debug for AsyncSpillSetPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AsyncSpillSetPlan")
            .field("source", &self.source.kind())
            .field("stages", &self.stages.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone)]
enum Operation {
    Identity,
    Project(Vec<GraphSetProjection>),
    Filter(RowPredicate),
    Unwind(GraphSetValue),
}

/// A checked per-input operation and its complete finish contract.
///
/// Consume and evaluate EVERY input row first, in the preceding stage's order.
/// UNWIND appends each list element in order and retains duplicate occurrences.
/// Once that input succeeds, sort when order() is Some, deduplicate complete
/// tuples when distinct(), then apply offset()/count(). Only the resulting
/// complete sequence may feed the next stage. No stage has hidden columns.
/// None order preserves the child's sequence; Some(empty) requests canonical
/// whole-row order with NULL first, while a nonempty order uses the native
/// explicit key directions/null placements and canonical whole-row tie break.
#[derive(Clone)]
pub struct SpillSetStage {
    input: Vec<GraphSetColumnType>,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    operation: Operation,
    order: Option<Vec<GraphValueOrder>>,
    distinct: bool,
    offset: u64,
    count: Option<u64>,
}
impl SpillSetStage {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn input_types(&self) -> &[GraphSetColumnType] {
        &self.input
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }
    pub fn order(&self) -> Option<&[GraphValueOrder]> {
        self.order.as_deref()
    }
    pub fn distinct(&self) -> bool {
        self.distinct
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn count(&self) -> Option<u64> {
        self.count
    }

    /// Start one admitted input occurrence using the existing native kernel.
    /// This never sorts, deduplicates, windows, reads storage or charges a final
    /// ResultRow. row_index is its position in this stage's complete input,
    /// matching the ordinary projection error vocabulary.
    ///
    /// The host must own the input's byte reservation before calling. Every
    /// callback precedes its native operation; charge the SAME cumulative query
    /// meter and grow a reservation on ScratchEntry before returning success.
    /// This reservation owns the returned cursor's input and evaluated state;
    /// retain it through the last emitted row's use, including awaited encoding
    /// or scratch append. UNWIND evaluates one complete list per input, never
    /// the expanded output bag. Its minimum resident state must fit admission.
    /// Each cursor step takes a separate control for its new output allocations.
    /// Refusals pass through unchanged; an unaccounted guard supplies no byte
    /// bound. Neither an empty output page nor a later stage skips this work.
    pub fn start<E>(
        &self,
        row: GraphValueRow,
        row_index: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<SpillSetRows, SpillSetRowError<E>> {
        let mut event = |event| control(event).map_err(SpillSetRowError::Control);
        event(GlaExecutionEvent::Work)?;
        if row.len() != self.input.len() {
            return Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ));
        }
        for (kind, value) in self.input.iter().zip(row.values()) {
            event(GlaExecutionEvent::Work)?;
            if !kind.accepts(value) {
                return Err(SpillSetRowError::Native(
                    GraphSetExecutionError::InputSchema { operand: 0 },
                ));
            }
        }
        let state = match &self.operation {
            Operation::Identity => StageRows::One(Some(row)),
            Operation::Filter(predicate) => {
                StageRows::One(predicate.evaluate(&row, &mut event)?.then_some(row))
            }
            Operation::Project(projection) => {
                StageRows::One(Some(GraphSetProjection::evaluate_row_with_control(
                    &row,
                    projection,
                    &mut event,
                    |column, error| {
                        SpillSetRowError::Native(GraphSetExecutionError::Projection {
                            row: row_index,
                            column,
                            error,
                        })
                    },
                )?))
            }
            Operation::Unwind(value) => {
                StageRows::Unwind(projection::UnwindRows::new(row, value, &mut event).map_err(
                    |error| match error {
                        projection::ProjectionFailure::Control(error) => error,
                        projection::ProjectionFailure::Arithmetic { column, error } => {
                            SpillSetRowError::Native(GraphSetExecutionError::Projection {
                                row: row_index,
                                column,
                                error,
                            })
                        }
                    },
                )?)
            }
        };
        Ok(SpillSetRows { state })
    }
}
impl core::fmt::Debug for SpillSetStage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SpillSetStage([REDACTED])")
    }
}

enum StageRows {
    One(Option<GraphValueRow>),
    Unwind(projection::UnwindRows),
}

/// The owned output sequence of one original input occurrence.
///
/// Drain and retain this cursor with the reservation used by start(). UNWIND
/// state includes the original input and the evaluated list's backing storage.
/// No borrow of the plan, source, host control or storage survives evaluation.
#[must_use = "drain every stage input before applying its output clauses"]
pub struct SpillSetRows {
    state: StageRows,
}

impl SpillSetRows {
    /// Produce the next occurrence and its remaining cursor, or finish.
    ///
    /// The host supplies a fresh output reservation on each call and charges
    /// every event to the original cumulative query meter. ScratchEntry admits
    /// new output allocations before they occur. Keep that reservation with
    /// the output through encoding or awaited scratch append, then release it
    /// before requesting the next row. The state reservation remains live too:
    /// moved list elements still own payload admitted during start().
    ///
    /// Consuming self makes a refusal terminal: no remaining cursor escapes
    /// after interruption or failed admission. This method never charges a
    /// public ResultRow; only the host's complete final stage may do so.
    pub fn next_with_control<E>(
        self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(GraphValueRow, Self)>, SpillSetRowError<E>> {
        match self.state {
            StageRows::One(Some(row)) => Ok(Some((
                row,
                Self {
                    state: StageRows::One(None),
                },
            ))),
            StageRows::One(None) => Ok(None),
            StageRows::Unwind(rows) => rows
                .next_with_control(control)
                .map(|next| {
                    next.map(|(row, rows)| {
                        (
                            row,
                            Self {
                                state: StageRows::Unwind(rows),
                            },
                        )
                    })
                })
                .map_err(SpillSetRowError::Control),
        }
    }
}

impl core::fmt::Debug for SpillSetRows {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SpillSetRows([REDACTED])")
    }
}

/// Native row failures contain no fabricated storage error. The host may lift
/// Infallible with an exhaustive match; typed source/control refusals stay in
/// their own domain, never converted to a string or an empty relation.
#[derive(Debug, PartialEq, Eq)]
pub enum SpillSetRowError<E> {
    Control(E),
    Native(GraphSetExecutionError<Infallible>),
}
impl<E: core::fmt::Display> core::fmt::Display for SpillSetRowError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Control(error) => error.fmt(f),
            Self::Native(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for SpillSetRowError<E> {}

fn compile_stages<'a>(
    query: &'a PreparedGraphSet,
    depth: usize,
    stages: &mut Vec<SpillSetStage>,
) -> Result<CompiledSource<'a>, SpillSetBuildError> {
    let (pattern, input, operation, canonical, distinct) = match &query.node {
        SetNode::Values => (
            CompiledSource::Values,
            Vec::new(),
            Operation::Identity,
            false,
            false,
        ),
        SetNode::Pattern(pattern) => (
            CompiledSource::Pattern(pattern),
            query.types.clone(),
            Operation::Identity,
            true,
            false,
        ),
        SetNode::Scope(input) => (
            compile_stages(input, depth + 1, stages)?,
            input.types.clone(),
            Operation::Identity,
            false,
            false,
        ),
        SetNode::Filter { input, predicate } => (
            compile_stages(input, depth + 1, stages)?,
            input.types.clone(),
            Operation::Filter(predicate.clone()),
            false,
            false,
        ),
        SetNode::Project {
            input,
            projection,
            quantifier,
            preserve_order,
        } => (
            compile_stages(input, depth + 1, stages)?,
            input.types.clone(),
            Operation::Project(projection.clone()),
            (!*preserve_order && !input.preserves_row_order())
                || *quantifier == GraphSetQuantifier::Distinct,
            *quantifier == GraphSetQuantifier::Distinct,
        ),
        SetNode::Unwind { input, value } => (
            compile_stages(input, depth + 1, stages)?,
            input.types.clone(),
            Operation::Unwind(value.clone()),
            false,
            false,
        ),
        SetNode::Binary {
            operation,
            quantifier,
            left,
            right,
        } => (
            CompiledSource::Binary(SpillSetBinaryPlan {
                left: Box::new(AsyncSpillSetPlan::compile_at(left, depth + 1)?),
                right: Box::new(AsyncSpillSetPlan::compile_at(right, depth + 1)?),
                operation: *operation,
                quantifier: *quantifier,
                columns: query.columns.clone(),
                types: query.types.clone(),
            }),
            query.types.clone(),
            Operation::Identity,
            false,
            false,
        ),
        SetNode::Aggregate(summary) => {
            let input = AsyncSpillSetPlan::compile_at(summary.input(), depth + 1)?;
            let definition = SpillAggregateDefinition::from_relation(summary)
                .map_err(|_| SpillSetBuildError::Unsupported { depth })?;
            (
                CompiledSource::Aggregate(SpillSetAggregatePlan {
                    input: Box::new(input),
                    definition,
                    columns: query.columns.clone(),
                    types: query.types.clone(),
                }),
                query.types.clone(),
                Operation::Identity,
                false,
                false,
            )
        }
        SetNode::ProcedureCall(_) | SetNode::CrossJoin { .. } | SetNode::Join { .. } => {
            return Err(SpillSetBuildError::Unsupported { depth });
        }
    };
    let order = if !query.order.is_empty() {
        Some(query.order.clone())
    } else {
        canonical.then(Vec::new)
    };
    stages.push(SpillSetStage {
        input,
        columns: query.columns.clone(),
        types: query.types.clone(),
        operation,
        order,
        distinct,
        offset: query.offset,
        count: query.count,
    });
    Ok(pattern)
}

enum CompiledSource<'a> {
    Values,
    Pattern(&'a PreparedGraphPattern<GraphValueRow>),
    Binary(SpillSetBinaryPlan),
    Aggregate(SpillSetAggregatePlan),
}
