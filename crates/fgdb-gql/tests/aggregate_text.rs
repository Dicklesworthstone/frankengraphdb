//! The aggregate text profile shares lexical, binding and execution machinery.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GlaOperator, GraphColumn, GraphPatternBuilder, GraphValue, IntegerComparison, VertexPredicate};
use fgdb_gql::{GraphAggregate, GraphAggregateError, GraphAggregateTextSlot, GraphPatternTextErrorKind,
    GraphSymbol, GraphSymbolKind, GqlParameters, GqlQueryError, GqlQueryPolicy,
    PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000) }

#[test]
fn mixed_return_order_group_order_and_shared_arguments_match_typed_preparation() {
    let text = "MATCH (a:L)-[:R]->(b)-[:S]->(c), (c)-[:R]->(a) \
        WHERE b.n >= $minimum AND a <> c \
        RETURN SUM(c.n) AS total,a AS owner,COUNT(*) AS paths,COUNT(DISTINCT b) AS vias \
        GROUP BY a SKIP $page LIMIT $page";
    let mut resolutions = BTreeMap::new();
    let template = PreparedGraphAggregateText::prepare(text, |kind, name| {
        *resolutions.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    }).unwrap();
    assert!(resolutions.values().all(|count| *count == 1));
    assert_eq!(template.statement(), text);
    assert_eq!(template.columns(), &["total", "owner", "paths", "vias"]);
    assert_eq!(template.output_slots(), &[GraphAggregateTextSlot::Aggregate(0), GraphAggregateTextSlot::GroupKey(0),
        GraphAggregateTextSlot::Aggregate(1), GraphAggregateTextSlot::Aggregate(2)]);
    assert_eq!(template.parameter_schema()[1].occurrences, 2);
    assert!(!template.parameter_schema()[1].requires_positive);
    let args = GqlParameters::new().with_int64("minimum", 7).unwrap().with_uint64("page", 1).unwrap();
    let actual = template.bind_parameters(&args).unwrap();
    let mut b = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] { b.vertex(name).unwrap(); }
    b.filter("a", VertexPredicate::HasLabel(LabelId(1))).unwrap();
    b.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    b.edge("b", RelationId(2), GlaDirection::Forward, "c").unwrap();
    b.edge("c", RelationId(1), GlaDirection::Forward, "a").unwrap();
    b.identity("a", "c", false).unwrap();
    b.filter("b", VertexPredicate::IntegerProperty { key: PropertyKeyId(1), comparison: IntegerComparison::GreaterOrEqual, value: 7 }).unwrap();
    let input = b.prepare_values(&[GraphColumn::vertex("owner", "a"), GraphColumn::property("total", "c", PropertyKeyId(1)),
        GraphColumn::vertex("vias", "b")], 0, None).unwrap().with_duplicates();
    let expected = PreparedGraphAggregate::prepare(input, &[0], &[GraphAggregate::sum_int("total", 1),
        GraphAggregate::count_rows("paths"), GraphAggregate::count_distinct("vias", 2)], 1, Some(1)).unwrap();
    assert_eq!(actual, expected);
    assert!(matches!(actual.input_pattern().plan().operators().last(), Some(GlaOperator::Limit { offset: 0, count: None })));
    let frozen = actual.canonical_bytes();
    let changed = GqlParameters::new().with_int64("minimum", 8).unwrap().with_uint64("page", 0).unwrap();
    assert_ne!(frozen, template.bind_parameters(&changed).unwrap().canonical_bytes());
    assert_eq!(actual.canonical_bytes(), frozen);
    assert!(resolutions.values().all(|count| *count == 1));
    assert!(!format!("{template:?}").contains("minimum"));
}

