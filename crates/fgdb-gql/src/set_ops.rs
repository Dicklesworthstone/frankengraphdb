//! Typed set algebra over complete GLA value projections.
//!
//! Operands keep their own DISTINCT, ordering and page. Set equality is over
//! the entire canonical row (null equals null here, unlike WHERE). Column
//! positions must agree, and each column keeps its common domain: equal
//! concrete domains remain concrete, Any stays open, and different concrete
//! domains refuse. Output names come from the left operand. This is the bounded
//! materialized implementation, not a spill engine, incremental derivative,
//! or full GQL conformance claim.

mod aggregate;
mod execute;
pub(crate) use aggregate::{WriteReturnGroup, WriteReturnGroupSpec};
pub(crate) use execute::{
    finish_owned_aggregate, finish_owned_projection, finish_owned_tail, owned_head_policy,
    sort_canonical_values,
};
mod filter;
mod incremental;
mod join;
mod merge;
mod procedure;
pub(crate) mod projection;
pub(crate) mod spill;
pub use aggregate::PreparedGraphSetAggregate;
pub use filter::incremental as row_filter;
pub use filter::{GraphSetFilterError, GraphSetOperand, GraphSetPredicateOp};
pub(crate) use procedure::{BorrowedSource, DynPatterns, DynProcedures, PatternSource};
pub use procedure::{
    GraphSetSource, PreparedProcedureCall, ProcedureGraphDirection, ProcedureGraphProjection,
    ProcedureGraphWeight, ProcedureMissingWeight, ProcedureParallelEdges, ProcedureSelfLoops,
    WithElementReads, WithProcedures,
};
pub(crate) use projection::{
    ElementRead, ProjectionFailure, construct_temporal, evaluate_equality, evaluate_membership,
    evaluate_order, evaluate_value, map_keys, no_element_reads, overlay_map, range_values,
    slice_bounds, temporal_arithmetic, temporal_field, temporal_field_name,
    temporal_field_scratch_entries, timestamp_text, value_function,
};
pub use projection::{
    GqlTimeZones, GraphElementKey, GraphListQuantifier, GraphSetProjection,
    GraphSetProjectionError, GraphSetValue, GraphTemporalFunction, GraphValueFunction,
};

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
    /// Every canonical native value, including maps, lists and graph elements.
    /// Combining this open domain with a concrete one never narrows it.
    Any,
    /// A stored property value (fgdb-k3nub, fgdb-kr8j1): a scalar, or a
    /// stored List or Map, never a graph element at any depth. A property
    /// column has this domain; every consumer that admits a Scalar column
    /// admits it too, and a scalar-only operator still refuses a collection
    /// at run time with its own typed error. Scalar keeps its strict domain.
    Stored,
}
impl From<&ValueProjection> for GraphSetColumnType {
    fn from(column: &ValueProjection) -> Self {
        use crate::algebra::GraphPathFunction;
        match column {
            ValueProjection::Vertex { .. } => Self::Vertex,
            ValueProjection::Property { .. } | ValueProjection::EdgeProperty { .. } => Self::Stored,
            ValueProjection::Type { .. } => Self::Scalar,
            ValueProjection::Labels { .. } => Self::List,
            ValueProjection::Properties { .. } | ValueProjection::EdgeProperties { .. } => {
                Self::Any
            }
            ValueProjection::Path { function, .. } => match function {
                GraphPathFunction::Value => Self::Path,
                GraphPathFunction::Length => Self::Scalar,
                GraphPathFunction::Nodes => Self::Vertices,
                GraphPathFunction::Edges => Self::Edges,
                GraphPathFunction::Edge => Self::Edge,
                GraphPathFunction::Labels => Self::List,
                GraphPathFunction::Type => Self::Scalar,
                GraphPathFunction::Properties => Self::Any,
            },
        }
    }
}
impl GraphSetColumnType {
    pub(crate) const fn parameter_domain(kind: crate::GqlParameterType) -> Self {
        match kind {
            crate::GqlParameterType::List => Self::List,
            crate::GqlParameterType::Map => Self::Any,
            _ => Self::Scalar,
        }
    }

