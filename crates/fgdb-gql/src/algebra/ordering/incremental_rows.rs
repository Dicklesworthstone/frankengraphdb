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
/// input contains all authorized transit vertices. Optional endpoint row sources
/// are joined AFTER closure and BEFORE the final projection; identity-only
/// sources use SEMI, while sources carrying properties use INNER.
#[derive(Clone, Debug)]
pub struct IncrementalClosureDefinition {
    input: crate::PreparedGraphSet,
    endpoint_sources: [Option<crate::PreparedGraphSet>; 2],
    projection: crate::row_projection::RowProjectionSpec,
}

impl IncrementalClosureDefinition {
    #[must_use]
    pub const fn input(&self) -> &crate::PreparedGraphSet {
        &self.input
    }

    #[must_use]
    pub const fn endpoint_sources(&self) -> &[Option<crate::PreparedGraphSet>; 2] {
        &self.endpoint_sources
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
        (self.input, self.endpoint_sources, self.projection)
    }
}

/// A finite endpoint-reachability circuit for repeatable-element WALK.
/// Every hop keeps DISTINCT endpoint support, not complete path occurrences.
/// The host shares the edge source between hop layers and unions exactly the
/// requested inclusive interval before applying endpoint predicates and properties.
#[derive(Clone, Debug)]
pub struct IncrementalBoundedWalkDefinition {
    edges: Option<crate::PreparedGraphSet>,
    lifetimes: Option<crate::PreparedGraphSet>,
    bounds: crate::GraphWalkBounds,
    endpoint_sources: [Option<crate::PreparedGraphSet>; 2],
    projection: crate::row_projection::RowProjectionSpec,
}

impl IncrementalBoundedWalkDefinition {
    #[must_use]
    pub const fn edges(&self) -> Option<&crate::PreparedGraphSet> {
        self.edges.as_ref()
    }

    #[must_use]
    pub const fn lifetimes(&self) -> Option<&crate::PreparedGraphSet> {
        self.lifetimes.as_ref()
    }

    #[must_use]
    pub const fn bounds(&self) -> crate::GraphWalkBounds {
        self.bounds
    }

    #[must_use]
    pub const fn endpoint_sources(&self) -> &[Option<crate::PreparedGraphSet>; 2] {
        &self.endpoint_sources
    }

    #[must_use]
    pub const fn projection(&self) -> &crate::row_projection::RowProjectionSpec {
        &self.projection
    }

    /// Owners beyond the source leaves and endpoint joins: one edge
    /// threshold, a join/projection per later hop, a union per extra selected
    /// hop, and the final output projection. All arithmetic is over an
    /// admitted finite GraphWalkBounds, never the unbounded sentinel.
    #[must_use]
    pub fn derived_nodes(&self) -> u64 {
        let maximum = u64::from(self.bounds.maximum());
        u64::from(self.edges.is_some()) + maximum.saturating_sub(1) * 2 + maximum
            - u64::from(self.bounds.minimum())
            + 1
    }

    /// Exact private registry footprint of this recipe before a host's
    /// optional outer window or capability owner.
    #[must_use]
    pub fn circuit_nodes(&self) -> u64 {
        self.derived_nodes()
            + u64::from(self.edges.is_some())
            + u64::from(self.lifetimes.is_some())
            + self.endpoint_sources.iter().flatten().count() as u64 * 2
    }
}

struct EndpointWalk {
    input: crate::PreparedGraphSet,
    bounds: crate::GraphWalkBounds,
    direction: crate::algebra::GlaDirection,
    search: crate::algebra::GraphWalkSearch,
    different_edges: bool,
    endpoint_sources: [Option<crate::PreparedGraphSet>; 2],
    projection: crate::row_projection::RowProjectionSpec,
}

fn vertex_lifetimes() -> Option<crate::PreparedGraphSet> {
    use crate::algebra::{GraphColumn, GraphPatternBuilder};

    let mut vertices = GraphPatternBuilder::new();
    vertices.vertex("vertex").ok()?;
    Some(
        vertices
            .prepare_values(
                &[
                    GraphColumn::vertex("source", "vertex"),
                    GraphColumn::vertex("destination", "vertex"),
                ],
                0,
                None,
            )
            .ok()?
            .with_duplicates()
            .into(),
    )
}

