//! Native multi-part reads execute real GLA leaves through one governed source.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::cell::RefCell;
use std::collections::BTreeSet;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const EDGES: [(VId, RelationId, VId); 5] = [
    (VId(1), R, VId(2)),
    (VId(1), R, VId(2)),
    (VId(1), R, VId(3)),
    (VId(2), R, VId(3)),
    (VId(3), R, VId(4)),
];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    run_reading(query, policy, checkpoint, false)
}
fn run_reading(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
    fail_elements: bool,
) -> Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
> {
    let values = [1, 2, 2, 3].map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed_with_elements(
        policy,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                (values.len() + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |vid, tests| {
                    Ok::<_, &'static str>(
                        tests.iter().all(|test| {
                            test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                        }),
                    )
                },
                |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                remaining,
                || (checkpoint.borrow_mut())(),
            )
        },
        |_, _, _| panic!("this fixture has no CALL source"),
        |element, key| {
            if fail_elements {
                return Err("element-read-error");
            }
            Ok(match element {
                ElementId::Vertex(vid) if key == P => Some(values[vid.0 as usize - 1].clone()),
                _ => None,
            })
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn execute(text: &str) -> Vec<GraphValueRow> {
    run(&prepare(text), wide(), &mut || Ok(())).unwrap().value
}
fn vertices(values: &[u128]) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .iter()
            .map(|id| GraphValue::Vertex(VId(*id)))
            .collect(),
    )
}
fn scalar_vertex(value: i64, vid: u128) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(CanonicalScalar::Int(value)),
        GraphValue::Vertex(VId(vid)),
    ])
}

#[test]
fn inline_call_imports_preserve_bags_nulls_and_local_pages() {
    let row = |cells: &[Option<i64>]| {
        GraphValueRow::from_owned_values(
            cells
                .iter()
                .map(|value| {
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
                })
                .collect(),
        )
    };
    assert_eq!(
        execute("CALL { RETURN 1 AS a } RETURN a"),
        vec![row(&[Some(1)])]
    );
    assert_eq!(
        execute("WITH 2 AS x,3 AS z CALL (z,x) { RETURN z-x AS y } RETURN x,z,y"),
        vec![row(&[Some(2), Some(3), Some(1)])]
    );
    assert_eq!(
        execute("WITH 2 AS x CALL { WITH x RETURN x+1 AS y } RETURN x,y"),
        vec![row(&[Some(2), Some(3)])]
    );
    let expected: Vec<_> = [Some(2), Some(2), None]
        .into_iter()
        .map(|value| row(&[value, value]))
        .collect();
    assert_eq!(
        execute("UNWIND [2,2,null] AS x CALL (x) { RETURN x AS y } RETURN x,y"),
        expected
    );
    assert_eq!(
        execute(
            "WITH 2 AS x CALL (x) { UNWIND [x,x+1] AS y RETURN y ORDER BY y DESC LIMIT 1 } RETURN x,y"
        ),
        vec![row(&[Some(2), Some(3)])]
    );
    assert!(execute("WITH 1 AS x CALL (x) { UNWIND [] AS y RETURN y } RETURN x,y").is_empty());
    assert_eq!(
        execute("WITH 3 AS x CALL (x) { CALL (x) { RETURN x*2 AS z } RETURN z+1 AS y } RETURN x,y"),
        vec![row(&[Some(3), Some(7)])]
    );
}

#[test]
fn inline_call_correlates_actual_graph_sources_before_export() {
    let expected: Vec<_> = EDGES
        .iter()
        .map(|(left, _, right)| vertices(&[left.0, right.0]))
        .collect();
    assert_eq!(
        execute("MATCH (n) CALL (n) { MATCH (n)-[:R]->(m) RETURN m } RETURN n,m ORDER BY n,m"),
        expected
    );
    let expected: Vec<_> = (1..=4)
        .map(|id| scalar_vertex([1, 2, 2, 3][id - 1], id as u128))
        .collect();
    assert_eq!(
        execute("MATCH (n) CALL (n) { RETURN n.p AS p } RETURN p,n ORDER BY n"),
        expected
    );
    assert_eq!(
        execute("CALL { MATCH (n) RETURN n } RETURN n.p AS p,n ORDER BY n"),
        expected
    );
    assert_eq!(
        execute("MATCH (n) CALL (n) { RETURN n.p+1 AS p } RETURN p,n ORDER BY n"),
        (1..=4)
            .map(|id| scalar_vertex([2, 3, 3, 4][id - 1], id as u128))
            .collect::<Vec<_>>()
    );
    let query = prepare("MATCH (n) CALL (n) { RETURN n.p AS p } RETURN p");
    assert!(run_reading(&query, wide(), &mut || Ok(()), true).is_err());
}

#[test]
fn inline_call_parameters_governance_and_scope_bounds_span_the_whole_definition() {
    let text = "WITH $outer AS x CALL (x) { RETURN x+$inner AS y } RETURN y+$tail AS total";
    let template = PreparedGraphSetText::prepare(text, |_, _: &str| {
        panic!("source-free CALL must not resolve a symbol")
    })
    .unwrap();
    let before = template.canonical_template_bytes();
    let arguments = GqlParameters::new()
        .with_int64("outer", 2)
        .unwrap()
        .with_int64("inner", 3)
        .unwrap()
        .with_int64("tail", 4)
        .unwrap();
    let query = template.bind_parameters(&arguments).unwrap();
    let expected = vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        CanonicalScalar::Int(9),
    )])];
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value, expected);
    assert_eq!(before, template.canonical_template_bytes());
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert!(
        template
            .bind_parameters(&arguments.with_int64("extra", 0).unwrap())
            .is_err()
    );
    assert!(
        run(
            &query,
            GqlQueryPolicy::new(u64::MAX, u64::MAX, 0, u64::MAX),
            &mut || Ok(())
        )
        .is_err()
    );
    let mut checkpoints = 0;
    run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    assert!(checkpoints > 0);
    for stop in 0..checkpoints {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            let current = seen;
            seen += 1;
            if current == stop { Err(stop) } else { Ok(()) }
        });
        assert!(
            matches!(result,Err(GqlQueryError::Interrupted(at)) if at==stop),
            "{stop}: {result:?}"
        );
    }
    for text in [
        "WITH 0 AS x CALL (x) { WITH 1/x AS y RETURN y LIMIT 0 } RETURN y",
        "WITH 0 AS x CALL (x) { RETURN 1/x AS y } RETURN y LIMIT 0",
    ] {
        assert!(
            run(&prepare(text), wide(), &mut || Ok(())).is_err(),
            "{text}"
        );
    }
    let nested = |count| {
        (0..count).fold("RETURN 1 AS x".to_owned(), |body, _| {
            format!("CALL {{ {body} }} RETURN x")
        })
    };
    assert_eq!(execute(&nested(8)).len(), 1);
    assert!(
        PreparedGraphSetText::prepare(&nested(9), |_, _: &str| panic!(
            "depth must refuse before catalog"
        ))
        .is_err()
    );
}

