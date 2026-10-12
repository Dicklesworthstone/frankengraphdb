//! Optional read-part continuations use the ordinary governed GLA and row join.
//! Expected bags below come from explicit multigraph enumeration, not another
//! query or a second optional matcher. The source fixture never changes mid-run.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::{Cell, RefCell};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const EDGES: [(VId, RelationId, VId); 4] = [
    (VId(1), R, VId(2)),
    (VId(1), R, VId(2)),
    (VId(2), R, VId(3)),
    (VId(3), R, VId(3)),
];

type ResultRows =
    Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<usize>, usize>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn run(
    query: &PreparedGraphSet,
    policy: GqlQueryPolicy,
    fail_source: Option<usize>,
    calls: &Cell<usize>,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> ResultRows {
    let checkpoint = RefCell::new(checkpoint);
    let props: Vec<_> = (1..=4)
        .map(|id| {
            vec![(
                P,
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(id * 10)),
            )]
        })
        .collect();
    query.execute_governed(
        policy,
        |pattern, remaining| {
            calls.set(calls.get() + 1);
            if fail_source == Some(calls.get()) {
                return Err(GqlQueryError::Source(77));
            }
            pattern.plan().execute_governed_with_properties(
                8, // four vertices and four edge occurrences per admitted source
                (1..=4).map(VId),
                EDGES,
                |vid, predicates| {
                    Ok::<_, usize>(predicates.iter().all(|predicate| {
                        predicate.matches(&[LabelId(1)], &props[vid.0 as usize - 1])
                    }))
                },
                |vid, key| {
                    Ok(props[vid.0 as usize - 1]
                        .iter()
                        .find(|(candidate, _)| *candidate == key)
                        .map(|(_, value)| value))
                },
                remaining,
                || (checkpoint.borrow_mut())(),
            )
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn bind(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn rows(text: &str) -> Vec<GraphValueRow> {
    run(&bind(text), wide(), None, &Cell::new(0), || Ok(()))
        .unwrap()
        .value
}
/// `rows` over the same fixture with each edge occurrence's identity, for a
/// plan that captures a relationship.
fn identified_rows(text: &str) -> Vec<GraphValueRow> {
    let props: Vec<_> = (1..=4)
        .map(|id| {
            vec![(
                P,
                CanonicalPropertyValue::Scalar(CanonicalScalar::Int(id * 10)),
            )]
        })
        .collect();
    bind(text)
        .execute_governed(
            wide(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_identified_properties(
                    8,
                    (1..=4).map(VId),
                    EDGES
                        .iter()
                        .enumerate()
                        .map(|(at, &(source, relation, target))| {
                            (EId(at as u128 + 1), source, relation, target)
                        }),
                    |vid, predicates| {
                        Ok::<_, usize>(predicates.iter().all(|predicate| {
                            predicate.matches(&[LabelId(1)], &props[vid.0 as usize - 1])
                        }))
                    },
                    |vid, key| {
                        Ok(props[vid.0 as usize - 1]
                            .iter()
                            .find(|(candidate, _)| *candidate == key)
                            .map(|(_, value)| value))
                    },
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value
}
fn vertices(values: &[Option<u128>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .iter()
            .map(|value| {
                value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
                    GraphValue::Vertex(VId(id))
                })
            })
            .collect(),
    )
}
fn scalar_vertex(value: Option<i64>, vertex: Option<u128>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
        vertex.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
            GraphValue::Vertex(VId(id))
        }),
    ])
}

#[test]
fn optional_parts_match_an_independent_parallel_edge_and_absence_oracle() {
    let mut expected = Vec::new();
    for source in 1..=4 {
        let mut matched = false;
        for &(from, _, to) in &EDGES {
            if from == VId(source) {
                matched = true;
                expected.push(vertices(&[Some(source), Some(to.0)]));
            }
        }
        if !matched {
            expected.push(vertices(&[Some(source), None]));
        }
    }
    assert_eq!(
        rows("MATCH (a:L) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN a,b ORDER BY a,b"),
        expected,
    );
    assert_eq!(expected.len(), 5);
}

#[test]
fn unmatched_parts_preserve_carried_values_and_keep_private_join_columns_hidden() {
    let text = "MATCH (a) WHERE a.score=40 WITH a,a.score AS saved \
        OPTIONAL MATCH (a)-[:R]->(b) RETURN *";
    let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
    assert_eq!(prepared.columns(), &["a", "saved", "b"]);
    assert_eq!(
        rows(text),
        vec![GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(4)),
            GraphValue::Scalar(CanonicalScalar::Int(40)),
            GraphValue::Scalar(CanonicalScalar::Null),
        ])]
    );
    assert_eq!(
        rows(
            "MATCH (a) WHERE a.score=40 WITH a AS kept,7 AS token \
            OPTIONAL MATCH (kept)-[:R]->(b) RETURN token,kept"
        ),
        vec![scalar_vertex(Some(7), Some(4))],
    );
}

