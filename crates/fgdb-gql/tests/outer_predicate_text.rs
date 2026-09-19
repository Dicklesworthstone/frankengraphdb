//! Native predicate-only correlations share typed capture, source and scope semantics.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaOperator, GraphColumn, GraphMatchClause, GraphPatternBuilder, GraphValueRow,
    IntegerComparison, PatternBuildError, PatternLimitDimension, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphPatternTextErrorKind, GraphSymbol,
    GraphSymbolKind, GraphWriteStatement, PreparedGraphAggregateText, PreparedGraphText,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const VERTICES: [VId; 4] = [VId(1), VId(2), VId(3), VId(4)];
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 5_000_000, 5_000_000)
}
fn values() -> BTreeMap<VId, CanonicalScalar> {
    BTreeMap::from([
        (VId(1), CanonicalScalar::Int(7)),
        (VId(2), CanonicalScalar::Int(7)),
        (VId(3), CanonicalScalar::Int(9)),
        (VId(4), CanonicalScalar::Null),
    ])
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(text: &str, edges: &[(VId, RelationId, VId)]) -> Vec<GraphValueRow> {
    let pattern = prepare(text);
    let properties = values();
    pattern
        .plan()
        .execute_governed_with_properties(
            (VERTICES.len() + edges.len()) as u64,
            VERTICES,
            edges.iter().copied(),
            |vid, predicates| {
                Ok::<_, ()>(
                    predicates
                        .iter()
                        .all(|predicate| predicate.matches(&[], &[(P, properties[&vid].clone())])),
                )
            },
            |vid, key| Ok(if key == P { properties.get(&vid) } else { None }),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}
fn captures(pattern: &PreparedGraphPattern<GraphValueRow>) -> usize {
    pattern
        .plan()
        .operators()
        .iter()
        .filter(|operator| matches!(operator, GlaOperator::BindOuterVertex { .. }))
        .count()
}

#[test]
fn native_outer_property_join_matches_typed_capture_and_resolves_symbols_once() {
    let text = "MATCH (a) MATCH (b) WHERE b.p=a.p RETURN ALL a,b SKIP $off LIMIT $take";
    let mut names = BTreeSet::new();
    let template = PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
        assert!(names.insert((kind, name.to_owned())));
        symbols(kind, name)
    })
    .unwrap();
    let arguments = GqlParameters::new()
        .with_uint64("off", 1)
        .unwrap()
        .with_uint64("take", 3)
        .unwrap();
    let actual = template.bind_parameters(&arguments).unwrap();
    let mut root = GraphPatternBuilder::new();
    root.vertex("a").unwrap();
    let mut child = GraphPatternBuilder::new();
    child.vertex("b").unwrap().outer_vertex("a").unwrap();
    child
        .compare_properties("b", P, IntegerComparison::Equal, "a", P)
        .unwrap();
    let expected = root
        .prepare_values_with_clauses(
            &[GraphMatchClause::required(&child)],
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")],
            1,
            Some(3),
        )
        .unwrap()
        .with_duplicates();
    assert_eq!(actual, expected);
    assert_eq!(template.bind_parameters(&arguments).unwrap(), actual);
    assert_eq!(captures(&actual), 1);
    assert_eq!(names.len(), 1);
    assert_eq!(template.statement(), text);
    assert_eq!(
        run("MATCH (a) MATCH (b) WHERE b.p=a.p RETURN a,b", &[]).len(),
        5
    );
}

