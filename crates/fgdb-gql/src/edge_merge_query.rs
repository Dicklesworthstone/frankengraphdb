//! Native RETURN after a relationship MERGE (fgdb-at06q).
//!
//! Every selected MATCH row continues once per relationship its endpoint pair
//! resolved to: each matching edge, or the one created edge. The write owner
//! builds those rows after staging and supplies their final fields; no second
//! MATCH runs. Aggregation, projection, DISTINCT, ORDER BY and pagination use
//! the shared write-RETURN kernels. A WITH continues those owned rows through
//! the native relational pipeline, under the same statement budget.

use crate::algebra::{GraphOrderError, GraphValue, GraphValueOrder, GraphValueRow};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphSetColumnType, GraphSetExecutionError, GraphSetProjection, GraphSetQuantifier,
    PreparedGraphEdgeMerge,
};
use fgdb_delta_types::PropertyKeyId;

/// One input column of a relationship MERGE RETURN row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeBinding {
    /// A MERGE selection column. MERGE changes no selected element, so its
    /// frozen value is also its final one.
    Input(usize),
    /// The relationship this row resolved to.
    Edge,
    /// A property of that relationship after the statement. A missing field
    /// reads NULL. The host applies property authority first.
    Property(PropertyKeyId),
    /// `startNode(r)` (`start`) or `endNode(r)`: the relationship's own source
    /// or destination, which an undirected match may hold either way round.
    Endpoint { start: bool },
    /// A property of that endpoint vertex. A missing field reads NULL.
    EndpointProperty { start: bool, key: PropertyKeyId },
    /// A property, after the statement, of the vertex or relationship in a
    /// selection column: a carried element of a chain's row (fgdb-cpny9). A
    /// missing field or a NULL column reads NULL.
    InputProperty { column: usize, key: PropertyKeyId },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeMergeQueryBuildError {
    TooManyBindings {
        limit: usize,
        observed: usize,
    },
    /// An Input binding names no selection column.
    Binding {
        binding: usize,
    },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
}
impl core::fmt::Display for GraphEdgeMergeQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyBindings { limit, observed } => {
                write!(f, "RETURN has {observed} bindings, limit {limit}")
            }
            Self::Binding { binding } => {
                write!(f, "RETURN binding {binding} names no input column")
            }
            Self::Projection(error) => error.fmt(f),
            Self::Aggregate(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GraphEdgeMergeQueryBuildError {}

/// A relationship MERGE RETURN refusal: the MERGE itself, or its RETURN.
#[derive(Debug)]
pub enum GraphEdgeMergeQueryError<E, A> {
    Merge(crate::GraphEdgeMergeError<E, A>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeMergeQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Merge(error) => error.fmt(f),
            Self::Returning(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeMergeQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Merge(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeMergeQuery {
    merge: PreparedGraphEdgeMerge,
    bindings: Vec<GraphEdgeMergeBinding>,
    input_types: Vec<GraphSetColumnType>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
    /// A WITH head supplies private rows, not a second graph selection.
    tail: Option<crate::PreparedGraphSet>,
}
impl core::fmt::Debug for PreparedGraphEdgeMergeQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeMergeQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphEdgeMergeQuery {
    pub fn prepare(
        merge: PreparedGraphEdgeMerge,
        bindings: Vec<GraphEdgeMergeBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphEdgeMergeQueryBuildError> {
        Self::prepare_with_grouping(merge, bindings, projection, quantifier, None)
    }

    pub(crate) fn prepare_with_grouping(
        merge: PreparedGraphEdgeMerge,
        bindings: Vec<GraphEdgeMergeBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphEdgeMergeQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphEdgeMergeQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let selection = merge.input_column_types();
        let mut input_types = Vec::with_capacity(bindings.len());
        for (at, binding) in bindings.iter().enumerate() {
            input_types.push(match *binding {
                GraphEdgeMergeBinding::Input(column) => selection
                    .get(column)
                    .copied()
                    .ok_or(GraphEdgeMergeQueryBuildError::Binding { binding: at })?,
                GraphEdgeMergeBinding::Edge => GraphSetColumnType::Edge,
                GraphEdgeMergeBinding::Endpoint { .. } => GraphSetColumnType::Vertex,
                GraphEdgeMergeBinding::Property(_)
                | GraphEdgeMergeBinding::EndpointProperty { .. } => GraphSetColumnType::Stored,
                GraphEdgeMergeBinding::InputProperty { column, .. } => {
                    match selection.get(column) {
                        Some(GraphSetColumnType::Vertex | GraphSetColumnType::Edge) => {
                            GraphSetColumnType::Stored
                        }
                        _ => return Err(GraphEdgeMergeQueryBuildError::Binding { binding: at }),
                    }
                }
            });
        }
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphEdgeMergeQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or_else(|| input_types.clone());
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphEdgeMergeQueryBuildError::Projection)?;
        Ok(Self {
            merge,
            bindings,
            input_types,
            columns: checked.columns().map(str::to_owned).collect(),
            types: checked.column_types().to_vec(),
            projection,
            grouping,
            quantifier,
            order: Vec::new(),
            offset: 0,
            count: None,
            tail: None,
        })
    }

    #[must_use]
    pub const fn merge(&self) -> &PreparedGraphEdgeMerge {
        &self.merge
    }

    /// The allowance for the MERGE step itself. Only a plain RETURN (no
    /// grouping, WITH tail, DISTINCT or page) emits at least one row per
    /// selected row, so only then does a selection beyond the row allowance
    /// refuse before any identity is allocated. Otherwise the selected rows
    /// are private input to the returning pipeline, which alone charges
    /// result rows; work, scratch and source records stay cumulative.
    #[must_use]
    pub fn merge_policy(&self, policy: crate::GraphEdgeMergePolicy) -> crate::GraphEdgeMergePolicy {
        if self.grouping.is_none()
            && self.tail.is_none()
            && matches!(self.quantifier, GraphSetQuantifier::All)
            && self.offset == 0
            && self.count.is_none()
        {
            return policy;
        }
        crate::GraphEdgeMergePolicy {
            query: crate::set_ops::owned_head_policy(policy.query),
            ..policy
        }
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphEdgeMergeBinding] {
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

    /// Finalize the statement's final-state rows, one per selected row and
    /// relationship it resolved to, under its cumulative counters. Every cell
    /// is checked against its binding before any projection runs. The host
    /// must retain its rollback guard until this succeeds. LIMIT 0 discards
    /// output, never the MERGE or its field reads.
    pub fn finish_governed<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.finish_final(
            rows,
            selection,
            evaluator,
            policy,
            &mut |_, _| None,
            checkpoint,
        )
    }

    /// Read computed identities only after the complete MERGE bag is staged.
    /// The host provides authority-checked point reads and keeps its rollback
    /// guard until every projection, grouping and WITH-tail expression succeeds.
    pub fn finish_governed_reading<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut elements: impl crate::GraphElementReader<E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.finish_final(
            rows,
            selection,
            evaluator,
            policy,
            &mut crate::set_ops::projection::HostElements(&mut elements),
            checkpoint,
        )
    }

    fn finish_final<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        mut evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        elements: &mut crate::set_ops::ElementRead<'_, E>,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
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
        let invalid = || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
        for row in &rows {
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for ((binding, kind), value) in self.bindings.iter().zip(&self.input_types).zip(row) {
                work()?;
                // A field cell is a stored value: a scalar or a stored list.
                let admitted = match binding {
                    GraphEdgeMergeBinding::Input(_)
                    | GraphEdgeMergeBinding::Property(_)
                    | GraphEdgeMergeBinding::EndpointProperty { .. }
                    | GraphEdgeMergeBinding::InputProperty { .. } => kind.accepts(value),
                    GraphEdgeMergeBinding::Edge => matches!(value, GraphValue::Edge(_)),
                    GraphEdgeMergeBinding::Endpoint { .. } => {
                        matches!(value, GraphValue::Vertex(_))
                    }
                };
                if !admitted {
                    return Err(invalid());
                }
            }
        }
        drop(work);
        let mut returning = GqlQueryExecution {
            value: rows
                .into_iter()
                .map(GraphValueRow::from_owned_values)
                .collect(),
            rows: selection,
            evaluator,
        };
        // A WITH head's rows, grouped or not, are private input to its tail;
        // only the tail's rows count against the statement's row allowance.
        let head = if self.tail.is_some() {
            crate::set_ops::owned_head_policy(policy)
        } else {
            policy
        };
        if let Some(grouping) = &self.grouping {
            returning = crate::set_ops::finish_owned_aggregate_reading(
                returning,
                head,
                grouping,
                elements,
                &mut checkpoint,
            )?;
        }
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
        let mut bytes = b"fgdb:edge-merge-query:v1\0".to_vec();
        let merge = self.merge.canonical_bytes();
        bytes.extend_from_slice(&(merge.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&merge);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match *binding {
                GraphEdgeMergeBinding::Input(column) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&(column as u64).to_be_bytes());
                }
                GraphEdgeMergeBinding::Edge => bytes.push(1),
                GraphEdgeMergeBinding::Property(key) => {
                    bytes.push(2);
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
                GraphEdgeMergeBinding::Endpoint { start } => {
                    bytes.push(3);
                    bytes.push(u8::from(start));
                }
                GraphEdgeMergeBinding::EndpointProperty { start, key } => {
                    bytes.push(4);
                    bytes.push(u8::from(start));
                    bytes.extend_from_slice(&key.0.to_be_bytes());
                }
                GraphEdgeMergeBinding::InputProperty { column, key } => {
                    bytes.push(5);
                    bytes.extend_from_slice(&(column as u64).to_be_bytes());
                    bytes.extend_from_slice(&key.0.to_be_bytes());
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
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeMergeQueryText};
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CanonicalScalar, EId, VId};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn prepare(text: &str) -> PreparedGraphEdgeMergeQuery {
        PreparedGraphEdgeMergeQueryText::prepare(text, R, symbols)
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

    fn values(result: GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
        result
            .value
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    fn finish(
        query: &PreparedGraphEdgeMergeQuery,
        rows: Vec<Vec<GraphValue>>,
        limit: u64,
    ) -> GqlQueryExecution<GraphValueRow> {
        query
            .finish_governed::<(), ()>(
                rows,
                GqlExecutionStats::default(),
                GlaExecutionStats::default(),
                policy(limit),
                || Ok(()),
            )
            .unwrap()
    }

    #[test]
    fn with_dispatch_keeps_unrelated_write_owners_and_quoted_words() {
        for text in [
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r RETURN r",
            "MATCH (a),(b) MERGE (a)<-[r:R]-(b) WITH r AS edge RETURN edge",
            "MATCH (a),(b) MERGE (a)-[:R]-(b) WITH a RETURN a;",
            // A WITH row may head the MERGE (fgdb-cpny9).
            "MATCH (a),(b) WITH a,b MERGE (a)-[:R]->(b) RETURN a",
        ] {
            assert!(PreparedGraphEdgeMergeQueryText::has_return_clause(text).unwrap());
            prepare(text);
        }
        for text in [
            "MATCH (a),(b) MERGE (a)-[:R {p: 'WITH'}]->(b)",
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) ON CREATE SET r.p=1 WITH r RETURN r",
            "MATCH (a),(b) MERGE (a)-[:R]->(b) MERGE (b)-[:R]->(a) RETURN a",
            "MERGE (n) WITH n RETURN n",
        ] {
            assert!(
                !PreparedGraphEdgeMergeQueryText::has_return_clause(text).unwrap(),
                "{text}"
            );
        }
    }

    #[test]
    fn duplicate_occurrences_reach_aggregate_without_spending_final_row_quota() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             WITH r AS edge RETURN count(edge) AS occurrences",
        );
        let edge = GraphValue::Edge(EId((1_u128 << 100) + 1));
        let rows = vec![vec![edge.clone()], vec![edge.clone()], vec![edge]];
        assert_eq!(query.columns(), &["occurrences"]);
        assert_eq!(query.column_types(), &[GraphSetColumnType::Scalar]);
        assert_eq!(values(finish(&query, rows, 1)), vec![vec![int(3)]]);

        let distinct = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             WITH DISTINCT r AS edge RETURN count(edge) AS occurrences",
        );
        let edge = GraphValue::Edge(EId(1));
        assert_eq!(
            values(finish(&distinct, vec![vec![edge.clone()], vec![edge]], 1)),
            vec![vec![int(1)]]
        );
    }

    #[test]
    fn aliased_merged_edges_and_actual_endpoints_hoist_owned_property_bindings() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]-(b) \
             WITH r AS edge, startNode(r) AS source, a AS matched \
             WHERE edge.p > 0 RETURN edge.p AS ep, source.p AS sp, matched.p AS mp",
        );
        assert_eq!(query.columns(), &["ep", "sp", "mp"]);
        assert_eq!(
            query.bindings(),
            &[
                GraphEdgeMergeBinding::Edge,
                GraphEdgeMergeBinding::Endpoint { start: true },
                GraphEdgeMergeBinding::Input(0),
                GraphEdgeMergeBinding::Property(P),
                GraphEdgeMergeBinding::EndpointProperty {
                    start: true,
                    key: P,
                },
                GraphEdgeMergeBinding::Input(2),
            ]
        );
        // The undirected relationship's actual source differs from MATCH a.
        let rows = vec![vec![
            GraphValue::Edge(EId((1_u128 << 100) + 1)),
            GraphValue::Vertex(VId(2)),
            GraphValue::Vertex(VId(1)),
            int(7),
            int(20),
            int(10),
        ]];
        assert_eq!(
            values(finish(&query, rows, 1)),
            vec![vec![int(7), int(20), int(10)]]
        );
    }

    #[test]
    fn filtering_unwind_grouping_and_rebinding_use_one_native_tail() {
        let text = "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
                    WITH r.p AS p WHERE p > $minimum \
                    UNWIND [p, p + 1] AS x WITH sum(x) AS total RETURN total";
        let calls = std::cell::Cell::new(0);
        let template = PreparedGraphEdgeMergeQueryText::prepare(
            text,
            R,
            |kind: crate::GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap();
        let prepared_calls = calls.get();
        let first = template
            .bind_parameters(&GqlParameters::new().with_int64("minimum", 1).unwrap())
            .unwrap();
        let second = template
            .bind_parameters(&GqlParameters::new().with_int64("minimum", 2).unwrap())
            .unwrap();
        assert_eq!(calls.get(), prepared_calls);
        assert_ne!(first.canonical_bytes(), second.canonical_bytes());
        let rows = vec![vec![int(1)], vec![int(2)], vec![int(2)]];
        assert_eq!(values(finish(&first, rows.clone(), 1)), vec![vec![int(10)]]);
        // No row passes the filter: the global SUM's one row is NULL, the
        // engine's SUM over no input (empty_input_releases_one_zero_count_
        // and_null_sum_row), not 0.
        assert_eq!(
            values(finish(&second, rows, 1)),
            vec![vec![GraphValue::Scalar(CanonicalScalar::Null)]]
        );
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    }

    #[test]
    fn with_and_return_windows_are_distinct_and_only_final_rows_are_charged() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             WITH r.p AS p ORDER BY p DESC LIMIT 2 \
             RETURN p AS selected ORDER BY selected LIMIT 1",
        );
        let rows = vec![vec![int(1)], vec![int(2)], vec![int(3)]];
        assert_eq!(values(finish(&query, rows, 1)), vec![vec![int(2)]]);
        let empty = prepare("MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r RETURN r LIMIT 0");
        assert!(
            finish(&empty, vec![vec![GraphValue::Edge(EId(1))]], 0)
                .value
                .is_empty()
        );
    }

    #[test]
    fn public_windows_address_tail_columns_and_preserve_the_with_window() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             WITH r.p AS p ORDER BY p DESC LIMIT 2 \
             RETURN 100-p AS inverse, p AS value",
        );
        let selected = query
            .clone()
            .with_order_by(&[GraphValueOrder {
                column: 1,
                descending: false,
                nulls_first: false,
            }])
            .unwrap()
            .with_page(1, Some(1));
        // Head: p=3,2. Tail sorted by its second column: p=2,3.
        // Skip one tail row, not one source row or the entire WITH input.
        assert_eq!(
            values(finish(
                &selected,
                vec![vec![int(1)], vec![int(2)], vec![int(3)]],
                1,
            )),
            vec![vec![int(97), int(3)]]
        );
        assert_eq!(selected.order, query.order);
        assert_eq!((selected.offset, selected.count), (0, Some(2)));
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
    fn public_paging_follows_expansion_and_cannot_hide_a_tail_failure() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r.p AS p \
             UNWIND [p,p+1,p+2] AS x RETURN x AS value",
        )
        .with_page(1, Some(1));
        assert_eq!(
            values(finish(&query, vec![vec![int(3)]], 1)),
            vec![vec![int(4)]]
        );
        let failed = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r.p AS p \
             UNWIND [1,0] AS divisor RETURN p / divisor AS value",
        )
        .with_page(0, Some(0));
        assert!(matches!(
            failed.finish_governed::<(), ()>(
                vec![vec![int(3)]],
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
    fn native_tail_identity_retains_semantics_and_leaves_plain_return_unchanged() {
        let plain = prepare("MATCH (a),(b) MERGE (a)-[r:R]->(b) RETURN r AS edge");
        let piped = prepare("MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r AS edge RETURN edge");
        let sorted = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r AS edge RETURN edge ORDER BY edge DESC",
        );
        assert!(plain.tail.is_none());
        assert!(piped.tail.is_some());
        assert_ne!(plain.canonical_bytes(), piped.canonical_bytes());
        assert_ne!(piped.canonical_bytes(), sorted.canonical_bytes());
        let tail = piped.tail.as_ref().unwrap().canonical_bytes();
        let mut expected = plain.canonical_bytes();
        expected.extend_from_slice(b"fgdb:write-tail:v1\0");
        expected.extend_from_slice(&(tail.len() as u64).to_be_bytes());
        expected.extend_from_slice(&tail);
        assert_eq!(piped.canonical_bytes(), expected);
    }

    #[test]
    fn malformed_tail_and_out_of_scope_names_refuse_before_catalog() {
        for tail in [
            "WITH r AS edge RETURN missing",
            "WITH r AS edge RETURN r",
            "WITH r AS edge RETURN edge; CREATE (n)",
            "WITH r AS edge WHERE RETURN edge",
            "WITH r AS edge SET edge.p = 1 RETURN edge",
        ] {
            let text = format!("MATCH (a),(b) MERGE (a)-[r:R]->(b) {tail}");
            let calls = std::cell::Cell::new(0);
            assert!(
                PreparedGraphEdgeMergeQueryText::prepare(
                    &text,
                    R,
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
    fn late_expressions_and_bad_cells_are_not_hidden_by_limit_zero() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             WITH r.p AS p RETURN 10 / (p - 2) AS x LIMIT 0",
        );
        assert!(
            query
                .finish_governed::<(), ()>(
                    vec![vec![int(1)], vec![int(2)]],
                    GqlExecutionStats::default(),
                    GlaExecutionStats::default(),
                    policy(0),
                    || Ok(()),
                )
                .is_err()
        );
        assert!(
            query
                .finish_governed::<(), ()>(
                    vec![vec![GraphValue::Vertex(VId(1))]],
                    GqlExecutionStats::default(),
                    GlaExecutionStats::default(),
                    policy(0),
                    || Ok(()),
                )
                .is_err()
        );
    }

    #[test]
    fn cancellation_and_work_limits_cover_the_whole_owned_pipeline() {
        let query = prepare(
            "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
             WITH r.p AS p RETURN sum(p + 1) AS total",
        );
        let rows = vec![vec![int(1)], vec![int(2)], vec![int(3)]];
        let prefix = GlaExecutionStats {
            work_units: 17,
            ..GlaExecutionStats::default()
        };
        let mut checkpoints = 0;
        let result = query
            .finish_governed::<(), ()>(
                rows.clone(),
                GqlExecutionStats::default(),
                prefix,
                policy(1),
                || {
                    checkpoints += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(result.value[0].values(), &[int(9)]);
        assert!(result.evaluator.work_units > 17);
        assert!(checkpoints > 1);
        let exact = GqlQueryPolicy::new(100, 1, result.evaluator.work_units, 100_000);
        assert!(
            query
                .finish_governed::<(), ()>(
                    rows.clone(),
                    GqlExecutionStats::default(),
                    prefix,
                    exact,
                    || Ok(()),
                )
                .is_ok()
        );
        let low = GqlQueryPolicy::new(100, 1, result.evaluator.work_units - 1, 100_000);
        assert!(matches!(
            query.finish_governed::<(), ()>(
                rows.clone(),
                GqlExecutionStats::default(),
                prefix,
                low,
                || Ok(()),
            ),
            Err(GqlQueryError::Evaluator(_))
        ));
        for stop in [0, checkpoints / 2, checkpoints - 1] {
            let mut visited = 0;
            assert!(matches!(
                query.finish_governed::<(), ()>(
                    rows.clone(),
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

    #[test]
    fn only_a_plain_return_charges_the_merge_selection_as_result_rows() {
        // A plain RETURN emits at least one row per selected row, so a large
        // selection refuses before allocation. Grouping, a WITH tail,
        // DISTINCT and a page can emit fewer rows than were selected.
        let plain = "MATCH (a),(b) MERGE (a)-[r:R]->(b) RETURN r AS edge";
        let allowance = crate::GraphEdgeMergePolicy::new(policy(1));
        let merge = |query: &PreparedGraphEdgeMergeQuery| query.merge_policy(allowance);
        assert_eq!(merge(&prepare(plain)), allowance);
        for query in [
            prepare("MATCH (a),(b) MERGE (a)-[r:R]->(b) RETURN count(*) AS n"),
            prepare("MATCH (a),(b) MERGE (a)-[r:R]->(b) WITH r.p AS p RETURN p"),
            prepare("MATCH (a),(b) MERGE (a)-[r:R]->(b) RETURN DISTINCT r.p AS p"),
            prepare(plain).with_page(0, Some(1)),
        ] {
            let lifted = merge(&query);
            assert_ne!(lifted.query.rows.max_result_rows(), Some(1), "{query:?}");
            assert_eq!(
                lifted.query.rows.max_snapshot_records(),
                allowance.query.rows.max_snapshot_records()
            );
            assert_eq!(lifted.query.evaluator, allowance.query.evaluator);
            assert_eq!(lifted.max_created_edges, allowance.max_created_edges);
        }
    }
}
