//! Hidden connectors must retain joint endpoints, weights and fallible-source boundaries.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
use std::collections::BTreeMap;

type Edge = (VId, RelationId, VId);
type Atom = (usize, u64, u8, usize);
type Summary = (VId, VId, u64, u64, u64);
const R: RelationId = RelationId(0);
const S: RelationId = RelationId(u64::MAX);
const T: RelationId = RelationId(7);
fn prepare(statement: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(statement, |kind, name| match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(T)),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 2_000_000, 1_000_000)
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter()
        .map(|row| {
            (
                row.keys()[0].as_vertex().unwrap(),
                row.keys()[1].as_vertex().unwrap(),
                row.get(0).unwrap().as_count().unwrap(),
                row.get(1).unwrap().as_count().unwrap(),
                row.get(2).unwrap().as_count().unwrap(),
            )
        })
        .collect()
}
fn oracle(edges: &[Edge], atoms: &[Atom], variables: usize, unequal: bool) -> Vec<Summary> {
    let mut groups: BTreeMap<(VId, VId), u64> = BTreeMap::new();
    for bits in 0..(1 << variables) {
        let values: Vec<_> = (0..variables)
            .map(|at| {
                if bits & (1 << at) == 0 {
                    VId(0)
                } else {
                    VId(u128::MAX)
                }
            })
            .collect();
        if unequal && values[2] == values[0] {
            continue;
        }
        let mut occurrences = 1_u64;
        for &(left, relation, direction, right) in atoms {
            let n = edges
                .iter()
                .filter(|&&(s, r, d)| {
                    r == RelationId(relation)
                        && match direction {
                            0 => s == values[left] && d == values[right],
                            1 => d == values[left] && s == values[right],
                            _ => {
                                (s == values[left] && d == values[right])
                                    || (d == values[left] && s == values[right])
                            }
                        }
                })
                .count() as u64;
            occurrences *= n;
        }
        if occurrences != 0 {
            *groups.entry((values[0], values[4])).or_default() += occurrences;
        }
    }
    groups
        .into_iter()
        .map(|((a, c), n)| (a, c, n, n, 1))
        .collect()
}

#[test]
fn chains_cycles_directions_and_forest_attachments_match_complete_assignment_enumeration() {
    type Case<'a> = (&'a str, &'a [Atom], usize, bool);
    // The oracle multiplies per-atom edge counts: it counts homomorphisms,
    // which is what the factorized chain engine computes. Repeated -[:S]->
    // atoms may share an edge, so each law asks for REPEATABLE ELEMENTS; the
    // DIFFERENT EDGES default would need edge identities (fgdb-vexfr).
    let cases: [Case<'_>; 6] = [
        (
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:S]->(h)-[:S]->(k)-[:S]->(c)",
            &[
                (0, 0, 0, 1),
                (1, u64::MAX, 0, 2),
                (2, u64::MAX, 0, 3),
                (3, u64::MAX, 0, 4),
            ],
            5,
            false,
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a)<-[:R]-(b)<-[:S]-(h)<-[:S]-(k)<-[:S]-(c)",
            &[
                (0, 0, 1, 1),
                (1, u64::MAX, 1, 2),
                (2, u64::MAX, 1, 3),
                (3, u64::MAX, 1, 4),
            ],
            5,
            false,
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a)-[:R]-(b)-[:S]-(h)-[:S]-(k)-[:S]-(c)",
            &[
                (0, 0, 2, 1),
                (1, u64::MAX, 2, 2),
                (2, u64::MAX, 2, 3),
                (3, u64::MAX, 2, 4),
            ],
            5,
            false,
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:S]->(h)-[:S]->(k)-[:S]->(c), (c)-[:T]->(a)",
            &[
                (0, 0, 0, 1),
                (1, u64::MAX, 0, 2),
                (2, u64::MAX, 0, 3),
                (3, u64::MAX, 0, 4),
                (4, 7, 0, 0),
            ],
            5,
            false,
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:S]->(h), (h)-[:T]->(side), (h)-[:S]->(k)-[:S]->(c)",
            &[
                (0, 0, 0, 1),
                (1, u64::MAX, 0, 2),
                (2, 7, 0, 5),
                (2, u64::MAX, 0, 3),
                (3, u64::MAX, 0, 4),
            ],
            6,
            false,
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:S]->(h)-[:S]->(k)-[:S]->(c) WHERE h<>a",
            &[
                (0, 0, 0, 1),
                (1, u64::MAX, 0, 2),
                (2, u64::MAX, 0, 3),
                (3, u64::MAX, 0, 4),
            ],
            5,
            true,
        ),
    ];
    let queries: Vec<_> = cases
        .iter()
        .map(|(head, _, _, _)| {
            prepare(&format!(
                "{head} RETURN a,c,COUNT(*) AS n,COUNT(c) AS m,COUNT(DISTINCT a) AS d GROUP BY a,c"
            ))
        })
        .collect();
    let universe: Vec<_> = [R, S]
        .into_iter()
        .flat_map(|r| {
            [VId(0), VId(u128::MAX)]
                .into_iter()
                .flat_map(move |a| [VId(0), VId(u128::MAX)].into_iter().map(move |b| (a, r, b)))
        })
        .collect();
    for mask in 0..256_usize {
        let mut edges: Vec<_> = universe
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge)
            .collect();
        if let Some(first) = edges.first().copied() {
            edges.push(first);
        }
        if mask & 1 != 0 {
            edges.push((VId(0), T, VId(u128::MAX)));
        }
        if mask & 2 != 0 {
            edges.push((VId(u128::MAX), T, VId(0)));
        }
        for ((_, atoms, variables, unequal), query) in cases.iter().zip(&queries) {
            let original = query.canonical_bytes();
            let actual = query
                .execute_governed(
                    edges.len() as u64,
                    [],
                    edges.iter().copied(),
                    |_, _| Ok::<_, ()>(true),
                    |_, _| Ok(None),
                    wide(),
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(
                plain(&actual.value),
                oracle(&edges, atoms, *variables, *unequal),
                "mask={mask}, atoms={atoms:?}"
            );
            assert_eq!(query.canonical_bytes(), original);
        }
    }
}

