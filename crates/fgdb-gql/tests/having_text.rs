//! Compound HAVING compares complete aggregate cells before ranking/pages.
//! Expected groups come from owned occurrence enumeration, not another plan.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{IntegerComparison, ScalarPredicate};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateColumn,
    GraphAggregateError, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphHavingExpression, GraphHavingOp as Op, GraphHavingOperand as Arg,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, MAX_AGGREGATE_FILTERS,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, CanonicalScalarKind, VId};
use std::collections::BTreeMap;

type Edge = (VId, RelationId, VId);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 1_000_000)
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn cmp(left: Arg, comparison: IntegerComparison, right: Arg) -> Op {
    Op::Compare {
        left,
        comparison,
        right,
    }
}

#[test]
fn ordinary_having_is_unchanged_and_compound_text_equals_the_typed_expression() {
    use GraphAggregateColumn::{Aggregate as A, GroupKey as K};
    let head = "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n,a,SUM(b.p) AS total GROUP BY a";
    let plain = prepare(head);
    let flat = prepare(&format!(
        "{head} HAVING n>=2 AND total IS NOT NULL ORDER BY total DESC"
    ));
    let expected = plain
        .clone()
        .with_result_clauses(
            &[
                GraphAggregateFilter {
                    column: A(0),
                    test: GraphAggregateTest::Integer {
                        comparison: IntegerComparison::GreaterOrEqual,
                        value: 2,
                    },
                },
                GraphAggregateFilter {
                    column: A(1),
                    test: GraphAggregateTest::IsNotNull,
                },
            ],
            &[GraphAggregateOrder::descending(A(1))],
        )
        .unwrap();
    assert_eq!(flat, expected);
    assert!(flat.having_expression().is_none());
    let text = format!("{head} HAVING (n>total OR total IS NULL) AND NOT (n=0) ORDER BY a");
    let mut calls = BTreeMap::new();
    let template = PreparedGraphAggregateText::prepare(&text, |kind, name| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    })
    .unwrap();
    let expr = GraphHavingExpression::prepare(&[
        cmp(
            Arg::Column(A(0)),
            IntegerComparison::Greater,
            Arg::Column(A(1)),
        ),
        Op::IsNull {
            operand: Arg::Column(A(1)),
            is_null: true,
        },
        Op::Or,
        cmp(Arg::Column(A(0)), IntegerComparison::Equal, Arg::Integer(0)),
        Op::Not,
        Op::And,
    ])
    .unwrap();
    let typed = plain
        .with_result_clauses(&[], &[GraphAggregateOrder::ascending(K(0))])
        .unwrap()
        .with_having_expression(&expr)
        .unwrap();
    let bound = template.bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(bound, typed);
    assert_eq!(template.columns(), &["n", "a", "total"]);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(
        bound.canonical_bytes(),
        prepare(&text.replace("n>total", "COUNT(*)>SUM_INT(b.p)")).canonical_bytes()
    );
    assert!(bound.having().is_empty());
    assert!(bound.having_expression().is_some());
}

