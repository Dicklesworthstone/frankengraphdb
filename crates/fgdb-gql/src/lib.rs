//! Bounded GQL parsing, binding, preparation, and evidence vocabulary.
//!
//! [`PreparedGqlQuery`] owns one coherent statement, bind map and bound plan.

#![forbid(unsafe_code)]

mod aggregation;
pub mod algebra;
mod algebra_exec;
mod deletion;
mod edge_merge;
mod evidence_artifact;
mod evidence_cursor;
mod evidence_limits;
mod evidence_page;
mod graph_text;
pub mod insertion;
mod insertion_text;
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
mod shortest_walk;
mod temporal_aggregate_text;
mod temporal_set_text;
mod temporal_text;
mod vertex_merge;
mod vertex_upsert;
mod vertex_upsert_text;
mod walk;
mod write_receipt;

pub use vertex_upsert_text::{
    GraphVertexUpsertTextError, GraphVertexUpsertTextErrorKind, PreparedGraphVertexUpsertText,
};
pub use vertex_upsert::{
    GraphVertexUpsertAction, GraphVertexUpsertBranch, GraphVertexUpsertBuildError,
    GraphVertexUpsertError, GraphVertexUpsertPolicy, GraphVertexUpsertStats,
    MAX_GRAPH_VERTEX_UPSERT_ACTIONS, PreparedGraphVertexUpsert,
};
pub use edge_merge::{
    GraphEdgeMergeBuildError, GraphEdgeMergeError, GraphEdgeMergeOutcome,
    GraphEdgeMergePolicy, GraphEdgeMergeRequest, GraphEdgeMergeStats,
    MAX_GRAPH_EDGE_MERGE_PROPERTIES, PreparedGraphEdgeMerge,
};
pub use vertex_merge::{
    GraphVertexMergeBuildError, GraphVertexMergeError, GraphVertexMergeOutcome,
    GraphVertexMergePolicy, GraphVertexMergeStats, PreparedGraphVertexMerge,
};
pub use deletion::{
    GraphDeleteBuildError, GraphDeleteError, GraphDeletePolicy, GraphDeleteProposal,
    GraphDeleteStats, MAX_GRAPH_DELETE_TARGETS, PreparedGraphDelete,
};
pub use temporal_set_text::{
    BoundTemporalGraphSetQuery, GraphTemporalSetTextError, GraphTemporalSetTextErrorKind,
    PreparedTemporalGraphSetText,
};
pub use temporal_aggregate_text::{
    BoundTemporalGraphAggregateQuery, PreparedTemporalGraphAggregateText,
};
pub use temporal_text::{
    BoundTemporalGraphQuery, GraphTemporalTextError, GraphTemporalTextErrorKind,
    PreparedTemporalGraphText,
};
pub use shortest_walk::GraphShortestWalkCursor;
pub use write_receipt::{GraphWriteProgramReceipt, GraphWriteStepReceipt};
pub use mutation_program_template::mixed::{
    GraphWriteProgramTemplateError, GraphWriteTemplateStatement, PreparedGraphWriteProgramTemplate,
};
pub use mutation_program::mixed::{
    GraphWriteIdentityRequest, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteProgramStats, GraphWriteStatement, GraphWriteStepError, GraphWriteStepStats,
    PreparedGraphWriteProgram,
};
pub use insertion_text::{GraphInsertTextError, GraphInsertTextErrorKind, PreparedGraphInsertText};
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
    GraphDeleteTextError, GraphDeleteTextErrorKind, GraphEdgeMergeTextError,
    GraphEdgeMergeTextErrorKind, GraphMutationTextError, GraphMutationTextErrorKind,
    GraphVertexMergeTextError, GraphVertexMergeTextErrorKind, PreparedGraphDeleteText,
    PreparedGraphEdgeMergeText, PreparedGraphMutationText, PreparedGraphVertexMergeText,
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