#[test]
fn grouping_argument_reuse_and_count_only_do_not_create_accidental_distinct_children() {
    let t = PreparedGraphAggregateText::prepare(
        "MATCH (n) RETURN SUM(n.n) AS total,n.n AS key,n.n AS repeated,COUNT(*) AS rows GROUP BY n.n", symbols).unwrap();
    let aggregate = t.bind_parameters(&GqlParameters::new()).unwrap();
    assert_eq!(aggregate.input_pattern().columns(), &["key"]);
    assert_eq!(t.output_slots(), &[GraphAggregateTextSlot::Aggregate(0), GraphAggregateTextSlot::GroupKey(0),
        GraphAggregateTextSlot::GroupKey(0), GraphAggregateTextSlot::Aggregate(1)]);
    let count = prepare("MATCH (n) RETURN COUNT(*)");
    assert_eq!(count.input_pattern().columns().len(), 1);
    let empty = count.execute_governed(0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), policy(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(empty.value.len(), 1);
    assert_eq!(empty.value[0].get(0).unwrap().as_count(), Some(0));
    let distinct = prepare("MATCH (n) RETURN DISTINCT COUNT(*)");
    assert_eq!(distinct.canonical_bytes(), count.canonical_bytes());
    let grouped = prepare("MATCH (n) RETURN n,COUNT(*) GROUP BY n");
    assert!(grouped.execute_governed(0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), policy(), || Ok::<_, ()>(())).unwrap().value.is_empty());
}

#[test]
fn summaries_match_independent_oriented_edge_enumeration_and_paginate_groups() {
    let universe = [(VId(1), RelationId(1), VId(1)), (VId(1), RelationId(1), VId(2)),
        (VId(2), RelationId(1), VId(1)), (VId(2), RelationId(1), VId(2))];
    let high = CanonicalScalar::Int(i64::MAX);
    for mask in 0..16 {
        let mut edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0).map(|(_, edge)| *edge).collect();
        if let Some(edge) = edges.first().copied() { edges.push(edge); }
        for (direction, arrow) in [(0, "-[:R]->"), (1, "<-[:R]-"), (2, "-[:R]-")] {
            let text = format!("MATCH (a){arrow}(b) RETURN a,COUNT(*) AS paths,COUNT(b.n) AS present,COUNT(DISTINCT b.n) AS different,SUM_INT(b.n) AS total GROUP BY a");
            let aggregate = prepare(&text);
            let result = aggregate.execute_governed(edges.len() as u64, [], edges.iter().copied(),
                |_, _| Ok::<_, ()>(true), |vid, _| Ok((vid == VId(2)).then_some(&high)), policy(), || Ok::<_, ()>(())).unwrap();
            let mut expected: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
            for &(src, _, dst) in &edges {
                let pairs = if direction == 1 { vec![(dst, src)] }
                    else if direction == 2 && src != dst { vec![(src, dst), (dst, src)] } else { vec![(src, dst)] };
                for (a, b) in pairs { expected.entry(a).or_default().push((b == VId(2)).then_some(i64::MAX)); }
            }
            assert_eq!(result.value.len(), expected.len());
            for (row, (owner, inputs)) in result.value.iter().zip(expected) {
                let nonnull: Vec<_> = inputs.iter().flatten().copied().collect();
                assert_eq!(row.keys(), &[GraphValue::Vertex(owner)]);
                assert_eq!(row.get(0).unwrap().as_count(), Some(inputs.len() as u64));
                assert_eq!(row.get(1).unwrap().as_count(), Some(nonnull.len() as u64));
                assert_eq!(row.get(2).unwrap().as_count(), Some(nonnull.iter().collect::<BTreeSet<_>>().len() as u64));
                assert_eq!(row.get(3).unwrap().as_integer(), (!nonnull.is_empty()).then(|| nonnull.iter().map(|x| i128::from(*x)).sum()));
            }
            let paged = prepare(&format!("{text} SKIP 1 LIMIT 1"));
            let page = paged.execute_governed(edges.len() as u64, [], edges.iter().copied(),
                |_, _| Ok::<_, ()>(true), |vid, _| Ok((vid == VId(2)).then_some(&high)), policy(), || Ok::<_, ()>(())).unwrap();
            assert_eq!(page.value, result.value.into_iter().skip(1).take(1).collect::<Vec<_>>());
        }
    }
}