#[test]
fn property_map_correlations_and_where_filter_candidates_before_null_extension() {
    for (wanted, expected) in [
        (20, vec![vertices(&[Some(1), Some(2)]); 2]),
        (99, vec![vertices(&[Some(1), None])]),
    ] {
        assert_eq!(
            rows(&format!(
                "MATCH (a) WHERE a.score=10 WITH a,{wanted} AS wanted \
             OPTIONAL MATCH (a)-[:R]->(b {{score:wanted}}) RETURN a,b"
            )),
            expected
        );
    }
    assert_eq!(
        rows(
            "MATCH (a) WHERE a.score=10 WITH a \
            OPTIONAL MATCH (a)-[:R]->(b) WHERE b.score=99 RETURN a,b"
        ),
        vec![vertices(&[Some(1), None])],
    );
    assert_eq!(
        rows(
            "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) \
            WITH a,b WHERE b IS NULL RETURN a"
        ),
        vec![vertices(&[Some(4)])],
    );
}

#[test]
fn null_imports_remain_null_through_optional_parts_and_fail_required_matching() {
    assert_eq!(
        rows(
            "MATCH (a) WHERE a.score=40 WITH a OPTIONAL MATCH (a)-[:R]->(b) \
            WITH a,b OPTIONAL MATCH (b)-[:R]->(c) RETURN a,b,c"
        ),
        vec![vertices(&[Some(4), None, None])],
    );
    assert!(
        rows(
            "MATCH (a) WHERE a.score=40 WITH a OPTIONAL MATCH (a)-[:R]->(b) \
         WITH a,b MATCH (b)-[:R]->(c) RETURN a,c"
        )
        .is_empty()
    );
    assert_eq!(
        rows(
            "MATCH (a) WHERE a.score=10 WITH a OPTIONAL MATCH (a)-[:R]->(b) \
            WITH b AS pivot OPTIONAL MATCH (pivot)-[:R]->(c) RETURN c"
        ),
        vec![vertices(&[Some(3)]); 2],
    );
}

