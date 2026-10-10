//! Native computed RETURN shares MATCH/scalar preparation and set execution.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphIntegerErrorKind, GraphPatternTextErrorKind, GraphSetBuildError, GraphSetExecutionError,
    GraphSetTextErrorKind, GraphSymbol, GraphSymbolKind, MAX_GRAPH_SET_DEPTH, PreparedGraphSet,
    PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);
type QueryResult =
    Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, ()>>;
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 1_000_000)
}
fn run(query: &PreparedGraphSet, vertices: &[VId], edges: &[Triple], props: &Props) -> QueryResult {
    query.execute_governed(
        policy(),
        |pattern, budget| {
            pattern.plan().execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vid, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            [],
                            props.iter().filter_map(|(&(owner, key), value)| {
                                (owner == vid).then_some((key, value))
                            }),
                        )
                    }))
                },
                |vid, key| Ok(props.get(&(vid, key))),
                budget,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    )
}
fn ints(rows: &[GraphValueRow]) -> Vec<Vec<Option<i64>>> {
    rows.iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| match value.as_scalar().unwrap() {
                    CanonicalScalar::Int(value) => Some(*value),
                    CanonicalScalar::Null => None,
                    _ => panic!("unexpected scalar fixture kind"),
                })
                .collect()
        })
        .collect()
}

#[test]
fn computed_order_keys_rank_before_paging_and_do_not_escape_the_return_schema() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(2)),
        ((VId(1), Q), CanonicalScalar::Int(0)),
        ((VId(2), P), CanonicalScalar::Int(1)),
        ((VId(2), Q), CanonicalScalar::Int(2)),
        ((VId(3), P), CanonicalScalar::Int(3)),
        ((VId(3), Q), CanonicalScalar::Int(1)),
    ]);
    let text = "MATCH (n) RETURN n.p AS p \
                ORDER BY n.p + n.q * $weight DESC, p ASC SKIP $skip LIMIT $count";
    let template = PreparedGraphSetText::prepare(text, symbols).unwrap();
    assert_eq!(template.columns(), &["p"]);
    for (weight, skip, expected) in [(2, 0, 1), (2, 1, 3), (-2, 0, 2)] {
        let args = GqlParameters::new()
            .with_int64("weight", weight)
            .unwrap()
            .with_uint64("skip", skip)
            .unwrap()
            .with_uint64("count", 1)
            .unwrap();
        let query = template.bind_parameters(&args).unwrap();
        assert_eq!(query.columns(), &["p"]);
        for vertices in [[VId(3), VId(1), VId(2)], [VId(2), VId(1), VId(3)]] {
            let execution = run(&query, &vertices, &[], &props).unwrap();
            assert_eq!(ints(&execution.value), vec![vec![Some(expected)]]);
            assert_eq!(execution.rows.snapshot_records, 3);
            assert_eq!(execution.rows.result_rows, 1);
        }
    }
    let ids = prepare("MATCH (n) RETURN n.p AS p ORDER BY id(n) DESC");
    assert_eq!(
        ints(
            &run(&ids, &[VId(1), VId(2), VId(3)], &[], &props)
                .unwrap()
                .value
        ),
        vec![vec![Some(3)], vec![Some(1)], vec![Some(2)]]
    );
}