#[test]
fn inline_call_refuses_escaping_names_collisions_and_effects_before_catalog() {
    for text in [
        "WITH 1 AS x CALL (missing) { RETURN 2 AS y } RETURN y",
        "WITH 1 AS x CALL (x,x) { RETURN 2 AS y } RETURN y",
        "WITH 1 AS x CALL { RETURN x AS y } RETURN y",
        "WITH 1 AS x CALL (x) { RETURN x } RETURN x",
        "CALL { WITH 2 AS hidden RETURN 1 AS y } RETURN hidden",
        "CALL { MATCH (n) RETURN n } WITH 1 AS x RETURN n.p",
        "CALL { CREATE (n) RETURN n } RETURN n",
        "CALL { MATCH (n) DELETE n RETURN 1 AS y } RETURN y",
    ] {
        let calls = std::cell::Cell::new(0);
        let result = PreparedGraphSetText::prepare(text, |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        });
        assert!(result.is_err(), "{text}");
        assert_eq!(
            calls.get(),
            0,
            "refusal must precede catalog access: {text}"
        );
    }
}

#[test]
fn required_continuation_preserves_parallel_edges_and_applies_input_page_first() {
    // This exact form used to be on with_pipelines' refusal list. Independent
    // edge enumeration determines its positive result, including parallel edges.
    let query = prepare("MATCH (n) WITH n MATCH (n)-[:R]->(m) RETURN n,m ORDER BY n,m");
    let result = run(&query, wide(), &mut || Ok(())).unwrap();
    let expected: Vec<_> = EDGES
        .iter()
        .map(|(a, _, b)| vertices(&[a.0, b.0]))
        .collect();
    assert_eq!(result.value, expected);
    assert_eq!(result.rows.snapshot_records, 18);
    assert_eq!(query.operand_count(), 2);
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m ORDER BY m"),
        vec![vertices(&[2]), vertices(&[2]), vertices(&[3])],
    );
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY n LIMIT 1 WHERE n IS NULL MATCH (n)-[:R]->(m) RETURN m"),
        Vec::<GraphValueRow>::new(),
    );
}

#[test]
fn renamed_identity_and_carried_scalar_survive_three_graph_parts() {
    let text = "MATCH (n) WITH n AS root,n.p AS score WHERE score=1 \
        MATCH (root)-[:R]->(m) WITH m AS root,score+1 AS score \
        MATCH (root)-[:R]->(destination) RETURN score,destination ORDER BY destination";
    let mut expected = Vec::new();
    for &(source, _, middle) in &EDGES {
        if source != VId(1) {
            continue;
        }
        for &(next, _, destination) in &EDGES {
            if middle == next {
                expected.push(scalar_vertex(2, destination.0));
            }
        }
    }
    expected.sort();
    assert_eq!(execute(text), expected);
    assert_eq!(
        execute("MATCH (n) WITH n AS owner MATCH (owner) RETURN owner.p AS p ORDER BY p"),
        [1, 2, 2, 3]
            .map(
                |n| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                    CanonicalScalar::Int(n)
                ),])
            )
            .to_vec(),
    );
}

#[test]
fn repeated_property_values_join_as_bags_and_null_unwind_keys_never_match() {
    let expected = vec![
        scalar_vertex(2, 2),
        scalar_vertex(2, 2),
        scalar_vertex(2, 3),
        scalar_vertex(2, 3),
    ];
    assert_eq!(
        execute(
            "MATCH (n) WITH n.p AS wanted WHERE wanted=2 MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
        ),
        expected
    );
    assert_eq!(
        execute(
            "UNWIND [2,2,NULL] AS wanted WITH wanted MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
        ),
        expected
    );
    assert_eq!(
        execute(
            "MATCH (n) WITH DISTINCT n.p AS wanted WHERE wanted=2 MATCH (m {p:wanted}) \
         RETURN wanted,m ORDER BY m"
        ),
        vec![scalar_vertex(2, 2), scalar_vertex(2, 3)]
    );
}

#[test]
fn null_imports_from_optional_match_are_not_rebound_to_arbitrary_vertices() {
    assert_eq!(
        execute(
            "MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) WITH m \
         MATCH (m)-[:R]->(destination) RETURN destination ORDER BY destination"
        ),
        vec![
            vertices(&[3]),
            vertices(&[3]),
            vertices(&[4]),
            vertices(&[4])
        ]
    );
}

#[test]
fn one_parameter_and_catalog_contract_spans_all_parts_and_rebinding() {
    let text = "\u{2003}MATCH (n) WHERE n.p=$start WITH n LIMIT $top \
        MATCH (n)-[:R]->(m) WHERE m.p >= $min RETURN m.p+$add AS score";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        assert!(
            calls.insert((kind, name.to_owned())),
            "duplicate catalog callback"
        );
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(template.parameter_schema().len(), 4);
    let before = template.canonical_template_bytes();
    let args = GqlParameters::new()
        .with_int64("start", 1)
        .unwrap()
        .with_uint64("top", 1)
        .unwrap()
        .with_int64("min", 2)
        .unwrap();
    let missing = template.bind_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$add").unwrap());
    let a = template
        .bind_parameters(&args.clone().with_int64("add", 3).unwrap())
        .unwrap();
    let b = template
        .bind_parameters(&args.with_int64("add", 4).unwrap())
        .unwrap();
    assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    assert_eq!(template.canonical_template_bytes(), before);
    let result = run(&a, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value.len(), 3);
    assert!(
        result
            .value
            .iter()
            .all(|row| row.get(0).unwrap().as_scalar() == Some(&CanonicalScalar::Int(5)))
    );
    assert!(!format!("{template:?}").contains("score"));
}

