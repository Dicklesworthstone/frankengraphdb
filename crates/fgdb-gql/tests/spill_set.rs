//! Physical stages preserve binary bags, Cartesian occurrence order, inner
//! pages, canonicalization and complete-input failure barriers. This small
//! materialized host tests the public semantic seam, not external I/O bounds.

use core::cmp::Ordering;
use core::convert::Infallible;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
use fgdb_gql::spill_aggregate::SpillAggregateDefinition;
use fgdb_gql::spill_set::{
    AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetAggregateRowError, SpillSetApplyKind,
    SpillSetBuildError, SpillSetRowError, SpillSetStage,
};
use fgdb_gql::{
    GlaExecutionEvent, GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError,
    GqlQueryExecution, GqlQueryPolicy, GraphAggregate, GraphAggregateError, GraphAggregateRow,
    GraphAggregateValue, GraphExactAverage, GraphIntegerBinary, GraphIntegerErrorKind,
    GraphIntegerExpression, GraphIntegerOp, GraphSetColumnType, GraphSetExecutionError,
    GraphSetOperation, GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphSet, PreparedGraphSetAggregate, PreparedGraphSetText,
    PreparedGraphText, PreparedProcedureCall,
};
use fgdb_types::{CanonicalF64, CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 10_000_000, 10_000_000)
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn leaf(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}

fn procedure_rows(call: &PreparedProcedureCall, arguments: &[GraphValue]) -> Vec<GraphValueRow> {
    assert_eq!((call.namespace(), call.name()), ("test", "rows"));
    let [GraphValue::Scalar(CanonicalScalar::Int(bias))] = arguments else {
        panic!("the semantic procedure fixture takes one integer");
    };
    [(3, 5), (1, 7), (2, 2)]
        .into_iter()
        .map(|(id, score)| {
            row(call
                .outputs()
                .iter()
                .map(|output| match output.as_str() {
                    "vertex" => GraphValue::Vertex(VId(id)),
                    "score" => scalar(bias + score),
                    _ => panic!("the semantic procedure fixture exposes vertex and score"),
                })
                .collect())
        })
        .collect()
}

fn stage_rows<E>(
    stage: &SpillSetStage,
    input: GraphValueRow,
    index: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<Vec<GraphValueRow>, SpillSetRowError<E>> {
    let mut cursor = stage.start(input, index, control)?;
    let mut rows = Vec::new();
    while let Some((row, next)) = cursor.next_with_control(control)? {
        rows.push(row);
        cursor = next;
    }
    Ok(rows)
}

struct Graph {
    properties: Vec<Option<CanonicalPropertyValue>>,
    edges: Vec<(EId, VId, RelationId, VId)>,
    edge_properties: Vec<CanonicalPropertyValue>,
}
impl Graph {
    fn sample() -> Self {
        Self {
            // Source identity order and canonical property order disagree.
            properties: [Some(3), Some(1), Some(4), Some(1), Some(2), None]
                .into_iter()
                .map(|value| {
                    value.map(|value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)))
                })
                .collect(),
            edges: vec![
                (EId(1), VId(1), R, VId(2)),
                (EId(2), VId(1), R, VId(2)),
                (EId(3), VId(2), R, VId(2)),
                (EId(4), VId(2), R, VId(3)),
            ],
            edge_properties: [7, 8, 9, 10]
                .map(|value| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value)))
                .to_vec(),
        }
    }
    fn execute(
        &self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<Infallible, ()>> {
        let ranks: Vec<_> = (1..=self.properties.len())
            .map(|index| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(index as i64)))
            .collect();
        pattern.plan().execute_governed_with_element_properties(
            (self.properties.len() + self.edges.len()) as u64,
            (1..=self.properties.len()).map(|id| VId(id as u128)),
            self.edges.iter().copied(),
            |_, _| Ok(true),
            |vid, key| {
                Ok(match key {
                    P => self.properties[vid.0 as usize - 1].as_ref(),
                    Q => Some(&ranks[vid.0 as usize - 1]),
                    _ => None,
                })
            },
            |eid, key| Ok((key == P).then(|| &self.edge_properties[eid.0 as usize - 1])),
            policy,
            || Ok(()),
        )
    }
    fn native(
        &self,
        query: &PreparedGraphSet,
    ) -> Result<Vec<GraphValueRow>, GqlQueryError<GraphSetExecutionError<Infallible>, ()>> {
        query
            .execute_governed(
                policy(),
                |pattern, policy| self.execute(pattern, policy),
                || Ok(()),
            )
            .map(|execution| execution.value)
    }
}

// Deliberately simple independent comparator: explicit scalar/element keys,
// their NULL policy, then the ordinary complete value tuple as the tie break.
fn compare(a: &GraphValueRow, b: &GraphValueRow, order: &[GraphValueOrder]) -> Ordering {
    for key in order {
        let (a, b) = (&a.values()[key.column], &b.values()[key.column]);
        let ordering = match (a.is_null(), b.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            // Explicit ORDER BY keys use openCypher orderability (42fb01c3);
            // maps order by size, then keys, then values, not canonically.
            (false, false) => {
                if key.descending {
                    b.compare_orderability(a)
                } else {
                    a.compare_orderability(b)
                }
            }
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    a.cmp(b)
}

fn staged(
    graph: &Graph,
    query: &PreparedGraphSet,
) -> Result<Vec<GraphValueRow>, SpillSetRowError<()>> {
    let plan = AsyncSpillSetPlan::compile(query).unwrap();
    staged_plan(graph, &plan)
}

fn aggregate_error(
    error: GqlQueryError<GraphAggregateError<Infallible>, ()>,
) -> SpillSetRowError<()> {
    match error {
        GqlQueryError::Source(error) => {
            SpillSetRowError::Native(GraphSetExecutionError::Aggregate(Box::new(error)))
        }
        GqlQueryError::Interrupted(error) => SpillSetRowError::Control(error),
        error => panic!("this semantic test host has no resource ceilings: {error:?}"),
    }
}

fn conversion_error(error: SpillSetAggregateRowError<()>) -> SpillSetRowError<()> {
    match error {
        SpillSetAggregateRowError::Control(error) => SpillSetRowError::Control(error),
        SpillSetAggregateRowError::Value(error) => {
            SpillSetRowError::Native(GraphSetExecutionError::Aggregate(Box::new(error)))
        }
    }
}

// A materialized reference host for the public reducer seam. Group populations
// and argument support use ordinary ordered maps; production partitions/spill
// algorithms are not involved. All accumulator arithmetic remains native.
fn reduce_groups(
    definition: &SpillAggregateDefinition,
    rows: Vec<GraphValueRow>,
) -> Result<Vec<GraphAggregateRow>, SpillSetRowError<()>> {
    let mut control = |_| Ok::<_, GqlQueryError<GraphAggregateError<Infallible>, ()>>(());
    for row in &rows {
        definition
            .validate_input(row, &mut control)
            .map_err(aggregate_error)?;
    }
    let mut groups = BTreeMap::<Vec<GraphValue>, Vec<GraphValueRow>>::new();
    if rows.is_empty() && definition.group_key_columns().is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let key = definition
            .group_key_columns()
            .iter()
            .map(|&column| row.values()[column].clone())
            .collect();
        groups.entry(key).or_default().push(row);
    }
    let mut output = Vec::new();
    for (keys, rows) in groups {
        let mut state = definition
            .new_state(&mut control)
            .map_err(aggregate_error)?;
        for row in &rows {
            definition
                .update(&mut state, row, &mut control)
                .map_err(aggregate_error)?;
        }
        for column in definition.distinct_argument_columns() {
            let mut support = BTreeMap::new();
            for row in &rows {
                support.entry(&row.values()[column]).or_insert(row);
            }
            for row in support.into_values() {
                definition
                    .update_distinct_argument(&mut state, row, column, &mut control)
                    .map_err(aggregate_error)?;
            }
        }
        output.push(
            definition
                .finish(keys, state, &mut control)
                .map_err(aggregate_error)?,
        );
    }
    Ok(output)
}

fn staged_plan(
    graph: &Graph,
    plan: &AsyncSpillSetPlan,
) -> Result<Vec<GraphValueRow>, SpillSetRowError<()>> {
    staged_plan_with_arguments(graph, plan, None)
}

fn staged_plan_with_arguments(
    graph: &Graph,
    plan: &AsyncSpillSetPlan,
    arguments: Option<&GraphValueRow>,
) -> Result<Vec<GraphValueRow>, SpillSetRowError<()>> {
    // A graph source completes its own ordered/page/visible contract first.
    // Values starts with one unit occurrence and never opens a graph source.
    let mut rows = match plan.source() {
        AsyncSpillSetSourcePlan::Values(values) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            let mut cursor = values.start(&mut |_| Ok::<_, ()>(()))?;
            let mut rows = Vec::new();
            while let Some((row, next)) = cursor.next_with_control(&mut |_| Ok::<_, ()>(()))? {
                rows.push(row);
                cursor = next;
            }
            rows
        }
        AsyncSpillSetSourcePlan::Arguments(input) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), input.columns());
            let mut cursor = input.start(arguments, &mut |_| Ok::<_, ()>(()))?;
            let mut rows = Vec::new();
            while let Some((row, next)) = cursor.next_with_control(&mut |_| Ok::<_, ()>(()))? {
                rows.push(row);
                cursor = next;
            }
            rows
        }
        AsyncSpillSetSourcePlan::Procedure(procedure) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), procedure.columns());
            assert_eq!(plan.source_count(), 1);
            assert_eq!(plan.source().kind(), fgdb_gql::scan_stream::ScanKind::Set);
            let arguments = procedure.evaluate_arguments(&mut |_| Ok::<_, ()>(()))?;
            procedure_rows(procedure.call(), &arguments)
        }
        AsyncSpillSetSourcePlan::Apply(apply) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), apply.columns());
            let left = staged_plan_with_arguments(graph, apply.left(), arguments)?;
            let mut rows = Vec::new();
            for (index, left) in left.iter().enumerate() {
                let collected = matches!(apply.kind(), SpillSetApplyKind::Collect { .. });
                if collected && apply.collect_input_is_null(left, &mut |_| Ok::<_, ()>(()))? {
                    rows.push(apply.null_collect(left, &mut |_| Ok::<_, ()>(()))?);
                    continue;
                }
                let frame = apply.prepare_arguments(left, index, &mut |_| Ok::<_, ()>(()))?;
                let right = staged_plan_with_arguments(graph, apply.right(), Some(&frame))?;
                if collected {
                    let mut collector = apply.start_collection(index, &mut |_| Ok::<_, ()>(()))?;
                    for right in &right {
                        collector = collector.push(right, &mut |_| Ok::<_, ()>(()))?;
                    }
                    rows.push(collector.finish(left, &mut |_| Ok::<_, ()>(()))?);
                    continue;
                }
                if apply.kind() == SpillSetApplyKind::Optional && right.is_empty() {
                    rows.push(apply.null_extend(left, &mut |_| Ok::<_, ()>(()))?);
                }
                for right in right {
                    rows.push(apply.copy_pair(left, &right, &mut |_| Ok::<_, ()>(()))?);
                }
            }
            rows
        }
        AsyncSpillSetSourcePlan::Binary(binary) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), binary.columns());
            // Complete both children before any parent operation, then use
            // independent count arithmetic, not the production merge driver.
            let left = staged_plan_with_arguments(graph, binary.left(), arguments)?;
            let right = staged_plan_with_arguments(graph, binary.right(), arguments)?;
            let mut counts = BTreeMap::<GraphValueRow, (usize, usize)>::new();
            for row in left {
                counts.entry(row).or_default().0 += 1;
            }
            for row in right {
                counts.entry(row).or_default().1 += 1;
            }
            let mut rows = Vec::new();
            for (row, (mut left, mut right)) in counts {
                if binary.quantifier() == GraphSetQuantifier::Distinct {
                    left = usize::from(left != 0);
                    right = usize::from(right != 0);
                }
                let count = match binary.operation() {
                    GraphSetOperation::Union
                        if binary.quantifier() == GraphSetQuantifier::Distinct =>
                    {
                        usize::from(left + right != 0)
                    }
                    GraphSetOperation::Union => left + right,
                    GraphSetOperation::Intersect => left.min(right),
                    GraphSetOperation::Except => left.saturating_sub(right),
                };
                rows.extend(std::iter::repeat_n(row, count));
            }
            rows
        }
        AsyncSpillSetSourcePlan::Cross(cross) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), cross.columns());
            assert_eq!(plan.source().kind(), fgdb_gql::scan_stream::ScanKind::Set);
            let left = staged_plan_with_arguments(graph, cross.left(), arguments)?;
            let right = staged_plan_with_arguments(graph, cross.right(), arguments)?;
            // A selected product (7d0bb0bb) keeps only its accepted pairs, as
            // the production host does; an unselected one copies every pair.
            let mut rows = Vec::new();
            for left in &left {
                for right in &right {
                    if let Some(row) =
                        cross.select_pair_with_control(left, right, &mut |event| {
                            assert_ne!(event, GlaExecutionEvent::ResultRow);
                            Ok(())
                        })?
                    {
                        rows.push(row);
                    }
                }
            }
            rows
        }
        AsyncSpillSetSourcePlan::RowJoin(join) => {
            let left = staged_plan_with_arguments(graph, join.left(), arguments)?;
            let right = staged_plan_with_arguments(graph, join.right(), arguments)?;
            let mut rows = Vec::new();
            let mut right_seen = vec![false; right.len()];
            for left in &left {
                let mut matched = false;
                for (index, right) in right.iter().enumerate() {
                    if join.matches_pair_with_control(left, right, &mut |_| Ok(()))? {
                        matched = true;
                        right_seen[index] = true;
                        if matches!(
                            join.kind(),
                            RowJoinKind::Inner
                                | RowJoinKind::Left
                                | RowJoinKind::Right
                                | RowJoinKind::Full
                        ) {
                            rows.push(join.project_pair_with_control(
                                Some(left),
                                Some(right),
                                &mut |_| Ok(()),
                            )?);
                        }
                    }
                }
                if (matched && join.kind() == RowJoinKind::Semi)
                    || (!matched
                        && matches!(
                            join.kind(),
                            RowJoinKind::Left | RowJoinKind::Full | RowJoinKind::Anti
                        ))
                {
                    rows.push(join.project_pair_with_control(Some(left), None, &mut |_| Ok(()))?);
                }
            }
            if matches!(join.kind(), RowJoinKind::Right | RowJoinKind::Full) {
                for (right, matched) in right.iter().zip(right_seen) {
                    if !matched {
                        rows.push(join.project_pair_with_control(
                            None,
                            Some(right),
                            &mut |_| Ok(()),
                        )?);
                    }
                }
            }
            // The compiler-owned identity stage below supplies the native
            // canonical join order before any body aggregate or local page.
            rows
        }
        AsyncSpillSetSourcePlan::Aggregate(aggregate) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), aggregate.columns());
            assert_eq!(plan.source().kind(), aggregate.input().source().kind());
            assert!(!aggregate.definition().has_output_stage());
            let input = staged_plan_with_arguments(graph, aggregate.input(), arguments)?;
            let groups = reduce_groups(aggregate.definition(), input)?;
            let mut rows = Vec::new();
            for group in groups {
                rows.push(
                    aggregate
                        .convert_row_with_control(&group, &mut |event| {
                            assert_ne!(event, GlaExecutionEvent::ResultRow);
                            Ok(())
                        })
                        .map_err(conversion_error)?,
                );
            }
            aggregate
                .finish_conversion_with_control(&mut |_| Ok(()))
                .map_err(conversion_error)?;
            rows
        }
        _ => {
            graph
                .execute(plan.source_pattern().unwrap(), policy())
                .unwrap()
                .value
        }
    };
    for stage in plan.stages() {
        let mut output = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            output.extend(stage_rows(stage, row, index, &mut |event| {
                assert_ne!(event, GlaExecutionEvent::ResultRow);
                Ok(())
            })?);
        }
        if let Some(order) = stage.order() {
            output.sort_by(|a, b| compare(a, b, order));
        }
        if stage.distinct() {
            output.dedup();
        }
        rows = output
            .into_iter()
            .skip(usize::try_from(stage.offset()).unwrap_or(usize::MAX))
            .take(
                stage
                    .count()
                    .and_then(|count| usize::try_from(count).ok())
                    .unwrap_or(usize::MAX),
            )
            .collect();
    }
    Ok(rows)
}

