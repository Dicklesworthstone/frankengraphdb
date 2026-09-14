//! Bounded GQL parsing, binding, preparation, and evidence vocabulary.
//!
//! [`PreparedGqlQuery`] owns one coherent statement, bind map and bound plan.

#![forbid(unsafe_code)]

mod aggregation;
pub mod algebra;
mod algebra_exec;
mod evidence_artifact;
mod evidence_cursor;
mod evidence_limits;
mod evidence_page;
mod graph_text;
mod integer_expression;
mod mutation;
mod mutation_program;
mod mutation_program_template;
mod mutation_text;
mod overlay_evidence;
mod parameters;
mod parser;
mod prepared;
mod set_ops;
mod set_text;
mod walk;

pub use mutation_program_template::{
    GraphMutationProgramTemplateError, PreparedGraphMutationProgramTemplate,
};
pub use mutation_program::{
    GraphMutationProgramBuildError, GraphMutationProgramDimension, GraphMutationProgramError,
    GraphMutationProgramStats, MAX_GRAPH_MUTATION_STATEMENTS, PreparedGraphMutationProgram,
};
pub use aggregation::{
    GraphAggregate, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateError,
    GraphAggregateFilter, GraphAggregateFunction, GraphAggregateOrder, GraphAggregateRow,
    GraphAggregateTest, GraphAggregateValue, GraphExactAverage, GraphHavingError,
    GraphHavingExpression, GraphHavingOp, GraphHavingOperand, GraphNullPlacement,
    MAX_AGGREGATE_FILTERS, MAX_HAVING_INSTRUCTIONS, PreparedGraphAggregate,
};
pub use algebra_exec::{
    GlaExecution, GlaExecutionError, GlaExecutionEvent, GlaExecutionLimits, GlaExecutionStats,
    GlaLimitDimension, GlaLimitExceeded, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
};
pub use evidence_artifact::{
    GqlEvidenceArtifactKind, GqlEvidenceAuditError, GqlEvidenceDecodeError,
    GqlOverlayResultArtifact, GqlPreparedResultArtifact,
};
pub use evidence_cursor::{
    GqlEvidenceCursor, GqlEvidenceCursorError, GqlEvidenceCursorLimitDimension,
    GqlEvidenceCursorLimitExceeded, GqlEvidenceCursorLimits, GqlEvidenceCursorState,
};
pub use evidence_limits::{
    GqlEvidenceLimitDimension, GqlEvidenceLimitExceeded, GqlEvidenceLimitedAuditError,
    GqlEvidenceLimitedDecodeError, GqlEvidenceLimits,
};
pub use evidence_page::{
    GQL_EVIDENCE_PAGE_TOKEN_LEN, GqlEvidencePage, GqlEvidencePageAuditError, GqlEvidencePageError,
    GqlEvidencePageToken, GqlEvidencePageTokenDecodeError,
};
pub use graph_text::{
    GraphAggregateTextSlot, GraphPatternTextError, GraphPatternTextErrorKind, GraphSymbol,
    GraphSymbolKind, MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_TEXT_TOKENS, PreparedGraphAggregateText,
    PreparedGraphText,
};
pub use integer_expression::{
    GraphIntegerBinary, GraphIntegerBuildError, GraphIntegerError, GraphIntegerErrorKind,
    GraphIntegerEvaluationError, GraphIntegerExpression, GraphIntegerOp, GraphIntegerUnary,
    MAX_GRAPH_INTEGER_INSTRUCTIONS,
};
pub use mutation::{
    GraphMutationAction, GraphMutationBatch, GraphMutationBuildError, GraphMutationError,
    GraphMutationIntent, GraphMutationPolicy, GraphMutationStats, GraphMutationValue,
    MAX_GRAPH_MUTATION_ACTIONS, PreparedGraphMutation,
};
pub use mutation_text::{
    GraphMutationTextError, GraphMutationTextErrorKind, PreparedGraphMutationText,
};
pub use overlay_evidence::GqlOverlayResultCertificate;
pub use parameters::{
    GqlParameterError, GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters,
    GqlScalarParameter, PreparedGqlTemplate,
};
pub use parser::{
    BindError, BoundPlan, EdgeDirection, ParseError, ParseErrorKind, RelationBind, ReturnProjection,
};
pub use prepared::{
    BudgetedGqlError, BudgetedGqlExecution, GqlBudgetDimension, GqlBudgetExceeded,
    GqlExecutionBudget, GqlExecutionStats, PreparedGqlQuery,
};
pub use set_ops::{
    GraphSetBuildError, GraphSetColumnType, GraphSetExecutionError, GraphSetOperation,
    GraphSetProjection, GraphSetProjectionError, GraphSetQuantifier, GraphSetValue,
    MAX_GRAPH_SET_DEPTH, MAX_GRAPH_SET_OPERANDS, PreparedGraphSet,
};
pub use set_text::{GraphSetTextError, GraphSetTextErrorKind, PreparedGraphSetText};
pub use walk::{GraphWalkBounds, GraphWalkBoundsError, GraphWalkCursor, MAX_GRAPH_WALK_HOPS};