/// A chain of repeated -[:S]-> atoms, counted as homomorphisms (REPEATABLE
/// ELEMENTS), as the factorized engine counts them (fgdb-vexfr).
fn path(length: usize, tail: &str) -> String {
    let mut statement = "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)".to_owned();
    for at in 1..=length {
        statement.push_str(&format!("-[:S]->(x{at})"));
    }
    statement.push_str(tail);
    statement
}

#[test]
fn a_billion_paths_keep_their_returned_endpoint_without_visiting_a_billion_bindings() {
    let mut edges = vec![(VId(0), R, VId(1))];
    for a in 1..=8 {
        for b in 1..=8 {
            edges.push((VId(a), S, VId(b)));
        }
    }
    let query = prepare(&path(
        10,
        " RETURN a,x10,COUNT(*) AS n,COUNT(x10) AS m,COUNT(DISTINCT a) AS d GROUP BY a,x10",
    ));
    let result = query
        .execute_governed(
            65,
            [],
            edges.iter().copied(),
            |_, _| Err::<bool, _>("topology-only path read a vertex"),
            |_, _| Err::<Option<&CanonicalPropertyValue>, _>("topology-only path read a property"),
            GqlQueryPolicy::new(65, 8, 65_536, 16_384),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 8);
    for (at, row) in plain(&result.value).iter().enumerate() {
        assert_eq!(
            *row,
            (VId(0), VId(at as u128 + 1), 8_u64.pow(9), 8_u64.pow(9), 1)
        );
    }
    assert_eq!(
        result
            .value
            .iter()
            .map(|row| row.get(0).unwrap().as_count().unwrap())
            .sum::<u64>(),
        8_u64.pow(10)
    );
    assert_eq!(result.rows.snapshot_records, 65);
    assert!(result.evaluator.work_units < 65_536);
}

