//! Bounded GQL parsing, binding, preparation, and evidence vocabulary.
//!
//! [`PreparedGqlQuery`] is the coherent reusable form: exact statement bytes,
//! the canonical caller-supplied [`RelationBind`], and the executor-ready
//! [`BoundPlan`] are created together and cannot be mutated independently.

#![forbid(unsafe_code)]

mod evidence_artifact;
mod overlay_evidence;
mod parser;
mod prepared;

pub use evidence_artifact::{
    GqlEvidenceArtifactKind, GqlEvidenceAuditError, GqlEvidenceDecodeError,
    GqlOverlayResultArtifact, GqlPreparedResultArtifact,
};
pub use overlay_evidence::GqlOverlayResultCertificate;
pub use parser::{
    BindError, BoundPlan, EdgeDirection, ParseError, ParseErrorKind, RelationBind,
    ReturnProjection,
};
pub use prepared::{
    BudgetedGqlError, BudgetedGqlExecution, GqlBudgetDimension, GqlBudgetExceeded,
    GqlExecutionBudget, GqlExecutionStats, PreparedGqlQuery,
};
