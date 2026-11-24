//! Compile ordinary MATCH rows into counted tuples for incremental consumers.
//!
//! The input is the original GLA, not a new matcher. Grouping every projected
//! column retains an exact occurrence count for each complete tuple. A consumer
//! must expand that count (or threshold it for DISTINCT) BEFORE applying the
//! original result window. Executing the carrier as the user query is incorrect.

use crate::algebra::{
    GlaOperator, GraphPathFunction, GraphValue, GraphValueRow, MAX_PATTERN_VERTICES,
    PreparedGraphPattern, ValueProjection,
};
use crate::{
    GlaExecutionEvent, GraphAggregate, GraphAggregateColumn, GraphAggregateOrder,
    GraphAggregateRow, GraphAggregateValue, GraphNullPlacement, PreparedGraphAggregate,
};

struct Output<'a> {
    projection: usize,
    columns: &'a [ValueProjection],
    order: &'a [super::GraphValueOrder],
    distinct: bool,
    offset: u64,
    count: Option<u64>,
}

/// Compiler-owned stages for DISTINCT endpoint reachability. The incidence
/// input contains all authorized transit vertices. Optional endpoint membership
/// sources must be semi-joined AFTER closure and BEFORE the final projection.
#[derive(Clone, Debug)]
pub struct IncrementalClosureDefinition {
    input: crate::PreparedGraphSet,
    endpoint_filters: [Option<crate::PreparedGraphSet>; 2],
    projection: crate::row_projection::RowProjectionSpec,
}

impl IncrementalClosureDefinition {
    #[must_use]
    pub const fn input(&self) -> &crate::PreparedGraphSet {
        &self.input
    }

    #[must_use]
    pub const fn endpoint_filters(&self) -> &[Option<crate::PreparedGraphSet>; 2] {
        &self.endpoint_filters
    }

    #[must_use]
    pub const fn projection(&self) -> &crate::row_projection::RowProjectionSpec {
        &self.projection
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        crate::PreparedGraphSet,
        [Option<crate::PreparedGraphSet>; 2],
        crate::row_projection::RowProjectionSpec,
    ) {
        (self.input, self.endpoint_filters, self.projection)
    }
}

