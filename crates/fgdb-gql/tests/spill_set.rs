//! Physical stages must preserve the native relation, including every binary
//! bag, inner page, canonicalization and complete-input failure barrier. This small
//! materialized host tests the public semantic seam, not external I/O bounds.

use core::cmp::Ordering;
use core::convert::Infallible;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::spill_set::{
    AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetBuildError, SpillSetRowError, SpillSetStage,
};
use fgdb_gql::{
    GlaExecutionEvent, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphIntegerErrorKind, GraphSetExecutionError, GraphSetOperation,
    GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol, GraphSymbolKind,
    PreparedGraphSet, PreparedGraphSetAggregate, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, EId, VId};
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
    staged_plan(graph, &plan)
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
    assert_eq!(unit, GraphValueRow::unit());
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
        vec![GraphValueRow::unit()]
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
    let unsupported = pattern.clone().cross_join(pattern.clone()).unwrap();
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
