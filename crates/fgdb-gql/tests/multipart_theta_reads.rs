//! Multipart theta joins through the public text compiler and governed GLA.
//! Independent vertex/edge enumeration supplies expected bags; source callbacks
//! execute actual plans, including their policy and interruption boundaries.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::RefCell;

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
    let values = [1, 2, 2, 3]
        .map(|value| fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(value)));
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(
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
fn nullable_pair(value: Option<i64>, vertex: Option<u128>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)),
        vertex.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
            GraphValue::Vertex(VId(id))
        }),
    ])
}

#[test]
fn carried_vertex_inequality_matches_both_spellings_and_operand_orders() {
    let expected: Vec<_> = (1..=4)
        .flat_map(|left| (1..=4).map(move |right| (left, right)))
        .filter(|(left, right)| left != right)
        .map(|(left, right)| vertices(&[left, right]))
        .collect();
    for predicate in ["n <> m", "m <> n", "n != m", "m != n"] {
        let text = format!("MATCH (n) WITH n MATCH (m) WHERE {predicate} RETURN n,m ORDER BY n,m");
        assert_eq!(execute(&text), expected, "{text}");
    }
}

#[test]
fn all_carried_scalar_comparisons_preserve_direction_and_duplicate_scores() {
    type Comparison = fn(i64, i64) -> bool;
    let cases: [(&str, &str, Comparison); 6] = [
        ("<>", "<>", |a, b| a != b),
        ("!=", "!=", |a, b| a != b),
        ("<", ">", |a, b| a < b),
        ("<=", ">=", |a, b| a <= b),
        (">", "<", |a, b| a > b),
        (">=", "<=", |a, b| a >= b),
    ];
    let scores = [1, 2, 2, 3];
    for (operator, inverse, accepts) in cases {
        let mut expected = Vec::new();
        for (n, threshold) in scores.iter().enumerate() {
            for (m, value) in scores.iter().enumerate() {
                if accepts(*value, *threshold) {
                    expected.push(vertices(&[n as u128 + 1, m as u128 + 1]));
                }
            }
        }
        for predicate in [
            format!("m.p {operator} threshold"),
            format!("threshold {inverse} m.p"),
        ] {
            let text = format!(
                "MATCH (n) WITH n,n.p AS threshold MATCH (m) WHERE {predicate} \
                 RETURN n,m ORDER BY n,m"
            );
            assert_eq!(execute(&text), expected, "{text}");
        }
    }
}

#[test]
fn theta_predicates_do_not_replace_typed_identity_keys_or_parallel_edges() {
    let scores = [1, 2, 2, 3];
    let expected: Vec<_> = EDGES
        .iter()
        .filter(|(n, _, m)| scores[m.0 as usize - 1] > scores[n.0 as usize - 1])
        .map(|(n, _, m)| vertices(&[n.0, m.0]))
        .collect();
    assert_eq!(
        execute(
            "MATCH (n) WITH n,n.p AS threshold MATCH (n)-[:R]->(m) \
             WHERE m.p > threshold RETURN n,m ORDER BY n,m"
        ),
        expected,
    );
    assert_eq!(expected.len(), 4);
}

#[test]
fn any_domain_equality_on_is_conjoined_not_overwritten() {
    assert_eq!(
        execute(
            "UNWIND [1,2,NULL] AS wanted WITH wanted MATCH (n {p:wanted}) \
             WHERE n.p >= wanted RETURN wanted,n ORDER BY wanted,n"
        ),
        vec![
            scalar_vertex(1, 1),
            scalar_vertex(2, 2),
            scalar_vertex(2, 3)
        ],
    );
    // Replacing the Any equality would incorrectly admit larger values.
    assert!(
        execute(
            "UNWIND [1,2,NULL] AS wanted WITH wanted MATCH (n {p:wanted}) \
             WHERE n.p > wanted RETURN wanted,n"
        )
        .is_empty()
    );
}

#[test]
fn optional_theta_extends_only_unmatched_inputs_with_null_and_keeps_multiplicity() {
    let mut actual = execute(
        "UNWIND [2,2,3,NULL] AS wanted WITH wanted OPTIONAL MATCH (m) \
         WHERE m.p > wanted RETURN wanted,m",
    );
    let mut expected = vec![
        scalar_vertex(2, 4),
        scalar_vertex(2, 4),
        nullable_pair(Some(3), None),
        nullable_pair(None, None),
    ];
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
    assert_eq!(
        execute(
            "UNWIND [2,2,3,NULL] AS wanted WITH wanted MATCH (m) \
             WHERE m.p > wanted RETURN wanted,m ORDER BY wanted,m"
        ),
        vec![scalar_vertex(2, 4), scalar_vertex(2, 4)],
    );
}