#[test]
fn explicit_single_procedure_composes_with_native_stages_and_graph_sources() {
    let graph = Graph::sample();
    let vertex = |id| GraphValue::Vertex(VId(id));
    let mut joined = Vec::new();
    for marker in [7, 7] {
        for (_, from, _, to) in &graph.edges {
            joined.push(row(vec![
                scalar(marker),
                vertex(from.0),
                vertex(to.0),
                scalar(if from.0 == 1 { 17 } else { 12 }),
            ]));
        }
    }
    joined.sort();
    for (text, expected, sources, aggregates) in [
        (
            "CALL test.rows(10) YIELD vertex AS n,score RETURN n,score ORDER BY n",
            vec![
                row(vec![vertex(1), scalar(17)]),
                row(vec![vertex(2), scalar(12)]),
                row(vec![vertex(3), scalar(15)]),
            ],
            1,
            0,
        ),
        (
            "CALL test.rows(10) YIELD vertex AS n,score WITH n,score ORDER BY score DESC SKIP 1 LIMIT 2 RETURN n,score ORDER BY score DESC",
            vec![
                row(vec![vertex(3), scalar(15)]),
                row(vec![vertex(2), scalar(12)]),
            ],
            1,
            0,
        ),
        (
            "CALL test.rows(10) YIELD vertex AS n,score WITH score % 2 AS parity,count(*) AS total RETURN parity,total ORDER BY parity",
            vec![
                row(vec![scalar(0), scalar(1)]),
                row(vec![scalar(1), scalar(2)]),
            ],
            1,
            1,
        ),
        (
            "UNWIND [7,7] AS marker CALL test.rows(10) YIELD vertex AS n,score MATCH (n)-[:R]->(m) RETURN marker,n,m,score ORDER BY marker,n,m,score",
            joined,
            2,
            0,
        ),
    ] {
        let query = prepare(text);
        let transcript = query.canonical_bytes();
        assert!(AsyncSpillSetPlan::compile(&query).is_err(), "{text}");
        let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(plan.source_count(), sources, "{text}");
        assert_eq!(plan.aggregate_count(), aggregates, "{text}");
        assert_eq!(
            plan.procedure_source().unwrap().call().outputs(),
            ["vertex", "score"]
        );
        assert_eq!(staged_plan(&graph, &plan).unwrap(), expected, "{text}");
        let mut calls = 0;
        let native = query
            .execute_governed_with_procedures(
                policy(),
                |pattern, policy| graph.execute(pattern, policy),
                |call, arguments, _| {
                    calls += 1;
                    let value = procedure_rows(call, arguments);
                    Ok(GqlQueryExecution {
                        rows: GqlExecutionStats {
                            snapshot_records: 0,
                            result_rows: value.len() as u64,
                        },
                        evaluator: GlaExecutionStats::default(),
                        value,
                    })
                },
                || Ok(()),
            )
            .unwrap();
        assert_eq!(calls, 1, "{text}");
        assert_eq!(native.value, expected, "{text}");
        assert_eq!(query.canonical_bytes(), transcript);
    }
}

#[test]
fn earlier_child_failures_precede_lazy_procedure_arguments_under_empty_pages() {
    let graph = Graph::sample();
    let argument = GraphSetValue::Integer(
        GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
        ])
        .unwrap(),
    );
    let procedure = PreparedGraphSet::singleton()
        .procedure_call(
            "test".into(),
            "rows".into(),
            vec![argument],
            Vec::new(),
            vec![("vertex".into(), "n".into())],
            vec![0],
        )
        .unwrap();
    let query = PreparedGraphSet::singleton()
        .unwind("x".into(), GraphSetValue::Value(scalar(1)))
        .unwrap()
        .cross_join(procedure)
        .unwrap()
        .with_page(0, Some(0));
    let plan = AsyncSpillSetPlan::compile_with_single_procedure(&query).unwrap();
    let mut called = false;
    let native = query.execute_governed_with_procedures(
        policy(),
        |pattern, policy| graph.execute(pattern, policy),
        |call, arguments, _| {
            called = true;
            let value = procedure_rows(call, arguments);
            Ok(GqlQueryExecution {
                rows: GqlExecutionStats {
                    snapshot_records: 0,
                    result_rows: value.len() as u64,
                },
                evaluator: GlaExecutionStats::default(),
                value,
            })
        },
        || Ok(()),
    );
    let Err(GqlQueryError::Source(expected)) = native else {
        panic!("UNWIND of a scalar must fail before the procedure source");
    };
    assert!(!called);
    assert_eq!(
        staged_plan(&graph, &plan),
        Err(SpillSetRowError::Native(expected))
    );
}

#[test]
fn graph_reading_collections_preserve_member_order_nulls_and_nested_frames() {
    let graph = Graph::sample();
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let members = [2, 1, 2].map(|id| GraphValue::Vertex(VId(id)));
    let mapped = GraphValue::List(members.to_vec().into());
    let parameters = GqlParameters::new()
        .with_list(
            "items",
            vec![
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
                null.clone(),
                GraphValue::Vertex(VId(3)),
            ],
        )
        .unwrap();
    for (text, expected) in [
        (
            "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | x] AS value",
            vec![row(vec![mapped.clone()])],
        ),
        (
            "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | NULL] AS value",
            vec![row(vec![GraphValue::List(vec![null.clone(); 3].into())])],
        ),
        (
            "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | [x,NULL,x]] AS value",
            vec![row(vec![GraphValue::List(
                members
                    .iter()
                    .map(|member| {
                        GraphValue::List(vec![member.clone(), null.clone(), member.clone()].into())
                    })
                    .collect(),
            )])],
        ),
        (
            "UNWIND [7,7] AS marker WITH marker,$items AS xs \
             RETURN marker,[x IN xs WHERE (x)-[:R]->() | x] AS value",
            vec![
                row(vec![scalar(7), mapped.clone()]),
                row(vec![scalar(7), mapped.clone()]),
            ],
        ),
        (
            "WITH 7 AS x,$items AS xs \
             CALL (xs) { RETURN [x IN xs WHERE (x)-[:R]->() | x] AS value } RETURN x,value",
            vec![row(vec![scalar(7), mapped.clone()])],
        ),
        (
            "WITH $items AS xs WITH [x IN xs WHERE (x)-[:R]->() | x] AS active \
             UNWIND active AS node RETURN node",
            members
                .iter()
                .cloned()
                .map(|member| row(vec![member]))
                .collect(),
        ),
    ] {
        let query = PreparedGraphSetText::prepare_with_parameter_types(
            text,
            &[("items", fgdb_gql::GqlParameterType::List)],
            symbols,
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&parameters)
        .unwrap();
        let transcript = query.canonical_bytes();
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        assert!(plan.source_count() > 0, "{text}");
        assert_eq!(staged_plan(&graph, &plan).unwrap(), expected, "{text}");
        assert_eq!(graph.native(&query).unwrap(), expected, "{text}");
        assert_eq!(query.canonical_bytes(), transcript);
    }
}

