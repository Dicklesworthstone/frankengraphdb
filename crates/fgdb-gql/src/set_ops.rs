//! Typed set algebra over complete GLA value projections.
//!
//! Operands keep their own DISTINCT, ordering and page. Set equality is over
//! the entire canonical row (null equals null here, unlike WHERE). Column
//! positions and vertex/scalar domains must agree; output names come from the
//! left operand. This is the bounded materialized implementation, not a spill
//! engine, incremental derivative, or full GQL conformance claim.

mod execute;
mod filter;
mod merge;
mod projection;
pub use filter::{GraphSetFilterError, GraphSetOperand, GraphSetPredicateOp};
pub use projection::{GraphSetProjection, GraphSetProjectionError, GraphSetValue};

use crate::algebra::{GraphOrderError, GraphValueOrder, GraphValueRow, PreparedGraphPattern, ValueProjection};
use crate::{GqlBudgetDimension, GqlQueryError, GqlQueryExecution, GqlQueryPolicy};

pub const MAX_GRAPH_SET_OPERANDS: usize = 32;
pub const MAX_GRAPH_SET_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSetOperation { Union, Intersect, Except }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSetQuantifier { All, Distinct }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSetColumnType { Vertex, Scalar }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetBuildError {
    TooManyOperands { limit: usize, observed: usize },
    TooDeep { limit: usize, observed: usize },
    ColumnCount { left: usize, right: usize },
    ColumnType { column: usize, left: GraphSetColumnType, right: GraphSetColumnType },
}
impl core::fmt::Display for GraphSetBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph set definition: {self:?}")
    }
}
impl core::error::Error for GraphSetBuildError {}

#[derive(Debug)]
pub enum GraphSetExecutionError<E> {
    Source(E),
    InputSchema { operand: usize },
    InvalidSourceStatistics { operand: usize },
    AccountingOverflow { dimension: GqlBudgetDimension },
    Projection { row: usize, column: usize, error: crate::GraphIntegerError },
}
impl<E: core::fmt::Display> core::fmt::Display for GraphSetExecutionError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InputSchema { operand } => write!(f, "set operand {operand} returned an incompatible row"),
            Self::InvalidSourceStatistics { operand } => write!(f, "set operand {operand} returned inconsistent row statistics"),
            Self::AccountingOverflow { dimension } => write!(f, "set {dimension:?} accounting overflow"),
            Self::Projection { row, column, error } => write!(f, "projection row {row} column {column}: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphSetExecutionError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Projection { error, .. } => Some(error),
            _ => None,
        }
    }
}

type SetResult<T, E, C> = Result<T, GqlQueryError<GraphSetExecutionError<E>, C>>;

#[derive(Clone, PartialEq, Eq)]
enum SetNode {
    Pattern(PreparedGraphPattern<GraphValueRow>),
    Scope(Box<PreparedGraphSet>),
    Filter {
        input: Box<PreparedGraphSet>,
        predicate: filter::RowPredicate,
    },
    Project {
        input: Box<PreparedGraphSet>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    },
    Binary {
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
        left: Box<PreparedGraphSet>,
        right: Box<PreparedGraphSet>,
    },
}

/// Immutable relational composition. It deliberately does not expose a fake
/// binding pattern: a set result has values, not the operands' private slots.
/// Operand and depth caps also bound recursive compilation, execution and drop.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphSet {
    node: SetNode,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    operands: usize,
    depth: usize,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
}
impl core::fmt::Debug for PreparedGraphSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphSet")
            .field("operands", &self.operands)
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl From<PreparedGraphPattern<GraphValueRow>> for PreparedGraphSet {
    fn from(pattern: PreparedGraphPattern<GraphValueRow>) -> Self {
        let types = pattern.value_columns().iter().map(|column| match column {
            ValueProjection::Vertex { .. } => GraphSetColumnType::Vertex,
            ValueProjection::Property { .. } => GraphSetColumnType::Scalar,
        }).collect();
        Self {
            columns: pattern.columns().to_vec(), types, operands: 1, depth: 1,
            node: SetNode::Pattern(pattern), order: Vec::new(), offset: 0, count: None,
        }
    }
}
fn check_depth(depth: usize) -> Result<(), GraphSetBuildError> {
    if depth > MAX_GRAPH_SET_DEPTH {
        Err(GraphSetBuildError::TooDeep { limit: MAX_GRAPH_SET_DEPTH, observed: depth })
    } else { Ok(()) }
}
impl PreparedGraphSet {
    /// Combine exact, position-compatible relations. Heterogeneous canonical
    /// scalar kinds remain distinct; there is no implicit numeric coercion.
    pub fn combine(
        self, operation: GraphSetOperation, quantifier: GraphSetQuantifier,
        right: Self,
    ) -> Result<Self, GraphSetBuildError> {
        let operands = self.operands + right.operands;
        if operands > MAX_GRAPH_SET_OPERANDS {
            return Err(GraphSetBuildError::TooManyOperands {
                limit: MAX_GRAPH_SET_OPERANDS, observed: operands,
            });
        }
        let depth = 1 + self.depth.max(right.depth);
        check_depth(depth)?;
        if self.types.len() != right.types.len() {
            return Err(GraphSetBuildError::ColumnCount { left: self.types.len(), right: right.types.len() });
        }
        for (column, (&left, &right)) in self.types.iter().zip(&right.types).enumerate() {
            if left != right { return Err(GraphSetBuildError::ColumnType { column, left, right }); }
        }
        Ok(Self {
            columns: self.columns.clone(), types: self.types.clone(), operands, depth,
            node: SetNode::Binary { operation, quantifier, left: Box::new(self), right: Box::new(right) },
            order: Vec::new(), offset: 0, count: None,
        })
    }

