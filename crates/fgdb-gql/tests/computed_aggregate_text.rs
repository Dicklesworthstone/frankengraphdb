//! Native aggregate text must share the checked expression and group engines.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregate, GraphAggregateError, GraphAggregateRow, GraphAggregateTextSlot,
    GraphIntegerErrorKind, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText, PreparedGraphText,
    MAX_GRAPH_INTEGER_INSTRUCTIONS,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);
type ResultOf = Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<&'static str>, ()>>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1_000, 1_000, 5_000_000, 2_000_000) }
fn run(plan: &PreparedGraphAggregate, vertices: &[VId], edges: &[Triple], props: &Props) -> ResultOf {
    plan.execute_governed((vertices.len() + edges.len()) as u64, vertices.iter().copied(), edges.iter().copied(),
        |_, _| Ok(true), |vid, key| Ok(props.get(&(vid, key))), policy(), || Ok(()))
}
fn properties() -> Props {
    let mut props = Props::new();
    for (id, p, q) in [(1, -2, -2), (2, 2, 2), (3, 2, 3), (4, 5, 1)] {
        props.insert((VId(id), P), CanonicalScalar::Int(p));
        props.insert((VId(id), Q), CanonicalScalar::Int(q));
    }
    props.insert((VId(5), Q), CanonicalScalar::Null);
    props
}

#[test]
fn computed_group_keys_products_distinct_and_exact_averages_have_native_syntax() {
    let text = "MATCH (n) RETURN ABS(n.p) AS bucket,SUM(n.p*n.q) AS total, \
        AVG(COALESCE(n.q,0)) AS mean,COUNT(DISTINCT ABS(n.p)) AS unique \
        GROUP BY ABS(n.p) ORDER BY bucket";
    let template = PreparedGraphAggregateText::prepare(text, symbols).unwrap();
    assert_eq!(template.columns(), &["bucket", "total", "mean", "unique"]);
    assert_eq!(template.output_slots(), &[GraphAggregateTextSlot::GroupKey(0),
        GraphAggregateTextSlot::Aggregate(0), GraphAggregateTextSlot::Aggregate(1),
        GraphAggregateTextSlot::Aggregate(2)]);
    let plan = template.bind_parameters(&GqlParameters::new()).unwrap();
    let result = run(&plan, &[VId(1), VId(2), VId(3), VId(4), VId(5)], &[], &properties()).unwrap();
    assert_eq!(result.value.len(), 3);
    let rows = &result.value;
    assert_eq!(rows[0].keys()[0].as_scalar(), Some(&CanonicalScalar::Int(2)));
    assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(14));
    assert_eq!(rows[0].get(1).unwrap().as_average().unwrap().to_string(), "1");
    assert_eq!(rows[0].get(2).unwrap().as_count(), Some(1));
    assert_eq!(rows[1].keys()[0].as_scalar(), Some(&CanonicalScalar::Int(5)));
    assert_eq!(rows[1].get(0).unwrap().as_integer(), Some(5));
    assert!(rows[2].keys()[0].is_null());
    assert!(rows[2].get(0).unwrap().is_null());
    assert_eq!(rows[2].get(1).unwrap().as_average().unwrap().to_string(), "0");
    assert_eq!(rows[2].get(2).unwrap().as_count(), Some(0));
    let expression = prepare("MATCH (n) RETURN SUM(n.p+2*n.q) AS x,SUM((n.p+2)*n.q) AS y");
    let rows = run(&expression, &[VId(2)], &[], &properties()).unwrap().value;
    assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(6));
    assert_eq!(rows[0].get(1).unwrap().as_integer(), Some(8));
}

#[test]
fn repeated_hidden_expressions_share_state_but_preserve_parameter_occurrences() {
    let text = "MATCH (n) RETURN SUM(n.p*$factor) AS total \
        HAVING SUM(n.p * $factor)>$floor ORDER BY SUM(n.p*$factor)";
    let calls = Cell::new(0);
    let template = PreparedGraphAggregateText::prepare(text, |kind, name| {
        calls.set(calls.get() + 1); symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.get(), 1);
    assert_eq!(template.parameter_schema()[0].name, "factor");
    assert_eq!(template.parameter_schema()[0].occurrences, 3);
    assert_eq!(template.parameter_schema()[1].name, "floor");
    let args = GqlParameters::new().with_int64("factor", 3).unwrap().with_int64("floor", 0).unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(bound.evaluation_aggregate_columns().len(), 1);
    assert_eq!(bound.input_projection().unwrap().len(), 1);
    assert_eq!(bound.input_pattern().columns().len(), 1);
    assert_eq!(bound, template.bind_parameters(&args).unwrap());
    assert_eq!(calls.get(), 1);
    let rows = run(&bound, &[VId(2), VId(4)], &[], &properties()).unwrap().value;
    assert_eq!(rows[0].get(0).unwrap().as_integer(), Some(21));
    let hidden = prepare("MATCH (n) RETURN COUNT(*) AS c HAVING \
        SUM(n.p*n.q) IN [14,19] ORDER BY SUM(n.p*n.q)");
    assert_eq!(hidden.evaluation_aggregate_columns().len(), 2);
    assert_eq!(hidden.aggregate_columns().len(), 1);
    let rows = run(&hidden, &[VId(1), VId(2), VId(3)], &[], &properties()).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values().len(), 1);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(3));
}