#[test]
fn compound_filter_precedence_matches_independent_multigraph_group_and_page_oracle() {
    let universe = [
        (VId(0), RelationId(1), VId(0)),
        (VId(0), RelationId(1), VId(1)),
        (VId(1), RelationId(1), VId(0)),
        (VId(1), RelationId(1), VId(1)),
    ];
    let conditions = [
        "n>present OR total>0 AND present=1",
        "(n>present OR total>0) AND NOT (present=0)",
        "NOT (total<=0) OR total IS NULL",
    ];
    for mask in 0..16 {
        for (direction, arrow) in [(0, "-[:R]->"), (1, "<-[:R]-"), (2, "-[:R]-")] {
            let mut edges: Vec<_> = universe
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, edge)| *edge)
                .collect();
            if let Some(edge) = edges.first().copied() {
                edges.push(edge);
            }
            for left in [None, Some(-3), Some(4)] {
                for right in [None, Some(-3), Some(4)] {
                    let raw = [left, right];
                    let values = raw.map(|value| {
                        value.map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)))
                    });
                    let mut groups: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
                    for &(s, _, d) in &edges {
                        let oriented = if direction == 1 {
                            vec![(d, s)]
                        } else if direction == 2 && s != d {
                            vec![(s, d), (d, s)]
                        } else {
                            vec![(s, d)]
                        };
                        for (a, b) in oriented {
                            groups.entry(a).or_default().push(raw[b.0 as usize]);
                        }
                    }
                    for (which, condition) in conditions.iter().enumerate() {
                        let mut expected = Vec::new();
                        for (&owner, items) in &groups {
                            let n = items.len() as u64;
                            let present = items.iter().flatten().count() as u64;
                            let total = (present != 0).then(|| {
                                items.iter().flatten().map(|n| i128::from(*n)).sum::<i128>()
                            });
                            let positive = total.is_some_and(|value| value > 0);
                            let accepted = match which {
                                0 => n > present || (positive && present == 1),
                                1 => (n > present || positive) && present != 0,
                                _ => positive || total.is_none(),
                            };
                            if accepted {
                                expected.push((owner, n, present, total));
                            }
                        }
                        expected.sort_by_key(|row| (std::cmp::Reverse(row.1), row.0));
                        for (offset, count) in [(0, None), (0, Some(0)), (1, Some(1))] {
                            let text = format!(
                                "MATCH (a){arrow}(b) RETURN a,COUNT(*) AS n,COUNT(b.p) AS present,SUM(b.p) AS total \
                        GROUP BY a HAVING {condition} ORDER BY n DESC,a SKIP {offset}{}",
                                count.map_or(String::new(), |n| format!(" LIMIT {n}"))
                            );
                            let actual = prepare(&text)
                                .execute_governed(
                                    edges.len() as u64,
                                    [],
                                    edges.iter().copied(),
                                    |_, _| Ok::<_, ()>(true),
                                    |vid, _| Ok(values[vid.0 as usize].as_ref()),
                                    wide(),
                                    || Ok::<_, ()>(()),
                                )
                                .unwrap();
                            let actual: Vec<_> = actual
                                .value
                                .iter()
                                .map(|row| {
                                    (
                                        row.keys()[0].as_vertex().unwrap(),
                                        row.get(0).unwrap().as_count().unwrap(),
                                        row.get(1).unwrap().as_count().unwrap(),
                                        row.get(2).unwrap().as_integer(),
                                    )
                                })
                                .collect();
                            assert_eq!(
                                actual,
                                expected
                                    .iter()
                                    .copied()
                                    .skip(offset)
                                    .take(count.unwrap_or(usize::MAX))
                                    .collect::<Vec<_>>(),
                                "{text}, mask={mask}"
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn scalar_and_unsigned_arguments_rebind_without_reparsing_or_catalog_reads() {
    let text = "MATCH (a) RETURN a.p AS category,COUNT(*) AS n GROUP BY a.p \
        HAVING (category=$wanted OR category IS NULL) AND NOT (n>$ceiling) LIMIT $take";
    let mut resolves = 0;
    let template = PreparedGraphAggregateText::prepare_with_parameter_types(
        text,
        &[
            (
                "wanted",
                GqlParameterType::Scalar(CanonicalScalarKind::Text),
            ),
            ("ceiling", GqlParameterType::UInt64),
        ],
        |kind, name| {
            resolves += 1;
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(resolves, 1);
    let args = GqlParameters::new()
        .with_text("wanted", "O'Brien $take")
        .unwrap()
        .with_uint64("ceiling", u64::MAX)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let first = template.bind_parameters(&args).unwrap();
    let frozen = first.canonical_bytes();
    let changed = GqlParameters::new()
        .with_text("wanted", "other")
        .unwrap()
        .with_uint64("ceiling", u64::MAX)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    assert_ne!(
        template
            .bind_parameters(&changed)
            .unwrap()
            .canonical_bytes(),
        frozen
    );
    assert_eq!(first.canonical_bytes(), frozen);
    assert_eq!(resolves, 1);
    let values = [
        CanonicalScalar::ucs_basic_text("O'Brien $take").unwrap(),
        CanonicalScalar::ucs_basic_text("other").unwrap(),
        CanonicalScalar::Null,
    ];
    let stored = values.clone().map(CanonicalPropertyValue::from);
    let run = |query: &PreparedGraphAggregate| {
        query
            .execute_governed(
                3,
                [VId(0), VId(1), VId(2)],
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(Some(&stored[vid.0 as usize])),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value
    };
    assert_eq!(run(&first).len(), 2);
    assert_eq!(run(&first)[1].keys()[0].as_scalar(), Some(&values[0]));
    let null = GqlParameters::new()
        .with_null("wanted")
        .unwrap()
        .with_uint64("ceiling", u64::MAX)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let rows = run(&template.bind_parameters(&null).unwrap());
    assert_eq!(rows.len(), 1);
    assert!(rows[0].keys()[0].is_null());
    assert!(matches!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::MissingParameter
    ));
    let wrong = GqlParameters::new()
        .with_bool("wanted", true)
        .unwrap()
        .with_uint64("ceiling", 1)
        .unwrap()
        .with_uint64("take", 1)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
    ));
    let literal = prepare(
        "MATCH (a) RETURN a.p AS category,COUNT(*) AS n GROUP BY a.p HAVING category='O''Brien $take'",
    );
    assert_eq!(run(&literal).len(), 1);
    assert_eq!(run(&literal)[0].keys()[0].as_scalar(), Some(&values[0]));
    let scalar = ScalarPredicate::new(values[0].clone(), IntegerComparison::Equal).unwrap();
    assert!(!format!("{scalar:?} {template:?}").contains("O'Brien"));
}

#[test]
fn computed_operands_share_aliases_aggregates_and_frozen_parameter_binding() {
    let text = "MATCH (a) RETURN COUNT(*) + 1 AS adjusted,COUNT(*) AS raw \
        HAVING (adjusted - 1) * 2 = raw * 2 \
        AND COUNT(*) + $delta BETWEEN $minimum + 1 AND $maximum - 1 \
        AND abs(adjusted - COUNT(*)) IN [1 + 0, NULL]";
    let mut calls = 0;
    let template = PreparedGraphAggregateText::prepare(text, |kind, name| {
        calls += 1;
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls, 0);
    assert_eq!(template.columns(), &["adjusted", "raw"]);
    assert!(
        template
            .parameter_schema()
            .iter()
            .all(|arg| arg.occurrences == 1)
    );
    let bind = |delta| {
        template
            .bind_parameters(
                &GqlParameters::new()
                    .with_int64("delta", delta)
                    .unwrap()
                    .with_int64("minimum", 2)
                    .unwrap()
                    .with_int64("maximum", 6)
                    .unwrap(),
            )
            .unwrap()
    };
    for (delta, expected) in [(1, 1), (9, 0)] {
        let query = bind(delta);
        let frozen = query.canonical_bytes();
        let rows = query
            .execute_governed(
                3,
                [VId(1), VId(2), VId(3)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), expected);
        if let Some(row) = rows.first() {
            assert_eq!(row.values()[0].as_integer(), Some(4));
            assert_eq!(row.values()[1].as_count(), Some(3));
        }
        assert_eq!(query.canonical_bytes(), frozen);
    }
    assert_ne!(bind(1).canonical_bytes(), bind(9).canonical_bytes());
    assert_eq!(calls, 0);
}

#[test]
fn computed_having_retains_wide_sums_and_exact_fractional_averages() {
    let values = [i64::MAX, i64::MAX - 1]
        .map(|value| CanonicalPropertyValue::from(CanonicalScalar::Int(value)));
    for text in [
        "MATCH (a) RETURN COUNT(*) AS n HAVING \
         SUM(a.p) + 1 = COUNT(*) * 9223372036854775807 \
         AND COALESCE(AVG(a.p), 0) > 9223372036854775807 - 1 \
         AND COALESCE(AVG(a.p), 0) < 9223372036854775807",
        "MATCH (a) RETURN SUM(a.p) + 1 AS total,COALESCE(AVG(a.p), 0) AS mean HAVING \
         total - 1 = SUM(a.p) AND COALESCE(mean, 0) > 9223372036854775807 - 1 \
         AND COALESCE(mean, 0) < 9223372036854775807",
    ] {
        let rows = prepare(text)
            .execute_governed(
                2,
                [VId(0), VId(1)],
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(Some(&values[vid.0 as usize])),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), 1, "{text}");
    }
}

#[test]
fn computed_having_null_functions_and_parentheses_keep_three_valued_logic() {
    for (condition, expected) in [
        ("SUM(a.p) + 1 IS NULL", 1),
        ("SUM(a.p) + 1 > 0", 0),
        ("NOT (SUM(a.p) + 1 > 0)", 0),
        ("COALESCE(SUM(a.p), COUNT(*) + 1) = 1", 1),
        ("((COUNT(*) + 1) * 2) BETWEEN 1 + 1 AND 2 + 1", 1),
        ("((COUNT(*) + 1) > 0 OR SUM(a.p) + 1 = 0)", 1),
        ("(COUNT(*) + 1) NOT IN [2 + 0, NULL]", 0),
    ] {
        let text = format!("MATCH (a) RETURN COUNT(*) AS n HAVING {condition}");
        let rows = prepare(&text)
            .execute_governed(
                0,
                [],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), expected, "{condition}");
    }
}

#[test]
fn scalar_aliases_in_lazy_having_branches_do_not_evaluate_unselected_errors() {
    for condition in [
        "CASE WHEN TRUE THEN 0 ELSE bad END = 1",
        "COALESCE(0, bad) = 1",
        "CASE COUNT(*) WHEN 0 THEN 0 ELSE bad END = 1",
    ] {
        let text = format!("MATCH (a) RETURN COUNT(*) / 0 AS bad HAVING {condition}");
        let rows = prepare(&text)
            .execute_governed(
                0,
                [],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert!(rows.is_empty(), "{condition}");
    }
    for condition in [
        "CASE WHEN FALSE THEN 0 ELSE bad END = 1",
        "COALESCE(NULL, bad) = 1",
    ] {
        let text = format!("MATCH (a) RETURN COUNT(*) / 0 AS bad HAVING {condition} LIMIT 0");
        assert!(
            matches!(
                prepare(&text).execute_governed(
                    0,
                    [],
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    wide(),
                    || Ok::<_, ()>(()),
                ),
                Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression {
                        error: fgdb_gql::GraphIntegerError {
                            kind: fgdb_gql::GraphIntegerErrorKind::DivisionByZero,
                            ..
                        },
                        ..
                    }
                ))
            ),
            "{condition}"
        );
    }
}

#[test]
fn deferred_composite_aliases_refuse_instead_of_eagerly_running_their_branches() {
    for condition in [
        "CASE WHEN TRUE THEN 0 ELSE bad END = 1",
        "COALESCE(0, bad) = 1",
        "size([x IN [] | bad]) = 0",
    ] {
        let mut calls = 0;
        let text = format!("MATCH (a) RETURN [SUM(a.p) / 0][0] AS bad HAVING {condition}");
        assert!(
            PreparedGraphAggregateText::prepare(&text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{condition}"
        );
        assert_eq!(calls, 0, "{condition}");
    }
}

#[test]
fn computed_alias_operands_preserve_element_identity_and_collection_values() {
    for text in [
        "MATCH (a) RETURN collect(a)[0] AS first HAVING id(first) + 1 = 8",
        "MATCH (a) RETURN collect(a)[0] AS first HAVING elementId(first) = '7'",
        "MATCH (a) RETURN tail(collect(a)) AS rest HAVING size(rest) + 1 = 2",
    ] {
        let rows = prepare(text)
            .execute_governed(
                2,
                [VId(7), VId(8)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len(), 1, "{text}");
    }
}

#[test]
fn global_empty_null_negation_aliases_and_optional_groups_preserve_semantics() {
    for (condition, expected) in [
        ("NOT (total=0)", 0),
        ("total IS NULL OR total>n", 1),
        ("NOT FALSE AND (TRUE OR NULL)", 1),
        ("NULL OR FALSE", 0),
        ("n=0 AND total IS NULL", 1),
    ] {
        let query = prepare(&format!(
            "MATCH (a) RETURN COUNT(*) AS n,SUM(a.p) AS total HAVING {condition}"
        ));
        assert_eq!(
            query
                .execute_governed(
                    0,
                    [],
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    wide(),
                    || Ok::<_, ()>(())
                )
                .unwrap()
                .value
                .len(),
            expected
        );
    }
    let aliases = prepare(
        "MATCH (a) RETURN COUNT(*) AS not,COUNT(a) AS true HAVING not=true AND NOT (not=0)",
    );
    assert_eq!(
        aliases
            .execute_governed(
                1,
                [VId(u128::MAX)],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(())
            )
            .unwrap()
            .value
            .len(),
        1
    );
    let query = prepare(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,COUNT(b) AS present \
        GROUP BY a HAVING n>present OR NOT (present=1) ORDER BY a",
    );
    let edges = [
        (VId(1), RelationId(1), VId(2)),
        (VId(1), RelationId(1), VId(2)),
    ];
    let result = query
        .execute_governed(
            4,
            [VId(0), VId(1)],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(0));
    assert_eq!(result.value[1].get(0).unwrap().as_count(), Some(2));
}

#[test]
fn malformed_hidden_columns_and_structural_limits_refuse_before_catalog_access() {
    let head = "MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,SUM(b.p) AS total GROUP BY a HAVING";
    let mut cases = vec![
        "",
        "()",
        "NOT",
        "n>0 OR OR n<0",
        "(n>0",
        "n>0)",
        "n>missing",
        "MIN(*)>0",
        "a.p='x'",
        "n+>0",
        "n>0;",
        "n IS TRUE",
        "1",
        "n>0 XOR n<3",
        "EXISTS { MATCH (a) }",
        "n>$x LIMIT $x",
        "n>9223372036854775808",
        "n< -9223372036854775809",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    cases.push(format!("{}n>0{}", "(".repeat(65), ")".repeat(65)));
    cases.push(format!("{}n>0", "NOT ".repeat(65)));
    cases.push(vec!["n>0"; MAX_AGGREGATE_FILTERS + 1].join(" OR "));
    for tail in cases {
        let mut calls = 0;
        assert!(
            PreparedGraphAggregateText::prepare(&format!("{head} {tail}"), |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{tail}"
        );
        assert_eq!(calls, 0);
    }
    assert!(
        PreparedGraphAggregateText::prepare(
            &format!("{head} {}n>0{}", "(".repeat(64), ")".repeat(64)),
            symbols
        )
        .is_ok()
    );
    assert!(
        PreparedGraphAggregateText::prepare(
            &format!("{head} {}", vec!["n>0"; 64].join(" OR ")),
            symbols
        )
        .is_ok()
    );
    let source = "MATCH (a) RETURN COUNT(*) AS n HAVING NOT (n=0 OR n>2)";
    for at in (0..=source.len()).filter(|at| source.is_char_boundary(*at)) {
        let _ = PreparedGraphAggregateText::prepare(&source[..at], symbols);
    }
    let mut calls = 0;
    assert!(
        PreparedGraphAggregateText::prepare_with_parameter_types(
            source,
            &[("unused", GqlParameterType::Int64)],
            |kind, name| {
                calls += 1;
                symbols(kind, name)
            }
        )
        .is_err()
    );
    assert_eq!(calls, 0);
}

#[test]
fn having_uses_exact_shared_limits_and_every_interruption_checkpoint() {
    let query = prepare(
        "MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,SUM(b.p) AS total GROUP BY a \
        HAVING NOT (n>=total) OR total IS NULL ORDER BY total DESC NULLS LAST LIMIT 2",
    );
    let edges: Vec<Edge> = (0..4).map(|id| (VId(id), RelationId(1), VId(id))).collect();
    let values = [
        CanonicalScalar::Int(3),
        CanonicalScalar::Int(2),
        CanonicalScalar::Null,
        CanonicalScalar::Int(-1),
    ]
    .map(CanonicalPropertyValue::from);
    let run = |policy| {
        query.execute_governed(
            4,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    let result = run(wide()).unwrap();
    assert_eq!(result.value.len(), 2);
    let exact = GqlQueryPolicy::new(
        4,
        2,
        result.evaluator.work_units,
        result.evaluator.scratch_entries,
    );
    assert_eq!(run(exact).unwrap(), result);
    for policy in [
        GqlQueryPolicy::new(3, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 2, result.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(4, 2, u64::MAX, result.evaluator.scratch_entries - 1),
    ] {
        assert!(run(policy).is_err());
    }
    let mut calls = 0;
    query
        .execute_governed(
            4,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=calls {
        let mut at = 0;
        let result = query.execute_governed(
            4,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result,Err(GqlQueryError::Interrupted(value)) if value==stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn decisive_having_never_masks_input_errors_or_numeric_domain_refusals() {
    for suffix in [
        "HAVING TRUE OR least>0",
        "HAVING FALSE AND least>0",
        "HAVING (least>0)",
    ] {
        let query = prepare(&format!(
            "MATCH (a) RETURN MIN(a.p) AS least {suffix} LIMIT 0"
        ));
        let bad = CanonicalPropertyValue::from(CanonicalScalar::Bool(true));
        let result = query.execute_governed(
            1,
            [VId(1)],
            [],
            |_, _| Ok::<_, &str>(true),
            |_, _| Ok(Some(&bad)),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(
                GraphAggregateError::NonIntegerHaving { .. }
            ))
        ));
        let error = query.execute_governed(
            1,
            [VId(1)],
            [],
            |_, _| Ok(true),
            |_, _| Err::<Option<&CanonicalPropertyValue>, _>("unreadable input"),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            error,
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                "unreadable input"
            )))
        ));
    }
}

#[test]
fn full_width_counts_and_sums_reach_having_without_narrowing_or_child_bags() {
    // 63 self-loop atoms over two parallel edges: 2^63 homomorphisms, which
    // REPEATABLE ELEMENTS counts (DIFFERENT EDGES would admit none; fgdb-vexfr).
    let mut text = "MATCH REPEATABLE ELEMENTS (a)".to_owned();
    for _ in 0..63 {
        text.push_str("-[:R]->(a)");
    }
    text.push_str(" RETURN COUNT(*) AS n,COUNT(a) AS present HAVING n=present AND NOT (n<=9223372036854775807)");
    let query = prepare(&text);
    let result = query
        .execute_governed(
            2,
            [],
            [(VId(7), RelationId(1), VId(7)); 2],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            GqlQueryPolicy::new(2, 1, 65_536, 4096),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        result.value[0].get(0).unwrap().as_count(),
        Some(1_u64 << 63)
    );
    let query = prepare(
        "MATCH (a)-[:R]->(b) RETURN SUM(b.p) AS total,COUNT(*) AS n HAVING total>n AND total>9223372036854775807",
    );
    let value = CanonicalPropertyValue::from(CanonicalScalar::Int(i64::MAX));
    let result = query
        .execute_governed(
            2,
            [],
            [(VId(0), RelationId(1), VId(1)); 2],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&value)),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(
        result.value[0].get(0).unwrap().as_integer(),
        Some(2 * i128::from(i64::MAX))
    );
}