impl PreparedGraphPattern<GraphValueRow> {
    /// Compile the support of one unbounded TRAIL into a counted edge/lifetime
    /// relation, optional endpoint row sources and a final projection.
    /// The host must apply deletion-safe closure, then the endpoint joins,
    /// then the projection. Endpoint predicates never restrict transit vertices.
    ///
    /// Each endpoint source starts with its vertex identity, followed by its
    /// sorted, unique demanded property keys. Predicate-only sources use SEMI
    /// joins. Property-bearing sources use INNER joins that append their entire
    /// row, including the identity, in endpoint order. The projection schema
    /// describes exactly that layout. Ordinary sources retain missing values
    /// as NULL and preserve stored scalar, list and map values.
    ///
    /// DISTINCT is load-bearing: closure has one row per reachable pair, not
    /// one row per path. Output may retain endpoint identities and properties,
    /// with aliases and repetitions. Unless both identities survive, projected
    /// support is thresholded so deleting one witness cannot retract another.
    ///
    /// The admitted interval is zero-or-more or one-or-more hops, with ordinary
    /// endpoint predicates and no captures, edge predicates, order or page.
    /// Zero-hop support adds one self pair per visible vertex, including
    /// isolates, through an ordinary UNION ALL source. Those lifetime rows and
    /// real self-loops retain independent counted support, so deleting one does
    /// not retract the other. An undirected one-or-more trail cannot use this
    /// recipe: traversing one physical edge in both directions does not form a
    /// legal cycle. Zero-or-more support is sound in every direction because
    /// each distinct reachable pair has a simple path and every visible vertex
    /// has an empty path. Ordinary selection retains relation unions, wildcard
    /// types and source authorization before closure sees topology/lifetimes.
    #[must_use]
    pub fn incremental_closure_definition(&self) -> Option<IncrementalClosureDefinition> {
        use crate::algebra::{GlaDirection, GraphWalkSearch};
        use crate::{GraphSetOperation, GraphSetQuantifier};

        let path = self.incremental_endpoint_walk()?;
        if path.bounds.minimum() > 1
            || path.bounds.is_bounded()
            || (path.direction == GlaDirection::Undirected && path.bounds.minimum() != 0)
            || !matches!(path.search, GraphWalkSearch::Trail | GraphWalkSearch::All)
            || (path.search == GraphWalkSearch::All && !path.different_edges)
        {
            return None;
        }
        let input = if path.bounds.minimum() == 0 {
            path.input
                .combine(
                    GraphSetOperation::Union,
                    GraphSetQuantifier::All,
                    vertex_lifetimes()?,
                )
                .ok()?
        } else {
            path.input
        };
        Some(IncrementalClosureDefinition {
            input,
            endpoint_sources: path.endpoint_sources,
            projection: path.projection,
        })
    }

    /// Compile finite WALK reachability using only existing maintained joins,
    /// DISTINCT projections and unions. Each layer represents the existence
    /// of a walk of exactly that length. Intermediate vertices remain wholly
    /// unrestricted by endpoint predicates, and zero hops include visible isolates.
    ///
    /// Reusing an edge is legal only for repeatable-element WALK. Bounded
    /// TRAIL, SIMPLE, ACYCLIC, shortest selectors, path capture/bags, edge or
    /// cross-endpoint predicates, and pages keep their refusal; their semantics cannot be
    /// recovered from endpoint support. The entire recipe must fit the native
    /// relational depth budget before any source is read or owner registered.
    #[must_use]
    pub fn incremental_bounded_walk_definition(&self) -> Option<IncrementalBoundedWalkDefinition> {
        let path = self.incremental_endpoint_walk()?;
        if !path.bounds.is_bounded()
            || path.bounds.minimum() > path.bounds.maximum()
            || path.search != crate::algebra::GraphWalkSearch::All
            || path.different_edges
        {
            return None;
        }
        let definition = IncrementalBoundedWalkDefinition {
            edges: (path.bounds.maximum() != 0).then_some(path.input),
            lifetimes: if path.bounds.minimum() == 0 {
                Some(vertex_lifetimes()?)
            } else {
                None
            },
            bounds: path.bounds,
            endpoint_sources: path.endpoint_sources,
            projection: path.projection,
        };
        (definition.circuit_nodes() <= crate::MAX_GRAPH_SET_DEPTH as u64).then_some(definition)
    }