#[test]
fn computed_keys_work_as_hidden_grouping_and_direct_clause_references() {
    let vertices = [VId(1), VId(2), VId(3), VId(4), VId(5)];
    let plan = prepare("MATCH (n) RETURN COUNT(*) AS c GROUP BY ABS(n.p) \
        HAVING ABS(n.p) IS NOT NULL ORDER BY ABS(n.p) DESC");
    let rows = run(&plan, &vertices, &[], &properties()).unwrap().value;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.keys().is_empty()));
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(1));
    assert_eq!(rows[1].get(0).unwrap().as_count(), Some(3));
    let plan = prepare("MATCH (n) RETURN COUNT(*) AS c GROUP BY n.p+1 ORDER BY n.p+1 DESC LIMIT 1");
    let rows = run(&plan, &vertices, &[], &properties()).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(1));
    let plan = prepare("MATCH (n) RETURN DISTINCT COUNT(*) AS c GROUP BY ABS(n.p)");
    assert_eq!(run(&plan, &vertices, &[], &properties()).unwrap().value.len(), 2);
}

#[test]
fn constant_and_nullable_arguments_preserve_walk_optional_and_empty_bags() {
    let plan = prepare("MATCH WALK (a)-[:R*0..2]->(b) RETURN \
        COUNT(1) AS c,COUNT(NULL) AS absent,SUM(7) AS total,MIN('quoted '' value') AS text");
    let edges = [(VId(1), R, VId(1)), (VId(1), R, VId(1)), (VId(1), R, VId(2))];
    let rows = run(&plan, &[VId(1), VId(2)], &edges, &Props::new()).unwrap().value;
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(11));
    assert_eq!(rows[0].get(1).unwrap().as_count(), Some(0));
    assert_eq!(rows[0].get(2).unwrap().as_integer(), Some(77));
    assert_eq!(rows[0].get(3).unwrap().as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::ucs_basic_text("quoted ' value").unwrap()));
    let empty = run(&plan, &[], &[], &Props::new()).unwrap().value;
    assert_eq!(empty.len(), 1);
    assert_eq!(empty[0].get(0).unwrap().as_count(), Some(0));
    assert!(empty[0].get(2).unwrap().is_null());
    let optional = prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN \
        COUNT(1) AS c,AVG(COALESCE(b.p,0)) AS mean");
    let rows = run(&optional, &[VId(1), VId(2), VId(3)],
        &[(VId(1), R, VId(2)), (VId(1), R, VId(2))], &properties()).unwrap().value;
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(4));
    assert_eq!(rows[0].get(1).unwrap().as_average().unwrap().to_string(), "1");
}

#[test]
fn plain_column_aggregates_keep_the_original_streaming_definition() {
    let actual = prepare("MATCH (n) RETURN n.p AS p,SUM(n.q) AS total GROUP BY n.p");
    let child = PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p,n.q AS total", symbols)
        .unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let expected = PreparedGraphAggregate::prepare(child, &[0], &[GraphAggregate::sum_int("total", 1)], 0, None).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.canonical_bytes(), expected.canonical_bytes());
    assert!(actual.input_projection().is_none());
    let count = prepare("MATCH (n) RETURN COUNT(*) AS c");
    assert!(count.input_projection().is_none());
    let identity = prepare("MATCH (n) RETURN MIN(n) AS first,COUNT(n) AS c");
    assert!(identity.input_projection().is_none());
    let rows = run(&identity, &[VId(2), VId(1)], &[], &Props::new()).unwrap().value;
    assert_eq!(rows[0].get(0).unwrap().as_value().unwrap().as_vertex(), Some(VId(1)));
    assert_eq!(rows[0].get(1).unwrap().as_count(), Some(2));
}

