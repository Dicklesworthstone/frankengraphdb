//! Query-selected vertex mutations over the existing typed GLA relation.
//!
//! Matching and every right-hand value are evaluated before any write is
//! staged. Repeated identical assignments collapse; inconsistent assignments
//! to the same vertex field refuse rather than depend on traversal order.
//! These are statement-local proposals, NOT a second durable effect format.
//! The embedded adapter submits them through ordinary WriteTxn preparation.

mod collect;
mod relational;

use crate::algebra::{GraphValueRow, PreparedGraphPattern, ValueProjection};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GqlScalarParameter, GraphIntegerError, GraphIntegerExpression, GraphSetColumnType,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, VId};

pub const MAX_GRAPH_MUTATION_ACTIONS: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub enum GraphMutationValue {
    /// Position of a scalar property in the selection's immutable output schema.
    Column(usize),
    /// A checked canonical operand; clones share its bounded encoded storage.
    Literal(GqlScalarParameter),
    /// Checked nullable i64 bytecode over the same frozen selection row.
    /// All column references are validated before storage execution begins.
    Expression(GraphIntegerExpression),
}
impl core::fmt::Debug for GraphMutationValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphMutationValue([REDACTED])")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum GraphMutationAction {
    /// SET retains canonical NULL as a stored value. REMOVE unsets the field.
    SetProperty {
        target: usize,
        key: PropertyKeyId,
        value: GraphMutationValue,
    },
    RemoveProperty {
        target: usize,
        key: PropertyKeyId,
    },
    SetLabel {
        target: usize,
        label: LabelId,
        present: bool,
    },
    /// Explicit cascade request, subject to the storage engine's write rules.
    /// A non-detaching DELETE is intentionally not represented by this variant.
    DetachDelete {
        target: usize,
    },
}
impl GraphMutationAction {
    fn target(&self) -> usize {
        match self {
            Self::SetProperty { target, .. }
            | Self::RemoveProperty { target, .. }
            | Self::SetLabel { target, .. }
            | Self::DetachDelete { target } => *target,
        }
    }
}
impl core::fmt::Debug for GraphMutationAction {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphMutationAction([REDACTED])")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationBuildError {
    RequiresSingleGraphSource,
    RelationalInput(crate::GraphSetBuildError),
    EmptyActions,
    TooManyActions { limit: usize, observed: usize },
    TargetColumn { action: usize, column: usize },
    ValueColumn { action: usize, column: usize },
    MixedDeletionAndUpdates,
}
impl core::fmt::Display for GraphMutationBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph mutation definition: {self:?}")
    }
}
impl core::error::Error for GraphMutationBuildError {}

#[derive(Debug)]
pub enum GraphMutationError<E> {
    Source(E),
    /// A relational selection failed before any assignment was proposed.
    InputRelation(crate::GraphSetExecutionError<E>),
    InvalidSourceStatistics,
    InputSchema {
        row: usize,
        column: usize,
    },
    Arithmetic {
        row: usize,
        action: usize,
        error: GraphIntegerError,
    },
    ConflictingAssignment {
        first_row: usize,
        first_action: usize,
        row: usize,
        action: usize,
    },
    EffectLimit {
        limit: u64,
        observed: u128,
    },
}
impl<E: core::fmt::Display> core::fmt::Display for GraphMutationError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Source(error) => error.fmt(f),
            Self::InputRelation(error) => write!(f, "mutation input relation: {error}"),
            Self::InvalidSourceStatistics => {
                f.write_str("mutation source returned inconsistent statistics")
            }
            Self::InputSchema { row, column } => {
                write!(f, "mutation row {row} has an incompatible column {column}")
            }
            Self::Arithmetic { row, action, error } => {
                write!(f, "mutation row {row} action {action}: {error}")
            }
            Self::ConflictingAssignment {
                first_row,
                first_action,
                row,
                action,
            } => write!(
                f,
                "mutation assignments disagree: row {first_row} action {first_action}, row {row} action {action}"
            ),
            Self::EffectLimit { limit, observed } => {
                write!(f, "mutation effect limit exceeded: {observed} > {limit}")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphMutationError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Source(error) => Some(error),
            Self::InputRelation(error) => Some(error),
            Self::Arithmetic { error, .. } => Some(error),
            _ => None,
        }
    }
}

