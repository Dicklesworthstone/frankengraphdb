//! Bounded GQL parsing, binding, preparation, and evidence vocabulary.
//!
//! [`PreparedGqlQuery`] is the coherent reusable form: exact statement bytes,
//! the canonical caller-supplied [`RelationBind`], and the executor-ready
//! [`BoundPlan`] are created together and cannot be mutated independently.

#![forbid(unsafe_code)]

mod evidence_artifact;
mod evidence_limits;
mod evidence_page;
mod overlay_evidence;
mod parser;
mod prepared;

pub use evidence_artifact::{
    GqlEvidenceArtifactKind, GqlEvidenceAuditError, GqlEvidenceDecodeError,
    GqlOverlayResultArtifact, GqlPreparedResultArtifact,
};
pub use evidence_limits::{
    GqlEvidenceLimitDimension, GqlEvidenceLimitExceeded,
    GqlEvidenceLimitedAuditError, GqlEvidenceLimitedDecodeError,
    GqlEvidenceLimits,
};
pub use evidence_page::{
    GQL_EVIDENCE_PAGE_TOKEN_LEN, GqlEvidencePage, GqlEvidencePageAuditError,
    GqlEvidencePageError, GqlEvidencePageToken,
    GqlEvidencePageTokenDecodeError,
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
