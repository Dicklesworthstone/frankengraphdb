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
mod overlay_evidence;
mod parameters;
mod parser;
mod prepared;

pub use aggregation::{
    GraphAggregate, GraphAggregateBuildError, GraphAggregateColumn, GraphAggregateError,
    GraphAggregateFilter, GraphAggregateFunction, GraphAggregateOrder, GraphAggregateRow,
    GraphAggregateTest, GraphAggregateValue, GraphNullPlacement, MAX_AGGREGATE_FILTERS,
    GraphHavingError, GraphHavingExpression, GraphHavingOp, GraphHavingOperand,
    MAX_HAVING_INSTRUCTIONS,
    PreparedGraphAggregate,
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
