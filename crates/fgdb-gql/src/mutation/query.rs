//! Native `SET ... RETURN` over the mutation's own selection occurrences.
//! Rows and intents stay private until the mutation and the whole output
//! succeed; RETURN never rescans the graph or re-evaluates the MATCH.

use super::*;
use crate::algebra::{GraphOrderError, GraphValueOrder};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationQueryBuildError {
    Binding {
        binding: usize,
    },
    TooManyBindings {
        limit: usize,
        observed: usize,
    },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
    InputDepth(crate::GraphSetBuildError),
    /// Distinct property IDs must have distinct output names before a
    /// frozen named map can safely merge newly assigned properties.
    PropertyMapCatalog,
    /// Distinct label IDs must have distinct names before a frozen
    /// `labels()` list can be resolved back to identities and updated.
    LabelCatalog,
}
impl core::fmt::Display for GraphMutationQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Binding { binding } => {
                write!(f, "RETURN binding {binding} names no input column")
            }
            Self::TooManyBindings { limit, observed } => {
                write!(f, "RETURN has {observed} bindings, limit {limit}")
            }
            Self::Projection(error) => error.fmt(f),
            Self::Aggregate(error) => error.fmt(f),
            Self::InputDepth(error) => error.fmt(f),
            Self::PropertyMapCatalog => f.write_str(
                "two properties share one name, so a returned property map is ambiguous",
            ),
            Self::LabelCatalog => {
                f.write_str("two labels share one name, so a returned label list is ambiguous")
            }
        }
    }
}
impl core::error::Error for GraphMutationQueryBuildError {}

/// A private complete proposal plus its RETURN rows, not a commit receipt.
/// Adapters stage the intents atomically and publish the rows only when the
/// transaction outcome permits.
#[derive(Debug)]
pub struct GraphMutationQueryBatch {
    mutation: GraphMutationBatch,
    returning: GqlQueryExecution<GraphValueRow>,
}
impl GraphMutationQueryBatch {
    #[must_use]
    pub const fn mutation(&self) -> &GraphMutationBatch {
        &self.mutation
    }
    /// WHOLE-operation source/work/scratch counters and final RETURN row
    /// count. They already include `mutation().stats()`: never sum them.
    #[must_use]
    pub const fn returning(&self) -> &GqlQueryExecution<GraphValueRow> {
        &self.returning
    }
    #[must_use]
    pub fn into_parts(self) -> (GraphMutationBatch, GqlQueryExecution<GraphValueRow>) {
        (self.mutation, self.returning)
    }
}