#[test]
fn nested_graph_collections_and_quantifiers_keep_unknowns_and_outer_frames() {
    let graph = Graph::sample();
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let vertices = |ids: &[u128]| {
        GraphValue::List(ids.iter().map(|&id| GraphValue::Vertex(VId(id))).collect())
    };
    let bind = |text: &str, parameters: &GqlParameters| {
        PreparedGraphSetText::prepare_with_parameter_types(
            text,
            &[("items", fgdb_gql::GqlParameterType::List)],
            symbols,
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(parameters)
        .unwrap()
    };
    let compare = |query: &PreparedGraphSet, expected: Vec<GraphValueRow>| {
        let plan = AsyncSpillSetPlan::compile(query).unwrap();
        assert!(plan.source_count() > 0);
        assert_eq!(staged_plan(&graph, &plan).unwrap(), expected);
        assert_eq!(graph.native(query).unwrap(), expected);
    };

    // Each inner list is a collected Apply under another collected Apply.
    // Equal inputs remain separate occurrences; an empty inner body yields
    // [], while its NULL input skips that body and retains NULL. The inner x
    // shadows, then restores, the outer scalar binding.
    let parameters = GqlParameters::new()
        .with_list(
            "items",
            vec![
                vertices(&[2, 1, 2, 3]),
                vertices(&[2, 1, 2, 3]),
                vertices(&[]),
                null.clone(),
            ],
        )
        .unwrap();
    let query = bind(
        "WITH 7 AS x,$items AS groups RETURN x,\
         [xs IN groups | [x IN xs WHERE (x)-[:R]->() | x]] AS values,x AS restored",
        &parameters,
    );
    compare(
        &query,
        vec![row(vec![
            scalar(7),
            GraphValue::List(
                vec![
                    vertices(&[2, 1, 2]),
                    vertices(&[2, 1, 2]),
                    vertices(&[]),
                    null.clone(),
                ]
                .into(),
            ),
            scalar(7),
        ])],
    );

    // Graph predicates are evaluated per member and their Boolean/UNKNOWN
    // results are collected before the ordinary quantifier runs. In the
    // [true,NULL] case, dropping mapped NULLs would turn ALL and SINGLE from
    // UNKNOWN into true. A duplicate true makes SINGLE false instead.
    for (ids, expected) in [
        (Some(vec![1, 3]), [Some(true), None, Some(false), None]),
        (Some(vec![3]), [None; 4]),
        (
            Some(vec![1, 1]),
            [Some(true), Some(true), Some(false), Some(false)],
        ),
        (
            Some(vec![]),
            [Some(false), Some(true), Some(true), Some(false)],
        ),
        (None, [None; 4]),
    ] {
        let parameters = if let Some(ids) = ids {
            GqlParameters::new()
                .with_list(
                    "items",
                    ids.into_iter()
                        .map(|id| GraphValue::Vertex(VId(id)))
                        .collect(),
                )
                .unwrap()
        } else {
            GqlParameters::new().with_null("items").unwrap()
        };
        for (kind, expected) in ["ANY", "ALL", "NONE", "SINGLE"].into_iter().zip(expected) {
            let text = format!(
                "WITH $items AS xs RETURN {kind}(x IN xs WHERE (x)-[:R]->() OR NULL) AS value"
            );
            let expected = expected.map_or_else(
                || null.clone(),
                |value| GraphValue::Scalar(CanonicalScalar::Bool(value)),
            );
            compare(&bind(&text, &parameters), vec![row(vec![expected])]);
        }
    }
}

#[test]
fn graph_reading_collections_skip_null_and_empty_inputs_but_keep_late_failures() {
    let graph = Graph::sample();
    for (list, expected) in [
        ("NULL", GraphValue::Scalar(CanonicalScalar::Null)),
        ("[]", GraphValue::List(Box::new([]))),
    ] {
        for suffix in ["", " LIMIT 0"] {
            let query = prepare(&format!(
                "WITH {list} AS xs RETURN [x IN xs WHERE (x)-[:R]->() | 1/0] AS value{suffix}"
            ));
            let expected = if suffix.is_empty() {
                vec![row(vec![expected.clone()])]
            } else {
                vec![]
            };
            assert_eq!(staged(&graph, &query).unwrap(), expected);
            assert_eq!(graph.native(&query).unwrap(), expected);
        }
    }
    let parameters = GqlParameters::new()
        .with_list(
            "items",
            vec![
                GraphValue::Vertex(VId(2)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
            ],
        )
        .unwrap();
    for suffix in ["", " LIMIT 0"] {
        let text = format!(
            "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | \
             CASE WHEN x = xs[1] THEN 1/0 ELSE 1 END] AS value{suffix}"
        );
        let query = PreparedGraphSetText::prepare_with_parameter_types(
            &text,
            &[("items", fgdb_gql::GqlParameterType::List)],
            symbols,
        )
        .unwrap()
        .bind_parameters(&parameters)
        .unwrap();
        let GqlQueryError::Source(expected) = graph.native(&query).unwrap_err() else {
            panic!("the reached division must remain a native source error");
        };
        assert!(
            matches!(&expected, GraphSetExecutionError::Projection { error, .. }
            if error.kind == GraphIntegerErrorKind::DivisionByZero)
        );
        assert_eq!(
            staged(&graph, &query),
            Err(SpillSetRowError::Native(expected))
        );
    }
}

#[test]
fn correlated_call_keeps_occurrences_nulls_local_pages_and_nested_frames() {
    let graph = Graph::sample();
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    for (text, expected) in [
        (
            "UNWIND [2,2,null] AS x CALL (x) { RETURN x AS y } RETURN x,y",
            vec![
                row(vec![scalar(2), scalar(2)]),
                row(vec![scalar(2), scalar(2)]),
                row(vec![null.clone(), null.clone()]),
            ],
        ),
        (
            "UNWIND [2,1,2] AS x CALL (x) { \
             UNWIND [x,x+10,x+10] AS y RETURN DISTINCT y ORDER BY y DESC SKIP 1 LIMIT 1 \
             } RETURN x,y",
            vec![
                row(vec![scalar(2), scalar(2)]),
                row(vec![scalar(1), scalar(1)]),
                row(vec![scalar(2), scalar(2)]),
            ],
        ),
        (
            "WITH 3 AS x CALL (x) { \
             CALL (x) { RETURN x*2 AS z } RETURN z+1 AS y } RETURN x,y",
            vec![row(vec![scalar(3), scalar(7)])],
        ),
        (
            "WITH 1 AS x CALL (x) { UNWIND [] AS y RETURN y } RETURN x,y",
            vec![],
        ),
    ] {
        let query = prepare(text);
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        assert_eq!(plan.source_count(), 0, "{text}");
        assert_eq!(staged_plan(&graph, &plan).unwrap(), expected, "{text}");
        assert_eq!(graph.native(&query).unwrap(), expected, "{text}");
    }
}

#[test]
fn correlated_graph_count_preserves_zero_groups_and_static_source_ordinals() {
    let graph = Graph::sample();
    let query = prepare(
        "MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) RETURN count(*) AS degree } \
         RETURN n,degree ORDER BY n",
    );
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    assert_eq!(plan.source_count(), 2);
    assert_eq!(plan.aggregate_count(), 1);
    let expected = (1..=6)
        .map(|id| {
            row(vec![
                GraphValue::Vertex(VId(id)),
                scalar(if id <= 2 { 2 } else { 0 }),
            ])
        })
        .collect::<Vec<_>>();
    assert_eq!(staged_plan(&graph, &plan).unwrap(), expected);
    assert_eq!(graph.native(&query).unwrap(), expected);
    // The source identity range is logical metadata; a later empty page
    // neither removes the body source nor changes its ordinal.
    let empty = AsyncSpillSetPlan::compile(&query.with_page(0, Some(0))).unwrap();
    assert_eq!(empty.source_count(), 2);
}

#[test]
fn correlated_late_failure_survives_empty_pages_but_empty_outer_never_invokes_body() {
    let graph = Graph::sample();
    for text in [
        "UNWIND [1,0] AS x CALL (x) { RETURN 1/x AS y } RETURN y LIMIT 0",
        "WITH 0 AS x CALL (x) { RETURN 1/x AS y LIMIT 0 } RETURN y",
    ] {
        let query = prepare(text);
        assert!(
            matches!(
                staged(&graph, &query),
                Err(SpillSetRowError::Native(GraphSetExecutionError::Projection { error, .. }))
                    if error.kind == GraphIntegerErrorKind::DivisionByZero
            ),
            "{text}"
        );
        assert!(graph.native(&query).is_err(), "{text}");
    }
    let query = prepare("UNWIND [] AS x CALL (x) { RETURN 1/(x-x) AS y } RETURN y");
    assert!(staged(&graph, &query).unwrap().is_empty());
    assert!(graph.native(&query).unwrap().is_empty());
}

#[test]
fn computed_values_predicates_and_complete_inner_pages_match_native_relations() {
    let graph = Graph::sample();
    for text in [
        "MATCH (n) RETURN n.p%2 AS value",
        "MATCH (n) RETURN DISTINCT n.p%2 AS value ORDER BY value DESC SKIP 1 LIMIT 2",
        "MATCH (n) RETURN {bucket:n.p%2,nested:[n.p,null]} AS value ORDER BY value",
        "MATCH (n) RETURN [x IN [n.p,2,3] WHERE x > 1 | x * 2] AS value ORDER BY value DESC",
        "MATCH (n) RETURN reduce(total=0,x IN [n.p,2] | total+x) AS value",
        "MATCH (n) WITH n.p AS x WHERE x>1 RETURN x*2 AS value ORDER BY value DESC",
        "MATCH (n) WITH n.p AS x ORDER BY x DESC SKIP 1 LIMIT 3 RETURN 10-x AS value",
        "MATCH (n) WITH DISTINCT n.p%2 AS bucket ORDER BY bucket DESC SKIP 1 LIMIT 1 RETURN bucket+10 AS value",
        "MATCH (n) WITH n.p AS x WHERE x%2=1 RETURN [x,x+1] AS value LIMIT 2",
        "MATCH (n) WITH n.p AS x ORDER BY x DESC LIMIT 0 RETURN 1/(x-x) AS value",
    ] {
        let query = prepare(text);
        assert_eq!(
            staged(&graph, &query).unwrap(),
            graph.native(&query).unwrap(),
            "{text}"
        );
    }
    let query = prepare(
        "MATCH (n) WITH DISTINCT n.p%2 AS bucket ORDER BY bucket DESC SKIP 1 LIMIT 1 RETURN bucket+10 AS value",
    );
    assert_eq!(staged(&graph, &query).unwrap(), vec![row(vec![scalar(10)])]);
}

#[test]
fn unwind_preserves_native_bags_nested_values_and_each_relational_boundary() {
    let graph = Graph::sample();
    for text in [
        "MATCH (n) WITH n.p AS p UNWIND [p,p,NULL] AS item RETURN item ORDER BY item",
        "MATCH (n) WITH [] AS items UNWIND items AS item RETURN item",
        "MATCH (n) WITH NULL AS items UNWIND items AS item RETURN item",
        "MATCH (n) WITH n.p AS p UNWIND [[p,p+1],[],NULL,[p]] AS items UNWIND items AS item RETURN item ORDER BY item DESC",
        "MATCH (n) WITH n.p AS p UNWIND [{value:p,items:[p,p+1]}] AS record UNWIND record.items AS item RETURN record.value AS owner,item ORDER BY owner,item",
        "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 2 UNWIND [p,p-1] AS item WITH DISTINCT item ORDER BY item SKIP 1 LIMIT 2 RETURN item+10 AS value ORDER BY value",
        "MATCH (n) WITH n.p AS p UNWIND [p,p+1] AS item WITH item WHERE item>2 RETURN DISTINCT item ORDER BY item DESC LIMIT 2",
        "MATCH (a)-[r:R]->(b) WITH r.p AS p UNWIND [p,p] AS item RETURN item ORDER BY item",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH r.p+s.p AS p UNWIND [p,p+1] AS item RETURN item ORDER BY item DESC SKIP 1 LIMIT 3",
        "MATCH (n) WITH n.p AS p ORDER BY p LIMIT 0 UNWIND [1/(p-p)] AS item RETURN item",
    ] {
        let query = prepare(text);
        assert_eq!(
            staged(&graph, &query).unwrap_or_else(|error| panic!("{text}: {error:?}")),
            graph
                .native(&query)
                .unwrap_or_else(|error| panic!("{text}: {error:?}")),
            "{text}"
        );
    }
}

#[test]
fn unwind_cursor_keeps_element_order_and_obeys_every_host_refusal() {
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let map = GraphValue::map(vec![("value".into(), scalar(7))]).unwrap();
    let nested = GraphValue::List(vec![scalar(4), null.clone()].into_boxed_slice());
    let elements = vec![scalar(3), null, map, nested, scalar(3)];
    let query = PreparedGraphSet::from(leaf("MATCH (n) RETURN n.p AS value"))
        .unwind(
            "element".to_owned(),
            GraphSetValue::List(elements.iter().cloned().map(GraphSetValue::Value).collect()),
        )
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let stage = plan.stages().last().unwrap();
    assert_eq!(stage.order(), None, "UNWIND preserves list order");
    let input = row(vec![scalar(11)]);
    let mut events = Vec::new();
    let actual = stage_rows(stage, input.clone(), 17, &mut |event| {
        events.push(event);
        Ok::<_, &'static str>(())
    })
    .unwrap();
    let expected: Vec<_> = elements
        .into_iter()
        .map(|element| row(vec![scalar(11), element]))
        .collect();
    assert_eq!(actual, expected);
    assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
    // A refusal at any allocation/work checkpoint stops that owned cursor.
    // No later callback or output may be produced after the refusal.
    for refusal in 0..events.len() {
        let mut observed = Vec::new();
        let result = stage_rows(stage, input.clone(), 17, &mut |event| {
            observed.push(event);
            if observed.len() == refusal + 1 {
                Err("cancelled or exhausted")
            } else {
                Ok(())
            }
        });
        assert_eq!(
            result,
            Err(SpillSetRowError::Control("cancelled or exhausted"))
        );
        assert_eq!(observed, events[..=refusal]);
    }
}

#[test]
fn unwind_null_empty_and_type_errors_keep_original_input_locations() {
    let query = PreparedGraphSet::from(leaf("MATCH (n) RETURN n.p AS value"))
        .unwind("element".into(), GraphSetValue::Column(0))
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let stage = plan.stages().last().unwrap();
    assert_eq!(
        stage_rows(
            stage,
            row(vec![GraphValue::Scalar(CanonicalScalar::Null)]),
            23,
            &mut |_| Ok::<_, ()>(()),
        ),
        Ok(vec![])
    );
    assert!(matches!(
        stage_rows(stage, row(vec![scalar(2)]), 23, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetRowError::Native(GraphSetExecutionError::Projection {
            row: 23,
            column: 1,
            error,
        })) if error.kind == GraphIntegerErrorKind::IncompatibleOperands
    ));
    let graph = Graph::sample();
    for tail in ["LIMIT 0", "LIMIT 1", "SKIP 1000 LIMIT 2"] {
        let text = format!(
            "MATCH (n) WITH n.p AS p UNWIND [[p],2] AS items UNWIND items AS item RETURN item {tail}"
        );
        let query = prepare(&text);
        let GqlQueryError::Source(expected) = graph.native(&query).unwrap_err() else {
            panic!("native UNWIND type error");
        };
        assert!(matches!(
            expected,
            GraphSetExecutionError::Projection { row: 1, .. }
        ));
        assert_eq!(
            staged(&graph, &query),
            Err(SpillSetRowError::Native(expected))
        );
    }
}

#[test]
fn values_source_emits_one_metered_unit_without_graph_metadata_or_result_charges() {
    let query = PreparedGraphSet::singleton();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Values(values) = plan.source() else {
        panic!("source-free plan must have a Values source");
    };
    assert!(plan.source_pattern().is_none());
    assert!(plan.source_tail().is_none());
    assert!(plan.source_columns().is_empty());
    let mut events = Vec::new();
    let cursor = values
        .start(&mut |event| {
            events.push(event);
            Ok::<_, &'static str>(())
        })
        .unwrap();
    let (unit, cursor) = cursor
        .next_with_control(&mut |event| {
            events.push(event);
            Ok::<_, &'static str>(())
        })
        .unwrap()
        .unwrap();
    assert_eq!(unit, GraphValueRow::from_owned_values(Vec::new()));
    assert!(
        cursor
            .next_with_control(&mut |event| {
                events.push(event);
                Ok::<_, &'static str>(())
            })
            .unwrap()
            .is_none()
    );
    assert_eq!(
        events,
        [GlaExecutionEvent::Work, GlaExecutionEvent::ScratchEntry]
    );
    for refusal in 0..events.len() {
        let mut observed = Vec::new();
        let result = values.start(&mut |event| {
            observed.push(event);
            if observed.len() == refusal + 1 {
                Err("source admission refused")
            } else {
                Ok(())
            }
        });
        assert_eq!(
            result.err(),
            Some(SpillSetRowError::Control("source admission refused"))
        );
        assert_eq!(observed, events[..=refusal]);
    }
    assert_eq!(
        staged(&Graph::sample(), &query).unwrap(),
        vec![GraphValueRow::from_owned_values(Vec::new())]
    );
}

#[test]
fn source_free_values_and_nested_unwind_match_independent_rows_without_graph_access() {
    for (text, expected) in [
        ("RETURN 42 AS answer", vec![row(vec![scalar(42)])]),
        (
            "UNWIND [3,1,3,NULL] AS item RETURN item ORDER BY item",
            vec![
                row(vec![scalar(1)]),
                row(vec![scalar(3)]),
                row(vec![scalar(3)]),
                row(vec![GraphValue::Scalar(CanonicalScalar::Null)]),
            ],
        ),
        (
            "WITH 2 AS x UNWIND [x,x+1,x] AS item RETURN item ORDER BY item",
            vec![
                row(vec![scalar(2)]),
                row(vec![scalar(2)]),
                row(vec![scalar(3)]),
            ],
        ),
        (
            "UNWIND [3,1,3] AS x WITH DISTINCT x ORDER BY x DESC LIMIT 1 UNWIND [x,x+1] AS value RETURN value ORDER BY value",
            vec![row(vec![scalar(3)]), row(vec![scalar(4)])],
        ),
        (
            "UNWIND [{items:[1,2]},{items:[]},{items:NULL}] AS record UNWIND record.items AS item RETURN item ORDER BY item",
            vec![row(vec![scalar(1)]), row(vec![scalar(2)])],
        ),
        ("UNWIND [] AS item RETURN item", vec![]),
        ("UNWIND NULL AS item RETURN item", vec![]),
        ("RETURN 42 AS answer LIMIT 0", vec![]),
        (
            "WITH 0 AS x LIMIT 0 UNWIND [1/x] AS item RETURN item",
            vec![],
        ),
    ] {
        let query = prepare(text);
        let native = query
            .execute_governed(
                GqlQueryPolicy::new(0, 100, 100_000, 100_000),
                |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<Infallible, ()>> {
                    panic!("{text}: a source-free query must not ask for graph rows");
                },
                || Ok(()),
            )
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert_eq!(native.rows.snapshot_records, 0);
        assert_eq!(native.value, expected, "{text}");
        assert_eq!(
            staged(&Graph::sample(), &query).unwrap(),
            expected,
            "{text}"
        );
    }
}

#[test]
fn source_free_complete_child_failures_precede_parent_expressions_and_empty_pages() {
    let graph = Graph::sample();
    for text in [
        "UNWIND [[1],2] AS items UNWIND items AS item RETURN 1/(item-item) AS value LIMIT 0",
        "UNWIND [1,0] AS value RETURN 1/value AS result LIMIT 0",
        "UNWIND [1,0] AS value WITH 1/value AS result RETURN result LIMIT 1",
    ] {
        let query = prepare(text);
        let GqlQueryError::Source(expected) = graph.native(&query).unwrap_err() else {
            panic!("{text}: native expression or UNWIND type failure");
        };
        assert!(matches!(
            expected,
            GraphSetExecutionError::Projection { row: 1, .. }
        ));
        assert_eq!(
            staged(&graph, &query),
            Err(SpillSetRowError::Native(expected)),
            "{text}"
        );
    }
}

#[test]
fn binary_sources_preserve_all_six_bag_laws_and_canonical_null_equality() {
    let graph = Graph::sample();
    for (operation, quantifier, expected) in [
        (
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            vec![
                None,
                None,
                None,
                Some(1),
                Some(2),
                Some(2),
                Some(2),
                Some(3),
            ],
        ),
        (
            GraphSetOperation::Union,
            GraphSetQuantifier::Distinct,
            vec![None, Some(1), Some(2), Some(3)],
        ),
        (
            GraphSetOperation::Intersect,
            GraphSetQuantifier::All,
            vec![None, Some(2)],
        ),
        (
            GraphSetOperation::Intersect,
            GraphSetQuantifier::Distinct,
            vec![None, Some(2)],
        ),
        (
            GraphSetOperation::Except,
            GraphSetQuantifier::All,
            vec![None, Some(1), Some(2)],
        ),
        (
            GraphSetOperation::Except,
            GraphSetQuantifier::Distinct,
            vec![Some(1)],
        ),
    ] {
        let query = prepare("UNWIND [2,NULL,1,2,NULL] AS value RETURN value")
            .combine(
                operation,
                quantifier,
                prepare("UNWIND [3,NULL,2] AS value RETURN value"),
            )
            .unwrap();
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        let AsyncSpillSetSourcePlan::Binary(binary) = plan.source() else {
            panic!("binary source must retain both operands");
        };
        assert_eq!(binary.operation(), operation);
        assert_eq!(binary.quantifier(), quantifier);
        assert_eq!(plan.source().kind(), fgdb_gql::scan_stream::ScanKind::Set);
        assert_eq!(
            plan.stage_count(),
            plan.stages().len() + binary.left().stage_count() + binary.right().stage_count() + 3
        );
        let expected: Vec<_> = expected
            .into_iter()
            .map(|value| {
                row(vec![value.map_or_else(
                    || GraphValue::Scalar(CanonicalScalar::Null),
                    scalar,
                )])
            })
            .collect();
        assert_eq!(staged_plan(&graph, &plan).unwrap(), expected);
        assert_eq!(graph.native(&query).unwrap(), expected);
    }
}

#[test]
fn binary_sources_keep_typed_scalars_nested_values_child_pages_and_parent_stages() {
    let graph = Graph::sample();
    let text = "UNWIND [1,1.0,NULL,{z:[2,NULL],a:1},{a:1,z:[2,NULL]},[1,2]] AS value RETURN value UNION DISTINCT UNWIND [1.0,NULL,{a:1,z:[2,NULL]}] AS value RETURN value";
    let query = prepare(text);
    let mut expected = vec![
        row(vec![scalar(1)]),
        row(vec![GraphValue::Scalar(CanonicalScalar::Float(
            CanonicalF64::new(1.0),
        ))]),
        row(vec![GraphValue::Scalar(CanonicalScalar::Null)]),
        row(vec![
            GraphValue::map(vec![
                ("a".into(), scalar(1)),
                (
                    "z".into(),
                    GraphValue::List(
                        vec![scalar(2), GraphValue::Scalar(CanonicalScalar::Null)]
                            .into_boxed_slice(),
                    ),
                ),
            ])
            .unwrap(),
        ]),
        row(vec![GraphValue::List(
            vec![scalar(1), scalar(2)].into_boxed_slice(),
        )]),
    ];
    expected.sort();
    assert_eq!(staged(&graph, &query).unwrap(), expected);
    assert_eq!(graph.native(&query).unwrap(), expected);
    for (text, expected) in [
        (
            "(UNWIND [4,1,4,2] AS value RETURN value ORDER BY value DESC LIMIT 2) EXCEPT ALL (UNWIND [4] AS value RETURN value)",
            vec![row(vec![scalar(4)])],
        ),
        (
            "((UNWIND [1,2,2] AS value RETURN value) EXCEPT ALL (UNWIND [2] AS value RETURN value)) UNION ALL (UNWIND [3] AS value RETURN value) ORDER BY value DESC SKIP 1 LIMIT 2",
            vec![row(vec![scalar(2)]), row(vec![scalar(1)])],
        ),
        (
            "RETURN 1 AS a,2 AS b UNION DISTINCT RETURN 2 AS b,1 AS a",
            vec![row(vec![scalar(1), scalar(2)])],
        ),
        (
            "MATCH (n) RETURN n.p AS value INTERSECT ALL UNWIND [1,1,3,NULL] AS value RETURN value+0 AS value",
            vec![
                row(vec![GraphValue::Scalar(CanonicalScalar::Null)]),
                row(vec![scalar(1)]),
                row(vec![scalar(1)]),
                row(vec![scalar(3)]),
            ],
        ),
    ] {
        let query = prepare(text);
        assert_eq!(staged(&graph, &query).unwrap(), expected, "{text}");
        assert_eq!(graph.native(&query).unwrap(), expected, "{text}");
    }
    let query = prepare("RETURN 3 AS value UNION ALL RETURN 1 AS value")
        .unwind(
            "item".into(),
            GraphSetValue::List(vec![GraphSetValue::Column(0), GraphSetValue::Column(0)]),
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new("item", GraphSetValue::Column(1))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(1, Some(2));
    assert_eq!(
        staged(&graph, &query).unwrap(),
        vec![row(vec![scalar(1)]), row(vec![scalar(3)])]
    );
    assert_eq!(
        staged(&graph, &query).unwrap(),
        graph.native(&query).unwrap()
    );
}

#[test]
fn binary_empty_left_and_zero_pages_never_hide_right_or_reorder_child_failures() {
    let graph = Graph::sample();
    for operator in ["UNION ALL", "INTERSECT ALL", "EXCEPT DISTINCT"] {
        for left in [
            "UNWIND [] AS value RETURN value+0 AS value",
            "UNWIND [1,0] AS value RETURN 1/value AS value",
        ] {
            let text = format!(
                "({left}) {operator} (UNWIND [[1],2] AS items UNWIND items AS item RETURN item+0 AS value) LIMIT 0"
            );
            let query = prepare(&text);
            let GqlQueryError::Source(expected) = graph.native(&query).unwrap_err() else {
                panic!("{text}: native child failure");
            };
            let expected_kind = if left.contains("1/value") {
                GraphIntegerErrorKind::DivisionByZero
            } else {
                GraphIntegerErrorKind::IncompatibleOperands
            };
            assert!(
                matches!(expected, GraphSetExecutionError::Projection { row: 1, error, .. }
                if error.kind == expected_kind),
                "{text}: {expected:?}"
            );
            assert_eq!(
                staged(&graph, &query),
                Err(SpillSetRowError::Native(expected))
            );
        }
    }
}

#[test]
fn typed_binary_aggregate_compilation_retains_the_complete_relation() {
    let query = prepare("RETURN 3 AS value UNION ALL RETURN 1 AS value");
    let aggregate = PreparedGraphSetAggregate::prepare(
        query.clone(),
        &[],
        &[
            GraphAggregate::count_rows("rows"),
            GraphAggregate::sum_int("total", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let plan =
        fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan::compile_relation(&aggregate).unwrap();
    assert_eq!(plan.kind(), fgdb_gql::scan_stream::ScanKind::Set);
    let fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan::Relation { input, definition } = plan
    else {
        panic!("compound aggregation must retain its original relation");
    };
    assert_eq!(definition.input_width(), 1);
    assert_eq!(
        staged_plan(&Graph::sample(), &input).unwrap(),
        vec![row(vec![scalar(1)]), row(vec![scalar(3)])]
    );
    assert_eq!(
        input.stage_count(),
        AsyncSpillSetPlan::compile(&query).unwrap().stage_count()
    );
}

#[test]
fn single_edge_parallel_multiplicity_orientation_and_computed_payloads_are_admitted() {
    let graph = Graph::sample();
    for direction in ["->", "-"] {
        let text = format!(
            "MATCH (a)-[r:R]{direction}(b) RETURN {{from:a.p,total:r.p+b.p}} AS value ORDER BY value DESC SKIP 1 LIMIT 5"
        );
        let query = prepare(&text);
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        assert!(matches!(plan.source(), AsyncSpillSetSourcePlan::Edge(_)));
        assert_eq!(
            staged(&graph, &query).unwrap(),
            graph.native(&query).unwrap(),
            "{text}"
        );
    }
}

#[test]
fn source_page_precedes_visible_row_canonicalization_and_scopes_preserve_selected_order() {
    let graph = Graph::sample();
    let pattern = leaf("MATCH (n) RETURN n.p AS value ORDER BY n.q DESC SKIP 1 LIMIT 3");
    let query = PreparedGraphSet::from(pattern)
        .nested()
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(2))
        .nested()
        .unwrap()
        .with_page(1, Some(1));
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let tail = plan.source_tail().unwrap();
    assert!(tail.evaluation_width() > tail.visible_width());
    assert_eq!(tail.offset(), 1);
    assert_eq!(tail.count(), Some(3));
    assert_eq!(plan.stages().len(), 3);
    assert_eq!(plan.stages()[0].order(), Some(&[][..]));
    assert_eq!(plan.stages()[2].order(), None);
    assert_eq!(plan.stages()[2].offset(), 1);
    assert_eq!(plan.stages()[2].count(), Some(1));
    assert_eq!(staged(&graph, &query).unwrap(), vec![row(vec![scalar(1)])]);
    assert_eq!(
        staged(&graph, &query).unwrap(),
        graph.native(&query).unwrap()
    );
}

#[test]
fn complete_stage_failure_wins_before_any_downstream_expression_even_limit_zero() {
    let graph = Graph {
        properties: [1, 2, 3]
            .map(|value| Some(CanonicalPropertyValue::Scalar(CanonicalScalar::Int(value))))
            .to_vec(),
        edges: vec![],
        edge_properties: vec![],
    };
    // Fusing per input would fail the outer expression on the FIRST row.
    // The native barrier requires the inner third-row failure to win instead.
    let query = prepare("MATCH (n) WITH 12/(3-n.p) AS value RETURN 1/(value-6) AS result LIMIT 0");
    let GqlQueryError::Source(expected) = graph.native(&query).unwrap_err() else {
        panic!("native expression failure");
    };
    assert!(
        matches!(expected, GraphSetExecutionError::Projection { row: 2, column: 0, error } if error.kind == GraphIntegerErrorKind::DivisionByZero)
    );
    assert_eq!(
        staged(&graph, &query),
        Err(SpillSetRowError::Native(expected))
    );
}

#[test]
fn projected_allocations_and_refusals_use_the_host_control_without_final_row_charges() {
    let query = prepare("MATCH (n) RETURN {value:n.p,nested:[n.p,n.p+1]} AS result LIMIT 0");
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let stage = plan.stages().last().unwrap();
    assert_eq!(stage.count(), Some(0));
    let input = row(vec![scalar(2)]);
    let mut events = Vec::new();
    let output = stage_rows(stage, input.clone(), 17, &mut |event| {
        events.push(event);
        Ok::<_, &'static str>(())
    })
    .unwrap();
    assert_eq!(output.len(), 1);
    assert!(matches!(output[0].get(0), Some(GraphValue::Map { .. })));
    assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
    for refuse in 0..events.len() {
        let mut observed = Vec::new();
        let error = stage_rows(stage, input.clone(), 17, &mut |event| {
            observed.push(event);
            if observed.len() == refuse + 1 {
                Err("reservation refused")
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error, SpillSetRowError::Control("reservation refused"));
        assert_eq!(observed, events[..=refuse]);
    }
    assert!(matches!(
        stage.start(
            row(vec![GraphValue::Vertex(VId(2))]),
            0,
            &mut |_| Ok::<_, ()>(())
        ),
        Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 0 }
        ))
    ));
}

#[test]
fn unsupported_sources_and_relational_descendants_refuse_before_limit_zero() {
    // A vertex source with an existence probe is still outside the profile.
    let text = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a.p+1 AS value LIMIT 0";
    let error = AsyncSpillSetPlan::compile(&prepare(text)).unwrap_err();
    assert!(
        matches!(error, SpillSetBuildError::Vertex(_)),
        "{text}: {error:?}"
    );
    // A fixed-hop edge source carries its probe and spills with the native
    // answer. Relational joins execute over completed child runs (d3574354;
    // their rows are relational_join's laws), so a nested join and a union
    // over one now compile, LIMIT 0 included.
    let graph = Graph::sample();
    let probed = prepare(
        "MATCH (a)-[:R]->(b)-[:R]->(c) WHERE EXISTS { MATCH (c)-[:R]->(d) } RETURN a.p+c.p AS value",
    );
    assert_eq!(
        staged(&graph, &probed).unwrap(),
        graph.native(&probed).unwrap()
    );
    let pattern = PreparedGraphSet::from(leaf("MATCH (n) RETURN n.p AS value"));
    let spec = RowJoinSpec::cross(pattern.column_types(), pattern.column_types()).unwrap();
    let joined = pattern.clone().join(pattern.clone(), spec).unwrap();
    let nested = joined.clone().nested().unwrap();
    let joined = joined
        .project(
            vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let union = pattern
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, joined)
        .unwrap();
    for query in [nested, union] {
        assert!(AsyncSpillSetPlan::compile(&query).is_ok());
        assert!(AsyncSpillSetPlan::compile(&query.with_page(0, Some(0))).is_ok());
    }
}

#[test]
fn bound_parameters_and_cloned_definitions_outlive_the_template_and_arguments() {
    let template = PreparedGraphSetText::prepare(
        "MATCH (n) WITH n.p*$factor AS value ORDER BY value DESC SKIP $skip LIMIT $take RETURN {answer:value+$delta} AS result",
        symbols,
    ).unwrap();
    let params = GqlParameters::new()
        .with_int64("factor", 2)
        .unwrap()
        .with_uint64("skip", 1)
        .unwrap()
        .with_uint64("take", 2)
        .unwrap()
        .with_int64("delta", 10)
        .unwrap();
    let query = template.bind_parameters(&params).unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap().clone();
    drop(template);
    drop(params);
    assert_eq!(plan.columns(), &["result"]);
    assert!(
        plan.stages()
            .iter()
            .any(|stage| stage.offset() == 1 && stage.count() == Some(2))
    );
    let graph = Graph::sample();
    assert_eq!(
        staged(&graph, &query).unwrap(),
        graph.native(&query).unwrap()
    );
    let projected = PreparedGraphSet::from(leaf("MATCH (n) RETURN n.p AS value"))
        .project(
            vec![GraphSetProjection::new("renamed", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert_eq!(
        AsyncSpillSetPlan::compile(&projected).unwrap().columns(),
        &["renamed"]
    );
}

#[test]
fn fixed_hop_sources_preserve_computed_barriers_multiplicity_and_hidden_source_windows() {
    let graph = Graph::sample();
    for text in [
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r.p+s.p AS value ORDER BY value DESC",
        "MATCH (a)-[r:R]-(b)-[s:R]-(c) RETURN {from:a.p,total:r.p+s.p,to:c.p} AS value ORDER BY value LIMIT 5",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH s.p AS value ORDER BY value DESC SKIP 1 LIMIT 3 RETURN value*2 AS result",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH DISTINCT c.p AS value ORDER BY value DESC RETURN [value,value+1] AS result",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH s.p AS value WHERE value>9 RETURN value+1 AS result",
    ] {
        let query = prepare(text);
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        assert!(matches!(plan.source(), AsyncSpillSetSourcePlan::Join(_)));
        assert_eq!(
            staged(&graph, &query).unwrap(),
            graph.native(&query).unwrap(),
            "{text}"
        );
    }
    // A RETURN without ORDER BY yields canonical row order, not the WITH
    // stage's (owner ruling 2026-10-09), so the expected order is explicit.
    let query = prepare(
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH s.p AS value ORDER BY value DESC SKIP 1 LIMIT 3 \
         RETURN value*2 AS result ORDER BY result DESC",
    );
    assert_eq!(
        staged(&graph, &query).unwrap(),
        vec![
            row(vec![scalar(20)]),
            row(vec![scalar(20)]),
            row(vec![scalar(18)])
        ]
    );
    let source = leaf(
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN c.p AS value ORDER BY s.p DESC SKIP 1 LIMIT 3",
    );
    let query = PreparedGraphSet::from(source)
        .nested()
        .unwrap()
        .with_page(1, Some(1));
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    assert!(matches!(plan.source(), AsyncSpillSetSourcePlan::Join(_)));
    let tail = plan.source_tail().unwrap();
    assert!(tail.evaluation_width() > tail.visible_width());
    assert_eq!(tail.offset(), 1);
    assert_eq!(tail.count(), Some(3));
    assert_eq!(staged(&graph, &query).unwrap(), vec![row(vec![scalar(4)])]);
}

#[test]
fn intermediate_numeric_grouping_reuses_complete_native_rows_and_clause_boundaries() {
    let graph = Graph::sample();
    for text in [
        "MATCH (n) WITH n.p AS value,COUNT(*) AS copies WHERE copies>1 RETURN value,copies ORDER BY value",
        "MATCH (n) WITH n,COUNT(*) AS copies RETURN n.p AS value,copies ORDER BY value",
        "MATCH (a)-[:R]->(b) WITH a,COUNT(*) AS copies RETURN a,copies ORDER BY copies DESC,a",
        "UNWIND [3,1,3,NULL,2] AS value WITH value,COUNT(*) AS copies ORDER BY copies DESC,value LIMIT 2 RETURN value,copies ORDER BY value",
        "UNWIND [1,1,2,3,3] AS value WITH value,COUNT(*) AS copies WITH copies,COUNT(*) AS groups WITH SUM(groups) AS total RETURN total",
        "UNWIND [2,2,1] AS value WITH DISTINCT COUNT(*) AS copies UNWIND [copies,copies+1] AS item RETURN item",
        "UNWIND [1,1,2,NULL] AS value WITH COUNT(*) AS rows,COUNT(value) AS nonnull,COUNT(DISTINCT value) AS unique_count,SUM(value) AS total,SUM(DISTINCT value) AS unique_total,MIN(value) AS minimum,MAX(value) AS maximum RETURN rows,nonnull,unique_count,total,unique_total,minimum,maximum",
        "UNWIND [1.0,3.0] AS value WITH AVG(value) AS mean RETURN mean",
        "UNWIND [[2],[1],[2],NULL] AS value WITH value AS key,COUNT(*) AS copies RETURN key,copies",
        "UNWIND [[2],[1],[2],NULL] AS value WITH MIN(value) AS minimum,MAX(value) AS maximum RETURN minimum,maximum",
        "UNWIND [] AS value WITH COUNT(*) AS rows,SUM(value) AS total,AVG(value) AS mean RETURN rows,total,mean",
        "UNWIND [] AS value WITH value,COUNT(*) AS copies RETURN value,copies",
    ] {
        let query = prepare(text);
        let transcript = query.canonical_bytes();
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        assert!(plan.aggregate_count() != 0, "{text}");
        assert_eq!(
            staged_plan(&graph, &plan).unwrap(),
            graph.native(&query).unwrap(),
            "{text}"
        );
        assert_eq!(query.canonical_bytes(), transcript);
    }
    assert_eq!(
        staged(
            &graph,
            &prepare(
                "MATCH (n) WITH n.p AS value,COUNT(*) AS copies WHERE copies>1 RETURN value,copies"
            )
        )
        .unwrap(),
        vec![row(vec![scalar(1), scalar(2)])]
    );
    let repeated = prepare(
        "UNWIND [1,1,2,3,3] AS value WITH value,COUNT(*) AS copies WITH copies,COUNT(*) AS groups WITH SUM(groups) AS total RETURN total",
    );
    assert_eq!(
        AsyncSpillSetPlan::compile(&repeated)
            .unwrap()
            .aggregate_count(),
        3
    );
    assert_eq!(
        staged(&graph, &repeated).unwrap(),
        vec![row(vec![scalar(3)])]
    );
    assert_eq!(
        staged(&graph, &prepare("UNWIND [] AS value WITH COUNT(*) AS rows,SUM(value) AS total,AVG(value) AS mean RETURN rows,total,mean")).unwrap(),
        vec![row(vec![scalar(0),GraphValue::Scalar(CanonicalScalar::Null),GraphValue::Scalar(CanonicalScalar::Null)])]
    );
}

#[test]
fn aggregate_sources_retain_binary_children_own_pages_and_terminal_exact_summaries() {
    let graph = Graph::sample();
    let input = prepare(
        "(UNWIND [1,1,2,3] AS value RETURN value ORDER BY value DESC LIMIT 3) EXCEPT ALL UNWIND [1] AS value RETURN value",
    );
    let query = input
        .group_by(&[0], &[GraphAggregate::count_rows("copies")])
        .unwrap()
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(1, Some(1));
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Aggregate(aggregate) = plan.source() else {
        panic!("a group is its own complete-input source");
    };
    assert_eq!(plan.source().kind(), fgdb_gql::scan_stream::ScanKind::Set);
    assert_eq!(aggregate.columns(), &["value", "copies"]);
    assert_eq!(
        aggregate.column_types(),
        &[GraphSetColumnType::Any, GraphSetColumnType::Scalar]
    );
    assert!(matches!(
        aggregate.input().source(),
        AsyncSpillSetSourcePlan::Binary(_)
    ));
    assert_eq!(aggregate.definition().result_window(), (0, None));
    assert_eq!(plan.stages().len(), 1);
    assert_eq!(plan.stages()[0].offset(), 1);
    assert_eq!(plan.stages()[0].count(), Some(1));
    assert_eq!(plan.stage_count(), aggregate.input().stage_count() + 4);
    assert_eq!(plan.aggregate_count(), 1);
    assert_eq!(
        staged_plan(&graph, &plan).unwrap(),
        vec![row(vec![scalar(2), scalar(1)])]
    );
    assert_eq!(
        staged_plan(&graph, &plan).unwrap(),
        graph.native(&query).unwrap()
    );

    let binary = prepare(
        "UNWIND [1,1,2] AS value WITH value,COUNT(*) AS copies RETURN value,copies UNION ALL (UNWIND [1,3,3] AS value WITH value,COUNT(*) AS copies RETURN copies,value) ORDER BY value,copies",
    );
    let terminal = PreparedGraphSetAggregate::prepare(
        binary.clone(),
        &[],
        &[
            GraphAggregate::sum_int("total", 1),
            GraphAggregate::count_rows("groups"),
        ],
        0,
        None,
    )
    .unwrap();
    let physical =
        fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan::compile_relation(&terminal).unwrap();
    let fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan::Relation { input, definition } =
        physical
    else {
        panic!("terminal grouping must consume every completed binary child");
    };
    assert_eq!(input.aggregate_count(), 2);
    let rows = staged_plan(&graph, &input).unwrap();
    assert_eq!(
        rows,
        vec![
            row(vec![scalar(1), scalar(1)]),
            row(vec![scalar(1), scalar(2)]),
            row(vec![scalar(2), scalar(1)]),
            row(vec![scalar(3), scalar(2)])
        ]
    );
    let exact = reduce_groups(&definition, rows).unwrap();
    assert_eq!(
        exact[0].values(),
        &[
            GraphAggregateValue::Integer(6),
            GraphAggregateValue::Count(4)
        ]
    );
    assert_eq!(
        exact,
        terminal
            .execute_governed(
                policy(),
                |pattern, policy| graph.execute(pattern, policy),
                || Ok(())
            )
            .unwrap()
            .value
    );
    let grouped_again = binary
        .group_by(&[], &[GraphAggregate::sum_int("total", 1)])
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&grouped_again).unwrap();
    assert_eq!(plan.aggregate_count(), 3);
    assert_eq!(
        staged_plan(&graph, &plan).unwrap(),
        vec![row(vec![scalar(6)])]
    );
}

#[test]
fn complete_group_conversion_and_input_errors_precede_downstream_zero_pages() {
    let graph = Graph::sample();
    for text in [
        "UNWIND [9223372036854775807,1] AS value WITH SUM(value) AS total RETURN 1/(total-total) AS failure LIMIT 0",
        "UNWIND [{key:1,value:1},{key:2,value:9223372036854775807},{key:2,value:1}] AS record WITH record.key AS key,SUM(record.value) AS total ORDER BY key LIMIT 1 RETURN total",
        "UNWIND [1,0] AS value WITH SUM(1/value) AS total RETURN total LIMIT 0",
        "UNWIND [1,'bad'] AS value WITH SUM(value) AS total RETURN total LIMIT 0",
    ] {
        let query = prepare(text);
        let Err(GqlQueryError::Source(expected)) = graph.native(&query) else {
            panic!("{text}: expected complete-input failure");
        };
        assert_eq!(
            staged(&graph, &query),
            Err(SpillSetRowError::Native(expected)),
            "{text}"
        );
    }
    // An integer AVG converts to the binary64 nearest its exact mean
    // (fgdb-y255t), in the staged plan exactly as natively.
    let mean = |value: f64| {
        row(vec![GraphValue::Scalar(CanonicalScalar::Float(
            CanonicalF64::new(value),
        ))])
    };
    for (text, expected) in [
        (
            "UNWIND [1,2] AS value WITH AVG(value) AS mean RETURN mean",
            vec![mean(1.5)],
        ),
        (
            "UNWIND [2,2] AS value WITH AVG(value) AS mean RETURN mean",
            vec![mean(2.0)],
        ),
        (
            "UNWIND [1,2] AS value WITH AVG(value) AS mean RETURN mean LIMIT 0",
            vec![],
        ),
        (
            "UNWIND [] AS value RETURN value UNION ALL UNWIND [1,2,2] AS item WITH AVG(item) AS value RETURN value",
            vec![mean(5.0 / 3.0)],
        ),
    ] {
        let query = prepare(text);
        assert_eq!(graph.native(&query).unwrap(), expected, "{text}");
        assert_eq!(staged(&graph, &query).unwrap(), expected, "{text}");
    }
    let empty =
        prepare("UNWIND [] AS value WITH value,COUNT(*) AS copies RETURN 1/0 AS failure LIMIT 0");
    assert!(staged(&graph, &empty).unwrap().is_empty());
}

#[test]
fn aggregate_row_conversion_keeps_exact_domains_and_each_host_checkpoint() {
    let query = prepare("RETURN [1,2] AS key,1 AS value")
        .group_by(
            &[0],
            &[
                GraphAggregate::count_rows("count"),
                GraphAggregate::sum_int("sum", 1),
                GraphAggregate::min("minimum", 0),
            ],
        )
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Aggregate(aggregate) = plan.source() else {
        panic!("aggregate source");
    };
    let key = GraphValue::List(vec![scalar(1), scalar(2)].into_boxed_slice());
    let complete = GraphAggregateRow::from_group_values(
        vec![key.clone()],
        vec![
            GraphAggregateValue::Count(7),
            GraphAggregateValue::Integer(-9),
            GraphAggregateValue::Value(key.clone()),
        ],
    );
    let expected = row(vec![key.clone(), scalar(7), scalar(-9), key]);
    let mut events = Vec::new();
    let converted = aggregate
        .convert_row_with_control(&complete, &mut |event| {
            events.push(event);
            Ok::<_, usize>(())
        })
        .unwrap();
    aggregate
        .finish_conversion_with_control(&mut |event| {
            events.push(event);
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(converted, expected);
    assert_eq!(events.last(), Some(&GlaExecutionEvent::Work));
    assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
    for stop in 1..=events.len() {
        let mut seen = 0;
        let mut control = |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        };
        let result = (|| {
            let row = aggregate.convert_row_with_control(&complete, &mut control)?;
            aggregate.finish_conversion_with_control(&mut control)?;
            Ok(row)
        })();
        assert_eq!(result, Err(SpillSetAggregateRowError::Control(stop)));
        assert_eq!(seen, stop);
    }
    assert_eq!(
        aggregate
            .convert_row_with_control(&complete, &mut |_| Ok::<_, ()>(()))
            .unwrap(),
        expected
    );

    let query = prepare("RETURN 1 AS value")
        .group_by(&[], &[GraphAggregate::sum_int("total", 0)])
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Aggregate(aggregate) = plan.source() else {
        panic!("aggregate source");
    };
    for (value, expected) in [
        (
            GraphAggregateValue::Count(i64::MAX as u64),
            scalar(i64::MAX),
        ),
        (
            GraphAggregateValue::Integer(i128::from(i64::MIN)),
            scalar(i64::MIN),
        ),
        (
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Float(
                CanonicalF64::new(1.25),
            ))),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(1.25))),
        ),
        (
            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            GraphValue::Scalar(CanonicalScalar::Null),
        ),
        // A row has no exact rational: an average is the binary64 nearest
        // it (fgdb-y255t).
        (
            GraphAggregateValue::Average(GraphExactAverage::new(2, 1).unwrap()),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(2.0))),
        ),
        (
            GraphAggregateValue::Average(GraphExactAverage::new(-7, 3).unwrap()),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(-7.0 / 3.0))),
        ),
    ] {
        let complete = GraphAggregateRow::from_group_values(vec![], vec![value]);
        assert_eq!(
            aggregate
                .convert_row_with_control(&complete, &mut |_| Ok::<_, ()>(()))
                .unwrap(),
            row(vec![expected])
        );
    }
    for (value, kind) in [
        (
            GraphAggregateValue::Count(i64::MAX as u64 + 1),
            GraphIntegerErrorKind::Overflow,
        ),
        (
            GraphAggregateValue::Integer(i128::from(i64::MAX) + 1),
            GraphIntegerErrorKind::Overflow,
        ),
        (
            GraphAggregateValue::Integer(i128::from(i64::MIN) - 1),
            GraphIntegerErrorKind::Overflow,
        ),
    ] {
        let complete = GraphAggregateRow::from_group_values(vec![], vec![value]);
        assert!(
            matches!(aggregate.convert_row_with_control(&complete,&mut |_|Ok::<_,()>(())),Err(SpillSetAggregateRowError::Value(GraphAggregateError::OutputExpression{column:0,error})) if error.kind==kind)
        );
    }
    let wrong = GraphAggregateRow::from_group_values(vec![], vec![]);
    assert_eq!(
        aggregate.convert_row_with_control(&wrong, &mut |_| Ok::<_, ()>(())),
        Err(SpillSetAggregateRowError::Value(
            GraphAggregateError::InvalidReductionInput
        ))
    );
    assert_eq!(
        aggregate.finish_conversion_with_control(&mut |_| Err(7)),
        Err(SpillSetAggregateRowError::Control(7))
    );
}

