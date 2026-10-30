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
    /// SET of a value that is NULL when it runs unsets the field, exactly as
    /// REMOVE does: a property set to null is absent (fgdb-e26tv).
    SetProperty {
        target: usize,
        key: PropertyKeyId,
        value: GraphMutationValue,
    },
    RemoveProperty {
        target: usize,
        key: PropertyKeyId,
    },
    /// Apply a computed property map to one element. Every value reads the
    /// same frozen selection as ordinary SET; maps expand to ordinary field
    /// proposals, with the same duplicate/conflict and storage-value rules.
    /// `None` merges only the supplied keys (`+=`). `Some(current)`
    /// replaces all properties (`=`): `current` reads the target's complete
    /// admitted property map at that frozen cut. Missing keys are removed;
    /// explicit NULL removes the field, as for SetProperty. NULL elements
    /// inside a stored list remain values.
    /// The pinned catalog must name every supplied/current key injectively.
    /// An absent OPTIONAL target skips both expressions.
    SetProperties {
        target: usize,
        value: crate::GraphSetValue,
        current: Option<crate::GraphSetValue>,
        catalog: std::sync::Arc<crate::ReverseSymbolCatalog>,
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
            | Self::SetProperties { target, .. }
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
    /// names every final assignment merged into it. REMOVE and a SET to NULL
    /// omit a field; SET retains its scalar or list. A NULL OPTIONAL
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
    /// Real graph leaves must agree on symbol identities and their names.
    IncompatibleSourceCatalog,
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
    /// A computed property map needs one unambiguous name per property ID.
    PropertyMapCatalog {
        action: usize,
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
            Self::IncompatibleSourceCatalog => {
                f.write_str("mutation sources disagree on reverse symbol names")
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
            Self::PropertyMapCatalog { action } => write!(
                f,
                "mutation action {action} requires an injective property catalog"
            ),
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
    /// A legacy once-only source was asked to execute another graph leaf or
    /// another occurrence of an Apply body. No mutation batch is released.
    ReusableSourceRequired,
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
            Self::ReusableSourceRequired => {
                f.write_str("mutation input requires a reusable graph source")
            }
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
    /// The sole eager graph source when there is one. A multipart relation,
    /// conditional body-only input or owned/unit relation need not have one.
    selection: Option<PreparedGraphPattern<GraphValueRow>>,
    input_relation: Option<crate::PreparedGraphSet>,
    reverse_catalog: Option<std::sync::Arc<crate::ReverseSymbolCatalog>>,
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
    /// deletion and mixed vertex/relationship updates are excluded.
    pub(crate) fn is_vertex_update_only(&self) -> bool {
        self.actions.iter().all(|action| {
            !action.deletes()
                && !matches!(action, GraphMutationAction::SetProperties { .. })
                && action.target().and_then(|target| self.columns.get(target))
                    == Some(&GraphSetColumnType::Vertex)
        })
    }

    /// Relationship field updates may freeze their input independently of
    /// vertex updates. They cannot change endpoint images or topology; mixed
    /// vertex/edge actions and deletion retain their own staging contracts.
    pub(crate) fn is_edge_update_only(&self) -> bool {
        !self.actions.is_empty()
            && self.actions.iter().all(|action| {
                matches!(
                    action,
                    GraphMutationAction::SetProperty { .. }
                        | GraphMutationAction::RemoveProperty { .. }
                        | GraphMutationAction::ClearProperties { .. }
                ) && action.target().and_then(|target| self.columns.get(target))
                    == Some(&GraphSetColumnType::Edge)
            })
    }

    /// Vertex and relationship field updates in one statement. This profile
    /// changes no topology, but endpoint fields may change an edge's authority
    /// image, so its authorized staging must retain every ordered field check.
    pub(crate) fn is_mixed_update_only(&self) -> bool {
        let mut vertex = false;
        let mut edge = false;
        for action in &self.actions {
            if action.deletes() || matches!(action, GraphMutationAction::SetProperties { .. }) {
                return false;
            }
            match action.target().and_then(|target| self.columns.get(target)) {
                Some(GraphSetColumnType::Vertex) => vertex = true,
                Some(GraphSetColumnType::Edge) => edge = true,
                _ => return false,
            }
        }
        vertex && edge
    }

    /// Every assignment writes a bound constant or row-independent scalar or
    /// collection expression, changes a label, or removes a field. Bytecode
    /// dependencies and lexical value scopes prove that it reads no selection
    /// column or graph state. An expression can still fail; normal execution
    /// must evaluate it and preserve its original row's error and budget charges.
    pub(crate) fn writes_only_constants(&self) -> bool {
        self.actions.iter().all(|action| match action {
            GraphMutationAction::SetProperty { value, .. } => match value {
                GraphMutationValue::Literal(_) => true,
                GraphMutationValue::Expression(expression) => {
                    expression.referenced_columns().next().is_none()
                        && expression.referenced_locals().next().is_none()
                }
                GraphMutationValue::Composite(value) => value.is_input_independent(),
                GraphMutationValue::Column(_) => false,
            },
            GraphMutationAction::RemoveProperty { .. } | GraphMutationAction::SetLabel { .. } => {
                true
            }
            _ => false,
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
                GraphMutationAction::ClearProperties { keep, .. } => {
                    self.reverse_catalog().map_or(0, |catalog| {
                        catalog
                            .properties
                            .keys()
                            .filter(|key| !keep.contains(key))
                            .count() as u128
                    })
                }
                GraphMutationAction::SetProperties { catalog, .. } => {
                    catalog.properties.len() as u128
                }
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
    /// No actions is a write without effect, as `SET n += {}` is (TCK Set5
    /// [5]): it still selects, and returns, its rows.
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
        let mut reverse_catalog = match &input_relation {
            Some(input) => relational::input_catalog(input)?,
            None => selection
                .as_ref()
                .and_then(|selection| selection.plan().reverse_catalog.clone()),
        };
        let deleting = actions.first().is_some_and(GraphMutationAction::deletes);
        let detaching = actions.first().is_some_and(GraphMutationAction::detaches);
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
                    | GraphMutationAction::RemoveProperty { .. }
                    | GraphMutationAction::SetProperties { .. },
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
                let injective = reverse_catalog.as_ref().is_some_and(|catalog| {
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
            if let GraphMutationAction::SetProperties {
                value,
                current,
                catalog,
                ..
            } = action
            {
                let mut names = std::collections::BTreeSet::new();
                if !catalog.properties.values().all(|name| names.insert(name)) {
                    return Err(GraphMutationBuildError::PropertyMapCatalog { action: at });
                }
                for value in core::iter::once(value).chain(current.iter()) {
                    let refused =
                        |kind| GraphMutationBuildError::ValueExpression { action: at, kind };
                    let kind = crate::GraphSetProjection::admit_output(value, &columns, at)
                        .map_err(|kind| refused(Some(kind)))?;
                    if kind != GraphSetColumnType::Any {
                        return Err(refused(None));
                    }
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
                        for column in expression.scalar_columns() {
                            check(column)?;
                        }
                        // As a projection admits them: id()/elementId() and an
                        // element's IS [NOT] NULL read its identity, and a
                        // presence test its NULL-ness alone, so no element
                        // value reaches the property (fgdb-j687q).
                        for column in expression.element_columns() {
                            if !matches!(
                                columns.get(column),
                                Some(
                                    GraphSetColumnType::Vertex
                                        | GraphSetColumnType::Edge
                                        | GraphSetColumnType::Any
                                )
                            ) {
                                return Err(GraphMutationBuildError::ValueColumn {
                                    action: at,
                                    column,
                                });
                            }
                        }
                        if let Some(column) = expression
                            .referenced_columns()
                            .find(|column| *column >= columns.len())
                        {
                            return Err(GraphMutationBuildError::ValueColumn {
                                action: at,
                                column,
                            });
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
        // A computed map owns a trusted property catalog even when its input
        // leaf never read an assigned key. Preserve those names for final
        // metadata bindings, while refusing disagreements with any real leaf.
        // Only properties participate: they are the action's encoded catalog.
        for action in &actions {
            if let GraphMutationAction::SetProperties { catalog, .. } = action {
                relational::merge_property_catalog(&mut reverse_catalog, catalog)?;
            }
        }
        Ok(Self {
            selection,
            input_relation,
            reverse_catalog,
            columns,
            relation,
            actions,
        })
    }
    #[must_use]
    /// Sole eager graph source, when present. Actions address the completed
    /// input_relation schema, not this leaf's private columns. Use
    /// [`Self::admit_patterns`] to admit every graph source of a multipart input.
    pub fn selection(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        self.selection.as_ref()
    }
    /// Compatible names from every real graph leaf and the explicit property
    /// catalogs of computed-map actions. Owned/unit input invents no names.
    pub(crate) fn reverse_catalog(&self) -> Option<&crate::graph_text::ReverseSymbolCatalog> {
        self.reverse_catalog.as_deref()
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
        collect::execute(self, policy, None, owned, source, None, None, checkpoint)
            .map(|(batch, _)| batch)
    }

    /// [`Self::execute_governed`] with the host's element reader
    /// (fgdb-694ga). `x.key` of an element the input relation or a
    /// right-hand side computes (`WITH collect(n) AS ns UNWIND ns AS m SET
    /// m.q = m.p * 3`) reads `elements.element_property(element, key)` at the statement's
    /// frozen selection cut, under the same authorization as `source`.
    /// `Ok(None)` is an absent property. Without a reader that read stays a
    /// typed error. The reader retains its point witnesses and never observes
    /// this statement's own proposed assignments.
    /// Dynamic labels/type/keys/properties use the same frozen reader and
    /// reserve their owned metadata through the expression's cumulative meter.
    pub fn execute_governed_reading<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        elements: impl crate::GraphElementReader<E>,
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
        mut elements: impl crate::GraphElementReader<E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute(
            self,
            policy,
            None,
            owned,
            source,
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            None,
            checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// Execute with per-field admission for computed property maps. Each
    /// actual assigned or removed key is admitted before its proposal and
    /// before any RETURN can observe it, including equal-to-current writes.
    /// Static field admission remains the host's existing preparation check.
    pub fn execute_governed_reading_with_field_admission<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        elements: impl crate::GraphElementReader<E>,
        admit: impl FnMut(PropertyKeyId) -> Result<(), GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        self.execute_governed_over_reading_with_field_admission(
            None, policy, source, elements, admit, checkpoint,
        )
    }

    /// Per-field admission over an owned chain input. The same callback
    /// applies to every dynamic field occurrence before conflict reduction.
    pub fn execute_governed_over_reading_with_field_admission<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut elements: impl crate::GraphElementReader<E>,
        mut admit: impl FnMut(PropertyKeyId) -> Result<(), GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute(
            self,
            policy,
            None,
            owned,
            source,
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            Some(&mut admit),
            checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// Execute a complete multipart input with a reusable frozen graph reader.
    /// Each leaf and each correlated Apply occurrence receives the remaining
    /// allowance from the same statement meter. `elements` shares the source's
    /// pinned overlay and authority; all rows finish before assignments begin.
    /// Owned rows feed the relation's owned leaf when present. A failure of any
    /// source, expression, checkpoint or quota releases no mutation batch.
    pub fn execute_governed_over_reading_with_source<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut elements: impl crate::GraphElementReader<E>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute_erased(
            self,
            policy,
            None,
            owned,
            collect::ErasedSource::Reusable(Box::new(source)),
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            None,
            &mut checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// [`Self::execute_governed_over_reading_with_source`] plus the host's
    /// seeded single-root pattern reader (fgdb-2npty): a MATCH the input
    /// relation joins to its rows, as after a WITH stage, admits only the
    /// roots those rows can join. `seeded` may decline a shape with `None`,
    /// which keeps the unchanged leaf.
    pub fn execute_governed_over_reading_with_seeded_source<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        seeded: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            crate::GraphSeeds<'_>,
            GqlQueryPolicy,
        )
            -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>>,
        mut elements: impl crate::GraphElementReader<E>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute_erased(
            self,
            policy,
            None,
            owned,
            collect::ErasedSource::Seeded {
                patterns: Box::new(source),
                seeded: Box::new(seeded),
            },
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            None,
            &mut checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// Reusable multipart selection with admission of every computed-map
    /// field. Assigned and replacement-removed keys are admitted before any
    /// proposal, deduplication or RETURN observes them, including no-op writes.
    pub fn execute_governed_over_reading_with_source_and_field_admission<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut elements: impl crate::GraphElementReader<E>,
        mut admit: impl FnMut(PropertyKeyId) -> Result<(), GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        collect::execute_erased(
            self,
            policy,
            None,
            owned,
            collect::ErasedSource::Reusable(Box::new(source)),
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            Some(&mut admit),
            &mut checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// Reusable multipart selection with both indexed relational roots and
    /// admission of every actual computed-map field. `None` disables seed
    /// construction entirely for a host whose authority cannot use the index.
    /// A seeded reader returning an error never retries the ordinary source.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_over_reading_with_seeded_source_and_field_admission<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        seeded: Option<
            impl FnMut(
                &PreparedGraphPattern<GraphValueRow>,
                crate::GraphSeeds<'_>,
                GqlQueryPolicy,
            )
                -> Option<Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>>,
        >,
        mut elements: impl crate::GraphElementReader<E>,
        mut admit: impl FnMut(PropertyKeyId) -> Result<(), GqlQueryError<E, C>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationBatch, GqlQueryError<GraphMutationError<E>, C>> {
        let source = match seeded {
            Some(seeded) => collect::ErasedSource::Seeded {
                patterns: Box::new(source),
                seeded: Box::new(seeded),
            },
            None => collect::ErasedSource::Reusable(Box::new(source)),
        };
        collect::execute_erased(
            self,
            policy,
            None,
            owned,
            source,
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            Some(&mut admit),
            &mut checkpoint,
        )
        .map(|(batch, _)| batch)
    }

    /// Application transcript, not a durable format or authorization permit.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-mutation:v1\0".to_vec();
        bytes.extend_from_slice(&self.relation.0.to_be_bytes());
        // Inputs without a sole eager graph source use u64::MAX, which no
        // encoded selection reaches. Their full relation follows below.
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
                GraphMutationAction::SetProperties {
                    value,
                    current,
                    catalog,
                    ..
                } => {
                    bytes.push(7);
                    value.append_canonical_bytes(&mut bytes);
                    bytes.push(u8::from(current.is_some()));
                    if let Some(current) = current {
                        current.append_canonical_bytes(&mut bytes);
                    }
                    crate::set_ops::projection::append_metadata_catalog(
                        crate::GraphElementMetadata::Properties,
                        Some(catalog),
                        &mut bytes,
                    );
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