#[test]
fn values_only_optional_inputs_keep_bags_null_keys_and_no_match_rows() {
    assert_eq!(
        rows(
            "UNWIND [20,99,20,NULL] AS wanted WITH wanted \
            OPTIONAL MATCH (n {score:wanted}) RETURN wanted,n ORDER BY wanted NULLS FIRST,n"
        ),
        vec![
            scalar_vertex(None, None),
            scalar_vertex(Some(20), Some(2)),
            scalar_vertex(Some(20), Some(2)),
            scalar_vertex(Some(99), None),
        ],
    );
    assert_eq!(
        rows("WITH 7 AS token OPTIONAL MATCH (n) WHERE n.score<0 RETURN token,n"),
        vec![scalar_vertex(Some(7), None)],
    );
    assert_eq!(
        rows("WITH 7 AS token OPTIONAL MATCH (n) RETURN token,n ORDER BY n"),
        (1..=4)
            .map(|n| scalar_vertex(Some(7), Some(n)))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn typed_candidates_preserve_full_width_ids_without_coercing_scalar_integers() {
    let prepared = PreparedGraphSetText::prepare(
        "UNWIND $ids AS seed WITH seed OPTIONAL MATCH (seed)-[:R]->(neighbor) \
         RETURN seed,neighbor",
        symbols,
    )
    .unwrap();
    let arguments = GqlParameters::new()
        .with_list(
            "ids",
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(1)),
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ],
        )
        .unwrap();
    let calls = Cell::new(0);
    let observed = run(
        &prepared.bind_parameters(&arguments).unwrap(),
        wide(),
        None,
        &calls,
        || Ok(()),
    )
    .unwrap();
    let mut expected = vec![vertices(&[Some(1), Some(2)]); 4];
    expected.push(scalar_vertex(Some(1), None));
    expected.push(vertices(&[Some(u128::MAX), None]));
    expected.push(vertices(&[None, None]));
    expected.sort();
    let mut actual = observed.value;
    actual.sort();
    assert_eq!(actual, expected);
    assert_eq!(calls.get(), 1);
    assert_eq!(observed.rows.snapshot_records, 8);
}

#[test]
fn local_pages_and_distinct_finish_before_the_next_optional_part() {
    assert_eq!(
        rows(
            "MATCH (a) WITH a ORDER BY a DESC LIMIT 1 \
            OPTIONAL MATCH (a)-[:R]->(b) RETURN a,b"
        ),
        vec![vertices(&[Some(4), None])],
    );
    for (quantifier, count) in [("", 4), ("DISTINCT ", 2)] {
        assert_eq!(
            rows(&format!(
                "MATCH (a)-[:R]->(b) WITH {quantifier}b AS pivot \
             OPTIONAL MATCH (pivot)-[:R]->(c) RETURN c"
            )),
            vec![vertices(&[Some(3)]); count]
        );
    }
    assert_eq!(
        rows(
            "(MATCH (a) WHERE a.score=40 WITH a \
            OPTIONAL MATCH (a)-[:R]->(b) RETURN b) \
            UNION ALL (MATCH (n) WHERE n.score=20 RETURN n AS b) \
            ORDER BY b NULLS FIRST"
        ),
        vec![vertices(&[None]), vertices(&[Some(2)])],
    );
}

