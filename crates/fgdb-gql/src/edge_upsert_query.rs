//! Native RETURN over the relationships selected by MERGE.
//!
//! The write owner supplies final fields of the chosen edges, never a second
//! MATCH. NoInput supplies no row, so even a constant projection stays empty.
//! Aggregation, projection, DISTINCT and pagination use the shared kernels.
//! WITH continues the final-state head through native row stages. It never
//! reselects the relationship or refreshes the statement's execution budget.

use crate::algebra::{GraphOrderError, GraphValue, GraphValueOrder, GraphValueRow};
use crate::row_projection::{RowProjectionBuildError, RowProjectionSpec};
use crate::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphEdgeUpsertError, GraphSetColumnType, GraphSetExecutionError, GraphSetProjection,
    GraphSetQuantifier, PreparedGraphEdgeUpsert,
};
use fgdb_delta_types::PropertyKeyId;

/// Cumulative usage of an occurrence-preserving relationship upsert.
///
/// MATCH is executed once. Each selected occurrence resolves against the
/// preceding occurrence's staged graph and runs its own ON/SET clauses.
/// A matched relationship can therefore contribute more than once. These
/// counters describe execution, not distinct identities or a durable commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GraphEdgeUpsertBatchStats {
    pub match_selection: GqlExecutionStats,
    /// Every relationship record examined by the native existence probes.
    pub overlay_edges: u64,
    pub created_edges: u64,
    pub matched_occurrences: u64,
    /// All selected ON and trailing SET proposals, including overwrites.
    pub action_effects: u64,
    /// MATCH, all probes, actions and receipt ownership share this allowance.
    /// RETURN's additional usage is in the returned GqlQueryExecution.
    pub evaluator: GlaExecutionStats,
}

