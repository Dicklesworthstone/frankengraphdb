//! Native CREATE RETURN over the collector's exact occurrence bindings.
//! Rows and intents stay private until creation and the entire output succeed.

use crate::algebra::{GraphOrderError, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use crate::insertion::{
    GraphInsertBatch, GraphInsertError, GraphInsertPolicy, GraphInsertRequest, PreparedGraphInsert,
};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetBuildError, GraphSetColumnType,
    GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier, GraphSetSource,
};
use fgdb_delta_types::{ElementId, PropertyKeyId};

/// One private column of the per-occurrence RETURN input. Property bindings
/// read the already-checked CREATE values, and missing keys yield scalar NULL.
/// They do not issue graph reads or evaluate CREATE expressions again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphInsertBinding {
    Input(usize),
    CreatedVertex(usize),
    VertexProperty {
        vertex: usize,
        key: PropertyKeyId,
    },
    CreatedEdge(usize),
    EdgeProperty {
        edge: usize,
        key: PropertyKeyId,
    },
    /// Complete checked CREATE fields, named by the query's property catalog.
    VertexProperties(usize),
    EdgeProperties(usize),
    /// The created vertex's CREATE labels, named by the query's catalog in
    /// label-identity order, as every labels() read names them (Graph3).
    VertexLabels(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphInsertQueryBuildError {
    Binding {
        binding: usize,
    },
    TooManyBindings {
        limit: usize,
        observed: usize,
    },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
    InputDepth(GraphSetBuildError),
    /// A property of a returned created element has no trusted output name.
    UnmappedProperty {
        binding: usize,
    },
    /// Distinct property IDs cannot share one output name.
    PropertyMapCatalog,
    /// A label of a returned created vertex has no trusted output name.
    UnmappedLabel {
        binding: usize,
    },
}
impl core::fmt::Display for GraphInsertQueryBuildError {
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
            Self::UnmappedProperty { binding } => write!(
                f,
                "a property of created element {binding} has no bound name to return it under"
            ),
            Self::PropertyMapCatalog => f.write_str(
                "two properties share one name, so a returned property map is ambiguous",
            ),
            Self::UnmappedLabel { binding } => write!(
                f,
                "a label of created vertex {binding} has no bound name to return it under"
            ),
        }
    }
}
impl core::error::Error for GraphInsertQueryBuildError {}

#[derive(Debug)]
pub enum GraphInsertQueryError<E, A> {
    Insertion(GraphInsertError<E, A>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphInsertQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Insertion(error) => error.fmt(f),
            Self::Returning(error) => write!(f, "insertion RETURN: {error}"),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphInsertQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Insertion(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

/// A private complete proposal, not a commit receipt. Adapters must stage the
/// intents atomically and retain rows until their transaction outcome permits
/// publication. No partial result exists on expression, budget or cancellation
/// failure, although the allocator may already have spent identities.
#[derive(Debug, PartialEq, Eq)]
pub struct GraphInsertQueryBatch {
    insertion: GraphInsertBatch,
    returning: GqlQueryExecution<GraphValueRow>,
}
impl GraphInsertQueryBatch {
    /// Creation counts and the cumulative prefix through binding collection.
    #[must_use]
    pub const fn insertion(&self) -> &GraphInsertBatch {
        &self.insertion
    }
    /// WHOLE-operation source/work/scratch counters and final RETURN row count.
    /// These counters already include `insertion().stats()`: never sum them.
    #[must_use]
    pub const fn returning(&self) -> &GqlQueryExecution<GraphValueRow> {
        &self.returning
    }
    #[must_use]
    pub fn into_parts(self) -> (GraphInsertBatch, GqlQueryExecution<GraphValueRow>) {
        (self.insertion, self.returning)
    }
}

/// An admitted CREATE occurrence bag whose result expressions have not run.
/// The insertion and output remain private to the host's rollback workspace.
/// Stage the complete insertion before evaluating `output` with a reader over
/// that workspace; never publish either part until the statement succeeds.
#[derive(Debug)]
pub struct GraphInsertQueryCollection<'query> {
    insertion: GraphInsertBatch,
    output: GraphInsertQueryOutput<'query>,
}

impl<'query> GraphInsertQueryCollection<'query> {
    #[must_use]
    pub fn into_parts(self) -> (GraphInsertBatch, GraphInsertQueryOutput<'query>) {
        (self.insertion, self.output)
    }
}

/// The output phase is bound to its original prepared query, occurrence bag
/// and cumulative allowance. It cannot be reused with another definition or
/// reset the creation prefix. Its element reader must observe the complete
/// CREATE staged privately over the same frozen input snapshot.
#[derive(Debug)]
pub struct GraphInsertQueryOutput<'query> {
    query: &'query PreparedGraphInsertQuery,
    policy: GqlQueryPolicy,
    input: GqlQueryExecution<GraphValueRow>,
}

impl GraphInsertQueryOutput<'_> {
    /// Evaluate the complete RETURN/WITH head, grouping and row tail after
    /// private staging. The trusted host owns point-read dependencies,
    /// authorization and rollback if any output expression or budget fails.
    /// Result paging cannot suppress expression evaluation or creation.
    pub fn finish_governed_reading<E, C>(
        self,
        mut elements: impl crate::GraphElementReader<E>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.finish(
            &mut crate::set_ops::projection::HostElements(&mut elements),
            &mut checkpoint,
        )
    }

