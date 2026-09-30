//! Typed set algebra over complete GLA value projections.
//!
//! Operands keep their own DISTINCT, ordering and page. Set equality is over
//! the entire canonical row (null equals null here, unlike WHERE). Column
//! positions and vertex/scalar domains must agree; output names come from the
//! left operand. This is the bounded materialized implementation, not a spill
//! engine, incremental derivative, or full GQL conformance claim.

mod aggregate;
mod execute;
pub(crate) use execute::finish_owned_projection;
mod filter;
mod incremental;
mod join;
mod merge;
mod procedure;
mod projection;
pub use aggregate::PreparedGraphSetAggregate;
pub use filter::incremental as row_filter;
pub use filter::{GraphSetFilterError, GraphSetOperand, GraphSetPredicateOp};
pub use procedure::{GraphSetSource, PreparedProcedureCall, WithProcedures};
pub use projection::{
    GraphListQuantifier, GraphSetProjection, GraphSetProjectionError, GraphSetValue,
};
pub(crate) use projection::{evaluate_membership, map_keys, range_values, slice_bounds};

use crate::algebra::{
    GraphOrderError, GraphValueOrder, GraphValueRow, PreparedGraphPattern, ValueProjection,
};
use crate::{GqlBudgetDimension, GqlQueryError, GqlQueryExecution, GqlQueryPolicy};