/// `MATCH ... SET/REMOVE ... RETURN ...`: one RETURN row per selection row
/// (before DISTINCT and paging), with every property read reflecting the
/// statement's own simultaneous assignments.
#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphMutationQuery {
    mutation: PreparedGraphMutation,
    bindings: Vec<GraphMutationBinding>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
    /// A WITH after the write (`SET ... WITH n WHERE ... RETURN`): row stages
    /// over the head's owned rows. The head above is then the WITH, and the
    /// public schema is the tail's.
    tail: Option<crate::PreparedGraphSet>,
}
impl core::fmt::Debug for PreparedGraphMutationQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphMutationQuery {
    pub fn prepare(
        mutation: PreparedGraphMutation,
        bindings: Vec<GraphMutationBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphMutationQueryBuildError> {
        Self::prepare_with_grouping(mutation, bindings, projection, quantifier, None)
    }

    pub(crate) fn prepare_with_grouping(
        mutation: PreparedGraphMutation,
        bindings: Vec<GraphMutationBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphMutationQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphMutationQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        if let Some(input) = &mutation.input_relation {
            input
                .check_ancestor_depth(2)
                .map_err(GraphMutationQueryBuildError::InputDepth)?;
        }
        let columns = &mutation.columns;
        let deleting = mutation.actions.iter().any(GraphMutationAction::detaches);
        let mut input_types = Vec::new();
        for (at, binding) in bindings.iter().enumerate() {
            let kind = match *binding {
                GraphMutationBinding::Input(column) => columns.get(column).copied(),
                GraphMutationBinding::Property {
                    target, current, ..
                } => (matches!(
                    columns.get(target),
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                ) && columns
                    .get(current)
                    .is_some_and(|kind| kind.is_scalar_like() || *kind == GraphSetColumnType::Any))
                .then_some(GraphSetColumnType::Stored),
                GraphMutationBinding::Properties { target, current } => (matches!(
                    columns.get(target),
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                ) && !(deleting
                    && columns.get(target) == Some(&GraphSetColumnType::Edge))
                    && matches!(columns.get(current), Some(GraphSetColumnType::Any)))
                .then_some(GraphSetColumnType::Any),
                GraphMutationBinding::Labels { target, current } => (columns.get(target)
                    == Some(&GraphSetColumnType::Vertex)
                    && columns.get(current) == Some(&GraphSetColumnType::List))
                .then_some(GraphSetColumnType::List),
            };
            input_types.push(kind.ok_or(GraphMutationQueryBuildError::Binding { binding: at })?);
        }
        if bindings
            .iter()
            .any(|binding| matches!(binding, GraphMutationBinding::Labels { .. }))
        {
            // The frozen list holds names; resolving them back needs both
            // the catalog and one name per label identity.
            let Some(catalog) = mutation.reverse_catalog() else {
                return Err(GraphMutationQueryBuildError::LabelCatalog);
            };
            let mut names = std::collections::BTreeSet::new();
            if !catalog.labels.values().all(|name| names.insert(name)) {
                return Err(GraphMutationQueryBuildError::LabelCatalog);
            }
        }
        if bindings
            .iter()
            .any(|binding| matches!(binding, GraphMutationBinding::Properties { .. }))
            && let Some(catalog) = mutation.reverse_catalog()
        {
            let mut names = std::collections::BTreeSet::new();
            if !catalog.properties.values().all(|name| names.insert(name)) {
                return Err(GraphMutationQueryBuildError::PropertyMapCatalog);
            }
        }
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphMutationQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or(input_types);
        // Complete expression/name/schema admission includes lazy branches.
        // Grouped outputs address native keys then aggregates, not bindings.
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphMutationQueryBuildError::Projection)?;
        let output = checked.columns().map(str::to_owned).collect();
        let types = checked.column_types().to_vec();
        Ok(Self {
            mutation,
            bindings,
            projection,
            grouping,
            quantifier,
            columns: output,
            types,
            order: Vec::new(),
            offset: 0,
            count: None,
            tail: None,
        })
    }

    /// Continue the head (now a WITH) with row stages over its owned rows.
    /// The statement's output schema becomes the tail's.
    pub(crate) fn with_tail(mut self, tail: crate::PreparedGraphSet) -> Self {
        self.columns = tail.columns().to_vec();
        self.types = tail.column_types().to_vec();
        self.tail = Some(tail);
        self
    }