#[test]
fn malformed_ungrouped_and_unsupported_aggregates_never_resolve_names() {
    for text in ["MATCH (a) RETURN a", "MATCH (a) RETURN a,COUNT(*)", "MATCH (a) RETURN COUNT(*) GROUP BY a",
        "MATCH (a) RETURN a,COUNT(*) GROUP BY a,a", "MATCH (a) RETURN a AS x,COUNT(*) GROUP BY x",
        "MATCH (a) RETURN COUNT(DISTINCT *)", "MATCH (a) RETURN SUM(*)", "MATCH (a) RETURN AVG(a.n)",
        "MATCH (a) RETURN MIN(DISTINCT a.n)", "MATCH (a) RETURN COUNT(SUM(a.n))", "MATCH (a) RETURN COUNT(a,b)",
        "MATCH (a) RETURN a.n AS key,COUNT(*) GROUP BY a", "MATCH (a) RETURN COUNT(*) AS x,SUM(a.n) AS x",
        "MATCH (a) RETURN COUNT(*) LIMIT -1", "MATCH (a) RETURN COUNT(*) GROUP BY a;",
        "MATCH (a) RETURN COUNT(*) HAVING count > 1", "MATCH (a) RETURN COUNT(*) ORDER BY count",
        "MATCH (a) WHERE a.n >= $p RETURN COUNT(*) LIMIT $p"] {
        let mut calls = 0;
        assert!(PreparedGraphAggregateText::prepare(text, |kind, name| { calls += 1; symbols(kind, name) }).is_err(), "{text}");
        assert_eq!(calls, 0, "catalog called for refused syntax: {text}");
    }
    assert!(PreparedGraphText::prepare("MATCH (a) RETURN COUNT(*)", symbols).is_err(), "ordinary profile must not silently widen");
    let source = "\u{2003}MATCH (a:L) RETURN COUNT(a.n) AS c GROUP BY a";
    for at in (0..=source.len()).filter(|at| source.is_char_boundary(*at)) {
        let _ = PreparedGraphAggregateText::prepare(&source[..at], symbols);
    }
}

#[test]
fn shared_argument_errors_and_definition_caps_keep_their_domains() {
    let t = PreparedGraphAggregateText::prepare("MATCH (a:L) WHERE a.n >= $n RETURN COUNT(*) SKIP $page LIMIT $page", symbols).unwrap();
    assert_eq!(t.bind_parameters(&GqlParameters::new()).unwrap_err().kind, GraphPatternTextErrorKind::MissingParameter);
    let wrong = GqlParameters::new().with_uint64("n", 0).unwrap().with_uint64("page", 0).unwrap();
    assert!(matches!(t.bind_parameters(&wrong).unwrap_err().kind, GraphPatternTextErrorKind::ParameterTypeMismatch { .. }));
    let valid = GqlParameters::new().with_int64("n", i64::MIN).unwrap().with_uint64("page", u64::MAX).unwrap();
    t.bind_parameters(&valid).unwrap();
    assert_eq!(t.bind_parameters(&valid.with_int64("extra", 1).unwrap()).unwrap_err().kind, GraphPatternTextErrorKind::UnexpectedArguments);
    let columns = (0..66).map(|at| format!("COUNT(*) AS c{at}")).collect::<Vec<_>>().join(",");
    assert!(PreparedGraphAggregateText::prepare(&format!("MATCH (a) RETURN {columns}"), symbols).is_err());
    let text = format!("MATCH (a) RETURN {}", (0..65).map(|at| format!("COUNT(*) AS c{at}")).collect::<Vec<_>>().join(","));
    assert_eq!(PreparedGraphAggregateText::prepare(&text, symbols).unwrap().columns().len(), 65);
}

#[test]
fn text_aggregates_observe_every_checkpoint_limits_and_noninteger_sum_refusal() {
    let t = prepare("MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS paths,SUM(b.n) AS total GROUP BY a");
    let edges = [(VId(1), RelationId(1), VId(2)), (VId(1), RelationId(1), VId(2))];
    let amount = CanonicalScalar::Int(7);
    let mut events = 0;
    let full = t.execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&amount)), policy(),
        || { events += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=events {
        let mut at = 0;
        let result = t.execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&amount)), policy(),
            || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(actual)) if actual == stop));
        assert_eq!(at, stop);
    }
    let run = |policy| t.execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&amount)), policy, || Ok::<_, usize>(()));
    let exact = GqlQueryPolicy::new(2, 1, full.evaluator.work_units, full.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), full);
    assert!(matches!(run(GqlQueryPolicy::new(2, 0, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    assert!(matches!(run(GqlQueryPolicy::new(2, 1, full.evaluator.work_units - 1, u64::MAX)), Err(GqlQueryError::Evaluator(_))));
    let noninteger = CanonicalScalar::Bool(true);
    assert!(matches!(t.execute_governed(2, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&noninteger)), policy(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 1 }))));
}
