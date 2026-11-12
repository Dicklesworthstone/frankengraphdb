//! Compile bound set expressions into the existing dependency-ordered registry.
//! A private append suffix is protected by one guard; no handle, callback that
//! can borrow the database, await, or commit can expose a partially built circuit.
//! Rebuild prepares a complete replacement suffix before swapping any old node.

use super::*;
use fgdb_delta_types::{LabelId, SchemaSelection, zset::set::SetOperation};
use fgdb_gql::PreparedGraphSetAggregate;
use fgdb_gql::algebra::{
    EdgeRelation, GlaDirection, GraphColumn, GraphPatternBuilder, MAX_EDGE_RELATION_ALTERNATIVES,
    VertexPredicate,
};
use fgdb_gql::row_aggregate::definition::GroupDefinition;
use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
use fgdb_gql::row_projection::RowProjectionSpec;
use fgdb_gql::row_window::RowWindowSpec;
use fgdb_gql::{
    GraphSetColumnType, GraphSetExecutionError, GraphSetOperation, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, PreparedGraphSet,
};
use fgdb_prism::{
    FnxAlgorithm, FnxOutput, FnxWeightSpec, FoundationAlgorithm, ParallelEdgePolicy, SelfLoopPolicy,
};

pub(super) enum AnalyticTopology {
    Pattern(Box<PreparedGraphSet>),
    // Compile each source once, then share the selected vertex relation
    // between both endpoint semijoins and the isolate lifetime projection.
    Union {
        vertices: Box<PreparedGraphSet>,
        edges: Box<PreparedGraphSet>,
    },
}

pub(super) enum AnalyticSource {
    Components(components::ComponentRelation),
    SelectedComponents {
        input: AnalyticTopology,
        strong: bool,
    },
    CoreNumbers(Option<RelationId>),
    SelectedCoreNumbers {
        input: AnalyticTopology,
    },
    TriangleCounts(Option<RelationId>, triangle_counts::Output),
    SelectedTriangleCounts {
        input: AnalyticTopology,
        output: triangle_counts::Output,
    },
}

fn component_source(
    relations: SchemaSelection<RelationId>,
    labels: SchemaSelection<LabelId>,
    strong: bool,
) -> Result<AnalyticSource, StandingQueryError> {
    if labels.is_all() && (relations.is_all() || relations.single().is_some()) {
        return Ok(AnalyticSource::Components(
            match (strong, relations.single()) {
                (false, Some(relation)) => components::ComponentRelation::Weak(relation),
                (true, Some(relation)) => components::ComponentRelation::Strong(relation),
                (false, None) => components::ComponentRelation::WeakAll,
                (true, None) => components::ComponentRelation::StrongAll,
            },
        ));
    }
    Ok(AnalyticSource::SelectedComponents {
        input: analytic_topology(relations, labels)?,
        strong,
    })
}

fn analytic_topology(
    relations: SchemaSelection<RelationId>,
    labels: SchemaSelection<LabelId>,
) -> Result<AnalyticTopology, StandingQueryError> {
    if let Some(label) = labels.single()
        && (relations.is_all() || relations.single().is_some())
    {
        // Preserve the existing compact source and its admission footprint.
        return Ok(AnalyticTopology::Pattern(selected_topology(
            relations.single(),
            label,
        )?));
    }
    let vertices = match labels.members() {
        None => topology_vertices(None)?,
        Some(labels) => {
            let (first, rest) = labels
                .split_first()
                .ok_or(StandingQueryError::Unsupported)?;
            let mut vertices = topology_vertices(Some(*first))?;
            for &label in rest {
                // ANY label is one vertex lifetime, even if several selected
                // labels overlap or change together on a committed tick.
                vertices = vertices
                    .combine(
                        GraphSetOperation::Union,
                        GraphSetQuantifier::Distinct,
                        topology_vertices(Some(label))?,
                    )
                    .map_err(StandingQueryError::SetSchema)?;
            }
            vertices
        }
    };
    let edges = match relations.members() {
        None => topology_edges(EdgeRelation::Any)?,
        Some(relations) => {
            // A graph atom admits eight types; the Prism selector admits
            // sixteen. Disjoint canonical chunks preserve every edge's
            // counted support without changing either public bound.
            let mut chunks = relations.chunks(MAX_EDGE_RELATION_ALTERNATIVES);
            let first = chunks.next().ok_or(StandingQueryError::Unsupported)?;
            let mut edges = topology_edges(
                EdgeRelation::alternation(first).ok_or(StandingQueryError::Unsupported)?,
            )?;
            for chunk in chunks {
                edges = edges
                    .combine(
                        GraphSetOperation::Union,
                        GraphSetQuantifier::All,
                        topology_edges(
                            EdgeRelation::alternation(chunk)
                                .ok_or(StandingQueryError::Unsupported)?,
                        )?,
                    )
                    .map_err(StandingQueryError::SetSchema)?;
            }
            edges
        }
    };
    Ok(AnalyticTopology::Union {
        vertices: Box::new(vertices),
        edges: Box::new(edges),
    })
}

fn topology_vertices(label: Option<LabelId>) -> Result<PreparedGraphSet, StandingQueryError> {
    let mut pattern = GraphPatternBuilder::new();
    pattern
        .vertex("v")
        .map_err(|_| StandingQueryError::Unsupported)?;
    if let Some(label) = label {
        pattern
            .filter("v", VertexPredicate::HasLabel(label))
            .map_err(|_| StandingQueryError::Unsupported)?;
    }
    Ok(pattern
        .prepare_values(&[GraphColumn::vertex("v", "v")], 0, None)
        .map_err(|_| StandingQueryError::Unsupported)?
        .with_duplicates()
        .into())
}