#[test]
fn all_sources_and_fallible_projections_execute_even_after_empty_input_or_limit_zero() {
    for text in [
        "MATCH (n) WITH n LIMIT 0 MATCH (n) RETURN n",
        "MATCH (n) WITH n MATCH (n) RETURN n LIMIT 0",
    ] {
        let query = prepare(text);
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |_, _| {
                calls += 1;
                if calls == 1 {
                    Ok(GqlQueryExecution {
                        value: Vec::new(),
                        rows: fgdb_gql::GqlExecutionStats {
                            snapshot_records: 0,
                            result_rows: 0,
                        },
                        evaluator: fgdb_gql::GlaExecutionStats::default(),
                    })
                } else {
                    Err(GqlQueryError::Source("second-source-error"))
                }
            },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "second-source-error"
            )))
        ));
        assert_eq!(calls, 2);
    }
    let late =
        prepare("MATCH (n) WITH n LIMIT 1 MATCH (n)-[:R]->(m) RETURN 1/(m.p-m.p) AS bad LIMIT 0");
    assert!(matches!(
        run(&late, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
}

#[test]
fn cumulative_budgets_and_every_cancellation_checkpoint_cover_the_whole_join() {
    let query = prepare("MATCH (n) WITH n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m");
    let mut calls = 0;
    let measured = run(&query, wide(), &mut || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn illegal_imports_scopes_and_statement_wide_depth_refuse_before_catalog() {
    // A carried vertex's property after a later MATCH re-matches it by
    // identity (fgdb-dhcfo; the rows are pinned below).
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (n) WITH n AS kept MATCH (m) RETURN kept.p AS value",
            symbols
        )
        .is_ok()
    );
    for text in [
        "MATCH (n) WITH n.p AS n MATCH (n)-[:R]->(m) RETURN m",
        "MATCH (n) WITH n AS kept MATCH (m) RETURN n",
        "MATCH (n) WITH n AS kept MATCH (m) OPTIONAL MATCH (kept)-[:R]->(x) RETURN x",
        "MATCH (n) WITH n AS kept MATCH (m) WHERE EXISTS { MATCH (kept)-[:R]->(x) } RETURN m",
        "MATCH (n) WITH n MATCH (n) RETURN n+1 AS bad",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
    let too_deep = format!(
        "MATCH (n){} RETURN n",
        " WITH n MATCH (n)".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH)
    );
    let mut calls = 0;
    assert!(
        PreparedGraphSetText::prepare(&too_deep, |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls, 0);
}

#[test]
fn set_arms_and_literal_routing_keep_their_original_boundaries() {
    assert_eq!(
        execute(
            "(MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) RETURN m LIMIT 1) \
         UNION ALL (MATCH (n) WITH n WHERE n.p=3 MATCH (n) RETURN n AS m) ORDER BY m"
        ),
        vec![vertices(&[2]), vertices(&[4])]
    );
    let literal = "RETURN 'WITH n MATCH (m)' AS text";
    let template = PreparedGraphSetText::prepare(literal, |_, _| None).unwrap();
    assert_eq!(template.columns(), &["text"]);
    assert_eq!(execute("MATCH (n) WITH n MATCH (n) RETURN *").len(), 4);
}

/// fgdb-ezgeq: after WITH, `MATCH (q) WHERE q.p = m` is the equi-join that
/// openCypher's `MATCH (q {p: m})` spells, not a parameter slot. The two
/// definitions are byte-identical, so they share one semantics: NULL never
/// matches and kinds never coerce.
#[test]
fn a_top_level_where_equality_with_an_imported_column_is_the_property_map_join() {
    let template = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes()
    };
    for (map, spelled) in [
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q {p: m}) RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m RETURN q",
        ),
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q {p: m}) RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE m = q.p RETURN q",
        ),
        (
            "MATCH (n) WITH n.p AS v MATCH (q {p: v})-[:R]->(r) WHERE r.p > 1 RETURN v, r",
            "MATCH (n) WITH n.p AS v MATCH (q)-[:R]->(r) WHERE q.p = v AND r.p > 1 RETURN v, r",
        ),
    ] {
        assert_eq!(template(map), template(spelled), "{spelled}");
    }
    // p = [1, 2, 2, 3]: only vertex 4 holds the maximum.
    assert_eq!(
        execute("MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m RETURN q"),
        vec![vertices(&[4])]
    );
    // Each imported v joins every vertex holding it: v = 2 arrives twice and
    // joins vertices 2 and 3 each time.
    assert_eq!(
        execute("MATCH (n) WITH n.p AS v MATCH (q) WHERE q.p = v RETURN v, q ORDER BY v, q"),
        [(1, 1), (2, 2), (2, 2), (2, 3), (2, 3), (3, 4)]
            .map(|(value, vid)| scalar_vertex(value, vid))
            .to_vec()
    );
    // A comparison that is not equality filters the same imported product:
    // every vertex below the maximum 3.
    assert_eq!(
        execute("MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p < m RETURN q ORDER BY q"),
        [1, 2, 3].map(|id| vertices(&[id])).to_vec()
    );
    // Not a top-level AND conjunct: no join key, but the row filter after
    // the join that `WITH * WHERE` spells. m = 3, held only by vertex 4.
    for (text, ids) in [
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m OR q.p = 1 RETURN q ORDER BY q",
            &[1, 4][..],
        ),
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE NOT q.p = m RETURN q ORDER BY q",
            &[1, 2, 3][..],
        ),
    ] {
        let expected: Vec<_> = ids.iter().map(|&id| vertices(&[id])).collect();
        assert_eq!(execute(text), expected, "{text}");
    }
    // An OPTIONAL clause after MATCH (a) would hide the imported column in
    // a child scope. It starts a new part at an implicit `WITH *`, which
    // openCypher's clause-by-clause join already draws: the template of the
    // explicit spelling, and m = 3 held only by vertex 4, reached from 3.
    let text = "MATCH (n) WITH max(n.p) AS m MATCH (a) OPTIONAL MATCH (a)-[:R]->(q) \
                WHERE q.p = m RETURN a, q ORDER BY a";
    let explicit = "MATCH (n) WITH max(n.p) AS m MATCH (a) WITH * OPTIONAL MATCH (a)-[:R]->(q) \
                    WHERE q.p = m RETURN a, q ORDER BY a";
    assert_eq!(
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes(),
        PreparedGraphSetText::prepare(explicit, symbols)
            .unwrap()
            .canonical_template_bytes()
    );
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let row = |a: u128, q: GraphValue| {
        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(a)), q])
    };
    assert_eq!(
        execute(text),
        vec![
            row(1, null.clone()),
            row(2, null.clone()),
            row(3, GraphValue::Vertex(VId(4))),
            row(4, null),
        ]
    );
}

