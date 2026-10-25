//! Relationship predicates execute through real identified-edge GLA sources.
//! Equal endpoints do not make parallel relationships the same identity.

use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphPipelineAggregateText, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::cell::RefCell;

const R: RelationId = RelationId(1);
const EDGES: [(EId, VId, RelationId, VId); 4] = [
    (EId(11), VId(1), R, VId(2)),
    (EId(12), VId(1), R, VId(2)),
    (EId(13), VId(2), R, VId(3)),
    (EId(14), VId(3), R, VId(3)),
];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    (kind == GraphSymbolKind::Relation && name == "R").then_some(GraphSymbol::Relation(R))
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
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(
        policy,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_identified_properties(
                7,
                (1..=3).map(VId),
                EDGES,
                |_, tests| Ok::<_, &'static str>(tests.is_empty()),
                |_, _| Ok(None),
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
fn pair(left: u128, right: Option<u128>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Edge(EId(left)),
        right.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
            GraphValue::Edge(EId(id))
        }),
    ])
}

#[test]
fn relationship_equality_and_inequality_agree_in_row_filters_and_match_on() {
    for (operator, equal) in [("=", true), ("<>", false), ("!=", false)] {
        let expected: Vec<_> = (11..=14)
            .flat_map(|r| (11..=14).map(move |s| (r, s)))
            .filter(|(r, s)| (r == s) == equal)
            .map(|(r, s)| pair(r, Some(s)))
            .collect();
        assert_eq!(expected.len(), if equal { 4 } else { 12 });
        for predicate in [format!("r {operator} s"), format!("s {operator} r")] {
            for boundary in ["", "WITH r,s "] {
                let text = format!(
                    "MATCH (a)-[r:R]->(b) WITH r MATCH (c)-[s:R]->(d) \
                     {boundary}WHERE {predicate} RETURN r,s ORDER BY r,s"
                );
                assert_eq!(execute(&text), expected, "{text}");
            }
        }
    }
}

#[test]
fn relationship_values_through_unwind_preserve_duplicates_and_never_equal_non_edges() {
    for (operator, equal) in [("=", true), ("<>", false)] {
        let mut expected = Vec::new();
        for r in 11..=14 {
            for s in 11..=14 {
                if (r == s) == equal {
                    expected.extend([pair(r, Some(s)), pair(r, Some(s))]);
                }
            }
        }
        // The integer 11 is of another domain than every relationship
        // (fgdb-v6ku6): never `=`, always `<>`. Numbers order after
        // relationships; each carried r contributes one 11.
        if !equal {
            for s in 11..=14 {
                for _ in 11..=14 {
                    expected.push(GraphValueRow::from_owned_values(vec![
                        GraphValue::Scalar(CanonicalScalar::Int(11)),
                        GraphValue::Edge(EId(s)),
                    ]));
                }
            }
        }
        let text = format!(
            "MATCH (a)-[r:R]->(b) WITH r UNWIND [r,r,NULL,11] AS saved WITH saved \
             MATCH (c)-[s:R]->(d) WHERE saved {operator} s \
             RETURN saved,s ORDER BY saved,s"
        );
        assert_eq!(execute(&text), expected, "{text}");
    }
}

#[test]
fn optional_relationship_inequality_keeps_the_carried_edge_when_no_witness_exists() {
    // Only edge 14 is a self-loop. It witnesses every other carried edge,
    // but cannot witness itself: ON must run before the left null extension.
    assert_eq!(
        execute(
            "MATCH (a)-[r:R]->(b) WITH r OPTIONAL MATCH (c)-[s:R]->(d) \
             WHERE r <> s AND c = d RETURN r,s ORDER BY r"
        ),
        vec![
            pair(11, Some(14)),
            pair(12, Some(14)),
            pair(13, Some(14)),
            pair(14, None)
        ],
    );
}

#[test]
fn relationship_aggregate_terminals_count_matches_not_null_extensions() {
    for optional in [false, true] {
        let continuation = if optional { "OPTIONAL MATCH" } else { "MATCH" };
        let local = if optional { " AND c = d" } else { "" };
        let text = format!(
            "MATCH (a)-[r:R]->(b) WITH r {continuation} (c)-[s:R]->(d) \
             WHERE r <> s{local} RETURN r,COUNT(*) AS rows,COUNT(s) AS matches,\
             COUNT(DISTINCT s) AS distincts ORDER BY r"
        );
        let query = PreparedGraphPipelineAggregateText::prepare(&text, symbols)
            .unwrap()
            .bind_relation_parameters(&GqlParameters::new())
            .unwrap();
        let result = query
            .execute_governed(
                wide(),
                |pattern, remaining| {
                    pattern.plan().execute_governed_with_identified_properties(
                        7,
                        (1..=3).map(VId),
                        EDGES,
                        |_, tests| Ok::<_, &'static str>(tests.is_empty()),
                        |_, _| Ok(None),
                        remaining,
                        || Ok::<_, usize>(()),
                    )
                },
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 4);
        for (r, row) in (11..=14).zip(result.value) {
            let witnesses = EDGES
                .iter()
                .filter(|(s, from, _, to)| s.0 != r && (!optional || from == to))
                .count() as u64;
            assert_eq!(row.keys(), &[GraphValue::Edge(EId(r))]);
            assert_eq!(
                row.values()[0].as_count(),
                Some(witnesses.max(u64::from(optional)))
            );
            assert_eq!(row.values()[1].as_count(), Some(witnesses));
            assert_eq!(row.values()[2].as_count(), Some(witnesses));
        }
    }
}

#[test]
fn relationship_match_predicates_keep_budgets_and_cancellation_atomic() {
    let query = prepare(
        "MATCH (a)-[r:R]->(b) WITH r OPTIONAL MATCH (c)-[s:R]->(d) \
         WHERE r <> s AND c = d RETURN r,s ORDER BY r",
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
            &mut || Ok(())
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

/// Relationships and paths compare by identity in a projection, as in WHERE:
/// parallel relationships between the same endpoints are different, a path
/// compares node by node and relationship by relationship, so a self-loop
/// traversed either way is one path (TCK Comparison1 [14]).
#[test]
fn relationships_and_paths_compare_by_identity_in_projections() {
    let truth = |value: bool| {
        GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Bool(value))])
    };
    let bag = |text: &str| {
        let mut rows = execute(text);
        rows.sort();
        rows
    };
    // Same endpoints: 11 and 12 are parallel, 13 and the loop 14 alone.
    assert_eq!(
        bag("MATCH (a)-[r:R]->(b) MATCH (a)-[s:R]->(b) RETURN r = s AS same"),
        vec![
            truth(false),
            truth(false),
            truth(true),
            truth(true),
            truth(true),
            truth(true)
        ]
    );
    assert_eq!(
        bag("MATCH (a)-[r:R]->(b) MATCH (a)-[s:R]->(b) RETURN r <> s AS different"),
        vec![
            truth(false),
            truth(false),
            truth(false),
            truth(false),
            truth(true),
            truth(true)
        ]
    );
    assert_eq!(
        bag("MATCH (a)-[r:R]->(b) RETURN CASE WHEN r = r THEN true ELSE false END AS c"),
        vec![truth(true); 4]
    );
    assert_eq!(
        bag("MATCH p = (a)-[:R]->(b) RETURN p = p AS same"),
        vec![truth(true); 4]
    );
    assert_eq!(
        bag("MATCH p1 = (a)-[:R]->(a) MATCH p2 = (a)<-[:R]-(a) RETURN p1 = p2 AS same"),
        vec![truth(true)]
    );
}
