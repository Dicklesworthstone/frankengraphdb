//! Appendix C `ProcedureCall`: `CALL ns.name(args) YIELD ...` starts a read
//! pipeline whose rows the host supplies. Every law here drives the real text
//! grammar, binder and relational executor; only the procedure is a test host.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSetExecutionError, GraphSymbol, GraphSymbolKind, GraphSymbolResolver,
    PreparedGraphSet, PreparedGraphSetText, PreparedProcedureCall, ProcedureGraphDirection,
    ProcedureGraphProjection, ProcedureGraphWeight, ProcedureMissingWeight, ProcedureParallelEdges,
    ProcedureSelfLoops, ReverseSymbolCatalog,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::RefCell;

const P: PropertyKeyId = PropertyKeyId(1);
type Fault = GqlQueryError<GraphSetExecutionError<&'static str>, usize>;
type Rows = Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    matches!((kind, name), (GraphSymbolKind::Property, "p")).then_some(GraphSymbol::Property(P))
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str, params: &GqlParameters) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(params)
        .unwrap()
}
fn int(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn rows(values: Vec<Vec<GraphValue>>) -> Rows {
    let count = values.len() as u64;
    Ok(GqlQueryExecution {
        value: values
            .into_iter()
            .map(GraphValueRow::from_owned_values)
            .collect(),
        rows: GqlExecutionStats {
            snapshot_records: count,
            result_rows: count,
        },
        evaluator: GlaExecutionStats::default(),
    })
}
/// The test host: `t.scores()` yields (vertex, score); `t.echo(...)` yields
/// its arguments back as one row; anything else fails.
fn host(call: &PreparedProcedureCall, arguments: &[GraphValue]) -> Rows {
    match (call.namespace(), call.name()) {
        ("t", "scores") => {
            let table = [(1, 1), (2, 3), (3, 2), (4, 5)];
            rows(
                table
                    .iter()
                    .map(|&(vertex, score)| {
                        call.outputs()
                            .iter()
                            .map(|output| match output.as_str() {
                                "vertex" => GraphValue::Vertex(VId(vertex)),
                                _ => int(score),
                            })
                            .collect()
                    })
                    .collect(),
            )
        }
        ("t", "echo") => rows(vec![arguments.to_vec()]),
        ("t", "empty") => rows(Vec::new()),
        _ => Err(GqlQueryError::Source("unknown procedure")),
    }
}
fn no_graph(_: &fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow>, _: GqlQueryPolicy) -> Rows {
    Err(GqlQueryError::Source("this law reads no graph"))
}
fn call(query: &PreparedGraphSet) -> Result<Vec<GraphValueRow>, Fault> {
    query
        .execute_governed_with_procedures(
            wide(),
            no_graph,
            |call, arguments, _| host(call, arguments),
            || Ok(()),
        )
        .map(|execution| execution.value)
}
fn column(rows: &[GraphValueRow], at: usize) -> Vec<GraphValue> {
    rows.iter().map(|row| row.values()[at].clone()).collect()
}

fn projection_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(11))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(17))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