fn topology_edges(relation: EdgeRelation) -> Result<PreparedGraphSet, StandingQueryError> {
    let mut pattern = GraphPatternBuilder::new();
    pattern
        .vertex("a")
        .and_then(|pattern| pattern.vertex("b"))
        .and_then(|pattern| pattern.edge("a", relation, GlaDirection::Forward, "b"))
        .map_err(|_| StandingQueryError::Unsupported)?;
    Ok(pattern
        .prepare_values(
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")],
            0,
            None,
        )
        .map_err(|_| StandingQueryError::Unsupported)?
        .with_duplicates()
        .into())
}

fn selected_topology(
    relation: Option<RelationId>,
    label: LabelId,
) -> Result<Box<PreparedGraphSet>, StandingQueryError> {
    // Compile the induced topology through ordinary maintained GLA sources.
    // A selected vertex contributes a self-pair even when it has no edge;
    // this supplies its lifetime without adding adjacency to another vertex.
    // Edge rows retain counted parallel support and require BOTH labels.
    let mut pattern = GraphPatternBuilder::new();
    pattern
        .vertex("a")
        .and_then(|pattern| pattern.filter("a", VertexPredicate::HasLabel(label)))
        .map_err(|_| StandingQueryError::Unsupported)?;
    let vertices = pattern
        .prepare_values(
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "a")],
            0,
            None,
        )
        .map_err(|_| StandingQueryError::Unsupported)?
        .with_duplicates();
    pattern
        .vertex("b")
        .and_then(|pattern| pattern.filter("b", VertexPredicate::HasLabel(label)))
        .and_then(|pattern| {
            pattern.edge(
                "a",
                relation.map_or(EdgeRelation::Any, EdgeRelation::One),
                GlaDirection::Forward,
                "b",
            )
        })
        .map_err(|_| StandingQueryError::Unsupported)?;
    let edges = pattern
        .prepare_values(
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")],
            0,
            None,
        )
        .map_err(|_| StandingQueryError::Unsupported)?
        .with_duplicates();
    let input = PreparedGraphSet::from(vertices)
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            PreparedGraphSet::from(edges),
        )
        .map_err(StandingQueryError::SetSchema)?;
    Ok(Box::new(input))
}

fn core_source(
    relations: SchemaSelection<RelationId>,
    labels: SchemaSelection<LabelId>,
) -> Result<AnalyticSource, StandingQueryError> {
    if labels.is_all() && (relations.is_all() || relations.single().is_some()) {
        return Ok(AnalyticSource::CoreNumbers(relations.single()));
    }
    Ok(AnalyticSource::SelectedCoreNumbers {
        input: analytic_topology(relations, labels)?,
    })
}

fn triangle_source(
    relations: SchemaSelection<RelationId>,
    labels: SchemaSelection<LabelId>,
    output: triangle_counts::Output,
) -> Result<AnalyticSource, StandingQueryError> {
    if labels.is_all() && (relations.is_all() || relations.single().is_some()) {
        return Ok(AnalyticSource::TriangleCounts(relations.single(), output));
    }
    Ok(AnalyticSource::SelectedTriangleCounts {
        input: analytic_topology(relations, labels)?,
        output,
    })
}

/// The compiler and subscription footprint share one source-free admission.
/// Only the complete, explicit topology contract has an existing derivative;
/// future duplicates or self-loops cannot silently defeat a Reject policy.
pub(super) fn analytic_procedure(
    query: &PreparedGraphSet,
) -> Result<Option<(AnalyticSource, RowProjectionSpec)>, StandingQueryError> {
    let Some(call) = query.incremental_procedure() else {
        return Ok(None);
    };
    let bound = crate::query::bind_procedure(call, &[]).map_err(|error| {
        prepare_error(QueryError::Set(
            error.map_source(GraphSetExecutionError::Source),
        ))
    })?;
    // Read the immutable input recipe only; no source is pinned or executed
    // at this placeholder cut. The analytic owner admits the live snapshot.
    let options = bound.options_at(CommitSeq(0));
    let relations = options.selection.relations;
    let labels = options.selection.vertex_labels;
    if options.selection.weight != FnxWeightSpec::Unit
        || options.projection.parallel_edges != ParallelEdgePolicy::CollapseUnit
        || options.projection.self_loops == SelfLoopPolicy::Reject
    {
        return Err(StandingQueryError::Unsupported);
    }
    if !labels.is_all()
        && !matches!(
            bound.algorithm(),
            FnxAlgorithm::ConnectedComponents
                | FnxAlgorithm::WeaklyConnectedComponents
                | FnxAlgorithm::StronglyConnectedComponents
                | FnxAlgorithm::Foundation(FoundationAlgorithm::CoreNumber)
                | FnxAlgorithm::Triangles
                | FnxAlgorithm::ClusteringCoefficient
        )
    {
        return Err(StandingQueryError::Unsupported);
    }
    // The shared binder already enforced the registered graph-kind law.
    // Reversal and retaining self-loops preserve weak and strong membership;
    // neither changes reachability between distinct vertices.
    let (source, value_type, value_field) = match bound.algorithm() {
        FnxAlgorithm::ConnectedComponents | FnxAlgorithm::WeaklyConnectedComponents => (
            component_source(relations, labels, false)?,
            GraphSetColumnType::Vertex,
            FnxOutput::Component,
        ),
        FnxAlgorithm::StronglyConnectedComponents => (
            component_source(relations, labels, true)?,
            GraphSetColumnType::Vertex,
            FnxOutput::Component,
        ),
        FnxAlgorithm::Foundation(FoundationAlgorithm::CoreNumber)
            if options.projection.self_loops == SelfLoopPolicy::Drop =>
        {
            (
                core_source(relations, labels)?,
                GraphSetColumnType::Scalar,
                FnxOutput::Core,
            )
        }
        FnxAlgorithm::Triangles if options.projection.self_loops == SelfLoopPolicy::Drop => (
            triangle_source(relations, labels, triangle_counts::Output::Triangles)?,
            GraphSetColumnType::Scalar,
            FnxOutput::Triangles,
        ),
        FnxAlgorithm::ClusteringCoefficient => (
            triangle_source(relations, labels, triangle_counts::Output::Clustering)?,
            GraphSetColumnType::Scalar,
            FnxOutput::Score,
        ),
        _ => return Err(StandingQueryError::Unsupported),
    };
    let mut projection = Vec::with_capacity(bound.outputs().len());
    for (column, name) in bound.outputs().iter().zip(query.columns()) {
        let input = match column.field {
            FnxOutput::Vertex => 0,
            field if field == value_field => 1,
            _ => return Err(StandingQueryError::Unsupported),
        };
        projection.push(GraphSetProjection::new(
            name.clone(),
            GraphSetValue::Column(input),
        ));
    }
    let output = RowProjectionSpec::new(
        vec![GraphSetColumnType::Vertex, value_type],
        projection,
        GraphSetQuantifier::All,
    )
    .and_then(|spec| spec.widen_output_types(query.column_types().to_vec()))
    .map_err(StandingQueryError::ProjectionSchema)?;
    Ok(Some((source, output)))
}