/// A required continuation's WHERE that is no graph predicate, such as a
/// computed graph operand against an imported column (TCK Comparison1
/// [1]-[3]: `WITH ... AS expected MATCH (n) WHERE toInteger(n.id) =
/// expected`), is that part's `WITH * WHERE`: the same template, so the same
/// rows. An OPTIONAL continuation's WHERE belongs to its left join, and a
/// computed operand there still refuses before any catalog call.
#[test]
fn a_continuation_where_over_a_computed_graph_operand_filters_after_the_join() {
    let template = |text: &str| {
        PreparedGraphSetText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes()
    };
    for (implicit, explicit) in [
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p + 0 = m RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WITH * WHERE q.p + 0 = m RETURN q",
        ),
        (
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE toInteger(q.p) = m RETURN q",
            "MATCH (n) WITH max(n.p) AS m MATCH (q) WITH * WHERE toInteger(q.p) = m RETURN q",
        ),
    ] {
        assert_eq!(template(implicit), template(explicit), "{implicit}");
        // m = 3, held only by vertex 4.
        assert_eq!(execute(implicit), vec![vertices(&[4])], "{implicit}");
    }
    // The same after a leading UNWIND, the import idiom's shape (`UNWIND
    // $rows AS row MATCH (n) WHERE toLower(n.name) = row.name`).
    assert_eq!(
        template("UNWIND [3, 1] AS m MATCH (q) WHERE q.p + 0 = m RETURN m, q"),
        template("UNWIND [3, 1] AS m MATCH (q) WITH * WHERE q.p + 0 = m RETURN m, q")
    );
    assert_eq!(
        execute("UNWIND [3, 1] AS m MATCH (q) WHERE q.p + 0 = m RETURN m, q ORDER BY m, q"),
        [(1, 1), (3, 4)]
            .map(|(value, vid)| scalar_vertex(value, vid))
            .to_vec()
    );
    let text = "MATCH (n) WITH max(n.p) AS m MATCH (a) OPTIONAL MATCH (a)-[:R]->(q) \
                WHERE q.p + 0 = m RETURN a, q";
    let mut calls = 0;
    assert!(
        PreparedGraphSetText::prepare(text, |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls, 0);
}

/// `MATCH (a) WITH a MATCH (b) WHERE a = b` correlates `b` with the carried
/// element: the identity form of the same join key, answering what reusing
/// the name `MATCH (a)` answers (TCK Comparison1 [4]/[5], MatchWhere6 [5]).
#[test]
fn a_top_level_where_identity_with_an_imported_element_is_the_reused_name_join() {
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE a = b RETURN a, b ORDER BY a"),
        [1, 2, 3, 4].map(|id| vertices(&[id, id])).to_vec()
    );
    // Parallel edges keep their multiplicity, in either operand order.
    let out = [(1, 2), (1, 2), (1, 3), (2, 3), (3, 4)].map(|(a, m)| vertices(&[a, m]));
    for text in [
        "MATCH (a) WITH a MATCH (b)-[:R]->(m) WHERE b = a RETURN a, m ORDER BY a, m",
        "MATCH (a) WITH a MATCH (b)-[:R]->(m) WHERE a = b AND m.p > 0 RETURN a, m ORDER BY a, m",
        "MATCH (a) WITH a MATCH (a)-[:R]->(m) RETURN a, m ORDER BY a, m",
    ] {
        assert_eq!(execute(text), out.to_vec(), "{text}");
    }
    // OPTIONAL: the equality decides the match before null extension, so
    // vertex 4, which has no outgoing edge, keeps one row with a NULL m.
    let mut optional = out.to_vec();
    optional.push(GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(4)),
        GraphValue::Scalar(CanonicalScalar::Null),
    ]));
    assert_eq!(
        execute(
            "MATCH (a) WITH a OPTIONAL MATCH (b)-[:R]->(m) WHERE b = a RETURN a, m ORDER BY a, m"
        ),
        optional
    );
    // Relationship identity needs identified edges: multipart_optional_reads
    // a_top_level_where_identity_correlates_a_carried_relationship.
    // Inequality filters the carried product: each of the four vertices is
    // every other carried vertex's b, three times.
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE a <> b RETURN b ORDER BY b"),
        [1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
            .map(|id| vertices(&[id]))
            .to_vec()
    );
    // Not a top-level AND conjunct: no join key, but the row filter after the
    // join that `WITH * WHERE` spells, as for the property form (fgdb-c6hvm).
    // Only vertex 1 has p = 1, so every carried a keeps it: a = 1 keeps 1;
    // a = 2, 3 and 4 keep themselves and 1.
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE a = b OR b.p = 1 RETURN b ORDER BY b"),
        [1, 1, 1, 1, 2, 3, 4].map(|id| vertices(&[id])).to_vec()
    );
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) WHERE NOT a = b RETURN b ORDER BY b"),
        [1, 1, 1, 2, 2, 2, 3, 3, 3, 4, 4, 4]
            .map(|id| vertices(&[id]))
            .to_vec()
    );
}

/// fgdb-dhcfo: a carried vertex's property read after a later MATCH is that
/// vertex's own. The part re-matches it by identity, as the name-reuse
/// spelling `WITH a MATCH (a), (b)` does, so both spellings answer alike.
#[test]
fn a_carried_vertex_property_read_after_a_later_match_re_matches_it_by_identity() {
    let ints = |x: i64, y: i64| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(x)),
            GraphValue::Scalar(CanonicalScalar::Int(y)),
        ])
    };
    // p = [1, 2, 2, 3] on vertices 1..4: every pair with a.p < b.p.
    let theta = execute(
        "MATCH (a) WITH a MATCH (b) WHERE a.p < b.p RETURN a.p AS x, b.p AS y ORDER BY x, y",
    );
    assert_eq!(
        theta,
        [(1, 2), (1, 2), (1, 3), (2, 3), (2, 3)]
            .map(|(x, y)| ints(x, y))
            .to_vec()
    );
    assert_eq!(
        theta,
        execute(
            "MATCH (a) WITH a MATCH (a), (b) WHERE a.p < b.p RETURN a.p AS x, b.p AS y ORDER BY x, y"
        )
    );
    // A RETURN-only read keeps the carried product: 4 x 4 rows.
    assert_eq!(
        execute("MATCH (a) WITH a MATCH (b) RETURN a.p AS x, b.p AS y ORDER BY x, y").len(),
        16
    );
    // OPTIONAL reads the carried identity on the LEFT, including its value
    // on an unmatched row. The independent bag oracle below covers it.
    // A binding the WITH dropped is still unknown after the next MATCH.
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (a), (c) WITH a MATCH (b) WHERE c.p < b.p RETURN b",
            symbols
        )
        .is_err()
    );
}