/// One private input column, read after all of the selected edge's SET clauses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphEdgeReturnBinding {
    Edge,
    /// A missing field reads NULL. The host applies property authority first.
    Property(PropertyKeyId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphEdgeUpsertQueryBuildError {
    TooManyBindings { limit: usize, observed: usize },
    Projection(RowProjectionBuildError),
    Aggregate(crate::GraphAggregateBuildError),
}
impl core::fmt::Display for GraphEdgeUpsertQueryBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooManyBindings { limit, observed } => {
                write!(f, "RETURN has {observed} bindings, limit {limit}")
            }
            Self::Projection(error) => error.fmt(f),
            Self::Aggregate(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for GraphEdgeUpsertQueryBuildError {}

/// RETURN errors remain distinct from the existing non-returning upsert API.
#[derive(Debug)]
pub enum GraphEdgeUpsertQueryError<E, A> {
    Upsert(GraphEdgeUpsertError<E, A>),
    Returning(GraphSetExecutionError<E>),
}
impl<E: core::fmt::Display, A: core::fmt::Display> core::fmt::Display
    for GraphEdgeUpsertQueryError<E, A>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Upsert(error) => error.fmt(f),
            Self::Returning(error) => error.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static> core::error::Error
    for GraphEdgeUpsertQueryError<E, A>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Upsert(error) => Some(error),
            Self::Returning(error) => Some(error),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PreparedGraphEdgeUpsertQuery {
    upsert: PreparedGraphEdgeUpsert,
    bindings: Vec<GraphEdgeReturnBinding>,
    projection: Vec<GraphSetProjection>,
    grouping: Option<crate::set_ops::WriteReturnGroup>,
    quantifier: GraphSetQuantifier,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    order: Vec<GraphValueOrder>,
    offset: u64,
    count: Option<u64>,
    tail: Option<crate::PreparedGraphSet>,
}
impl core::fmt::Debug for PreparedGraphEdgeUpsertQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphEdgeUpsertQuery")
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphEdgeUpsertQuery {
    pub fn prepare(
        upsert: PreparedGraphEdgeUpsert,
        bindings: Vec<GraphEdgeReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, GraphEdgeUpsertQueryBuildError> {
        Self::prepare_with_grouping(upsert, bindings, projection, quantifier, None)
    }

    pub(crate) fn prepare_with_grouping(
        upsert: PreparedGraphEdgeUpsert,
        bindings: Vec<GraphEdgeReturnBinding>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
        grouping: Option<crate::set_ops::WriteReturnGroupSpec>,
    ) -> Result<Self, GraphEdgeUpsertQueryBuildError> {
        let limit = crate::algebra::MAX_PATTERN_VERTICES;
        if bindings.len() > limit {
            return Err(GraphEdgeUpsertQueryBuildError::TooManyBindings {
                limit,
                observed: bindings.len(),
            });
        }
        let input_types = bindings
            .iter()
            .map(|binding| match binding {
                GraphEdgeReturnBinding::Edge => GraphSetColumnType::Edge,
                GraphEdgeReturnBinding::Property(_) => GraphSetColumnType::Stored,
            })
            .collect::<Vec<_>>();
        let grouping = grouping
            .map(|group| group.prepare(input_types.clone()))
            .transpose()
            .map_err(GraphEdgeUpsertQueryBuildError::Aggregate)?;
        let output_input = grouping
            .as_ref()
            .map(|group| group.column_types().to_vec())
            .unwrap_or(input_types);
        let checked = RowProjectionSpec::new(output_input, projection.clone(), quantifier)
            .map_err(GraphEdgeUpsertQueryBuildError::Projection)?;
        Ok(Self {
            upsert,
            bindings,
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
    pub const fn upsert(&self) -> &PreparedGraphEdgeUpsert {
        &self.upsert
    }
    #[must_use]
    pub fn bindings(&self) -> &[GraphEdgeReturnBinding] {
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

    /// The tail's schema is public output; the WITH head remains private.
    pub(crate) fn with_tail(mut self, tail: crate::PreparedGraphSet) -> Self {
        self.columns = tail.columns().to_vec();
        self.types = tail.column_types().to_vec();
        self.tail = Some(tail);
        self
    }

    /// Finalize zero or one final-state input row under the statement's
    /// cumulative counters. The host must retain its rollback guard until this
    /// succeeds. LIMIT 0 discards output, never the MERGE or its field reads.
    pub fn project_governed<E, C>(
        &self,
        row: Option<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_rows_governed(
            row.into_iter().collect(),
            selection,
            evaluator,
            policy,
            checkpoint,
        )
    }

    /// Finalize one chosen relationship with reads of computed identities in
    /// its final private overlay. The host retains authorization and rollback.
    pub fn project_governed_reading<E, C>(
        &self,
        row: Option<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        elements: impl crate::GraphElementReader<E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_rows_governed_reading(
            row.into_iter().collect(),
            selection,
            evaluator,
            policy,
            elements,
            checkpoint,
        )
    }

    /// Finalize the complete ordered occurrence bag, not one projection per
    /// relationship. The write host must have admitted all input ownership and
    /// read each field from the final staged graph. Duplicate identities remain
    /// duplicate rows until explicit DISTINCT or grouping removes them.
    /// Aggregates and WITH stages see the whole bag; only the final page spends
    /// ResultRows. Errors beyond LIMIT still refuse the complete statement.
    pub fn project_rows_governed<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_final_rows(
            rows,
            selection,
            evaluator,
            policy,
            &mut |_, _| None,
            checkpoint,
        )
    }

    /// Evaluate the complete output bag using final staged graph point reads.
    /// This includes grouping and every WITH stage, without resetting budgets
    /// or weakening the original occurrence and input-cell validation.
    pub fn project_rows_governed_reading<E, C>(
        &self,
        rows: Vec<Vec<GraphValue>>,
        selection: GqlExecutionStats,
        evaluator: GlaExecutionStats,
        policy: GqlQueryPolicy,
        mut elements: impl crate::GraphElementReader<E>,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<E>, C>> {
        self.project_final_rows(
            rows,
            selection,
            evaluator,
            policy,
            &mut crate::set_ops::projection::HostElements(&mut elements),
            checkpoint,
        )
    }

    fn project_final_rows<E, C>(
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
        for (at, row) in rows.iter().enumerate() {
            // Keep the singular path's charge while accounting for additional
            // occurrences even when RETURN has no field/identity bindings.
            if at != 0 {
                work()?;
            }
            let invalid =
                || GqlQueryError::Source(GraphSetExecutionError::InputSchema { operand: 0 });
            if row.len() != self.bindings.len() {
                return Err(invalid());
            }
            for (binding, value) in self.bindings.iter().zip(row) {
                work()?;
                if !matches!(
                    (binding, value),
                    (GraphEdgeReturnBinding::Edge, GraphValue::Edge(_))
                        | (GraphEdgeReturnBinding::Property(_), GraphValue::Scalar(_))
                ) {
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
        let mut bytes = b"fgdb:directed-edge-upsert-query:v1\0".to_vec();
        let upsert = self.upsert.canonical_bytes();
        bytes.extend_from_slice(&(upsert.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&upsert);
        bytes.extend_from_slice(&(self.bindings.len() as u64).to_be_bytes());
        for binding in &self.bindings {
            match binding {
                GraphEdgeReturnBinding::Edge => bytes.push(0),
                GraphEdgeReturnBinding::Property(key) => {
                    bytes.push(1);
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
    use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphEdgeUpsertQueryText};
    use fgdb_delta_types::RelationId;
    use fgdb_types::{CanonicalScalar, EId, VId};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const PREFIX: &str = "MATCH (a),(b) MERGE (a)-[r:R]->(b) \
        ON CREATE SET r.p=2 ON MATCH SET r.p=r.p+1 SET r.p=r.p*2";
    const EDGE: EId = EId((1_u128 << 100) + 7);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }
    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn prepare(tail: &str) -> PreparedGraphEdgeUpsertQuery {
        PreparedGraphEdgeUpsertQueryText::prepare(&format!("{PREFIX} {tail}"), R, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }
    fn policy(rows: u64) -> GqlQueryPolicy {
        GqlQueryPolicy::new(1_000, rows, 1_000_000, 100_000)
    }
    fn input(query: &PreparedGraphEdgeUpsertQuery, value: i64) -> Vec<GraphValue> {
        query
            .bindings()
            .iter()
            .map(|binding| match binding {
                GraphEdgeReturnBinding::Edge => GraphValue::Edge(EDGE),
                GraphEdgeReturnBinding::Property(key) => {
                    assert_eq!(*key, P);
                    int(value)
                }
            })
            .collect()
    }
    fn finish(
        query: &PreparedGraphEdgeUpsertQuery,
        row: Option<Vec<GraphValue>>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, ()>>
    {
        query.project_governed(
            row,
            GqlExecutionStats::default(),
            GlaExecutionStats::default(),
            policy,
            || Ok(()),
        )
    }
    fn values(output: &GqlQueryExecution<GraphValueRow>) -> Vec<Vec<GraphValue>> {
        output
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    fn aliases_read_final_action_cells_without_losing_identity_or_leaking_hidden_columns() {
        let query = prepare(
            "WITH r AS edge, r AS again WHERE edge.p > 0 \
             RETURN edge AS identity, edge.p AS p, again.p AS again",
        );
        assert_eq!(
            query.bindings(),
            &[
                GraphEdgeReturnBinding::Edge,
                GraphEdgeReturnBinding::Property(P)
            ]
        );
        assert_eq!(query.columns(), &["identity", "p", "again"]);
        let output = finish(
            &query,
            Some(vec![GraphValue::Edge(EDGE), int(14)]),
            policy(1),
        )
        .unwrap();
        assert_eq!(
            values(&output),
            vec![vec![GraphValue::Edge(EDGE), int(14), int(14)]]
        );
        let star = prepare("WITH r AS __fg_boundary_0 WHERE __fg_boundary_0.p > 0 RETURN *");
        assert_eq!(star.columns(), &["__fg_boundary_0"]);
        assert_eq!(
            values(&finish(&star, Some(input(&star, 14)), policy(1)).unwrap()),
            vec![vec![GraphValue::Edge(EDGE)]]
        );
    }

    #[test]
    fn only_final_rows_spend_the_result_grant_including_zero_and_empty_input() {
        let query = prepare("WITH r WHERE r.p < 0 RETURN r");
        let output = finish(&query, Some(input(&query, 14)), policy(0)).unwrap();
        assert!(output.value.is_empty());
        assert_eq!(output.rows.result_rows, 0);
        let query = prepare("WITH r.p AS p UNWIND [p,p+1,p+1] AS x RETURN count(*) AS n");
        let output = finish(&query, Some(input(&query, 14)), policy(1)).unwrap();
        assert_eq!(values(&output), vec![vec![int(3)]]);
        assert_eq!(output.rows.result_rows, 1);
        assert!(finish(&query, Some(input(&query, 14)), policy(0)).is_err());
        assert_eq!(
            values(&finish(&query, None, policy(1)).unwrap()),
            vec![vec![int(0)]]
        );
        let constant = prepare("WITH r RETURN 42 AS n");
        assert!(finish(&constant, None, policy(0)).unwrap().value.is_empty());
        let grouped_head = prepare("WITH count(*) AS n RETURN n");
        assert_eq!(
            values(&finish(&grouped_head, None, policy(1)).unwrap()),
            vec![vec![int(0)]]
        );
    }

    #[test]
    fn final_api_order_and_page_do_not_replace_the_with_head_window() {
        let query = prepare(
            "WITH r.p AS p LIMIT 1 UNWIND [p,p+1,p+2] AS x \
             RETURN x AS value, -x AS opposite",
        );
        let order = [GraphValueOrder {
            column: 1,
            descending: false,
            nulls_first: false,
        }];
        let query = query.with_order_by(&order).unwrap().with_page(1, Some(1));
        let output = finish(&query, Some(input(&query, 4)), policy(1)).unwrap();
        assert_eq!(values(&output), vec![vec![int(5), int(-5)]]);
        let empty_head = prepare("WITH r LIMIT 0 RETURN count(*) AS n").with_page(0, Some(1));
        assert_eq!(
            values(&finish(&empty_head, Some(input(&empty_head, 4)), policy(1)).unwrap()),
            vec![vec![int(0)]]
        );
    }

    #[test]
    fn rebind_tail_parameters_and_keep_plain_return_identity_unchanged() {
        let text = format!("{PREFIX} WITH r.p AS p WHERE p >= $minimum RETURN p+$extra AS result");
        let template = PreparedGraphEdgeUpsertQueryText::prepare(&text, R, symbols).unwrap();
        assert_eq!(template.parameter_schema().len(), 2);
        let arguments = |minimum| {
            GqlParameters::new()
                .with_int64("minimum", minimum)
                .unwrap()
                .with_int64("extra", 3)
                .unwrap()
        };
        let visible = template.bind_parameters(&arguments(4)).unwrap();
        let hidden = template.bind_parameters(&arguments(5)).unwrap();
        assert_eq!(
            values(&finish(&visible, Some(input(&visible, 4)), policy(1)).unwrap()),
            vec![vec![int(7)]]
        );
        assert!(
            finish(&hidden, Some(input(&hidden, 4)), policy(0))
                .unwrap()
                .value
                .is_empty()
        );
        assert_ne!(visible.canonical_bytes(), hidden.canonical_bytes());
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());
        let plain = prepare("RETURN r.p AS p");
        let native = PreparedGraphEdgeUpsertQuery::prepare(
            plain.upsert().clone(),
            vec![GraphEdgeReturnBinding::Property(P)],
            vec![GraphSetProjection::new(
                "p",
                crate::GraphSetValue::Column(0),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
        assert_eq!(plain.canonical_bytes(), native.canonical_bytes());
        assert_ne!(
            plain.canonical_bytes(),
            prepare("WITH r.p AS p RETURN p").canonical_bytes()
        );
    }

    #[test]
    fn native_tail_errors_and_input_schema_errors_survive_limit_zero() {
        for suffix in [
            "WITH r.p AS p RETURN 10/(p-4) AS n LIMIT 0",
            "WITH r.p AS p RETURN 10/(p-4) AS n",
        ] {
            let query = prepare(suffix).with_page(0, Some(0));
            assert!(matches!(
                finish(&query, Some(input(&query, 4)), policy(0)),
                Err(GqlQueryError::Source(
                    GraphSetExecutionError::Projection { .. }
                ))
            ));
        }
        let query = prepare("WITH r RETURN 1 AS n LIMIT 0");
        for row in [vec![], vec![GraphValue::Vertex(VId(1))]] {
            assert!(matches!(
                finish(&query, Some(row), policy(0)),
                Err(GqlQueryError::Source(
                    GraphSetExecutionError::InputSchema { .. }
                ))
            ));
        }
    }

    #[test]
    fn work_scratch_and_each_cancellation_point_cover_head_and_tail() {
        let query = prepare("WITH r.p AS p UNWIND [p,p+1,p+1] AS x RETURN DISTINCT x ORDER BY x");
        let prefix = GlaExecutionStats {
            work_units: 17,
            scratch_entries: 11,
        };
        let mut checkpoints = 0_usize;
        let output = query
            .project_governed::<(), usize>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                policy(2),
                || {
                    checkpoints += 1;
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(values(&output), vec![vec![int(4)], vec![int(5)]]);
        assert!(output.evaluator.work_units > prefix.work_units);
        assert!(output.evaluator.scratch_entries > prefix.scratch_entries);
        let work = output.evaluator.work_units;
        let scratch = output.evaluator.scratch_entries;
        let exact = GqlQueryPolicy::new(1_000, 2, work, scratch);
        let rerun = query
            .project_governed::<(), ()>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                exact,
                || Ok(()),
            )
            .unwrap();
        assert_eq!(output.value, rerun.value);
        for limits in [(work - 1, scratch), (work, scratch - 1)] {
            let result = query.project_governed::<(), ()>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                GqlQueryPolicy::new(1_000, 2, limits.0, limits.1),
                || Ok(()),
            );
            assert!(matches!(result, Err(GqlQueryError::Evaluator(_))));
        }
        assert!(checkpoints > 1);
        for fail_at in 0..checkpoints {
            let mut at = 0;
            let result = query.project_governed::<(), usize>(
                Some(input(&query, 4)),
                GqlExecutionStats::default(),
                prefix,
                policy(2),
                || {
                    let current = at;
                    at += 1;
                    if current == fail_at {
                        Err(current)
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(index)) if index == fail_at));
        }
    }

    #[test]
    fn malformed_or_unowned_tail_leaves_refuse_before_catalog_callbacks() {
        for suffix in [
            "WITH r AS edge RETURN missing.p",
            "WITH r AS edge RETURN a",
            "WITH r.p AS p RETURN r",
            "WITH r RETURN r.p +",
            "WITH * RETURN r",
            "WITH r RETURN r; RETURN 1",
        ] {
            let mut calls = 0;
            let result = PreparedGraphEdgeUpsertQueryText::prepare(
                &format!("{PREFIX} {suffix}"),
                R,
                |kind: crate::GraphSymbolKind, name: &str| {
                    calls += 1;
                    symbols(kind, name)
                },
            );
            assert!(result.is_err(), "{suffix}");
            assert_eq!(calls, 0, "{suffix}");
        }
        let text = format!("{PREFIX} WITH r WHERE r.p > 0 RETURN r.p");
        assert!(PreparedGraphEdgeUpsertQueryText::has_return_clause(&text).unwrap());
        assert!(!crate::PreparedGraphEdgeMergeQueryText::has_return_clause(&text).unwrap());
        assert!(!crate::PreparedGraphMutationQueryText::has_return_clause(&text).unwrap());
    }

    #[test]
    fn batch_with_groups_and_tail_consume_only_the_final_result_grant() {
        let query = prepare("WITH r.p AS p,count(*) AS n WHERE p > 100 RETURN p");
        let result = query
            .project_rows_governed::<(), ()>(
                vec![input(&query, 4), input(&query, 5)],
                GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 2,
                },
                GlaExecutionStats::default(),
                policy(0),
                || Ok(()),
            )
            .unwrap();
        assert!(result.value.is_empty());
        assert_eq!(result.rows.result_rows, 0);
        let query = prepare("WITH r.p AS p UNWIND [p,p+1] AS x RETURN count(*) AS n");
        let result = query
            .project_rows_governed::<(), ()>(
                vec![input(&query, 4), input(&query, 5)],
                GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: 2,
                },
                GlaExecutionStats::default(),
                policy(1),
                || Ok(()),
            )
            .unwrap();
        assert_eq!(values(&result), vec![vec![int(4)]]);
        let query = prepare("WITH r.p AS p RETURN 10/p AS n LIMIT 0");
        let result = query.project_rows_governed::<(), ()>(
            vec![input(&query, 4), input(&query, 0)],
            GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 2,
            },
            GlaExecutionStats::default(),
            policy(0),
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
    }
}