#[test]
fn optional_kind_and_parameters_are_bound_into_the_existing_transcripts() {
    let text = "MATCH (a) WHERE a.score=$start WITH a \
        OPTIONAL MATCH (a)-[:R]->(b) WHERE b.score=$wanted RETURN a,b";
    let mut calls = 0;
    let template = PreparedGraphSetText::prepare(text, |kind, name| {
        calls += 1;
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls, 2); // one property and one relation, cached across parts
    assert_eq!(template.parameter_schema().len(), 2);
    let transcript = template.canonical_template_bytes();
    let required = PreparedGraphSetText::prepare(&text.replace("OPTIONAL ", ""), symbols).unwrap();
    assert_ne!(required.canonical_template_bytes(), transcript);
    let mut previous = None;
    for (wanted, expected) in [
        (20, vec![vertices(&[Some(1), Some(2)]); 2]),
        (99, vec![vertices(&[Some(1), None])]),
    ] {
        let args = GqlParameters::new()
            .with_int64("start", 10)
            .unwrap()
            .with_int64("wanted", wanted)
            .unwrap();
        let bound = template.bind_parameters(&args).unwrap();
        assert_ne!(
            required.bind_parameters(&args).unwrap().canonical_bytes(),
            bound.canonical_bytes()
        );
        if let Some(previous) = previous {
            assert_ne!(previous, bound.canonical_bytes());
        }
        previous = Some(bound.canonical_bytes());
        assert_eq!(
            run(&bound, wide(), None, &Cell::new(0), || Ok(()))
                .unwrap()
                .value,
            expected
        );
        assert_eq!(template.canonical_template_bytes(), transcript);
    }
    let args = GqlParameters::new().with_int64("start", 10).unwrap();
    assert_eq!(
        template.bind_parameters(&args).unwrap_err().offset,
        text.find("$wanted").unwrap()
    );
    assert!(
        template
            .bind_parameters(&args.with_uint64("wanted", 20).unwrap())
            .is_err()
    );
    assert_eq!(calls, 2);
}

#[test]
fn unsupported_optional_scopes_and_carried_dereferences_refuse_before_catalog() {
    for text in [
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN a.score",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN labels(a) AS labels",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN SIZE(labels(a)) AS size",
        "MATCH (a) WITH a.score AS a OPTIONAL MATCH (a) RETURN a",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:R]->(c) RETURN c",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) OPTIONAL MATCH (b)-[:R]->(c) RETURN c",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) WHERE EXISTS { MATCH (b)-[:R]->(c) } RETURN b",
        "MATCH (a) WITH a OPTIONAL RETURN a",
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
    // The guard is not a blanket refusal of properties on OPTIONAL output.
    assert_eq!(
        rows(
            "MATCH (a) WHERE a.score=10 WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN b.score AS score"
        ),
        vec![
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(20))]);
            2
        ],
    );
    assert_eq!(
        rows(
            "MATCH (a) WHERE a.score=40 WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN b.score AS score"
        ),
        vec![vertices(&[None])],
    );
}

#[test]
fn empty_input_limit_zero_and_unmatched_results_never_hide_late_failures() {
    for text in [
        "MATCH (a) WHERE a.score<0 WITH a OPTIONAL MATCH (b) RETURN b",
        "MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN b LIMIT 0",
        "MATCH (a) WHERE a.score<0 WITH a OPTIONAL MATCH (b) RETURN b LIMIT 0",
    ] {
        let calls = Cell::new(0);
        assert!(matches!(
            run(&bind(text), wide(), Some(2), &calls, || Ok(())),
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(77))),
        ));
        assert_eq!(calls.get(), 2);
    }
    assert!(matches!(
        run(
            &bind(
                "MATCH (a) WHERE a.score=40 WITH a OPTIONAL MATCH (a)-[:R]->(b) \
            RETURN 1/0 AS bad LIMIT 0"
            ),
            wide(),
            None,
            &Cell::new(0),
            || Ok(())
        ),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        )),
    ));
}