fn nullable_pair(left: Option<i64>, right: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        [left, right]
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .to_vec(),
    )
}

#[test]
fn carried_properties_are_optional_on_operands_with_exact_bag_and_null_semantics() {
    let scores = [1_i64, 2, 2, 3];
    for (operator, reverse) in [
        ("=", "="),
        ("<>", "<>"),
        ("!=", "!="),
        ("<", ">"),
        (">", "<"),
        ("<=", ">="),
        (">=", "<="),
    ] {
        let mut expected = Vec::new();
        for a in scores {
            let mut matched = false;
            for b in scores {
                let keep = match operator {
                    "=" => a == b,
                    "<>" | "!=" => a != b,
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    ">=" => a >= b,
                    _ => unreachable!(),
                };
                if keep {
                    expected.push((a, Some(b)));
                    matched = true;
                }
            }
            if !matched {
                expected.push((a, None));
            }
        }
        expected.sort_by_key(|&(a, b)| (a, b.unwrap_or(i64::MAX)));
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(a, b)| nullable_pair(Some(a), b))
            .collect();
        for predicate in [format!("a.p {operator} b.p"), format!("b.p {reverse} a.p")] {
            let text = format!(
                "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE {predicate} \
                 RETURN a.p AS x,b.p AS y ORDER BY x,y"
            );
            assert_eq!(execute(&text), expected, "{text}");
        }
    }
    assert_eq!(
        execute(
            "MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN a.p AS x,b.p AS y ORDER BY x,y"
        ),
        [(1, 1), (2, 2), (2, 2), (2, 2), (2, 2), (3, 3)]
            .map(|(a, b)| nullable_pair(Some(a), Some(b)))
            .to_vec()
    );
    // Private computed map keys use the same join path; NULL/absence never
    // satisfies equality, and duplicate left occurrences are not deduplicated.
    assert_eq!(
        execute(
            "UNWIND [{p:2},{p:2},{p:NULL},{}] AS row WITH row \
                 OPTIONAL MATCH (b {p:row.p}) RETURN row.p AS x,b.p AS y ORDER BY x,y"
        ),
        [(Some(2), Some(2)); 4]
            .into_iter()
            .chain([(None, None); 2])
            .map(|(a, b)| nullable_pair(a, b))
            .collect::<Vec<_>>()
    );
}

#[test]
fn null_carried_bindings_survive_later_required_and_optional_parts() {
    for kind in ["MATCH", "OPTIONAL MATCH"] {
        let text = format!(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
             {kind} (b) RETURN a.p AS x,b.p AS y ORDER BY y"
        );
        assert_eq!(
            execute(&text),
            [1, 2, 2, 3].map(|b| nullable_pair(None, Some(b))).to_vec(),
            "{text}"
        );
    }
    assert_eq!(
        execute(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                 OPTIONAL MATCH (b) WHERE a.p < b.p RETURN a.p AS x,b.p AS y"
        ),
        vec![nullable_pair(None, None)]
    );
    // Explicitly matching the carried NULL remains a failed required match.
    assert!(
        execute(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                     MATCH (a) RETURN a.p AS x"
        )
        .is_empty()
    );
}

#[test]
fn carried_property_grouping_keeps_nulls_and_completed_optional_occurrences() {
    assert_eq!(
        execute(
            "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE a.p < b.p \
                 WITH a.p AS score,COUNT(b) AS matched RETURN score,matched ORDER BY score"
        ),
        [(1, 3), (2, 2), (3, 0)]
            .map(|(score, count)| nullable_pair(Some(score), Some(count)))
            .to_vec()
    );
    assert_eq!(
        execute(
            "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                 MATCH (b) WITH a.p AS score,COUNT(*) AS rows RETURN score,rows"
        ),
        vec![nullable_pair(None, Some(4))]
    );
}

#[test]
fn carried_property_ordering_shadowing_and_private_columns_keep_the_visible_scope() {
    let query = prepare("MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN *");
    assert_eq!(query.columns(), &["a", "b"]);
    assert!(
        run(&query, wide(), &mut || Ok(()))
            .unwrap()
            .value
            .iter()
            .all(|row| row.len() == 2)
    );
    let values = |text: &str| {
        execute(text)
            .into_iter()
            .map(|row| row.get(0).unwrap().as_scalar().cloned().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        values(
            "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.p>a.p \
                RETURN a.p AS x ORDER BY a.p DESC"
        ),
        [3, 2, 2, 1, 1, 1].map(CanonicalScalar::Int).to_vec()
    );
    for page in ["ORDER BY x", "ORDER BY a.p"] {
        assert_eq!(
            values(&format!(
                "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.p>a.p \
                             RETURN DISTINCT a.p AS x {page}"
            )),
            [1, 2, 3].map(CanonicalScalar::Int).to_vec()
        );
    }
    assert_eq!(
        values(
            "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE b.p>a.p \
                RETURN b.p AS a ORDER BY a"
        ),
        [2, 2, 3, 3, 3]
            .map(CanonicalScalar::Int)
            .into_iter()
            .chain([CanonicalScalar::Null])
            .collect::<Vec<_>>()
    );
    for text in [
        "MATCH (a) WITH a.p AS x OPTIONAL MATCH (b) RETURN a.p",
        "MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN __fg_match_key_1",
        "MATCH (a) WITH a OPTIONAL MATCH (b {p:a.p}) RETURN __fg_match_input_0",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0);
    }
}

#[test]
fn carried_property_join_preserves_every_checkpoint_and_element_source_refusal() {
    let text = "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE a.p>b.p \
                RETURN a.p AS x,b.p AS y ORDER BY x,y";
    let query = prepare(text);
    let mut calls = 0;
    let measured = run(&query, wide(), &mut || {
        calls += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limits = caps;
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=calls {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(seen, stop);
    }
    for suffix in ["", " LIMIT 0"] {
        let failure = run_reading(
            &prepare(&format!("{text}{suffix}")),
            wide(),
            &mut || Ok(()),
            true,
        )
        .unwrap_err();
        assert!(
            format!("{failure:?}").contains("element-read-error"),
            "{failure:?}"
        );
    }
}

/// A grouping WITH right after MATCH keeps its grouped keys' reads for the
/// scope after it, even when its aggregate reads a property or a path
/// function (`WITH p, avg(f.age) AS a RETURN p.name, a`; TCK Return6 [13]:
/// `WITH a, other, min(length(p)) AS len RETURN a.name ...`). aa92179b let
/// the boundary hoist scan that WITH's own items, and hoisting `b.p` or
/// `length(p)` replaced the reads with_graph_head had made for `a.p`.
#[test]
fn a_grouping_head_keeps_its_key_reads_beside_a_property_aggregate() {
    let ints = |x: i64, y: i64| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(x)),
            GraphValue::Scalar(CanonicalScalar::Int(y)),
        ])
    };
    // a = 1 reaches b = 2, 2, 3 (min p 2); a = 2 reaches 3 (p 2); a = 3
    // reaches 4 (p 3). Their own p is 1, 2, 2.
    assert_eq!(
        execute(
            "MATCH (a)-[:R]->(b) WITH a, min(b.p) AS m \
             RETURN a.p AS ap, m ORDER BY ap, m"
        ),
        vec![ints(1, 2), ints(2, 2), ints(2, 3)]
    );
}