    fn finish<E, C>(
        self,
        elements: &mut crate::set_ops::ElementRead<'_, E>,
        mut checkpoint: &mut dyn FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        let query = self.query;
        let mut returning = self.input;
        if let Some(grouping) = &query.grouping {
            returning = crate::set_ops::finish_owned_aggregate_reading(
                returning,
                self.policy,
                grouping,
                elements,
                &mut checkpoint,
            )?;
        }
        let head = if query.tail.is_some() {
            crate::set_ops::owned_head_policy(self.policy)
        } else {
            self.policy
        };
        let returning = crate::set_ops::finish_owned_projection_reading(
            returning,
            head,
            &query.projection,
            query.quantifier,
            &query.order,
            (query.offset, query.count),
            elements,
            &mut checkpoint,
        )?;
        match &query.tail {
            None => Ok(returning),
            Some(tail) => crate::set_ops::finish_owned_tail_reading(
                returning,
                tail,
                self.policy,
                elements,
                checkpoint,
            ),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphInsertQuery {
    insertion: PreparedGraphInsert,
    bindings: Vec<GraphInsertBinding>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
    property_catalog: Option<std::sync::Arc<crate::ReverseSymbolCatalog>>,
    /// A WITH after the CREATE (`CREATE (n ...) WITH n WHERE ... RETURN`):
    /// row stages over the head's owned rows, whose schema is the output.
    tail: Option<crate::PreparedGraphSet>,
}
impl core::fmt::Debug for PreparedGraphInsertQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphInsertQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphInsertQuery {
    pub fn prepare(
        insertion: PreparedGraphInsert,
        bindings: Vec<GraphInsertBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphInsertQueryBuildError> {
        Self::prepare_with_grouping(insertion, bindings, projection, quantifier, None, None)
    }

    /// Complete maps of newly created elements use these trusted property
    /// names. Names and their uniqueness are admitted before source access or
    /// identity allocation; names are never guessed from creation text.
    pub fn prepare_with_property_catalog(
        insertion: PreparedGraphInsert,
        bindings: Vec<GraphInsertBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        catalog: crate::ReverseSymbolCatalog,
    ) -> Result<Self, GraphInsertQueryBuildError> {
        Self::prepare_with_grouping(
            insertion,
            bindings,
            projection,
            quantifier,
            None,
            Some(std::sync::Arc::new(catalog)),
        )
    }

    pub(crate) fn prepare_with_grouping(
        insertion: PreparedGraphInsert,
        bindings: Vec<GraphInsertBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
        property_catalog: Option<std::sync::Arc<crate::ReverseSymbolCatalog>>,
    ) -> Result<Self, GraphInsertQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphInsertQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        if let Some(input) = insertion.relational_selection() {
            input
                .check_ancestor_depth(2)
                .map_err(GraphInsertQueryBuildError::InputDepth)?;
        }
        let mut input_types = Vec::new();
        for (at, binding) in bindings.iter().enumerate() {
            let kind = match *binding {
                GraphInsertBinding::Input(column) => {
                    insertion.input_column_types().get(column).copied()
                }
                GraphInsertBinding::CreatedVertex(vertex) => {
                    (vertex < insertion.vertices_per_row()).then_some(GraphSetColumnType::Vertex)
                }
                GraphInsertBinding::VertexProperty { vertex, .. } => {
                    (vertex < insertion.vertices_per_row()).then_some(GraphSetColumnType::Stored)
                }
                GraphInsertBinding::CreatedEdge(edge) => {
                    (edge < insertion.edges_per_row()).then_some(GraphSetColumnType::Edge)
                }
                GraphInsertBinding::EdgeProperty { edge, .. } => {
                    (edge < insertion.edges_per_row()).then_some(GraphSetColumnType::Stored)
                }
                GraphInsertBinding::VertexProperties(vertex) => {
                    (vertex < insertion.vertices_per_row()).then_some(GraphSetColumnType::Any)
                }
                GraphInsertBinding::EdgeProperties(edge) => {
                    (edge < insertion.edges_per_row()).then_some(GraphSetColumnType::Any)
                }
                GraphInsertBinding::VertexLabels(vertex) => {
                    (vertex < insertion.vertices_per_row()).then_some(GraphSetColumnType::List)
                }
            };
            input_types.push(kind.ok_or(GraphInsertQueryBuildError::Binding { binding: at })?);
        }
        let maps = bindings.iter().any(|binding| {
            matches!(
                binding,
                GraphInsertBinding::VertexProperties(_) | GraphInsertBinding::EdgeProperties(_)
            )
        });
        let labels = bindings
            .iter()
            .any(|binding| matches!(binding, GraphInsertBinding::VertexLabels(_)));
        let property_catalog = if maps || labels {
            if maps && let Some(catalog) = &property_catalog {
                let mut names = std::collections::BTreeSet::new();
                if !catalog.properties.values().all(|name| names.insert(name)) {
                    return Err(GraphInsertQueryBuildError::PropertyMapCatalog);
                }
            }
            // Every name a created vertex's labels() returns is admitted here,
            // before source access or identity allocation.
            for (at, binding) in bindings.iter().enumerate() {
                if let Some(labels) = insertion.return_label_ids(*binding) {
                    for label in labels {
                        if !property_catalog.as_ref().is_some_and(|catalog| {
                            catalog.labels.get(label).is_some_and(|name| {
                                fgdb_types::CanonicalScalar::ucs_basic_text(name).is_ok()
                            })
                        }) {
                            return Err(GraphInsertQueryBuildError::UnmappedLabel { binding: at });
                        }
                    }
                }
            }
            for (at, binding) in bindings.iter().enumerate() {
                if let Some(keys) = insertion.return_property_keys(*binding) {
                    for key in keys {
                        if !property_catalog
                            .as_ref()
                            .is_some_and(|catalog| catalog.properties.contains_key(key))
                        {
                            return Err(GraphInsertQueryBuildError::UnmappedProperty {
                                binding: at,
                            });
                        }
                    }
                }
            }
            property_catalog
        } else {
            None
        };
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphInsertQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or(input_types);
        // Complete expression/name/schema admission includes lazy branches.
        // Grouped outputs address native keys then aggregates, not bindings.
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphInsertQueryBuildError::Projection)?;
        let columns = checked.columns().map(str::to_owned).collect();
        let types = checked.column_types().to_vec();
        Ok(Self {
            insertion,
            bindings,
            projection,
            grouping,
            quantifier,
            columns,
            types,
            order: Vec::new(),
            offset: 0,
            count: None,
            property_catalog,
            tail: None,
        })
    }

    #[must_use]
    pub const fn insertion(&self) -> &PreparedGraphInsert {
        &self.insertion
    }
    /// The frozen occurrence bindings, including private continuation columns.
    /// Authorized hosts use these before execution to admit property reads on
    /// created elements; a hidden WITH column is subject to the same scope as
    /// a delivered RETURN column.
    #[must_use]
    pub fn bindings(&self) -> &[GraphInsertBinding] {
        &self.bindings
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

    /// Continue the head (now a WITH) with row stages over its owned rows.
    /// The statement's output schema becomes the tail's.
    pub(crate) fn with_tail(mut self, tail: crate::PreparedGraphSet) -> Self {
        self.columns = tail.columns().to_vec();
        self.types = tail.column_types().to_vec();
        self.tail = Some(tail);
        self
    }

    /// Source selection, frozen properties, all identities and RETURN share one
    /// work/scratch allowance. The result-row allowance applies only AFTER
    /// projection, DISTINCT, ORDER BY and paging; it never limits creation.
    /// `max_vertices`/`max_edges` still admit all creation counts before any ID.
    /// LIMIT 0 evaluates every source and RETURN expression and creates the same
    /// structures. No rows/intents escape if any later expression fails.
    pub fn execute_governed<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        source: impl FnMut(
            &PreparedGraphPattern<GraphValueRow>,
            GqlQueryPolicy,
        ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<E, C>>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<E, A>, C>> {
        self.execute_governed_with_source(policy, source, allocate, checkpoint)
    }

    /// CREATE and its complete RETURN tail with one explicit graph source
    /// owner. Factor-aware and ordinary sources use the same private proposal,
    /// expression validation, allocation boundary and cumulative allowance.
    pub fn execute_governed_with_source<E, A, C>(
        &self,
        policy: GraphInsertPolicy,
        source: impl GraphSetSource<E, C>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<E, A>, C>> {
        self.execute_governed_over(None, policy, source, allocate, checkpoint)
    }

    /// One step of a chained write (`CREATE (a) WITH a CREATE (b)`): `rows`
    /// are the rows the previous write of the same statement returned, and
    /// this CREATE's relational input reads them through its one owned leaf.
    /// The host must have staged that write first, so reads see its effects.
    /// `None` is [`Self::execute_governed_with_source`].
    pub fn execute_governed_over<E, A, C>(
        &self,
        rows: Option<Vec<GraphValueRow>>,
        policy: GraphInsertPolicy,
        source: impl GraphSetSource<E, C>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertQueryBatch, GqlQueryError<GraphInsertQueryError<E, A>, C>> {
        let collection =
            self.collect_governed_over(rows, policy, source, allocate, &mut checkpoint)?;
        let (insertion, output) = collection.into_parts();
        // A pure proposal has not staged its created identities. Preserve a
        // typed refusal for graph point reads instead of reading them as
        // missing from the pre-CREATE source and inventing NULL results.
        let returning = output
            .finish(&mut crate::set_ops::no_element_reads, &mut checkpoint)
            .map_err(|error| error.map_source(GraphInsertQueryError::Returning))?;
        Ok(GraphInsertQueryBatch {
            insertion,
            returning,
        })
    }

    /// Freeze selection and creation payloads, admit the whole creation bag,
    /// then allocate its identities. The returned output retains this query
    /// and the spent allowance. Hosts stage the original intents atomically
    /// in a private rollback workspace before finishing computed output reads;
    /// no later occurrence may affect this phase's source selection.
    pub fn collect_governed_over<E, A, C>(
        &self,
        rows: Option<Vec<GraphValueRow>>,
        policy: GraphInsertPolicy,
        source: impl GraphSetSource<E, C>,
        allocate: impl FnMut(GraphInsertRequest) -> Result<ElementId, A>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GraphInsertQueryCollection<'_>, GqlQueryError<GraphInsertQueryError<E, A>, C>> {
        // Every engine below (creation, aggregate, ordering, tail) sees one
        // checkpoint type per error-type pair, not one per caller (fgdb-2goz).
        let mut checkpoint: &mut dyn FnMut() -> Result<(), C> = &mut checkpoint;
        let mut creation_policy = policy;
        creation_policy.query = GqlQueryPolicy::new(
            policy.query.rows.max_snapshot_records().unwrap_or(u64::MAX),
            u64::MAX,
            policy.query.evaluator.max_work_units,
            policy.query.evaluator.max_scratch_entries,
        );
        let (insertion, value) = self
            .insertion
            .execute_returning(
                creation_policy,
                &self.bindings,
                self.property_catalog.as_deref(),
                rows,
                source,
                allocate,
                &mut checkpoint,
            )
            .map_err(|error| error.map_source(GraphInsertQueryError::Insertion))?;
        let prefix = insertion.stats();
        let input = GqlQueryExecution {
            value,
            rows: prefix.selection,
            evaluator: prefix.evaluator,
        };
        Ok(GraphInsertQueryCollection {
            insertion,
            output: GraphInsertQueryOutput {
                query: self,
                policy: policy.query,
                input,
            },
        })
    }

    /// Definition identity, not an allocation record or durable effect format.
    /// As in native set projection, aliases are output schema metadata.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:graph-insert-query:v1\0".to_vec();
        let insertion = self.insertion.canonical_bytes();
        bytes.extend_from_slice(&(insertion.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&insertion);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            let (tag, index, key) = match *binding {
                GraphInsertBinding::Input(column) => (0, column, None),
                GraphInsertBinding::CreatedVertex(vertex) => (1, vertex, None),
                GraphInsertBinding::VertexProperty { vertex, key } => (2, vertex, Some(key)),
                GraphInsertBinding::CreatedEdge(edge) => (3, edge, None),
                GraphInsertBinding::EdgeProperty { edge, key } => (4, edge, Some(key)),
                GraphInsertBinding::VertexProperties(vertex) => (5, vertex, None),
                GraphInsertBinding::EdgeProperties(edge) => (6, edge, None),
                GraphInsertBinding::VertexLabels(vertex) => (7, vertex, None),
            };
            bytes.push(tag);
            bytes.extend_from_slice(&(index as u64).to_be_bytes());
            if let Some(key) = key {
                bytes.extend_from_slice(&key.0.to_be_bytes());
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
        if self.bindings.iter().any(|binding| {
            matches!(
                binding,
                GraphInsertBinding::VertexProperties(_) | GraphInsertBinding::EdgeProperties(_)
            )
        }) {
            crate::algebra::append_property_catalog_transcript(
                self.property_catalog.as_deref(),
                &mut bytes,
            );
        }
        if self
            .bindings
            .iter()
            .any(|binding| matches!(binding, GraphInsertBinding::VertexLabels(_)))
        {
            crate::algebra::append_label_catalog_transcript(
                self.property_catalog.as_deref(),
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

#[cfg(test)]
mod tests;