pub const MAX_GRAPH_SET_OPERANDS: usize = 32;
pub const MAX_GRAPH_SET_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSetOperation {
    Union,
    Intersect,
    Except,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSetQuantifier {
    All,
    Distinct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphSetColumnType {
    Vertex,
    Scalar,
    Path,
    Vertices,
    Edges,
    Edge,
    List,
    Any,
}
impl From<&ValueProjection> for GraphSetColumnType {
    fn from(column: &ValueProjection) -> Self {
        use crate::algebra::GraphPathFunction;
        match column {
            ValueProjection::Vertex { .. } => Self::Vertex,
            ValueProjection::Property { .. }
            | ValueProjection::EdgeProperty { .. }
            | ValueProjection::Type { .. } => Self::Scalar,
            ValueProjection::Labels { .. } => Self::List,
            ValueProjection::Path { function, .. } => match function {
                GraphPathFunction::Value => Self::Path,
                GraphPathFunction::Length => Self::Scalar,
                GraphPathFunction::Nodes => Self::Vertices,
                GraphPathFunction::Edges => Self::Edges,
                GraphPathFunction::Edge => Self::Edge,
                GraphPathFunction::Labels => Self::List,
                GraphPathFunction::Type => Self::Scalar,
            },
        }
    }
}
impl GraphSetColumnType {
    pub(crate) fn accepts(self, value: &crate::algebra::GraphValue) -> bool {
        use crate::algebra::GraphValue;
        self == Self::Any
            || value.is_null()
            || matches!(
                (self, value),
                (Self::Vertex, GraphValue::Vertex(_))
                    | (Self::Scalar, GraphValue::Scalar(_))
                    | (Self::Path, GraphValue::Path(_))
                    | (Self::Vertices, GraphValue::Vertices(_))
                    | (Self::Edges, GraphValue::Edges(_))
                    | (Self::Edge, GraphValue::Edge(_))
                    | (Self::List, GraphValue::List(_))
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetBuildError {
    /// The complete left (0) or right (1) schema differs from the checked join.
    JoinInputSchema {
        side: usize,
    },
    TooManyOperands {
        limit: usize,
        observed: usize,
    },
    TooDeep {
        limit: usize,
        observed: usize,
    },
    ColumnCount {
        left: usize,
        right: usize,
    },
    ColumnType {
        column: usize,
        left: GraphSetColumnType,
        right: GraphSetColumnType,
    },
}
impl core::fmt::Display for GraphSetBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph set definition: {self:?}")
    }
}
impl core::error::Error for GraphSetBuildError {}

#[derive(Debug, PartialEq, Eq)]
pub enum GraphSetExecutionError<E> {
    Source(E),
    /// Native join refusal after source admission; control errors keep their
    /// original outer GqlQueryError instead of being hidden inside this arm.
    Join(crate::row_join::RowJoinError<core::convert::Infallible>),
    /// Native grouping or checked row-domain output failed after its complete
    /// input succeeded. Boxing keeps nested relational/aggregate errors finite.
    Aggregate(Box<crate::GraphAggregateError<E>>),
    InputSchema {
        operand: usize,
    },
    InvalidSourceStatistics {
        operand: usize,
    },
    AccountingOverflow {
        dimension: GqlBudgetDimension,
    },
    Projection {
        row: usize,
        column: usize,
        error: crate::GraphIntegerError,
    },
    /// A CALL source reached a host that executes no procedures.
    ProcedureUnavailable {
        operand: usize,
    },
}
impl<E> GraphSetExecutionError<E> {
    /// Preserve relational failures while translating only the host source.
    pub fn map_source<T>(self, map: impl FnOnce(E) -> T) -> GraphSetExecutionError<T> {
        match self {
            Self::Source(source) => GraphSetExecutionError::Source(map(source)),
            Self::Join(error) => GraphSetExecutionError::Join(error),
            Self::Aggregate(error) => {
                GraphSetExecutionError::Aggregate(Box::new((*error).map_source(map)))
            }
            Self::InputSchema { operand } => GraphSetExecutionError::InputSchema { operand },
            Self::InvalidSourceStatistics { operand } => {
                GraphSetExecutionError::InvalidSourceStatistics { operand }
            }
            Self::AccountingOverflow { dimension } => {
                GraphSetExecutionError::AccountingOverflow { dimension }
            }
            Self::Projection { row, column, error } => {
                GraphSetExecutionError::Projection { row, column, error }
            }
            Self::ProcedureUnavailable { operand } => {
                GraphSetExecutionError::ProcedureUnavailable { operand }
            }
        }
    }
}
impl<E: core::fmt::Display> core::fmt::Display for GraphSetExecutionError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::Join(error) => error.fmt(f),
            Self::Aggregate(error) => write!(f, "aggregate row stage: {error}"),
            Self::InputSchema { operand } => {
                write!(f, "set operand {operand} returned an incompatible row")
            }
            Self::InvalidSourceStatistics { operand } => write!(
                f,
                "set operand {operand} returned inconsistent row statistics"
            ),
            Self::AccountingOverflow { dimension } => {
                write!(f, "set {dimension:?} accounting overflow")
            }
            Self::Projection { row, column, error } => {
                write!(f, "projection row {row} column {column}: {error}")
            }
            Self::ProcedureUnavailable { operand } => {
                write!(
                    f,
                    "set operand {operand} calls a procedure this host does not run"
                )
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphSetExecutionError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::Join(error) => Some(error),
            Self::Aggregate(error) => Some(error.as_ref()),
            Self::Projection { error, .. } => Some(error),
            _ => None,
        }
    }
}

type SetResult<T, E, C> = Result<T, GqlQueryError<GraphSetExecutionError<E>, C>>;

#[derive(Clone, PartialEq, Eq)]
enum SetNode {
    Pattern(PreparedGraphPattern<GraphValueRow>),
    Aggregate(Box<PreparedGraphSetAggregate>),
    Values,
    Unwind {
        input: Box<PreparedGraphSet>,
        value: GraphSetValue,
    },
    /// A procedure's rows, supplied by the host (Appendix C `ProcedureCall`).
    ProcedureCall(Box<PreparedProcedureCall>),
    CrossJoin {
        left: Box<PreparedGraphSet>,
        right: Box<PreparedGraphSet>,
    },
    Join {
        left: Box<PreparedGraphSet>,
        right: Box<PreparedGraphSet>,
        spec: crate::row_join::RowJoinSpec,
    },
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
        let types = pattern
            .value_columns()
            .iter()
            .map(GraphSetColumnType::from)
            .collect();
        Self {
            columns: pattern.columns().to_vec(),
            types,
            operands: 1,
            depth: 1,
            node: SetNode::Pattern(pattern),
            order: Vec::new(),
            offset: 0,
            count: None,
        }
    }
}
fn check_depth(depth: usize) -> Result<(), GraphSetBuildError> {
    if depth > MAX_GRAPH_SET_DEPTH {
        Err(GraphSetBuildError::TooDeep {
            limit: MAX_GRAPH_SET_DEPTH,
            observed: depth,
        })
    } else {
        Ok(())
    }
}
impl PreparedGraphSet {
    /// A zero-column, one-row relation with no graph source admission.
    #[must_use]
    pub fn singleton() -> Self {
        Self {
            node: SetNode::Values,
            columns: Vec::new(),
            types: Vec::new(),
            operands: 0,
            depth: 1,
            order: Vec::new(),
            offset: 0,
            count: None,
        }
    }

