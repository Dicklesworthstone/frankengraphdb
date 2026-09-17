//! Parse-once exact aggregation after native WITH row stages.
//!
//! This is preparation metadata. Binding emits the existing
//! PreparedGraphAggregate with its real source leaf and relational input.

use crate::set_text::{BoundSetTextInput, ReadFilterOp, ReadPageNumber};
use crate::{
    GqlParameterSpec, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateFunction,
    GraphAggregateOrder, GraphAggregateTextSlot, GraphHavingError, GraphPatternTextError,
    GraphSetTextError, GraphSetTextErrorKind,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphPipelineAggregateTextErrorKind {
    Input(GraphSetTextErrorKind),
    Build(GraphAggregateBuildError),
    Having(GraphHavingError),
}

/// Original UTF-8 byte coordinate; neither source fragments nor parameter
/// payloads are included in diagnostics. Execution retains ordinary aggregate
/// errors rather than converting data failures into parser errors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphPipelineAggregateTextError {
    pub offset: usize,
    pub kind: GraphPipelineAggregateTextErrorKind,
}
impl core::fmt::Display for GraphPipelineAggregateTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "aggregate pipeline at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphPipelineAggregateTextError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.kind {
            GraphPipelineAggregateTextErrorKind::Build(error) => Some(error),
            GraphPipelineAggregateTextErrorKind::Having(error) => Some(error),
            GraphPipelineAggregateTextErrorKind::Input(_) => None,
        }
    }
}
impl From<GraphSetTextError> for GraphPipelineAggregateTextError {
    fn from(error: GraphSetTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphPipelineAggregateTextErrorKind::Input(error.kind),
        }
    }
}
impl From<GraphPatternTextError> for GraphPipelineAggregateTextError {
    fn from(error: GraphPatternTextError) -> Self {
        GraphSetTextError::from(error).into()
    }
}

#[derive(Clone)]
pub(crate) struct PipelineSummary {
    pub(crate) name: String,
    pub(crate) function: GraphAggregateFunction,
    pub(crate) column: Option<usize>,
}

/// Native MATCH ... WITH ... RETURN aggregate [GROUP BY ...] [HAVING ...]
/// [ORDER BY ...] [SKIP ...] [LIMIT ...]. WITH expressions/filters/pages retain
/// their original boundaries. Terminal keys and aggregate arguments refer to
/// completed row aliases, not discarded graph variables or property lookups.
/// Compute complex arguments in a preceding WITH. COUNT(*), COUNT, SUM/SUM_INT,
/// AVG/AVG_INT, MIN, MAX and argument DISTINCT use the existing exact engine.
///
/// GROUP BY is explicit for every returned nonaggregate alias and may include
/// hidden aliases. HAVING is three-valued Boolean over returned aliases and
/// literals/parameters; ORDER BY also uses returned aliases. Keys may be renamed
/// in RETURN. output_slots() maps written column order to GraphAggregateRow's
/// separate key/summary arrays. No cast to a narrower scalar domain occurs.
///
/// One frozen parameter/catalog contract spans the complete statement. Binding
/// neither reparses text nor reopens a catalog. The result executes through all
/// existing Database, historical, pinned-view and WriteTxn aggregate APIs.
/// This bounded single-source profile does not implement aggregate WITH stages,
/// binary set inputs, inline aggregate-argument arithmetic or writes after WITH.
#[derive(Clone)]
pub struct PreparedGraphPipelineAggregateText {
    pub(crate) statement: String,
    pub(crate) input: BoundSetTextInput,
    pub(crate) keys: Vec<usize>,
    pub(crate) output_keys: Vec<usize>,
    pub(crate) summaries: Vec<PipelineSummary>,
    pub(crate) output_distinct: bool,
    pub(crate) names: Vec<String>,
    pub(crate) slots: Vec<GraphAggregateTextSlot>,
    pub(crate) having: Vec<ReadFilterOp>,
    pub(crate) having_columns: Vec<GraphAggregateColumn>,
    pub(crate) ordering: Vec<GraphAggregateOrder>,
    pub(crate) offset: ReadPageNumber,
    pub(crate) count: Option<ReadPageNumber>,
    pub(crate) aggregate_at: usize,
    pub(crate) having_at: usize,
}
impl core::fmt::Debug for PreparedGraphPipelineAggregateText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphPipelineAggregateText")
            .field("columns", &self.names.len())
            .field("parameters", &self.input.parameter_schema().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphPipelineAggregateText {
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.names
    }
    #[must_use]
    pub fn output_slots(&self) -> &[GraphAggregateTextSlot] {
        &self.slots
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.input.parameter_schema()
    }
}