#[test]
fn inline_projection_is_catalog_bound_and_never_an_algorithm_argument() {
    let mut resolutions = Vec::new();
    let template = PreparedGraphSetText::prepare(
        "CALL fnx.test({labels:['Person'], relations:['R'], weight:'p', \
         missing:'unit', direction:'reversed', parallel:'max', self_loops:'drop'}, 7, $k) \
         YIELD value RETURN value",
        |kind, name| {
            resolutions.push((kind, name.to_owned()));
            projection_symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(
        resolutions,
        [
            (GraphSymbolKind::Label, "Person".to_owned()),
            (GraphSymbolKind::Relation, "R".to_owned()),
            (GraphSymbolKind::Property, "p".to_owned()),
        ]
    );
    assert_eq!(template.parameter_schema().len(), 1);
    let expected = ProcedureGraphProjection {
        vertex_label: Some(LabelId(11)),
        relation: Some(RelationId(17)),
        weight: ProcedureGraphWeight::Property {
            key: P,
            missing: ProcedureMissingWeight::Unit,
        },
        direction: Some(ProcedureGraphDirection::Reversed),
        parallel_edges: ProcedureParallelEdges::Maximum,
        self_loops: ProcedureSelfLoops::Drop,
    };
    for k in [19, 42] {
        let parameters = GqlParameters::new().with_int64("k", k).unwrap();
        let query = template.bind_parameters(&parameters).unwrap();
        let result = query
            .execute_governed_with_procedures(
                wide(),
                no_graph,
                |call, arguments, _| {
                    assert_eq!(call.graph_projection(), Some(&expected));
                    assert_eq!(arguments, [int(7), int(k)]);
                    assert!(call.argument_names().is_empty());
                    rows(vec![vec![int(k)]])
                },
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(column(&result.value, 0), [int(k)]);
    }
}

#[test]
fn graph_projection_defaults_and_every_option_have_one_typed_meaning() {
    let base = ProcedureGraphProjection {
        vertex_label: None,
        relation: None,
        weight: ProcedureGraphWeight::Unit,
        direction: None,
        parallel_edges: ProcedureParallelEdges::Reject,
        self_loops: ProcedureSelfLoops::Keep,
    };
    let cases = [
        ("{}", base),
        ("{labels:[], relations:[]}", base),
        (
            "{direction:'directed'}",
            ProcedureGraphProjection {
                direction: Some(ProcedureGraphDirection::Directed),
                ..base
            },
        ),
        (
            "{direction:'undirected'}",
            ProcedureGraphProjection {
                direction: Some(ProcedureGraphDirection::Undirected),
                ..base
            },
        ),
        (
            "{parallel:'collapse'}",
            ProcedureGraphProjection {
                parallel_edges: ProcedureParallelEdges::CollapseUnit,
                ..base
            },
        ),
        (
            "{parallel:'min'}",
            ProcedureGraphProjection {
                parallel_edges: ProcedureParallelEdges::Minimum,
                ..base
            },
        ),
        (
            "{parallel:'sum'}",
            ProcedureGraphProjection {
                parallel_edges: ProcedureParallelEdges::Sum,
                ..base
            },
        ),
        (
            "{self_loops:'reject'}",
            ProcedureGraphProjection {
                self_loops: ProcedureSelfLoops::Reject,
                ..base
            },
        ),
        (
            "{weight:'p'}",
            ProcedureGraphProjection {
                weight: ProcedureGraphWeight::Property {
                    key: P,
                    missing: ProcedureMissingWeight::Reject,
                },
                ..base
            },
        ),
        (
            "{weight:'p', missing:'zero'}",
            ProcedureGraphProjection {
                weight: ProcedureGraphWeight::Property {
                    key: P,
                    missing: ProcedureMissingWeight::Zero,
                },
                ..base
            },
        ),
    ];
    for (projection, expected) in cases {
        let text = format!("CALL fnx.test({projection}) YIELD value RETURN value");
        let query = PreparedGraphSetText::prepare(&text, projection_symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        query
            .execute_governed_with_procedures(
                wide(),
                no_graph,
                |call, arguments, _| {
                    assert_eq!(call.graph_projection(), Some(&expected), "{text}");
                    assert!(arguments.is_empty(), "{text}");
                    rows(vec![vec![int(1)]])
                },
                || Ok::<_, usize>(()),
            )
            .unwrap();
    }
}

#[test]
fn invalid_graph_options_refuse_before_any_schema_lookup() {
    for projection in [
        "{labels:['Person'], unknown:'x'}",
        "{labels:['Person','Other']}",
        "{relations:['R','R']}",
        "{labels:'Person'}",
        "{relations:[17]}",
        "{labels:[$label]}",
        "{weight:1}",
        "{missing:'unit'}",
        "{weight:'p',missing:'skip'}",
        "{labels:['Person'],direction:'sideways'}",
        "{labels:['Person'],parallel:'first'}",
        "{labels:['Person'],self_loops:'ignore'}",
        "{labels:['Person'],labels:[]}",
    ] {
        let text = format!("CALL fnx.test({projection}) YIELD value RETURN value LIMIT 0");
        let mut lookups = 0;
        assert!(
            PreparedGraphSetText::prepare(&text, |kind, name| {
                lookups += 1;
                projection_symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(lookups, 0, "{text}");
    }
    for projection in [
        "{labels:['Absent']}",
        "{relations:['Absent']}",
        "{weight:'Absent'}",
    ] {
        let text = format!("CALL fnx.test({projection}) YIELD value RETURN value LIMIT 0");
        assert!(PreparedGraphSetText::prepare(&text, projection_symbols).is_err());
    }
    let wrong_kind = PreparedGraphSetText::prepare(
        "CALL fnx.test({labels:['Person']}) YIELD value RETURN value",
        |_, _| Some(GraphSymbol::Relation(RelationId(11))),
    );
    assert!(matches!(
        wrong_kind,
        Err(fgdb_gql::GraphSetTextError {
            kind: fgdb_gql::GraphSetTextErrorKind::Pattern(
                fgdb_gql::GraphPatternTextErrorKind::WrongSymbolKind {
                    expected: GraphSymbolKind::Label,
                    found: GraphSymbolKind::Relation,
                }
            ),
            ..
        })
    ));
}

#[test]
fn projection_identity_is_canonical_and_pins_every_answer_affecting_option() {
    let prepare = |projection: &str| {
        PreparedGraphSetText::prepare(
            &format!("CALL fnx.test({projection}, 1) YIELD value RETURN value"),
            projection_symbols,
        )
        .unwrap()
    };
    let first = prepare("{labels:['Person'],relations:['R'],weight:'p',missing:'unit'}");
    let reordered = prepare("{missing:'unit',weight:'p',relations:['R'],labels:['Person']}");
    assert_eq!(
        first.canonical_template_bytes(),
        reordered.canonical_template_bytes()
    );
    assert_eq!(
        first
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes(),
        reordered
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    );
    let default = prepare("{}");
    for projection in [
        "{labels:['Person']}",
        "{relations:['R']}",
        "{weight:'p'}",
        "{weight:'p',missing:'zero'}",
        "{direction:'directed'}",
        "{direction:'reversed'}",
        "{direction:'undirected'}",
        "{parallel:'collapse'}",
        "{parallel:'min'}",
        "{parallel:'max'}",
        "{parallel:'sum'}",
        "{self_loops:'drop'}",
        "{self_loops:'reject'}",
    ] {
        let changed = prepare(projection);
        assert_ne!(
            default.canonical_template_bytes(),
            changed.canonical_template_bytes()
        );
        assert_ne!(
            default
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .canonical_bytes(),
            changed
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .canonical_bytes(),
            "{projection}"
        );
    }
    let same_names_other_catalog = PreparedGraphSetText::prepare(
        "CALL fnx.test({labels:['Person'],relations:['R'],weight:'p',missing:'unit'},1) \
         YIELD value RETURN value",
        |kind, name| match kind {
            GraphSymbolKind::Label => Some(GraphSymbol::Label(LabelId(u64::MAX))),
            _ => projection_symbols(kind, name),
        },
    )
    .unwrap();
    assert_ne!(
        first.canonical_template_bytes(),
        same_names_other_catalog.canonical_template_bytes()
    );
}

#[test]
fn non_prism_map_arguments_keep_their_value_domain() {
    let query = prepare(
        "CALL t.echo({labels:['Person'],weight:'p'}) YIELD value RETURN value",
        &GqlParameters::new(),
    );
    let result = query
        .execute_governed_with_procedures(
            wide(),
            no_graph,
            |call, arguments, _| {
                assert!(call.graph_projection().is_none());
                assert_eq!(arguments.len(), 1);
                assert!(matches!(arguments[0], GraphValue::Map { .. }));
                host(call, arguments)
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 1);
    assert!(matches!(
        result.value[0].values()[0],
        GraphValue::Map { .. }
    ));
}

#[test]
fn the_original_procedure_builder_signature_keeps_its_unprojected_behavior() {
    let query = PreparedGraphSet::singleton()
        .procedure_call(
            "t".to_owned(),
            "scores".to_owned(),
            Vec::new(),
            Vec::new(),
            vec![("score".to_owned(), "s".to_owned())],
            Vec::new(),
        )
        .unwrap();
    assert_eq!(query.columns(), ["s"]);
    let result = query
        .execute_governed_with_procedures(
            wide(),
            no_graph,
            |call, arguments, _| {
                assert!(call.graph_projection().is_none());
                host(call, arguments)
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    // A procedure source is admitted in canonical bag order, as a pattern
    // source is, whatever order the host yields (set_ops admit_source_rows).
    assert_eq!(column(&result.value, 0), [int(1), int(2), int(3), int(5)]);
}

#[test]
fn procedure_rows_flow_through_filter_projection_order_and_page() {
    let query = prepare(
        "CALL t.scores() YIELD vertex, score WHERE score > 1 \
         RETURN vertex, score ORDER BY score DESC LIMIT 2",
        &GqlParameters::new(),
    );
    assert!(query.calls_procedure());
    let result = call(&query).unwrap();
    assert_eq!(
        column(&result, 0),
        vec![GraphValue::Vertex(VId(4)), GraphValue::Vertex(VId(2))]
    );
    assert_eq!(column(&result, 1), vec![int(5), int(3)]);
    // Without the page every row survives the same filter exactly once.
    let all = prepare(
        "CALL t.scores() YIELD vertex, score WHERE score > 1 RETURN score ORDER BY score",
        &GqlParameters::new(),
    );
    assert_eq!(
        column(&call(&all).unwrap(), 0),
        vec![int(2), int(3), int(5)]
    );
}

#[test]
fn yield_order_and_aliases_are_the_host_contract() {
    let seen = RefCell::new(Vec::new());
    let query = prepare(
        "CALL t.scores() YIELD score AS s, vertex AS v RETURN v, s ORDER BY s",
        &GqlParameters::new(),
    );
    let result = query
        .execute_governed_with_procedures(
            wide(),
            no_graph,
            |call, arguments, _| {
                seen.borrow_mut().push(call.outputs().to_vec());
                host(call, arguments)
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value;
    // The host was asked for its OWN output names, once, in YIELD order.
    assert_eq!(
        *seen.borrow(),
        vec![vec!["score".to_owned(), "vertex".to_owned()]]
    );
    assert_eq!(column(&result, 1), vec![int(1), int(2), int(3), int(5)]);
    assert_eq!(column(&result, 0)[0], GraphValue::Vertex(VId(1)));
}

#[test]
fn constant_and_parameter_arguments_reach_the_host_evaluated() {
    let params = GqlParameters::new().with_int64("k", 42).unwrap();
    let query = prepare(
        "CALL t.echo(3, 'x', $k) YIELD a, b, c RETURN a, b, c",
        &params,
    );
    let result = call(&query).unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].values()[0], int(3));
    assert_eq!(
        result[0].values()[1],
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("x").unwrap())
    );
    assert_eq!(result[0].values()[2], int(42));
}

#[test]
fn empty_results_errors_and_width_violations_publish_no_rows() {
    let empty = prepare("CALL t.empty() YIELD v RETURN v", &GqlParameters::new());
    assert!(call(&empty).unwrap().is_empty());
    let unknown = prepare("CALL t.nope() YIELD v RETURN v", &GqlParameters::new());
    assert!(matches!(
        call(&unknown),
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "unknown procedure"
        )))
    ));
    // A host that returns the wrong width is refused, not truncated.
    let narrow = prepare("CALL t.echo(1, 2) YIELD a RETURN a", &GqlParameters::new());
    assert!(matches!(
        call(&narrow),
        Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
            operand: 0
        }))
    ));
}

#[test]
fn a_host_without_procedures_refuses_instead_of_inventing_rows() {
    let query = prepare(
        "CALL t.scores() YIELD score RETURN score",
        &GqlParameters::new(),
    );
    let refused = query.execute_governed(wide(), no_graph, || Ok::<_, usize>(()));
    assert!(matches!(
        refused,
        Err(GqlQueryError::Source(
            GraphSetExecutionError::ProcedureUnavailable { operand: 0 }
        ))
    ));
}

#[test]
fn each_call_in_a_set_expression_runs_exactly_once() {
    let calls = RefCell::new(0);
    let query = prepare(
        "CALL t.scores() YIELD score RETURN score \
         UNION ALL CALL t.scores() YIELD score RETURN score",
        &GqlParameters::new(),
    );
    let result = query
        .execute_governed_with_procedures(
            wide(),
            no_graph,
            |call, arguments, _| {
                *calls.borrow_mut() += 1;
                host(call, arguments)
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value;
    assert_eq!(*calls.borrow(), 2);
    assert_eq!(result.len(), 8);
}

#[test]
fn procedure_columns_correlate_with_a_later_match() {
    // Graph: VId(i) has p = values[i]. The procedure yields ids to look up.
    let values = [10, 20, 30].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let query = prepare(
        "CALL t.echo(20) YIELD id MATCH (n {p: id}) RETURN n",
        &GqlParameters::new(),
    );
    let result =
        query
            .execute_governed_with_procedures(
                wide(),
                |pattern, remaining| {
                    pattern.plan().execute_governed_with_properties(
                        values.len() as u64,
                        (0..values.len()).map(|at| VId(at as u128)),
                        [],
                        |vid, predicates| {
                            Ok::<_, &'static str>(predicates.iter().all(|test| {
                                test.matches(&[], &[(P, values[vid.0 as usize].clone())])
                            }))
                        },
                        |vid, _| Ok(Some(&values[vid.0 as usize])),
                        remaining,
                        || Ok::<_, usize>(()),
                    )
                },
                |call, arguments, _| host(call, arguments),
                || Ok(()),
            )
            .unwrap()
            .value;
    assert_eq!(result.len(), 1);
    assert!(result[0].values().contains(&GraphValue::Vertex(VId(1))));
}

#[test]
fn malformed_or_misplaced_calls_refuse_before_any_catalog_access() {
    for text in [
        "CALL t.scores() RETURN 1 AS x",
        "CALL t.scores YIELD score RETURN score",
        "CALL scores() YIELD score RETURN score",
        "CALL t.scores() YIELD score, score RETURN score",
        "UNWIND [1] AS x CALL t.scores() YIELD score RETURN score",
        "CALL t.scores(1 YIELD score RETURN score",
    ] {
        let mut lookups = 0;
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                lookups += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(lookups, 0, "a refused CALL resolved symbols: {text}");
    }
}

#[test]
fn the_call_is_part_of_the_relation_identity() {
    let base = prepare("CALL t.echo(1) YIELD a RETURN a", &GqlParameters::new());
    assert_eq!(
        base.canonical_bytes(),
        prepare("CALL t.echo(1) YIELD a RETURN a", &GqlParameters::new()).canonical_bytes()
    );
    for other in [
        "CALL t.echo(2) YIELD a RETURN a",
        "CALL t.other(1) YIELD a RETURN a",
        "CALL u.echo(1) YIELD a RETURN a",
        "CALL t.echo(1) YIELD b AS a RETURN a",
    ] {
        assert_ne!(
            base.canonical_bytes(),
            prepare(other, &GqlParameters::new()).canonical_bytes(),
            "{other}"
        );
    }
}

/// Graph VId(i) has p = values[i]; any property read of `p` sees it.
fn graph(
    values: &[CanonicalPropertyValue],
) -> impl FnMut(&fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow>, GqlQueryPolicy) -> Rows + '_
{
    move |pattern, remaining| {
        pattern.plan().execute_governed_with_properties(
            values.len() as u64,
            (0..values.len()).map(|at| VId(at as u128)),
            [],
            |vid, predicates| {
                Ok::<_, &'static str>(
                    predicates
                        .iter()
                        .all(|test| test.matches(&[], &[(P, values[vid.0 as usize].clone())])),
                )
            },
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            remaining,
            || Ok::<_, usize>(()),
        )
    }
}

#[test]
fn a_match_vertex_reusing_a_yielded_name_is_that_vertex_not_a_cross_product() {
    let values =
        [10, 20, 30, 40, 50].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let run = |text: &str| {
        prepare(text, &GqlParameters::new())
            .execute_governed_with_procedures(
                wide(),
                graph(&values),
                |call, arguments, _| host(call, arguments),
                || Ok(()),
            )
            .unwrap()
            .value
    };
    let vertex = |id| GraphValue::Vertex(VId(id));
    // t.scores() yields (1,1), (2,3), (3,2), (4,5); all four vertices exist.
    let joined =
        run("CALL t.scores() YIELD vertex AS n, score MATCH (n) RETURN n, score ORDER BY score");
    assert_eq!(column(&joined, 0), [1, 3, 2, 4].map(vertex));
    let properties = run("CALL t.scores() YIELD vertex AS n, score MATCH (n) \
         WITH n.p AS p, score RETURN p, score ORDER BY score");
    assert_eq!(column(&properties, 0), [20, 40, 30, 50].map(int));
    let star = run("CALL t.scores() YIELD vertex AS n MATCH (n) WITH * RETURN n");
    assert_eq!(star.len(), 4);
    assert_eq!(star[0].len(), 1);
    // Control: a fresh MATCH name is the full product, 4 x 5 rows.
    assert_eq!(
        run("CALL t.scores() YIELD vertex MATCH (n) RETURN vertex, n").len(),
        20
    );
    for text in [
        "CALL t.scores() YIELD vertex AS r MATCH ()-[r]->() RETURN r",
        "CALL t.scores() YIELD vertex AS q MATCH q = (a)-[]->(b) RETURN q",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "an edge or path rebound a yielded name: {text}"
        );
    }
}