#[test]
fn captured_nullable_values_do_not_become_positive_matches_or_remove_outer_duplicates() {
    let edges = [(VId(1), R, VId(2)), (VId(1), R, VId(2))];
    let head = "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b)";
    let captured = format!("{head} MATCH (c) WHERE b IS NULL OR c.p=b.p RETURN a,b,c");
    let rows = run(&captured, &edges);
    assert_eq!(rows.len(), 16);
    assert_eq!(
        rows.iter().filter(|row| row.values()[1].is_null()).count(),
        12
    );
    assert_eq!(captures(&prepare(&captured)), 1);
    let matched = format!("{head} MATCH (b),(c) WHERE b IS NULL OR c.p=b.p RETURN a,b,c");
    assert_eq!(run(&matched, &edges).len(), 4);
    assert_eq!(captures(&prepare(&matched)), 0);
    assert_eq!(
        run(
            &format!("{head} MATCH (c) WHERE b.p IS NULL RETURN a,c"),
            &edges
        )
        .len(),
        12
    );
    assert_eq!(
        run(
            &format!("{head} MATCH (c) WHERE b.p IS NOT NULL RETURN a,c"),
            &edges
        )
        .len(),
        8
    );
    // A following capture sees the original outer binding, never an optional
    // child's private null-extension copy of that captured value.
    let twice = "MATCH (a) OPTIONAL MATCH (x) WHERE a.p=999 MATCH (c) WHERE c.p=a.p RETURN a,x,c";
    let rows = run(twice, &[]);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row.values()[1].is_null()));
}

#[test]
fn every_native_predicate_form_captures_only_referenced_outer_operands() {
    for (predicate, expected) in [
        ("a.p=7", 8),
        ("a.p='not an integer'", 0),
        ("a.p IS NULL", 4),
        ("a IS NULL", 0),
        ("b.p=a.p", 5),
        ("b=a", 4),
        ("a.p IN [b.p,NULL]", 5),
        ("a.p BETWEEN b.p AND b.p", 5),
        ("NOT (a IS NULL) AND (b.p=a.p OR b=a)", 6),
    ] {
        let text = format!("MATCH (unused),(a) MATCH (b) WHERE {predicate} RETURN a,b");
        assert_eq!(
            captures(&prepare(&text)),
            1,
            "unused outer variable was captured: {predicate}"
        );
        // The independent unused root variable contributes four occurrences.
        assert_eq!(run(&text, &[]).len(), expected * 4, "{predicate}");
    }
}

#[test]
fn existential_outer_only_correlations_keep_local_names_private_and_do_not_multiply() {
    let rows = run(
        "MATCH (a) WHERE EXISTS { MATCH (b) WHERE b=a } RETURN a",
        &[],
    );
    assert_eq!(rows.len(), 4);
    let rows = run(
        "MATCH (a) WHERE NOT EXISTS { MATCH (b) WHERE b.p=a.p } RETURN a",
        &[],
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_vertex(), Some(VId(4)));
    let joined = "MATCH (a) MATCH (b) WHERE b.p=a.p OPTIONAL MATCH (c) WHERE c.p=b.p RETURN a,b,c";
    assert_eq!(run(joined, &[]).len(), 9);
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (hidden) WHERE hidden.p=a.p } MATCH (b) WHERE hidden.p=b.p RETURN b",
        "MATCH (a) MATCH (b) WHERE later.p=b.p MATCH (later) RETURN later",
        "MATCH (a) MATCH (b) WHERE missing IS NULL RETURN b",
        "MATCH (a) WHERE EXISTS { MATCH (b) WHERE a.p=b.p } RETURN b",
        "MATCH (a) OPTIONAL MATCH (b) WHERE EXISTS { MATCH (c) WHERE c.p=a.p } RETURN b",
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
        assert_eq!(calls, 0, "invalid scope reached catalog: {text}");
    }
}

#[test]
fn capture_admission_counts_used_names_not_the_temporary_visible_symbol_table() {
    let nodes = (0..65)
        .map(|at| format!("(v{at})"))
        .collect::<Vec<_>>()
        .join(",");
    let unused = format!("MATCH (a) WHERE EXISTS {{ MATCH {nodes} }} RETURN a LIMIT 0");
    assert_eq!(captures(&prepare(&unused)), 0);
    let overflow =
        format!("MATCH (a) WHERE EXISTS {{ MATCH {nodes} WHERE a.p=1 }} RETURN a LIMIT 0");
    let mut calls = 0;
    let error = PreparedGraphText::prepare(&overflow, |kind: GraphSymbolKind, name: &str| {
        calls += 1;
        symbols(kind, name)
    })
    .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
            dimension: PatternLimitDimension::Vertices,
            limit: 65,
            observed: 66,
        })
    ));
    assert_eq!(error.offset, overflow.find("a.p").unwrap());
    assert_eq!(calls, 0);
    // Merely reading an outer value must not add another positive graph atom
    // to a selected one-atom shortest scope or change its search selector.
    let shortest =
        prepare("MATCH (a) MATCH ANY SHORTEST WALK (b)-[:R*0..3]->(c) WHERE c.p=a.p RETURN a,c");
    assert_eq!(captures(&shortest), 1);
}

