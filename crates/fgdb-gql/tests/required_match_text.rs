//! Native repeated MATCH uses ordered GLA joins, not a concatenated pattern.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValueRow,
    GraphWalkSearch, IntegerComparison, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphWriteStatement, PreparedGraphAggregateText, PreparedGraphText, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const P: PropertyKeyId = PropertyKeyId(1);
const L: LabelId = LabelId(1);
type Edge = (VId, RelationId, VId);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(T)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}
fn builder(names: &[&str]) -> GraphPatternBuilder {
    let mut value = GraphPatternBuilder::new();
    for name in names {
        value.vertex(name).unwrap();
    }
    value
}
fn integer(key: PropertyKeyId, comparison: IntegerComparison, value: i64) -> VertexPredicate {
    VertexPredicate::IntegerProperty {
        key,
        comparison,
        value,
    }
}
fn evaluate(text: &str, vertices: &[VId], edges: &[Edge]) -> Vec<GraphValueRow> {
    let pattern = PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let values = vertices
        .iter()
        .map(|vid| (*vid, CanonicalScalar::Int(vid.0 as i64)))
        .collect::<BTreeMap<_, _>>();
    pattern
        .plan()
        .execute_governed_with_properties(
            (vertices.len() + edges.len()) as u64,
            vertices.iter().copied(),
            edges.iter().copied(),
            |vid, predicates| {
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|test| test.matches(&[L], &[(P, values[&vid].clone())])),
                )
            },
            |vid, key| Ok(if key == P { values.get(&vid) } else { None }),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

#[test]
fn repeated_match_binds_to_ordered_typed_clauses_and_resolves_each_symbol_once() {
    let text = "MATCH (a:L) WHERE a.p=$lo OPTIONAL MATCH (a)-[:R]->(b) WHERE b.p >= $cut MATCH (b)<-[:S]-(c) WHERE c.p <= $hi RETURN ALL a,b,c SKIP $off LIMIT $n";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
        assert!(calls.insert((kind, name.to_owned())));
        symbols(kind, name)
    })
    .unwrap();
    let arguments = GqlParameters::new()
        .with_int64("lo", 1)
        .unwrap()
        .with_int64("cut", 2)
        .unwrap()
        .with_int64("hi", 9)
        .unwrap()
        .with_uint64("off", 1)
        .unwrap()
        .with_uint64("n", 3)
        .unwrap();
    let actual = template.bind_parameters(&arguments).unwrap();
    let mut root = builder(&["a"]);
    root.filter("a", VertexPredicate::HasLabel(L)).unwrap();
    root.filter("a", integer(P, IntegerComparison::Equal, 1))
        .unwrap();
    let mut optional = builder(&["a", "b"]);
    optional.edge("a", R, GlaDirection::Forward, "b").unwrap();
    optional
        .filter("b", integer(P, IntegerComparison::GreaterOrEqual, 2))
        .unwrap();
    let mut required = builder(&["b", "c"]);
    required.edge("b", S, GlaDirection::Reverse, "c").unwrap();
    required
        .filter("c", integer(P, IntegerComparison::LessOrEqual, 9))
        .unwrap();
    let expected = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::optional(&optional),
                GraphMatchClause::required(&required),
            ],
            &[
                GraphColumn::vertex("a", "a"),
                GraphColumn::vertex("b", "b"),
                GraphColumn::vertex("c", "c"),
            ],
            1,
            Some(3),
        )
        .unwrap()
        .with_duplicates();
    assert_eq!(actual, expected);
    assert_eq!(template.bind_parameters(&arguments).unwrap(), actual);
    assert_eq!(calls.len(), 4);
    assert_eq!(template.parameter_schema().len(), 5);
    assert_eq!(template.statement(), text);
}