#[test]
fn exact_quotas_and_every_cancellation_checkpoint_cover_optional_join_and_delivery() {
    let query = bind("MATCH (a) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN a,b");
    let checkpoints = Cell::new(0);
    let measured = run(&query, wide(), None, &Cell::new(0), || {
        checkpoints.set(checkpoints.get() + 1);
        Ok(())
    })
    .unwrap();
    assert_eq!(measured.rows.snapshot_records, 16);
    assert_eq!(measured.rows.result_rows, 5);
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
            None,
            &Cell::new(0),
            || Ok(())
        )
        .unwrap(),
        measured,
    );
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                None,
                &Cell::new(0),
                || Ok(())
            )
            .is_err()
        );
    }
    for stop in 1..=checkpoints.get() {
        let seen = Cell::new(0);
        let result = run(&query, wide(), None, &Cell::new(0), || {
            seen.set(seen.get() + 1);
            if seen.get() == stop {
                Err(stop)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen.get(), stop);
    }
}

/// `UNWIND ... OPTIONAL MATCH` with no WITH between is a Left continuation of
/// the unwound rows: every value keeps its multiplicity, and a value with no
/// witness is one NULL-extended row. Expectations come from EDGES by hand:
/// score s is vertex s/10, R runs 1->2 twice, 2->3 and 3->3.
#[test]
fn optional_match_directly_after_unwind_null_extends_each_unwound_value() {
    assert_eq!(
        rows(
            "UNWIND [10, 30, 40] AS s OPTIONAL MATCH (a {score: s})-[:R]->(b) \
             RETURN s, b ORDER BY s, b"
        ),
        vec![
            scalar_vertex(Some(10), Some(2)),
            scalar_vertex(Some(10), Some(2)),
            scalar_vertex(Some(30), Some(3)),
            scalar_vertex(Some(40), None),
        ]
    );
    assert_eq!(
        rows(
            "UNWIND [10, 50, 10] AS s OPTIONAL MATCH (a) WHERE a.score = s RETURN s, a ORDER BY s, a"
        ),
        vec![
            scalar_vertex(Some(10), Some(1)),
            scalar_vertex(Some(10), Some(1)),
            scalar_vertex(Some(50), None),
        ]
    );
    // An inner UNWIND ... MATCH keeps its existing correlated-root route.
    assert_eq!(
        rows("UNWIND [10, 50] AS s MATCH (a {score: s}) RETURN s, a"),
        vec![scalar_vertex(Some(10), Some(1))]
    );
}

/// A leading OPTIONAL MATCH is a Left continuation of the unit relation: its
/// witnesses, or exactly one NULL-extended row (openCypher, TCK Match7).
#[test]
fn a_leading_optional_match_is_a_left_join_of_the_unit_row() {
    assert_eq!(
        rows("OPTIONAL MATCH (a {score: 70}) RETURN a"),
        vec![vertices(&[None])]
    );
    assert_eq!(
        rows("OPTIONAL MATCH (a {score: 20})-[:R]->(b) RETURN b"),
        vec![vertices(&[Some(3)])]
    );
    assert_eq!(
        rows("OPTIONAL MATCH (a) WHERE a.score > 20 RETURN a ORDER BY a"),
        vec![vertices(&[Some(3)]), vertices(&[Some(4)])]
    );
    // The NULL row flows through WITH into a later required part.
    assert_eq!(
        rows("OPTIONAL MATCH (a {score: 70}) WITH a MATCH (c {score: 10}) RETURN a, c"),
        vec![vertices(&[None, Some(1)])]
    );
    // A later required part that expands from the NULL vertex matches nothing
    // (TCK Match3 [27]); from a matched root it expands as usual.
    assert_eq!(
        rows("OPTIONAL MATCH (a {score: 70}) WITH a MATCH (a)-[:R]->(b) RETURN b"),
        Vec::<GraphValueRow>::new()
    );
    assert_eq!(
        rows("OPTIONAL MATCH (a {score: 20}) WITH a MATCH (a)-[:R]->(b) RETURN b"),
        vec![vertices(&[Some(3)])]
    );
    // A set operand may open with OPTIONAL MATCH too.
    let union =
        rows("MATCH (a {score: 10}) RETURN a UNION ALL OPTIONAL MATCH (a {score: 70}) RETURN a");
    assert_eq!(union.len(), 2, "{union:?}");
    assert!(union.contains(&vertices(&[Some(1)])), "{union:?}");
    assert!(union.contains(&vertices(&[None])), "{union:?}");
}

/// A relationship carried past WITH correlates by identity in the next MATCH,
/// as a carried vertex does (TCK Match2 [7], Match3 [24] to [26], Match7 [4]
/// to [6], With1 [3], With3 [1]): it matches that one relationship again, in
/// its own direction and types, or not at all. The parallel 1->2 edges stay
/// two rows.
#[test]
fn a_carried_relationship_matches_itself_again() {
    let pairs = |found: &[GraphValueRow]| {
        let mut pairs: Vec<_> = found
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .map(|value| match value {
                        GraphValue::Vertex(id) => Some(id.0),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        pairs.sort();
        pairs
    };
    let all = vec![
        vec![Some(1), Some(2)],
        vec![Some(1), Some(2)],
        vec![Some(2), Some(3)],
        vec![Some(3), Some(3)],
    ];
    assert_eq!(
        pairs(&identified_rows(
            "MATCH (a)-[r:R]->(b) WITH r, a MATCH (a)-[r:R]->(c) RETURN a, c"
        )),
        all
    );
    assert_eq!(
        pairs(&identified_rows(
            "MATCH (a)-[r:R]->(b) WITH r AS s MATCH (x)-[s:R]->(y) RETURN x, y"
        )),
        all
    );
    // Reversed, only the self-loop is the same relationship; OPTIONAL keeps
    // every carried row and NULL-extends the rest.
    assert_eq!(
        pairs(&identified_rows(
            "MATCH (a)-[r:R]->(b) WITH r, a MATCH (a)<-[r:R]-(c) RETURN a, c"
        )),
        vec![vec![Some(3), Some(3)]]
    );
    assert_eq!(
        pairs(&identified_rows(
            "MATCH (a)-[r:R]->(b) WITH r, a OPTIONAL MATCH (a)<-[r:R]-(c) RETURN a, c"
        )),
        vec![
            vec![Some(1), None],
            vec![Some(1), None],
            vec![Some(2), None],
            vec![Some(3), Some(3)],
        ]
    );
    // A path name, or a scalar under the relationship's name, still refuses.
    for text in [
        "MATCH p = (a)-[:R]->(b) WITH p MATCH p = (x)-[:R]->(y) RETURN x",
        "MATCH (a)-[r:R]->(b) WITH a.score AS r MATCH (x)-[r:R]->(y) RETURN x",
    ] {
        assert!(
            PreparedGraphSetText::prepare(text, symbols).is_err(),
            "{text}"
        );
    }
}

/// A RETURN item after a leading OPTIONAL MATCH is a whole expression, as it
/// is after a required MATCH: a comparison, IS [NOT] NULL or Boolean connector
/// over the optional binding is the item's three-valued Boolean (openCypher,
/// TCK Null1 [2], [3] and Null2 [2], [3]: `n.missing IS NULL`).
#[test]
fn a_leading_optional_match_returns_whole_predicate_items() {
    let flags = |values: &[Option<bool>]| {
        GraphValueRow::from_owned_values(
            values
                .iter()
                .map(|value| {
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
                })
                .collect(),
        )
    };
    // No witness: the NULL row's property is NULL, so a comparison is UNKNOWN.
    assert_eq!(
        rows(
            "OPTIONAL MATCH (a {score: 70}) RETURN a.score IS NULL AS absent, \
             a.score IS NOT NULL AS present, a.score = 70 AS same"
        ),
        vec![flags(&[Some(true), Some(false), None])]
    );
    // Witnesses: one Boolean row each, connectors included.
    let found = rows(
        "OPTIONAL MATCH (a) WHERE a.score > 20 \
         RETURN a.score = 30 AS third, a.score > 30 OR a.score < 0 AS last",
    );
    assert_eq!(found.len(), 2, "{found:?}");
    assert!(
        found.contains(&flags(&[Some(true), Some(false)])),
        "{found:?}"
    );
    assert!(
        found.contains(&flags(&[Some(false), Some(true)])),
        "{found:?}"
    );
    // IS [NOT] NULL over the optional element itself (TCK Match7 [29] to
    // [31], Return3 [1]): only its NULL-ness is observed.
    assert_eq!(
        rows("OPTIONAL MATCH (a {score: 70}) RETURN a IS NULL AS absent, a IS NOT NULL AS present"),
        vec![flags(&[Some(true), Some(false)])]
    );
    // Vertex 4 has no outgoing R: its NULL-extended row's edge is NULL.
    let scored = |score: i64, unmatched: bool| {
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Int(score)),
            GraphValue::Scalar(CanonicalScalar::Bool(unmatched)),
        ])
    };
    assert_eq!(
        identified_rows(
            "MATCH (a) WITH a, a.score AS score OPTIONAL MATCH (a)-[r:R]->(b) \
             RETURN score, r IS NULL AS unmatched ORDER BY score"
        ),
        vec![
            scored(10, false),
            scored(10, false),
            scored(20, false),
            scored(30, false),
            scored(40, true),
        ]
    );
}