    /// One source/selection/projection proof shared by finite and unbounded
    /// reachability. The caller separately proves its path-mode derivative.
    /// Endpoint row sources retain their predicates and demanded properties;
    /// the full incidence source never acquires an endpoint restriction.
    fn incremental_endpoint_walk(&self) -> Option<EndpointWalk> {
        use crate::algebra::{GraphColumn, GraphPatternBuilder, VertexPredicate};
        use crate::row_join::{RowJoinKind, RowJoinSpec};
        use crate::{
            GraphSetColumnType, GraphSetProjection, GraphSetQuantifier, GraphSetValue,
            PreparedGraphSet,
        };

        let output = self.incremental_rows_output()?;
        if !output.distinct
            || !output.order.is_empty()
            || output.offset != 0
            || output.count.is_some()
        {
            return None;
        }
        let mut identities = [false, false];
        let mut property_keys = [Vec::new(), Vec::new()];
        for column in output.columns {
            match column {
                ValueProjection::Vertex { slot } if slot.ordinal() < 2 => {
                    identities[usize::from(slot.ordinal() == 1)] = true;
                }
                ValueProjection::Property { slot, key } if slot.ordinal() < 2 => {
                    property_keys[usize::from(slot.ordinal() == 1)].push(*key);
                }
                _ => return None,
            }
        }
        for keys in &mut property_keys {
            keys.sort_unstable();
            keys.dedup();
        }
        let [GlaOperator::ScanVertices, remainder @ ..] =
            &self.plan().operators()[..output.projection]
        else {
            return None;
        };
        let mut expansion = None;
        let mut different_edges = false;
        let mut endpoint_predicates = [Vec::new(), Vec::new()];
        for operator in remainder {
            match operator {
                GlaOperator::VarLengthExpand { .. } if expansion.is_none() => {
                    expansion = Some(operator);
                }
                GlaOperator::Select { slot, predicates }
                    if slot.ordinal() < 2 && (slot.ordinal() == 0 || expansion.is_some()) =>
                {
                    for predicate in predicates {
                        match predicate {
                            VertexPredicate::HasLabel(_)
                            | VertexPredicate::IntegerProperty { .. }
                            | VertexPredicate::ScalarProperty { .. }
                            | VertexPredicate::PropertyNull { .. } => {
                                endpoint_predicates[usize::from(slot.ordinal() == 1)]
                                    .push(predicate.clone());
                            }
                        }
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
        if source.ordinal() != 0 || !edge_predicates.is_empty() {
            return None;
        }

        let mut pattern = GraphPatternBuilder::new();
        pattern.vertex("source").ok()?.vertex("destination").ok()?;
        pattern
            .edge("source", *relation, *direction, "destination")
            .ok()?;
        let input: PreparedGraphSet = pattern
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
        let mut endpoint_sources = [None, None];
        let mut input_types = vec![GraphSetColumnType::Vertex; 2];
        let mut property_offsets = [None, None];
        for (endpoint, predicates) in endpoint_predicates.into_iter().enumerate() {
            let keys = &property_keys[endpoint];
            if predicates.is_empty() && keys.is_empty() {
                continue;
            }
            let mut vertices = GraphPatternBuilder::new();
            vertices.vertex("vertex").ok()?;
            for predicate in predicates {
                vertices.filter("vertex", predicate).ok()?;
            }
            let names: Vec<_> = (0..keys.len())
                .map(|index| format!("property_{index}"))
                .collect();
            let mut columns = vec![GraphColumn::vertex("vertex", "vertex")];
            columns.extend(
                names
                    .iter()
                    .zip(keys)
                    .map(|(name, key)| GraphColumn::property(name, "vertex", *key)),
            );
            // The ordinary counted source needs one private COUNT column.
            if columns.len() >= MAX_PATTERN_VERTICES {
                return None;
            }
            let source: PreparedGraphSet = vertices
                .prepare_values(&columns, 0, None)
                .ok()?
                .with_duplicates()
                .into();
            // Match the host's exact join admission, including the combined
            // input-width bound of a predicate-only SEMI join after enrichment.
            let join = RowJoinSpec::new(&input_types, source.column_types(), &[(endpoint, 0)])
                .ok()?
                .with_kind(if keys.is_empty() {
                    RowJoinKind::Semi
                } else {
                    RowJoinKind::Inner
                });
            if !keys.is_empty() {
                property_offsets[endpoint] = Some(input_types.len() + 1);
            }
            input_types = join.column_types().collect();
            endpoint_sources[endpoint] = Some(source);
        }
        // Keeping both endpoint identities is injective, even with repeated
        // columns and property payloads. Otherwise values can collide.
        let quantifier = if identities.into_iter().all(|present| present) {
            GraphSetQuantifier::All
        } else {
            GraphSetQuantifier::Distinct
        };
        let columns = self
            .columns()
            .iter()
            .zip(output.columns)
            .map(|(name, column)| {
                let column = match column {
                    ValueProjection::Vertex { slot } => usize::from(slot.ordinal() == 1),
                    ValueProjection::Property { slot, key } => {
                        let endpoint = usize::from(slot.ordinal() == 1);
                        property_offsets[endpoint]?
                            + property_keys[endpoint].binary_search(key).ok()?
                    }
                    _ => return None,
                };
                Some(GraphSetProjection::new(
                    name.clone(),
                    GraphSetValue::Column(column),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let projection =
            crate::row_projection::RowProjectionSpec::new(input_types, columns, quantifier).ok()?;
        Some(EndpointWalk {
            input,
            bounds: *bounds,
            direction: *direction,
            search: *search,
            different_edges,
            endpoint_sources,
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
    /// Up to MAX_PATTERN_VERTICES - 1 stored-property/vertex/edge columns are admitted.
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
                    crate::GraphSetColumnType::Stored.accepts(value) && value.validate_bounds()
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
    #[test]
    fn stored_property_tuples_preserve_nested_values_and_reject_graph_entities() {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let pattern = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("value", "n", PropertyKeyId(1)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let source = pattern.incremental_row_source_definition().unwrap();
        let nested = GraphValue::map(vec![(
            "items".into(),
            GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::Int(7)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]
                .into(),
            ),
        )])
        .unwrap();
        for value in [
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(9)),
            GraphValue::List(vec![nested.clone()].into()),
            nested,
        ] {
            let values = vec![GraphValue::Vertex(VId(1)), value];
            assert_eq!(
                source
                    .evaluate_incremental_input(values.clone(), &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                    .unwrap()
                    .values(),
                values
            );
            let counted = source
                .materialize_incremental_row(values.clone(), vec![GraphAggregateValue::Count(3)])
                .unwrap();
            let (row, count) = pattern
                .materialize_incremental_values(&counted, &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .unwrap();
            assert_eq!(row.values(), values);
            assert_eq!(count, 3);
        }
        let mut too_deep = GraphValue::Scalar(CanonicalScalar::Null);
        for _ in 0..=GraphValue::MAX_LIST_DEPTH {
            too_deep = GraphValue::List(vec![too_deep].into());
        }
        for invalid in [
            GraphValue::Vertex(VId(1)),
            GraphValue::List(vec![GraphValue::Vertex(VId(1))].into()),
            GraphValue::map(vec![(
                "entity".into(),
                GraphValue::Edge(fgdb_types::EId(1)),
            )])
            .unwrap(),
            too_deep,
        ] {
            let values = vec![GraphValue::Vertex(VId(1)), invalid];
            assert!(
                source
                    .evaluate_incremental_input(values.clone(), &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                    .is_none()
            );
            assert!(
                source
                    .materialize_incremental_row(
                        values.clone(),
                        vec![GraphAggregateValue::Count(1)],
                    )
                    .is_none()
            );
            // A physical reducer can construct a row directly. The ordinary
            // output adapter must independently reject an invalid Stored cell.
            let counted =
                GraphAggregateRow::from_group_values(values, vec![GraphAggregateValue::Count(1)]);
            assert!(
                pattern
                    .materialize_incremental_values(&counted, &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                    .is_none()
            );
        }
        let payload = GraphValue::List(
            vec![GraphValue::Scalar(
                CanonicalScalar::bytes(vec![7; 4096]).unwrap(),
            )]
            .into(),
        );
        let counted = source
            .materialize_incremental_row(
                vec![GraphValue::Vertex(VId(1)), payload],
                vec![GraphAggregateValue::Count(2)],
            )
            .unwrap();
        let mut calls = 0;
        let expected = pattern
            .materialize_incremental_values(&counted, &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        assert!(calls > 32);
        for stop in 1..=calls {
            let mut seen = 0;
            assert_eq!(
                pattern.materialize_incremental_values(&counted, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                }),
                Err(stop)
            );
            assert_eq!(
                pattern
                    .materialize_incremental_values(&counted, &mut |_| Ok::<_, usize>(()))
                    .unwrap(),
                expected
            );
        }
    }
}