struct Staging<'a, V: Vfs + Clone> {
    database: &'a mut Database<V>,
    first: usize,
    accepted: bool,
    group_input: bool,
    private_inputs: Vec<bool>,
}
impl<'a, V: Vfs + Clone> Staging<'a, V> {
    fn new(database: &'a mut Database<V>) -> Self {
        Self {
            first: database.standing_queries.len(),
            database,
            accepted: false,
            group_input: false,
            private_inputs: Vec::new(),
        }
    }
    fn append(&mut self, query: StandingQuery) -> usize {
        self.append_with_role(query, self.group_input)
    }
    fn append_private(&mut self, query: StandingQuery) -> usize {
        self.append_with_role(query, true)
    }
    fn append_with_role(&mut self, query: StandingQuery, private_input: bool) -> usize {
        // Exactly one bounded role entry per compiled node. The same suffix
        // guard owns both allocations until the complete handle is accepted.
        self.private_inputs.push(private_input);
        self.database.store_standing_query(query).index
    }

    fn compile_group_input(
        &mut self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        let previous = self.group_input;
        self.group_input = true;
        let result = self.compile(cx, query, policy, checkpoint);
        self.group_input = previous;
        result
    }

    fn compile_topology(
        &mut self,
        cx: &QueryCx,
        topology: &AnalyticTopology,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        // Topology rows are private analytic input, including dense parallel
        // edges. Every source/work/scratch ceiling still applies; only the
        // final analytic/YIELD owners spend the delivered-row allowance.
        let policy = group::input_policy(policy);
        match topology {
            AnalyticTopology::Pattern(input) => {
                self.compile_group_input(cx, input, policy, checkpoint)
            }
            AnalyticTopology::Union { vertices, edges } => {
                let vertices = self.compile_group_input(cx, vertices, policy, checkpoint)?;
                let mut edges = self.compile_group_input(cx, edges, policy, checkpoint)?;
                for endpoint in [0, 1] {
                    let spec = RowJoinSpec::new(
                        &[GraphSetColumnType::Vertex, GraphSetColumnType::Vertex],
                        &[GraphSetColumnType::Vertex],
                        &[(endpoint, 0)],
                    )
                    .map_err(StandingQueryError::JoinSchema)?
                    .with_kind(RowJoinKind::Semi);
                    let state = self.database.prepare_standing_join_spec(
                        cx,
                        [edges, vertices],
                        &spec,
                        policy,
                        self.database.standing_queries.len(),
                    )?;
                    edges = self.append_private(StandingQuery::Join(Box::new(state)));
                    checkpoint()?;
                }
                let spec = RowProjectionSpec::new(
                    vec![GraphSetColumnType::Vertex],
                    vec![
                        GraphSetProjection::new("a", GraphSetValue::Column(0)),
                        GraphSetProjection::new("b", GraphSetValue::Column(0)),
                    ],
                    GraphSetQuantifier::All,
                )
                .map_err(StandingQueryError::ProjectionSchema)?;
                let state = self.database.prepare_standing_projection(
                    cx,
                    vertices,
                    spec,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                let lifetimes = self.append_private(StandingQuery::Projection(Box::new(state)));
                checkpoint()?;
                // Positional union retains the lifetime branch's a,b schema.
                // Semijoins preserve exact left multiplicity; a label overlap
                // never multiplies edge support, and isolated vertices remain.
                let state = self.database.prepare_standing_set(
                    cx,
                    [lifetimes, edges],
                    SetOperation::UnionAll,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                let input = self.append_private(StandingQuery::Set(Box::new(state)));
                checkpoint()?;
                Ok(input)
            }
        }
    }

    // Internal consumers need the selected bag; native delivery also needs
    // inherited rank after scopes/filters. Restore that rank with an ordinary
    // compressed window only when a finite occurrence bound is proved.
    fn compile_root(
        &mut self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        checkpoint()?;
        // A source-free root has an exact, immutable sequence. Freeze the
        // complete subtree before any rank/page decomposition; its positional
        // selection never needs a dynamic canonical-order approximation.
        if query.operand_count() == 0 {
            return self.compile(cx, query, policy, checkpoint);
        }
        // Keep the existing refusal for an implicitly ordered positional root
        // mixed with pages. Explicit/proved ordering can safely consume those
        // bags; every internal page must independently prove its own rank.
        if query.incremental_result_order().is_none()
            && !query.incremental_window_sequence_compatible()
        {
            return Err(StandingQueryError::Unsupported);
        }
        let index = self.compile(cx, query, policy, checkpoint)?;
        let Some((order, Some(bound))) = query.incremental_result_order() else {
            return Ok(index);
        };
        if order.is_empty()
            || matches!(
                &self.database.standing_queries[index],
                StandingQuery::Window(_)
            )
        {
            return Ok(index);
        }
        let spec = RowWindowSpec::new(
            query.column_types().to_vec(),
            order.to_vec(),
            GraphSetQuantifier::All,
            0,
            bound,
        )
        .map_err(StandingQueryError::WindowSchema)?;
        let query = self.database.prepare_standing_window(
            cx,
            index,
            spec,
            policy,
            self.database.standing_queries.len(),
        )?;
        let index = self.append(StandingQuery::Window(Box::new(query)));
        checkpoint()?;
        Ok(index)
    }
    #[allow(clippy::too_many_arguments)]
    fn compile_count_recipe(
        &mut self,
        cx: &QueryCx,
        left_query: &PreparedGraphSet,
        right_query: &PreparedGraphSet,
        restrict: &RowJoinSpec,
        group: PreparedGraphSetAggregate,
        group_columns: &[String],
        group_types: &[GraphSetColumnType],
        join: &RowJoinSpec,
        output: Vec<RowProjectionSpec>,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        let left = self.compile(cx, left_query, policy, checkpoint)?;
        // Count witnesses are private aggregate input. Retain every
        // source/work/scratch ceiling without spending a final-row
        // allowance on each matching relationship occurrence.
        let input_policy = group::input_policy(policy);
        let right = self.compile_group_input(cx, right_query, input_policy, checkpoint)?;
        let state = self.database.prepare_standing_join_spec(
            cx,
            [right, left],
            restrict,
            input_policy,
            self.database.standing_queries.len(),
        )?;
        let active = self.append_private(StandingQuery::Join(Box::new(state)));
        checkpoint()?;
        // Group only keys needed by a current outer row. Unrequested
        // groups must not introduce a conditional Int64 count overflow.
        let state = self.database.prepare_standing_group_rows(
            cx,
            active,
            group,
            input_policy,
            self.database.standing_queries.len(),
            group_columns,
            group_types,
        )?;
        let grouped = self.append_private(StandingQuery::Group(Box::new(state)));
        checkpoint()?;
        let state = self.database.prepare_standing_join_spec(
            cx,
            [left, grouped],
            join,
            policy,
            self.database.standing_queries.len(),
        )?;
        let mut input = self.append(StandingQuery::Join(Box::new(state)));
        checkpoint()?;
        for spec in output {
            let state = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            input = self.append(StandingQuery::Projection(Box::new(state)));
            checkpoint()?;
        }
        Ok(input)
    }
    fn compile(
        &mut self,
        cx: &QueryCx,
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
        checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
    ) -> Result<usize, StandingQueryError> {
        checkpoint()?;
        let index = if query.operand_count() == 0 {
            let state = self
                .database
                .prepare_standing_constant(cx, query.clone(), policy)?;
            self.append(StandingQuery::Constant(Box::new(state)))
        } else if let Some((input, order, offset, count)) = query.incremental_ordered_window() {
            // Peel just this scope. Never move a page across DISTINCT, a
            // filter, or an expression, including when the page is empty.
            let spec = RowWindowSpec::new(
                input.column_types().to_vec(),
                order.to_vec(),
                GraphSetQuantifier::All,
                offset,
                count,
            )
            .map_err(StandingQueryError::WindowSchema)?;
            let input = self.compile(cx, &input, policy, checkpoint)?;
            let state = self.database.prepare_standing_window(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Window(Box::new(state)))
        } else if let Some((input, spec)) = query
            .incremental_window()
            .map_err(StandingQueryError::WindowSchema)?
        {
            let input = self.compile(cx, &input, policy, checkpoint)?;
            let state = self.database.prepare_standing_window(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Window(Box::new(state)))
        } else if let Some(pattern) = query.incremental_pattern() {
            let query = self.database.prepare_registered_rows_with_delivery(
                cx,
                pattern.clone(),
                policy,
                self.group_input,
            )?;
            self.append(query)
        } else if let Some((source, spec)) = analytic_procedure(query)? {
            let state = match source {
                AnalyticSource::Components(relation) => StandingQuery::Components(Box::new(
                    self.database
                        .prepare_standing_components(cx, relation, policy)?,
                )),
                AnalyticSource::SelectedComponents { input, strong } => {
                    // Selected topology is private input, not delivered rows.
                    // A dense component may contain far more edges than the
                    // final vertex membership allowance permits as results.
                    let input = self.compile_topology(cx, &input, policy, checkpoint)?;
                    StandingQuery::Components(Box::new(self.database.prepare_standing_components(
                        cx,
                        components::ComponentRelation::selected_rows(input, strong),
                        policy,
                    )?))
                }
                AnalyticSource::CoreNumbers(relation) => StandingQuery::CoreNumbers(Box::new(
                    self.database
                        .prepare_standing_core_numbers(cx, relation, policy, true)?,
                )),
                AnalyticSource::SelectedCoreNumbers { input } => {
                    let input = self.compile_topology(cx, &input, policy, checkpoint)?;
                    StandingQuery::CoreNumbers(Box::new(
                        self.database.prepare_standing_selected_core_numbers(
                            cx,
                            components::relation::Definition {
                                input,
                                endpoints: [0, 1],
                                width: 2,
                            },
                            policy,
                            true,
                        )?,
                    ))
                }
                AnalyticSource::TriangleCounts(relation, output) => {
                    StandingQuery::TriangleCounts(Box::new(
                        self.database
                            .prepare_standing_triangle_counts(cx, relation, output, policy)?,
                    ))
                }
                AnalyticSource::SelectedTriangleCounts { input, output } => {
                    let input = self.compile_topology(cx, &input, policy, checkpoint)?;
                    StandingQuery::TriangleCounts(Box::new(
                        self.database.prepare_standing_selected_triangle_counts(
                            cx,
                            components::relation::Definition {
                                input,
                                endpoints: [0, 1],
                                width: 2,
                            },
                            output,
                            policy,
                        )?,
                    ))
                }
            };
            let input = self.append(state);
            checkpoint()?;
            // Keep aliases, column selection/order and Any procedure domains
            // in an ordinary maintained projection.
            let state = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(state)))
        } else if let Some(definition) = query.incremental_group() {
            // Keep the entire selected child relation. The existing exact
            // reducer and checked row sink publish one atomic derivative;
            // projection, HAVING-as-filter and top-k stay downstream. As for a
            // terminal group, pre-group occurrences are not result rows.
            let input = self.compile_group_input(
                cx,
                definition.input(),
                group::input_policy(policy),
                checkpoint,
            )?;
            let state = self.database.prepare_standing_group_rows(
                cx,
                input,
                definition.clone(),
                policy,
                self.database.standing_queries.len(),
                query.columns(),
                query.column_types(),
            )?;
            self.append(StandingQuery::Group(Box::new(state)))
        } else if let Some(plan) =
            query.incremental_apply_count_with_control(&mut |_| checkpoint())?
        {
            self.compile_count_recipe(
                cx,
                plan.left,
                plan.right,
                &plan.restrict,
                plan.group,
                &plan.group_columns,
                &plan.group_types,
                &plan.join,
                plan.output,
                policy,
                checkpoint,
            )?
        } else if let Some(plan) =
            query.incremental_collected_count_with_control(&mut |_| checkpoint())?
        {
            self.compile_count_recipe(
                cx,
                &plan.left,
                &plan.right,
                &plan.restrict,
                plan.group,
                &plan.group_columns,
                &plan.group_types,
                &plan.join,
                plan.output,
                policy,
                checkpoint,
            )?
        } else if let Some(plan) =
            query.incremental_apply_distinct_with_control(&mut |_| checkpoint())?
        {
            // Distinctness belongs to one argument tuple, not the outer bag.
            // Share the original source, deduplicate its imported arguments,
            // and restore every outer occurrence only after the body stages.
            let left = self.compile(cx, plan.left, policy, checkpoint)?;
            let input_policy = group::input_policy(policy);
            let right = self.compile_group_input(cx, plan.right, input_policy, checkpoint)?;
            let state = self.database.prepare_standing_projection(
                cx,
                left,
                plan.outer,
                policy,
                self.database.standing_queries.len(),
            )?;
            let outer = self.append(StandingQuery::Projection(Box::new(state)));
            checkpoint()?;
            let state = self.database.prepare_standing_projection(
                cx,
                outer,
                plan.arguments,
                input_policy,
                self.database.standing_queries.len(),
            )?;
            let arguments = self.append_private(StandingQuery::Projection(Box::new(state)));
            checkpoint()?;
            let state = self.database.prepare_standing_join_spec(
                cx,
                [arguments, right],
                &plan.join,
                input_policy,
                self.database.standing_queries.len(),
            )?;
            let mut input = self.append_private(StandingQuery::Join(Box::new(state)));
            checkpoint()?;
            // The GQL recipe carries normalized import keys through each
            // body projection. Its DISTINCT owners retain parallel support
            // privately; only the final restored rows spend result allowance.
            for spec in plan.body {
                let state = self.database.prepare_standing_projection(
                    cx,
                    input,
                    spec,
                    input_policy,
                    self.database.standing_queries.len(),
                )?;
                input = self.append_private(StandingQuery::Projection(Box::new(state)));
                checkpoint()?;
            }
            let state = self.database.prepare_standing_join_spec(
                cx,
                [outer, input],
                &plan.restore,
                policy,
                self.database.standing_queries.len(),
            )?;
            input = self.append(StandingQuery::Join(Box::new(state)));
            checkpoint()?;
            for spec in plan.output {
                let state = self.database.prepare_standing_projection(
                    cx,
                    input,
                    spec,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                input = self.append(StandingQuery::Projection(Box::new(state)));
                checkpoint()?;
            }
            input
        } else if let Some(plan) =
            query.incremental_apply_exists_with_control(&mut |_| checkpoint())?
        {
            // A Boolean witness needs only support, not matching pair rows.
            // Share both complete sources between the complementary joins so
            // duplicate outer occurrences retain their original multiplicity.
            let left = self.compile(cx, plan.left, policy, checkpoint)?;
            let right = self.compile(cx, plan.right, policy, checkpoint)?;
            let mut branches = [0; 2];
            for (branch, (kind, projection)) in [
                (RowJoinKind::Semi, plan.present),
                (RowJoinKind::Anti, plan.absent),
            ]
            .into_iter()
            .enumerate()
            {
                let spec = plan.join.clone().with_kind(kind);
                let state = self.database.prepare_standing_join_spec(
                    cx,
                    [left, right],
                    &spec,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                let input = self.append(StandingQuery::Join(Box::new(state)));
                checkpoint()?;
                let state = self.database.prepare_standing_projection(
                    cx,
                    input,
                    projection,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                branches[branch] = self.append(StandingQuery::Projection(Box::new(state)));
                checkpoint()?;
            }
            let state = self.database.prepare_standing_set(
                cx,
                branches,
                SetOperation::UnionAll,
                policy,
                self.database.standing_queries.len(),
            )?;
            let input = self.append(StandingQuery::Set(Box::new(state)));
            checkpoint()?;
            let state = self.database.prepare_standing_projection(
                cx,
                input,
                plan.output,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(state)))
        } else if let Some(plan) =
            query.incremental_apply_join_with_control(&mut |_| checkpoint())?
        {
            // The GQL owner proves the complete correlated body before any
            // state is appended. Maintain its original children and exact
            // joins/projections; no per-commit subquery replay is introduced.
            let left = self.compile(cx, plan.left, policy, checkpoint)?;
            let mut right = self.compile(cx, plan.right, policy, checkpoint)?;
            if let Some(spec) = plan.right_projection {
                let state = self.database.prepare_standing_projection(
                    cx,
                    right,
                    spec,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                right = self.append(StandingQuery::Projection(Box::new(state)));
                checkpoint()?;
            }
            let state = self.database.prepare_standing_join_spec(
                cx,
                [left, right],
                &plan.join,
                policy,
                self.database.standing_queries.len(),
            )?;
            let mut input = self.append(StandingQuery::Join(Box::new(state)));
            checkpoint()?;
            for spec in plan.output {
                let state = self.database.prepare_standing_projection(
                    cx,
                    input,
                    spec,
                    policy,
                    self.database.standing_queries.len(),
                )?;
                input = self.append(StandingQuery::Projection(Box::new(state)));
                checkpoint()?;
            }
            input
        } else if let Some(input) = query.incremental_scope() {
            return self.compile(cx, input, policy, checkpoint);
        } else if let Some((input, projection, quantifier)) = query.incremental_projection() {
            let spec = RowProjectionSpec::new(
                input.column_types().to_vec(),
                projection.to_vec(),
                quantifier,
            )
            .map_err(StandingQueryError::ProjectionSchema)?;
            let input = self.compile(cx, input, policy, checkpoint)?;
            let query = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(query)))
        } else if let Some((left, right, spec, output)) =
            query.incremental_selected_join_with_control(&mut |_| checkpoint())?
        {
            // Use the SAME complete children, but never retain their rejected
            // Cartesian pairs. ON is part of the join definition from the
            // first baseline through every committed tick and whole rebuild.
            let left = self.compile(cx, left, policy, checkpoint)?;
            let right = self.compile(cx, right, policy, checkpoint)?;
            let state = self.database.prepare_standing_join_spec(
                cx,
                [left, right],
                &spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            let input = self.append(StandingQuery::Join(Box::new(state)));
            checkpoint()?;
            // Internal join names are prefixed. Restore the original schema
            // before any downstream filter/set/group sees the completed bag.
            let state = self.database.prepare_standing_projection(
                cx,
                input,
                output,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(state)))
        } else if let Some((input, predicate)) = query.incremental_filter() {
            // Keep the complete child scope upstream, including DISTINCT and
            // source paging. Identity projection preserves all native cells.
            let spec = RowProjectionSpec::selection(
                input.column_types().to_vec(),
                input.columns().to_vec(),
                predicate,
            )
            .map_err(StandingQueryError::ProjectionSchema)?;
            let input = self.compile(cx, input, policy, checkpoint)?;
            let query = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(query)))
        } else if let Some((input, name, value)) = query.incremental_unwind() {
            let spec = RowProjectionSpec::unwind(
                input.column_types().to_vec(),
                input.columns().to_vec(),
                name.to_owned(),
                value.clone(),
            )
            .map_err(StandingQueryError::ProjectionSchema)?;
            let input = self.compile(cx, input, policy, checkpoint)?;
            let query = self.database.prepare_standing_projection(
                cx,
                input,
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Projection(Box::new(query)))
        } else if let Some((left, right, spec)) = query.incremental_join() {
            // Match the completed child bags, not extracted graph leaves.
            // Keep the checked ON program intact through initialization and
            // the same dependency-ordered committed-delta publication path.
            let left = self.compile(cx, left, policy, checkpoint)?;
            let right = self.compile(cx, right, policy, checkpoint)?;
            let query = self.database.prepare_standing_join_spec(
                cx,
                [left, right],
                spec,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Join(Box::new(query)))
        } else if let Some((left, right)) = query.incremental_cross_join() {
            // Never short-circuit the other operand when one result is empty:
            // its schema, definition and failures are still part of the query.
            let left = self.compile(cx, left, policy, checkpoint)?;
            let right = self.compile(cx, right, policy, checkpoint)?;
            let query = self.database.prepare_standing_join(
                cx,
                [left, right],
                &[],
                RowJoinKind::Inner,
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Join(Box::new(query)))
        } else if let Some((operation, quantifier, left, right)) = query.incremental_binary() {
            let left = self.compile(cx, left, policy, checkpoint)?;
            let right = self.compile(cx, right, policy, checkpoint)?;
            let query = self.database.prepare_standing_set(
                cx,
                [left, right],
                operation_of(operation, quantifier),
                policy,
                self.database.standing_queries.len(),
            )?;
            self.append(StandingQuery::Set(Box::new(query)))
        } else {
            return Err(StandingQueryError::Unsupported);
        };
        checkpoint()?;
        Ok(index)
    }
}
impl<V: Vfs + Clone> Drop for Staging<'_, V> {
    fn drop(&mut self) {
        if !self.accepted {
            // Private append suffix only. Existing entries and their indexes
            // are unchanged on every recoverable preparation failure/unwind.
            self.database.standing_queries.truncate(self.first);
        }
    }
}
fn operation_of(operation: GraphSetOperation, quantifier: GraphSetQuantifier) -> SetOperation {
    match (operation, quantifier) {
        (GraphSetOperation::Union, GraphSetQuantifier::All) => SetOperation::UnionAll,
        (GraphSetOperation::Union, GraphSetQuantifier::Distinct) => SetOperation::UnionDistinct,
        (GraphSetOperation::Intersect, GraphSetQuantifier::All) => SetOperation::IntersectAll,
        (GraphSetOperation::Intersect, GraphSetQuantifier::Distinct) => {
            SetOperation::IntersectDistinct
        }
        (GraphSetOperation::Except, GraphSetQuantifier::All) => SetOperation::ExceptAll,
        (GraphSetOperation::Except, GraphSetQuantifier::Distinct) => SetOperation::ExceptDistinct,
    }
}

