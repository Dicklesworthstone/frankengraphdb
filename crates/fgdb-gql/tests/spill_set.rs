//! Physical stages preserve binary bags, Cartesian occurrence order, inner
//! pages, canonicalization and complete-input failure barriers. This small
//! materialized host tests the public semantic seam, not external I/O bounds.

use core::cmp::Ordering;
use core::convert::Infallible;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::row_join::RowJoinSpec;
use fgdb_gql::spill_aggregate::SpillAggregateDefinition;
use fgdb_gql::spill_set::{
    AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetAggregateRowError, SpillSetBuildError,
    SpillSetRowError, SpillSetStage,
};
use fgdb_gql::{
    GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphAggregateError, GraphAggregateRow, GraphAggregateValue, GraphExactAverage,
    GraphIntegerErrorKind, GraphSetColumnType, GraphSetExecutionError, GraphSetOperation,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphSet, PreparedGraphSetAggregate, PreparedGraphSetText, PreparedGraphText,
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
        AsyncSpillSetSourcePlan::Binary(binary) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), binary.columns());
            // Complete both children before any parent operation, then use
            // independent count arithmetic, not the production merge driver.
            let left = staged_plan(graph, binary.left())?;
            let right = staged_plan(graph, binary.right())?;
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
            let left = staged_plan(graph, cross.left())?;
            let right = staged_plan(graph, cross.right())?;
            let mut rows = Vec::new();
            for left in &left {
                for right in &right {
                    rows.push(cross.copy_pair_with_control(left, right, &mut |event| {
                        assert_ne!(event, GlaExecutionEvent::ResultRow);
                        Ok(())
                    })?);
                }
            }
            rows
        }
        AsyncSpillSetSourcePlan::Aggregate(aggregate) => {
            assert!(plan.source_pattern().is_none());
            assert!(plan.source_tail().is_none());
            assert_eq!(plan.source_columns(), aggregate.columns());
            assert_eq!(plan.source().kind(), aggregate.input().source().kind());
            assert!(!aggregate.definition().has_output_stage());
            let input = staged_plan(graph, aggregate.input())?;
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
    for text in [
        "MATCH (a)-[:R]->(b)-[:R]->(c) WHERE EXISTS { MATCH (c)-[:R]->(d) } RETURN a.p+c.p AS value LIMIT 0",
        "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(x) } RETURN a.p+1 AS value LIMIT 0",
    ] {
        let error = AsyncSpillSetPlan::compile(&prepare(text)).unwrap_err();
        assert!(
            matches!(
                error,
                SpillSetBuildError::Vertex(_) | SpillSetBuildError::Edge(_)
            ),
            "{text}: {error:?}"
        );
    }
    let pattern = PreparedGraphSet::from(leaf("MATCH (n) RETURN n.p AS value"));
    let spec = RowJoinSpec::cross(pattern.column_types(), pattern.column_types()).unwrap();
    let unsupported = pattern.clone().join(pattern.clone(), spec).unwrap();
    let query = unsupported.clone().nested().unwrap().with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&query),
        Err(SpillSetBuildError::Unsupported { depth: 1 })
    ));
    let unsupported = unsupported
        .project(
            vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
            GraphSetQuantifier::All,
        )
        .unwrap();
    let query = pattern
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            unsupported,
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&query),
        Err(SpillSetBuildError::Unsupported { depth: 2 })
    ));
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
        "UNWIND [1,2] AS value WITH AVG(value) AS mean RETURN mean LIMIT 0",
        "UNWIND [2,2] AS value WITH AVG(value) AS mean RETURN mean LIMIT 0",
        "UNWIND [{key:1,value:1},{key:2,value:9223372036854775807},{key:2,value:1}] AS record WITH record.key AS key,SUM(record.value) AS total ORDER BY key LIMIT 1 RETURN total",
        "UNWIND [1,0] AS value WITH SUM(1/value) AS total RETURN total LIMIT 0",
        "UNWIND [1,'bad'] AS value WITH SUM(value) AS total RETURN total LIMIT 0",
        "UNWIND [] AS value RETURN value UNION ALL UNWIND [1,2] AS item WITH AVG(item) AS value RETURN value LIMIT 0",
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
        (
            GraphAggregateValue::Average(GraphExactAverage::new(2, 1).unwrap()),
            GraphIntegerErrorKind::IncompatibleOperands,
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
    assert!(matches!(
        AsyncSpillSetPlan::compile(&collection.clone().with_page(0, Some(0))),
        Err(SpillSetBuildError::Unsupported { depth: 0 })
    ));
    let nested = collection.nested().unwrap().with_page(0, Some(0));
    assert!(matches!(
        AsyncSpillSetPlan::compile(&nested),
        Err(SpillSetBuildError::Unsupported { depth: 1 })
    ));
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
        "UNWIND [2] AS value WITH AVG(value) AS bad RETURN bad",
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
    // Relational joins execute over completed child runs since d3574354, so
    // a product over one compiles; a collection child still refuses.
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
    let unsupported = collection;
    for product in [
        input
            .clone()
            .with_page(0, Some(0))
            .cross_join(unsupported.clone())
            .unwrap(),
        unsupported
            .cross_join(input.clone().with_page(0, Some(0)))
            .unwrap(),
    ] {
        assert!(matches!(
            AsyncSpillSetPlan::compile(&product.clone().with_page(0, Some(0))),
            Err(SpillSetBuildError::Unsupported { depth: 1 })
        ));
        assert!(matches!(
            AsyncSpillSetPlan::compile(&product.nested().unwrap().with_page(0, Some(0))),
            Err(SpillSetBuildError::Unsupported { depth: 2 })
        ));
    }
}