#[test]
fn typed_parameters_share_argument_validation_and_original_offsets_without_interpolation() {
    let text = "\u{2003}MATCH (n) RETURN MIN($payload) AS text,SUM(n.p*$step) AS total";
    let calls = Cell::new(0);
    let template = PreparedGraphAggregateText::prepare_with_parameter_types(text,
        &[("payload", GqlParameterType::Scalar(CanonicalScalarKind::Text))], |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).unwrap();
    let payload = "λ') GROUP BY secret; SET n.p=0";
    let args = GqlParameters::new().with_text("payload", payload).unwrap().with_int64("step", 2).unwrap();
    let plan = template.bind_parameters(&args).unwrap();
    let frozen = plan.canonical_bytes();
    let rows = run(&plan, &[VId(2)], &[], &properties()).unwrap().value;
    assert_eq!(rows[0].get(0).unwrap().as_value().unwrap().as_scalar(),
        Some(&CanonicalScalar::ucs_basic_text(payload).unwrap()));
    assert_eq!(rows[0].get(1).unwrap().as_integer(), Some(4));
    assert_eq!(calls.get(), 1);
    assert_eq!(frozen, template.bind_parameters(&args).unwrap().canonical_bytes());
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, text.find("$payload").unwrap());
    assert!(matches!(missing.kind, GraphPatternTextErrorKind::MissingParameter));
    let wrong = GqlParameters::new().with_bool("payload", true).unwrap().with_int64("step", 2).unwrap();
    assert!(matches!(template.bind_parameters(&wrong).unwrap_err().kind,
        GraphPatternTextErrorKind::ParameterTypeMismatch { .. }));
    assert!(template.bind_parameters(&args.clone().with_int64("extra", 1).unwrap()).is_err());
    assert!(!format!("{plan:?} {template:?} {rows:?} {missing:?}").contains(payload));
}

#[test]
fn malformed_computation_and_grouping_refuse_before_catalog_resolution() {
    for text in [
        "MATCH (n) RETURN SUM(n.p+) AS x", "MATCH (n) RETURN SUM(AVG(n.p)) AS x",
        "MATCH (n) RETURN SUM(n+1) AS x", "MATCH (n) RETURN SUM(TRUE+1) AS x",
        "MATCH (n) RETURN SUM(COALESCE(n.p)) AS x", "MATCH (n) RETURN SUM(other.p*2) AS x",
        "MATCH (n) RETURN SUM(n.p+$x) AS x LIMIT $x", "MATCH (n) RETURN SUM(*) AS x",
        "MATCH (n) RETURN ABS(n.p),COUNT(*) AS c GROUP BY ABS(n.p)",
        "MATCH (n) RETURN ABS(n.p) AS g,COUNT(*) AS c GROUP BY ABS(n.q)",
        "MATCH (n) RETURN COUNT(*) AS c GROUP BY n.p+1,n.p+1",
        "MATCH (n) RETURN COUNT(*) AS c HAVING SUM(n.p+) > 0",
        "MATCH (n) RETURN COUNT(*) AS c ORDER BY SUM(n.p+)",
    ] {
        let calls = Cell::new(0);
        assert!(PreparedGraphAggregateText::prepare(text, |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls.get(), 0, "{text}");
    }
    for expression in [format!("{}n.p{}", "(".repeat(66), ")".repeat(66)),
        std::iter::repeat_n("n.p", MAX_GRAPH_INTEGER_INSTRUCTIONS).collect::<Vec<_>>().join("+")] {
        let calls = Cell::new(0);
        assert!(PreparedGraphAggregateText::prepare(&format!("MATCH (n) RETURN SUM({expression}) AS x"),
            |kind, name| { calls.set(calls.get() + 1); symbols(kind, name) }).is_err());
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn hidden_arithmetic_and_eager_source_failures_survive_limit_zero_and_lazy_branches() {
    let props = BTreeMap::from([((VId(1), P), CanonicalScalar::Int(1)),
        ((VId(2), P), CanonicalScalar::Int(0))]);
    for text in [
        "MATCH (n) RETURN SUM(10/n.p) AS x LIMIT 0",
        "MATCH (n) RETURN COUNT(*) AS c HAVING SUM(10/n.p) IS NOT NULL LIMIT 0",
        "MATCH (n) RETURN COUNT(*) AS c ORDER BY SUM(10/n.p) LIMIT 0",
    ] {
        let plan = prepare(text);
        assert!(matches!(run(&plan, &[VId(1), VId(2)], &[], &props),
            Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
                if error.kind == GraphIntegerErrorKind::DivisionByZero));
    }
    let plan = prepare("MATCH (n) RETURN SUM(COALESCE(1,10/n.p)) AS x LIMIT 0");
    assert!(run(&plan, &[VId(1), VId(2)], &[], &props).unwrap().value.is_empty());
    let result: ResultOf = plan.execute_governed(2, [VId(1), VId(2)], [], |_, _| Ok(true),
        |vid, key| if vid == VId(2) { Err("unreadable lazy input") } else { Ok(props.get(&(vid, key))) },
        policy(), || Ok(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("unreadable lazy input")))));
    // Preparation validates shape, not data-independent constant evaluation.
    let plan = prepare("MATCH (n) RETURN COUNT(*) AS c,SUM(1/0) AS bad");
    let empty = run(&plan, &[], &[], &Props::new()).unwrap().value;
    assert_eq!(empty[0].get(0).unwrap().as_count(), Some(0));
    assert!(empty[0].get(1).unwrap().is_null());
    assert!(matches!(run(&plan, &[VId(1)], &[], &Props::new()),
        Err(GqlQueryError::Source(GraphAggregateError::InputExpression { error, .. }))
            if error.kind == GraphIntegerErrorKind::DivisionByZero));
}