    #[must_use]
    pub const fn mutation(&self) -> &PreparedGraphMutation {
        &self.mutation
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    pub fn with_order_by(mut self, order: &[GraphValueOrder]) -> Result<Self, GraphOrderError> {
        if order.is_empty() {
            return Err(GraphOrderError::EmptyOrder);
        }
        if order.len() > self.columns.len() {
            return Err(GraphOrderError::TooManyColumns {
                limit: self.columns.len(),
                observed: order.len(),
            });
        }
        for (at, key) in order.iter().enumerate() {
            if key.column >= self.columns.len() {
                return Err(GraphOrderError::UnknownColumn { column: key.column });
            }
            if order[..at].iter().any(|prior| prior.column == key.column) {
                return Err(GraphOrderError::DuplicateColumn { column: key.column });
            }
        }
        self.order = order.to_vec();
        Ok(self)
    }
    #[must_use]
    pub fn with_page(mut self, offset: u64, count: Option<u64>) -> Self {
        self.offset = offset;
        self.count = count;
        self
    }

    /// Selection, proposals, RETURN bindings and projection share one
    /// work/scratch allowance. The result-row allowance applies only AFTER
    /// projection, DISTINCT, ORDER BY and paging: it never limits which
    /// occurrences the mutation updates, and LIMIT 0 still proposes every
    /// assignment and evaluates every RETURN expression. No rows or intents
    /// escape if any expression, conflict check or budget fails.
    pub fn execute_governed<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<E>, C>> {
        self.execute_governed_over(None, policy, source, checkpoint)
    }

    /// The last write of a chain with RETURN (fgdb-cpny9): `owned` are the
    /// rows the previous write returned, read by the mutation's one owned
    /// leaf. `None` is [`Self::execute_governed`].
    pub fn execute_governed_over<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<E>, C>> {
        self.execute_with(owned, policy, source, None, checkpoint)
    }

    /// [`Self::execute_governed`] with the host's element property reader
    /// (fgdb-694ga), as [`PreparedGraphMutation::execute_governed_reading`].
    /// RETURN retains its existing final binding semantics; this reader never
    /// substitutes old values for them.
    pub fn execute_governed_reading<E, C>(
        &self,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        elements: impl crate::GraphElementReader<E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<E>, C>> {
        self.execute_governed_over_reading(None, policy, source, elements, checkpoint)
    }

    /// [`Self::execute_governed_reading`] over already admitted chain rows.
    /// Both source callbacks use the same authority and frozen overlay.
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
    ) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<E>, C>> {
        self.execute_with(
            owned,
            policy,
            source,
            Some(&mut crate::set_ops::projection::HostElements(&mut elements)),
            checkpoint,
        )
    }

    fn execute_with<E, C>(
        &self,
        owned: Option<Vec<GraphValueRow>>,
        policy: GraphMutationPolicy,
        source: impl FnOnce(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        mut elements: Option<&mut super::collect::MutationElementReader<'_, E>>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphMutationQueryBatch, GqlQueryError<GraphMutationQueryError<E>, C>> {
        // Every engine below (selection, aggregate, ordering, tail) sees one
        // checkpoint type per error-type pair, not one per caller (fgdb-2goz).
        let mut checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        let mut selection = policy;
        selection.query = GqlQueryPolicy::new(
            policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
            u64::MAX,
            policy.query.evaluator.max_work_units,
            policy.query.evaluator.max_scratch_entries,
        );
        let (mutation, value) = collect::execute(
            &self.mutation,
            selection,
            Some(&self.bindings),
            owned,
            source,
            elements.as_deref_mut(),
            &mut checkpoint,
        )
        .map_err(|error| error.map_source(GraphMutationQueryError::Mutation))?;
        let prefix = mutation.stats();
        let mut returning = GqlQueryExecution {
            value,
            rows: prefix.selection,
            evaluator: prefix.evaluator,
        };
        // Bound fields already carry their final proposals. Computed element
        // identities need the same post-write view for the head, grouping and
        // every tail. Index borrowed proposals once, without cloning payloads;
        // charge each retained entry before allocating it.
        let reads_elements = self.tail.is_some()
            || self
                .projection
                .iter()
                .any(|value| value.value().may_read_element_properties())
            || self
                .grouping
                .as_ref()
                .is_some_and(crate::set_ops::WriteReturnGroup::may_read_element_properties);
        let mut post_write = {
            let mut charges = crate::algebra_exec::Charges::new(
                &mut checkpoint,
                policy.query,
                returning.rows.snapshot_records,
            );
            charges.rows = returning.rows;
            charges.evaluator = returning.evaluator;
            let post_write = PostWriteReads::new(
                if reads_elements {
                    mutation.intents()
                } else {
                    &[]
                },
                elements,
                &mut |event| charges.event::<GraphMutationQueryError<E>, C>(event),
            )?;
            returning.evaluator = charges.evaluator;
            post_write
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate_reading(
                returning,
                policy.query,
                grouping,
                &mut post_write,
                &mut checkpoint,
            )
            .map_err(|error| error.map_source(GraphMutationQueryError::Returning))?;
        }
        // A WITH head's rows are private input to its tail.
        let head = if self.tail.is_some() {
            crate::set_ops::owned_head_policy(policy.query)
        } else {
            policy.query
        };
        let returning = crate::set_ops::finish_owned_projection_reading(
            returning,
            head,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            &mut post_write,
            &mut checkpoint,
        )
        .map_err(|error| error.map_source(GraphMutationQueryError::Returning))?;
        let returning = match &self.tail {
            None => returning,
            Some(tail) => {
                // The tail reads this statement's post-write rows: an element
                // property it wrote reads that write, any other the host's
                // value before the statement (fgdb-122y1).
                crate::set_ops::finish_owned_tail_reading(
                    returning,
                    tail,
                    policy.query,
                    &mut post_write,
                    checkpoint,
                )
                .map_err(|error| error.map_source(GraphMutationQueryError::Returning))?
            }
        };
        drop(post_write);
        Ok(GraphMutationQueryBatch {
            mutation,
            returning,
        })
    }

    /// Definition identity, not a durable effect format. As in native set
    /// projection, aliases are output schema metadata.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-mutation-query:v1\0".to_vec();
        let mutation = self.mutation.canonical_bytes();
        bytes.extend_from_slice(&(mutation.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&mutation);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match *binding {
                GraphMutationBinding::Input(column) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&(column as u64).to_be_bytes());
                }
                GraphMutationBinding::Property {
                    target,
                    key,
                    current,
                } => {
                    bytes.push(1);
                    bytes.extend_from_slice(&(target as u64).to_be_bytes());
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                    bytes.extend_from_slice(&(current as u64).to_be_bytes());
                }
                GraphMutationBinding::Properties { target, current } => {
                    bytes.push(2);
                    bytes.extend_from_slice(&(target as u64).to_be_bytes());
                    bytes.extend_from_slice(&(current as u64).to_be_bytes());
                }
                GraphMutationBinding::Labels { target, current } => {
                    bytes.push(3);
                    bytes.extend_from_slice(&(target as u64).to_be_bytes());
                    bytes.extend_from_slice(&(current as u64).to_be_bytes());
                }
            }
        }
        bytes.extend_from_slice(&(self.projection.len() as u64).to_be_bytes());
        for column in &self.projection {
            column.value().append_canonical_bytes(&mut bytes);
        }
        bytes.push(u8::from(self.quantifier == GraphSetQuantifier::Distinct));
        bytes.extend_from_slice(&(self.order.len() as u64).to_be_bytes());
        for key in &self.order {
            bytes.extend_from_slice(&(key.column as u64).to_be_bytes());
            bytes.extend_from_slice(&[u8::from(key.descending), u8::from(key.nulls_first)]);
        }
        bytes.extend_from_slice(&self.offset.to_be_bytes());
        bytes.push(u8::from(self.count.is_some()));
        if let Some(count) = self.count {
            bytes.extend_from_slice(&count.to_be_bytes());
        }
        if let Some(grouping) = &self.grouping {
            grouping.append_canonical_bytes(&mut bytes);
        }
        if self
            .bindings
            .iter()
            .any(|binding| matches!(binding, GraphMutationBinding::Properties { .. }))
        {
            crate::algebra::append_property_catalog_transcript(
                self.mutation.reverse_catalog(),
                &mut bytes,
            );
        }
        if self
            .bindings
            .iter()
            .any(|binding| matches!(binding, GraphMutationBinding::Labels { .. }))
        {
            crate::algebra::append_label_catalog_transcript(
                self.mutation.reverse_catalog(),
                &mut bytes,
            );
        }
        if let Some(tail) = &self.tail {
            bytes.extend_from_slice(b"fgdb:write-tail:v1\0");
            let tail = tail.canonical_bytes();
            bytes.extend_from_slice(&(tail.len() as u64).to_be_bytes());
            bytes.extend_from_slice(&tail);
        }
        bytes
    }
}