    pub(crate) const fn tag(self) -> u8 {
        match self {
            Self::Vertex => 0,
            Self::Scalar => 1,
            Self::Path => 2,
            Self::Vertices => 3,
            Self::Edges => 4,
            Self::Edge => 5,
            Self::List => 6,
            Self::Any => 7,
            Self::Stored => 8,
        }
    }

    /// Whether this column may be read as a scalar operand: a Scalar column,
    /// or a stored property value, whose collections refuse at run time.
    #[must_use]
    pub(crate) const fn is_scalar_like(self) -> bool {
        matches!(self, Self::Scalar | Self::Stored)
    }

    /// The common checked domain of two set-operand columns. Any already
    /// admits every native value, so it remains open when paired with a known
    /// domain. A stored property value widens a Scalar column it is paired
    /// with, and is itself widened to Any by a List column, whose lists may
    /// hold graph elements. Other different concrete domains still refuse;
    /// this changes schema admission only, never canonical equality, values or
    /// numeric coercion.
    ///
    /// Native and maintained set compilers share this rule. Apply it to the
    /// complete declared schemas before inspecting any rows, including empty
    /// or all-NULL operands. The result must remain the output domain for all
    /// later updates and consumers; currently observed values cannot narrow it.
    /// This does not admit a source, grant capabilities, or make unsupported
    /// operators maintainable.
    #[must_use]
    pub fn common_set_domain(self, other: Self) -> Option<Self> {
        if self == Self::Any || other == Self::Any {
            Some(Self::Any)
        } else if self == other {
            Some(self)
        } else if self.is_scalar_like() && other.is_scalar_like() {
            Some(Self::Stored)
        } else if matches!(
            (self, other),
            (Self::Stored, Self::List) | (Self::List, Self::Stored)
        ) {
            Some(Self::Any)
        } else {
            None
        }
    }

