//! Branch-specific edge-property actions for bounded directed relationship MERGE.
//!
//! The underlying MERGE decides NoInput/Matched/Created once. Match/create
//! branches then apply a finite simultaneous set of canonical edge-property SETs
//! to the chosen relationship. NoInput runs no branch. Relationship deletion,
//! endpoint rewrites and property-valued match identity remain outside this core.

use crate::{GqlScalarParameter, GraphEdgeMergePolicy, PreparedGraphEdgeMerge};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeSet;

pub const MAX_GRAPH_EDGE_UPSERT_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct GraphEdgeUpsertAction {
    pub key: PropertyKeyId,
    pub value: GqlScalarParameter,
}
impl core::fmt::Debug for GraphEdgeUpsertAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphEdgeUpsertAction([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertBranch { NoInput, Match, Create }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertBuildError {
    TooManyActions { branch: GraphEdgeUpsertBranch, limit: usize, observed: usize },
    DuplicateProperty { branch: GraphEdgeUpsertBranch },
}
impl core::fmt::Display for GraphEdgeUpsertBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph relationship MERGE action definition: {self:?}")
    }
}
impl core::error::Error for GraphEdgeUpsertBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeUpsertPolicy {
    /// One query allowance covers MERGE, property copies and final acceptance.
    pub merge: GraphEdgeMergePolicy,
    /// Number of proposals in the selected branch, independent of creation.
    pub max_actions: u64,
}
impl GraphEdgeUpsertPolicy {
    #[must_use]
    pub const fn new(merge: GraphEdgeMergePolicy, max_actions: u64) -> Self {
        Self { merge, max_actions }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeUpsertStats {
    pub merge: crate::GraphEdgeMergeStats,
    pub branch: GraphEdgeUpsertBranch,
    pub action_effects: u64,
    /// Cumulative MERGE plus branch work/scratch. Do not add merge.evaluator
    /// again. Logical entries do not measure allocator or durable I/O costs.
    pub evaluator: crate::GlaExecutionStats,
}

#[derive(Debug)]
pub enum GraphEdgeUpsertError<E, A> {
    Merge(crate::GraphEdgeMergeError<E, A>),
    ActionLimit { limit: u64, observed: u128 },
    Staging(E),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display for GraphEdgeUpsertError<E, A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::ActionLimit { limit, observed } => write!(f, "relationship MERGE branch action limit exceeded: {observed} > {limit}"),
            Self::Staging(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeUpsertError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Merge(error) => Some(error),
            Self::Staging(error) => Some(error),
            Self::ActionLimit { .. } => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeUpsert {
    merge: PreparedGraphEdgeMerge,
    on_match: Vec<GraphEdgeUpsertAction>,
    on_create: Vec<GraphEdgeUpsertAction>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsert")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("definition", &"[REDACTED]").finish()
    }
}
fn validate(branch: GraphEdgeUpsertBranch, actions: &[GraphEdgeUpsertAction])
    -> Result<(), GraphEdgeUpsertBuildError>
{
    if actions.len() > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
        return Err(GraphEdgeUpsertBuildError::TooManyActions {
            branch, limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS, observed: actions.len(),
        });
    }
    let mut keys = BTreeSet::new();
    for action in actions {
        if !keys.insert(action.key) {
            return Err(GraphEdgeUpsertBuildError::DuplicateProperty { branch });
        }
    }
    Ok(())
}
impl PreparedGraphEdgeUpsert {
    pub fn prepare(
        merge: PreparedGraphEdgeMerge,
        on_match: Vec<GraphEdgeUpsertAction>,
        on_create: Vec<GraphEdgeUpsertAction>,
    ) -> Result<Self, GraphEdgeUpsertBuildError> {
        validate(GraphEdgeUpsertBranch::Match, &on_match)?;
        validate(GraphEdgeUpsertBranch::Create, &on_create)?;
        Ok(Self { merge, on_match, on_create })
    }
    #[must_use] pub fn merge(&self) -> &PreparedGraphEdgeMerge { &self.merge }
    #[must_use] pub fn on_match(&self) -> &[GraphEdgeUpsertAction] { &self.on_match }
    #[must_use] pub fn on_create(&self) -> &[GraphEdgeUpsertAction] { &self.on_create }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:directed-edge-upsert:v1\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        for actions in [&self.on_match, &self.on_create] {
            bytes.extend_from_slice(&(actions.len() as u64).to_be_bytes());
            for action in actions {
                bytes.extend_from_slice(&action.key.0.to_be_bytes());
                bytes.extend_from_slice(&(action.value.canonical_bytes().len() as u64).to_be_bytes());
                bytes.extend_from_slice(action.value.canonical_bytes());
            }
        }
        bytes
    }
}