#[test]
fn original_parameter_offsets_rebinding_and_eager_source_failures_survive_captures() {
    let text = "\u{2003}MATCH (a) MATCH (b) WHERE a.p=$key AND b.p=a.p RETURN a,b LIMIT $take";
    let template = PreparedGraphText::prepare(text, symbols).unwrap();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find('$').unwrap());
    assert_eq!(missing.kind, GraphPatternTextErrorKind::MissingParameter);
    let args = GqlParameters::new()
        .with_int64("key", 7)
        .unwrap()
        .with_uint64("take", 2)
        .unwrap();
    let first = template.bind_parameters(&args).unwrap();
    let frozen = first.canonical_bytes();
    let changed = GqlParameters::new()
        .with_int64("key", 9)
        .unwrap()
        .with_uint64("take", 1)
        .unwrap();
    assert_ne!(
        frozen,
        template
            .bind_parameters(&changed)
            .unwrap()
            .canonical_bytes()
    );
    assert_eq!(first.canonical_bytes(), frozen);
    for at in (0..=text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphText::prepare(&text[..at], symbols);
    }
    assert!(!format!("{template:?}").contains("$key"));
    for head in [
        "MATCH (b) WHERE TRUE OR a.p IS NULL",
        "OPTIONAL MATCH (b) WHERE TRUE OR a.p IS NULL",
    ] {
        let pattern = prepare(&format!("MATCH (a) {head} RETURN a LIMIT 0"));
        let result = pattern.plan().execute_governed_with_properties(
            4,
            VERTICES,
            [],
            |_, _| Ok::<_, &str>(true),
            |_, _| Err("unreadable outer operand"),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("unreadable outer operand"))
        ));
    }
}

#[test]
fn aggregate_input_and_write_script_batches_reuse_the_same_outer_binding_path() {
    let aggregate = PreparedGraphAggregateText::prepare(
        "MATCH (a) OPTIONAL MATCH (b) WHERE b.p=a.p RETURN a,COUNT(*) AS rows,COUNT(b) AS matches GROUP BY a ORDER BY a",
        symbols,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let properties = values();
    let rows = aggregate
        .execute_governed(
            4,
            VERTICES,
            [],
            |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(properties.get(&vid)),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value;
    assert_eq!(
        rows.iter()
            .map(|row| (
                row.keys()[0].as_vertex().unwrap(),
                row.values()[0].as_count().unwrap(),
                row.values()[1].as_count().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            (VId(1), 2, 2),
            (VId(2), 2, 2),
            (VId(3), 1, 1),
            (VId(4), 1, 0)
        ]
    );
    let script = PreparedGraphWriteScript::prepare(
        "MATCH (a) MATCH (b) WHERE b.p=a.p SET b.q=$value; MATCH (a) MATCH (b) WHERE a.p=b.p DELETE b",
        R, symbols,
    ).unwrap();
    let args = GqlParameters::new().with_int64("value", 42).unwrap();
    let program = script.bind_parameters(&args).unwrap();
    let [
        GraphWriteStatement::Mutation(update),
        GraphWriteStatement::Delete(delete),
    ] = program.statements()
    else {
        panic!("wrong shared write dispatch")
    };
    assert_eq!(captures(update.selection()), 1);
    assert_eq!(captures(delete.selection()), 1);
    let batch = script.bind_parameter_sets(&[args.clone(), args]).unwrap();
    assert_eq!(batch.program().statements().len(), 4);
    assert_eq!(batch.location(3).unwrap().argument_set, 1);
}