    pub(crate) fn accepts(self, value: &crate::algebra::GraphValue) -> bool {
        use crate::algebra::GraphValue;
        self == Self::Any
            || value.is_null()
            || match (self, value) {
                (Self::Stored, GraphValue::Scalar(_)) => true,
                (Self::Stored, GraphValue::List(_) | GraphValue::Map { .. }) => {
                    value.is_stored_value()
                }
                _ => matches!(
                    (self, value),
                    (Self::Vertex, GraphValue::Vertex(_))
                        | (Self::Scalar, GraphValue::Scalar(_))
                        | (Self::Path, GraphValue::Path(_))
                        | (Self::Vertices, GraphValue::Vertices(_))
                        | (Self::Edges, GraphValue::Edges(_))
                        | (Self::Edge, GraphValue::Edge(_))
                        | (Self::List, GraphValue::List(_))
                ),
            }
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
    /// An owned-row leaf reached a host that supplies no owned rows.
    OwnedRowsUnavailable,
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
            Self::OwnedRowsUnavailable => GraphSetExecutionError::OwnedRowsUnavailable,
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
            Self::OwnedRowsUnavailable => {
                f.write_str("a write tail reached a host without its head rows")
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
    /// A write's RETURN head, owned and already charged by the write, under
    /// its WITH tail (`SET ... WITH n WHERE ... RETURN`). The executing host
    /// supplies the rows through `GraphSetSource::owned`; no read builds one.
    Owned,
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
        /// A compiler-owned output projection after ranking removes private
        /// sort cells without canonicalizing the selected occurrence sequence.
        preserve_order: bool,
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
    /// A write head's owned rows under its WITH tail: the schema only. The
    /// executing write supplies the rows (`GraphSetSource::owned`).
    pub(crate) fn owned(columns: Vec<String>, types: Vec<GraphSetColumnType>) -> Self {
        Self {
            node: SetNode::Owned,
            columns,
            types,
            operands: 0,
            depth: 1,
            order: Vec::new(),
            offset: 0,
            count: None,
        }
    }

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
        projection::validate_binding_name(&name, column)?;
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
        names: Vec<String>,
        outputs: Vec<(String, String)>,
        vertices: Vec<usize>,
    ) -> Result<Self, GraphSetProjectionError> {
        self.procedure_call_with_projection(
            namespace, name, arguments, names, outputs, vertices, None,
        )
    }

    /// A procedure call with a catalog-bound graph projection. The projection
    /// narrows the host's authorized snapshot; source, output and argument
    /// checks are the same as [`Self::procedure_call`].
    #[allow(clippy::too_many_arguments)]
    pub fn procedure_call_with_projection(
        self,
        namespace: String,
        name: String,
        arguments: Vec<GraphSetValue>,
        names: Vec<String>,
        outputs: Vec<(String, String)>,
        vertices: Vec<usize>,
        graph_projection: Option<ProcedureGraphProjection>,
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
        // Named arguments name every argument, each once.
        if !names.is_empty() {
            if names.len() != arguments.len() {
                return Err(Error::Empty);
            }
            for (column, argument) in names.iter().enumerate() {
                projection::validate_binding_name(argument, column)?;
                if names[..column].contains(argument) {
                    return Err(Error::DuplicateName { column });
                }
            }
        }
        let mut columns = Vec::with_capacity(outputs.len());
        let mut output_names = Vec::with_capacity(outputs.len());
        for (column, (output, alias)) in outputs.into_iter().enumerate() {
            projection::validate_binding_name(&alias, column)?;
            if columns.contains(&alias) {
                return Err(Error::DuplicateName { column });
            }
            columns.push(alias);
            output_names.push(output);
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
                names,
                graph_projection,
                outputs: output_names,
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
            // Procedure rows are canonically sorted on arrival, like a pattern;
            // so are a write head's owned rows.
            SetNode::Pattern(_)
            | SetNode::ProcedureCall(_)
            | SetNode::Owned
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

    /// Combine exact, position-compatible relations. A column with the open Any
    /// domain widens the corresponding output column to Any; different concrete
    /// domains refuse. Heterogeneous canonical scalar kinds remain distinct;
    /// there is no implicit value conversion or numeric coercion.
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
        let mut types = Vec::with_capacity(self.types.len());
        for (column, (&left, &right)) in self.types.iter().zip(&right.types).enumerate() {
            types.push(
                left.common_set_domain(right)
                    .ok_or(GraphSetBuildError::ColumnType {
                        column,
                        left,
                        right,
                    })?,
            );
        }
        Ok(Self {
            columns: self.columns.clone(),
            types,
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
            SetNode::Owned => bytes.push(11),
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
                preserve_order,
            } => {
                bytes.extend_from_slice(&[
                    if *preserve_order { 11 } else { 3 },
                    u8::from(*quantifier == GraphSetQuantifier::Distinct),
                ]);
                input.append_transcript(bytes);
                projection::append_transcript(projection, bytes);
            }
        }
        if !self.order.is_empty() {
            // Sorting semantics are part of the logical definition. Empty
            // order keys still mean the unchanged canonical row contract.
            bytes.extend_from_slice(b"fgdb:set-orderability:v1\0");
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
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        let patterns: &mut DynPatterns<'_, E, C> = &mut source;
        let mut source = PatternSource { patterns };
        let checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
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
        mut patterns: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        )
            -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut procedures: impl FnMut(
            &PreparedProcedureCall,
            &[crate::algebra::GraphValue],
            GqlQueryPolicy,
        )
            -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        let patterns: &mut DynPatterns<'_, E, C> = &mut patterns;
        let procedures: &mut DynProcedures<'_, E, C> = &mut procedures;
        let mut source = WithProcedures {
            patterns,
            procedures,
        };
        let checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        execute::execute(self, policy, &mut source, checkpoint)
    }

    /// `execute_governed_with_procedures` plus the host's element property
    /// reader (fgdb-xnd7x). `x.key` over a vertex or edge that an expression
    /// produced reads `elements(element, key)` at the same pinned cut and
    /// under the same authorization as `patterns`. Without this entry such a
    /// read stays a typed error.
    pub fn execute_governed_with_elements<E, C>(
        &self,
        policy: GqlQueryPolicy,
        mut patterns: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        )
            -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut procedures: impl FnMut(
            &PreparedProcedureCall,
            &[crate::algebra::GraphValue],
            GqlQueryPolicy,
        )
            -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut elements: impl FnMut(
            fgdb_delta_types::ElementId,
            fgdb_delta_types::PropertyKeyId,
        ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        let patterns: &mut DynPatterns<'_, E, C> = &mut patterns;
        let procedures: &mut DynProcedures<'_, E, C> = &mut procedures;
        let elements: &mut dyn FnMut(
            fgdb_delta_types::ElementId,
            fgdb_delta_types::PropertyKeyId,
        )
            -> Result<Option<fgdb_types::CanonicalPropertyValue>, E> = &mut elements;
        let mut source = WithElementReads {
            source: WithProcedures {
                patterns,
                procedures,
            },
            elements,
        };
        let checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        execute::execute(self, policy, &mut source, checkpoint)
    }

    /// A write tail over its head's owned rows. The tail holds row stages
    /// over one `SetNode::Owned` leaf and never a graph source.
    pub(crate) fn execute_over_owned<E, C>(
        &self,
        rows: Vec<GraphValueRow>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        struct OwnedRows(Option<Vec<GraphValueRow>>);
        impl<E, C> GraphSetSource<E, C> for OwnedRows {
            fn pattern(
                &mut self,
                _: &PreparedGraphPattern<GraphValueRow>,
                _: GqlQueryPolicy,
            ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>> {
                unreachable!("a write tail reads no graph source")
            }
            fn owned(&mut self) -> Option<Vec<GraphValueRow>> {
                self.0.take()
            }
        }
        let checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        execute::execute(self, policy, &mut OwnedRows(Some(rows)), checkpoint)
    }

    /// The executor over any trusted source, for relational consumers (a
    /// grouped aggregate) that forward their caller's procedure host.
    pub(crate) fn execute_with_source<E, C>(
        &self,
        policy: GqlQueryPolicy,
        source: &mut impl GraphSetSource<E, C>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> SetResult<GqlQueryExecution<GraphValueRow>, E, C> {
        let source: &mut dyn GraphSetSource<E, C> = source;
        let checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        execute::execute(self, policy, source, checkpoint)
    }

    /// Whether any source of this relation is a procedure call.
    #[must_use]
    pub fn calls_procedure(&self) -> bool {
        match &self.node {
            SetNode::ProcedureCall(_) => true,
            SetNode::Pattern(_) | SetNode::Values | SetNode::Owned => false,
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

    /// The row-by-row pull of this relation, when it has one (fgdb-7xqxb):
    /// `Project ALL` over one vertex-rooted pattern without relationships,
    /// with no order or page at either level. The first output cell is the
    /// root vertex itself; every other cell is a column, a bound constant or
    /// integer arithmetic over columns.
    ///
    /// The vertex scan visits strictly increasing identities, and each row
    /// leads with its own identity. Its visitation order is therefore exactly
    /// the canonical row order the eager projection sorts into, and no two
    /// rows can tie. Any other shape returns None: there is no pull fallback.
    #[must_use]
    pub fn row_pull(&self) -> Option<GraphSetRowPull> {
        if !self.order.is_empty() || self.offset != 0 || self.count.is_some() {
            return None;
        }
        let (pull, GraphSetQuantifier::All) = self.vertex_projection()? else {
            return None;
        };
        let GraphSetValue::Column(column) = *pull.projection.first()?.value() else {
            return None;
        };
        if !matches!(
            pull.pattern.value_columns().get(column),
            Some(ValueProjection::Vertex { slot }) if slot.ordinal() == 0
        ) {
            return None;
        }
        Some(pull)
    }

    /// The ordered pull of this relation, when it has one (fgdb-7xqxb): the
    /// same checked projection over one vertex-rooted pattern as
    /// [`Self::row_pull`], with this relation's ALL/DISTINCT, ORDER BY and
    /// page returned as the external sort's tail. Without ORDER BY, the
    /// tail specifies the ordinary canonical whole-row order, NULL first.
    ///
    /// The caller projects every scanned row with [`GraphSetRowPull::project`]
    /// and sorts the complete projected bag by the tail. DISTINCT removes
    /// equal complete projected rows only after sorting and before the page;
    /// it never deduplicates source rows or skips a fallible projection.
    /// Every evaluated cell is visible. Ties then break on
    /// the whole typed row, as the eager projection's canonical sort followed
    /// by its stable ORDER BY does, so no output cell needs to lead with the
    /// root. The pattern's own rows must lead with the root vertex, or
    /// project no cell. The scan then numbers source rows exactly as the
    /// eager path numbers its canonically sorted input.
    #[must_use]
    pub fn ordered_row_pull(&self) -> Option<(GraphSetRowPull, crate::scan_stream::ScanSortTail)> {
        let (pull, quantifier) = self.vertex_projection()?;
        let columns = pull.pattern.value_columns();
        if !columns.is_empty()
            && !matches!(
                columns.first(),
                Some(ValueProjection::Vertex { slot }) if slot.ordinal() == 0
            )
        {
            return None;
        }
        let tail = crate::scan_stream::ScanSortTail::for_relation(
            pull.projection.len(),
            self.order.clone(),
            quantifier,
            self.offset,
            self.count,
        )?;
        Some((pull, tail))
    }

    /// A projection over one vertex-rooted, relationship-free pattern with
    /// no inner order or page, whose every cell a pull can evaluate: a column,
    /// a bound constant or integer arithmetic over columns.
    /// The consumer, not this accessor, owns the returned quantifier.
    fn vertex_projection(&self) -> Option<(GraphSetRowPull, GraphSetQuantifier)> {
        let SetNode::Project {
            input,
            projection,
            quantifier,
            preserve_order: false,
        } = &self.node
        else {
            return None;
        };
        if !input.order.is_empty() || input.offset != 0 || input.count.is_some() {
            return None;
        }
        let SetNode::Pattern(pattern) = &input.node else {
            return None;
        };
        let operators = pattern.plan().operators();
        let roots = operators
            .iter()
            .filter(|operator| matches!(operator, crate::algebra::GlaOperator::ScanVertices))
            .count();
        if roots != 1
            || !matches!(
                operators.first(),
                Some(crate::algebra::GlaOperator::ScanVertices)
            )
            || pattern.plan().reads_edges()
        {
            return None;
        }
        if projection.is_empty()
            || !projection.iter().all(|cell| {
                matches!(
                    cell.value(),
                    GraphSetValue::Column(_)
                        | GraphSetValue::Literal(_)
                        | GraphSetValue::Value(_)
                        | GraphSetValue::TypedNull(_)
                        | GraphSetValue::Integer(_)
                )
            })
        {
            return None;
        }
        Some((
            GraphSetRowPull {
                pattern: pattern.clone(),
                projection: projection.clone(),
            },
            *quantifier,
        ))
    }
}

/// A relation that pulls row by row (PreparedGraphSet::row_pull): one
/// vertex-rooted source pattern and the projection each of its rows takes.
#[derive(Clone)]
pub struct GraphSetRowPull {
    pattern: PreparedGraphPattern<GraphValueRow>,
    projection: Vec<GraphSetProjection>,
}

impl core::fmt::Debug for GraphSetRowPull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphSetRowPull")
            .field("columns", &self.projection.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl GraphSetRowPull {
    /// The source pattern whose native cursor supplies the input rows.
    #[must_use]
    pub fn pattern(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.pattern
    }

    /// Project one source row exactly as the eager projection does.
    /// - `ordinal` numbers the source rows from zero.
    /// - Every evaluation event goes to `control`.
    /// - An arithmetic failure becomes `arithmetic(column, error)`, as the
    ///   eager path's Projection error. Its row is the caller's ordinal.
    pub fn project<E>(
        &self,
        row: &GraphValueRow,
        ordinal: u64,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
        arithmetic: impl FnOnce(usize, crate::GraphIntegerError) -> E,
    ) -> Result<GraphValueRow, E> {
        GraphSetProjection::evaluate_numbered_row_with_control(
            row,
            ordinal,
            &self.projection,
            control,
            &mut projection::no_element_reads,
            arithmetic,
        )
    }
}

#[cfg(test)]
mod pulled_projection_tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueOrder};
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{CanonicalScalar, VId};

    fn input(text: &str) -> PreparedGraphSet {
        PreparedGraphSet::from(
            PreparedGraphText::prepare(text, |kind, name: &str| {
                (kind == GraphSymbolKind::Property && name == "p")
                    .then_some(GraphSymbol::Property(PropertyKeyId(1)))
            })
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap(),
        )
    }

    #[test]
    fn relational_sort_tails_pin_quantifiers_windows_and_canonical_null_order() {
        use crate::scan_stream::ScanSortTail;
        for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
            for width in [1, crate::algebra::MAX_PATTERN_VERTICES] {
                for (offset, count) in [(0, None), (0, Some(0)), (u64::MAX, Some(1))] {
                    let tail =
                        ScanSortTail::for_relation(width, Vec::new(), quantifier, offset, count)
                            .unwrap();
                    let expected: Vec<_> = (0..width)
                        .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
                        .collect();
                    assert_eq!(tail.order(), expected);
                    assert_eq!(
                        (tail.evaluation_width(), tail.visible_width()),
                        (width, width)
                    );
                    assert_eq!(tail.distinct(), quantifier == GraphSetQuantifier::Distinct);
                    assert_eq!((tail.offset(), tail.count()), (offset, count));
                }
            }
            let keys = vec![
                GraphValueOrder::descending(1),
                GraphValueOrder::ascending(0),
            ];
            let tail = ScanSortTail::for_relation(2, keys.clone(), quantifier, 1, Some(3)).unwrap();
            assert_eq!(tail.order(), keys);
            for (width, keys) in [
                (0, Vec::new()),
                (crate::algebra::MAX_PATTERN_VERTICES + 1, Vec::new()),
                (2, vec![GraphValueOrder::ascending(2)]),
                (2, vec![GraphValueOrder::ascending(0); 2]),
            ] {
                assert!(ScanSortTail::for_relation(width, keys, quantifier, 0, Some(0)).is_none());
            }
        }
    }

    #[test]
    fn distinct_pull_keeps_every_projected_occurrence_for_the_existing_sorter() {
        let source = input("MATCH (n) RETURN n AS id,n.p AS p");
        let source_bytes = source.incremental_pattern().unwrap().canonical_bytes();
        for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
            let query = source
                .clone()
                .project(
                    vec![
                        GraphSetProjection::new("p", GraphSetValue::Column(1)),
                        GraphSetProjection::new(
                            "marker",
                            GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                        ),
                    ],
                    quantifier,
                )
                .unwrap()
                .with_page(1, Some(2));
            let frozen = query.canonical_bytes();
            let (pull, tail) = query.ordered_row_pull().unwrap();
            assert_eq!(query.canonical_bytes(), frozen);
            assert_eq!(pull.pattern().canonical_bytes(), source_bytes);
            assert_eq!(tail.distinct(), quantifier == GraphSetQuantifier::Distinct);
            assert_eq!((tail.offset(), tail.count()), (1, Some(2)));
            let mut events = 0;
            let mut projected = Vec::new();
            for (ordinal, value) in [9, -4, 9].into_iter().enumerate() {
                let row = GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(ordinal as u128 + 1)),
                    GraphValue::Scalar(CanonicalScalar::Int(value)),
                ]);
                projected.push(
                    pull.project(
                        &row,
                        ordinal as u64,
                        &mut |_| {
                            events += 1;
                            Ok::<_, ()>(())
                        },
                        |_, _| (),
                    )
                    .unwrap(),
                );
            }
            assert!(events > 0);
            assert_eq!(projected.len(), 3);
            assert_eq!(projected[0], projected[2]);
            assert_eq!(
                projected[1].values(),
                &[
                    GraphValue::Scalar(CanonicalScalar::Int(-4)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]
            );
        }
    }

    #[test]
    fn distinct_sort_admission_never_widens_unordered_or_unproved_source_profiles() {
        let source = input("MATCH (n) RETURN n AS id,n.p AS p");
        for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
            let identity = source
                .clone()
                .project(
                    vec![GraphSetProjection::new("id", GraphSetValue::Column(0))],
                    quantifier,
                )
                .unwrap();
            assert_eq!(
                identity.row_pull().is_some(),
                quantifier == GraphSetQuantifier::All
            );
            assert!(identity.ordered_row_pull().is_some());
            for child in [
                source.clone().with_page(0, Some(0)),
                source.clone().with_page(1, None),
                source.clone().nested().unwrap(),
                input("MATCH (n) RETURN n.p AS p,n AS id"),
            ] {
                let query = child
                    .project(
                        vec![GraphSetProjection::new("first", GraphSetValue::Column(0))],
                        quantifier,
                    )
                    .unwrap()
                    .with_page(0, Some(0));
                assert!(query.ordered_row_pull().is_none());
            }
        }
    }
}

