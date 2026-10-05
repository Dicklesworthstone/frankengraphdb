//! Branch-specific actions for bounded unique-vertex MERGE.
//!
//! MERGE chooses one vertex and one branch. Each SET clause evaluates all of
//! its right-hand sides before staging any of them; a trailing SET is a second
//! clause and sees the selected ON clause's canonical staged effects. Scalar
//! expressions use the ordinary checked bytecode, never a second interpreter.

use crate::{
    GqlScalarParameter, GraphIntegerExpression, GraphVertexMergePolicy, PreparedGraphVertexMerge,
};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use std::collections::BTreeSet;

pub const MAX_GRAPH_VERTEX_UPSERT_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub enum GraphVertexUpsertAction {
    SetProperty {
        key: PropertyKeyId,
        value: GqlScalarParameter,
    },
    /// Columns address `properties` in order, on the chosen vertex before
    /// this clause. Missing/masked fields are NULL. No ambient row or local
    /// comprehension binding is admitted. The entire input is frozen before
    /// evaluating this action, as on ordinary query-selected SET.
    SetExpression {
        key: PropertyKeyId,
        properties: Vec<PropertyKeyId>,
        value: GraphIntegerExpression,
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
            Self::SetExpression { .. } => f.write_str("SetExpression([REDACTED])"),
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
    /// A column is out of range, an input frame is oversized, or an
    /// expression refers to an ambient local instead of the chosen vertex.
    ExpressionInputs {
        branch: GraphVertexUpsertBranch,
        action: usize,
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
    /// All selected ON and trailing SET proposals, including overwritten ones.
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
    /// Match-source statistics remain the original MERGE statistics. Evaluator
    /// usage is cumulative: MERGE plus action input lookup, scalar bytecode,
    /// payload reservation and proposal production. Reading fields of the
    /// already selected vertex does not introduce another source/result row.
    pub merge: crate::GraphVertexMergeStats,
    pub branch: GraphVertexUpsertBranch,
    pub action_effects: u64,
}

#[derive(Debug)]
pub enum GraphVertexUpsertError<E, A> {
    Merge(crate::GraphVertexMergeError<E, A>),
    ActionLimit {
        limit: u64,
        observed: u128,
    },
    /// Clause 0 is the selected ON branch; clause 1 is the trailing SET.
    Expression {
        clause: usize,
        action: usize,
        source: crate::GraphIntegerError,
    },
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
            Self::Expression {
                clause,
                action,
                source,
            } => {
                write!(f, "MERGE clause {clause} action {action}: {source}")
            }
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
            Self::Expression { source, .. } => Some(source),
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
    after: Vec<GraphVertexUpsertAction>,
}
impl core::fmt::Debug for PreparedGraphVertexUpsert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsert")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("after", &self.after.len())
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
    for (at, action) in actions.iter().enumerate() {
        match action {
            GraphVertexUpsertAction::SetProperty { key, .. }
            | GraphVertexUpsertAction::SetExpression { key, .. } => {
                if !properties.insert(*key) {
                    return Err(GraphVertexUpsertBuildError::DuplicateProperty { branch });
                }
                if let GraphVertexUpsertAction::SetExpression {
                    properties, value, ..
                } = action
                    && (properties.len() > MAX_GRAPH_VERTEX_UPSERT_ACTIONS
                        || value
                            .referenced_columns()
                            .any(|column| column >= properties.len())
                        || value.referenced_locals().next().is_some())
                {
                    return Err(GraphVertexUpsertBuildError::ExpressionInputs {
                        branch,
                        action: at,
                    });
                }
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
        Self::prepare_with_trailing_actions(merge, on_match, on_create, Vec::new())
    }

    /// Keep the common SET as a separate clause. Never erase an earlier
    /// assignment merely because its target is overwritten: its expression
    /// may fail and its original write still requires authorization.
    pub fn prepare_with_trailing_actions(
        merge: PreparedGraphVertexMerge,
        on_match: Vec<GraphVertexUpsertAction>,
        on_create: Vec<GraphVertexUpsertAction>,
        after: Vec<GraphVertexUpsertAction>,
    ) -> Result<Self, GraphVertexUpsertBuildError> {
        for (branch, actions) in [
            (GraphVertexUpsertBranch::Match, &on_match),
            (GraphVertexUpsertBranch::Create, &on_create),
        ] {
            validate_branch(branch, actions)?;
            validate_branch(branch, &after)?;
            let observed = actions.len() + after.len();
            if observed > MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
                return Err(GraphVertexUpsertBuildError::TooManyActions {
                    branch,
                    limit: MAX_GRAPH_VERTEX_UPSERT_ACTIONS,
                    observed,
                });
            }
        }
        Ok(Self {
            merge,
            on_match,
            on_create,
            after,
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
    pub fn after(&self) -> &[GraphVertexUpsertAction] {
        &self.after
    }

    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-upsert:v2\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        for actions in [&self.on_match, &self.on_create, &self.after] {
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
                    GraphVertexUpsertAction::SetExpression {
                        key,
                        properties,
                        value,
                    } => {
                        bytes.push(2);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                        bytes.extend_from_slice(&(properties.len() as u64).to_be_bytes());
                        for property in properties {
                            bytes.extend_from_slice(&property.0.to_be_bytes());
                        }
                        let expression = value.canonical_bytes();
                        bytes.extend_from_slice(&(expression.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(&expression);
                    }
                }
            }
        }
        bytes
    }
}