    /// Append each element of the evaluated list to its input row, preserving
    /// input and element order. NULL and empty lists emit no rows; other
    /// non-list values fail atomically during governed execution.
    pub fn unwind(
        self,
        name: String,
        value: GraphSetValue,
    ) -> Result<Self, GraphSetProjectionError> {
        use GraphSetProjectionError as Error;
        let column = self.columns.len();
        if column >= crate::algebra::MAX_PATTERN_VERTICES {
            return Err(Error::TooManyColumns {
                limit: crate::algebra::MAX_PATTERN_VERTICES,
                observed: column + 1,
            });
        }
        let depth = self.depth + 1;
        check_depth(depth).map_err(Error::SetBuild)?;
        projection::validate_name(&name, column)?;
        if self.columns.contains(&name) {
            return Err(Error::DuplicateName { column });
        }
        projection::value_type(&value, &self.types, column)?;
        let mut columns = self.columns.clone();
        columns.push(name);
        let mut types = self.types.clone();
        types.push(GraphSetColumnType::Any);
        let operands = self.operands;
        Ok(Self {
            node: SetNode::Unwind {
                input: Box::new(self),
                value,
            },
            columns,
            types,
            operands,
            depth,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    /// A registered procedure's rows as this pipeline's source (Appendix C
    /// `ProcedureCall`). CALL starts a read pipeline, so the input must be the
    /// zero-column singleton and the call runs exactly once. `outputs` pairs
    /// each procedure output with its column alias; every column is the Any
    /// domain. Arguments are constants: literals or bound parameters.
    /// `vertices` lists, ascending, the YIELD positions the statement matches
    /// as vertices; the host must refuse unless each is vertex-valued.
    pub fn procedure_call(
        self,
        namespace: String,
        name: String,
        arguments: Vec<GraphSetValue>,
        outputs: Vec<(String, String)>,
        vertices: Vec<usize>,
    ) -> Result<Self, GraphSetProjectionError> {
        use GraphSetProjectionError as Error;
        if !matches!(self.node, SetNode::Values)
            || !self.columns.is_empty()
            || !self.order.is_empty()
            || self.offset != 0
            || self.count.is_some()
        {
            return Err(Error::ProcedureNotFirst);
        }
        if outputs.is_empty() {
            return Err(Error::Empty);
        }
        for (column, &input) in vertices.iter().enumerate() {
            let ascending = column == 0 || vertices[column - 1] < input;
            if input >= outputs.len() || !ascending {
                return Err(Error::UnknownInput { column, input });
            }
        }
        if outputs.len() > crate::algebra::MAX_PATTERN_VERTICES {
            return Err(Error::TooManyColumns {
                limit: crate::algebra::MAX_PATTERN_VERTICES,
                observed: outputs.len(),
            });
        }
        for (column, argument) in arguments.iter().enumerate() {
            projection::value_type(argument, &[], column)?;
        }
        let mut columns = Vec::with_capacity(outputs.len());
        let mut names = Vec::with_capacity(outputs.len());
        for (column, (output, alias)) in outputs.into_iter().enumerate() {
            projection::validate_name(&alias, column)?;
            if columns.contains(&alias) {
                return Err(Error::DuplicateName { column });
            }
            columns.push(alias);
            names.push(output);
        }
        let operands = self.operands + 1;
        if operands > MAX_GRAPH_SET_OPERANDS {
            return Err(Error::SetBuild(GraphSetBuildError::TooManyOperands {
                limit: MAX_GRAPH_SET_OPERANDS,
                observed: operands,
            }));
        }
        let types = vec![GraphSetColumnType::Any; columns.len()];
        Ok(Self {
            node: SetNode::ProcedureCall(Box::new(PreparedProcedureCall {
                namespace,
                name,
                arguments,
                outputs: names,
                vertices,
            })),
            columns,
            types,
            operands,
            depth: self.depth,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    /// Concatenate each left row with each right row, in left-major order.
    /// Both children retain their own ordering/page and execute exactly once.
    pub fn cross_join(self, right: Self) -> Result<Self, GraphSetBuildError> {
        let operands = self.operands + right.operands;
        if operands > MAX_GRAPH_SET_OPERANDS {
            return Err(GraphSetBuildError::TooManyOperands {
                limit: MAX_GRAPH_SET_OPERANDS,
                observed: operands,
            });
        }
        let depth = 1 + self.depth.max(right.depth);
        check_depth(depth)?;
        let mut columns = self.columns.clone();
        columns.extend_from_slice(&right.columns);
        let mut types = self.types.clone();
        types.extend_from_slice(&right.types);
        Ok(Self {
            node: SetNode::CrossJoin {
                left: Box::new(self),
                right: Box::new(right),
            },
            columns,
            types,
            operands,
            depth,
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    /// The actual sole graph leaf, for a host that admits that source once.
    /// This does not turn a relation into a pattern or flatten any row stage.
    /// Multiple graph sources require a multi-source admission owner.
    pub(crate) fn single_pattern_input(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        if self.operands == 1 {
            self.first_pattern_input()
        } else {
            None
        }
    }

    fn preserves_row_order(&self) -> bool {
        match &self.node {
            SetNode::Values | SetNode::Unwind { .. } | SetNode::CrossJoin { .. } => true,
            // Procedure rows are canonically sorted on arrival, like a pattern.
            SetNode::Pattern(_)
            | SetNode::ProcedureCall(_)
            | SetNode::Aggregate(_)
            | SetNode::Join { .. } => false,
            SetNode::Scope(input)
            | SetNode::Project { input, .. }
            | SetNode::Filter { input, .. } => input.preserves_row_order(),
            SetNode::Binary { left, right, .. } => {
                left.preserves_row_order() || right.preserves_row_order()
            }
        }
    }

    /// Combining a relational stage with a new owner must include that owner
    /// in the same depth bound, not reset admission at a subsystem boundary.
    pub(crate) fn check_parent_depth(&self) -> Result<(), GraphSetBuildError> {
        check_depth(self.depth + 1)
    }

    pub(crate) fn check_ancestor_depth(&self, ancestors: usize) -> Result<(), GraphSetBuildError> {
        check_depth(self.depth.saturating_add(ancestors))
    }

    /// Combine exact, position-compatible relations. Heterogeneous canonical
    /// scalar kinds remain distinct; there is no implicit numeric coercion.
    pub fn combine(
        self,
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
        right: Self,
    ) -> Result<Self, GraphSetBuildError> {
        let operands = self.operands + right.operands;
        if operands > MAX_GRAPH_SET_OPERANDS {
            return Err(GraphSetBuildError::TooManyOperands {
                limit: MAX_GRAPH_SET_OPERANDS,
                observed: operands,
            });
        }
        let depth = 1 + self.depth.max(right.depth);
        check_depth(depth)?;
        if self.types.len() != right.types.len() {
            return Err(GraphSetBuildError::ColumnCount {
                left: self.types.len(),
                right: right.types.len(),
            });
        }
        for (column, (&left, &right)) in self.types.iter().zip(&right.types).enumerate() {
            if left != right {
                return Err(GraphSetBuildError::ColumnType {
                    column,
                    left,
                    right,
                });
            }
        }
        Ok(Self {
            columns: self.columns.clone(),
            types: self.types.clone(),
            operands,
            depth,
            node: SetNode::Binary {
                operation,
                quantifier,
                left: Box::new(self),
                right: Box::new(right),
            },
            order: Vec::new(),
            offset: 0,
            count: None,
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
            columns: self.columns.clone(),
            types: self.types.clone(),
            operands: self.operands,
            depth,
            node: SetNode::Scope(Box::new(self)),
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }

    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }
    #[must_use]
    pub const fn operand_count(&self) -> usize {
        self.operands
    }

    /// Rank this completed set before its page. Child ordering/pages are not
    /// changed. Null placement is independent of ASC/DESC; whole rows break
    /// ties. Repeated calls replace, rather than accumulate, sort metadata.
    pub fn with_order_by(mut self, order: &[GraphValueOrder]) -> Result<Self, GraphOrderError> {
        if order.is_empty() {
            return Err(GraphOrderError::EmptyOrder);
        }
        if order.len() > self.columns.len() {
            return Err(GraphOrderError::TooManyColumns {
                limit: self.columns.len(),
                observed: order.len(),
            });
        }
        for (at, key) in order.iter().enumerate() {
            if key.column >= self.columns.len() {
                return Err(GraphOrderError::UnknownColumn { column: key.column });
            }
            if order[..at].iter().any(|prior| prior.column == key.column) {
                return Err(GraphOrderError::DuplicateColumn { column: key.column });
            }
        }
        self.order = order.to_vec();
        Ok(self)
    }

    #[must_use]
    pub fn with_page(mut self, offset: u64, count: Option<u64>) -> Self {
        self.offset = offset;
        self.count = count;
        self
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
            SetNode::Aggregate(summary) => {
                bytes.push(9);
                let definition = summary.canonical_bytes();
                bytes.extend_from_slice(&(definition.len() as u64).to_be_bytes());
                bytes.extend_from_slice(&definition);
            }
            SetNode::Values => bytes.push(5),
            SetNode::ProcedureCall(call) => {
                bytes.push(10);
                call.append_transcript(bytes);
            }
            SetNode::Unwind { input, value } => {
                bytes.push(6);
                input.append_transcript(bytes);
                projection::append_value_transcript(value, bytes);
            }
            SetNode::CrossJoin { left, right } => {
                bytes.push(7);
                left.append_transcript(bytes);
                right.append_transcript(bytes);
            }
            SetNode::Join { left, right, spec } => {
                bytes.push(8);
                left.append_transcript(bytes);
                right.append_transcript(bytes);
                join::append_transcript(spec, bytes);
            }
            SetNode::Binary {
                operation,
                quantifier,
                left,
                right,
            } => {
                bytes.push(1);
                bytes.push(match operation {
                    GraphSetOperation::Union => 0,
                    GraphSetOperation::Intersect => 1,
                    GraphSetOperation::Except => 2,
                });
                bytes.push(match quantifier {
                    GraphSetQuantifier::All => 0,
                    GraphSetQuantifier::Distinct => 1,
                });
                left.append_transcript(bytes);
                right.append_transcript(bytes);
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
            SetNode::Project {
                input,
                projection,
                quantifier,
            } => {
                bytes
                    .extend_from_slice(&[3, u8::from(*quantifier == GraphSetQuantifier::Distinct)]);
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
        if let Some(count) = self.count {
            bytes.extend_from_slice(&count.to_be_bytes());
        }
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
        &self,
        policy: GqlQueryPolicy,
        mut source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        execute::execute(self, policy, &mut source, checkpoint)
    }

    /// `execute_governed` plus a procedure host for CALL sources. `procedures`
    /// receives each call with its evaluated constant arguments and returns
    /// exactly `call.outputs()` columns per row, under the same allowance and
    /// pinned snapshot contract as `patterns`. Without this entry a CALL
    /// source refuses with `ProcedureUnavailable`.
    pub fn execute_governed_with_procedures<E, C>(
        &self,
        policy: GqlQueryPolicy,
        patterns: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        procedures: impl FnMut(
            &PreparedProcedureCall,
            &[crate::algebra::GraphValue],
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        let mut source = WithProcedures {
            patterns,
            procedures,
        };
        execute::execute(self, policy, &mut source, checkpoint)
    }

    /// The executor over any trusted source, for relational consumers (a
    /// grouped aggregate) that forward their caller's procedure host.
    pub(crate) fn execute_with_source<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: &mut impl GraphSetSource<E, C>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        execute::execute(self, policy, source, checkpoint)
    }

    /// Whether any source of this relation is a procedure call.
    #[must_use]
    pub fn calls_procedure(&self) -> bool {
        match &self.node {
            SetNode::ProcedureCall(_) => true,
            SetNode::Pattern(_) | SetNode::Values => false,
            SetNode::Aggregate(summary) => summary.input().calls_procedure(),
            SetNode::Unwind { input, .. }
            | SetNode::Scope(input)
            | SetNode::Filter { input, .. }
            | SetNode::Project { input, .. } => input.calls_procedure(),
            SetNode::CrossJoin { left, right }
            | SetNode::Join { left, right, .. }
            | SetNode::Binary { left, right, .. } => {
                left.calls_procedure() || right.calls_procedure()
            }
        }
    }
}
