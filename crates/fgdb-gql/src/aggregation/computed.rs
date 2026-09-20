//! Computed inputs for the existing GroupAggregate implementation.
//!
//! The ordinary unprojected input keeps its streaming/factorized path. This
//! bounded projected path owns computed rows while the SAME accumulators borrow
//! them. No ephemeral scalar is retained by reference, no synthetic graph is
//! built, and neither source matching nor aggregate semantics are reimplemented.

mod incremental;

use super::*;
use crate::{GraphSetProjection, GraphSetQuantifier, PreparedGraphSet};
use fgdb_types::EId;
use std::cell::RefCell;

impl PreparedGraphAggregate {
    /// Compute named input columns before grouping and aggregate argument
    /// DISTINCT. Columns refer to the original unpaginated ALL pattern, never
    /// to another projected alias. Keys and aggregate arguments refer to the
    /// projected schema. Constants retain one occurrence per complete match.
    ///
    /// Reuses GraphSetProjection's scalar/type checks and checked integer VM.
    /// Computed inputs are materialized under the same cumulative work/scratch
    /// allowance as source admission and summaries. This is not a spill-backed
    /// path. Plain prepare() retains its original streaming implementation.
    pub fn prepare_projected(
        input: PreparedGraphPattern<GraphValueRow>,
        projection: Vec<GraphSetProjection>,
        keys: &[usize],
        aggregates: &[GraphAggregate<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<Self, GraphAggregateBuildError> {
        Self::prepare_input(
            input,
            Some(projection),
            None,
            keys,
            aggregates,
            offset,
            count,
        )
    }

    /// The optional value transformation between matching and aggregation.
    /// input_pattern() always describes the actual graph source and therefore
    /// retains the existing storage admission and transaction observation path.
    #[must_use]
    pub fn input_projection(&self) -> Option<&[GraphSetProjection]> {
        self.computed_input.as_deref()
    }

    /// Admission for physical reducers that execute the optional row-local
    /// input projection themselves. This is NOT delta-maintenance admission:
    /// that public contract continues to reject transformed definitions.
    /// The consumer must independently admit the complete graph input and its
    /// accumulator family. A relational child cannot be replaced by its leaf,
    /// and no result modifier may disappear merely because input is empty.
    pub(crate) fn supports_row_local_aggregate_stream(&self) -> bool {
        self.relational_input.is_none()
            && self.having.is_empty()
            && self.having_expression.is_none()
            && self.output_projection.is_none()
            && !self.output_distinct
            && self.offset == 0
            && self.count.is_none()
            && self.ordering.is_empty()
            && self.key_output.is_none()
            && self.output_aggregates == self.aggregates.len()
    }

    /// Consume one complete admitted binding without retaining an input table.
    /// All declared projection columns run in declaration order, including
    /// unused columns, through the same scalar/type/payload evaluator as batch
    /// aggregation. Group keys and arguments subsequently address this OUTPUT
    /// schema, never the original graph projection's positions.
    ///
    /// Like evaluate_incremental_input, InputExpression.row is zero: this
    /// invocation owns one local binding, not a sorted graph-wide row ordinal.
    /// Source and expression failures are encountered in physical pull order,
    /// unlike batch execution's source-materialization-then-projection order.
    /// Control failures remain their original errors. Unprojected definitions
    /// return their row directly with no extra copies or control events.
    pub(crate) fn evaluate_streamed_input<E, C>(
        &self,
        input: GraphValueRow,
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<GraphValueRow, GqlQueryError<GraphAggregateError<E>, C>> {
        match &self.computed_input {
            None => Ok(input),
            Some(projection) => GraphSetProjection::evaluate_row_with_control(
                &input,
                projection,
                control,
                |column, error| {
                    GqlQueryError::Source(GraphAggregateError::InputExpression {
                        row: 0,
                        column,
                        error,
                    })
                },
            ),
        }
    }

    /// Execute an identified source whose property expressions refer to vertices.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_identified_properties<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        self.execute_governed_with_element_properties(
            snapshot_records,
            vertices,
            edges,
            test_vertex,
            property,
            |_, _| Ok(None),
            policy,
            checkpoint,
        )
    }

    /// Summarize paths using the real edge identities of one admitted source.
    /// This is the identified counterpart of execute_governed, not another
    /// matcher or aggregate implementation. Uncaptured inputs retain their
    /// ordinary streaming/factorized path, counters and source error types.
    ///
    /// Captured inputs own bounded rows through the existing GLA executor;
    /// projections and row pipelines run before the same borrowed accumulators.
    /// Distinct paths remain distinct even when their endpoints or lengths agree.
    /// All source, path-copy, projection and grouping work shares one allowance.
    /// Only final groups consume the public result-row quota. An output LIMIT 0
    /// never suppresses source, path or scalar failures.
    ///
    /// The caller must supply one immutable, authorized graph generation. EIds
    /// must be the real source identities, not ordinals assigned to edge triples.
    /// This is bounded materialization, not spill-backed path aggregation.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_governed_with_element_properties<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (EId, VId, RelationId, VId)>,
        mut test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        mut property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        mut edge_property: impl FnMut(EId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        if !self.input.plan().requires_identified_edges() {
            return self.execute_governed(
                snapshot_records,
                vertices,
                edges.into_iter().map(|(_, s, r, d)| (s, r, d)),
                test_vertex,
                property,
                policy,
                checkpoint,
            );
        }
        if self.relational_input.is_some() {
            // Both the source and row-stage owner reach the same stateful
            // checkpoint. No RefCell borrow spans either executor invocation.
            let checkpoint = RefCell::new(&mut checkpoint);
            let mut admitted = Some((vertices, edges));
            return self.execute_relational_with_source(
                policy,
                |pattern, remaining| {
                    let (vertices, edges) = admitted
                        .take()
                        .expect("single-source aggregate preparation admits exactly one leaf");
                    pattern.plan().execute_governed_with_element_properties(
                        snapshot_records,
                        vertices,
                        edges,
                        &mut test_vertex,
                        &mut property,
                        &mut edge_property,
                        remaining,
                        || (*checkpoint.borrow_mut())(),
                    )
                },
                || (*checkpoint.borrow_mut())(),
            );
        }
        let source_policy = GqlQueryPolicy {
            rows: crate::GqlExecutionBudget::snapshot_records(
                policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
            ),
            evaluator: policy.evaluator,
        };
        let source = self
            .input
            .plan()
            .execute_governed_with_element_properties(
                snapshot_records,
                vertices,
                edges,
                test_vertex,
                property,
                edge_property,
                source_policy,
                &mut checkpoint,
            )
            .map_err(|error| error.map_source(GraphAggregateError::Source))?;
        self.finish_materialized_governed(source, policy, checkpoint)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_projected_governed<'a, E, C>(
        &self,
        snapshot_records: u64,
        vertices: impl IntoIterator<Item = VId>,
        edges: impl IntoIterator<Item = (VId, RelationId, VId)>,
        test_vertex: impl FnMut(VId, &[VertexPredicate]) -> Result<bool, E>,
        property: impl FnMut(VId, PropertyKeyId) -> Result<Option<&'a CanonicalScalar>, E>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        // MATCH rows are private aggregate input, not public result rows. They
        // still pay every ordinary GLA instruction and allocation. Source
        // admission is counted once, not once again for the computed relation.
        let source_policy = GqlQueryPolicy {
            rows: crate::GqlExecutionBudget::snapshot_records(
                policy.rows.max_snapshot_records().unwrap_or(u64::MAX),
            ),
            evaluator: policy.evaluator,
        };
        let source = self
            .input
            .plan()
            .execute_governed_with_properties(
                snapshot_records,
                vertices,
                edges,
                test_vertex,
                property,
                source_policy,
                &mut checkpoint,
            )
            .map_err(|error| error.map_source(GraphAggregateError::Source))?;
        self.finish_materialized_governed(source, policy, checkpoint)
    }

    /// Source rows are already admitted. Keep them alive until the shared
    /// summary/result engine has copied its final owned cells. No second input
    /// clone or freshly initialized evaluator budget is introduced here.
    fn finish_materialized_governed<E, C>(
        &self,
        source: GqlQueryExecution<GraphValueRow>,
        policy: GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>>
    {
        let mut evaluator = source.evaluator;
        let mut rows = GqlExecutionStats {
            snapshot_records: source.rows.snapshot_records,
            result_rows: 0,
        };
        let mut control = |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let result_rows = if event == GlaExecutionEvent::ResultRow {
                // The vector backing a completed result cannot contain 2^64
                // rows. The checked path keeps the refusal explicit anyway.
                let next = rows.result_rows.checked_add(1).ok_or_else(|| {
                    GqlQueryError::Source(GraphAggregateError::ResultCountOverflow)
                })?;
                policy
                    .rows
                    .check(GqlBudgetDimension::ResultRows, next)
                    .map_err(GqlQueryError::Rows)?;
                next
            } else {
                rows.result_rows
            };
            evaluator
                .charge_event(policy.evaluator, event)
                .map_err(GqlQueryError::Evaluator)?;
            rows.result_rows = result_rows;
            Ok(())
        };
        let input = if let Some(projection) = &self.computed_input {
            let mut computed = Vec::new();
            for (row, input) in source.value.into_iter().enumerate() {
                control(GlaExecutionEvent::ScratchEntry)?;
                // Reuse the read-projection evaluator, including payload charging,
                // frozen column references and lazy scalar branches. Source reads
                // above remain eager and source errors remain typed errors.
                let value = GraphSetProjection::evaluate_row_with_control(
                    &input,
                    projection,
                    &mut control,
                    |column, error| {
                        GqlQueryError::Source(GraphAggregateError::InputExpression {
                            row,
                            column,
                            error,
                        })
                    },
                )?;
                computed.push(value);
            }
            computed
        } else {
            source.value
        };
        let value = self.summarize_projected_rows(&input, &mut control)?;
        control(GlaExecutionEvent::Work)?;
        Ok(GqlQueryExecution {
            value,
            rows,
            evaluator,
        })
    }

    pub(super) fn summarize_projected_rows<'a, E, C>(
        &self,
        input: &'a [GraphValueRow],
        control: &mut impl FnMut(
            GlaExecutionEvent,
        ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<Vec<GraphAggregateRow>, GqlQueryError<GraphAggregateError<E>, C>> {
        let mut groups: BTreeMap<Vec<ValueRef<'a>>, Vec<Accumulator<'a>>> = BTreeMap::new();
        if self.keys.is_empty() {
            control(GlaExecutionEvent::ScratchEntry)?;
            groups.insert(Vec::new(), new_group(&self.aggregates, control)?);
        }
        for row in input {
            control(GlaExecutionEvent::Work)?;
            let mut values = [ValueRef::Scalar(&NULL); MAX_PATTERN_VERTICES];
            for (at, value) in row.values().iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                values[at] = match value {
                    GraphValue::Vertex(vertex) => ValueRef::Vertex(*vertex),
                    GraphValue::Scalar(scalar) => ValueRef::Scalar(scalar),
                    GraphValue::Path(path) => ValueRef::Path(path),
                    GraphValue::Vertices(vertices) => ValueRef::Vertices(vertices),
                    GraphValue::Edges(edges) => ValueRef::Edges(edges),
                    GraphValue::Edge(edge) => ValueRef::Edge(*edge),
                    GraphValue::List(values) => ValueRef::List(values),
                };
                for _ in 0..values[at].payload_units() {
                    control(GlaExecutionEvent::Work)?;
                }
            }
            let mut key = [ValueRef::Scalar(&NULL); MAX_PATTERN_VERTICES];
            for (at, column) in self.keys.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                key[at] = values[*column];
            }
            let key = &key[..self.keys.len()];
            control(GlaExecutionEvent::Work)?;
            if !groups.contains_key(key) {
                control(GlaExecutionEvent::ScratchEntry)?;
                let mut owned_key = Vec::new();
                for value in key {
                    control(GlaExecutionEvent::ScratchEntry)?;
                    owned_key.push(*value);
                }
                let state = new_group(&self.aggregates, control)?;
                groups.insert(owned_key, state);
            }
            let state = groups.get_mut(key).expect("admitted projected group");
            update_group(
                &self.aggregates,
                state,
                &values[..row.len()],
                weighted::Multiplicity::ONE,
                control,
            )?;
        }
        // The existing result engine still owns HAVING, hidden keys/summaries,
        // exact averages, output DISTINCT, ranking, paging and owned release.
        self.finish_groups(&groups, control)
    }

    pub(super) fn append_input_projection(&self, bytes: &mut Vec<u8>) {
        let Some(projection) = &self.computed_input else {
            return;
        };
        bytes.extend_from_slice(b"fgdb:aggregate-input-projection:v1\0");
        bytes.extend_from_slice(&(projection.len() as u64).to_be_bytes());
        for column in projection {
            column.value().append_canonical_bytes(bytes);
        }
    }
}

/// Validate exactly the same projected schema as ordinary computed reads.
/// The temporary definition exists only during preparation and is never
/// executed. It does not replace or fabricate the aggregate's graph source.
pub(super) fn projected_schema(
    input: &PreparedGraphPattern<GraphValueRow>,
    projection: &[GraphSetProjection],
) -> Result<Vec<String>, GraphAggregateBuildError> {
    let definition = PreparedGraphSet::from(input.clone())
        .project(projection.to_vec(), GraphSetQuantifier::All)
        .map_err(GraphAggregateBuildError::InputProjection)?;
    Ok(definition.columns().to_vec())
}