/// openCypher existence predicates anywhere in a WHERE, under OR or NOT and
/// beside any other predicate (TCK Pattern1 [21], MatchWhere4 [2],
/// WithWhere4 [2]). The graph predicate takes them only as AND conjuncts, so
/// a refused root MATCH WHERE filters at an implicit `WITH *`, and an
/// explicit WITH's WHERE reads them the same way: each predicate is a hidden
/// Boolean column of that boundary over a Collect scope. An existence test
/// never multiplies rows, whatever its witness count (1 -R-> 2 twice).
#[test]
fn existence_predicates_combine_with_any_predicate_in_a_where() {
    let out = |n: u128| EDGES.iter().any(|&(from, _, _)| from == VId(n));
    let into = |n: u128| EDGES.iter().any(|&(_, _, to)| to == VId(n));
    let edge = |a: u128, b: u128| EDGES.iter().any(|&(x, _, y)| (x, y) == (VId(a), VId(b)));
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let ids = |rows: Vec<GraphValueRow>| -> Vec<Vec<u128>> {
        let mut ids: Vec<_> = rows
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .map(|cell| cell.as_vertex().expect("vertex output").0)
                    .collect()
            })
            .collect();
        ids.sort_unstable();
        ids
    };
    let singles = |keep: &dyn Fn(u128) -> bool| -> Vec<Vec<u128>> {
        (1..=4).filter(|&n| keep(n)).map(|n| vec![n]).collect()
    };
    for (text, expected) in [
        (
            "MATCH (n) WHERE (n)-[:R]->() OR (n)<-[:R]-() RETURN n",
            singles(&|n| out(n) || into(n)),
        ),
        (
            "MATCH (n) WHERE n.p = 1 OR NOT (n)-[:R]->() RETURN n",
            singles(&|n| p(n) == 1 || !out(n)),
        ),
        (
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) WHERE m.p = 3 } OR n.p = 2 RETURN n",
            singles(&|n| (1..=4).any(|m| edge(n, m) && p(m) == 3) || p(n) == 2),
        ),
        (
            "MATCH (n) WHERE NOT (exists((n)<-[:R]-()) OR n.p = 3) RETURN n",
            singles(&|n| !(into(n) || p(n) == 3)),
        ),
    ] {
        assert_eq!(ids(execute(text)), expected, "{text}");
    }
    // Two correlated bindings, at a root MATCH and after a plain WITH.
    let mut pairs: Vec<Vec<u128>> = Vec::new();
    for a in 1..=4 {
        for b in 1..=4 {
            if (p(a) == 1 && edge(a, b)) || edge(b, a) {
                pairs.push(vec![a, b]);
            }
        }
    }
    for text in [
        "MATCH (a), (b) WHERE a.p = 1 AND (a)-[:R]->(b) OR (b)-[:R]->(a) RETURN a, b",
        "MATCH (a), (b) WITH a, b WHERE a.p = 1 AND (a)-[:R]->(b) OR (b)-[:R]->(a) \
         RETURN a, b",
    ] {
        assert_eq!(ids(execute(text)), pairs, "{text}");
    }
    // Edge-shaped arithmetic stays arithmetic: `(x) - -1` is x + 1.
    assert_eq!(
        ids(execute(
            "MATCH (n) WITH n, n.p AS x WHERE (x) - -1 > 2 OR (n)<-[:R]-() RETURN n"
        )),
        singles(&|n| p(n) + 1 > 2 || into(n))
    );
    // The hidden columns are never output columns.
    let star = execute("MATCH (n) WHERE (n)-[:R]->() OR n.p = 3 RETURN *");
    assert!(star.iter().all(|row| row.len() == 1), "{star:?}");
    assert_eq!(ids(star), singles(&|n| out(n) || p(n) == 3));
    // A predicate correlates by name: a binding the WITH renames or drops
    // would read the wrong element, so it is refused.
    for text in [
        "MATCH (a)-[:R]->(b) WITH b AS a WHERE (a)-[:R]->() OR a.p = 1 RETURN a",
        "MATCH (a)-[:R]->(b) WITH a WHERE (b)-[:R]->() OR a.p = 1 RETURN a",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// A pattern comprehension among a grouping WITH's items (fgdb-1f8f7, TCK
/// Pattern2 [8], [9]): the WITH's graph head gathers it into a hidden list
/// column, as a RETURN's head does, and the grouping reads that column, as a
/// key or inside an aggregate's argument. The path-capturing form needs edge
/// identities: multipart_optional_reads holds it.
#[test]
fn a_grouping_with_reads_pattern_comprehensions_its_head_gathers() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let row = |cells: Vec<GraphValue>| GraphValueRow::from_owned_values(cells);
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    // Inside an aggregate's argument: every edge once.
    assert_eq!(
        execute("MATCH (n) WITH sum(size([(n)-->() | 1])) AS total RETURN total"),
        vec![row(vec![int(EDGES.len() as i64)])]
    );
    // A projection reading the pattern's own local: each n's target values,
    // grouped (vertex 4 has none: the empty list is its key).
    let mut expected: Vec<GraphValueRow> = (1..=4)
        .map(|n| {
            let targets: Vec<GraphValue> = EDGES
                .iter()
                .filter(|&&(from, _, _)| from == VId(n))
                .map(|&(_, _, to)| int(p(to.0)))
                .collect();
            row(vec![
                GraphValue::List(targets.into_boxed_slice()),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
            ])
        })
        .collect();
    expected.sort();
    let grouped = execute("MATCH (n) WITH [(n)-->(m) | m.p] AS ps, count(*) AS c RETURN ps, c");
    let grouped: Vec<GraphValueRow> = grouped
        .into_iter()
        .map(|row| {
            let cells: Vec<GraphValue> = row
                .values()
                .iter()
                .map(|cell| match cell.as_scalar() {
                    // count(*) delivers a count cell; compare as an integer.
                    Some(CanonicalScalar::Int(n)) => int(*n),
                    _ => cell.clone(),
                })
                .collect();
            GraphValueRow::from_owned_values(cells)
        })
        .collect();
    assert_eq!(sorted(grouped), expected);
}

