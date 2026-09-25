//! Optional read-part continuations use the ordinary governed GLA and row join.
//! Expected bags below come from explicit multigraph enumeration, not another
//! query or a second optional matcher. The source fixture never changes mid-run.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, VId};
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
        .map(|id| vec![(P, CanonicalScalar::Int(id * 10))])
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