#[test]
fn connector_overflow_is_not_a_saturated_count_or_an_error_for_empty_or_support_results() {
    let mut edges = vec![(VId(0), R, VId(1))];
    for a in 1..=2 {
        for b in 1..=2 {
            for _ in 0..2 {
                edges.push((VId(a), S, VId(b)));
            }
        }
    }
    let count = prepare(&path(63, " RETURN COUNT(*) AS n,COUNT(DISTINCT x63) AS d"));
    let result = count.execute_governed(
        9,
        [],
        edges.iter().copied(),
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None),
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(
            GraphAggregateError::ArithmeticOverflow { aggregate: 0 }
        ))
    ));
    let support = prepare(&path(
        63,
        " RETURN COUNT(DISTINCT x63) AS d,MIN(x63) AS lo,MAX(x63) AS hi",
    ));
    let result = support
        .execute_governed(
            9,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(2));
    assert_eq!(
        result.value[0]
            .get(1)
            .unwrap()
            .as_value()
            .unwrap()
            .as_vertex(),
        Some(VId(1))
    );
    assert_eq!(
        result.value[0]
            .get(2)
            .unwrap()
            .as_value()
            .unwrap()
            .as_vertex(),
        Some(VId(2))
    );
    let doomed = prepare(&path(62, "-[:T]->(a) RETURN COUNT(*) AS n"));
    let result = doomed
        .execute_governed(
            9,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(0));
}