pub(super) fn register<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
) -> Result<StandingQueryHandle, StandingQueryError> {
    let mut checkpoint = || cx.checkpoint().map_err(StandingQueryError::Interrupted);
    register_checked(database, cx, query, policy, &mut checkpoint)
}
fn register_checked<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
) -> Result<StandingQueryHandle, StandingQueryError> {
    let mut staged = Staging::new(database);
    let index = staged.compile_root(cx, query, policy, checkpoint)?;
    let layout = Arc::new(Layout::Circuit {
        columns: query.columns().to_vec(),
        first: staged.first,
        private_inputs: std::mem::take(&mut staged.private_inputs),
    });
    let handle = StandingQueryHandle {
        owner: Arc::clone(&staged.database.handle_owner),
        index,
        native: Some(layout),
    };
    checkpoint()?;
    staged.accepted = true;
    Ok(handle)
}

/// One complete row circuit followed by one native aggregate. The immutable
/// aggregate owner retains its real relational input throughout admission.
/// No first-leaf conversion or fallible work occurs after acceptance.
pub(in crate::standing_query) fn register_group<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    definition: &PreparedGraphSetAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    policy: GqlQueryPolicy,
) -> Result<StandingQueryHandle, StandingQueryError> {
    let mut checkpoint = || cx.checkpoint().map_err(StandingQueryError::Interrupted);
    register_group_checked(
        database,
        cx,
        definition,
        columns,
        slots,
        policy,
        &mut checkpoint,
    )
}