    /// Establish a new relational scope without replacing the input's order or
    /// page. An outer ORDER BY or LIMIT must operate on the already selected
    /// inner rows, not overwrite the inner selection. Sources are not copied or
    /// rerun. Without an outer order, this scope retains the input's ordering.
    pub fn nested(self) -> Result<Self, GraphSetBuildError> {
        let depth = self.depth + 1;
        check_depth(depth)?;
        Ok(Self {
            columns: self.columns.clone(), types: self.types.clone(),
            operands: self.operands, depth, node: SetNode::Scope(Box::new(self)),
            order: Vec::new(), offset: 0, count: None,
        })
    }

    #[must_use]
    pub fn columns(&self) -> &[String] { &self.columns }
    #[must_use]
    pub fn column_types(&self) -> &[GraphSetColumnType] { &self.types }
    #[must_use]
    pub const fn operand_count(&self) -> usize { self.operands }

    /// Rank this completed set before its page. Child ordering/pages are not
    /// changed. Null placement is independent of ASC/DESC; whole rows break
    /// ties. Repeated calls replace, rather than accumulate, sort metadata.
    pub fn with_order_by(mut self, order: &[GraphValueOrder]) -> Result<Self, GraphOrderError> {
        if order.is_empty() { return Err(GraphOrderError::EmptyOrder); }
        if order.len() > self.columns.len() {
            return Err(GraphOrderError::TooManyColumns { limit: self.columns.len(), observed: order.len() });
        }
        for (at, key) in order.iter().enumerate() {
            if key.column >= self.columns.len() { return Err(GraphOrderError::UnknownColumn { column: key.column }); }
            if order[..at].iter().any(|prior| prior.column == key.column) {
                return Err(GraphOrderError::DuplicateColumn { column: key.column });
            }
        }
        self.order = order.to_vec();
        Ok(self)
    }

    #[must_use]
    pub fn with_page(mut self, offset: u64, count: Option<u64>) -> Self {
        self.offset = offset; self.count = count; self
    }

    /// Application transcript, not an Appendix A durable format. Preserve
    /// operand grouping/order, quantifiers and every local/final page. Aliases
    /// are schema metadata, as in the underlying pattern transcript.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:bounded-set:v1\0".to_vec();
        self.append_transcript(&mut bytes);
        bytes
    }
    fn append_transcript(&self, bytes: &mut Vec<u8>) {
        match &self.node {
            SetNode::Pattern(pattern) => {
                bytes.push(0);
                let input = pattern.canonical_bytes();
                bytes.extend_from_slice(&(input.len() as u64).to_be_bytes());
                bytes.extend_from_slice(&input);
            }
            SetNode::Binary { operation, quantifier, left, right } => {
                bytes.push(1);
                bytes.push(match operation { GraphSetOperation::Union => 0, GraphSetOperation::Intersect => 1, GraphSetOperation::Except => 2 });
                bytes.push(match quantifier { GraphSetQuantifier::All => 0, GraphSetQuantifier::Distinct => 1 });
                left.append_transcript(bytes); right.append_transcript(bytes);
            }
            SetNode::Scope(input) => {
                bytes.push(2);
                input.append_transcript(bytes);
            }
            SetNode::Filter { input, predicate } => {
                bytes.push(4);
                input.append_transcript(bytes);
                predicate.append_transcript(bytes);
            }
            SetNode::Project { input, projection, quantifier } => {
                bytes.extend_from_slice(&[3, u8::from(*quantifier == GraphSetQuantifier::Distinct)]);
                input.append_transcript(bytes);
                projection::append_transcript(projection, bytes);
            }
        }
        bytes.extend_from_slice(&(self.order.len() as u64).to_be_bytes());
        for key in &self.order {
            bytes.extend_from_slice(&(key.column as u64).to_be_bytes());
            bytes.extend_from_slice(&[u8::from(key.descending), u8::from(key.nulls_first)]);
        }
        bytes.extend_from_slice(&self.offset.to_be_bytes());
        bytes.push(u8::from(self.count.is_some()));
        if let Some(count) = self.count { bytes.extend_from_slice(&count.to_be_bytes()); }
    }

    /// Execute typed set operators over the ordinary governed GLA entrypoint.
    /// The host must pin ONE immutable snapshot/transaction overlay and the
    /// same authorization/catalog context for every source invocation. This
    /// closure is a trusted source adapter, not an arbitrary result provider.
    /// Database and pinned-view adapters enforce that lifetime structurally.
    ///
    /// Each operand executes exactly once, left-to-right, including an empty
    /// right side and LIMIT 0. No short circuit erases negative-read witnesses
    /// or turns a late source failure into a successful difference. Source
    /// record counts are admission VISITS summed across operands, not unique
    /// database records. All work/scratch shares one allowance. Only the final
    /// page consumes the external result-row allowance; operand rows remain
    /// private, and are additionally charged as retained set scratch.
    pub fn execute_governed<E, C>(
        &self, policy: GqlQueryPolicy,
        mut source: impl FnMut(&PreparedGraphPattern<GraphValueRow>, GqlQueryPolicy)
            -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        execute::execute(self, policy, &mut source, checkpoint)
    }
}