#[test]
fn optional_local_filter_and_theta_run_before_null_extension() {
    let mut actual = execute(
        "UNWIND [2,2,3,NULL] AS wanted WITH wanted OPTIONAL MATCH (m) \
         WHERE m.p > wanted AND m.p < 3 RETURN wanted,m",
    );
    let mut expected = vec![
        nullable_pair(Some(2), None),
        nullable_pair(Some(2), None),
        nullable_pair(Some(3), None),
        nullable_pair(None, None),
    ];
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn multiple_theta_conjuncts_are_eager_checked_on_bytecode() {
    assert_eq!(
        execute(
            "UNWIND [1] AS low WITH low,3 AS high MATCH (m) \
             WHERE m.p > low AND high > m.p RETURN m ORDER BY m"
        ),
        vec![vertices(&[2]), vertices(&[3])],
    );
}

#[test]
fn earlier_with_page_is_not_moved_after_the_theta_join() {
    assert_eq!(
        execute(
            "MATCH (n) WITH n,n.p AS threshold ORDER BY n LIMIT 1 \
             MATCH (m) WHERE m.p > threshold RETURN n,m ORDER BY n,m"
        ),
        vec![vertices(&[1, 2]), vertices(&[1, 3]), vertices(&[1, 4])],
    );
}

#[test]
fn theta_template_identity_encodes_comparisons_and_normalizes_operand_direction() {
    let template = |predicate: &str| {
        PreparedGraphSetText::prepare(
            &format!("MATCH (n) WITH n,n.p AS threshold MATCH (m) WHERE {predicate} RETURN n,m"),
            symbols,
        )
        .unwrap()
    };
    let greater = template("m.p > threshold");
    let reversed = template("threshold < m.p");
    let inclusive = template("m.p >= threshold");
    assert_eq!(
        greater.canonical_template_bytes(),
        reversed.canonical_template_bytes()
    );
    assert_ne!(
        greater.canonical_template_bytes(),
        inclusive.canonical_template_bytes()
    );
    assert_ne!(
        greater
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes(),
        inclusive
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes(),
    );
    assert_eq!(
        template("m.p <> threshold").canonical_template_bytes(),
        template("m.p != threshold").canonical_template_bytes(),
    );
    let marker = b"fgdb:gql:match-continuations:theta:v1\0";
    assert!(
        greater
            .canonical_template_bytes()
            .windows(marker.len())
            .any(|w| w == marker)
    );
    assert!(
        !template("m.p = threshold")
            .canonical_template_bytes()
            .windows(marker.len())
            .any(|w| w == marker)
    );
}

#[test]
fn theta_work_is_inside_cumulative_budgets_and_cancellation() {
    let query = prepare(
        "UNWIND [2,3,NULL] AS wanted WITH wanted OPTIONAL MATCH (m) \
         WHERE m.p > wanted RETURN wanted,m",
    );
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
            &mut || Ok(()),
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
                &mut || Ok(()),
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
fn computed_carried_operands_keep_required_theta_results_and_direction() {
    let scores = [1, 2, 2, 3];
    let mut expected = Vec::new();
    for threshold in scores {
        for (m, value) in scores.iter().enumerate() {
            if *value > threshold + 1 {
                expected.push(vertices(&[m as u128 + 1]));
            }
        }
    }
    for predicate in ["m.p > t + 1", "t + 1 < m.p"] {
        let text =
            format!("MATCH (n) WITH n.p AS t MATCH (m) WHERE {predicate} RETURN m ORDER BY m");
        assert_eq!(execute(&text), expected, "{text}");
    }
}

#[test]
fn boolean_comparison_forms_filter_after_the_join_and_unsupported_domains_refuse() {
    // A carried vertex's property re-matches it by identity (fgdb-dhcfo;
    // multipart_reads pins the rows).
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (n) WITH n MATCH (m) WHERE m.p > n.p RETURN n,m",
            symbols
        )
        .is_ok()
    );
    // A WHERE that is no join key or ON comparison is the row filter after
    // the join, as `WITH * WHERE` spells it (fgdb-c6hvm): the same template,
    // so the same rows. p = [1, 2, 2, 3], so m.p = 2 adds the pairs (2, 2)
    // and (3, 3) to the 12 unequal ones, and t + m.p > m.p for every t.
    for (text, rows) in [
        (
            "MATCH (n) WITH n MATCH (m) WHERE n <> m OR m.p = 2 RETURN n,m",
            14,
        ),
        ("MATCH (n) WITH n MATCH (m) WHERE NOT n <> m RETURN n,m", 4),
        ("MATCH (n) WITH n MATCH (m) WHERE (n <> m) RETURN n,m", 12),
        (
            "MATCH (n) WITH n.p AS t MATCH (m) WHERE m.p > t + m.p RETURN m",
            0,
        ),
    ] {
        let explicit = text.replacen(" WHERE ", " WITH * WHERE ", 1);
        assert_eq!(
            PreparedGraphSetText::prepare(text, symbols)
                .unwrap()
                .canonical_template_bytes(),
            PreparedGraphSetText::prepare(&explicit, symbols)
                .unwrap()
                .canonical_template_bytes(),
            "{text}"
        );
        assert_eq!(execute(text).len(), rows, "{text}");
    }
    // Vertex ordering and a comparison beside a later OPTIONAL scope stay
    // refused before any catalog call.
    for text in [
        "MATCH (n) WITH n MATCH (m) WHERE n < m RETURN n,m",
        "MATCH (n) WITH n MATCH (m) WHERE n <> m OPTIONAL MATCH (m)-[:R]->(x) RETURN n,m",
    ] {
        let mut calls = 0;
        let result = PreparedGraphSetText::prepare(text, |kind, name| {
            calls += 1;
            symbols(kind, name)
        });
        assert!(result.is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    // openCypher allows a reserved word as a property name. The row filter
    // reads `m.RETURN`, so this refusal now comes from the catalog, which
    // binds only `p`.
    let mut calls = 0;
    let result = PreparedGraphSetText::prepare(
        "MATCH (n) WITH n.p AS t MATCH (m) WHERE m.RETURN > t RETURN m",
        |kind, name| {
            calls += 1;
            symbols(kind, name)
        },
    );
    assert!(result.is_err());
    assert!(calls > 0);
}
