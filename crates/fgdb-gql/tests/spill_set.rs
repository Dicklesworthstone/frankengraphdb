//! Physical unary stages must preserve the native relation, including every
//! inner page, canonicalization and complete-input failure barrier. This small
//! materialized host tests the public semantic seam, not external I/O bounds.

use core::cmp::Ordering;
use core::convert::Infallible;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::spill_set::{
    AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetBuildError, SpillSetRowError,
};
use fgdb_gql::{
    GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphIntegerErrorKind, GraphSetExecutionError, GraphSetOperation, GraphSetProjection,
    GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, EId, VId};

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

struct Graph {
    properties: Vec<Option<CanonicalScalar>>,
    edges: Vec<(EId, VId, RelationId, VId)>,
    edge_properties: Vec<CanonicalScalar>,
}
impl Graph {
    fn sample() -> Self {
        Self {
            // Source identity order and canonical property order disagree.
            properties: [Some(3), Some(1), Some(4), Some(1), Some(2), None]
                .into_iter()
                .map(|value| value.map(CanonicalScalar::Int))
                .collect(),
            edges: vec![
                (EId(1), VId(1), R, VId(2)),
                (EId(2), VId(1), R, VId(2)),
                (EId(3), VId(2), R, VId(2)),
                (EId(4), VId(2), R, VId(3)),
            ],
            edge_properties: [7, 8, 9, 10].map(CanonicalScalar::Int).to_vec(),
        }
    }
    fn execute(
        &self,
        pattern: &PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<Infallible, ()>> {
        let ranks: Vec<_> = (1..=self.properties.len())
            .map(|index| CanonicalScalar::Int(index as i64))
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
            (false, false) => {
                if key.descending {
                    b.cmp(a)
                } else {
                    a.cmp(b)
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
    // The graph's own ordered/page/visible contract is completed before the
    // relational host runs, as the real buffered host must do with source_tail.
    let mut rows = graph
        .execute(plan.source_pattern(), policy())
        .unwrap()
        .value;
    for stage in plan.stages() {
        let mut output = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            if let Some(row) = stage.evaluate(row, index, &mut |event| {
                assert_ne!(event, GlaExecutionEvent::ResultRow);
                Ok(())
            })? {
                output.push(row);
            }
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
    assert!(plan.source_tail().evaluation_width() > plan.source_tail().visible_width());
    assert_eq!(plan.source_tail().offset(), 1);
    assert_eq!(plan.source_tail().count(), Some(3));
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
            .map(|value| Some(CanonicalScalar::Int(value)))
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
    let output = stage
        .evaluate(input.clone(), 17, &mut |event| {
            events.push(event);
            Ok::<_, &'static str>(())
        })
        .unwrap()
        .unwrap();
    assert!(matches!(output.get(0), Some(GraphValue::Map { .. })));
    assert!(events.contains(&GlaExecutionEvent::ScratchEntry));
    assert!(!events.contains(&GlaExecutionEvent::ResultRow));
    for refuse in 0..events.len() {
        let mut observed = Vec::new();
        let error = stage
            .evaluate(input.clone(), 17, &mut |event| {
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
        stage.evaluate(
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
    let unsupported = [
        PreparedGraphSet::singleton(),
        pattern
            .clone()
            .unwind(
                "element".to_owned(),
                GraphSetValue::List(vec![GraphSetValue::Column(0)]),
            )
            .unwrap(),
        pattern
            .clone()
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                pattern.clone(),
            )
            .unwrap(),
        pattern.clone().cross_join(pattern).unwrap(),
    ];
    for input in unsupported {
        let query = input.nested().unwrap().with_page(0, Some(0));
        assert!(matches!(
            AsyncSpillSetPlan::compile(&query),
            Err(SpillSetBuildError::Unsupported { depth: 1 })
        ));
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
    let query = prepare(
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH s.p AS value ORDER BY value DESC SKIP 1 LIMIT 3 RETURN value*2 AS result",
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
    assert!(plan.source_tail().evaluation_width() > plan.source_tail().visible_width());
    assert_eq!(plan.source_tail().offset(), 1);
    assert_eq!(plan.source_tail().count(), Some(3));
    assert_eq!(staged(&graph, &query).unwrap(), vec![row(vec![scalar(4)])]);
}