#[test]
fn a_return_reads_a_correlated_vertex_properties_without_a_with() {
    let values =
        [10, 20, 30, 40, 50].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let run = |text: &str| {
        prepare(text, &GqlParameters::new())
            .execute_governed_with_procedures(
                wide(),
                graph(&values),
                |call, arguments, _| host(call, arguments),
                || Ok(()),
            )
            .unwrap()
            .value
    };
    // t.scores() yields (1,1), (2,3), (3,2), (4,5); VId(i) has p = 10 * (i + 1).
    let named = run("CALL t.scores() YIELD vertex AS n, score MATCH (n) \
         RETURN n.p AS p, score ORDER BY score");
    assert_eq!(column(&named, 0), [20, 40, 30, 50].map(int));
    // Unaliased, `n.p` is named by its text, as a MATCH-first RETURN names it.
    let query = PreparedGraphSetText::prepare(
        "CALL t.scores() YIELD vertex AS n, score MATCH (n) RETURN n.p, score ORDER BY score DESC",
        symbols,
    )
    .unwrap();
    assert_eq!(query.columns(), ["n.p", "score"]);
    // The correlated column is carried as the vertex itself, and the hidden
    // property read stays out of RETURN *.
    let star = run("CALL t.scores() YIELD vertex AS n, score MATCH (n) RETURN * ORDER BY score");
    assert_eq!(star[0].len(), 2);
    assert_eq!(
        column(&star, 0),
        [1, 3, 2, 4].map(|id| GraphValue::Vertex(VId(id)))
    );
}