#[test]
fn contraction_reuses_one_source_and_one_policy_and_propagates_every_interruption() {
    let query = prepare(&path(
        3,
        " RETURN x3,COUNT(*) AS n GROUP BY x3 ORDER BY n DESC,x3 LIMIT 1",
    ));
    let edges = [
        (VId(0), R, VId(1)),
        (VId(1), S, VId(1)),
        (VId(1), S, VId(2)),
        (VId(2), S, VId(1)),
    ];
    let mut calls = 0;
    let mut reads = 0;
    let measured = query
        .execute_governed(
            4,
            [],
            edges.into_iter().inspect(|_| reads += 1),
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || {
                calls += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    assert_eq!(reads, 4);
    let exact = GqlQueryPolicy::new(
        4,
        1,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    let run = |policy| {
        query.execute_governed(
            4,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            policy,
            || Ok::<_, usize>(()),
        )
    };
    assert_eq!(run(exact).unwrap(), measured);
    for cap in [
        GqlQueryPolicy::new(3, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(4, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(run(cap).is_err());
    }
    for stop in 1..=calls {
        let mut at = 0;
        let result = query.execute_governed(
            4,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
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
fn property_bearing_connectors_keep_the_uncompressed_fallible_read_contract() {
    let query = prepare(
        "MATCH REPEATABLE ELEMENTS (a)-[:R]->(b)-[:S]->(h)-[:S]->(c) RETURN COUNT(c.n) AS n",
    );
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), S, VId(2)),
        (VId(2), S, VId(3)),
    ];
    let scalar = CanonicalPropertyValue::from(CanonicalScalar::Int(7));
    let mut reads = 0;
    let result = query.execute_governed(
        4,
        [],
        edges,
        |_, _| Ok::<_, &str>(true),
        |_, _| {
            reads += 1;
            if reads == 2 {
                Err("second occurrence failure")
            } else {
                Ok(Some(&scalar))
            }
        },
        wide(),
        || Ok::<_, ()>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphAggregateError::Source(
            "second occurrence failure"
        )))
    ));
    assert_eq!(reads, 2);
}

#[test]
fn implicit_equalities_plus_maximum_explicit_identities_keep_a_valid_fallback() {
    use fgdb_gql::GraphAggregate;
    use fgdb_gql::algebra::{
        GlaDirection, GraphColumn, GraphPatternBuilder, MAX_PATTERN_IDENTITIES,
    };
    let mut builder = GraphPatternBuilder::new();
    for name in ["a", "h", "k", "c"] {
        builder.vertex(name).unwrap();
    }
    builder.edge("a", R, GlaDirection::Forward, "a").unwrap();
    builder.edge("a", S, GlaDirection::Forward, "h").unwrap();
    builder.edge("h", S, GlaDirection::Forward, "k").unwrap();
    builder.edge("k", S, GlaDirection::Forward, "c").unwrap();
    for _ in 0..MAX_PATTERN_IDENTITIES {
        builder.identity("a", "a", true).unwrap();
    }
    let input = builder
        .prepare_values(&[GraphColumn::vertex("end", "c")], 0, None)
        .unwrap()
        .with_duplicates();
    let query = PreparedGraphAggregate::prepare(
        input,
        &[],
        &[
            GraphAggregate::count_rows("paths"),
            GraphAggregate::count_distinct("ends", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let edges = [
        (VId(0), R, VId(0)),
        (VId(0), S, VId(1)),
        (VId(1), S, VId(2)),
        (VId(2), S, VId(3)),
    ];
    let result = query
        .execute_governed(
            4,
            [],
            edges,
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(1));
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(1));
}

/// A named relationship that nothing reads is not captured (fgdb-61p1n): the
/// definition is its unnamed spelling's, operator for operator, so the
/// topology-only evaluator answers it with the same rows and the same work.
/// It used to require identified edges and enumerate every binding. A
/// relationship that IS read keeps its capture and still requires them.
#[test]
fn unread_relationship_names_leave_the_unnamed_definition() {
    use fgdb_gql::algebra::GlaOperator;
    let captures = |query: &PreparedGraphAggregate| {
        query
            .input_pattern()
            .plan()
            .operators()
            .iter()
            .filter(|operator| matches!(operator, GlaOperator::CapturePath { .. }))
            .count()
    };
    // Parallel edges, a 2-cycle and a triangle: multiplicities and the
    // DIFFERENT EDGES default both matter to the counts.
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(1)),
        (VId(2), R, VId(3)),
        (VId(3), R, VId(1)),
        (VId(3), S, VId(1)),
    ];
    let run = |query: &PreparedGraphAggregate| {
        query.execute_governed(
            9,
            [VId(1), VId(2), VId(3)],
            edges.iter().copied(),
            |_, _| Ok::<bool, &str>(true),
            |_, _| Ok::<Option<&CanonicalPropertyValue>, &str>(None),
            wide(),
            || Ok::<_, ()>(()),
        )
    };
    for (named, unnamed) in [
        (
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n",
        ),
        (
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b)-[:R]->(c) RETURN COUNT(*) AS n",
        ),
        (
            "MATCH (a)-[r:R]->(b)-[s:R]->(c)-[t:R]->(a) RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) RETURN COUNT(*) AS n",
        ),
        (
            "MATCH (a)-[r:R]->(b)<-[s:S]-(c) RETURN a,COUNT(*) AS n,COUNT(DISTINCT c) AS d GROUP BY a",
            "MATCH (a)-[:R]->(b)<-[:S]-(c) RETURN a,COUNT(*) AS n,COUNT(DISTINCT c) AS d GROUP BY a",
        ),
        // A Boolean WHERE over vertices only reads no relationship.
        (
            "MATCH (a)-[r:R]->(b) WHERE a.n = 1 OR b.n = 2 RETURN COUNT(*) AS n",
            "MATCH (a)-[:R]->(b) WHERE a.n = 1 OR b.n = 2 RETURN COUNT(*) AS n",
        ),
    ] {
        let (named_query, unnamed_query) = (prepare(named), prepare(unnamed));
        assert_eq!(captures(&named_query), 0, "{named}");
        assert_eq!(
            named_query.input_pattern().plan().operators(),
            unnamed_query.input_pattern().plan().operators(),
            "{named}"
        );
        let (answer, expected) = (run(&named_query).unwrap(), run(&unnamed_query).unwrap());
        assert_eq!(answer.value, expected.value, "{named}");
        assert_eq!(answer.evaluator, expected.evaluator, "{named}");
        assert!(!answer.value.is_empty(), "{named}");
    }
    for read in [
        "MATCH (a)-[r:R]->(b) RETURN COUNT(r) AS n",
        "MATCH (a)-[r:R]->(b) RETURN SUM(r.n) AS n",
        "MATCH (a)-[r:R]->(b) WHERE r.n = 1 RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b) WHERE r.n = 1 OR a.n = 2 RETURN COUNT(*) AS n",
        "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COUNT(s) AS n",
    ] {
        let query = prepare(read);
        assert!(captures(&query) > 0, "{read}");
        assert!(
            matches!(run(&query), Err(GqlQueryError::IdentifiedEdgesRequired)),
            "{read}"
        );
    }
    // A collection or a sum can observe the order occurrences arrive in, so
    // an unread name still keeps its identified plan there.
    for ordered in [
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(b) AS n",
        "MATCH (a)-[r:R]->(b) RETURN COLLECT(DISTINCT b) AS n",
        "MATCH (a)-[r:R]->(b) RETURN SUM(a.n) AS n",
        "MATCH (a)-[r:R]->(b) RETURN a,AVG(b.n) AS n GROUP BY a",
        "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS c,SUM(a.n) AS n",
    ] {
        assert!(captures(&prepare(ordered)) > 0, "{ordered}");
    }
}