/// A pattern comprehension anywhere in a WITH's row scope, its WHERE, pages
/// and RETURN, in the first UNWIND after a MATCH, or in a root MATCH's WHERE
/// at an implicit `WITH *` (fgdb-1f8f7): the head gathers it into a hidden
/// list column, once per row, which the stage reads where it stands. One
/// that names an enclosing list comprehension, quantifier or reduce local
/// stays where it stands, refused as before, in a grouping WITH too; one that
/// reads a binding its row does not carry under its own name refuses.
#[test]
fn a_row_scope_reads_pattern_comprehensions_its_head_gathers() {
    let out = |n: u128| EDGES.iter().filter(|&&(from, _, _)| from == VId(n)).count() as i64;
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let by_degree = |keep: &dyn Fn(i64) -> bool| -> Vec<GraphValueRow> {
        (1..=4)
            .filter(|&n| keep(out(n)))
            .map(|n| vertices(&[n]))
            .collect()
    };
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n WHERE size([(n)-->() | 1]) > 1 RETURN n"
        )),
        by_degree(&|degree| degree > 1)
    );
    assert_eq!(
        sorted(execute("MATCH (n) WHERE size([(n)-->() | 1]) = 1 RETURN n")),
        by_degree(&|degree| degree == 1)
    );
    // A page: degrees 3, 1, 1, 0, ties broken by identity.
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n ORDER BY size([(n)-->() | 1]) DESC, n LIMIT 2 RETURN n"
        )),
        vec![vertices(&[1]), vertices(&[2])]
    );
    // A RETURN after the WITH, the comprehension reading its own local, and
    // one inside a list comprehension that reads no local of it.
    let expected: Vec<GraphValueRow> = (1..=4)
        .map(|n| {
            let targets: Vec<GraphValue> = EDGES
                .iter()
                .filter(|&&(from, _, _)| from == VId(n))
                .map(|&(_, _, to)| int(p(to.0)))
                .collect();
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(n)),
                GraphValue::List(targets.into_boxed_slice()),
                GraphValue::List(Box::new([int(1 + out(n)), int(2 + out(n))])),
            ])
        })
        .collect();
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n RETURN n, [(n)-->(m) | m.p] AS ps, \
             [x IN [1, 2] | x + size([(n)-->() | 1])] AS shifted"
        )),
        expected
    );
    // A plain WITH's own WHERE reads a MATCH binding it drops, as its
    // property reads do: the sources of edges into a vertex with no out-edge.
    let mut sinks: Vec<GraphValueRow> = EDGES
        .iter()
        .filter(|&&(_, _, to)| out(to.0) == 0)
        .map(|&(from, _, _)| vertices(&[from.0]))
        .collect();
    sinks.sort();
    assert_eq!(
        sorted(execute(
            "MATCH (n)-->(m) WITH n WHERE size([(m)-->() | 1]) = 0 RETURN n"
        )),
        sinks
    );
    // The first UNWIND after a MATCH: one row per out-edge.
    let mut pairs: Vec<GraphValueRow> = EDGES
        .iter()
        .map(|&(from, _, to)| vertices(&[from.0, to.0]))
        .collect();
    pairs.sort();
    assert_eq!(
        sorted(execute("MATCH (n) UNWIND [(n)-->(m) | m] AS t RETURN n, t")),
        pairs
    );
    for text in [
        // An enclosing element local: gathered once per row, the
        // comprehension would read a fresh node for it.
        "MATCH (n) WITH n RETURN [x IN [n] | size([(x)-->() | 1])] AS l",
        "MATCH (n) WITH [x IN [n] | size([(x)-->() | 1])] AS l, count(*) AS c RETURN l, c",
        "MATCH (n) WITH reduce(a = 0, x IN [n] | a + size([(x)-->() | 1])) AS l, \
         count(*) AS c RETURN l, c",
        "MATCH (n) WITH any(x IN [n] WHERE size([(x)-->() | 1]) > 0) AS l, \
         count(*) AS c RETURN l, c",
        // A binding the row renames or drops.
        "MATCH (n)-->(k) WITH n AS k RETURN [(k)-->() | 1] AS l",
        "MATCH (n)-->(m) WITH n RETURN [(m)-->() | 1] AS l",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// `COUNT { ... }`, a count subquery in either form EXISTS takes, is the
/// size of a Collect scope's witness list, one entry per witness: read in
/// place by a RETURN's head, and gathered by a WITH head for its WHERE, pages
/// and RETURN, by a grouping WITH for its items, and by the implicit `WITH *`
/// of a MATCH WHERE. The parallel 1 -R-> 2 edges are two witnesses.
/// `size((n)-->())` stays refused, as the openCypher TCK requires (List6
/// [6]).
#[test]
fn pattern_counts_read_witness_list_sizes() {
    let out = |n: u128| EDGES.iter().filter(|&&(from, _, _)| from == VId(n)).count() as i64;
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let into_high = |n: u128| {
        EDGES
            .iter()
            .filter(|&&(from, _, to)| from == VId(n) && p(to.0) > 2)
            .count() as i64
    };
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    let per_vertex = |value: &dyn Fn(u128) -> i64| -> Vec<GraphValueRow> {
        (1..=4)
            .map(|n| {
                GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(n)), int(value(n))])
            })
            .collect()
    };
    for (text, expected) in [
        (
            "MATCH (n) RETURN n, COUNT { (n)-->() } AS d",
            per_vertex(&out),
        ),
        (
            "MATCH (n) RETURN n, COUNT { MATCH (n)-->(m) WHERE m.p > 2 } AS d",
            per_vertex(&into_high),
        ),
        (
            "MATCH (n) RETURN n, COUNT { (n)-->() } + 1 AS d",
            per_vertex(&|n| out(n) + 1),
        ),
        (
            "MATCH (n) WITH n RETURN n, COUNT { (n)-->() } * 2 AS d",
            per_vertex(&|n| out(n) * 2),
        ),
    ] {
        assert_eq!(sorted(execute(text)), expected, "{text}");
    }
    let with_degree = |keep: &dyn Fn(i64) -> bool| -> Vec<GraphValueRow> {
        (1..=4)
            .filter(|&n| keep(out(n)))
            .map(|n| vertices(&[n]))
            .collect()
    };
    assert_eq!(
        sorted(execute("MATCH (n) WHERE COUNT { (n)-->() } > 1 RETURN n")),
        with_degree(&|degree| degree > 1)
    );
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n WHERE COUNT { (n)-->() } = 1 RETURN n"
        )),
        with_degree(&|degree| degree == 1)
    );
    assert_eq!(
        execute("MATCH (n) WITH n ORDER BY COUNT { (n)-->() } DESC, n LIMIT 1 RETURN n"),
        vec![vertices(&[1])]
    );
    // A grouping key: degrees 3, 1, 1 and 0.
    let mut groups: Vec<(i64, i64)> = Vec::new();
    for n in 1..=4 {
        match groups.iter_mut().find(|(degree, _)| *degree == out(n)) {
            Some((_, members)) => *members += 1,
            None => groups.push((out(n), 1)),
        }
    }
    let grouped: Vec<GraphValueRow> = groups
        .into_iter()
        .map(|(degree, members)| GraphValueRow::from_owned_values(vec![int(degree), int(members)]))
        .collect();
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH COUNT { (n)-->() } AS d, collect(n) AS ns RETURN d, size(ns) AS k"
        )),
        sorted(grouped)
    );
    for text in [
        // An enclosing element local, and a binding the row renames.
        "MATCH (n) WITH n RETURN [x IN [n] | COUNT { (x)-->() }] AS l",
        "MATCH (n)-->(k) WITH n AS k RETURN COUNT { (k)-->() } AS l",
        // A pattern expression inside size() (openCypher TCK List6 [6]).
        "MATCH (n) RETURN n, size((n)-->()) AS d",
        "MATCH (n) WHERE size((n)-->()) > 1 RETURN n",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// `x IN <computed list>` in a WHERE: a list comprehension (`[x IN list |
/// e]`, TCK List12 [6]) or a pattern comprehension after IN is one list
/// value, never a literal of members, at a root MATCH's WHERE (its implicit
/// `WITH *`) and after a WITH.
#[test]
fn a_where_membership_reads_a_computed_list() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let sorted = |mut rows: Vec<GraphValueRow>| {
        rows.sort();
        rows
    };
    // n.p is m.p or twice m.p, edge by edge.
    let pairs = sorted(
        EDGES
            .iter()
            .filter(|&&(n, _, m)| [p(m.0), 2 * p(m.0)].contains(&p(n.0)))
            .map(|&(n, _, m)| vertices(&[n.0, m.0]))
            .collect(),
    );
    assert!(!pairs.is_empty());
    for text in [
        "MATCH (n)-->(m) WHERE n.p IN [x IN [1, 2] | x * m.p] RETURN n, m",
        "MATCH (n)-->(m) WITH n, m WHERE n.p IN [x IN [1, 2] | x * m.p] RETURN n, m",
    ] {
        assert_eq!(sorted(execute(text)), pairs, "{text}");
    }
    // A pattern comprehension: the vertices with an out-neighbour of p 2.
    let expected = sorted(
        (1..=4)
            .filter(|&n| {
                EDGES
                    .iter()
                    .any(|&(from, _, to)| from == VId(n) && p(to.0) == 2)
            })
            .map(|n| vertices(&[n]))
            .collect(),
    );
    assert_eq!(
        sorted(execute(
            "MATCH (n) WITH n WHERE 2 IN [(n)-->(m) | m.p] RETURN n"
        )),
        expected
    );
    // A literal list keeps its member-by-member reading.
    assert_eq!(
        sorted(execute("MATCH (n) WITH n WHERE n.p IN [1, 3] RETURN n")),
        vec![vertices(&[1]), vertices(&[4])]
    );
}