/// fgdb-luq0b: `YIELD n RETURN n.p` reads through the identity `MATCH (n)`
/// the statement could have written, byte for byte, and the host learns
/// which outputs it must supply as vertices.
#[test]
fn a_property_read_of_a_yielded_output_implies_its_identity_match() {
    let values =
        [10, 20, 30, 40, 50].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let seen = RefCell::new(Vec::new());
    let run = |text: &str| {
        prepare(text, &GqlParameters::new())
            .execute_governed_with_procedures(
                wide(),
                graph(&values),
                |call, arguments, _| {
                    seen.borrow_mut().push(call.vertex_outputs().to_vec());
                    host(call, arguments)
                },
                || Ok(()),
            )
            .unwrap()
            .value
    };
    let template = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes()
    };
    // t.scores() yields (1,1), (2,3), (3,2), (4,5); VId(i) has p = 10 * (i + 1).
    for (implied, written) in [
        (
            "CALL t.scores() YIELD vertex AS n, score RETURN n.p AS p, score ORDER BY score",
            "CALL t.scores() YIELD vertex AS n, score MATCH (n) RETURN n.p AS p, score ORDER BY score",
        ),
        (
            "CALL t.scores() YIELD vertex AS n, score WITH n.p AS p, score RETURN p ORDER BY p",
            "CALL t.scores() YIELD vertex AS n, score MATCH (n) WITH n.p AS p, score RETURN p ORDER BY p",
        ),
    ] {
        assert_eq!(template(implied), template(written), "{implied}");
        assert_eq!(run(implied), run(written), "{implied}");
    }
    assert_eq!(
        column(
            &run("CALL t.scores() YIELD vertex AS n, score RETURN n.p AS p, score ORDER BY score"),
            0
        ),
        [20, 40, 30, 50].map(int)
    );
    // A row WHERE right after the CALL reads the same property.
    seen.borrow_mut().clear();
    let filtered = run(
        "CALL t.scores() YIELD score, vertex WHERE vertex.p > 20 RETURN vertex.p, score ORDER BY score",
    );
    assert_eq!(column(&filtered, 0), [40, 30, 50].map(int));
    assert_eq!(*seen.borrow(), [vec![1]]);
    // A read through a scalar output still implies the match: the host is
    // told, and refuses (Prism does), instead of silently matching nothing.
    let query = prepare(
        "CALL t.scores() YIELD vertex, score RETURN score.p",
        &GqlParameters::new(),
    );
    assert!(
        query
            .execute_governed_with_procedures(
                wide(),
                graph(&values),
                |call, _, _| {
                    assert_eq!(call.vertex_outputs(), [1]);
                    Err(GqlQueryError::Source("score is not a vertex"))
                },
                || Ok(()),
            )
            .is_err()
    );
    // Unchanged: no read, no implied match; UNWIND columns and later stages
    // are never implied.
    seen.borrow_mut().clear();
    run("CALL t.scores() YIELD vertex, score RETURN vertex, score");
    assert_eq!(*seen.borrow(), [Vec::<usize>::new()]);
    // Their property reads stay row reads: the host is told of no vertex
    // output, and an unaliased read is named by its text, as openCypher does.
    for text in [
        "CALL t.echo(1) YIELD a UNWIND [1] AS x RETURN x.p",
        "CALL t.scores() YIELD vertex AS n, score WITH n, score WITH n RETURN n.p",
    ] {
        seen.borrow_mut().clear();
        let _ = prepare(text, &GqlParameters::new()).execute_governed_with_procedures(
            wide(),
            graph(&values),
            |call, arguments, _| {
                seen.borrow_mut().push(call.vertex_outputs().to_vec());
                host(call, arguments)
            },
            || Ok(()),
        );
        assert!(!seen.borrow().is_empty(), "{text}");
        assert!(seen.borrow().iter().all(Vec::is_empty), "{text}");
    }
    let unwound = PreparedGraphSetText::prepare("UNWIND [1, 2] AS x RETURN x.p", symbols).unwrap();
    assert_eq!(unwound.columns(), ["x.p"]);
}