#[test]
fn collection_and_unsupported_group_descendants_still_refuse_before_zero_limit() {
    let input = prepare("RETURN 1 AS value");
    let collection = input
        .clone()
        .group_by(&[], &[GraphAggregate::collect("items", 0)])
        .unwrap();
    // COLLECT groups spill under the byte-bounded collector (b9ddb0b0), as a
    // source and below a nested boundary, LIMIT 0 included.
    assert!(AsyncSpillSetPlan::compile(&collection.clone().with_page(0, Some(0))).is_ok());
    let nested = collection.nested().unwrap().with_page(0, Some(0));
    assert!(AsyncSpillSetPlan::compile(&nested).is_ok());
    // A relational join below a group was the unsupported descendant until
    // d3574354 executed joins over completed child runs; it now compiles.
    let spec = RowJoinSpec::cross(input.column_types(), input.column_types()).unwrap();
    let joined = input
        .clone()
        .join(input, spec)
        .unwrap()
        .group_by(&[], &[GraphAggregate::count_rows("count")])
        .unwrap()
        .with_page(0, Some(0));
    assert!(AsyncSpillSetPlan::compile(&joined).is_ok());
}

#[test]
fn cartesian_sources_preserve_child_occurrences_pages_and_zero_column_products() {
    let graph = Graph::sample();
    let left = prepare("UNWIND [3,1,3,NULL] AS a RETURN a").with_page(0, Some(3));
    let right = prepare("UNWIND [9,7,9,8] AS b RETURN b").with_page(1, Some(2));
    let query = left.clone().cross_join(right.clone()).unwrap();
    let transcript = query.canonical_bytes();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
        panic!("a Cartesian product owns both complete children");
    };
    assert_eq!(cross.columns(), &["a", "b"]);
    assert_eq!(
        cross.column_types(),
        &[GraphSetColumnType::Any, GraphSetColumnType::Any]
    );
    assert_eq!(plan.columns(), cross.columns());
    assert_eq!(plan.column_types(), cross.column_types());
    assert_eq!(plan.stages().len(), 1);
    assert_eq!(plan.stages()[0].order(), None);
    assert!(!plan.stages()[0].distinct());
    assert_eq!(
        plan.stage_count(),
        cross.left().stage_count() + cross.right().stage_count() + 3
    );
    assert_eq!(plan.aggregate_count(), 0);
    let expected: Vec<_> = [3, 1, 3]
        .into_iter()
        .flat_map(|a| [7, 9].map(|b| row(vec![scalar(a), scalar(b)])))
        .collect();
    assert_eq!(staged_plan(&graph, &plan).unwrap(), expected);
    assert_eq!(graph.native(&query).unwrap(), expected);
    assert_eq!(query.canonical_bytes(), transcript);

    for skip in [0, 1, 4, 6, u64::MAX] {
        for take in [0, 1, 3, 9] {
            let paged = query.clone().with_page(skip, Some(take));
            let expected: Vec<_> = expected
                .iter()
                .skip(usize::try_from(skip).unwrap_or(usize::MAX))
                .take(take as usize)
                .cloned()
                .collect();
            assert_eq!(staged(&graph, &paged).unwrap(), expected);
            assert_eq!(graph.native(&paged).unwrap(), expected);
        }
    }
    let order = [
        GraphValueOrder::descending(1),
        GraphValueOrder::ascending(0),
    ];
    let ordered = query
        .clone()
        .with_order_by(&order)
        .unwrap()
        .with_page(1, Some(3));
    let mut ranked = expected.clone();
    ranked.sort_by(|a, b| compare(a, b, &order));
    assert_eq!(staged(&graph, &ordered).unwrap(), ranked[1..4]);
    assert_eq!(
        staged(&graph, &ordered).unwrap(),
        graph.native(&ordered).unwrap()
    );

    let unit = PreparedGraphSet::singleton();
    for identity in [
        unit.clone().cross_join(left.clone()).unwrap(),
        left.clone().cross_join(unit.clone()).unwrap(),
    ] {
        assert_eq!(
            staged(&graph, &identity).unwrap(),
            graph.native(&left).unwrap()
        );
    }
    let twice = unit
        .clone()
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            unit.clone(),
        )
        .unwrap();
    let three = twice
        .clone()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, unit)
        .unwrap();
    let units = twice.cross_join(three).unwrap();
    let unit_plan = AsyncSpillSetPlan::compile(&units).unwrap();
    assert!(unit_plan.source_columns().is_empty());
    assert!(unit_plan.column_types().is_empty());
    assert_eq!(
        staged_plan(&graph, &unit_plan).unwrap(),
        vec![GraphValueRow::from_owned_values(Vec::new()); 6]
    );
    assert_eq!(
        graph.native(&units).unwrap(),
        vec![GraphValueRow::from_owned_values(Vec::new()); 6]
    );
    assert_eq!(
        staged(&graph, &units.with_page(4, Some(1))).unwrap(),
        vec![GraphValueRow::from_owned_values(Vec::new())]
    );

    let nested = left
        .cross_join(
            right
                .cross_join(prepare("UNWIND [6,5] AS c RETURN c"))
                .unwrap(),
        )
        .unwrap();
    let expected: Vec<_> = expected
        .iter()
        .flat_map(|pair| {
            [6, 5].map(|c| {
                let mut values = pair.values().to_vec();
                values.push(scalar(c));
                row(values)
            })
        })
        .collect();
    assert_eq!(staged(&graph, &nested).unwrap(), expected);
    assert_eq!(graph.native(&nested).unwrap(), expected);
}

