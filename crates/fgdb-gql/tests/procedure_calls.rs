//! Appendix C `ProcedureCall`: `CALL ns.name(args) YIELD ...` starts a read
//! pipeline whose rows the host supplies. Every law here drives the real text
//! grammar, binder and relational executor; only the procedure is a test host.

use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphSetExecutionError, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText, PreparedProcedureCall,
};
use fgdb_types::{CanonicalScalar, VId};
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
    let values = [10, 20, 30].map(CanonicalScalar::Int);
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
    values: &[CanonicalScalar],
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
    let values = [10, 20, 30, 40, 50].map(CanonicalScalar::Int);
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
    let values = [10, 20, 30, 40, 50].map(CanonicalScalar::Int);
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
    // Unaliased, `n.p` is named `p`, exactly as a MATCH-first RETURN names it.
    let query = PreparedGraphSetText::prepare(
        "CALL t.scores() YIELD vertex AS n, score MATCH (n) RETURN n.p, score ORDER BY score DESC",
        symbols,
    )
    .unwrap();
    assert_eq!(query.columns(), ["p", "score"]);
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
    let values = [10, 20, 30, 40, 50].map(CanonicalScalar::Int);
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
    for text in [
        "UNWIND [1, 2] AS x RETURN x.p",
        "CALL t.echo(1) YIELD a UNWIND [1] AS x RETURN x.p",
        "CALL t.scores() YIELD vertex AS n, score WITH n, score WITH n RETURN n.p",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}