fn register_group_checked<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    definition: &PreparedGraphSetAggregate,
    columns: &[String],
    slots: &[GraphAggregateTextSlot],
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
) -> Result<StandingQueryHandle, StandingQueryError> {
    checkpoint()?;
    // Reject unowned rank dependencies before compiling or seeding the input
    // circuit. Empty data or an input-record budget must not decide whether a
    // graph-property rank belongs to this reader-free maintained owner.
    if definition.has_computed_ordering() && definition.complete_groups().is_none() {
        return Err(StandingQueryError::Unsupported);
    }
    if columns.len() != slots.len()
        || columns.len() > fgdb_gql::algebra::MAX_PATTERN_VERTICES
        || slots.iter().any(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => at >= definition.key_columns().len(),
            GraphAggregateTextSlot::Aggregate(at) => at >= definition.aggregate_columns().len(),
        })
    {
        return Err(StandingQueryError::Unsupported);
    }
    let mut staged = Staging::new(database);
    // The nine admitted aggregates consume a bag, not an inherited order trace.
    // Every input-local page must still be admitted by the recursive compiler.
    let input = staged.compile_group_input(
        cx,
        definition.input(),
        group::input_policy(policy),
        checkpoint,
    )?;
    checkpoint()?;
    let state = staged.database.prepare_standing_group(
        cx,
        input,
        definition.clone(),
        policy,
        staged.database.standing_queries.len(),
    )?;
    let index = staged.append(StandingQuery::Group(Box::new(state)));
    let layout = Arc::new(Layout::GroupCircuit {
        columns: columns.to_vec(),
        slots: slots.to_vec(),
        first: staged.first,
        private_inputs: std::mem::take(&mut staged.private_inputs),
    });
    let handle = StandingQueryHandle {
        owner: Arc::clone(&staged.database.handle_owner),
        index,
        native: Some(layout),
    };
    checkpoint()?;
    staged.accepted = true;
    Ok(handle)
}