#[test]
fn required_match_after_optional_multiplies_real_rows_and_eliminates_absence() {
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), S, VId(3)),
        (VId(2), S, VId(3)),
    ];
    let head = "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c)";
    for (tail, expected) in [
        ("c", 4),
        ("ALL c SKIP 1 LIMIT 2", 2),
        ("DISTINCT c", 1),
        ("c LIMIT 0", 0),
    ] {
        let rows = evaluate(&format!("{head} RETURN {tail}"), &vertices, &edges);
        assert_eq!(rows.len(), expected);
        assert!(
            rows.iter()
                .all(|row| row.values()[0].as_vertex() == Some(VId(3)))
        );
    }
    let absent = evaluate(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b) WHERE b.p=999 RETURN a",
        &vertices,
        &edges,
    );
    assert!(
        absent.is_empty(),
        "rejected OPTIONAL witnesses must not invent replacement null rows"
    );
    let independent = evaluate(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (x) RETURN b,x",
        &vertices,
        &edges,
    );
    assert_eq!(independent.len(), 20);
    assert_eq!(
        independent
            .iter()
            .filter(|row| row.values()[0].is_null())
            .count(),
        12
    );
    assert_eq!(
        evaluate(
            "MATCH (a) MATCH (a) WHERE a.p=1 OPTIONAL MATCH (a)-[:T]->(x) RETURN a,x",
            &vertices,
            &edges
        )
        .len(),
        1
    );
}

#[test]
fn each_required_match_keeps_its_own_shortest_or_walk_selector() {
    let text = "MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) MATCH ANY SHORTEST WALK (b)-[:S*0..2]->(c) OPTIONAL MATCH WALK (c)-[:T*0]->(d) RETURN a,b,c,d";
    let pattern = PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let modes = pattern
        .plan()
        .operators()
        .iter()
        .filter_map(|op| match op {
            GlaOperator::VarLengthExpand { search, .. } => Some(*search),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        modes,
        vec![
            GraphWalkSearch::AllShortest,
            GraphWalkSearch::AnyShortest,
            GraphWalkSearch::All
        ]
    );
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), S, VId(1)),
        (VId(1), S, VId(1)),
        (VId(1), S, VId(1)),
    ];
    assert_eq!(evaluate(text, &[VId(1), VId(2)], &edges).len(), 2);
    let null = evaluate(
        "MATCH (a) OPTIONAL MATCH (a)-[:T]->(b) MATCH ANY SHORTEST WALK (b)-[:R*0]->(c) RETURN a,c",
        &[VId(1), VId(2)],
        &edges,
    );
    assert!(null.is_empty());
}

#[test]
fn aggregation_consumes_the_completed_join_bag_not_optional_prefix_rows() {
    let query = PreparedGraphAggregateText::prepare(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) RETURN a,COUNT(*) AS paths,COUNT(c) AS matched GROUP BY a HAVING paths >= 4 ORDER BY a",
        symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let rows = query
        .execute_governed(
            8,
            [VId(1), VId(2), VId(3), VId(4)],
            [
                (VId(1), R, VId(2)),
                (VId(1), R, VId(2)),
                (VId(2), S, VId(3)),
                (VId(2), S, VId(3)),
            ],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok::<Option<&CanonicalScalar>, ()>(None),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].keys()[0].as_vertex(), Some(VId(1)));
    assert_eq!(rows[0].values()[0].as_count(), Some(4));
    assert_eq!(rows[0].values()[1].as_count(), Some(4));
}