#[derive(Clone, Copy)]
struct MetadataCatalog;
impl GraphSymbolResolver for MetadataCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
            _ => None,
        }
    }
    fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
        let mut catalog = ReverseSymbolCatalog::new();
        catalog.insert_property(P, "p");
        catalog.insert_relation(RelationId(1), "R");
        catalog.insert_label(LabelId(1), "Person");
        Some(catalog)
    }
}

struct MetadataGraph {
    properties: [CanonicalPropertyValue; 5],
    maps: Vec<GraphValue>,
    labels: Vec<GraphValue>,
    edge_type: CanonicalScalar,
    visible: Vec<VId>,
}

impl MetadataGraph {
    fn new(visible: Vec<VId>) -> Self {
        let properties = [10, 20, 30, 40, 50].map(CanonicalScalar::Int);
        let maps = properties
            .iter()
            .cloned()
            .map(|value| GraphValue::map(vec![("p".into(), GraphValue::Scalar(value))]).unwrap())
            .collect();
        Self {
            properties: properties.map(CanonicalPropertyValue::from),
            maps,
            labels: vec![GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("Person").unwrap(),
            )],
            edge_type: CanonicalScalar::ucs_basic_text("R").unwrap(),
            visible,
        }
    }

    fn source(
        &self,
        pattern: &fgdb_gql::algebra::PreparedGraphPattern<GraphValueRow>,
        policy: GqlQueryPolicy,
    ) -> Rows {
        let edges = [
            (EId(11), VId(1), RelationId(1), VId(2)),
            (EId(12), VId(2), RelationId(1), VId(3)),
        ];
        pattern.plan().execute_governed_with_element_maps(
            self.visible.len() as u64 + edges.len() as u64,
            self.visible.iter().copied(),
            edges.into_iter().filter(|(_, source, _, destination)| {
                self.visible.contains(source) && self.visible.contains(destination)
            }),
            |vid, predicates| {
                Ok::<_, &'static str>(predicates.iter().all(|predicate| {
                    predicate.matches(
                        &[LabelId(1)],
                        &[(P, self.properties[vid.0 as usize].clone())],
                    )
                }))
            },
            |vid, _| Ok(Some(&self.properties[vid.0 as usize])),
            |_, _| Ok(None),
            |_| Ok(Some(self.labels.as_slice())),
            |_| Ok(Some(&self.edge_type)),
            |vid| Ok(Some(&self.maps[vid.0 as usize])),
            |_| Ok(None),
            policy,
            || Ok::<_, usize>(()),
        )
    }

    fn query(
        &self,
        text: &str,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, Fault> {
        PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &[],
            &mut MetadataCatalog,
        )
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .execute_governed_with_procedures(
            policy,
            |pattern, remaining| self.source(pattern, remaining),
            |call, arguments, _| host(call, arguments),
            || Ok(()),
        )
    }
}

