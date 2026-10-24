//! Native `MERGE (n ...) [ON MATCH SET ...] [ON CREATE SET ...] [SET ...]
//! RETURN ...`. MERGE chooses exactly one vertex, so its result head has one row.
//! The host reads the chosen vertex's fields after every clause is staged and
//! hands them back here; nothing is re-matched. A WITH continues those owned
//! values through the native relational pipeline under the same write budget.

mod batch;

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
        match self {
            Self::TooManyBindings { limit, observed } => {
                write!(f, "RETURN has {observed} bindings, limit {limit}")
            }
            Self::Projection(error) => error.fmt(f),
            Self::Aggregate(error) => error.fmt(f),
            Self::PropertyMapCatalog => f.write_str(
                "two properties share one name, so a returned property map is ambiguous",
            ),
        }
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
    /// Row stages over the WITH head, not another graph selection.
    tail: Option<crate::PreparedGraphSet>,
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
            tail: None,
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

    /// Rank the public output, leaving an earlier WITH's order/page alone.
    pub fn with_order_by(mut self, order: &[GraphValueOrder]) -> Result<Self, GraphOrderError> {
        if let Some(tail) = self.tail.take() {
            self.tail = Some(tail.with_order_by(order)?);
            return Ok(self);
        }
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
        if let Some(tail) = self.tail.take() {
            self.tail = Some(tail.with_page(offset, count));
        } else {
            self.offset = offset;
            self.count = count;
        }
        self
    }

    /// Continue the WITH head over its own rows. Only the tail is public output.
    pub(crate) fn with_tail(mut self, tail: crate::PreparedGraphSet) -> Self {
        self.columns = tail.columns().to_vec();
        self.types = tail.column_types().to_vec();
        self.tail = Some(tail);
        self
    }

    /// Project the one input row the host read for `bindings()` after every
    /// clause was staged. `selection` and `evaluator` are the upsert's own
    /// cumulative counters, so the result counters cover the whole statement.
    /// Complete property-map inputs must contain bounded, canonical maps of
    /// scalar fields, and label inputs bounded lists of text, even when
    /// grouping or LIMIT would discard their values. A WITH head remains
    /// private input: only the final tail's rows spend the result-row grant.
    pub fn project_governed<E, C>(
        &self,
        row: Vec<GraphValue>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_final(
            row,
            selection,
            evaluator,
            policy,
            &mut |_, _| None,
            checkpoint,
        )
    }

    /// Evaluate computed identities through the host's final staged graph.
    /// The reader owns authorization and point witnesses; its failures remain
    /// inside the same rollback boundary and cumulative statement allowance.
    pub fn project_governed_reading<E, C>(
        &self,
        row: Vec<GraphValue>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut elements: impl FnMut(
            fgdb_delta_types::ElementId,
            PropertyKeyId,
        ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_final(
            row,
            selection,
            evaluator,
            policy,
            &mut |element, key| Some(elements(element, key)),
            checkpoint,
        )
    }

    fn project_final<E, C>(
        &self,
        row: Vec<GraphValue>,
        selection: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        elements: &mut crate::set_ops::ElementRead<'_, E>,
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
                    // Any stored value, a List or Map included (fgdb-k3nub).
                    if !value.is_stored_value()
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
            returning = crate::set_ops::finish_owned_aggregate_reading(
                returning,
                policy,
                grouping,
                elements,
                &mut checkpoint,
            )?;
        }
        let head = if self.tail.is_some() {
            crate::set_ops::owned_head_policy(policy)
        } else {
            policy
        };
        let returning = crate::set_ops::finish_owned_projection_reading(
            returning,
            head,
            &self.projection,
            self.quantifier,
            &self.order,
            (self.offset, self.count),
            elements,
            &mut checkpoint,
        )?;
        match &self.tail {
            None => Ok(returning),
            Some(tail) => crate::set_ops::finish_owned_tail_reading(
                returning, tail, policy, elements, checkpoint,
            ),
        }
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
mod with_tests {
    use super::*;
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphVertexUpsertQueryText};
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CanonicalScalar, VId};

    const P: PropertyKeyId = PropertyKeyId(1);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn prepare(text: &str) -> PreparedGraphVertexUpsertQuery {
        assert!(PreparedGraphVertexUpsertQueryText::has_return_clause(text).unwrap());
        PreparedGraphVertexUpsertQueryText::prepare(text, RelationId(1), symbols)
            .expect(text)
            .bind_parameters(&GqlParameters::new())
            .expect(text)
    }

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn policy(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(100, rows, 100_000, 100_000)
    }

    fn finish(
        query: &PreparedGraphVertexUpsertQuery,
        row: Vec<GraphValue>,
        rows: u64,
    ) -> Vec<Vec<GraphValue>> {
        query
            .project_governed::<(), ()>(
                row,
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(rows),
                || Ok(()),
            )
            .unwrap()
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    fn aliases_and_star_carry_final_vertex_properties_without_a_graph_source() {
        let query = prepare(
            "MERGE (n {p:1}) ON CREATE SET n.p = 2 ON MATCH SET n.p = 3 \
             SET n.p = n.p + 10 WITH n AS item WHERE item.p > 0 \
             RETURN item AS vertex, item.p AS p",
        );
        assert_eq!(query.columns(), &["vertex", "p"]);
        assert_eq!(
            query.bindings(),
            &[
                GraphVertexReturnBinding::Vertex,
                GraphVertexReturnBinding::Property(P)
            ]
        );
        let vertex = GraphValue::Vertex(VId((1_u128 << 100) + 1));
        // The host supplies final state, not the MERGE map's original p=1.
        assert_eq!(
            finish(&query, vec![vertex.clone(), int(12)], 1),
            vec![vec![vertex.clone(), int(12)]]
        );
        let star = prepare("MERGE (n {p:1}) WITH * RETURN n.p AS p");
        assert_eq!(finish(&star, vec![vertex, int(12)], 1), vec![vec![int(12)]]);
    }

    #[test]
    fn expansion_distinct_and_aggregate_keep_one_final_row_allowance() {
        let query = prepare(
            "MERGE (n {p:1}) WITH n.p AS p UNWIND [p,p+1,p] AS x \
             WITH DISTINCT x RETURN sum(x) AS total",
        );
        assert_eq!(finish(&query, vec![int(3)], 1), vec![vec![int(7)]]);
        let grouped = prepare(
            "MERGE (n {p:1}) WITH collect(n.p) AS ps UNWIND ps AS p \
             RETURN p + 1 AS next",
        );
        assert_eq!(finish(&grouped, vec![int(3)], 1), vec![vec![int(4)]]);
        let empty = prepare("MERGE (n {p:1}) WITH n.p AS p WHERE p < 0 RETURN count(*) AS total");
        assert_eq!(finish(&empty, vec![int(3)], 1), vec![vec![int(0)]]);
    }

    #[test]
    fn only_final_output_spends_rows_and_limit_zero_cannot_hide_expression_failure() {
        let filtered = prepare("MERGE (n {p:1}) WITH n AS item WHERE item.p < 0 RETURN item");
        assert!(finish(&filtered, vec![GraphValue::Vertex(VId(1)), int(3)], 0).is_empty());
        let limited = prepare("MERGE (n {p:1}) WITH n.p AS p RETURN p LIMIT 0");
        assert!(finish(&limited, vec![int(3)], 0).is_empty());
        let failed = prepare(
            "MERGE (n {p:1}) WITH n.p AS p UNWIND [1,0] AS divisor \
             RETURN p / divisor AS value LIMIT 0",
        );
        assert!(matches!(
            failed.project_governed::<(), ()>(
                vec![int(3)],
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(0),
                || Ok(()),
            ),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
    }

    #[test]
    fn final_builder_windows_use_tail_columns_without_paging_away_the_head() {
        let query = prepare(
            "MERGE (n {p:1}) WITH n.p AS p LIMIT 1 UNWIND [p,p+1,p+2] AS x \
             RETURN 100-x AS inverse, x AS value",
        );
        let selected = query
            .clone()
            .with_order_by(&[GraphValueOrder {
                column: 1,
                descending: true,
                nulls_first: false,
            }])
            .unwrap()
            .with_page(1, Some(1));
        assert_eq!(
            finish(&selected, vec![int(3)], 1),
            vec![vec![int(96), int(4)]]
        );
        assert_ne!(selected.canonical_bytes(), query.canonical_bytes());
        assert!(matches!(
            query.with_order_by(&[GraphValueOrder {
                column: 2,
                descending: false,
                nulls_first: false,
            }]),
            Err(GraphOrderError::UnknownColumn { column: 2 })
        ));
    }

    #[test]
    fn rebind_parameters_without_catalog_and_keep_tail_semantics_in_identity() {
        let calls = std::cell::Cell::new(0);
        let template = PreparedGraphVertexUpsertQueryText::prepare(
            "MERGE (n {p:$key}) WITH n.p AS p WHERE p > $minimum RETURN p+$extra AS value",
            RelationId(1),
            |kind: crate::GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap();
        let prepared_calls = calls.get();
        let args = |minimum| {
            GqlParameters::new()
                .with_int64("key", 1)
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap()
                .with_int64("extra", 10)
                .unwrap()
        };
        let first = template.bind_parameters(&args(0)).unwrap();
        let second = template.bind_parameters(&args(5)).unwrap();
        assert_eq!(calls.get(), prepared_calls);
        assert_eq!(finish(&first, vec![int(3)], 1), vec![vec![int(13)]]);
        assert!(finish(&second, vec![int(3)], 0).is_empty());
        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());

        let plain = prepare("MERGE (n {p:1}) RETURN n.p AS p");
        let piped = prepare("MERGE (n {p:1}) WITH n.p AS p RETURN p");
        let mut expected = plain.canonical_bytes();
        let tail = piped.tail.as_ref().unwrap().canonical_bytes();
        expected.extend_from_slice(b"fgdb:write-tail:v1\0");
        expected.extend_from_slice(&(tail.len() as u64).to_be_bytes());
        expected.extend_from_slice(&tail);
        assert!(plain.tail.is_none());
        assert_eq!(piped.canonical_bytes(), expected);
    }

    #[test]
    fn malformed_tails_and_dropped_bindings_refuse_before_catalog_access() {
        for tail in [
            "WITH n AS item RETURN n",
            "WITH n AS item RETURN missing",
            "WITH n AS item SET item.p = 3 RETURN item",
            "WITH n AS item RETURN item; CREATE (m)",
            "WITH n AS item WHERE RETURN item",
        ] {
            let text = format!("MERGE (n {{p:1}}) {tail}");
            let calls = std::cell::Cell::new(0);
            assert!(
                PreparedGraphVertexUpsertQueryText::prepare(
                    &text,
                    RelationId(1),
                    |kind: crate::GraphSymbolKind, name: &str| {
                        calls.set(calls.get() + 1);
                        symbols(kind, name)
                    }
                )
                .is_err(),
                "{text}"
            );
            assert_eq!(calls.get(), 0, "{text}");
        }
    }

    #[test]
    fn exact_work_budget_and_every_cancellation_checkpoint_cover_the_native_tail() {
        let query = prepare(
            "MERGE (n {p:1}) WITH n.p AS p UNWIND [p,p+1,p+2] AS x \
             RETURN sum(x) AS total",
        );
        let prefix = GlaExecutionStats {
            work_units: 17,
            ..GlaExecutionStats::default()
        };
        let mut checkpoints = 0;
        let result = query
            .project_governed::<(), ()>(
                vec![int(3)],
                GqlExecutionStats::default(),
                prefix,
                policy(1),
                || {
                    checkpoints += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(result.value[0].values(), &[int(12)]);
        assert!(result.evaluator.work_units > prefix.work_units);
        assert!(checkpoints > 1);
        for (limit, success) in [
            (result.evaluator.work_units, true),
            (result.evaluator.work_units - 1, false),
        ] {
            let checked = query.project_governed::<(), ()>(
                vec![int(3)],
                GqlExecutionStats::default(),
                prefix,
                GqlQueryPolicy::new(100, 1, limit, 100_000),
                || Ok(()),
            );
            if success {
                assert!(checked.is_ok());
            } else {
                assert!(matches!(checked, Err(GqlQueryError::Evaluator(_))));
            }
        }
        for stop in 0..checkpoints {
            let mut visited = 0;
            assert!(matches!(
                query.project_governed::<(), ()>(
                    vec![int(3)],
                    GqlExecutionStats::default(),
                    prefix,
                    policy(1),
                    || {
                        if visited == stop {
                            return Err(());
                        }
                        visited += 1;
                        Ok(())
                    },
                ),
                Err(GqlQueryError::Interrupted(()))
            ));
        }
    }
}
