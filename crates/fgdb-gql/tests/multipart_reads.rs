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
    // Not a top-level AND conjunct, or inside a scope that hides the
    // imported columns: refused before any catalog call, as before.
    for text in [
        "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE q.p = m OR q.p = 1 RETURN q",
        "MATCH (n) WITH max(n.p) AS m MATCH (q) WHERE NOT q.p = m RETURN q",
        "MATCH (n) WITH max(n.p) AS m MATCH (a) OPTIONAL MATCH (a)-[:R]->(q) WHERE q.p = m RETURN a, q",
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
    // Not a top-level AND conjunct: refused before any catalog call, as for
    // the property form.
    for text in [
        "MATCH (a) WITH a MATCH (b) WHERE a = b OR b.p = 1 RETURN b",
        "MATCH (a) WITH a MATCH (b) WHERE NOT a = b RETURN b",
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