#[test]
fn yielded_labels_and_complete_maps_use_the_native_graph_boundary() {
    let graph = MetadataGraph::new((0..5).map(VId).collect());
    let prefix = "CALL t.scores() YIELD vertex AS n, score ";
    let suffix = "RETURN labels(n) AS tags, properties(n) AS props, keys(n) AS keys, \
        n{.*,score:score} AS entry ORDER BY score";
    let implied = format!("{prefix}{suffix}");
    let written = format!("{prefix}MATCH (n) {suffix}");
    let template = |text: &str| {
        PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &[],
            &mut MetadataCatalog,
        )
        .unwrap()
        .canonical_template_bytes()
    };
    assert_eq!(template(&implied), template(&written));
    let result = graph.query(&implied, wide()).unwrap().value;
    assert_eq!(result, graph.query(&written, wide()).unwrap().value);
    assert_eq!(result.len(), 4);
    for (row, (vertex, score)) in result.iter().zip([(1, 1), (3, 2), (2, 3), (4, 5)]) {
        assert_eq!(row.values()[0].as_list().unwrap(), graph.labels);
        assert_eq!(row.values()[1], graph.maps[vertex]);
        assert_eq!(
            row.values()[2].as_list().unwrap(),
            [GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text("p").unwrap()
            )],
        );
        assert_eq!(
            row.values()[3],
            GraphValue::map(vec![
                ("p".into(), int((vertex as i64 + 1) * 10)),
                ("score".into(), int(score)),
            ])
            .unwrap(),
        );
    }
    for projection in ["n,score", "*"] {
        let carried = format!("{prefix}WITH {projection} {suffix}");
        assert_eq!(result, graph.query(&carried, wide()).unwrap().value);
    }
    let projected = format!(
        "{prefix}WITH labels(n) AS tags,properties(n) AS props,score \
         RETURN tags,props ORDER BY score"
    );
    assert_eq!(
        column(&result, 1),
        column(&graph.query(&projected, wide()).unwrap().value, 1),
    );
    let renamed = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        "CALL t.scores() YIELD vertex AS n WHERE size(labels(n)) > 0 RETURN LABELS(n)",
        &[],
        &mut MetadataCatalog,
    )
    .unwrap();
    assert_eq!(renamed.columns(), ["LABELS(n)"]);
    assert_eq!(
        graph
            .query(
                "CALL t.scores() YIELD vertex AS n WHERE size(labels(n)) > 0 \
                 RETURN LABELS(n)",
                wide(),
            )
            .unwrap()
            .value
            .len(),
        4,
    );
}