#[cfg(test)]
mod column_domain_tests {
    use super::GraphSetColumnType;
    use crate::algebra::GraphValue;
    use fgdb_types::{CanonicalScalar, EId, VId};

    /// fgdb-kr8j1: a property column is Stored. It admits a scalar or a stored
    /// List or Map, nested ones included, and refuses a graph element at any
    /// depth. Scalar keeps its exact shape, so a spilled or copied Scalar
    /// column still refuses a collection.
    #[test]
    fn a_stored_column_admits_stored_collections_and_never_an_element() {
        let int = |value| GraphValue::Scalar(CanonicalScalar::Int(value));
        let list = |values: Vec<GraphValue>| GraphValue::List(values.into_boxed_slice());
        let map = |entries: Vec<(&str, GraphValue)>| {
            GraphValue::map(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.into(), value))
                    .collect(),
            )
            .unwrap()
        };
        let stored = [
            int(1),
            list(vec![]),
            list(vec![int(1), list(vec![int(2)])]),
            map(vec![("a", int(1)), ("b", list(vec![int(2)]))]),
        ];
        for value in &stored {
            assert!(GraphSetColumnType::Stored.accepts(value), "{value:?}");
        }
        for element in [
            GraphValue::Vertex(VId(1)),
            GraphValue::Edge(EId(1)),
            list(vec![GraphValue::Vertex(VId(1))]),
            list(vec![list(vec![GraphValue::Edge(EId(1))])]),
            map(vec![("a", GraphValue::Vertex(VId(1)))]),
        ] {
            assert!(!GraphSetColumnType::Stored.accepts(&element), "{element:?}");
        }
        assert!(GraphSetColumnType::Scalar.accepts(&stored[0]));
        for collection in &stored[1..] {
            assert!(
                !GraphSetColumnType::Scalar.accepts(collection),
                "{collection:?}"
            );
        }
        assert!(!GraphSetColumnType::Vertex.accepts(&list(vec![])));
        assert!(!GraphSetColumnType::List.accepts(&int(1)));
    }

    /// A set operand pairing keeps Scalar strict, widens Scalar with Stored
    /// to Stored, and Stored with List to Any. Elements never pair with it.
    #[test]
    fn set_operands_widen_a_stored_column_only_to_a_domain_holding_both() {
        use GraphSetColumnType as T;
        for (left, right, common) in [
            (T::Scalar, T::Scalar, Some(T::Scalar)),
            (T::Scalar, T::Stored, Some(T::Stored)),
            (T::Stored, T::Scalar, Some(T::Stored)),
            (T::Stored, T::Stored, Some(T::Stored)),
            (T::Stored, T::List, Some(T::Any)),
            (T::List, T::Stored, Some(T::Any)),
            (T::Stored, T::Any, Some(T::Any)),
            (T::Scalar, T::List, None),
            (T::Stored, T::Vertex, None),
            (T::Edge, T::Stored, None),
        ] {
            assert_eq!(left.common_set_domain(right), common, "{left:?} {right:?}");
        }
        assert!(T::Stored.is_scalar_like() && T::Scalar.is_scalar_like());
        assert!(!T::Any.is_scalar_like() && !T::List.is_scalar_like());
        assert_ne!(T::Stored.tag(), T::Scalar.tag());
    }
}
