//! Query-selected element mutations over the existing typed GLA relation.
//!
//! Matching and every right-hand value are evaluated before any write is
//! staged. Repeated identical assignments collapse; inconsistent assignments
//! to the same element field refuse rather than depend on traversal order.
//! These are statement-local proposals, NOT a second durable effect format.
//! The embedded adapter submits them through ordinary WriteTxn preparation.

mod collect;
mod query;
mod relational;

pub use query::{
    GraphMutationQueryBatch, GraphMutationQueryBuildError, GraphMutationQueryError,
    PreparedGraphMutationQuery,
};

use crate::algebra::{GraphValueRow, PreparedGraphPattern};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GqlScalarParameter, GraphIntegerError, GraphIntegerExpression, GraphSetColumnType,
};
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};

pub const MAX_GRAPH_MUTATION_ACTIONS: usize = 256;

/// The most elements one matched path holds (TCK Delete3): each pattern edge
/// is at most one walk of the longest admitted hop count, and every hop adds
/// one relationship and one node to the start node.
const MAX_GRAPH_PATH_ELEMENTS: u128 =
    2 * crate::algebra::MAX_PATTERN_EDGES as u128 * crate::MAX_GRAPH_WALK_HOPS as u128 + 1;

#[derive(Clone, PartialEq, Eq)]
pub enum GraphMutationValue {
    /// Position of a scalar or list property in the immutable selection schema.
    Column(usize),
    /// A checked canonical operand; clones share its bounded encoded storage.
    Literal(GqlScalarParameter),
    /// Checked canonical scalar bytecode over the same frozen selection row.
    /// All column references are validated before storage execution begins.
    Expression(GraphIntegerExpression),
    /// A composite read value over the same row. The shared row evaluator
    /// computes it; a scalar or bounded list of written values can be stored.
    /// Maps and graph elements at any depth refuse, never truncate.
    Composite(crate::GraphSetValue),
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
    /// openCypher `SET n = {...}`: unset every property `target` carries
    /// that `keep` does not name. `current` is the target's frozen property
    /// map (a Properties column), whose names resolve through the selection's
    /// reverse catalog; its property names must be injective. The kept keys'
    /// values are ordinary SetProperty actions of the same statement.
    ClearProperties {
        target: usize,
        current: usize,
        keep: Vec<PropertyKeyId>,
    },
    SetLabel {
        target: usize,
        label: LabelId,
        present: bool,
    },
    /// Explicit cascade request, subject to the storage engine's write rules.
    /// A non-detaching DELETE is never represented by this variant.
    DetachDelete {
        target: usize,
    },
    /// Non-detaching DELETE of a vertex or relationship. The write host must
    /// prove that no live relationship stays incident to a deleted vertex
    /// once this statement's own relationship deletions are applied; it never
    /// turns into a cascade. Plain and detaching deletes do not mix in one
    /// statement.
    Delete {
        target: usize,
    },
    /// DELETE or DETACH DELETE of a value each input row computes (TCK
    /// Delete5): `friends[$i]`, `map.key`, `m.key.key[0]` after a WITH. The
    /// value is a vertex, a relationship, a path (each of its elements, as for
    /// a path column) or NULL, which deletes nothing; any other value refuses
    /// when it runs. DETACH takes a vertex's remaining relationships with it
    /// and deletes a relationship as a plain DELETE does. No target column:
    /// the value reads the row.
    DeleteValue {
        value: crate::GraphSetValue,
        detach: bool,
    },
}
impl GraphMutationAction {
    /// The element column an action targets; a computed delete has none.
    fn target(&self) -> Option<usize> {
        match self {
            Self::SetProperty { target, .. }
            | Self::RemoveProperty { target, .. }
            | Self::ClearProperties { target, .. }
            | Self::SetLabel { target, .. }
            | Self::DetachDelete { target }
            | Self::Delete { target } => Some(*target),
            Self::DeleteValue { .. } => None,
        }
    }

    const fn deletes(&self) -> bool {
        matches!(
            self,
            Self::DetachDelete { .. } | Self::Delete { .. } | Self::DeleteValue { .. }
        )
    }