#[test]
fn leading_unwind_match_uses_native_correlations_property_reads_and_edge_bags() {
    let graph = Graph::sample();
    for (text, expected) in [
        (
            "UNWIND [2,1,2,NULL] AS wanted MATCH (n {p:wanted}) RETURN wanted,wanted AS value",
            vec![
                row(vec![scalar(2), scalar(2)]),
                row(vec![scalar(1), scalar(1)]),
                row(vec![scalar(1), scalar(1)]),
                row(vec![scalar(2), scalar(2)]),
            ],
        ),
        (
            "UNWIND [2,1,2,NULL] AS wanted MATCH (n) WITH wanted,n.p AS value WHERE wanted=value RETURN value,wanted",
            vec![
                row(vec![scalar(2), scalar(2)]),
                row(vec![scalar(1), scalar(1)]),
                row(vec![scalar(1), scalar(1)]),
                row(vec![scalar(2), scalar(2)]),
            ],
        ),
        (
            "UNWIND [1,1] AS wanted MATCH (n {p:wanted}) WITH DISTINCT wanted,n.p AS value RETURN wanted,value",
            vec![row(vec![scalar(1), scalar(1)])],
        ),
        (
            "UNWIND [3,1] AS wanted MATCH (n {p:wanted}) WITH wanted,n.q AS rank ORDER BY rank DESC LIMIT 2 RETURN wanted,rank",
            vec![
                row(vec![scalar(1), scalar(4)]),
                row(vec![scalar(1), scalar(2)]),
            ],
        ),
        (
            "UNWIND [3,1,3] AS wanted MATCH (a {p:wanted})-[:R]->(b) WITH wanted,a.q AS source,b.q AS target RETURN wanted,source,target",
            vec![
                row(vec![scalar(3), scalar(1), scalar(2)]),
                row(vec![scalar(3), scalar(1), scalar(2)]),
                row(vec![scalar(1), scalar(2), scalar(2)]),
                row(vec![scalar(1), scalar(2), scalar(3)]),
                row(vec![scalar(3), scalar(1), scalar(2)]),
                row(vec![scalar(3), scalar(1), scalar(2)]),
            ],
        ),
        (
            "UNWIND [1,2] AS wanted MATCH (n {p:wanted}) WITH n.p AS value,COUNT(*) AS copies RETURN value,copies",
            vec![
                row(vec![scalar(1), scalar(2)]),
                row(vec![scalar(2), scalar(1)]),
            ],
        ),
        ("UNWIND [] AS wanted MATCH (n) RETURN wanted,n", vec![]),
    ] {
        let query = prepare(text);
        let transcript = query.canonical_bytes();
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        assert_eq!(
            plan.source().kind(),
            fgdb_gql::scan_stream::ScanKind::Set,
            "{text}"
        );
        assert_eq!(staged_plan(&graph, &plan).unwrap(), expected, "{text}");
        assert_eq!(graph.native(&query).unwrap(), expected, "{text}");
        assert_eq!(query.canonical_bytes(), transcript);
    }
}

