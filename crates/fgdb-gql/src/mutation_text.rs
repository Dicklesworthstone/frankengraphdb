//! Immutable text-prepared query-selected mutations. The native MATCH parser
//! owns construction and binding; this facade cannot be executed as a read.

use crate::{
    GraphMutationAction, GraphMutationBuildError, GraphPatternTextError, GraphPatternTextErrorKind,
    PreparedGraphText,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};

#[derive(Clone)]
pub(crate) enum MutationActionTemplate {
    Bound(GraphMutationAction),
    ParameterProperty {
        target: usize,
        key: PropertyKeyId,
        parameter: usize,
    },
}

/// MATCH [WALK] ... SET / REMOVE / DETACH DELETE, with one parameter schema and
/// catalog pass. The retained native selection is private: its generated value
/// projection is compiler metadata, never a synthesized RETURN query string.
/// Its original statement bytes are retained only for export and diagnostics.
/// Actions are simultaneous over one pre-statement overlay, not sequential
/// expressions observing preceding assignments. Mutations return proposal
/// statistics through WriteTxn; RETURN and write-returning delivery are absent.
#[derive(Clone)]
pub struct PreparedGraphMutationText {
    pub(crate) selection: PreparedGraphText,
    pub(crate) relation: RelationId,
    pub(crate) actions: Vec<MutationActionTemplate>,
}
impl core::fmt::Debug for PreparedGraphMutationText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationText")
            .field("actions", &self.actions.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMutationTextError {
    pub offset: usize,
    pub kind: GraphMutationTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationTextErrorKind {
    Query(GraphPatternTextErrorKind),
    Build(GraphMutationBuildError),
}
impl From<GraphPatternTextError> for GraphMutationTextError {
    fn from(error: GraphPatternTextError) -> Self {
        Self {
            offset: error.offset,
            kind: GraphMutationTextErrorKind::Query(error.kind),
        }
    }
}
impl core::fmt::Display for GraphMutationTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "graph mutation text error at byte {}: {:?}",
            self.offset, self.kind
        )
    }
}
impl core::error::Error for GraphMutationTextError {}