#[derive(Debug)]
pub enum GraphMutationQueryError<E> {
    Mutation(GraphMutationError<E>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display> core::fmt::Display for GraphMutationQueryError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Mutation(error) => error.fmt(f),
            Self::Returning(error) => write!(f, "mutation RETURN: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for GraphMutationQueryError<E> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Mutation(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

/// Element reads over a write's post-write rows: the statement's own final
/// property and label proposals overlay the host's read before the
/// statement. A read of an element the statement deletes, or of any
/// relationship once a DETACH DELETE may have cascaded to it, stays a typed
/// refusal, never a value the statement removed.
struct PostWriteReads<'i, 'r, 'e, E> {
    written:
        std::collections::BTreeMap<(ElementId, PropertyKeyId), Option<&'i CanonicalPropertyValue>>,
    labels: std::collections::BTreeMap<(VId, LabelId), bool>,
    deleted: std::collections::BTreeSet<ElementId>,
    cascading: bool,
    base: Option<&'r mut super::collect::MutationElementReader<'e, E>>,
}

impl<'i, 'r, 'e, E> PostWriteReads<'i, 'r, 'e, E> {
    fn new<T>(
        intents: &'i [GraphMutationIntent],
        base: Option<&'r mut super::collect::MutationElementReader<'e, E>>,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), T>,
    ) -> Result<Self, T> {
        let mut written = std::collections::BTreeMap::new();
        let mut labels = std::collections::BTreeMap::new();
        let mut deleted = std::collections::BTreeSet::new();
        let mut cascading = false;
        for intent in intents {
            control(crate::GlaExecutionEvent::ScratchEntry)?;
            match intent {
                GraphMutationIntent::Property { vertex, key, value } => {
                    written.insert((ElementId::Vertex(*vertex), *key), value.as_ref());
                }
                GraphMutationIntent::EdgeProperty { edge, key, value } => {
                    written.insert((ElementId::Edge(*edge), *key), value.as_ref());
                }
                GraphMutationIntent::StoredProperty { target, key, value } => {
                    written.insert((*target, *key), Some(value));
                }
                GraphMutationIntent::Label {
                    vertex,
                    label,
                    present,
                } => {
                    labels.insert((*vertex, *label), *present);
                }
                GraphMutationIntent::DetachDelete { vertex } => {
                    deleted.insert(ElementId::Vertex(*vertex));
                    cascading = true;
                }
                GraphMutationIntent::Delete { vertex } => {
                    deleted.insert(ElementId::Vertex(*vertex));
                }
                GraphMutationIntent::DeleteEdge { edge } => {
                    deleted.insert(ElementId::Edge(*edge));
                }
            }
        }
        Ok(Self {
            written,
            labels,
            deleted,
            cascading,
            base,
        })
    }

    fn deleted(&self, element: ElementId) -> bool {
        self.deleted.contains(&element) || (self.cascading && matches!(element, ElementId::Edge(_)))
    }
}

impl<E> crate::set_ops::projection::ElementReader<E> for PostWriteReads<'_, '_, '_, E> {
    fn property(
        &mut self,
        element: ElementId,
        key: PropertyKeyId,
    ) -> Option<Result<Option<CanonicalPropertyValue>, E>> {
        if self.deleted(element) {
            return None;
        }
        if let Some(value) = self.written.get(&(element, key)) {
            return Some(Ok(value.cloned()));
        }
        self.base.as_mut()?.property(element, key)
    }