#[test]
fn malformed_late_clauses_and_scope_overflow_refuse_before_catalog_calls() {
    let too_many = format!("MATCH (a){} RETURN a LIMIT 0", " MATCH (a)".repeat(65));
    for text in [
        "MATCH (a) MATCH RETURN a",
        "MATCH (a) MATCH ( RETURN a",
        "MATCH (a) MATCH (b) WHERE missing.p=1 RETURN b",
        "MATCH (a) MATCH (b) WHERE c.p=1 MATCH (c) RETURN c",
        "MATCH (a) MATCH (b) WHERE EXISTS { MATCH (b) } RETURN b",
        "MATCH (a) WHERE EXISTS { MATCH (a) MATCH (a) } RETURN a",
        "MATCH (a) OPTIONAL MATCH (a) MATCH (a)-[:R*]->(b) RETURN b",
        "MATCH (a) MATCH ANY SHORTEST WALK (a)-[:R*1..2]->(b)-[:S]->(c) RETURN c LIMIT 0",
        &too_many,
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "failed syntax reached the catalog: {text}");
    }
    let maximal = format!("MATCH (a){} RETURN a", " MATCH (a)".repeat(64));
    assert_eq!(evaluate(&maximal, &[VId(1)], &[]).len(), 1);
    // Bounded var-length in a late plain MATCH is valid GQL since bounded
    // plain MATCH is a WALK: optional re-bind of (a), then one finite
    // 1..2 occurrence (1->2; length 2 dies at 2), absence eliminated.
    let optional_var = evaluate(
        "MATCH (a) OPTIONAL MATCH (a) MATCH (a)-[:R*1..2]->(b) RETURN b",
        &[VId(1), VId(2)],
        &[(VId(1), R, VId(2))],
    );
    assert_eq!(optional_var.len(), 1);
    assert_eq!(optional_var[0].values()[0].as_vertex(), Some(VId(2)));
    let independent = evaluate("MATCH (a) MATCH () RETURN a", &[VId(1)], &[]);
    assert_eq!(independent.len(), 1);
    assert_eq!(independent[0].values()[0].as_vertex(), Some(VId(1)));
}

#[test]
fn original_utf8_parameter_offsets_and_existential_privacy_survive_later_matches() {
    let text = "\u{2003}MATCH (a) MATCH (a)-[:R]->(b) WHERE b.p=$cut RETURN b LIMIT $take";
    let template = PreparedGraphText::prepare(text, symbols).unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find('$').unwrap());
    assert_eq!(missing.kind, GraphPatternTextErrorKind::MissingParameter);
    let wrong = GqlParameters::new()
        .with_uint64("cut", 1)
        .unwrap()
        .with_uint64("take", 2)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
    ));
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphText::prepare(&text[..at], symbols);
    }
    let hidden = "MATCH (a) WHERE EXISTS { MATCH (a)-[:R]->(hidden) } MATCH (x) RETURN hidden";
    assert_eq!(
        PreparedGraphText::prepare(hidden, symbols)
            .unwrap_err()
            .kind,
        GraphPatternTextErrorKind::UnknownVariable
    );
    assert!(!format!("{template:?}").contains("$cut"));
}

#[test]
fn joined_write_scripts_and_parameter_batches_share_the_same_selection_compiler() {
    let script = PreparedGraphWriteScript::prepare(
        "CREATE (n {p:$start}); MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) MATCH (b)-[:S]->(c) SET c.p=$value; MATCH (a) MATCH (a)-[:R]->(b) DELETE b",
        R, symbols,
    ).unwrap();
    let args = GqlParameters::new()
        .with_int64("start", 1)
        .unwrap()
        .with_int64("value", 8)
        .unwrap();
    let bound = script.bind_parameters(&args).unwrap();
    let [
        GraphWriteStatement::Insert(_),
        GraphWriteStatement::Mutation(update),
        GraphWriteStatement::Delete(delete),
    ] = bound.statements()
    else {
        panic!("wrong joined write dispatch")
    };
    let ops = update.selection().plan().operators();
    let boundary = ops
        .iter()
        .position(|op| matches!(op, GlaOperator::OptionalEnd { .. }))
        .unwrap();
    assert!(matches!(
        ops.get(boundary + 1),
        Some(GlaOperator::BindVertex { .. })
    ));
    assert!(
        delete
            .selection()
            .plan()
            .operators()
            .iter()
            .any(|op| matches!(op, GlaOperator::BindVertex { .. }))
    );
    let batch = script.bind_parameter_sets(&[args.clone(), args]).unwrap();
    assert_eq!(batch.program().statements().len(), 6);
    assert_eq!(batch.location(4).unwrap().statement, 1);
}
