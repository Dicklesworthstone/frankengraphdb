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

mod query;
pub use query::{
    GraphVertexReturnBinding, GraphVertexUpsertQueryBuildError, PreparedGraphVertexUpsertQuery,
};

pub const MAX_GRAPH_VERTEX_UPSERT_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub enum GraphVertexUpsertAction {
    SetProperty {
        key: PropertyKeyId,
        value: GqlScalarParameter,
    },
    /// A constant List value (fgdb-k3nub): a literal or a List parameter,
    /// evaluated once when the statement binds and written as it is.
    SetStored {
        key: PropertyKeyId,
        value: fgdb_types::CanonicalPropertyValue,
    },
    /// Unset the chosen vertex's currently visible properties except `keep`.
    /// The kept values are ordinary assignments in this same simultaneous
    /// clause. Keys are strictly increasing; the host reads them at the
    /// clause's input cut and retains a whole-element conflict witness.
    /// Every removed property consumes one action, just like a NULL write.
    ClearProperties {
        keep: Vec<PropertyKeyId>,
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
            Self::SetStored { .. } => f.write_str("SetStored([REDACTED])"),
            Self::ClearProperties { .. } => f.write_str("ClearProperties([REDACTED])"),
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
    PropertyReplacement {
        branch: GraphVertexUpsertBranch,
        action: usize,
    },
}
impl core::fmt::Display for GraphVertexUpsertBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyActions {
                branch,
                limit,
                observed,
            } => write!(f, "{branch} has {observed} actions, limit {limit}"),
            Self::DuplicateProperty { branch } => {
                write!(f, "{branch} sets the same property twice")
            }
            Self::DuplicateLabel { branch } => write!(f, "{branch} sets the same label twice"),
            Self::ExpressionInputs { branch, action } => write!(
                f,
                "{branch} action {action} reads a value other than the merged vertex's"
            ),
            Self::PropertyReplacement { branch, action } => write!(
                f,
                "{branch} action {action} has an invalid or repeated property replacement"
            ),
        }
    }
}
impl core::fmt::Display for GraphVertexUpsertBranch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Match => "ON MATCH",
            Self::Create => "ON CREATE",
        })
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
    /// Original property unsets expanded from ClearProperties, included in
    /// action_effects. Static assignments retain their exact declared count.
    pub property_removals: u64,
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
    /// The MERGE RETURN projection over the chosen vertex failed.
    Returning(crate::GraphSetExecutionError<E>),
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
            Self::Returning(error) => write!(f, "MERGE RETURN: {error}"),
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
            Self::Returning(error) => Some(error),
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
    let mut replacement = None;
    for (at, action) in actions.iter().enumerate() {
        match action {
            GraphVertexUpsertAction::SetProperty { key, .. }
            | GraphVertexUpsertAction::SetStored { key, .. }
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
            GraphVertexUpsertAction::ClearProperties { keep } => {
                if replacement.is_some()
                    || keep.len() > MAX_GRAPH_VERTEX_UPSERT_ACTIONS
                    || keep.windows(2).any(|keys| keys[0] >= keys[1])
                {
                    return Err(GraphVertexUpsertBuildError::PropertyReplacement {
                        branch,
                        action: at,
                    });
                }
                replacement = Some((at, keep));
            }
            _ => {}
        }
    }
    if let Some((action, keep)) = replacement
        && properties
            .iter()
            .any(|key| keep.binary_search(key).is_err())
    {
        // A simultaneous clause cannot both assign and clear one key. The
        // parser retains every assigned key, and typed callers obey the same
        // rule instead of making effects depend on their action-vector order.
        return Err(GraphVertexUpsertBuildError::PropertyReplacement { branch, action });
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

    /// Static assignments remain an exact count. A replacement additionally
    /// reports the original unsets it expanded; they are legal only when the
    /// selected clauses contain a replacement and must have been metered.
    pub(crate) fn validates_action_effects(
        &self,
        stats: &GraphVertexUpsertStats,
        targets: u64,
    ) -> bool {
        let selected = match stats.branch {
            GraphVertexUpsertBranch::Match => self.on_match(),
            GraphVertexUpsertBranch::Create => self.on_create(),
        };
        let mut fixed = 0_u128;
        let mut replacement = false;
        for action in selected.iter().chain(self.after()) {
            if matches!(action, GraphVertexUpsertAction::ClearProperties { .. }) {
                replacement = true;
            } else {
                fixed += 1;
            }
        }
        let removals = stats.property_removals;
        (removals == 0 || replacement)
            && removals <= stats.merge.evaluator.work_units
            && removals <= stats.merge.evaluator.scratch_entries
            && u128::from(stats.action_effects)
                == u128::from(targets) * fixed + u128::from(removals)
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
                    GraphVertexUpsertAction::SetStored { key, value } => {
                        bytes.push(3);
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                        // Binding admitted the value through the stored
                        // format's bounds, so it encodes; tag 4 marks the
                        // impossible failure instead of a panic.
                        match value.encode() {
                            Ok(encoded) => {
                                bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                                bytes.extend_from_slice(&encoded);
                            }
                            Err(_) => bytes.push(4),
                        }
                    }
                    GraphVertexUpsertAction::ClearProperties { keep } => {
                        bytes.push(5);
                        bytes.extend_from_slice(&(keep.len() as u64).to_be_bytes());
                        for key in keep {
                            bytes.extend_from_slice(&key.0.to_be_bytes());
                        }
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