#[test]
fn computed_order_uses_return_aliases_and_preserves_all_rows_under_distinct() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(2), P), CanonicalScalar::Int(1)),
        ((VId(3), P), CanonicalScalar::Int(3)),
    ]);
    let query = prepare("MATCH (n) RETURN DISTINCT n.p + 10 AS score ORDER BY -score");
    assert_eq!(
        ints(
            &run(&query, &[VId(3), VId(1), VId(2)], &[], &props)
                .unwrap()
                .value
        ),
        vec![vec![Some(13)], vec![Some(11)]]
    );
    // A function of the projected element cannot split its DISTINCT class.
    for text in [
        "MATCH (n) RETURN DISTINCT n ORDER BY n.p + 0 DESC",
        "MATCH (n) WITH n MATCH (n) RETURN DISTINCT n ORDER BY n.p + 0 DESC",
        "MATCH (original) WITH original AS n MATCH (n) \
         RETURN DISTINCT n ORDER BY n.p + 0 DESC",
    ] {
        let query = prepare(text);
        let rows = run(&query, &[VId(1), VId(3), VId(2), VId(1)], &[], &props)
            .unwrap()
            .value;
        assert_eq!(
            rows.iter()
                .map(|row| row.get(0).unwrap().as_vertex().unwrap())
                .collect::<Vec<_>>(),
            vec![VId(3), VId(1), VId(2)],
            "{text}"
        );
    }
    for text in [
        "MATCH (n) RETURN DISTINCT n.p AS p ORDER BY n.q + 1",
        "UNWIND [1,2] AS x RETURN DISTINCT 0 AS zero ORDER BY x + 1",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "semantic refusal precedes graph resolution");
    }
}

#[test]
fn computed_with_and_multipart_return_keep_their_scope_and_selected_occurrences() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(2), P), CanonicalScalar::Int(3)),
        ((VId(3), P), CanonicalScalar::Int(2)),
    ]);
    let vertices = [VId(3), VId(2), VId(1)];
    let query = prepare(
        "MATCH (n) WITH n ORDER BY -n.p LIMIT 2 \
                         RETURN n.p AS p ORDER BY -p",
    );
    assert_eq!(
        ints(&run(&query, &vertices, &[], &props).unwrap().value),
        vec![vec![Some(3)], vec![Some(2)]]
    );
    let query = prepare(
        "MATCH (n) WITH n ORDER BY -n.p LIMIT 2 \
                         MATCH (n) RETURN n.p AS p ORDER BY -p",
    );
    assert_eq!(
        ints(&run(&query, &vertices, &[], &props).unwrap().value),
        vec![vec![Some(3)], vec![Some(2)]]
    );
    let query = prepare(
        "MATCH (n) WITH n MATCH (n) WITH n.p AS p \
                         RETURN p ORDER BY -p SKIP 1 LIMIT 1",
    );
    assert_eq!(
        ints(&run(&query, &vertices, &[], &props).unwrap().value),
        vec![vec![Some(2)]]
    );
    let query = prepare(
        "UNWIND [3,1,2] AS x WITH x ORDER BY -x LIMIT 2 \
                         RETURN x % 2 AS parity ORDER BY x + 0",
    );
    assert_eq!(
        ints(&run(&query, &[], &[], &Props::new()).unwrap().value),
        vec![vec![Some(0)], vec![Some(1)]]
    );
}

