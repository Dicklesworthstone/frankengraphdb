//! Branch-specific actions for bounded unique-vertex MERGE.
//!
//! The underlying MERGE decides Matched versus Created exactly once. One branch
//! then applies a finite simultaneous set of literal property/label assignments
//! to that chosen vertex. There is no sequential assignment visibility, REMOVE,
//! relationship action or expression over the chosen row in this increment.

use crate::{GqlScalarParameter, GraphVertexMergePolicy, PreparedGraphVertexMerge};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use std::collections::BTreeSet;

pub const MAX_GRAPH_VERTEX_UPSERT_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub enum GraphVertexUpsertAction {
    SetProperty {
        key: PropertyKeyId,
        value: GqlScalarParameter,
    },
    SetLabel {
        label: LabelId,
        present: bool,
    },
}
impl core::fmt::Debug for GraphVertexUpsertAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SetProperty { .. } => f.write_str("SetProperty([REDACTED])"),
            Self::SetLabel { present, .. } => f
                .debug_struct("SetLabel")
                .field("present", present)
                .field("definition", &"[REDACTED]")
                .finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexUpsertBuildError {
    TooManyActions {
        branch: GraphVertexUpsertBranch,
        limit: usize,
        observed: usize,
    },
    DuplicateProperty {
        branch: GraphVertexUpsertBranch,
    },
    DuplicateLabel {
        branch: GraphVertexUpsertBranch,
    },
}
impl core::fmt::Display for GraphVertexUpsertBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph vertex MERGE action definition: {self:?}")
    }
}
impl core::error::Error for GraphVertexUpsertBuildError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphVertexUpsertBranch {
    Match,
    Create,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexUpsertPolicy {
    pub merge: GraphVertexMergePolicy,
    /// Branch action proposals, independent of the create vertex itself.
    pub max_actions: u64,
}
impl GraphVertexUpsertPolicy {
    #[must_use]
    pub const fn new(merge: GraphVertexMergePolicy, max_actions: u64) -> Self {
        Self { merge, max_actions }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphVertexUpsertStats {
    pub merge: crate::GraphVertexMergeStats,
    pub branch: GraphVertexUpsertBranch,
    pub action_effects: u64,
}

#[derive(Debug)]
pub enum GraphVertexUpsertError<E, A> {
    Merge(crate::GraphVertexMergeError<E, A>),
    ActionLimit { limit: u64, observed: u128 },
    Staging(E),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphVertexUpsertError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::ActionLimit { limit, observed } => write!(
                f,
                "MERGE branch action limit exceeded: {observed} > {limit}"
            ),
            Self::Staging(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphVertexUpsertError<E, A>
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
pub struct PreparedGraphVertexUpsert {
    merge: PreparedGraphVertexMerge,
    on_match: Vec<GraphVertexUpsertAction>,
    on_create: Vec<GraphVertexUpsertAction>,
}
impl core::fmt::Debug for PreparedGraphVertexUpsert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsert")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

fn validate_branch(
    branch: GraphVertexUpsertBranch,
    actions: &[GraphVertexUpsertAction],
) -> Result<(), GraphVertexUpsertBuildError> {
    if actions.len() > MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
        return Err(GraphVertexUpsertBuildError::TooManyActions {
            branch,
            limit: MAX_GRAPH_VERTEX_UPSERT_ACTIONS,
            observed: actions.len(),
        });
    }
    let mut properties = BTreeSet::new();
    let mut labels = BTreeSet::new();
    for action in actions {
        match action {
            GraphVertexUpsertAction::SetProperty { key, .. } if !properties.insert(*key) => {
                return Err(GraphVertexUpsertBuildError::DuplicateProperty { branch });
            }
            GraphVertexUpsertAction::SetLabel { label, .. } if !labels.insert(*label) => {
                return Err(GraphVertexUpsertBuildError::DuplicateLabel { branch });
            }
            _ => {}
        }
    }
    Ok(())
}

impl PreparedGraphVertexUpsert {
    pub fn prepare(
        merge: PreparedGraphVertexMerge,
        on_match: Vec<GraphVertexUpsertAction>,
        on_create: Vec<GraphVertexUpsertAction>,
    ) -> Result<Self, GraphVertexUpsertBuildError> {
        validate_branch(GraphVertexUpsertBranch::Match, &on_match)?;
        validate_branch(GraphVertexUpsertBranch::Create, &on_create)?;
        Ok(Self {
            merge,
            on_match,
            on_create,
        })
    }

    #[must_use]
    pub fn merge(&self) -> &PreparedGraphVertexMerge {
        &self.merge
    }
    #[must_use]
    pub fn on_match(&self) -> &[GraphVertexUpsertAction] {
        &self.on_match
    }
    #[must_use]
    pub fn on_create(&self) -> &[GraphVertexUpsertAction] {
        &self.on_create
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-upsert:v1\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        for actions in [&self.on_match, &self.on_create] {
            bytes.extend_from_slice(&(actions.len() as u64).to_be_bytes());
            for action in actions {
                match action {
                    GraphVertexUpsertAction::SetProperty { key, value } => {
                        bytes.push(0);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                        bytes.extend_from_slice(
                            &(value.canonical_bytes().len() as u64).to_be_bytes(),
                        );
                        bytes.extend_from_slice(value.canonical_bytes());
                    }
                    GraphVertexUpsertAction::SetLabel { label, present } => {
                        bytes.push(1);
                        bytes.extend_from_slice(&label.0.to_be_bytes());
                        bytes.push(u8::from(*present));
                    }
                }
            }
        }
        bytes
    }
}
