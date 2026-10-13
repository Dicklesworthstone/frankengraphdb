//! Native `MERGE (n ...) [ON MATCH SET ...] [ON CREATE SET ...] [SET ...]
//! RETURN ...`. MERGE chooses exactly one vertex, so RETURN projects one row.
//! The host reads the chosen vertex's fields after every clause is staged and
//! hands them back here; nothing is re-matched.

use super::*;
use crate::algebra::{GraphOrderError, GraphValue, GraphValueOrder, GraphValueRow};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetColumnType, GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier,
};

/// One private column of the single MERGE RETURN input row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphVertexReturnBinding {
    /// The chosen vertex's identity.
    Vertex,
    /// A field of the chosen vertex after every clause; missing reads NULL.
    Property(PropertyKeyId),
    /// Every visible property of the chosen vertex after every staged clause.
    /// The host enumerates its actual fields and uses `property_catalog()` to
    /// name them; an absent or unknown catalog entry is never an omitted field.
    Properties,
    /// The chosen vertex's visible labels after every staged clause (MERGE
    /// pattern, ON CREATE / ON MATCH / SET label edits), named through
    /// `property_catalog()`'s label map in identity order (fgdb-iufb7). An
    /// unnamed label refuses; it is never omitted.
    Labels,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphVertexUpsertQueryBuildError {
    TooManyBindings {
        limit: usize,
        observed: usize,
    },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
    /// Distinct property IDs cannot share one output name.
    PropertyMapCatalog,
}
impl core::fmt::Display for GraphVertexUpsertQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "MERGE RETURN definition: {self:?}")
    }
}
impl core::error::Error for GraphVertexUpsertQueryBuildError {}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphVertexUpsertQuery {
    upsert: PreparedGraphVertexUpsert,
    bindings: Vec<GraphVertexReturnBinding>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
    property_catalog: Option<std::sync::Arc<crate::ReverseSymbolCatalog>>,
}
impl core::fmt::Debug for PreparedGraphVertexUpsertQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphVertexUpsertQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphVertexUpsertQuery {
    pub fn prepare(
        upsert: PreparedGraphVertexUpsert,
        bindings: Vec<GraphVertexReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphVertexUpsertQueryBuildError> {
        Self::prepare_with_grouping(upsert, bindings, projection, quantifier, None, None)
    }

    /// Trusted names for complete property-map bindings. The source still
    /// enumerates the chosen vertex's actual, visible properties, including
    /// fields introduced by the selected ON clause and trailing SET.
    pub fn prepare_with_property_catalog(
        upsert: PreparedGraphVertexUpsert,
        bindings: Vec<GraphVertexReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        catalog: crate::ReverseSymbolCatalog,
    ) -> Result<Self, GraphVertexUpsertQueryBuildError> {
        Self::prepare_with_grouping(
            upsert,
            bindings,
            projection,
            quantifier,
            None,
            Some(std::sync::Arc::new(catalog)),
        )
    }

    pub(crate) fn prepare_with_grouping(
        upsert: PreparedGraphVertexUpsert,
        bindings: Vec<GraphVertexReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
        property_catalog: Option<std::sync::Arc<crate::ReverseSymbolCatalog>>,
    ) -> Result<Self, GraphVertexUpsertQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphVertexUpsertQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let input_types: Vec<_> = bindings
            .iter()
            .map(|binding| match binding {
                GraphVertexReturnBinding::Vertex => GraphSetColumnType::Vertex,
                GraphVertexReturnBinding::Property(_) => GraphSetColumnType::Stored,
                GraphVertexReturnBinding::Properties => GraphSetColumnType::Any,
                GraphVertexReturnBinding::Labels => GraphSetColumnType::List,
            })
            .collect();
        let property_catalog = if bindings.contains(&GraphVertexReturnBinding::Properties)
            || bindings.contains(&GraphVertexReturnBinding::Labels)
        {
            if bindings.contains(&GraphVertexReturnBinding::Properties)
                && let Some(catalog) = &property_catalog
            {
                let mut names = std::collections::BTreeSet::new();
                if !catalog.properties.values().all(|name| names.insert(name)) {
                    return Err(GraphVertexUpsertQueryBuildError::PropertyMapCatalog);
                }
            }
            property_catalog
        } else {
            // Unused host metadata does not change existing query identity.
            None
        };
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphVertexUpsertQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or(input_types);
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphVertexUpsertQueryBuildError::Projection)?;
        let columns = checked.columns().map(str::to_owned).collect();
        let types = checked.column_types().to_vec();
        Ok(Self {
            upsert,
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
        })
    }

    #[must_use]
    pub const fn upsert(&self) -> &PreparedGraphVertexUpsert {
        &self.upsert
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphVertexReturnBinding] {
        &self.bindings
    }
    /// Trusted output names for the complete-map source callback. A missing
    /// catalog is valid only when no visible property requires a name.
    #[must_use]
    pub fn property_catalog(&self) -> Option<&crate::ReverseSymbolCatalog> {
        self.property_catalog.as_deref()
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

    /// Project the one input row the host read for `bindings()` after every
    /// clause was staged. `selection` and `evaluator` are the upsert's own
    /// cumulative counters, so the result counters cover the whole statement.
    /// Complete property-map inputs must contain bounded, canonical maps of
    /// scalar fields, and label inputs bounded lists of text, even when
    /// grouping or LIMIT would discard their values.
    pub fn project_governed<E, C>(
        &self,
        row: Vec<GraphValue>,
        selection: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        if self
            .bindings
            .contains(&GraphVertexReturnBinding::Properties)
            || self.bindings.contains(&GraphVertexReturnBinding::Labels)
        {
            let invalid =
                || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
            let mut work = || -> Result<(), GqlQueryError<GraphSetExecutionError<E>, C>> {
                checkpoint().map_err(GqlQueryError::Interrupted)?;
                let observed = u128::from(evaluator.work_units) + 1;
                if observed > u128::from(policy.evaluator.max_work_units) {
                    return Err(GqlQueryError::Evaluator(crate::GlaLimitExceeded {
                        dimension: crate::GlaLimitDimension::WorkUnits,
                        limit: policy.evaluator.max_work_units,
                        observed,
                    }));
                }
                evaluator.work_units = observed as u64;
                Ok(())
            };
            work()?;
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for (binding, value) in self.bindings.iter().zip(&row) {
                if *binding == GraphVertexReturnBinding::Labels {
                    work()?;
                    let GraphValue::List(labels) = value else {
                        return Err(invalid());
                    };
                    if labels.len() >= GraphValue::MAX_LIST_NODES {
                        return Err(invalid());
                    }
                    for label in labels {
                        work()?;
                        if !matches!(
                            label,
                            GraphValue::Scalar(fgdb_types::CanonicalScalar::Text(_))
                        ) {
                            return Err(invalid());
                        }
                    }
                    continue;
                }
                if *binding != GraphVertexReturnBinding::Properties {
                    continue;
                }
                work()?;
                let (keys, values) = value.as_map().ok_or_else(invalid)?;
                if keys.len() != values.len() || keys.len() >= GraphValue::MAX_LIST_NODES {
                    return Err(invalid());
                }
                for (at, (key, value)) in keys.iter().zip(values).enumerate() {
                    work()?;
                    if !matches!(value, GraphValue::Scalar(_))
                        || at != 0 && keys[at - 1].as_bytes() >= key.as_bytes()
                    {
                        return Err(invalid());
                    }
                }
            }
        }
        let mut returning = GqlQueryExecution {
            value: vec![GraphValueRow::from_owned_values(row)],
            rows: selection,
            evaluator,
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate(
                returning,
                policy,
                grouping,
                &mut checkpoint,
            )?;
        }
        crate::set_ops::finish_owned_projection(
            returning,
            policy,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            checkpoint,
        )
    }

    /// Definition identity, not a durable effect format.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fgdb:unique-vertex-upsert-query:v1\0".to_vec();
        let upsert = self.upsert.canonical_bytes();
        bytes.extend_from_slice(&(upsert.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&upsert);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match binding {
                GraphVertexReturnBinding::Vertex => bytes.push(0),
                GraphVertexReturnBinding::Property(key) => {
                    bytes.push(1);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
                GraphVertexReturnBinding::Properties => bytes.push(2),
                GraphVertexReturnBinding::Labels => bytes.push(3),
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
            .contains(&GraphVertexReturnBinding::Properties)
        {
            crate::algebra::append_property_catalog_transcript(
                self.property_catalog.as_deref(),
                &mut bytes,
            );
        }
        if self.bindings.contains(&GraphVertexReturnBinding::Labels) {
            crate::algebra::append_label_catalog_transcript(
                self.property_catalog.as_deref(),
                &mut bytes,
            );
        }
        bytes
    }
}