#[test]
fn computed_sort_errors_and_work_limits_cannot_hide_behind_a_page() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(2), P), CanonicalScalar::Int(2)),
    ]);
    for limit in [0, 1] {
        let query = prepare(&format!(
            "MATCH (n) RETURN n ORDER BY 1 / (n.p - 2) LIMIT {limit}"
        ));
        assert!(matches!(
            run(&query, &[VId(1), VId(2)], &[], &props),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::Projection { .. }
            ))
        ));
    }
    let query = prepare("UNWIND [3,1,2] AS x RETURN x ORDER BY x * -1 LIMIT 1");
    let execute = |budget| {
        query.execute_governed(
            budget,
            |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
                panic!("source-free sorting must not open a graph source")
            },
            || Ok::<_, ()>(()),
        )
    };
    let complete = execute(policy()).unwrap();
    assert_eq!(ints(&complete.value), vec![vec![Some(3)]]);
    assert_eq!(complete.rows.snapshot_records, 0);
    assert_eq!(complete.rows.result_rows, 1);
    assert!(matches!(
        execute(GqlQueryPolicy::new(
            0,
            1,
            complete.evaluator.work_units - 1,
            u64::MAX
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert!(matches!(
        execute(GqlQueryPolicy::new(
            0,
            1,
            u64::MAX,
            complete.evaluator.scratch_entries - 1
        )),
        Err(GqlQueryError::Evaluator(_))
    ));
    assert!(matches!(
        execute(GqlQueryPolicy::new(0, 0, u64::MAX, u64::MAX)),
        Err(GqlQueryError::Rows(_))
    ));
}

#[test]
fn computed_order_identity_pins_each_expression_and_parameter_without_changing_plain_pages() {
    let template = |text| PreparedGraphSetText::prepare(text, symbols).unwrap();
    let first = template("MATCH (n) RETURN n.p AS p ORDER BY n.q + $step");
    let second = template("MATCH (n) RETURN n.p AS p ORDER BY n.q - $step");
    assert_ne!(
        first.canonical_template_bytes(),
        second.canonical_template_bytes()
    );
    let one = GqlParameters::new().with_int64("step", 1).unwrap();
    let two = GqlParameters::new().with_int64("step", 2).unwrap();
    assert_ne!(
        first.bind_parameters(&one).unwrap().canonical_bytes(),
        first.bind_parameters(&two).unwrap().canonical_bytes()
    );
    let plain = template("MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2");
    let leaf: PreparedGraphSet = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into();
    let typed = leaf
        .with_order_by(&[fgdb_gql::algebra::GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(0, Some(2));
    assert_eq!(
        plain
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes(),
        typed.canonical_bytes()
    );
}

#[test]
fn computed_text_list_and_null_keys_share_the_native_expression_language() {
    let query = prepare(
        "UNWIND ['beta','Alpha','alpha',null] AS x \
                         RETURN x ORDER BY toUpper(x) ASC NULLS LAST",
    );
    let rows = run(&query, &[], &[], &Props::new()).unwrap().value;
    let expected =
        ["Alpha", "alpha", "beta"].map(|value| CanonicalScalar::ucs_basic_text(value).unwrap());
    assert_eq!(rows.len(), 4);
    for (row, expected) in rows[..3].iter().zip(&expected) {
        assert_eq!(row.len(), 1);
        assert_eq!(row.get(0).unwrap().as_scalar(), Some(expected));
    }
    assert_eq!(
        rows[3].get(0).unwrap().as_scalar(),
        Some(&CanonicalScalar::Null)
    );
    let query = prepare("UNWIND [1,2,3,4] AS x RETURN x ORDER BY [x % 2, -x]");
    assert_eq!(
        ints(&run(&query, &[], &[], &Props::new()).unwrap().value),
        vec![vec![Some(4)], vec![Some(2)], vec![Some(3)], vec![Some(1)]]
    );
}

#[test]
fn computed_order_cancellation_never_returns_a_selected_prefix() {
    let query = prepare("UNWIND [3,1,2] AS x RETURN x ORDER BY x * -1 LIMIT 1");
    let calls = Cell::new(0_usize);
    let execute = |stop| {
        query.execute_governed(
            policy(),
            |_, _| -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), usize>> {
                panic!("source-free sorting must not open a graph source")
            },
            || {
                let at = calls.get();
                calls.set(at + 1);
                if at == stop { Err(at) } else { Ok(()) }
            },
        )
    };
    execute(usize::MAX).unwrap();
    let complete = calls.get();
    assert!(complete > 0);
    for stop in 0..complete {
        calls.set(0);
        assert!(matches!(execute(stop), Err(GqlQueryError::Interrupted(at)) if at == stop));
    }
}

#[test]
fn precedence_nullable_functions_and_public_column_order_match_scalar_semantics() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(-7)),
        ((VId(1), Q), CanonicalScalar::Int(2)),
    ]);
    let query =
        prepare("MATCH (n) RETURN n.p/n.q AS quotient,n.p%n.q AS remainder,-n.p+n.q*3 AS total");
    assert_eq!(query.columns(), &["quotient", "remainder", "total"]);
    assert_eq!(
        ints(&run(&query, &[VId(1)], &[], &props).unwrap().value),
        vec![vec![Some(-3), Some(-1), Some(13)]]
    );
    for (text, expected) in [
        ("COALESCE(n.p,1/0)", Some(-7)),
        ("COALESCE(NULL,NULL,n.q)+ABS(n.p)", Some(9)),
        ("n.p/NULLIF(n.q,2)", None),
        ("(-9223372036854775808)%-1", Some(0)),
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {text} AS value"));
        assert_eq!(
            ints(&run(&query, &[VId(1)], &[], &props).unwrap().value),
            vec![vec![expected]],
            "{text}"
        );
    }
}