/// A plain RETURN's ORDER BY over a count subquery or a pattern
/// comprehension (`ORDER BY COUNT { (n)-->() } DESC`, top-N by degree): the
/// RETURN's head gathers each into a private output, which the page sorts by
/// and then projects away. Degrees are 3, 1, 1 and 0; ties fall to the next
/// key. Under DISTINCT, and for a name an output alias shadows, it refuses.
#[test]
fn a_return_page_orders_by_pattern_counts() {
    let out = |n: u128| EDGES.iter().filter(|&&(from, _, _)| from == VId(n)).count();
    let mut by_degree: Vec<u128> = (1..=4).collect();
    by_degree.sort_by_key(|&n| (core::cmp::Reverse(out(n)), n));
    assert_eq!(
        execute("MATCH (n) RETURN n ORDER BY COUNT { (n)-->() } DESC, n"),
        by_degree
            .iter()
            .map(|&n| vertices(&[n]))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        execute("MATCH (n) RETURN n ORDER BY size([(n)-->(m) | m]) DESC, n LIMIT 2"),
        by_degree[..2]
            .iter()
            .map(|&n| vertices(&[n]))
            .collect::<Vec<_>>()
    );
    for text in [
        "MATCH (n) RETURN DISTINCT n.p AS p ORDER BY COUNT { (n)-->() }",
        "MATCH (n), (k) RETURN k AS n ORDER BY COUNT { (n)-->() }",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// openCypher names a parameter by a decimal integer as well as by an
/// identifier (`$1`, TCK MatchWhere2 [2], WithWhere2 [2]): the statement
/// lexer reads `$1` as the parameter `1`, and the argument `1` binds it.
#[test]
fn a_parameter_may_be_named_by_a_decimal_integer() {
    let p = |n: u128| [1, 2, 2, 3][n as usize - 1];
    let query = PreparedGraphSetText::prepare(
        "MATCH (n) WHERE n.p = $1 OR n.p = $2 RETURN n ORDER BY n",
        symbols,
    )
    .unwrap()
    .bind_parameters(
        &GqlParameters::new()
            .with_int64("1", 1)
            .unwrap()
            .with_int64("2", 3)
            .unwrap(),
    )
    .unwrap();
    let expected: Vec<GraphValueRow> = (1..=4)
        .filter(|&n| [1, 3].contains(&p(n)))
        .map(|n| vertices(&[n]))
        .collect();
    assert_eq!(run(&query, wide(), &mut || Ok(())).unwrap().value, expected);
    // A name still never carries its `$`.
    assert!(GqlParameters::new().with_int64("$1", 1).is_err());
}
