use super::*;
use crate::algebra::{GraphValue, GraphValueRow};
use crate::{GqlQueryError, GqlQueryPolicy, PreparedGraphSetText};
use fgdb_types::{EId, VId};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn scalar(n: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(n))
}
fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}
fn execute(text: &str) -> (Vec<GraphValueRow>, usize) {
    let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
    let bound = prepared.bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(prepared.column_types(), bound.column_types());
    let p = [
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
    ];
    let q = [
        CanonicalScalar::Int(10),
        CanonicalScalar::Int(20),
        CanonicalScalar::Int(30),
    ];
    let edge_p = CanonicalScalar::Int(5);
    let mut calls = 0;
    let result = bound
        .execute_governed(
            GqlQueryPolicy::new(1000, 1000, 2_000_000, 2_000_000),
            |pattern, remaining| {
                calls += 1;
                pattern.plan().execute_governed_with_element_properties(
                    6,
                    [VId(1), VId(2), VId(3)],
                    [
                        (EId(10), VId(1), RelationId(1), VId(2)),
                        (EId(11), VId(1), RelationId(1), VId(2)),
                        (EId(12), VId(2), RelationId(1), VId(3)),
                    ],
                    |_, _| Ok::<_, &'static str>(true),
                    |vid, key| {
                        Ok(match key {
                            PropertyKeyId(1) => p.get((vid.0 - 1) as usize),
                            PropertyKeyId(2) => q.get((vid.0 - 1) as usize),
                            _ => None,
                        })
                    },
                    |_, key| Ok((key == PropertyKeyId(1)).then_some(&edge_p)),
                    remaining,
                    || Ok::<_, &'static str>(()),
                )
            },
            || Ok::<_, &'static str>(()),
        )
        .unwrap();
    (result.value, calls)
}

#[test]
fn direct_graph_with_groups_property_expressions_and_filters_before_delivery() {
    let (rows, calls) = execute("MATCH (n) WITH n.p AS p, count(*) AS c WHERE c > 1 RETURN p, c");
    assert_eq!(calls, 1);
    assert_eq!(rows, vec![row(vec![scalar(1), scalar(2)])]);
    assert_eq!(
        execute("MATCH (n) WITH n.p + 10 AS p, sum(n.q) AS s RETURN p, s ORDER BY p").0,
        vec![
            row(vec![scalar(11), scalar(30)]),
            row(vec![scalar(12), scalar(30)])
        ]
    );
}

#[test]
fn identified_relationships_and_anonymous_patterns_retain_real_match_multiplicity() {
    assert_eq!(
        execute("MATCH (a)-[e:R]->(b) WITH a, count(e) AS c, sum(e.p) AS s RETURN a, c, s").0,
        vec![
            row(vec![GraphValue::Vertex(VId(1)), scalar(2), scalar(10)]),
            row(vec![GraphValue::Vertex(VId(2)), scalar(1), scalar(5)])
        ]
    );
    assert_eq!(
        execute("MATCH () WITH count(*) AS c RETURN c").0,
        vec![row(vec![scalar(3)])]
    );
}

#[test]
fn collection_output_can_be_unwound_and_correlated_to_a_later_graph_source() {
    let (rows, calls) =
        execute("MATCH (n) WITH collect(n) AS xs UNWIND xs AS n MATCH (n)-[:R]->(m) RETURN n");
    assert_eq!(calls, 2);
    assert_eq!(
        rows,
        vec![
            row(vec![GraphValue::Vertex(VId(1))]),
            row(vec![GraphValue::Vertex(VId(1))]),
            row(vec![GraphValue::Vertex(VId(2))])
        ]
    );
}

#[test]
fn leading_unwind_correlations_group_only_the_joined_occurrences() {
    assert_eq!(
        execute(
            "UNWIND [1, 2] AS wanted MATCH (n {p: wanted}) WITH n.p AS p, count(*) AS c RETURN p, c"
        )
        .0,
        vec![
            row(vec![scalar(1), scalar(2)]),
            row(vec![scalar(2), scalar(1)])
        ]
    );
}

#[test]
fn optional_continuations_group_nullable_local_bindings_not_nullable_carried_copies() {
    let (rows, calls) =
        execute("MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH n, count(m) AS c RETURN n, c");
    assert_eq!(calls, 2);
    assert_eq!(
        rows,
        vec![
            row(vec![GraphValue::Vertex(VId(1)), scalar(2)]),
            row(vec![GraphValue::Vertex(VId(2)), scalar(1)]),
            row(vec![GraphValue::Vertex(VId(3)), scalar(0)])
        ]
    );
    let mut calls = 0;
    assert!(
        PreparedGraphSetText::prepare(
            "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH sum(n.p) AS s RETURN s",
            |kind, name| {
                calls += 1;
                symbols(kind, name)
            },
        )
        .is_err()
    );
    assert_eq!(calls, 0);
}