    fn metadata(
        &mut self,
        element: ElementId,
        kind: crate::GraphElementMetadata,
        catalog: &crate::ReverseSymbolCatalog,
        control: &mut dyn FnMut(crate::GlaExecutionEvent) -> Result<(), ()>,
    ) -> Option<Result<Option<crate::algebra::GraphValue>, crate::GraphElementMetadataError<E>>>
    {
        if self.deleted(element) {
            return None;
        }
        let value = match self
            .base
            .as_mut()?
            .metadata(element, kind, catalog, control)?
        {
            Ok(Some(value)) => value,
            other => return Some(other),
        };
        let mut charge = |event| control(event).map_err(|()| MetadataOverlayError::Interrupted);
        match self.overlay_metadata(element, kind, catalog, value, &mut charge) {
            Ok(value) => Some(Ok(Some(value))),
            Err(MetadataOverlayError::Interrupted) => {
                Some(Err(crate::GraphElementMetadataError::Interrupted))
            }
            // An unavailable/ambiguous catalog cannot manufacture absent
            // fields. Keep the evaluator's typed unsupported-read refusal.
            Err(MetadataOverlayError::Invalid) => None,
        }
    }
}

enum MetadataOverlayError {
    Interrupted,
    Invalid,
}

impl<E> PostWriteReads<'_, '_, '_, E> {
    fn overlay_metadata(
        &self,
        element: ElementId,
        kind: crate::GraphElementMetadata,
        catalog: &crate::ReverseSymbolCatalog,
        value: crate::algebra::GraphValue,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), MetadataOverlayError>,
    ) -> Result<crate::algebra::GraphValue, MetadataOverlayError> {
        use crate::GlaExecutionEvent::{ScratchEntry, Work};
        use crate::GraphElementMetadata::{Keys, Labels, Properties, Type};
        use crate::algebra::GraphValue;
        if !crate::set_ops::projection::metadata_value_valid(kind, &value) {
            return Err(MetadataOverlayError::Invalid);
        }
        control(Work)?;
        match kind {
            Type => Ok(value),
            Labels => {
                let ElementId::Vertex(vertex) = element else {
                    return Err(MetadataOverlayError::Invalid);
                };
                let changes = self
                    .labels
                    .range((vertex, LabelId(0))..=(vertex, LabelId(u64::MAX)));
                if changes.clone().next().is_none() {
                    return Ok(value);
                }
                let reverse = metadata_reverse_names(&catalog.labels, control)?;
                let GraphValue::List(values) = value else {
                    return Err(MetadataOverlayError::Invalid);
                };
                let mut labels = std::collections::BTreeSet::new();
                for value in values.iter() {
                    control(Work)?;
                    let GraphValue::Scalar(CanonicalScalar::Text(name)) = value else {
                        return Err(MetadataOverlayError::Invalid);
                    };
                    let label = reverse
                        .get(name.as_str())
                        .ok_or(MetadataOverlayError::Invalid)?;
                    control(ScratchEntry)?;
                    labels.insert(*label);
                }
                for ((_, label), present) in changes {
                    control(Work)?;
                    if *present {
                        if !catalog.labels.contains_key(label) {
                            return Err(MetadataOverlayError::Invalid);
                        }
                        control(ScratchEntry)?;
                        labels.insert(*label);
                    } else {
                        labels.remove(label);
                    }
                }
                control(ScratchEntry)?;
                let mut names = Vec::new();
                for label in labels {
                    if names.len() >= GraphValue::MAX_LIST_NODES - 1 {
                        return Err(MetadataOverlayError::Invalid);
                    }
                    names.push(metadata_text(&catalog.labels[&label], control)?);
                }
                Ok(GraphValue::List(names.into_boxed_slice()))
            }
            Properties | Keys => {
                let changes = self
                    .written
                    .range((element, PropertyKeyId(0))..=(element, PropertyKeyId(u64::MAX)));
                if changes.clone().next().is_none() {
                    return Ok(value);
                }
                // Only names participate in this validation. In particular a
                // keys() read never touches any assigned property's payload.
                let _reverse = metadata_reverse_names(&catalog.properties, control)?;
                let mut updates = std::collections::BTreeMap::new();
                for ((_, key), value) in changes {
                    control(Work)?;
                    let name = catalog
                        .properties
                        .get(key)
                        .ok_or(MetadataOverlayError::Invalid)?;
                    control(ScratchEntry)?;
                    updates.insert(name.as_str(), *value);
                }
                if kind == Keys {
                    let GraphValue::List(values) = value else {
                        return Err(MetadataOverlayError::Invalid);
                    };
                    let mut keys = std::collections::BTreeSet::new();
                    for value in values.iter() {
                        control(Work)?;
                        let GraphValue::Scalar(CanonicalScalar::Text(name)) = value else {
                            return Err(MetadataOverlayError::Invalid);
                        };
                        control(ScratchEntry)?;
                        keys.insert(name.as_str());
                    }
                    for (key, value) in updates {
                        control(Work)?;
                        if value.is_some() {
                            control(ScratchEntry)?;
                            keys.insert(key);
                        } else {
                            keys.remove(key);
                        }
                    }
                    control(ScratchEntry)?;
                    let mut names = Vec::new();
                    for key in keys {
                        if names.len() >= GraphValue::MAX_LIST_NODES - 1 {
                            return Err(MetadataOverlayError::Invalid);
                        }
                        names.push(metadata_text(key, control)?);
                    }
                    return Ok(GraphValue::List(names.into_boxed_slice()));
                }
                let GraphValue::Map { keys, values } = value else {
                    return Err(MetadataOverlayError::Invalid);
                };
                let mut old = keys
                    .into_vec()
                    .into_iter()
                    .zip(values.into_vec())
                    .peekable();
                control(ScratchEntry)?;
                control(ScratchEntry)?;
                let mut keys = Vec::new();
                let mut values = Vec::new();
                let mut append = |key, value| {
                    if keys.len() >= GraphValue::MAX_LIST_NODES - 1 {
                        return Err(MetadataOverlayError::Invalid);
                    }
                    keys.push(key);
                    values.push(value);
                    Ok(())
                };
                for (key, value) in updates {
                    while let Some((previous, _)) = old.peek() {
                        control(Work)?;
                        if previous.as_bytes() >= key.as_bytes() {
                            break;
                        }
                        control(ScratchEntry)?;
                        control(ScratchEntry)?;
                        let (key, value) = old.next().expect("peeked map entry");
                        append(key, value)?;
                    }
                    if old
                        .peek()
                        .is_some_and(|(previous, _)| previous.as_ref() == key)
                    {
                        old.next();
                    }
                    if let Some(value) = value {
                        control(ScratchEntry)?;
                        for _ in 0..key
                            .len()
                            .div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                        {
                            control(Work)?;
                            control(ScratchEntry)?;
                        }
                        GraphValue::charge_property_value(value, control)?;
                        let value = GraphValue::from_property_value(value)
                            .ok_or(MetadataOverlayError::Invalid)?;
                        control(ScratchEntry)?;
                        control(ScratchEntry)?;
                        append(key.into(), value)?;
                    }
                }
                for (key, value) in old {
                    control(Work)?;
                    control(ScratchEntry)?;
                    control(ScratchEntry)?;
                    append(key, value)?;
                }
                // Unmodified values move from the admitted base. Only new
                // keys and assigned payloads are copied, under this control.
                Ok(GraphValue::Map {
                    keys: keys.into_boxed_slice(),
                    values: values.into_boxed_slice(),
                })
            }
        }
    }
}

fn metadata_reverse_names<'a, K: Copy + Ord>(
    catalog: &'a std::collections::BTreeMap<K, String>,
    control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), MetadataOverlayError>,
) -> Result<std::collections::BTreeMap<&'a str, K>, MetadataOverlayError> {
    let mut names = std::collections::BTreeMap::new();
    for (key, name) in catalog {
        control(crate::GlaExecutionEvent::Work)?;
        control(crate::GlaExecutionEvent::ScratchEntry)?;
        if names.insert(name.as_str(), *key).is_some() {
            return Err(MetadataOverlayError::Invalid);
        }
    }
    Ok(names)
}

fn metadata_text(
    name: &str,
    control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), MetadataOverlayError>,
) -> Result<crate::algebra::GraphValue, MetadataOverlayError> {
    control(crate::GlaExecutionEvent::ScratchEntry)?;
    for _ in 0..name
        .len()
        .div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
    {
        control(crate::GlaExecutionEvent::Work)?;
        control(crate::GlaExecutionEvent::ScratchEntry)?;
    }
    CanonicalScalar::ucs_basic_text(name)
        .map(crate::algebra::GraphValue::Scalar)
        .map_err(|_| MetadataOverlayError::Invalid)
}