#[test]
fn procedure_metadata_preserves_source_masks_budgets_and_scalar_refusals() {
    let statement = "CALL t.scores() YIELD vertex AS n,score RETURN labels(n),properties(n),score";
    let graph = MetadataGraph::new((0..5).map(VId).collect());
    let visible = MetadataGraph::new(vec![VId(0), VId(1), VId(3)]);
    let full = graph.query(statement, wide()).unwrap();
    let masked = visible.query(statement, wide()).unwrap().value;
    assert_eq!(masked.len(), 2);
    assert_eq!(column(&masked, 2), [int(1), int(2)]);
    assert_ne!(masked, full.value);
    assert_eq!(
        visible
            .query(
                "CALL t.scores() YIELD vertex AS n,score MATCH (n) \
                 RETURN labels(n),properties(n),score",
                wide(),
            )
            .unwrap()
            .value,
        masked,
    );
    assert!(
        graph
            .query(
                statement,
                GqlQueryPolicy::new(u64::MAX, u64::MAX, 0, u64::MAX),
            )
            .is_err(),
    );

    let query = prepare(
        "CALL t.scores() YIELD score RETURN labels(score) LIMIT 0",
        &GqlParameters::new(),
    );
    let result = query.execute_governed_with_procedures(
        wide(),
        |_, _| panic!("an invalid graph output must fail at procedure admission"),
        |call, _, _| {
            assert_eq!(call.vertex_outputs(), [0]);
            Err(GqlQueryError::Source("score is not a vertex"))
        },
        || Ok::<_, usize>(()),
    );
    assert!(result.is_err());
}

#[test]
fn generic_map_outputs_remain_values_and_metadata_scope_does_not_escape() {
    for expression in ["properties(m)", "keys(m)", "m{.*}"] {
        let text = format!("CALL t.echo({{p:7}}) YIELD m RETURN {expression} AS value");
        let query = prepare(&text, &GqlParameters::new());
        let result = query
            .execute_governed_with_procedures(
                wide(),
                no_graph,
                |call, arguments, _| {
                    assert!(call.vertex_outputs().is_empty());
                    host(call, arguments)
                },
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1, "{text}");
    }
    for statement in [
        "CALL t.scores() YIELD vertex AS n WITH n WITH n RETURN labels(n)",
        "MATCH (n) WITH n.p AS n RETURN labels(n)",
        "CALL t.scores() YIELD vertex AS n UNWIND [1] AS x RETURN labels(x)",
    ] {
        assert!(
            PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
                statement,
                &[],
                &mut MetadataCatalog
            )
            .is_err(),
            "{statement}",
        );
    }
}