#[test]
fn cartesian_sources_compose_with_complete_binary_and_numeric_group_relations() {
    let graph = Graph::sample();
    let left =
        prepare("UNWIND [2,2,1] AS key WITH key,COUNT(*) AS left_count RETURN key,left_count");
    let right = prepare(
        "UNWIND [5,5,4] AS other WITH other,COUNT(*) AS right_count RETURN other,right_count",
    );
    let query = left.cross_join(right).unwrap();
    let expected = vec![
        row(vec![scalar(1), scalar(1), scalar(4), scalar(1)]),
        row(vec![scalar(1), scalar(1), scalar(5), scalar(2)]),
        row(vec![scalar(2), scalar(2), scalar(4), scalar(1)]),
        row(vec![scalar(2), scalar(2), scalar(5), scalar(2)]),
    ];
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    assert_eq!(plan.aggregate_count(), 2);
    assert_eq!(staged_plan(&graph, &plan).unwrap(), expected);
    assert_eq!(graph.native(&query).unwrap(), expected);

    let grouped = query
        .clone()
        .group_by(
            &[0],
            &[
                GraphAggregate::count_rows("combinations"),
                GraphAggregate::sum_int("weight", 3),
            ],
        )
        .unwrap();
    assert_eq!(
        AsyncSpillSetPlan::compile(&grouped)
            .unwrap()
            .aggregate_count(),
        3
    );
    assert_eq!(
        staged(&graph, &grouped).unwrap(),
        vec![
            row(vec![scalar(1), scalar(2), scalar(3)]),
            row(vec![scalar(2), scalar(2), scalar(3)])
        ]
    );
    assert_eq!(
        staged(&graph, &grouped).unwrap(),
        graph.native(&grouped).unwrap()
    );

    let doubled = query
        .clone()
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            query.clone(),
        )
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&doubled).unwrap();
    assert_eq!(plan.aggregate_count(), 4);
    let mut expected: Vec<_> = expected.iter().chain(&expected).cloned().collect();
    expected.sort();
    assert_eq!(staged_plan(&graph, &plan).unwrap(), expected);
    assert_eq!(graph.native(&doubled).unwrap(), expected);

    let binary = prepare(
        "(UNWIND [9,7,9] AS item RETURN item ORDER BY item DESC LIMIT 2) EXCEPT ALL RETURN 9 AS item",
    );
    let before_binary = prepare("UNWIND [2,1] AS key RETURN key")
        .cross_join(binary)
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&before_binary).unwrap();
    let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
        panic!("the complete set operation is one Cartesian child");
    };
    assert!(matches!(
        cross.right().source(),
        AsyncSpillSetSourcePlan::Binary(_)
    ));
    assert_eq!(
        staged_plan(&graph, &plan).unwrap(),
        vec![
            row(vec![scalar(2), scalar(9)]),
            row(vec![scalar(1), scalar(9)])
        ]
    );
    assert_eq!(
        staged_plan(&graph, &plan).unwrap(),
        graph.native(&before_binary).unwrap()
    );

    let summary = PreparedGraphSetAggregate::prepare(
        query,
        &[],
        &[
            GraphAggregate::count_rows("pairs"),
            GraphAggregate::sum_int("weight", 1),
        ],
        0,
        None,
    )
    .unwrap();
    let fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan::Relation { input, definition } =
        fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan::compile_relation(&summary).unwrap()
    else {
        panic!("terminal summary retains the whole Cartesian relation");
    };
    assert!(matches!(input.source(), AsyncSpillSetSourcePlan::Cross(_)));
    let output = reduce_groups(&definition, staged_plan(&graph, &input).unwrap()).unwrap();
    assert_eq!(
        output[0].values(),
        &[
            GraphAggregateValue::Count(4),
            GraphAggregateValue::Integer(6)
        ]
    );
    let native = summary
        .execute_governed(
            policy(),
            |pattern, policy| graph.execute(pattern, policy),
            || Ok(()),
        )
        .unwrap();
    assert_eq!(output, native.value);
}