/// Called only after ordinary owner/health admission. One native handle owns a
/// contiguous, topologically ordered circuit. Original definitions are used;
/// no text, parameters, resolver or hidden handle is needed to repair it.
/// Per-node preparation uses the ordinary source, set and projection engines.
pub(in crate::standing_query) fn rebuild<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    first: usize,
    root: usize,
    private_inputs: &[bool],
    policy: GqlQueryPolicy,
) -> Result<CommitSeq, StandingQueryError> {
    let mut checkpoint = || cx.checkpoint().map_err(StandingQueryError::Interrupted);
    rebuild_checked(
        database,
        cx,
        first,
        root,
        private_inputs,
        policy,
        &mut checkpoint,
    )
}
fn rebuild_checked<V: Vfs + Clone>(
    database: &mut Database<V>,
    cx: &QueryCx,
    first: usize,
    root: usize,
    private_inputs: &[bool],
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), StandingQueryError>,
) -> Result<CommitSeq, StandingQueryError> {
    if first > root || root >= database.standing_queries.len() {
        return Err(StandingQueryError::UnknownHandle);
    }
    if private_inputs.len() != root - first + 1 {
        return Err(StandingQueryError::Unsupported);
    }
    // Reuse the compiler's structural roles, never old numeric allowances or
    // transitive dependency marking. A shared source may feed both a private
    // argument DISTINCT and the public restoration join; its public allowance
    // must remain effective. The immutable layout survives every failed rebuild.
    let mut staged = Staging::new(database);
    let at = staged.database.registration_frontier();
    for old in first..=root {
        checkpoint()?;
        let policy = if private_inputs[old - first] {
            group::input_policy(policy)
        } else {
            policy
        };
        let replacement = match &staged.database.standing_queries[old] {
            StandingQuery::Constant(query) => StandingQuery::Constant(Box::new(
                staged
                    .database
                    .prepare_standing_constant(cx, query.definition.clone(), policy)?,
            )),
            StandingQuery::Components(query) => {
                let mut relation = query.relation;
                if let Some(input) = relation.selected_input() {
                    if input < first || input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    let replacement = staged
                        .first
                        .checked_add(input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                    relation = relation.with_selected_input(replacement);
                }
                StandingQuery::Components(Box::new(
                    staged
                        .database
                        .prepare_standing_components(cx, relation, policy)?,
                ))
            }
            StandingQuery::CoreNumbers(query) if query.has_value_rows() => {
                let replacement = if let Some(mut definition) = query.selected {
                    if definition.input < first || definition.input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    definition.input = staged
                        .first
                        .checked_add(definition.input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                    staged
                        .database
                        .prepare_standing_selected_core_numbers(cx, definition, policy, true)?
                } else {
                    staged.database.prepare_standing_core_numbers(
                        cx,
                        query.relation,
                        policy,
                        true,
                    )?
                };
                StandingQuery::CoreNumbers(Box::new(replacement))
            }
            StandingQuery::TriangleCounts(query) => {
                let replacement = if let Some(mut definition) = query.selected {
                    if definition.input < first || definition.input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    definition.input = staged
                        .first
                        .checked_add(definition.input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                    staged.database.prepare_standing_selected_triangle_counts(
                        cx,
                        definition,
                        query.output(),
                        policy,
                    )?
                } else {
                    staged.database.prepare_standing_triangle_counts(
                        cx,
                        query.relation,
                        query.output(),
                        policy,
                    )?
                };
                StandingQuery::TriangleCounts(Box::new(replacement))
            }
            StandingQuery::Rows { output, .. } => {
                staged.database.prepare_registered_rows_with_delivery(
                    cx,
                    output.definition().clone(),
                    policy,
                    output.is_private_bag(),
                )?
            }
            StandingQuery::Set(query) => {
                let mut inputs = [0; 2];
                for (next, input) in inputs.iter_mut().zip(query.inputs) {
                    if input < first || input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    *next = staged
                        .first
                        .checked_add(input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                }
                StandingQuery::Set(Box::new(staged.database.prepare_standing_set(
                    cx,
                    inputs,
                    query.operation(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Join(query) => {
                let mut inputs = [0; 2];
                for (next, input) in inputs.iter_mut().zip(query.inputs) {
                    if input < first || input >= old {
                        return Err(StandingQueryError::Unsupported);
                    }
                    *next = staged
                        .first
                        .checked_add(input - first)
                        .ok_or(StandingQueryError::Unsupported)?;
                }
                StandingQuery::Join(Box::new(staged.database.prepare_standing_join_spec(
                    cx,
                    inputs,
                    query.spec(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Projection(query) => {
                if query.input < first || query.input >= old {
                    return Err(StandingQueryError::Unsupported);
                }
                let input = staged
                    .first
                    .checked_add(query.input - first)
                    .ok_or(StandingQueryError::Unsupported)?;
                StandingQuery::Projection(Box::new(staged.database.prepare_standing_projection(
                    cx,
                    input,
                    query.spec().clone(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Window(query) => {
                if query.input < first || query.input >= old {
                    return Err(StandingQueryError::Unsupported);
                }
                let input = staged
                    .first
                    .checked_add(query.input - first)
                    .ok_or(StandingQueryError::Unsupported)?;
                StandingQuery::Window(Box::new(staged.database.prepare_standing_window(
                    cx,
                    input,
                    query.spec().clone(),
                    policy,
                    staged.database.standing_queries.len(),
                )?))
            }
            StandingQuery::Group(query) => {
                if query.input < first || query.input >= old {
                    return Err(StandingQueryError::Unsupported);
                }
                let input = staged
                    .first
                    .checked_add(query.input - first)
                    .ok_or(StandingQueryError::Unsupported)?;
                let state = if let Some((columns, types)) = query.row_schema() {
                    staged.database.prepare_standing_group_rows(
                        cx,
                        input,
                        query.definition().clone(),
                        policy,
                        staged.database.standing_queries.len(),
                        columns,
                        types,
                    )?
                } else {
                    staged.database.prepare_standing_group(
                        cx,
                        input,
                        query.definition().clone(),
                        policy,
                        staged.database.standing_queries.len(),
                    )?
                };
                StandingQuery::Group(Box::new(state))
            }
            _ => return Err(StandingQueryError::Unsupported),
        };
        staged.append_with_role(replacement, private_inputs[old - first]);
        checkpoint()?;
    }
    // Rebase private dependency indexes before any old state is replaced.
    // The original circuit is never altered on arithmetic/admission refusal.
    for query in &mut staged.database.standing_queries[staged.first..] {
        checkpoint()?;
        if let StandingQuery::Components(query) = query {
            if let Some(input) = query.relation.selected_input() {
                let input = input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
                query.relation = query.relation.with_selected_input(input);
            }
        } else if let StandingQuery::TriangleCounts(query) = query {
            if let Some(definition) = &mut query.selected {
                definition.input = definition
                    .input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
            }
        } else if let StandingQuery::CoreNumbers(query) = query {
            if let Some(definition) = &mut query.selected {
                definition.input = definition
                    .input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
            }
        } else if let StandingQuery::Set(query) = query {
            for input in &mut query.inputs {
                *input = input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
            }
        } else if let StandingQuery::Join(query) = query {
            for input in &mut query.inputs {
                *input = input
                    .checked_sub(staged.first)
                    .and_then(|offset| first.checked_add(offset))
                    .ok_or(StandingQueryError::Unsupported)?;
            }
        } else if let StandingQuery::Projection(query) = query {
            query.input = query
                .input
                .checked_sub(staged.first)
                .and_then(|offset| first.checked_add(offset))
                .ok_or(StandingQueryError::Unsupported)?;
        } else if let StandingQuery::Window(query) = query {
            query.input = query
                .input
                .checked_sub(staged.first)
                .and_then(|offset| first.checked_add(offset))
                .ok_or(StandingQueryError::Unsupported)?;
        } else if let StandingQuery::Group(query) = query {
            query.input = query
                .input
                .checked_sub(staged.first)
                .and_then(|offset| first.checked_add(offset))
                .ok_or(StandingQueryError::Unsupported)?;
        }
    }
    checkpoint()?;
    // No callbacks/fallible work between node swaps. Old states move to the
    // private suffix and are released by Drop; public indexes stay unchanged.
    for offset in 0..=root - first {
        staged
            .database
            .standing_queries
            .swap(first + offset, staged.first + offset);
    }
    Ok(at)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod projection_tests;

#[cfg(test)]
mod filter_tests;

#[cfg(test)]
mod cross_tests;

#[cfg(test)]
mod unwind_tests;

#[cfg(test)]
mod window_tests;

#[cfg(test)]
mod nested_window_tests;

#[cfg(test)]
mod ranked_pipeline_tests;

#[cfg(test)]
mod group_tests;

#[cfg(test)]
mod constant_tests;

#[cfg(test)]
mod join_tests;

#[cfg(test)]
mod selected_join_tests;

#[cfg(test)]
mod component_tests;

#[cfg(test)]
mod core_tests;

#[cfg(test)]
mod triangle_tests;

#[cfg(test)]
mod clustering_tests;

#[cfg(test)]
mod apply_tests;

#[cfg(test)]
mod existence_tests;

#[cfg(test)]
mod count_apply_tests;

#[cfg(test)]
mod distinct_apply_tests;