#[test]
fn carried_vertex_edge_and_path_metadata_matches_direct_native_projection() {
    let graph = MetadataGraph::new((0..5).map(VId).collect());
    for (direct, carried) in [
        (
            "MATCH (n) RETURN labels(n) AS tags",
            "MATCH (n) WITH n AS owner RETURN labels(owner) AS tags",
        ),
        // A matched vertex keeps its metadata through a later plain WITH
        // (fgdb-iznzx); a procedure's YIELD vertex still refuses above.
        (
            "MATCH (n) RETURN labels(n) AS tags",
            "MATCH (n) WITH n WITH n RETURN labels(n) AS tags",
        ),
        (
            "MATCH (a)-[r:R]->(b) RETURN type(r) AS kind",
            "MATCH (a)-[r:R]->(b) WITH r AS relationship RETURN type(relationship) AS kind",
        ),
        (
            "MATCH p = (a)-[:R*1..2]->(b) \
             RETURN length(p) AS hops,nodes(p) AS vertices,relationships(p) AS edges",
            "MATCH p = (a)-[:R*1..2]->(b) WITH p AS path \
             RETURN length(path) AS hops,nodes(path) AS vertices,relationships(path) AS edges",
        ),
    ] {
        let expected = graph.query(direct, wide()).unwrap().value;
        assert!(!expected.is_empty(), "{direct}");
        assert_eq!(
            graph.query(carried, wide()).unwrap().value,
            expected,
            "{carried}",
        );
    }
}

#[test]
fn fnx_projection_schema_names_bind_once_before_procedure_execution() {
    let text = "CALL fnx.pagerank(label=>'Scoped',relation=>'LINK',weight=>'weight') \
                YIELD score RETURN score";
    let mut resolved = Vec::new();
    let prepared = PreparedGraphSetText::prepare(text, |kind, name: &str| {
        resolved.push((kind, name.to_owned()));
        match (kind, name) {
            (GraphSymbolKind::Label, "Scoped") => Some(GraphSymbol::Label(LabelId(7))),
            (GraphSymbolKind::Relation, "LINK") => Some(GraphSymbol::Relation(RelationId(11))),
            (GraphSymbolKind::Property, "weight") => Some(GraphSymbol::Property(PropertyKeyId(13))),
            _ => None,
        }
    })
    .unwrap();
    assert_eq!(
        resolved,
        [
            (GraphSymbolKind::Label, "Scoped".to_owned()),
            (GraphSymbolKind::Relation, "LINK".to_owned()),
            (GraphSymbolKind::Property, "weight".to_owned()),
        ],
    );
    for _ in 0..2 {
        let result = prepared
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .execute_governed_with_procedures(
                wide(),
                no_graph,
                |call, arguments, _| {
                    assert_eq!(call.namespace(), "fnx");
                    assert_eq!(call.name(), "pagerank");
                    assert_eq!(call.argument_names(), ["label", "relation", "weight"]);
                    assert_eq!(arguments, [int(7), int(11), int(13)]);
                    rows(vec![vec![int(3)]])
                },
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(column(&result.value, 0), [int(3)]);
    }

    // An ordinary procedure's named values are not catalog selectors.
    let generic = PreparedGraphSetText::prepare(
        "CALL t.echo(label=>'Scoped',relation=>'LINK',weight=>'weight') \
         YIELD a,b,c RETURN a,b,c",
        |_, _| panic!("generic named arguments must not touch the catalog"),
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let result = call(&generic).unwrap();
    assert_eq!(
        result[0].values(),
        ["Scoped", "LINK", "weight"]
            .map(|value| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap())),
    );
}

#[test]
fn invalid_fnx_projection_schema_selectors_refuse_during_preparation() {
    for argument in ["label", "relation", "weight"] {
        for value in ["7", "$selector", "'Sco' + 'ped'", "NULL", "{name:'Scoped'}"] {
            let text = format!("CALL fnx.pagerank({argument}=>{value}) YIELD score RETURN score");
            let mut lookups = 0;
            assert!(
                PreparedGraphSetText::prepare(&text, |_, _| {
                    lookups += 1;
                    None
                })
                .is_err(),
                "{text}",
            );
            assert_eq!(lookups, 0, "{text}");
        }
        let text = format!("CALL fnx.pagerank({argument}=>'missing') YIELD score RETURN score");
        assert!(PreparedGraphSetText::prepare(&text, |_, _| None).is_err());
        assert!(
            PreparedGraphSetText::prepare(&text, |kind, _| {
                Some(match kind {
                    GraphSymbolKind::Label => GraphSymbol::Relation(RelationId(1)),
                    _ => GraphSymbol::Label(LabelId(1)),
                })
            })
            .is_err(),
            "{text}",
        );
    }
}