#[test]
fn hidden_property_columns_and_ungrouped_bindings_never_escape_the_boundary() {
    for text in [
        "MATCH (n) WITH count(n.p) AS c RETURN __fg_group_0",
        "MATCH (n) WITH count(n.p) AS c RETURN n",
        "MATCH (n) WITH count(n.p) AS c WITH n.p AS p RETURN p",
    ] {
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
    let query =
        PreparedGraphSetText::prepare("MATCH (n) WITH count(n.p) AS c RETURN *", symbols).unwrap();
    assert_eq!(query.columns(), &["c"]);
}

fn refused_before_catalog(text: &str) {
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

/// fgdb-ezgeq: after `WITH a, count(e) AS c`, the grouping's own scope reads
/// the kept vertex's properties. Vertices 1 and 2 share p = 1, so a law that
/// merged groups by the property value would answer one row, not two.
#[test]
fn a_kept_binding_reads_its_properties_after_the_grouping() {
    let per_vertex = vec![
        row(vec![scalar(1), scalar(10), scalar(2)]),
        row(vec![scalar(1), scalar(20), scalar(1)]),
    ];
    let (rows, calls) = execute("MATCH (a)-[e:R]->(b) WITH a, count(e) AS c RETURN a.p, a.q, c");
    assert_eq!(calls, 1);
    assert_eq!(rows, per_vertex);
    let query = PreparedGraphSetText::prepare(
        "MATCH (a)-[e:R]->(b) WITH a, count(e) AS c RETURN a.p, a.q, c",
        symbols,
    )
    .unwrap();
    assert_eq!(query.columns(), &["p", "q", "c"]);
    // The same answer as re-matching the kept vertex by identity.
    assert_eq!(
        execute("MATCH (a)-[e:R]->(b) WITH a, count(e) AS c MATCH (a) RETURN a.p, a.q, c").0,
        per_vertex
    );
    // Kept last, under an alias, or DISTINCT: the same groups.
    for text in [
        "MATCH (a)-[e:R]->(b) WITH count(e) AS c, a RETURN a.p, a.q, c",
        "MATCH (a)-[e:R]->(b) WITH a AS x, count(e) AS c RETURN x.p, x.q, c",
        "MATCH (a)-[e:R]->(b) WITH DISTINCT a, count(e) AS c RETURN a.p, a.q, c",
    ] {
        assert_eq!(execute(text).0, per_vertex, "{text}");
    }
}

/// The aggregate-RETURN regrouping belongs to the pipeline-aggregate parser;
/// crates/fgdb/tests/with_grouping.rs covers it through the facade.
#[test]
fn grouped_property_reads_serve_the_grouping_where_and_pages() {
    assert_eq!(
        execute("MATCH (a)-[e:R]->(b) WITH a, count(e) AS c WHERE a.q > 10 RETURN a.q, c").0,
        vec![row(vec![scalar(20), scalar(1)])]
    );
    assert_eq!(
        execute(
            "MATCH (a)-[e:R]->(b) WITH a AS x, count(e) AS c ORDER BY x.q DESC LIMIT 1 RETURN x.q, c"
        )
        .0,
        vec![row(vec![scalar(20), scalar(1)])]
    );
}

#[test]
fn only_a_kept_binding_in_the_grouping_scope_reads_through_the_grouped_row() {
    for text in [
        // Aggregated, or only a computed value that reuses the binding's name.
        "MATCH (n) WITH count(n) AS c RETURN n.q",
        "MATCH (n) WITH n.p AS n, count(*) AS c RETURN n.q",
        // A later stage sees the grouped row, not the graph.
        "MATCH (n) WITH n, count(*) AS c WITH n, c RETURN n.q",
        // The hidden key never resolves by its private name.
        "MATCH (n) WITH n, count(*) AS c WHERE n.q > 0 RETURN __fg_group_0",
    ] {
        refused_before_catalog(text);
    }
    let query = PreparedGraphSetText::prepare(
        "MATCH (n) WITH n, count(*) AS c WHERE n.q > 0 RETURN *",
        symbols,
    )
    .unwrap();
    assert_eq!(query.columns(), &["n", "c"]);
}

#[test]
fn direct_grouping_failure_is_not_suppressed_by_limit_zero() {
    let query =
        PreparedGraphSetText::prepare("MATCH (n) WITH sum(1 / n.p) AS s RETURN s LIMIT 0", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
    let zero = CanonicalScalar::Int(0);
    let result = query.execute_governed(
        GqlQueryPolicy::new(10, 0, 100_000, 100_000),
        |pattern, policy| {
            pattern.plan().execute_governed_with_properties(
                1,
                [VId(1)],
                [],
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(Some(&zero)),
                policy,
                || Ok::<_, &'static str>(()),
            )
        },
        || Ok::<_, &'static str>(()),
    );
    assert!(matches!(result, Err(GqlQueryError::Source(_))));
}