#[test]
fn cartesian_children_finish_before_empty_products_and_downstream_zero_pages() {
    let graph = Graph::sample();
    let empty = prepare("UNWIND [] AS item RETURN item");
    for text in [
        "UNWIND [1,0] AS value RETURN 1/value AS bad",
        "UNWIND [[1],2] AS values UNWIND values AS item RETURN item AS bad",
        "UNWIND [9223372036854775807,1] AS value WITH SUM(value) AS bad RETURN bad",
        "UNWIND [2,'bad'] AS value WITH AVG(value) AS bad RETURN bad",
    ] {
        let bad = prepare(text);
        for query in [
            empty.clone().cross_join(bad.clone()).unwrap(),
            bad.clone().cross_join(empty.clone()).unwrap(),
            prepare("RETURN 1 AS item").cross_join(bad.clone()).unwrap(),
        ] {
            let query = query.with_page(0, Some(0));
            let GqlQueryError::Source(expected) = graph.native(&query).unwrap_err() else {
                panic!("{text}: the original native child refusal must survive");
            };
            assert_eq!(
                staged(&graph, &query),
                Err(SpillSetRowError::Native(expected)),
                "{text}"
            );
        }
    }

    let product = prepare("UNWIND [1,0] AS left RETURN left")
        .cross_join(prepare("UNWIND [4,3] AS right RETURN right"))
        .unwrap();
    let divide = fgdb_gql::GraphIntegerExpression::prepare(&[
        fgdb_gql::GraphIntegerOp::Literal(Some(1)),
        fgdb_gql::GraphIntegerOp::Column(0),
        fgdb_gql::GraphIntegerOp::Binary(fgdb_gql::GraphIntegerBinary::Divide),
    ])
    .unwrap();
    let invalid = product
        .project(
            vec![GraphSetProjection::new(
                "bad",
                GraphSetValue::Integer(divide),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    let GqlQueryError::Source(expected) = graph.native(&invalid).unwrap_err() else {
        panic!("downstream projection must inspect the complete left-major product");
    };
    assert!(matches!(
        expected,
        GraphSetExecutionError::Projection {
            row: 2,
            column: 0,
            ..
        }
    ));
    assert_eq!(
        staged(&graph, &invalid),
        Err(SpillSetRowError::Native(expected))
    );

    // Once a child has genuinely selected no rows, a later expression has no
    // inputs. This is distinct from skipping that child's own evaluation.
    let no_rows = empty
        .cross_join(prepare("RETURN 1 AS value"))
        .unwrap()
        .project(
            vec![GraphSetProjection::new(
                "bad",
                GraphSetValue::Integer(
                    fgdb_gql::GraphIntegerExpression::prepare(&[
                        fgdb_gql::GraphIntegerOp::Literal(Some(1)),
                        fgdb_gql::GraphIntegerOp::Literal(Some(0)),
                        fgdb_gql::GraphIntegerOp::Binary(fgdb_gql::GraphIntegerBinary::Divide),
                    ])
                    .unwrap(),
                ),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap();
    assert_eq!(
        staged(&graph, &no_rows).unwrap(),
        Vec::<GraphValueRow>::new()
    );
    assert!(graph.native(&no_rows).unwrap().is_empty());
}

#[test]
fn cartesian_pair_copy_preserves_original_domains_and_every_allocation_checkpoint() {
    let query = prepare("RETURN 1 AS number,[2] AS list,{payload:[3]} AS any")
        .cross_join(leaf("MATCH (a)-[r:R]->(b) RETURN a,r,b").into())
        .unwrap();
    let plan = AsyncSpillSetPlan::compile(&query).unwrap();
    let AsyncSpillSetSourcePlan::Cross(cross) = plan.source() else {
        panic!("pair admission requires the sealed Cartesian schema");
    };
    assert_eq!(
        cross.column_types(),
        &[
            GraphSetColumnType::Scalar,
            GraphSetColumnType::List,
            GraphSetColumnType::Any,
            GraphSetColumnType::Vertex,
            GraphSetColumnType::Edge,
            GraphSetColumnType::Vertex
        ]
    );
    let left = row(vec![
        GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(1.25))),
        GraphValue::List(
            vec![scalar(2), GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice(),
        ),
        GraphValue::map(vec![(
            "payload".into(),
            GraphValue::List(
                vec![
                    GraphValue::Scalar(
                        CanonicalScalar::ucs_basic_text(&"owned payload ".repeat(80)).unwrap(),
                    ),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ]
                .into_boxed_slice(),
            ),
        )])
        .unwrap(),
    ]);
    let right = row(vec![
        GraphValue::Vertex(VId(1)),
        GraphValue::Edge(EId(7)),
        GraphValue::Vertex(VId(2)),
    ]);
    let expected = row(left
        .values()
        .iter()
        .chain(right.values())
        .cloned()
        .collect());
    let mut events = Vec::new();
    let actual = cross
        .copy_pair_with_control(&left, &right, &mut |event| {
            events.push(event);
            Ok::<_, &'static str>(())
        })
        .unwrap();
    assert_eq!(actual, expected);
    assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
    for refusal in 0..events.len() {
        let mut observed = Vec::new();
        let result = cross.copy_pair_with_control(&left, &right, &mut |event| {
            observed.push(event);
            if observed.len() == refusal + 1 {
                Err("pair allocation or cancellation refused")
            } else {
                Ok(())
            }
        });
        assert_eq!(
            result,
            Err(SpillSetRowError::Control(
                "pair allocation or cancellation refused"
            ))
        );
        assert_eq!(observed, events[..=refusal]);
    }
    let nullable = row(vec![GraphValue::Scalar(CanonicalScalar::Null); 3]);
    assert_eq!(
        cross
            .copy_pair_with_control(&nullable, &nullable, &mut |_| Ok::<_, ()>(()))
            .unwrap(),
        row(vec![GraphValue::Scalar(CanonicalScalar::Null); 6])
    );
    let mut any = left.values().to_vec();
    any[2] = GraphValue::Vertex(VId(99));
    assert_eq!(
        cross
            .copy_pair_with_control(&row(any.clone()), &right, &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .values()[2],
        any[2]
    );
    for (left, right, operand) in [
        (
            GraphValueRow::from_owned_values(Vec::new()),
            right.clone(),
            0,
        ),
        (
            left.clone(),
            GraphValueRow::from_owned_values(Vec::new()),
            1,
        ),
        (
            row(vec![
                GraphValue::List(vec![].into_boxed_slice()),
                left.values()[1].clone(),
                left.values()[2].clone(),
            ]),
            right.clone(),
            0,
        ),
        (
            row(vec![scalar(1), scalar(2), left.values()[2].clone()]),
            right.clone(),
            0,
        ),
        (
            left.clone(),
            row(vec![
                GraphValue::Vertex(VId(1)),
                scalar(7),
                GraphValue::Vertex(VId(2)),
            ]),
            1,
        ),
    ] {
        let mut scratch = 0;
        let result = cross.copy_pair_with_control(&left, &right, &mut |event| {
            scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, ()>(())
        });
        assert_eq!(
            result,
            Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand }
            ))
        );
        assert_eq!(
            scratch, 0,
            "both schemas must be admitted before pair allocation"
        );
    }
}

#[test]
fn cartesian_oversized_rows_and_unsupported_children_refuse_before_empty_pages() {
    let width = fgdb_gql::algebra::MAX_PATTERN_VERTICES;
    let wide = PreparedGraphSet::singleton()
        .project(
            (0..width)
                .map(|at| {
                    GraphSetProjection::new(
                        format!("c{at}"),
                        GraphSetValue::Value(scalar(at as i64)),
                    )
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap();
    let oversized = wide.cross_join(prepare("RETURN 1 AS extra")).unwrap();
    assert!(matches!(
        AsyncSpillSetPlan::compile(&oversized.clone().with_page(0, Some(0))),
        Err(SpillSetBuildError::Unsupported { depth: 0 })
    ));
    let hidden = oversized
        .project(
            vec![GraphSetProjection::new("only", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&hidden),
        Err(SpillSetBuildError::Unsupported { depth: 1 })
    ));

    let input = prepare("RETURN 1 AS value");
    let collection = input
        .clone()
        .group_by(&[], &[GraphAggregate::collect("items", 0)])
        .unwrap();
    let spec = RowJoinSpec::cross(input.column_types(), input.column_types()).unwrap();
    let relational_join = input.clone().join(input.clone(), spec).unwrap();
    // Relational joins execute over completed child runs since d3574354 and
    // COLLECT groups spill since b9ddb0b0, so a product over either compiles,
    // directly and below a nested boundary.
    for product in [
        input
            .clone()
            .with_page(0, Some(0))
            .cross_join(relational_join.clone())
            .unwrap(),
        relational_join
            .cross_join(input.clone().with_page(0, Some(0)))
            .unwrap(),
    ] {
        assert!(AsyncSpillSetPlan::compile(&product.clone().with_page(0, Some(0))).is_ok());
    }
    for product in [
        input
            .clone()
            .with_page(0, Some(0))
            .cross_join(collection.clone())
            .unwrap(),
        collection
            .cross_join(input.clone().with_page(0, Some(0)))
            .unwrap(),
    ] {
        assert!(AsyncSpillSetPlan::compile(&product.clone().with_page(0, Some(0))).is_ok());
        assert!(
            AsyncSpillSetPlan::compile(&product.nested().unwrap().with_page(0, Some(0))).is_ok()
        );
    }
}

mod async_elements {
    use super::*;
    use fgdb_delta_types::{ElementId, LabelId};
    use fgdb_gql::spill_set::AsyncSpillElementSource;
    use fgdb_gql::{
        GraphElementKey, GraphElementMetadata, GraphSymbolResolver, ReverseSymbolCatalog,
    };
    use std::borrow::Borrow;
    use std::future::{Future, poll_fn};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Effect {
        Event(GlaExecutionEvent),
        Read(ElementId, PropertyKeyId),
    }
    type Trace = Arc<Mutex<Vec<Effect>>>;

    struct Property {
        value: CanonicalPropertyValue,
        live: Arc<AtomicUsize>,
    }
    impl Borrow<CanonicalPropertyValue> for Property {
        fn borrow(&self) -> &CanonicalPropertyValue {
            &self.value
        }
    }
    impl Drop for Property {
        fn drop(&mut self) {
            self.live.fetch_sub(1, AtomicOrdering::SeqCst);
        }
    }

    #[derive(Default)]
    struct Reader {
        trace: Trace,
        live: Arc<AtomicUsize>,
        refuse: Option<ElementId>,
        unavailable: bool,
        stored: Option<CanonicalPropertyValue>,
    }
    impl AsyncSpillElementSource for Reader {
        type Error = &'static str;
        type Property = Property;

        async fn element_property(
            &mut self,
            element: ElementId,
            key: PropertyKeyId,
        ) -> Option<Result<Option<Property>, &'static str>> {
            self.trace.lock().unwrap().push(Effect::Read(element, key));
            if self.unavailable {
                return None;
            }
            if key != P {
                return Some(Err("unselected property was read"));
            }
            // Independent fixture lookup, with no dependence on the evaluator
            // or a narrowing conversion of the full-width graph identity.
            let value = match element {
                ElementId::Vertex(VId(1)) => Some(2),
                ElementId::Vertex(VId(u128::MAX)) => Some(7),
                ElementId::Edge(EId(u128::MAX)) => Some(11),
                _ => None,
            };
            let property = value.map(|value| {
                self.live.fetch_add(1, AtomicOrdering::SeqCst);
                Property {
                    value: self
                        .stored
                        .clone()
                        .unwrap_or_else(|| CanonicalScalar::Int(value).into()),
                    live: Arc::clone(&self.live),
                }
            });
            // Hold an actual source guard across one suspension. Cancellation
            // must drop it even before the reader returns its scalar.
            let mut suspended = false;
            poll_fn(|cx| {
                if suspended {
                    Poll::Ready(())
                } else {
                    suspended = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            Some(if self.refuse == Some(element) {
                Err("original source failure")
            } else {
                Ok(property)
            })
        }
    }

    #[derive(Default)]
    struct ControlledReader {
        source: Reader,
    }
    impl AsyncSpillElementSource for ControlledReader {
        type Error = &'static str;
        type Property = Property;

        async fn element_property(
            &mut self,
            _: ElementId,
            _: PropertyKeyId,
        ) -> Option<Result<Option<Property>, &'static str>> {
            panic!("the evaluator must pass its original control to the source");
        }

        async fn element_property_with_control(
            &mut self,
            element: ElementId,
            key: PropertyKeyId,
            control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), Self::Error> + Send),
        ) -> Option<Result<Option<Property>, Self::Error>> {
            for event in [
                GlaExecutionEvent::Work,
                GlaExecutionEvent::ScratchEntry,
                GlaExecutionEvent::Work,
            ] {
                if let Err(error) = control(event) {
                    return Some(Err(error));
                }
            }
            self.source.element_property(element, key).await
        }
    }

    struct MetadataCatalog;
    impl GraphSymbolResolver for MetadataCatalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            symbols(kind, name)
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_label(LabelId(1), "Article");
            catalog.insert_label(LabelId(2), "Featured");
            catalog.insert_relation(R, "LINK");
            catalog.insert_property(P, "p");
            Some(catalog)
        }
    }

    #[derive(Default)]
    struct MetadataReader {
        calls: Vec<(ElementId, GraphElementMetadata)>,
        live: Arc<AtomicUsize>,
        refuse: bool,
    }
    struct MetadataGuard {
        value: Option<GraphValue>,
        live: Arc<AtomicUsize>,
    }
    impl Drop for MetadataGuard {
        fn drop(&mut self) {
            self.live.fetch_sub(1, AtomicOrdering::SeqCst);
        }
    }

    fn metadata_text<E>(
        name: &str,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValue, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        for _ in 0..name
            .len()
            .div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
        {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        Ok(GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(name).unwrap(),
        ))
    }

    impl AsyncSpillElementSource for MetadataReader {
        type Error = &'static str;
        type Property = Property;

        async fn element_property(
            &mut self,
            _: ElementId,
            _: PropertyKeyId,
        ) -> Option<Result<Option<Property>, Self::Error>> {
            Some(Err("metadata fixture received a per-key read"))
        }

        async fn element_metadata_with_control(
            &mut self,
            element: ElementId,
            kind: GraphElementMetadata,
            catalog: &ReverseSymbolCatalog,
            control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), Self::Error> + Send),
        ) -> Option<Result<Option<GraphValue>, Self::Error>> {
            self.calls.push((element, kind));
            let (number, labels, relation) = match element {
                ElementId::Vertex(VId(1)) => (2, &[LabelId(1), LabelId(2)][..], None),
                ElementId::Vertex(VId(u128::MAX)) => (7, &[LabelId(2)][..], None),
                ElementId::Edge(EId(u128::MAX)) => (11, &[][..], Some(R)),
                _ => return Some(Ok(None)),
            };
            let value = (|| {
                control(GlaExecutionEvent::ScratchEntry)?;
                Ok(match kind {
                    GraphElementMetadata::Labels => GraphValue::List(
                        labels
                            .iter()
                            .map(|id| metadata_text(&catalog.labels[id], control))
                            .collect::<Result<Vec<_>, _>>()?
                            .into_boxed_slice(),
                    ),
                    GraphElementMetadata::Type => {
                        metadata_text(&catalog.relations[&relation.unwrap()], control)?
                    }
                    GraphElementMetadata::Keys => GraphValue::List(
                        vec![metadata_text(&catalog.properties[&P], control)?].into_boxed_slice(),
                    ),
                    GraphElementMetadata::Properties => {
                        let name = &catalog.properties[&P];
                        for _ in 0..=name
                            .len()
                            .div_ceil(fgdb_gql::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
                        {
                            control(GlaExecutionEvent::ScratchEntry)?;
                        }
                        control(GlaExecutionEvent::ScratchEntry)?;
                        GraphValue::map(vec![(name.as_str().into(), scalar(number))]).unwrap()
                    }
                })
            })();
            let value = match value {
                Ok(value) => value,
                Err(error) => return Some(Err(error)),
            };
            self.live.fetch_add(1, AtomicOrdering::SeqCst);
            let mut guard = MetadataGuard {
                value: Some(value),
                live: Arc::clone(&self.live),
            };
            // The catalog and the source guard both survive a real suspension.
            let mut suspended = false;
            poll_fn(|cx| {
                if suspended {
                    Poll::Ready(())
                } else {
                    suspended = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
            Some(if self.refuse {
                Err("metadata source refused")
            } else {
                Ok(guard.value.take())
            })
        }
    }

    fn metadata_stage(expression: &str) -> SpillSetStage {
        let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            &format!("MATCH (n) WITH [n] AS items RETURN {expression} AS result"),
            &[],
            &mut MetadataCatalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        AsyncSpillSetPlan::compile(&query)
            .unwrap()
            .stages()
            .last()
            .unwrap()
            .clone()
    }

    #[derive(Default)]
    struct FrameState {
        current: AtomicUsize,
        peak: AtomicUsize,
        attempts: AtomicUsize,
    }
    struct Frames {
        state: Arc<FrameState>,
        limit: usize,
        refuse: Option<usize>,
    }
    struct Grant {
        state: Arc<FrameState>,
        bytes: usize,
    }
    impl Drop for Grant {
        fn drop(&mut self) {
            self.state
                .current
                .fetch_sub(self.bytes, AtomicOrdering::SeqCst);
        }
    }
    impl Frames {
        fn new(limit: usize, refuse: Option<usize>) -> Self {
            Self {
                state: Arc::default(),
                limit,
                refuse,
            }
        }
        fn reserve(&self, bytes: usize) -> Result<Grant, &'static str> {
            let attempt = self.state.attempts.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let next = self
                .state
                .current
                .load(AtomicOrdering::SeqCst)
                .checked_add(bytes)
                .ok_or("frame refused")?;
            if self.refuse == Some(attempt) || next > self.limit {
                return Err("frame refused");
            }
            self.state.current.store(next, AtomicOrdering::SeqCst);
            self.state.peak.fetch_max(next, AtomicOrdering::SeqCst);
            Ok(Grant {
                state: Arc::clone(&self.state),
                bytes,
            })
        }
        fn assert_released(&self, reader: &Reader) {
            assert_eq!(self.state.current.load(AtomicOrdering::SeqCst), 0);
            assert_eq!(reader.live.load(AtomicOrdering::SeqCst), 0);
        }
    }

    fn control(
        trace: Trace,
        stop: Option<usize>,
    ) -> impl FnMut(GlaExecutionEvent) -> Result<(), &'static str> + Send {
        let mut events = 0;
        move |event| {
            trace.lock().unwrap().push(Effect::Event(event));
            events += 1;
            if stop == Some(events) {
                Err("native event refused")
            } else {
                Ok(())
            }
        }
    }

    fn finish<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..32 {
            if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
                return result;
            }
        }
        panic!("finite fixture failed to make progress");
    }

    fn run(
        stage: &SpillSetStage,
        input: GraphValueRow,
        reader: &mut Reader,
        frames: &Frames,
        stop: Option<usize>,
    ) -> Result<Vec<GraphValueRow>, SpillSetRowError<&'static str>> {
        let mut control = control(Arc::clone(&reader.trace), stop);
        run_source(stage, input, reader, frames, &mut control)
    }

    fn run_source<S>(
        stage: &SpillSetStage,
        input: GraphValueRow,
        reader: &mut S,
        frames: &Frames,
        control: &mut (impl FnMut(GlaExecutionEvent) -> Result<(), &'static str> + Send),
    ) -> Result<Vec<GraphValueRow>, SpillSetRowError<&'static str>>
    where
        S: AsyncSpillElementSource<Error = &'static str>,
    {
        let reserve = |bytes| frames.reserve(bytes);
        finish(async {
            let mut cursor = stage
                .start_with_async_elements(input, 17, control, reader, &reserve)
                .await?;
            let mut rows = Vec::new();
            while let Some((row, next)) = cursor.next_with_control(control)? {
                rows.push(row);
                cursor = next;
            }
            Ok(rows)
        })
    }

    #[test]
    fn controlled_sources_use_the_same_native_meter_and_preserve_short_circuiting() {
        let expression = stage("[x IN items | x.p]");
        let frames = Frames::new(usize::MAX, None);
        let mut baseline = Reader::default();
        let expected_rows = run(&expression, input(), &mut baseline, &frames, None).unwrap();
        let baseline_events = baseline
            .trace
            .lock()
            .unwrap()
            .iter()
            .filter(|effect| matches!(effect, Effect::Event(_)))
            .count();
        let mut reader = ControlledReader::default();
        let mut meter = control(Arc::clone(&reader.source.trace), None);
        let rows = run_source(&expression, input(), &mut reader, &frames, &mut meter).unwrap();
        assert_eq!(rows, expected_rows);
        let expected = reader.source.trace.lock().unwrap().clone();
        assert_eq!(
            expected
                .iter()
                .filter(|effect| matches!(effect, Effect::Event(_)))
                .count(),
            baseline_events + 3 * identities().len()
        );
        assert_eq!(reads(&expected), identities().map(|element| (element, P)));
        frames.assert_released(&reader.source);
        let mut ordinal = 0;
        for (at, effect) in expected.iter().enumerate() {
            if !matches!(effect, Effect::Event(_)) {
                continue;
            }
            ordinal += 1;
            let mut reader = ControlledReader::default();
            let mut meter = control(Arc::clone(&reader.source.trace), Some(ordinal));
            assert_eq!(
                run_source(&expression, input(), &mut reader, &frames, &mut meter),
                Err(SpillSetRowError::Control("native event refused"))
            );
            assert_eq!(*reader.source.trace.lock().unwrap(), expected[..=at]);
            frames.assert_released(&reader.source);
        }
        for (expression, expected, reached) in [
            ("CASE WHEN true THEN 7 ELSE items[0].p END", 7, false),
            ("coalesce(7,items[0].p)", 7, false),
            ("coalesce(null,items[0].p)", 2, true),
        ] {
            let mut reader = ControlledReader::default();
            let mut meter = control(Arc::clone(&reader.source.trace), None);
            assert_eq!(
                run_source(
                    &stage(expression),
                    input(),
                    &mut reader,
                    &frames,
                    &mut meter
                )
                .unwrap(),
                vec![row(vec![scalar(expected)])],
                "{expression}"
            );
            assert_eq!(
                reads(&reader.source.trace.lock().unwrap()),
                if reached {
                    vec![(identities()[0], P)]
                } else {
                    Vec::new()
                },
                "{expression}"
            );
            frames.assert_released(&reader.source);
        }
    }

    #[test]
    fn stored_collections_admit_once_while_the_source_guard_is_live() {
        use fgdb_types::{CanonicalList, CanonicalMap, CanonicalMapEntry, CanonicalText};

        let stored = CanonicalPropertyValue::Map(
            CanonicalMap::try_new(vec![CanonicalMapEntry::new(
                CanonicalText::new_ucs_basic(&"key".repeat(65)).unwrap(),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![
                        CanonicalScalar::bytes(vec![0xa5; 257]).unwrap().into(),
                        CanonicalScalar::ucs_basic_text(&"value".repeat(31))
                            .unwrap()
                            .into(),
                    ])
                    .unwrap(),
                ),
            )])
            .unwrap(),
        );
        let mut admission = Vec::new();
        let expected = GraphValue::admit_property_value::<fgdb_gql::GraphIntegerError>(
            &stored,
            &mut |event| {
                admission.push(Effect::Event(event));
                Ok(())
            },
        )
        .unwrap();
        assert!(admission.len() > 10);
        let expression = stage("items[0].p");
        for stop in (1..=admission.len()).map(Some).chain([None]) {
            let mut reader = Reader {
                stored: Some(stored.clone()),
                ..Reader::default()
            };
            let trace = Arc::clone(&reader.trace);
            let live = Arc::clone(&reader.live);
            let frames = Frames::new(usize::MAX, None);
            let reserve = |bytes| frames.reserve(bytes);
            let mut after_read = 0;
            let mut meter = |event| {
                let mut trace = trace.lock().unwrap();
                let returned = trace
                    .iter()
                    .any(|effect| matches!(effect, Effect::Read(..)));
                trace.push(Effect::Event(event));
                if returned {
                    assert_eq!(live.load(AtomicOrdering::SeqCst), 1);
                    after_read += 1;
                    if stop == Some(after_read) {
                        return Err("collection admission refused");
                    }
                }
                Ok(())
            };
            fn require_send<F: Future + Send>(future: F) -> F {
                future
            }
            let result = finish(require_send(expression.start_with_async_elements(
                input(),
                17,
                &mut meter,
                &mut reader,
                &reserve,
            )));
            if stop.is_some() {
                assert!(matches!(
                    result,
                    Err(SpillSetRowError::Control("collection admission refused"))
                ));
            } else {
                let cursor = result.unwrap();
                let (actual, _) = cursor.next_with_control(&mut meter).unwrap().unwrap();
                assert_eq!(actual, row(vec![expected.clone()]));
            }
            let trace = reader.trace.lock().unwrap();
            let start = trace
                .iter()
                .position(|effect| matches!(effect, Effect::Read(..)))
                .unwrap();
            assert_eq!(
                trace[start + 1..],
                admission[..stop.unwrap_or(admission.len())]
            );
            frames.assert_released(&reader);
        }
    }

    fn stage(expression: &str) -> SpillSetStage {
        let query = prepare(&format!(
            "MATCH (n) WITH [n] AS items RETURN {expression} AS result"
        ));
        AsyncSpillSetPlan::compile(&query)
            .unwrap()
            .stages()
            .last()
            .unwrap()
            .clone()
    }
    fn identities() -> [ElementId; 5] {
        [
            ElementId::Vertex(VId(1)),
            ElementId::Vertex(VId(u128::MAX)),
            ElementId::Vertex(VId(1)),
            ElementId::Edge(EId(u128::MAX)),
            ElementId::Vertex(VId(2)),
        ]
    }
    fn input() -> GraphValueRow {
        row(vec![GraphValue::List(
            identities()
                .map(|element| match element {
                    ElementId::Vertex(vid) => GraphValue::Vertex(vid),
                    ElementId::Edge(eid) => GraphValue::Edge(eid),
                })
                .into(),
        )])
    }
    fn values() -> GraphValue {
        GraphValue::List(
            vec![
                scalar(2),
                scalar(7),
                scalar(2),
                scalar(11),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]
            .into_boxed_slice(),
        )
    }
    fn reads(trace: &[Effect]) -> Vec<(ElementId, PropertyKeyId)> {
        trace
            .iter()
            .filter_map(|effect| match effect {
                Effect::Read(element, key) => Some((*element, *key)),
                Effect::Event(_) => None,
            })
            .collect()
    }

    #[test]
    fn metadata_uses_the_same_async_stage_for_lists_nulls_names_and_failures() {
        let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
        let text = |name| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
        let props = |value| GraphValue::map(vec![("p".into(), scalar(value))]).unwrap();
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let v1 = GraphValue::Vertex(VId(1));
        let vmax = GraphValue::Vertex(VId(u128::MAX));
        let edge = GraphValue::Edge(EId(u128::MAX));
        let mixed = list(vec![
            v1.clone(),
            vmax.clone(),
            v1.clone(),
            edge.clone(),
            null.clone(),
            GraphValue::Vertex(VId(2)),
        ]);
        for (expression, input, expected, kind, count) in [
            (
                "[x IN items | properties(x)]",
                mixed.clone(),
                list(vec![
                    props(2),
                    props(7),
                    props(2),
                    props(11),
                    null.clone(),
                    null.clone(),
                ]),
                GraphElementMetadata::Properties,
                5,
            ),
            (
                "[x IN items | keys(x)]",
                mixed,
                list(
                    vec![list(vec![text("p")]); 4]
                        .into_iter()
                        .chain([null.clone(), null.clone()])
                        .collect(),
                ),
                GraphElementMetadata::Keys,
                5,
            ),
            (
                "[x IN items | labels(x)]",
                list(vec![v1.clone(), null.clone(), vmax, v1.clone()]),
                list(vec![
                    list(vec![text("Article"), text("Featured")]),
                    null.clone(),
                    list(vec![text("Featured")]),
                    list(vec![text("Article"), text("Featured")]),
                ]),
                GraphElementMetadata::Labels,
                3,
            ),
            (
                "[x IN items | type(x)]",
                list(vec![edge.clone(), null.clone(), edge]),
                list(vec![text("LINK"), null, text("LINK")]),
                GraphElementMetadata::Type,
                2,
            ),
            (
                "CASE WHEN true THEN labels(items[0]) ELSE type(items[0]) END",
                list(vec![v1]),
                list(vec![text("Article"), text("Featured")]),
                GraphElementMetadata::Labels,
                1,
            ),
        ] {
            let mut reader = MetadataReader::default();
            let frames = Frames::new(usize::MAX, None);
            let result = run_source(
                &metadata_stage(expression),
                row(vec![input]),
                &mut reader,
                &frames,
                &mut |_| Ok(()),
            );
            assert_eq!(result.unwrap(), vec![row(vec![expected])], "{expression}");
            assert_eq!(reader.calls.len(), count, "{expression}");
            assert!(reader.calls.iter().all(|(_, requested)| *requested == kind));
            assert_eq!(reader.live.load(AtomicOrdering::SeqCst), 0);
            assert_eq!(frames.state.current.load(AtomicOrdering::SeqCst), 0);
        }

        let stage = metadata_stage("properties(items[0])");
        let frames = Frames::new(usize::MAX, None);
        let mut reader = MetadataReader {
            refuse: true,
            ..MetadataReader::default()
        };
        assert_eq!(
            run_source(&stage, input(), &mut reader, &frames, &mut |_| Ok(())),
            Err(SpillSetRowError::Control("metadata source refused"))
        );
        assert_eq!(reader.live.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(frames.state.current.load(AtomicOrdering::SeqCst), 0);

        let mut reader = MetadataReader::default();
        let live = Arc::clone(&reader.live);
        let reserve = |bytes| frames.reserve(bytes);
        let mut meter = |_| Ok(());
        let mut future = Box::pin(stage.start_with_async_elements(
            input(),
            17,
            &mut meter,
            &mut reader,
            &reserve,
        ));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(live.load(AtomicOrdering::SeqCst), 1);
        drop(future);
        assert_eq!(live.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(frames.state.current.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn reached_reads_keep_bags_nulls_edges_and_full_width_ids_across_suspension() {
        for (expression, expected) in [
            ("[x IN items | x.p]", values()),
            (
                "reduce(total=0,x IN items | total+coalesce(x.p,0))",
                scalar(22),
            ),
            (
                "CASE WHEN true THEN [x IN items | x.p] ELSE [x IN items | x.q] END",
                values(),
            ),
            ("coalesce([x IN items | x.p],[x IN items | x.q])", values()),
        ] {
            let mut reader = Reader::default();
            let frames = Frames::new(usize::MAX, None);
            let output = run(&stage(expression), input(), &mut reader, &frames, None).unwrap();
            assert_eq!(output, vec![row(vec![expected])], "{expression}");
            assert_eq!(
                reads(&reader.trace.lock().unwrap()),
                identities().map(|element| (element, P)),
                "{expression}"
            );
            frames.assert_released(&reader);
        }

        let mut reader = Reader::default();
        let frames = Frames::new(usize::MAX, None);
        let output = run(
            &stage("[x IN items WHERE x.p IS NOT NULL | x.p]"),
            input(),
            &mut reader,
            &frames,
            None,
        )
        .unwrap();
        assert_eq!(
            output,
            vec![row(vec![GraphValue::List(
                vec![scalar(2), scalar(7), scalar(2), scalar(11)].into_boxed_slice()
            )])]
        );
        let mut expected = Vec::new();
        for (at, element) in identities().into_iter().enumerate() {
            expected.push((element, P));
            if at < 4 {
                expected.push((element, P));
            }
        }
        assert_eq!(reads(&reader.trace.lock().unwrap()), expected);
        frames.assert_released(&reader);
    }

    #[test]
    fn native_refusal_at_every_event_keeps_the_exact_effect_prefix() {
        let stage = stage("[x IN items | x.p]");
        let mut reader = Reader::default();
        let frames = Frames::new(usize::MAX, None);
        run(&stage, input(), &mut reader, &frames, None).unwrap();
        let expected = reader.trace.lock().unwrap().clone();
        frames.assert_released(&reader);
        let mut ordinal = 0;
        for (at, effect) in expected.iter().enumerate() {
            if !matches!(effect, Effect::Event(_)) {
                continue;
            }
            ordinal += 1;
            let mut reader = Reader::default();
            let frames = Frames::new(usize::MAX, None);
            assert_eq!(
                run(&stage, input(), &mut reader, &frames, Some(ordinal)),
                Err(SpillSetRowError::Control("native event refused"))
            );
            assert_eq!(*reader.trace.lock().unwrap(), expected[..=at]);
            frames.assert_released(&reader);
        }

        let refused = ElementId::Edge(EId(u128::MAX));
        let mut reader = Reader {
            refuse: Some(refused),
            ..Reader::default()
        };
        let frames = Frames::new(usize::MAX, None);
        assert_eq!(
            run(&stage, input(), &mut reader, &frames, None),
            Err(SpillSetRowError::Control("original source failure"))
        );
        let at = expected
            .iter()
            .position(|effect| *effect == Effect::Read(refused, P))
            .unwrap();
        assert_eq!(*reader.trace.lock().unwrap(), expected[..=at]);
        frames.assert_released(&reader);
    }

    #[test]
    fn every_recursive_allocation_is_admitted_and_released_even_when_dropped() {
        let stage = stage("[x IN items | x.p]");
        let mut reader = Reader::default();
        let frames = Frames::new(usize::MAX, None);
        let expected_rows = run(&stage, input(), &mut reader, &frames, None).unwrap();
        let expected_trace = reader.trace.lock().unwrap().clone();
        let peak = frames.state.peak.load(AtomicOrdering::SeqCst);
        let attempts = frames.state.attempts.load(AtomicOrdering::SeqCst);
        assert!(peak > 0);
        assert!(attempts > identities().len());
        frames.assert_released(&reader);

        let mut reader = Reader::default();
        let frames = Frames::new(peak, None);
        assert_eq!(
            run(&stage, input(), &mut reader, &frames, None).unwrap(),
            expected_rows
        );
        assert_eq!(*reader.trace.lock().unwrap(), expected_trace);
        frames.assert_released(&reader);
        let mut reader = Reader::default();
        let frames = Frames::new(peak - 1, None);
        assert_eq!(
            run(&stage, input(), &mut reader, &frames, None),
            Err(SpillSetRowError::Control("frame refused"))
        );
        frames.assert_released(&reader);

        for stop in 1..=attempts {
            let mut reader = Reader::default();
            let frames = Frames::new(usize::MAX, Some(stop));
            assert_eq!(
                run(&stage, input(), &mut reader, &frames, None),
                Err(SpillSetRowError::Control("frame refused"))
            );
            let trace = reader.trace.lock().unwrap();
            assert_eq!(*trace, expected_trace[..trace.len()]);
            frames.assert_released(&reader);
        }

        let mut reader = Reader::default();
        let trace = Arc::clone(&reader.trace);
        let live = Arc::clone(&reader.live);
        let frames = Frames::new(usize::MAX, None);
        let reserve = |bytes| frames.reserve(bytes);
        let mut control = control(Arc::clone(&trace), None);
        let mut future = Box::pin(stage.start_with_async_elements(
            input(),
            17,
            &mut control,
            &mut reader,
            &reserve,
        ));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(future.as_mut().poll(&mut cx).is_pending());
        assert_eq!(live.load(AtomicOrdering::SeqCst), 1);
        assert!(frames.state.current.load(AtomicOrdering::SeqCst) > 0);
        assert_eq!(reads(&trace.lock().unwrap()), vec![(identities()[0], P)]);
        drop(future);
        frames.assert_released(&reader);
        assert_eq!(reads(&trace.lock().unwrap()), vec![(identities()[0], P)]);
    }

    #[test]
    fn missing_properties_and_unavailable_readers_remain_distinct() {
        for property in [
            GraphElementKey::Missing,
            GraphElementKey::Unbound,
            GraphElementKey::Property(P),
        ] {
            let query = PreparedGraphSet::from(leaf("MATCH (n) RETURN n AS item"))
                .project(
                    vec![GraphSetProjection::new(
                        "value",
                        GraphSetValue::MapGet {
                            map: Box::new(GraphSetValue::Column(0)),
                            key: "p".into(),
                            property,
                        },
                    )],
                    GraphSetQuantifier::All,
                )
                .unwrap();
            let plan = AsyncSpillSetPlan::compile(&query).unwrap();
            let stage = plan.stages().last().unwrap();
            let mut reader = Reader {
                unavailable: true,
                ..Reader::default()
            };
            let frames = Frames::new(usize::MAX, None);
            let result = run(
                stage,
                row(vec![GraphValue::Vertex(VId(1))]),
                &mut reader,
                &frames,
                None,
            );
            if property == GraphElementKey::Missing {
                assert_eq!(
                    result.unwrap(),
                    vec![row(vec![GraphValue::Scalar(CanonicalScalar::Null)])]
                );
            } else {
                assert!(matches!(
                    result,
                    Err(SpillSetRowError::Native(
                        GraphSetExecutionError::Projection {
                            error: fgdb_gql::GraphIntegerError {
                                kind: GraphIntegerErrorKind::IncompatibleOperands,
                                ..
                            },
                            ..
                        }
                    ))
                ));
            }
            let expected = if property == GraphElementKey::Property(P) {
                vec![(ElementId::Vertex(VId(1)), P)]
            } else {
                Vec::new()
            };
            assert_eq!(reads(&reader.trace.lock().unwrap()), expected);
            frames.assert_released(&reader);
        }
    }

    #[test]
    fn synchronous_and_async_stages_preserve_no_read_values_and_native_events() {
        let mut stages = [
            "[x IN items WHERE x > 1 | x * 2]",
            "reduce(total=0,x IN items | total+x)",
            "CASE WHEN true THEN {values:items,tail:items[1..]} ELSE null END",
            "items[0] + 10",
            "CASE WHEN true THEN items[0] + 10 ELSE 0 END",
            "coalesce(null,items)",
            "[x IN items | rand()]",
        ]
        .map(|expression| (expression, stage(expression)))
        .to_vec();
        let query = prepare("MATCH (n) WITH [n] AS items UNWIND items AS value RETURN value");
        let plan = AsyncSpillSetPlan::compile(&query).unwrap();
        let unwind = plan
            .stages()
            .iter()
            .find(|stage| {
                stage
                    .columns()
                    .iter()
                    .map(String::as_str)
                    .eq(["items", "value"])
            })
            .unwrap()
            .clone();
        stages.push(("UNWIND items", unwind));
        for (expression, stage) in stages {
            let input = row(vec![GraphValue::List(
                vec![scalar(1), scalar(2), scalar(3)].into_boxed_slice(),
            )]);
            let mut expected_events = Vec::new();
            let expected = stage_rows(&stage, input.clone(), 17, &mut |event| {
                expected_events.push(Effect::Event(event));
                Ok::<_, &'static str>(())
            });
            let mut reader = Reader::default();
            let frames = Frames::new(0, None);
            let actual = run(&stage, input, &mut reader, &frames, None);
            assert_eq!(actual, expected, "{expression}");
            assert_eq!(
                *reader.trace.lock().unwrap(),
                expected_events,
                "{expression}"
            );
            assert_eq!(
                frames.state.attempts.load(AtomicOrdering::SeqCst),
                0,
                "{expression}"
            );
            frames.assert_released(&reader);
        }
    }
}