#[test]
fn constants_and_computed_distinct_preserve_walk_and_optional_bags() {
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    for (quantifier, expected) in [("", 11), ("DISTINCT ", 1)] {
        let query = prepare(&format!(
            "MATCH WALK (a)-[:R*0..2]->(b) RETURN {quantifier}7 AS value"
        ));
        let rows = run(&query, &[VId(1), VId(2)], &edges, &Props::new())
            .unwrap()
            .value;
        assert_eq!(rows.len(), expected);
        assert!(ints(&rows).iter().all(|row| row == &[Some(7)]));
    }
    let optional =
        prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN COALESCE(b.p,0)+1 AS value");
    let rows = run(
        &optional,
        &[VId(1), VId(2)],
        &[(VId(1), R, VId(2)), (VId(1), R, VId(2))],
        &Props::new(),
    )
    .unwrap()
    .value;
    assert_eq!(ints(&rows), vec![vec![Some(1)]; 3]);
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(-2)),
        ((VId(2), P), CanonicalScalar::Int(2)),
        ((VId(3), P), CanonicalScalar::Int(-3)),
        ((VId(4), P), CanonicalScalar::Int(3)),
    ]);
    let query =
        prepare("MATCH (n) RETURN DISTINCT n.p*n.p AS square ORDER BY square DESC SKIP 1 LIMIT 1");
    assert_eq!(
        ints(
            &run(&query, &[VId(1), VId(2), VId(3), VId(4)], &[], &props)
                .unwrap()
                .value
        ),
        vec![vec![Some(4)]]
    );
}

#[test]
fn computed_arms_obey_set_precedence_and_parenthesized_local_selection() {
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(2), P), CanonicalScalar::Int(2)),
    ]);
    let query = prepare(
        "MATCH (n) RETURN n.p*2 AS value UNION ALL MATCH (m) RETURN m.p+1 AS other \
        INTERSECT MATCH (k) RETURN 2 AS constant ORDER BY value DESC",
    );
    assert_eq!(
        ints(&run(&query, &[VId(1), VId(2)], &[], &props).unwrap().value),
        vec![vec![Some(4)], vec![Some(2)], vec![Some(2)]]
    );
    let nested = prepare(
        "(MATCH (n) RETURN n.p*2 AS value ORDER BY value DESC LIMIT 1) \
        UNION ALL (MATCH (m) RETURN m.p+1 AS other ORDER BY other LIMIT 1) ORDER BY value",
    );
    assert_eq!(
        ints(&run(&nested, &[VId(1), VId(2)], &[], &props).unwrap().value),
        vec![vec![Some(2)], vec![Some(4)]]
    );
    for text in [
        "MATCH (n) RETURN n,n.p AS p",
        "MATCH (a)-[:R]->(b) RETURN DISTINCT b,a AS owner",
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN *",
    ] {
        let plain: PreparedGraphSet = PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .into();
        assert_eq!(
            prepare(text).canonical_bytes(),
            plain.canonical_bytes(),
            "{text}"
        );
    }
}