/// Bounds selection plus canonical proposal materialization. Query result_rows
/// bounds completed selection rows before assignment deduplication; max_effects bounds
/// distinct vertex/field intents. Storage preparation and cascade/commit costs
/// retain their own existing contracts and are not priced by these counters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphMutationPolicy {
    pub query: GqlQueryPolicy,
    pub max_effects: u64,
}
impl GraphMutationPolicy {
    #[must_use]
    pub const fn new(query: GqlQueryPolicy, max_effects: u64) -> Self {
        Self { query, max_effects }
    }
}

/// Proposal counts, not a claim of committed or changed storage records.
/// Equal-to-current assignments can normalize away in ordinary preparation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphMutationStats {
    pub selection: GqlExecutionStats,
    pub evaluator: GlaExecutionStats,
    pub target_vertices: u64,
    pub effects: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub enum GraphMutationIntent {
    Property {
        vertex: VId,
        key: PropertyKeyId,
        value: Option<CanonicalScalar>,
    },
    Label {
        vertex: VId,
        label: LabelId,
        present: bool,
    },
    DetachDelete {
        vertex: VId,
    },
}
impl core::fmt::Debug for GraphMutationIntent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("GraphMutationIntent([REDACTED])")
    }
}

/// Fully checked private staging input. It grants no storage authority and has
/// no database side effects. Only the write-capable host may submit its intents.
#[derive(Debug)]
pub struct GraphMutationBatch {
    intents: Vec<GraphMutationIntent>,
    stats: GraphMutationStats,
}
impl GraphMutationBatch {
    #[must_use]
    pub fn intents(&self) -> &[GraphMutationIntent] {
        &self.intents
    }
    #[must_use]
    pub const fn stats(&self) -> GraphMutationStats {
        self.stats
    }
    #[must_use]
    pub fn into_intents(self) -> Vec<GraphMutationIntent> {
        self.intents
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphMutation {
    selection: PreparedGraphPattern<GraphValueRow>,
    input_relation: Option<crate::PreparedGraphSet>,
    columns: Vec<GraphSetColumnType>,
    relation: RelationId,
    actions: Vec<GraphMutationAction>,
}
impl core::fmt::Debug for PreparedGraphMutation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutation")
            .field("actions", &self.actions.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphMutation {
    /// The caller supplies the existing WriteBatch relation coordinate. It is
    /// never guessed from a MATCH relation, a result value, or an ambient catalog.
    /// All targets are vertex columns; assignment inputs must be scalar columns.
    /// Deletion and updates cannot be mixed in one simultaneous statement.
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        actions: Vec<GraphMutationAction>,
    ) -> Result<Self, GraphMutationBuildError> {
        Self::prepare_input(selection, None, relation, actions)
    }

    fn prepare_input(
        selection: PreparedGraphPattern<GraphValueRow>,
        input_relation: Option<crate::PreparedGraphSet>,
        relation: RelationId,
        actions: Vec<GraphMutationAction>,
    ) -> Result<Self, GraphMutationBuildError> {
        if actions.is_empty() {
            return Err(GraphMutationBuildError::EmptyActions);
        }
        if actions.len() > MAX_GRAPH_MUTATION_ACTIONS {
            return Err(GraphMutationBuildError::TooManyActions {
                limit: MAX_GRAPH_MUTATION_ACTIONS,
                observed: actions.len(),
            });
        }
        let columns: Vec<_> = match &input_relation {
            Some(input) => input.column_types().to_vec(),
            None => selection.value_columns().iter().map(|column| match column {
                ValueProjection::Vertex { .. } => GraphSetColumnType::Vertex,
                ValueProjection::Property { .. } => GraphSetColumnType::Scalar,
            }).collect(),
        };
        let deleting = matches!(actions[0], GraphMutationAction::DetachDelete { .. });
        for (at, action) in actions.iter().enumerate() {
            let target = action.target();
            if columns.get(target) != Some(&GraphSetColumnType::Vertex) {
                return Err(GraphMutationBuildError::TargetColumn {
                    action: at,
                    column: target,
                });
            }
            if let GraphMutationAction::SetProperty { value, .. } = action {
                let check = |column: usize| {
                    if columns.get(column) == Some(&GraphSetColumnType::Scalar) {
                        Ok(())
                    } else {
                        Err(GraphMutationBuildError::ValueColumn { action: at, column })
                    }
                };
                match value {
                    GraphMutationValue::Column(column) => check(*column)?,
                    GraphMutationValue::Expression(expression) => {
                        for column in expression.referenced_columns() {
                            check(column)?;
                        }
                    }
                    GraphMutationValue::Literal(_) => {}
                }
            }
            if deleting != matches!(action, GraphMutationAction::DetachDelete { .. }) {
                return Err(GraphMutationBuildError::MixedDeletionAndUpdates);
            }
        }
        Ok(Self {
            selection,
            input_relation,
            columns,
            relation,
            actions,
        })
    }
    #[must_use]
    /// Actual graph source for the trusted reader. Actions address the completed
    /// input_relation schema when present, not this leaf's private columns.
    pub fn selection(&self) -> &PreparedGraphPattern<GraphValueRow> {
        &self.selection
    }
    #[must_use]
    pub const fn relation(&self) -> RelationId {
        self.relation
    }
    #[must_use]
    pub fn actions(&self) -> &[GraphMutationAction] {
        &self.actions
    }

    /// Freeze the complete selection once, then reduce simultaneous assignments.
    /// Null OPTIONAL targets are ignored, never interpreted as an identity.
    /// Conflicting duplicates, arithmetic errors, source failure, cancellation
    /// and quota refusal return no batch. The trusted source must use one pinned
    /// GLA snapshot or canonical transaction overlay and retain read domains.
    pub fn execute_governed<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute(self, policy, source, checkpoint)
    }

    /// Application transcript, not a durable format or authorization permit.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-mutation:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        let input = self.selection.canonical_bytes();
        bytes.extend_from_slice(&(input.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&input);
        bytes.extend_from_slice(&(self.actions.len() as u64).to_be_bytes());
        for action in &self.actions {
            bytes.extend_from_slice(&(action.target() as u64).to_be_bytes());
            match action {
                GraphMutationAction::SetProperty { key, value, .. } => {
                    bytes.push(0);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    match value {
                        GraphMutationValue::Column(column) => {
                            bytes.push(0);
                            bytes.extend_from_slice(&(*column as u64).to_be_bytes());
                        }
                        GraphMutationValue::Literal(value) => {
                            bytes.push(1);
                            bytes.extend_from_slice(
                                &(value.canonical_bytes().len() as u64).to_be_bytes(),
                            );
                            bytes.extend_from_slice(value.canonical_bytes());
                        }
                        GraphMutationValue::Expression(expression) => {
                            bytes.push(2);
                            let expression = expression.canonical_bytes();
                            bytes.extend_from_slice(&(expression.len() as u64).to_be_bytes());
                            bytes.extend_from_slice(&expression);
                        }
                    }
                }
                GraphMutationAction::RemoveProperty { key, .. } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
                GraphMutationAction::SetLabel { label, present, .. } => {
                    bytes.push(2);
                    bytes.extend_from_slice(&label.0.to_be_bytes());
                    bytes.push(u8::from(*present));
                }
                GraphMutationAction::DetachDelete { .. } => bytes.push(3),
            }
        }
        if let Some(relation) = &self.input_relation {
            bytes.extend_from_slice(b"fgdb:mutation-relational-input:v1\0");
            let input = relation.canonical_bytes();
            bytes.extend_from_slice(&(input.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&input);
        }
        bytes
    }
}