impl PreparedGraphPattern<GraphValueRow> {
    /// Compile the support of one unbounded TRAIL into a counted edge/lifetime
    /// relation, optional endpoint membership sources and an endpoint projection.
    /// The host must apply deletion-safe closure, then the endpoint semi joins,
    /// then the projection. Label selection never restricts transit vertices.
    ///
    /// DISTINCT is load-bearing: closure has one row per reachable pair, not
    /// one row per path. Endpoint-only output may retain either or both vertices,
    /// with aliases and repetitions. Projecting away an endpoint thresholds the
    /// remaining support so deleting one witness cannot retract another.
    ///
    /// The admitted interval is zero-or-more or one-or-more hops, with optional
    /// endpoint label conjunctions and no captures, edge/property predicates,
    /// order or page. Zero-hop support adds one self pair per visible vertex,
    /// including isolates, through an ordinary UNION ALL source. Those lifetime
    /// rows and real self-loops retain independent counted support, so deleting
    /// one does not retract the other. An undirected one-or-more trail cannot
    /// use this recipe: traversing one physical edge in both directions does
    /// not constitute a legal cycle. Zero-or-more support is sound in every
    /// direction because each distinct reachable pair has a simple path and
    /// every visible vertex has an empty path. Ordinary selection retains
    /// relation unions, wildcard types and source authorization before closure
    /// sees incidence or vertex lifetimes.
    #[must_use]
    pub fn incremental_closure_definition(&self) -> Option<IncrementalClosureDefinition> {
        use crate::algebra::{
            GlaDirection, GraphColumn, GraphPatternBuilder, GraphWalkSearch, VertexPredicate,
        };
        use crate::{
            GraphSetColumnType, GraphSetOperation, GraphSetProjection, GraphSetQuantifier,
            GraphSetValue, PreparedGraphSet,
        };

        let output = self.incremental_rows_output()?;
        if !output.distinct
            || !output.order.is_empty()
            || output.offset != 0
            || output.count.is_some()
        {
            return None;
        }
        let slots = output
            .columns
            .iter()
            .map(|column| match column {
                ValueProjection::Vertex { slot } if slot.ordinal() < 2 => {
                    Some(usize::from(slot.ordinal() == 1))
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()?;
        let [GlaOperator::ScanVertices, remainder @ ..] =
            &self.plan().operators()[..output.projection]
        else {
            return None;
        };
        let mut expansion = None;
        let mut different_edges = false;
        let mut labels = [Vec::new(), Vec::new()];
        for operator in remainder {
            match operator {
                GlaOperator::VarLengthExpand { .. } if expansion.is_none() => {
                    expansion = Some(operator);
                }
                GlaOperator::Select { slot, predicates }
                    if slot.ordinal() < 2 && (slot.ordinal() == 0 || expansion.is_some()) =>
                {
                    for predicate in predicates {
                        let VertexPredicate::HasLabel(label) = predicate else {
                            return None;
                        };
                        labels[usize::from(slot.ordinal() == 1)].push(*label);
                    }
                }
                GlaOperator::DifferentEdges { segments }
                    if expansion.is_some()
                        && !different_edges
                        && segments.len() == 1
                        && segments[0].ordinal() == 1 =>
                {
                    different_edges = true;
                }
                _ => return None,
            }
        }
        let GlaOperator::VarLengthExpand {
            source,
            relation,
            direction,
            bounds,
            search,
            edge_predicates,
        } = expansion?
        else {
            return None;
        };
        if source.ordinal() != 0
            || bounds.minimum() > 1
            || bounds.is_bounded()
            || (*direction == GlaDirection::Undirected && bounds.minimum() != 0)
            || !edge_predicates.is_empty()
            || !matches!(search, GraphWalkSearch::Trail | GraphWalkSearch::All)
            || (*search == GraphWalkSearch::All && !different_edges)
        {
            return None;
        }

        let mut pattern = GraphPatternBuilder::new();
        pattern.vertex("source").ok()?.vertex("destination").ok()?;
        pattern
            .edge("source", *relation, *direction, "destination")
            .ok()?;
        let mut input: PreparedGraphSet = pattern
            .prepare_values(
                &[
                    GraphColumn::vertex("source", "source"),
                    GraphColumn::vertex("destination", "destination"),
                ],
                0,
                None,
            )
            .ok()?
            .with_duplicates()
            .into();
        if bounds.minimum() == 0 {
            let mut vertices = GraphPatternBuilder::new();
            vertices.vertex("vertex").ok()?;
            let lifetimes = vertices
                .prepare_values(
                    &[
                        GraphColumn::vertex("source", "vertex"),
                        GraphColumn::vertex("destination", "vertex"),
                    ],
                    0,
                    None,
                )
                .ok()?
                .with_duplicates();
            input = input
                .combine(
                    GraphSetOperation::Union,
                    GraphSetQuantifier::All,
                    lifetimes.into(),
                )
                .ok()?;
        }

        let mut endpoint_filters = [None, None];
        for (endpoint, mut labels) in labels.into_iter().enumerate() {
            if labels.is_empty() {
                continue;
            }
            labels.sort_unstable();
            labels.dedup();
            let mut vertices = GraphPatternBuilder::new();
            vertices.vertex("vertex").ok()?;
            for label in labels {
                vertices
                    .filter("vertex", VertexPredicate::HasLabel(label))
                    .ok()?;
            }
            endpoint_filters[endpoint] = Some(
                vertices
                    .prepare_values(&[GraphColumn::vertex("vertex", "vertex")], 0, None)
                    .ok()?
                    .with_duplicates()
                    .into(),
            );
        }
        // Keeping both endpoint IDs is injective even with repeated columns.
        // Otherwise several reachable pairs may support one projected row.
        let quantifier = if slots.contains(&0) && slots.contains(&1) {
            GraphSetQuantifier::All
        } else {
            GraphSetQuantifier::Distinct
        };
        let projection = crate::row_projection::RowProjectionSpec::new(
            vec![GraphSetColumnType::Vertex; 2],
            self.columns()
                .iter()
                .zip(slots)
                .map(|(name, slot)| {
                    GraphSetProjection::new(name.clone(), GraphSetValue::Column(slot))
                })
                .collect(),
            quantifier,
        )
        .ok()?;
        Some(IncrementalClosureDefinition {
            input,
            endpoint_filters,
            projection,
        })
    }

    fn incremental_rows_output(&self) -> Option<Output<'_>> {
        let ops = self.plan().operators();
        let order_at = ops.len().checked_sub(2)?;
        let (offset, count) = match ops.last()? {
            GlaOperator::Limit { offset, count } => (*offset, *count),
            _ => return None,
        };
        let order = match ops.get(order_at)? {
            GlaOperator::OrderByValues => &[][..],
            GlaOperator::OrderByValueColumns { columns } => columns.as_ref(),
            _ => return None,
        };
        let previous = order_at.checked_sub(1)?;
        let distinct = matches!(ops.get(previous), Some(GlaOperator::Distinct));
        let projection = previous.checked_sub(usize::from(distinct))?;
        let GlaOperator::ProjectValues { columns } = ops.get(projection)? else {
            return None;
        };
        // One private COUNT cell uses the existing aggregate schema bound.
        // Hidden sort-carrier columns and collection/captured-element outputs
        // require a separate admitted derivative, never silent truncation.
        if columns.is_empty()
            || columns.len() >= MAX_PATTERN_VERTICES
            || columns.len() != self.columns().len()
            || columns.iter().any(|column| {
                !matches!(
                    column,
                    ValueProjection::Vertex { .. }
                        | ValueProjection::Property { .. }
                        | ValueProjection::EdgeProperty { .. }
                        | ValueProjection::Path {
                            function: GraphPathFunction::Edge,
                            ..
                        }
                )
            })
            || order.iter().any(|order| order.column >= columns.len())
            || ops[..projection].iter().any(|op| {
                matches!(
                    op,
                    GlaOperator::ProjectValues { .. }
                        | GlaOperator::Distinct
                        | GlaOperator::OrderByValues
                        | GlaOperator::OrderByValueColumns { .. }
                        | GlaOperator::Limit { .. }
                )
            })
        {
            return None;
        }
        Some(Output {
            projection,
            columns,
            order,
            distinct,
            offset,
            count,
        })
    }

    /// Private counted-tuple producer for an incremental row consumer. Every
    /// key is a projected column in original order; the sole COUNT is tuple
    /// multiplicity. There is no global zero row on empty MATCH input.
    ///
    /// The caller MUST retain this original pattern and apply its quantifier,
    /// ordering and window downstream. Source topology is admitted separately
    /// by the standing-query engine; this method is not an authority grant.
    /// Up to MAX_PATTERN_VERTICES - 1 scalar/vertex/edge columns are admitted.
    #[must_use]
    pub fn incremental_row_source_definition(&self) -> Option<PreparedGraphAggregate> {
        let output = self.incremental_rows_output()?;
        let mut input = self.clone();
        input.logical.operators.truncate(output.projection + 1);
        input.logical.operators.extend([
            GlaOperator::OrderByValues,
            GlaOperator::Limit {
                offset: 0,
                count: None,
            },
        ]);
        // Aliases are metadata, not execution. Select a deterministic private
        // name that cannot collide with any user-provided projected alias.
        let name = (0..=self.columns().len())
            .map(|at| format!("_fgdb_tuple_count_{at}"))
            .find(|name| !self.columns().contains(name))?;
        let keys: Vec<_> = (0..output.columns.len()).collect();
        PreparedGraphAggregate::prepare(input, &keys, &[GraphAggregate::count_rows(&name)], 0, None)
            .ok()
    }

    /// DISTINCT flag and occurrence-based output window. DISTINCT thresholds
    /// the integrated count, never the signed change or the current page.
    #[must_use]
    pub fn incremental_row_window(&self) -> Option<(bool, u64, Option<u64>)> {
        let output = self.incremental_rows_output()?;
        Some((output.distinct, output.offset, output.count))
    }

    /// Reuse the existing maintained-group comparator on the tuple's keys.
    /// An empty list means canonical complete-tuple order; explicit NULL
    /// placement and direction have the same semantics as ordinary GLA.
    #[must_use]
    pub fn incremental_row_ordering(&self) -> Option<Vec<GraphAggregateOrder>> {
        Some(
            self.incremental_rows_output()?
                .order
                .iter()
                .map(|order| GraphAggregateOrder {
                    column: GraphAggregateColumn::GroupKey(order.column),
                    descending: order.descending,
                    nulls: if order.nulls_first {
                        GraphNullPlacement::First
                    } else {
                        GraphNullPlacement::Last
                    },
                })
                .collect(),
        )
    }

    /// Decode one positive counted tuple into the ordinary value-row domain.
    /// Counts remain u64, never narrowed into scalars. Schema and zero-count
    /// refusal is None; cancellation/resource failures preserve their type.
    /// The caller owns final quotas, retractions and atomic publication.
    pub fn materialize_incremental_values<E>(
        &self,
        counted: &GraphAggregateRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(GraphValueRow, u64)>, E> {
        control(GlaExecutionEvent::Work)?;
        let Some(output) = self.incremental_rows_output() else {
            return Ok(None);
        };
        if counted.keys().len() != output.columns.len() {
            return Ok(None);
        }
        let [GraphAggregateValue::Count(count)] = counted.values() else {
            return Ok(None);
        };
        if *count == 0 {
            return Ok(None);
        }
        // Validate ALL cells before copying any payload, including columns
        // beyond the first decisive sort key or an eventually empty page.
        for (projection, value) in output.columns.iter().zip(counted.keys()) {
            control(GlaExecutionEvent::Work)?;
            let valid = match projection {
                ValueProjection::Property { .. } | ValueProjection::EdgeProperty { .. } => {
                    matches!(value, GraphValue::Scalar(_))
                }
                ValueProjection::Vertex { .. } => {
                    matches!(value, GraphValue::Vertex(_)) || value.is_null()
                }
                ValueProjection::Path {
                    function: GraphPathFunction::Edge,
                    ..
                } => matches!(value, GraphValue::Edge(_)) || value.is_null(),
                _ => false,
            };
            if !valid {
                return Ok(None);
            }
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut values = Vec::new();
        for value in counted.keys() {
            values.push(value.copy_with_control(control)?);
        }
        control(GlaExecutionEvent::Work)?;
        Ok(Some((GraphValueRow::from_owned_values(values), *count)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GqlQueryPolicy;
    use crate::algebra::{GlaDirection, GraphColumn, GraphPatternBuilder, GraphValueOrder};
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
    use std::cmp::Ordering;

    fn definition(
        distinct: bool,
        offset: u64,
        count: Option<u64>,
    ) -> PreparedGraphPattern<GraphValueRow> {
        let mut builder = GraphPatternBuilder::new();
        builder
            .vertex("a")
            .unwrap()
            .vertex("b")
            .unwrap()
            .edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        let pattern = builder
            .prepare_values(
                &[GraphColumn::property(
                    "_fgdb_tuple_count_0",
                    "b",
                    PropertyKeyId(1),
                )],
                offset,
                count,
            )
            .unwrap();
        if distinct {
            pattern
        } else {
            pattern.with_duplicates()
        }
    }

    #[test]
    fn counted_lowering_preserves_original_plan_and_matches_all_distinct_pages() {
        let amounts = [
            CanonicalScalar::Int(7),
            CanonicalScalar::Int(7),
            CanonicalScalar::Null,
            CanonicalScalar::Bool(true),
        ]
        .map(CanonicalPropertyValue::Scalar);
        let edges = [
            (VId(9), RelationId(1), VId(0)),
            (VId(9), RelationId(1), VId(0)),
            (VId(9), RelationId(1), VId(1)),
            (VId(9), RelationId(1), VId(2)),
            (VId(9), RelationId(1), VId(3)),
        ];
        let policy = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
        for distinct in [false, true] {
            for offset in [0, 1, 3, u64::MAX] {
                for count in [None, Some(0), Some(1), Some(3)] {
                    for ordered in [false, true] {
                        let pattern = definition(distinct, offset, count);
                        let pattern = if ordered {
                            pattern
                                .with_order_by(&[
                                    GraphValueOrder::descending(0).with_nulls_first(true)
                                ])
                                .unwrap()
                        } else {
                            pattern
                        };
                        let bytes = pattern.canonical_bytes();
                        let source = pattern.incremental_row_source_definition().unwrap();
                        assert_eq!(pattern.canonical_bytes(), bytes);
                        assert_eq!(
                            pattern.incremental_row_window(),
                            Some((distinct, offset, count))
                        );
                        assert!(source.input_pattern().preserves_duplicates());
                        let mut actual = source
                            .execute_governed(
                                5,
                                [],
                                edges,
                                |_, _| Ok::<_, ()>(true),
                                |vid, _| Ok(Some(&amounts[vid.0 as usize])),
                                policy,
                                || Ok::<_, ()>(()),
                            )
                            .unwrap()
                            .value;
                        let ordering = pattern.incremental_row_ordering().unwrap();
                        actual.sort_by(|a, b| {
                            a.compare_incremental_order(b, &ordering, &mut |_| Ok::<_, ()>(()))
                                .unwrap()
                                .unwrap()
                        });
                        let mut expanded = Vec::new();
                        for row in actual {
                            let (row, n) = pattern
                                .materialize_incremental_values(&row, &mut |_| Ok::<_, ()>(()))
                                .unwrap()
                                .unwrap();
                            let occurrences = if distinct { 1 } else { n };
                            for _ in 0..occurrences {
                                expanded.push(row.clone());
                            }
                        }
                        let selected: Vec<_> = expanded
                            .into_iter()
                            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                            .take(
                                count
                                    .and_then(|n| usize::try_from(n).ok())
                                    .unwrap_or(usize::MAX),
                            )
                            .collect();
                        let expected = pattern
                            .plan()
                            .execute_governed_with_properties(
                                5,
                                [],
                                edges,
                                |_, _| Ok::<_, ()>(true),
                                |vid, _| Ok(Some(&amounts[vid.0 as usize])),
                                policy,
                                || Ok::<_, ()>(()),
                            )
                            .unwrap()
                            .value;
                        assert_eq!(selected, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn row_materialization_checks_schema_and_every_checkpoint_is_retryable() {
        let pattern = definition(false, 0, None);
        let source = pattern.incremental_row_source_definition().unwrap();
        let row = source
            .materialize_incremental_row(
                vec![GraphValue::Scalar(
                    CanonicalScalar::bytes(vec![7; 8192]).unwrap(),
                )],
                vec![GraphAggregateValue::Count(u64::MAX)],
            )
            .unwrap();
        let mut calls = 0;
        let expected = pattern
            .materialize_incremental_values(&row, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(expected.1, u64::MAX);
        assert!(calls > 128);
        for stop in 1..=calls {
            let mut seen = 0;
            assert_eq!(
                pattern.materialize_incremental_values(&row, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(seen, stop);
            assert_eq!(
                pattern
                    .materialize_incremental_values(&row, &mut |_| Ok::<_, usize>(()))
                    .unwrap(),
                Some(expected.clone())
            );
        }
        let zero = source
            .materialize_incremental_row(
                vec![GraphValue::Scalar(CanonicalScalar::Null)],
                vec![GraphAggregateValue::Count(0)],
            )
            .unwrap();
        assert_eq!(
            pattern
                .materialize_incremental_values(&zero, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            None
        );
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let other = builder
            .prepare_values(&[GraphColumn::vertex("id", "n")], 0, None)
            .unwrap();
        assert_eq!(
            other
                .materialize_incremental_values(&row, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            None
        );
        let a = source
            .materialize_incremental_row(
                vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
                vec![GraphAggregateValue::Count(1)],
            )
            .unwrap();
        let b = source
            .materialize_incremental_row(
                vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
                vec![GraphAggregateValue::Count(9)],
            )
            .unwrap();
        assert_eq!(
            a.compare_incremental_order(
                &b,
                &pattern.incremental_row_ordering().unwrap(),
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap(),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn unsupported_hidden_outputs_and_maximum_width_fail_closed() {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let hidden = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("a", "n"),
                    GraphColumn::property("hidden", "n", PropertyKeyId(1)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates()
            .with_visible_columns(1);
        assert!(hidden.incremental_row_source_definition().is_none());
        let names: Vec<_> = (0..MAX_PATTERN_VERTICES).map(|n| format!("c{n}")).collect();
        let columns: Vec<_> = names
            .iter()
            .map(|name| GraphColumn::vertex(name, "n"))
            .collect();
        let full = builder.prepare_values(&columns, 0, None).unwrap();
        assert!(full.incremental_row_source_definition().is_none());
        let supported = builder
            .prepare_values(&columns[..MAX_PATTERN_VERTICES - 1], 0, None)
            .unwrap();
        assert!(supported.incremental_row_source_definition().is_some());
    }

    #[test]
    fn edge_field_tuples_keep_scalar_domains_and_checked_multiplicity() {
        use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
        let text = PreparedGraphText::prepare(
            "MATCH (a)-[r:R]->(b) RETURN r.p AS p",
            |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            },
        )
        .unwrap();
        let pattern = text.bind_parameters(&GqlParameters::new()).unwrap();
        let source = pattern.incremental_row_source_definition().unwrap();
        assert!(source.supports_incremental_maintenance_with_having());
        for value in [CanonicalScalar::Int(7), CanonicalScalar::Null] {
            let value = GraphValue::Scalar(value);
            let counted = source
                .materialize_incremental_row(
                    vec![value.clone()],
                    vec![GraphAggregateValue::Count(3)],
                )
                .unwrap();
            let (row, count) = pattern
                .materialize_incremental_values(&counted, &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            assert_eq!(row.values(), &[value]);
            assert_eq!(count, 3);
        }
        assert!(
            source
                .materialize_incremental_row(
                    vec![GraphValue::Vertex(VId(u128::MAX))],
                    vec![GraphAggregateValue::Count(1)],
                )
                .is_none()
        );
    }

    #[test]
    fn edge_identity_tuples_preserve_ids_and_refuse_other_domains() {
        use crate::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
        use fgdb_types::EId;
        let text = PreparedGraphText::prepare(
            "MATCH (a)-[r:R]->(b) RETURN r AS edge,r.p AS p",
            |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
                _ => None,
            },
        )
        .unwrap();
        let pattern = text.bind_parameters(&GqlParameters::new()).unwrap();
        let source = pattern.incremental_row_source_definition().unwrap();
        assert!(source.supports_incremental_maintenance_with_having());
        for id in [0, u128::MAX] {
            let values = vec![
                GraphValue::Edge(EId(id)),
                GraphValue::Scalar(CanonicalScalar::Int(7)),
            ];
            let counted = source
                .materialize_incremental_row(values.clone(), vec![GraphAggregateValue::Count(2)])
                .unwrap();
            let (row, count) = pattern
                .materialize_incremental_values(&counted, &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            assert_eq!(row.values(), values);
            assert_eq!(count, 2);
        }
        for wrong in [
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Int(7)),
        ] {
            assert!(
                source
                    .materialize_incremental_row(
                        vec![wrong, GraphValue::Scalar(CanonicalScalar::Int(7))],
                        vec![GraphAggregateValue::Count(1)],
                    )
                    .is_none()
            );
        }
    }
}