/// fgdb-23qrt: `WITH r MATCH ()-[s]->() WHERE r = s` correlates `s` with the
/// carried relationship by identity, as reusing the name `[r]` does
/// (TCK Comparison1 [5]): each stored relationship matches only itself.
#[test]
fn a_top_level_where_identity_correlates_a_carried_relationship() {
    let rows =
        identified_rows("MATCH ()-[r:R]->() WITH r MATCH ()-[s:R]->() WHERE r = s RETURN r, s");
    assert_eq!(rows.len(), EDGES.len());
    assert!(rows.iter().all(|row| row.values()[0] == row.values()[1]));
    let distinct: std::collections::BTreeSet<_> =
        rows.iter().map(|row| row.values()[0].clone()).collect();
    assert_eq!(distinct.len(), EDGES.len());
}

/// An aggregating WITH may key on, or aggregate, a captured path's metadata:
/// `length(p)`, `nodes(p)` and `relationships(p)` each read the graph source
/// once as a hidden grouping input (TCK Quantifier1-4 [9]).
#[test]
fn an_aggregating_with_keys_on_captured_path_metadata() {
    use std::collections::BTreeMap;
    let int = |n: i64| GraphValue::Scalar(CanonicalScalar::Int(n));
    let pair = |a: i64, b: i64| GraphValueRow::from_owned_values(vec![int(a), int(b)]);
    // One- and two-edge trails over the fixture (DIFFERENT EDGES: no edge
    // twice, so the self-loop cannot follow itself), by edge index.
    let mut trails: Vec<Vec<usize>> = (0..EDGES.len()).map(|edge| vec![edge]).collect();
    for first in 0..EDGES.len() {
        for second in 0..EDGES.len() {
            if first != second && EDGES[first].2 == EDGES[second].0 {
                trails.push(vec![first, second]);
            }
        }
    }
    let by_length = |offset: i64| {
        let mut groups = BTreeMap::<i64, i64>::new();
        for trail in &trails {
            *groups.entry(trail.len() as i64 + offset).or_default() += 1;
        }
        groups
            .into_iter()
            .map(|(key, count)| pair(key, count))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        identified_rows(
            "MATCH p = (a)-[:R*1..2]->(b) WITH length(p) AS l, count(*) AS c RETURN l, c ORDER BY l"
        ),
        by_length(0)
    );
    assert_eq!(
        identified_rows(
            "MATCH p = (a)-[:R*1..2]->(b) WITH size(nodes(p)) AS n, count(*) AS c \
             RETURN n, c ORDER BY n"
        ),
        by_length(1)
    );
    // Grouping by the remaining relationships: each one-edge trail leaves [],
    // each two-edge trail leaves its second edge.
    let mut rests = BTreeMap::<Vec<usize>, i64>::new();
    for trail in &trails {
        *rests.entry(trail[1..].to_vec()).or_default() += 1;
    }
    let mut expected: Vec<_> = rests
        .iter()
        .map(|(rest, count)| (rest.len() as i64, *count))
        .collect();
    expected.sort();
    assert_eq!(
        identified_rows(
            "MATCH p = (a)-[:R*1..2]->(b) WITH tail(relationships(p)) AS rest, count(*) AS c \
             RETURN size(rest) AS s, c ORDER BY s, c"
        ),
        expected
            .into_iter()
            .map(|(size, count)| pair(size, count))
            .collect::<Vec<_>>()
    );
    // A metadata call whose domain is not a path stays refused.
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH p = (a)-[:R*1..2]->(b) WITH labels(p) AS l, count(*) AS c RETURN l, c",
            symbols
        )
        .is_err()
    );
}
