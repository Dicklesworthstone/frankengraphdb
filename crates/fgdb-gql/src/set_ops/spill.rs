//! Physical admission for external unary relations. Do not fuse across a
//! stage: the child's complete evaluation and selected row order precede the
//! parent's expressions, including when a later stage requests LIMIT zero.

use super::{
    GraphSetColumnType, GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier,
    PreparedGraphSet, SetNode, filter::RowPredicate,
};
use crate::GlaExecutionEvent;
use crate::algebra::{GlaOperator, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use crate::edge_stream::{AsyncEdgeJoinPlan, AsyncEdgeScanPlan, EdgeScanBuildError};
use crate::scan_stream::{ScanKind, ScanSortTail};
use crate::stream::{AsyncVertexScanPlan, VertexScanBuildError};
use core::convert::Infallible;

/// A refusal found before any graph source or scratch writer is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillSetBuildError {
    /// Only a chain of scope, selection and projection over one native graph
    /// leaf is admitted. This ordinal is the depth from the outermost node.
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
                    "external unary relation has an unsupported node at depth {depth}"
                )
            }
            Self::Vertex(error) => error.fmt(f),
            Self::Edge(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for SpillSetBuildError {}

/// The ordinary async sort-input compiler owns all graph predicates, captures
/// and projection. This is a fixed source choice, never a fallback strategy.
#[derive(Clone, Debug)]
pub enum AsyncSpillSetSourcePlan {
    Vertex(AsyncVertexScanPlan<GraphValueRow>),
    Edge(AsyncEdgeScanPlan),
    Join(AsyncEdgeJoinPlan),
}
impl AsyncSpillSetSourcePlan {
    pub fn kind(&self) -> ScanKind {
        match self {
            Self::Vertex(_) => ScanKind::Vertex,
            Self::Edge(_) | Self::Join(_) => ScanKind::Edge,
        }
    }
}

/// One bound graph source followed by exact native relational barriers.
///
/// First consume the complete source and apply source_tail(), including its
/// hidden-column removal. Then execute stages() in order. The first stage
/// canonicalizes the visible source tuples, just as native Set admission does;
/// a graph leaf's own ORDER BY/SKIP/LIMIT runs BEFORE this canonicalization.
/// No source, expression, clause or unsupported descendant is erased even when
/// the final count is zero. Bound plans contain no storage or query authority.
#[derive(Clone)]
pub struct AsyncSpillSetPlan {
    pattern: PreparedGraphPattern<GraphValueRow>,
    source: AsyncSpillSetSourcePlan,
    source_tail: ScanSortTail,
    stages: Vec<SpillSetStage>,
}
impl AsyncSpillSetPlan {
    pub fn compile(query: &PreparedGraphSet) -> Result<Self, SpillSetBuildError> {
        let mut stages = Vec::new();
        let pattern = compile_stages(query, 0, &mut stages)?;
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
            pattern: pattern.clone(),
            source,
            source_tail,
            stages,
        })
    }

    pub fn source_pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.pattern
    }
    pub fn source(&self) -> &AsyncSpillSetSourcePlan {
        &self.source
    }
    pub fn source_columns(&self) -> &[String] {
        self.pattern.columns()
    }
    pub fn source_tail(&self) -> &ScanSortTail {
        &self.source_tail
    }
    pub fn stages(&self) -> &[SpillSetStage] {
        &self.stages
    }
    pub fn columns(&self) -> &[String] {
        self.stages
            .last()
            .expect("a graph leaf is a stage")
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
}

/// A checked per-row operation and its complete finish contract.
///
/// Consume and evaluate EVERY input row first, in the preceding stage's order.
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

    /// Evaluate one admitted input occurrence using the existing native kernel.
    /// This never sorts, deduplicates, windows, reads storage or charges a final
    /// ResultRow. row_index is its position in this stage's complete input,
    /// matching the ordinary projection error vocabulary.
    ///
    /// The host must own the input's byte reservation before calling. Every
    /// callback precedes its native operation; charge the SAME cumulative query
    /// meter and grow a reservation on ScratchEntry before returning success.
    /// Keep that reservation with all input/intermediate/output payloads through
    /// their last use, including any awaited encoding or scratch append. A
    /// callback refusal passes through unchanged. This synchronous semantic
    /// seam provides no byte bound when the host supplies an unaccounted guard.
    pub fn evaluate<E>(
        &self,
        row: GraphValueRow,
        row_index: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphValueRow>, SpillSetRowError<E>> {
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
        match &self.operation {
            Operation::Identity => Ok(Some(row)),
            Operation::Filter(predicate) => {
                Ok(predicate.evaluate(&row, &mut event)?.then_some(row))
            }
            Operation::Project(projection) => GraphSetProjection::evaluate_row_with_control(
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
            )
            .map(Some),
        }
    }
}
impl core::fmt::Debug for SpillSetStage {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SpillSetStage([REDACTED])")
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
) -> Result<&'a PreparedGraphPattern<GraphValueRow>, SpillSetBuildError> {
    let (pattern, input, operation, canonical, distinct) = match &query.node {
        SetNode::Pattern(pattern) => (
            pattern,
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
        } => (
            compile_stages(input, depth + 1, stages)?,
            input.types.clone(),
            Operation::Project(projection.clone()),
            !input.preserves_row_order() || *quantifier == GraphSetQuantifier::Distinct,
            *quantifier == GraphSetQuantifier::Distinct,
        ),
        SetNode::Aggregate(_)
        | SetNode::Values
        | SetNode::Unwind { .. }
        | SetNode::ProcedureCall(_)
        | SetNode::CrossJoin { .. }
        | SetNode::Join { .. }
        | SetNode::Binary { .. } => return Err(SpillSetBuildError::Unsupported { depth }),
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