#[test]
fn parameter_contract_and_catalog_cache_span_predicates_outputs_and_all_arms() {
    let text = "MATCH (n) WHERE n.p >= $x RETURN n.p+$x AS value UNION ALL \
        MATCH (m) RETURN COALESCE(m.p,$x) AS other ORDER BY value LIMIT $limit";
    let calls = Cell::new(0);
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(template.parameter_schema()[0].occurrences, 3);
    let args = GqlParameters::new()
        .with_int64("x", 5)
        .unwrap()
        .with_uint64("limit", 4)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    let frozen = query.canonical_bytes();
    assert_eq!(
        frozen,
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::MissingParameter)
    ));
    let payload = "secret ' UNION ALL MATCH (x) RETURN x";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let template = PreparedGraphSetText::prepare_with_parameter_types(
        "MATCH (n) RETURN $text AS payload",
        &[("text", GqlParameterType::Scalar(kind))],
        symbols,
    )
    .unwrap();
    let query = template
        .bind_parameters(&GqlParameters::new().with_text("text", payload).unwrap())
        .unwrap();
    let rows = run(&query, &[VId(1)], &[], &Props::new()).unwrap().value;
    assert_eq!(
        rows[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::ucs_basic_text(payload).unwrap())
    );
    assert!(!format!("{template:?} {query:?} {rows:?}").contains(payload));
    let query = prepare("MATCH (n) RETURN 'x'' UNION ALL MATCH (m)' AS payload");
    assert_eq!(
        run(&query, &[VId(1)], &[], &Props::new())
            .unwrap()
            .value
            .len(),
        1
    );
}

#[test]
fn malformed_types_aliases_and_projection_depth_refuse_before_catalog_access() {
    for text in [
        "MATCH (n) RETURN n.p+ AS x",
        "MATCH (n) RETURN TRUE+1 AS x",
        "MATCH (n) RETURN n.p+1 AS x,n.q+1 AS x",
        "MATCH (n) RETURN m.p+1 AS x",
        "MATCH (n) RETURN COUNT(n) AS x",
        "MATCH (n) RETURN n.p+1 AS x ORDER BY p",
        "MATCH (n) RETURN n.p+1 AS x UNION MATCH (m) RETURN m",
        "MATCH (n) RETURN n.p+$x AS x UNION MATCH (m) RETURN m.p AS x LIMIT $x",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    // An unaliased computed RETURN value is named by its source text.
    let named = PreparedGraphSetText::prepare("MATCH (n) RETURN n.p+1, n.p * 2", symbols).unwrap();
    assert_eq!(named.columns(), ["n.p+1", "n.p * 2"]);
    let text = format!(
        "{}MATCH (n) RETURN n.p+1 AS x{}",
        "(".repeat(MAX_GRAPH_SET_DEPTH - 2),
        ")".repeat(MAX_GRAPH_SET_DEPTH - 2)
    );
    prepare(&text);
    let too_deep = format!("({text})");
    let calls = Cell::new(0);
    let failed = PreparedGraphSetText::prepare(&too_deep, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap_err();
    assert!(matches!(
        failed.kind,
        GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep { .. })
    ));
    assert_eq!(calls.get(), 0);
    let unicode = "\u{2003}MATCH (n) RETURN 1 AS x UNION MATCH (m) RETURN m.p+$missing AS y";
    let template = PreparedGraphSetText::prepare(unicode, symbols).unwrap();
    assert_eq!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .offset,
        unicode.find('$').unwrap()
    );
}

#[test]
fn lazy_arithmetic_cannot_hide_property_source_failure_or_late_overflow() {
    let query = prepare("MATCH (n) RETURN COALESCE(n.p,n.q) AS value LIMIT 0");
    let scalar = CanonicalScalar::Int(1);
    let result = query.execute_governed(
        policy(),
        |pattern, budget| {
            pattern.plan().execute_governed_with_properties(
                1,
                [VId(1)],
                [],
                |_, _| Ok::<_, &str>(true),
                |_, key| {
                    if key == P {
                        Ok(Some(&scalar))
                    } else {
                        Err("late source")
                    }
                },
                budget,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(
            "late source"
        )))
    ));
    let query = prepare("MATCH (n) RETURN n.p+1 AS value LIMIT 0");
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(0)),
        ((VId(2), P), CanonicalScalar::Int(i64::MAX)),
    ]);
    assert!(matches!(run(&query,&[VId(1),VId(2)],&[],&props),
        Err(GqlQueryError::Source(GraphSetExecutionError::Projection { row:1,error,.. })) if error.kind == GraphIntegerErrorKind::Overflow));
}