    const fn detaches(&self) -> bool {
        matches!(
            self,
            Self::DetachDelete { .. } | Self::DeleteValue { detach: true, .. }
        )
    }
}
/// One private column of a mutation RETURN input row (`SET ... RETURN`).
/// Rows are the statement's own selection, so each MATCH occurrence returns
/// exactly once, in selection order, before any later projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphMutationBinding {
    /// A selection column as frozen before the statement: an element
    /// identity, a path, a metadata value or a scalar no action can change.
    Input(usize),
    /// `target.key` as it stands after the statement. The statement's own
    /// assignment to that element field wins (REMOVE reads NULL); otherwise
    /// the frozen pre-statement value in `current` is returned. Simultaneous
    /// assignments are conflict-checked, so the value is one per element,
    /// never an order-dependent choice. Reading an element the statement
    /// deletes is refused, and a NULL OPTIONAL target reads NULL.
    Property {
        target: usize,
        key: PropertyKeyId,
        current: usize,
    },
    /// The target's complete post-statement property map. `current` holds
    /// the frozen admitted map, and the selection plan's reverse catalog
    /// names every final assignment merged into it. REMOVE omits a field;
    /// SET retains its scalar or list, including canonical NULL. A NULL OPTIONAL
    /// target returns NULL; a deleted target or unmapped assignment refuses.
    /// Edge maps with DETACH DELETE are refused at preparation because an
    /// incident edge can disappear through a vertex's cascade.
    Properties { target: usize, current: usize },
    /// `labels(target)` after the statement: the frozen `labels()` value in
    /// `current` with this statement's own SET/REMOVE label actions applied,
    /// in the label-identity order every `labels()` read uses. The selection
    /// plan's reverse catalog names each label; a NULL OPTIONAL target reads
    /// NULL and a deleted target refuses.
    Labels { target: usize, current: usize },
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
    TooManyActions {
        limit: usize,
        observed: usize,
    },
    TargetColumn {
        action: usize,
        column: usize,
    },
    ValueColumn {
        action: usize,
        column: usize,
    },
    /// A composite value that does not fit the selection schema or can never
    /// be a scalar or list property value.
    ValueExpression {
        action: usize,
        kind: Option<crate::GraphSetProjectionError>,
    },
    MixedDeletionAndUpdates,
    /// DELETE and DETACH DELETE in one statement: a cascade would have to
    /// be part of the plain delete's incidence proof.
    MixedDeleteModes,
}
impl core::fmt::Display for GraphMutationBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RequiresSingleGraphSource => {
                f.write_str("a mutation requires exactly one graph source")
            }
            Self::RelationalInput(error) => error.fmt(f),
            Self::EmptyActions => {
                f.write_str("a mutation requires at least one SET, REMOVE or DELETE action")
            }
            Self::TooManyActions { limit, observed } => {
                write!(f, "mutation has {observed} actions, limit {limit}")
            }
            Self::TargetColumn { action, column } => write!(
                f,
                "mutation action {action} targets column {column}, which is not an element it can change"
            ),
            Self::ValueColumn { action, column } => write!(
                f,
                "mutation action {action} reads column {column}, which is not an assignable value"
            ),
            Self::ValueExpression { action, kind } => {
                write!(
                    f,
                    "mutation action {action} assigns a value that cannot be stored"
                )?;
                match kind {
                    Some(kind) => write!(f, ": {kind}"),
                    None => Ok(()),
                }
            }
            Self::MixedDeletionAndUpdates => {
                f.write_str("one statement cannot both delete and update")
            }
            Self::MixedDeleteModes => {
                f.write_str("one statement cannot mix DELETE and DETACH DELETE")
            }
        }
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
    /// A value cannot be a written property: a map, graph element, or a list
    /// containing one or exceeding the native collection bounds. The legacy
    /// variant name is retained for existing callers.
    NonScalarValue {
        row: usize,
        action: usize,
    },
    /// A computed DELETE target is not a vertex, relationship, path or NULL
    /// (TCK Delete5).
    NonElementDelete {
        row: usize,
        action: usize,
    },
    /// A SET or REMOVE target read from a dynamically typed column holds a
    /// value that is not a node or relationship, or a label target holds a
    /// relationship. NULL is never one: it changes nothing.
    NonElementTarget {
        row: usize,
        action: usize,
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
    /// A RETURN binding read a property of an element this statement deletes.
    DeletedElementRead {
        row: usize,
        binding: usize,
    },
    /// A complete-map RETURN binding did not receive a bounded property map.
    InvalidPropertyMap {
        row: usize,
        binding: usize,
    },
    /// A final assignment has no name in the trusted reverse catalog.
    UnmappedProperty {
        row: usize,
        binding: usize,
    },
    /// A plain DELETE target vertex keeps at least one live incident
    /// relationship in the exact transaction overlay. IDs remain redacted.
    IncidentRelationships,
    /// A post-statement `labels()` read met a label the trusted reverse
    /// catalog does not name, or a frozen name it cannot resolve.
    UnmappedLabel {
        row: usize,
        binding: usize,
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
            Self::NonScalarValue { row, action } => {
                write!(
                    f,
                    "mutation row {row} action {action}: not a scalar property value or bounded stored list"
                )
            }
            Self::NonElementDelete { row, action } => write!(
                f,
                "mutation row {row} action {action}: DELETE of a value that is not a node, relationship or path"
            ),
            Self::NonElementTarget { row, action } => write!(
                f,
                "mutation row {row} action {action}: SET or REMOVE of a value that is not a node or relationship, or a label of a relationship"
            ),
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
            Self::DeletedElementRead { row, binding } => write!(
                f,
                "mutation RETURN row {row} binding {binding}: property of an element this statement deletes"
            ),
            Self::InvalidPropertyMap { row, binding } => write!(
                f,
                "mutation RETURN row {row} binding {binding}: invalid property map"
            ),
            Self::UnmappedProperty { row, binding } => write!(
                f,
                "mutation RETURN row {row} binding {binding}: unmapped property"
            ),
            Self::IncidentRelationships => {
                f.write_str("DELETE target keeps an incident relationship; use DETACH DELETE")
            }
            Self::UnmappedLabel { row, binding } => write!(
                f,
                "mutation RETURN row {row} binding {binding}: unmapped label"
            ),
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
/// distinct element/field intents. Storage preparation and cascade/commit costs
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
    pub target_edges: u64,
    pub effects: u64,
}

/// `value` is a stored property value (fgdb-k3nub): a scalar, canonical
/// NULL included, or a List; `None` removes the property.
#[derive(Clone, PartialEq, Eq)]
pub enum GraphMutationIntent {
    Property {
        vertex: VId,
        key: PropertyKeyId,
        value: Option<CanonicalPropertyValue>,
    },
    EdgeProperty {
        edge: EId,
        key: PropertyKeyId,
        value: Option<CanonicalPropertyValue>,
    },
    /// A bounded non-scalar written value. The collector currently emits
    /// lists only; scalar assignments and removals retain the existing arms.
    /// Adapters must stage this through their ordinary stored-property writer
    /// with the same before/after authorization and rollback as scalar fields.
    StoredProperty {
        target: ElementId,
        key: PropertyKeyId,
        value: CanonicalPropertyValue,
    },
    Label {
        vertex: VId,
        label: LabelId,
        present: bool,
    },
    DetachDelete {
        vertex: VId,
    },
    /// Non-detaching vertex deletion. The host proves the vertex is isolated
    /// once this statement's `DeleteEdge` intents apply.
    Delete {
        vertex: VId,
    },
    DeleteEdge {
        edge: EId,
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
    /// The graph source. `None` only for a chain step over the previous
    /// write's rows (fgdb-cpny9), whose input relation reads its one owned
    /// leaf and never the graph.
    selection: Option<PreparedGraphPattern<GraphValueRow>>,
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
    /// Closed profile used by the UNWIND clause-at-a-time adapter. Every
    /// target must be a resolved vertex column and every action a field update;
    /// deletions and relationship updates need different staging contracts.
    pub(crate) fn is_vertex_update_only(&self) -> bool {
        self.actions.iter().all(|action| {
            !action.deletes()
                && action.target().and_then(|target| self.columns.get(target))
                    == Some(&GraphSetColumnType::Vertex)
        })
    }

    /// Every assignment writes a bound constant (a literal or an argument) or
    /// a label, or removes a field. No right-hand side reads the selected
    /// row, so no earlier input row's staged effects can change its value.
    pub(crate) fn writes_only_constants(&self) -> bool {
        self.actions.iter().all(|action| {
            matches!(
                action,
                GraphMutationAction::SetProperty {
                    value: GraphMutationValue::Literal(_),
                    ..
                } | GraphMutationAction::RemoveProperty { .. }
                    | GraphMutationAction::SetLabel { .. }
            )
        })
    }

    /// Bound the program's field-intent statistics from the resolved definition.
    /// A complete-map replacement expands its clear action to at most every
    /// unkept catalog key: the collector refuses a property name absent from
    /// this same injective catalog. A path delete proposes one effect per
    /// element of its path, and so may a computed delete, whose value may be a
    /// path. Ordinary actions keep their one-intent bound.
    pub(crate) fn maximum_effects_per_binding(&self) -> usize {
        let maximum: u128 = self
            .actions
            .iter()
            .map(|action| match action {
                GraphMutationAction::ClearProperties { keep, .. } => self
                    .selection
                    .as_ref()
                    .and_then(|selection| selection.plan().reverse_catalog.as_ref())
                    .map_or(0, |catalog| {
                        catalog
                            .properties
                            .keys()
                            .filter(|key| !keep.contains(key))
                            .count() as u128
                    }),
                GraphMutationAction::DeleteValue { .. } => MAX_GRAPH_PATH_ELEMENTS,
                GraphMutationAction::Delete { .. } | GraphMutationAction::DetachDelete { .. }
                    if action.target().and_then(|target| self.columns.get(target))
                        == Some(&GraphSetColumnType::Path) =>
                {
                    MAX_GRAPH_PATH_ELEMENTS
                }
                _ => 1,
            })
            .sum();
        // At most MAX_GRAPH_MUTATION_ACTIONS usize-sized catalogs fit this
        // u128 sum. A materialized proposal has at most usize::MAX intents,
        // so capping to that length cannot admit an impossible larger count.
        maximum.min(usize::MAX as u128) as usize
    }

    /// The caller supplies the existing WriteBatch relation coordinate. It is
    /// never guessed from a MATCH relation, a result value, or an ambient catalog.
    /// Property targets are vertex or edge columns; inputs are written values.
    /// Deletion and updates cannot be mixed in one simultaneous statement.
    pub fn prepare(
        selection: PreparedGraphPattern<GraphValueRow>,
        relation: RelationId,
        actions: Vec<GraphMutationAction>,
    ) -> Result<Self, GraphMutationBuildError> {
        Self::prepare_input(Some(selection), None, relation, actions)
    }

    fn prepare_input(
        selection: Option<PreparedGraphPattern<GraphValueRow>>,
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
        let columns: Vec<_> = match (&input_relation, &selection) {
            (Some(input), _) => input.column_types().to_vec(),
            (None, Some(selection)) => selection
                .value_columns()
                .iter()
                .map(GraphSetColumnType::from)
                .collect(),
            (None, None) => return Err(GraphMutationBuildError::RequiresSingleGraphSource),
        };
        let deleting = actions[0].deletes();
        let detaching = actions[0].detaches();
        for (at, action) in actions.iter().enumerate() {
            // A computed delete's value may be an element, a path or NULL;
            // which one is known only when it runs (TCK Delete5).
            if let GraphMutationAction::DeleteValue { value, .. } = action {
                let refused = |kind| GraphMutationBuildError::ValueExpression { action: at, kind };
                let kind = crate::GraphSetProjection::admit_output(value, &columns, at)
                    .map_err(|kind| refused(Some(kind)))?;
                if !matches!(
                    kind,
                    GraphSetColumnType::Vertex
                        | GraphSetColumnType::Edge
                        | GraphSetColumnType::Path
                        | GraphSetColumnType::Any
                ) {
                    return Err(refused(None));
                }
            }
            // A path is a delete target: its elements (TCK Delete3). SET and
            // REMOVE of a property or label also take a dynamically typed
            // column (an UNWIND element); the collector checks each value it
            // holds when it runs (NonElementTarget).
            let valid_target = match (action, action.target().map(|target| columns.get(target))) {
                (GraphMutationAction::DeleteValue { .. }, _) => true,
                (
                    GraphMutationAction::SetProperty { .. }
                    | GraphMutationAction::RemoveProperty { .. },
                    Some(kind),
                ) => matches!(
                    kind,
                    Some(
                        GraphSetColumnType::Vertex
                            | GraphSetColumnType::Edge
                            | GraphSetColumnType::Any
                    )
                ),
                (GraphMutationAction::ClearProperties { .. }, Some(kind)) => matches!(
                    kind,
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                ),
                (GraphMutationAction::Delete { .. }, Some(kind)) => matches!(
                    kind,
                    Some(
                        GraphSetColumnType::Vertex
                            | GraphSetColumnType::Edge
                            | GraphSetColumnType::Path
                    )
                ),
                (GraphMutationAction::DetachDelete { .. }, Some(kind)) => matches!(
                    kind,
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Path)
                ),
                (GraphMutationAction::SetLabel { .. }, Some(kind)) => matches!(
                    kind,
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Any)
                ),
                (_, None) => false,
            };
            if !valid_target {
                return Err(GraphMutationBuildError::TargetColumn {
                    action: at,
                    column: action.target().unwrap_or(usize::MAX),
                });
            }
            if let GraphMutationAction::ClearProperties { current, .. } = action {
                // The frozen property map, read by name through the reverse
                // catalog: one name per property identity, or a removal could
                // land on the wrong key.
                let injective = selection
                    .as_ref()
                    .and_then(|selection| selection.plan().reverse_catalog.as_ref())
                    .is_some_and(|catalog| {
                        let mut names = std::collections::BTreeSet::new();
                        catalog.properties.values().all(|name| names.insert(name))
                    });
                if columns.get(*current) != Some(&GraphSetColumnType::Any) || !injective {
                    return Err(GraphMutationBuildError::ValueColumn {
                        action: at,
                        column: *current,
                    });
                }
            }
            if let GraphMutationAction::SetProperty { value, .. } = action {
                let check = |column: usize| {
                    if columns
                        .get(column)
                        .is_some_and(|kind| kind.is_scalar_like())
                    {
                        Ok(())
                    } else {
                        Err(GraphMutationBuildError::ValueColumn { action: at, column })
                    }
                };
                match value {
                    GraphMutationValue::Column(column) => {
                        if !matches!(
                            columns.get(*column),
                            Some(GraphSetColumnType::List | GraphSetColumnType::Any)
                        ) {
                            check(*column)?;
                        }
                    }
                    GraphMutationValue::Expression(expression) => {
                        for column in expression.referenced_columns() {
                            check(column)?;
                        }
                    }
                    GraphMutationValue::Literal(_) => {}
                    GraphMutationValue::Composite(value) => {
                        let refused =
                            |kind| GraphMutationBuildError::ValueExpression { action: at, kind };
                        let kind = crate::GraphSetProjection::admit_output(value, &columns, at)
                            .map_err(|kind| refused(Some(kind)))?;
                        if !(kind.is_scalar_like()
                            || matches!(kind, GraphSetColumnType::List | GraphSetColumnType::Any))
                        {
                            return Err(refused(None));
                        }
                    }
                }
            }
            if deleting != action.deletes() {
                return Err(GraphMutationBuildError::MixedDeletionAndUpdates);
            }
            if deleting && detaching != action.detaches() {
                return Err(GraphMutationBuildError::MixedDeleteModes);
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
    /// `None` for a chain step over the previous write's rows.
    pub fn selection(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        self.selection.as_ref()
    }
    /// The selection's label and property names, when it carries them. A
    /// chain step over owned rows has none.
    pub(crate) fn reverse_catalog(&self) -> Option<&crate::graph_text::ReverseSymbolCatalog> {
        self.selection
            .as_ref()
            .and_then(|selection| selection.plan().reverse_catalog.as_deref())
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
        self.execute_governed_over(None, policy, source, checkpoint)
    }

    /// A chain step's write (fgdb-cpny9): `owned` are the rows the previous
    /// write of the statement returned, read by the input relation's one
    /// owned leaf. `None` is [`Self::execute_governed`].
    pub fn execute_governed_over<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute(self, policy, None, owned, source, None, checkpoint)
            .map(|(batch, _)| batch)
    }

    /// [`Self::execute_governed`] with the host's element property reader
    /// (fgdb-694ga). `x.key` of an element the input relation or a
    /// right-hand side computes (`WITH collect(n) AS ns UNWIND ns AS m SET
    /// m.q = m.p * 3`) reads `elements(element, key)` at the statement's
    /// frozen selection cut, under the same authorization as `source`.
    /// `Ok(None)` is an absent property. Without a reader that read stays a
    /// typed error. The reader retains its point witnesses and never observes
    /// this statement's own proposed assignments.
    pub fn execute_governed_reading<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        elements: impl FnMut(ElementId, PropertyKeyId) -> Result<Option<CanonicalPropertyValue>, E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        self.execute_governed_over_reading(None, policy, source, elements, checkpoint)
    }

    /// [`Self::execute_governed_reading`] over a chain's owned input.
    /// Owned identities are not new authority: the host must admit them under
    /// the same scope as the graph source before any field is read.
    pub fn execute_governed_over_reading<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut elements: impl FnMut(ElementId, PropertyKeyId) -> Result<Option<CanonicalPropertyValue>, E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute(
            self,
            policy,
            None,
            owned,
            source,
            Some(&mut elements),
            checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// Application transcript, not a durable format or authorization permit.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-mutation:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        // A chain step over owned rows has no graph source: its length is
        // u64::MAX, which no encoded selection reaches.
        match &self.selection {
            Some(selection) => {
                let input = selection.canonical_bytes();
                bytes.extend_from_slice(&(input.len() as u64).to_be_bytes());
                bytes.extend_from_slice(&input);
            }
            None => bytes.extend_from_slice(&u64::MAX.to_be_bytes()),
        }
        bytes.extend_from_slice(&(self.actions.len() as u64).to_be_bytes());
        for action in &self.actions {
            let target = action.target().map_or(u64::MAX, |target| target as u64);
            bytes.extend_from_slice(&target.to_be_bytes());
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
                        GraphMutationValue::Composite(value) => {
                            bytes.push(3);
                            let mut encoded = Vec::new();
                            value.append_canonical_bytes(&mut encoded);
                            bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                            bytes.extend_from_slice(&encoded);
                        }
                    }
                }
                GraphMutationAction::RemoveProperty { key, .. } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
                GraphMutationAction::ClearProperties { current, keep, .. } => {
                    bytes.push(5);
                    bytes.extend_from_slice(&(*current as u64).to_be_bytes());
                    bytes.extend_from_slice(&(keep.len() as u64).to_be_bytes());
                    for key in keep {
                        bytes.extend_from_slice(&key.0.to_be_bytes());
                    }
                }
                GraphMutationAction::SetLabel { label, present, .. } => {
                    bytes.push(2);
                    bytes.extend_from_slice(&label.0.to_be_bytes());
                    bytes.push(u8::from(*present));
                }
                GraphMutationAction::DetachDelete { .. } => bytes.push(3),
                GraphMutationAction::Delete { .. } => bytes.push(4),
                GraphMutationAction::DeleteValue { value, detach } => {
                    bytes.push(6);
                    bytes.push(u8::from(*detach));
                    let mut encoded = Vec::new();
                    value.append_canonical_bytes(&mut encoded);
                    bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(&encoded);
                }
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
