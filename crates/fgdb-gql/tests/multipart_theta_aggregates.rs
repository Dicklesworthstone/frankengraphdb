//! Aggregate terminals share multipart theta matching and null extension.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphPipelineAggregateText,
};
use fgdb_types::{CanonicalScalar, VId};

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const EDGES: [(VId, RelationId, VId); 5] = [
    (VId(1), R, VId(2)),
    (VId(1), R, VId(2)),
    (VId(1), R, VId(3)),
    (VId(2), R, VId(3)),
    (VId(3), R, VId(4)),
];
const SCORES: [i64; 4] = [1, 2, 2, 3];

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn execute(text: &str) -> Vec<GraphAggregateRow> {
    let query = PreparedGraphPipelineAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap();
    let values =
        SCORES.map(|score| fgdb_types::CanonicalPropertyValue::from(CanonicalScalar::Int(score)));
    query
        .execute_governed(
            GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    (values.len() + EDGES.len()) as u64,
                    (1..=4).map(VId),
                    EDGES,
                    |vid, tests| {
                        Ok::<_, &'static str>(tests.iter().all(|test| {
                            test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                        }))
                    },
                    |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value
}

#[test]
fn implicit_aggregate_return_counts_only_inequality_witnesses() {
    let rows = execute(
        "MATCH (n) WITH n MATCH (m) WHERE n <> m \
         RETURN n,COUNT(*) AS matches,COUNT(DISTINCT m) AS distincts ORDER BY n",
    );
    assert_eq!(rows.len(), 4);
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.keys(), &[GraphValue::Vertex(VId(index as u128 + 1))]);
        assert_eq!(row.values()[0].as_count(), Some(3));
        assert_eq!(row.values()[1].as_count(), Some(3));
    }
}

#[test]
fn optional_theta_count_star_includes_null_extension_but_count_value_does_not() {
    let rows = execute(
        "MATCH (n) WITH n,n.p AS threshold OPTIONAL MATCH (m) \
         WHERE m.p > threshold RETURN n,COUNT(*) AS rows,COUNT(m) AS matches ORDER BY n",
    );
    assert_eq!(rows.len(), SCORES.len());
    for (index, row) in rows.iter().enumerate() {
        let matches = SCORES
            .iter()
            .filter(|value| **value > SCORES[index])
            .count() as u64;
        assert_eq!(row.keys(), &[GraphValue::Vertex(VId(index as u128 + 1))]);
        assert_eq!(row.values()[0].as_count(), Some(matches.max(1)));
        assert_eq!(row.values()[1].as_count(), Some(matches));
    }
}

#[test]
fn aggregate_template_cannot_alias_different_theta_comparisons() {
    let prepare_template = |operator: &str| {
        PreparedGraphPipelineAggregateText::prepare(
            &format!(
                "MATCH (n) WITH n,n.p AS threshold OPTIONAL MATCH (m) \
                 WHERE m.p {operator} threshold RETURN n,COUNT(*) AS rows,COUNT(m) AS matches"
            ),
            symbols,
        )
        .unwrap()
    };
    let strict = prepare_template(">");
    let inclusive = prepare_template(">=");
    assert_ne!(
        strict.canonical_template_bytes(),
        inclusive.canonical_template_bytes()
    );
    assert_ne!(
        strict
            .bind_relation_parameters(&GqlParameters::new())
            .unwrap()
            .input()
            .canonical_bytes(),
        inclusive
            .bind_relation_parameters(&GqlParameters::new())
            .unwrap()
            .input()
            .canonical_bytes(),
    );
}
