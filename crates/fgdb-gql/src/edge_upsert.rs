//! Branch-specific edge-property actions for bounded directed relationship MERGE.
//!
//! The underlying MERGE decides NoInput/Matched/Created once. Match/create
//! branches apply simultaneous SETs, then a distinct trailing SET clause reads
//! the preceding clause's staged effects. NoInput evaluates nothing, including
//! trailing expressions. Relationship deletion,
//! endpoint rewrites and property-valued match identity remain outside this core.

use crate::{
    GqlScalarParameter, GraphEdgeMergePolicy, GraphIntegerExpression, PreparedGraphEdgeMerge,
};
use fgdb_delta_types::PropertyKeyId;
use std::collections::BTreeSet;

pub const MAX_GRAPH_EDGE_UPSERT_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct GraphEdgeUpsertAction<Value = GqlScalarParameter> {
    pub key: PropertyKeyId,
    pub value: Value,
}
impl<Value> core::fmt::Debug for GraphEdgeUpsertAction<Value> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphEdgeUpsertAction([REDACTED])")
    }
}

/// A checked scalar or bytecode over explicit properties of the chosen edge.
/// Input positions address `properties`, never an ambient MATCH binding. Each
/// action evaluates against its clause's frozen native overlay. No graph row,
/// parameter map or execution authority is retained by this definition.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphEdgeUpsertValue {
    Literal(GqlScalarParameter),
    Expression {
        properties: Vec<PropertyKeyId>,
        value: GraphIntegerExpression,
    },
}
impl core::fmt::Debug for GraphEdgeUpsertValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphEdgeUpsertValue([REDACTED])")
    }
}
impl GraphEdgeUpsertValue {
    #[must_use]
    pub fn literal(&self) -> Option<&GqlScalarParameter> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Expression { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertBranch {
    NoInput,
    Match,
    Create,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertBuildError {
    TooManyActions {
        branch: GraphEdgeUpsertBranch,
        limit: usize,
        observed: usize,
    },
    DuplicateProperty {
        branch: GraphEdgeUpsertBranch,
    },
    TooManyInputs {
        branch: GraphEdgeUpsertBranch,
        action: usize,
        limit: usize,
        observed: usize,
    },
    InvalidExpressionInput {
        branch: GraphEdgeUpsertBranch,
        action: usize,
    },
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
    /// Selected ON plus trailing SET proposals, including overwritten fields.
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
    ActionLimit {
        limit: u64,
        observed: u128,
    },
    Staging(E),
    Expression {
        clause: usize,
        action: usize,
        source: crate::GraphIntegerError,
    },
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeUpsertError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::ActionLimit { limit, observed } => write!(
                f,
                "relationship MERGE branch action limit exceeded: {observed} > {limit}"
            ),
            Self::Staging(error) => error.fmt(f),
            Self::Expression {
                clause,
                action,
                source,
            } => {
                write!(
                    f,
                    "relationship MERGE clause {clause} action {action}: {source}"
                )
            }
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
            Self::Expression { source, .. } => Some(source),
            Self::ActionLimit { .. } => None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeUpsert {
    merge: PreparedGraphEdgeMerge,
    on_match: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
    on_create: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
    after: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsert")
            .field("on_match", &self.on_match.len())
            .field("on_create", &self.on_create.len())
            .field("after", &self.after.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
fn validate(
    branch: GraphEdgeUpsertBranch,
    actions: &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>],
) -> Result<(), GraphEdgeUpsertBuildError> {
    if actions.len() > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
        return Err(GraphEdgeUpsertBuildError::TooManyActions {
            branch,
            limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
            observed: actions.len(),
        });
    }
    let mut keys = BTreeSet::new();
    for (index, action) in actions.iter().enumerate() {
        if !keys.insert(action.key) {
            return Err(GraphEdgeUpsertBuildError::DuplicateProperty { branch });
        }
        if let GraphEdgeUpsertValue::Expression { properties, value } = &action.value {
            if properties.len() > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Err(GraphEdgeUpsertBuildError::TooManyInputs {
                    branch,
                    action: index,
                    limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                    observed: properties.len(),
                });
            }
            if value
                .referenced_columns()
                .any(|column| column >= properties.len())
                || value.referenced_locals().next().is_some()
            {
                return Err(GraphEdgeUpsertBuildError::InvalidExpressionInput {
                    branch,
                    action: index,
                });
            }
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
        let literals = |actions: Vec<GraphEdgeUpsertAction>| {
            actions
                .into_iter()
                .map(|action| GraphEdgeUpsertAction {
                    key: action.key,
                    value: GraphEdgeUpsertValue::Literal(action.value),
                })
                .collect()
        };
        Self::prepare_with_clauses(merge, literals(on_match), literals(on_create), Vec::new())
    }

    /// Compile two alternative ON clauses and one subsequent SET clause.
    /// Duplicate fields within a clause refuse; overwrites BETWEEN clauses are
    /// retained as distinct writes. Validate every branch, even an unused one.
    pub fn prepare_with_clauses(
        merge: PreparedGraphEdgeMerge,
        on_match: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
        on_create: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
        after: Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
    ) -> Result<Self, GraphEdgeUpsertBuildError> {
        for (branch, actions) in [
            (GraphEdgeUpsertBranch::Match, &on_match),
            (GraphEdgeUpsertBranch::Create, &on_create),
        ] {
            validate(branch, actions)?;
            validate(branch, &after)?;
            let observed = actions.len() + after.len();
            if observed > MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Err(GraphEdgeUpsertBuildError::TooManyActions {
                    branch,
                    limit: MAX_GRAPH_EDGE_UPSERT_ACTIONS,
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
    pub fn merge(&self) -> &PreparedGraphEdgeMerge {
        &self.merge
    }
    #[must_use]
    pub fn on_match(&self) -> &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>] {
        &self.on_match
    }
    #[must_use]
    pub fn on_create(&self) -> &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>] {
        &self.on_create
    }
    #[must_use]
    pub fn after(&self) -> &[GraphEdgeUpsertAction<GraphEdgeUpsertValue>] {
        &self.after
    }
    #[must_use]
    pub fn action_count(&self, branch: GraphEdgeUpsertBranch) -> usize {
        match branch {
            GraphEdgeUpsertBranch::NoInput => 0,
            GraphEdgeUpsertBranch::Match => self.on_match.len() + self.after.len(),
            GraphEdgeUpsertBranch::Create => self.on_create.len() + self.after.len(),
        }
    }
    pub(crate) fn fixed_action_usage(
        &self,
        branch: GraphEdgeUpsertBranch,
    ) -> Option<crate::GlaExecutionStats> {
        use crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES;
        use fgdb_types::CanonicalScalar;
        let selected = match branch {
            GraphEdgeUpsertBranch::NoInput => {
                return Some(crate::GlaExecutionStats {
                    work_units: 1,
                    scratch_entries: 0,
                });
            }
            GraphEdgeUpsertBranch::Match => &self.on_match,
            GraphEdgeUpsertBranch::Create => &self.on_create,
        };
        let mut scratch = self.action_count(branch) as u64;
        for action in selected.iter().chain(&self.after) {
            let GraphEdgeUpsertValue::Literal(value) = &action.value else {
                return None;
            };
            let sizes = match value.value() {
                CanonicalScalar::Text(value) => [
                    value.len(),
                    value.canonical_sort_key().map_or(0, <[u8]>::len),
                ],
                CanonicalScalar::Bytes(value) => [value.as_slice().len(), 0],
                CanonicalScalar::Timestamp(value) => {
                    [value.zone().map_or(0, |zone| zone.identifier().len()), 0]
                }
                _ => [0, 0],
            };
            for bytes in sizes {
                scratch += bytes.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) as u64;
            }
        }
        Some(crate::GlaExecutionStats {
            work_units: scratch + 1,
            scratch_entries: scratch,
        })
    }
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:directed-edge-upsert:v2\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        for actions in [&self.on_match, &self.on_create, &self.after] {
            bytes.extend_from_slice(&(actions.len() as u64).to_be_bytes());
            for action in actions {
                bytes.extend_from_slice(&action.key.0.to_be_bytes());
                match &action.value {
                    GraphEdgeUpsertValue::Literal(value) => {
                        bytes.push(0);
                        bytes.extend_from_slice(
                            &(value.canonical_bytes().len() as u64).to_be_bytes(),
                        );
                        bytes.extend_from_slice(value.canonical_bytes());
                    }
                    GraphEdgeUpsertValue::Expression { properties, value } => {
                        bytes.push(1);
                        bytes.extend_from_slice(&(properties.len() as u64).to_be_bytes());
                        for key in properties {
                            bytes.extend_from_slice(&key.0.to_be_bytes());
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
