//! Exact numeric aggregates use the existing compiled pattern/result stages.
//! Small-case expectations enumerate original edge occurrences independently.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GlaDirection, IntegerComparison};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate,
    GraphAggregateColumn, GraphAggregateError, GraphAggregateOrder, GraphExactAverage,
    GraphHavingExpression, GraphHavingOp, GraphHavingOperand, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalF64, CanonicalScalar, VId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn ratio(value: Option<GraphExactAverage>) -> Option<(i128, u64)> {
    value.map(|value| (value.numerator(), value.denominator()))
}
fn oracle_mean(values: &[i64]) -> Option<(i128, u64)> {
    if values.is_empty() { return None; }
    let sum: i128 = values.iter().map(|n| i128::from(*n)).sum();
    let count = values.len() as u64;
    // Independent Euclidean normalization of the small oracle result.
    let (mut a, mut b) = (sum.unsigned_abs(), u128::from(count));
    while b != 0 { let remainder = a % b; a = b; b = remainder; }
    Some((sum / a as i128, count / a as u64))
}
fn compare_means(a: Option<(i128, u64)>, b: Option<(i128, u64)>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some((a, da)), Some((b, db))) => (b * i128::from(da)).cmp(&(a * i128::from(db))),
    }
}

#[test]
fn all_and_distinct_averages_match_owned_occurrence_oracles_and_rank_exactly() {
    let query = prepare("MATCH (a)-[:R]->(b) RETURN a,SUM(b.p) AS s,SUM(DISTINCT b.p) AS d,AVG(b.p) AS m,AVG(DISTINCT b.p) AS u,COUNT(b.p) AS n GROUP BY a ORDER BY m DESC NULLS LAST,a");
    let universe = [(VId(1), R, VId(10)), (VId(1), R, VId(10)),
        (VId(1), R, VId(11)), (VId(2), R, VId(11)), (VId(2), R, VId(12))];
    for mask in 0..32 {
        let edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge).collect();
        for code in 0..27 {
            let mut encoded = code;
            let values: Vec<_> = (0..3).map(|_| {
                let value = [None, Some(-3), Some(7)][encoded % 3]; encoded /= 3; value
            }).collect();
            let scalars: Vec<_> = values.iter().map(|value| value.map(CanonicalScalar::Int)).collect();
            let mut groups: BTreeMap<VId, Vec<i64>> = BTreeMap::new();
            for (owner, _, destination) in &edges {
                let group = groups.entry(*owner).or_default();
                if let Some(value) = values[(destination.0 - 10) as usize] { group.push(value); }
            }
            let mut expected: Vec<_> = groups.into_iter().map(|(key, values)| {
                let distinct: Vec<_> = values.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
                (key, (!values.is_empty()).then(|| values.iter().map(|v| i128::from(*v)).sum::<i128>()),
                    (!distinct.is_empty()).then(|| distinct.iter().map(|v| i128::from(*v)).sum::<i128>()),
                    oracle_mean(&values), oracle_mean(&distinct), values.len() as u64)
            }).collect();
            expected.sort_by(|a, b| compare_means(a.3, b.3).then_with(|| a.0.cmp(&b.0)));
            let result = query.execute_governed(edges.len() as u64, [], edges.iter().copied(),
                |_, _| Ok::<_, ()>(true), |vid, _| Ok(scalars[(vid.0 - 10) as usize].as_ref()), wide(), || Ok::<_, ()>(())).unwrap();
            let actual: Vec<_> = result.value.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
                row.get(0).unwrap().as_integer(), row.get(1).unwrap().as_integer(),
                ratio(row.get(2).unwrap().as_average()), ratio(row.get(3).unwrap().as_average()),
                row.get(4).unwrap().as_count().unwrap())).collect();
            assert_eq!(actual, expected, "mask={mask} code={code}");
        }
    }
}

#[test]
fn typed_and_text_forms_share_columns_tags_having_and_ordering() {
    let text = "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS mean,SUM(DISTINCT b.p) AS total GROUP BY a HAVING mean < total ORDER BY mean DESC LIMIT 3";
    let actual = prepare(text);
    let mut builder = GraphPatternBuilder::new(); builder.vertex("a").unwrap(); builder.vertex("b").unwrap();
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let input = builder.prepare_values(&[GraphColumn::vertex("a", "a"),
        GraphColumn::property("mean", "b", P)], 0, None).unwrap().with_duplicates();
    let expression = GraphHavingExpression::prepare(&[GraphHavingOp::Compare {
        left: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(0)),
        comparison: IntegerComparison::Less,
        right: GraphHavingOperand::Column(GraphAggregateColumn::Aggregate(1)),
    }]).unwrap();
    let expected = PreparedGraphAggregate::prepare(input, &[0],
        &[GraphAggregate::average_int("mean", 1), GraphAggregate::sum_int_distinct("total", 1)], 0, Some(3)).unwrap()
        .with_result_clauses(&[], &[GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(0))]).unwrap()
        .with_having_expression(&expression).unwrap();
    assert_eq!(actual, expected);
    assert_eq!(actual.input_pattern().columns().len(), 2);
    assert_eq!(prepare(&text.replace("AVG(", "AVG_INT(ALL ").replace("SUM(", "SUM_INT(")).canonical_bytes(), actual.canonical_bytes());
    let distinct = prepare("MATCH (a) RETURN AVG(DISTINCT a.p) AS mean");
    let all = prepare("MATCH (a) RETURN AVG(a.p) AS mean");
    assert_ne!(distinct.canonical_bytes(), all.canonical_bytes());
    assert_eq!(all.canonical_bytes(), prepare("MATCH (a) RETURN AVG_INT(ALL a.p) AS mean").canonical_bytes());
}

#[test]
fn sub_float_differences_survive_ordering_flat_and_compound_having() {
    let n = i64::MAX;
    let scalars = [CanonicalScalar::Int(n - 1), CanonicalScalar::Int(n), CanonicalScalar::Int(i64::MIN)];
    let edges = [(VId(1), R, VId(10)), (VId(1), R, VId(11)),
        (VId(2), R, VId(11)), (VId(3), R, VId(12)), (VId(3), R, VId(11))];
    let head = "MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS mean,SUM(b.p) AS total GROUP BY a";
    let query = prepare(&format!("{head} ORDER BY mean DESC"));
    let run = |query: &PreparedGraphAggregate| query.execute_governed(5, [], edges,
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), wide(), || Ok::<_, ()>(())).unwrap().value;
    let result = run(&query);
    assert_eq!(result.iter().map(|row| row.keys()[0].as_vertex().unwrap()).collect::<Vec<_>>(), vec![VId(2), VId(1), VId(3)]);
    assert_eq!(ratio(result[1].get(0).unwrap().as_average()), Some((2 * i128::from(n) - 1, 2)));
    assert_eq!(ratio(result[2].get(0).unwrap().as_average()), Some((-1, 2)));
    let template = PreparedGraphAggregateText::prepare(&format!("{head} HAVING mean > $lo AND mean < $hi"), symbols).unwrap();
    let args = GqlParameters::new().with_int64("lo", n - 1).unwrap().with_int64("hi", n).unwrap();
    let bound = template.bind_parameters(&args).unwrap();
    assert_eq!(run(&bound).iter().map(|row| row.keys()[0].as_vertex().unwrap()).collect::<Vec<_>>(), vec![VId(1)]);
    let compound = prepare(&format!("{head} HAVING NOT (mean >= total) OR mean < 0 ORDER BY mean DESC"));
    assert_eq!(run(&compound).len(), 2);
}

#[test]
fn empty_null_and_noninteger_domains_are_explicit_even_with_zero_output() {
    let query = prepare("MATCH (n) RETURN AVG(n.p) AS a,AVG(DISTINCT n.p) AS d,SUM(DISTINCT n.p) AS s");
    let empty = query.execute_governed(0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(empty.value.len(), 1);
    assert!(empty.value[0].values().iter().all(|value| value.is_null()));
    let null = CanonicalScalar::Null;
    let missing = query.execute_governed(2, [VId(1), VId(2)], [], |_, _| Ok::<_, ()>(true),
        |vid, _| Ok((vid == VId(1)).then_some(&null)), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(missing.value, empty.value);
    for scalar in [CanonicalScalar::Bool(true), CanonicalScalar::Float(CanonicalF64::new(1.0)),
        CanonicalScalar::ucs_basic_text("7").unwrap()] {
        let query = prepare("MATCH (n) RETURN AVG(n.p) AS a HAVING TRUE OR a IS NULL LIMIT 0");
        assert!(matches!(query.execute_governed(1, [VId(1)], [], |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)), wide(), || Ok::<_, ()>(())),
            Err(GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { aggregate: 0 }))));
        let sum = prepare("MATCH (n) RETURN SUM(DISTINCT n.p) LIMIT 0");
        assert!(matches!(sum.execute_governed(1, [VId(1)], [], |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&scalar)), wide(), || Ok::<_, ()>(())),
            Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 0 }))));
    }
    let vertex_average = prepare("MATCH (a)-[:R]->(b) RETURN AVG(b)");
    assert!(matches!(vertex_average.execute_governed(1, [], [(VId(1), R, VId(2))],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())),
        Err(GqlQueryError::Source(GraphAggregateError::NonIntegerAverage { .. }))));
    for text in ["MATCH (a) RETURN AVG(*)", "MATCH (a) RETURN AVG(DISTINCT *)",
        "MATCH (a) RETURN AVG(ALL DISTINCT a.p)", "MATCH (a) RETURN AVG(SUM(a.p))",
        "MATCH (a) RETURN AVG(a.p,a.p)"] {
        let mut calls = 0;
        assert!(PreparedGraphAggregateText::prepare(text, |kind, name| {
            calls += 1; symbols(kind, name)
        }).is_err());
        assert_eq!(calls, 0);
    }
}

#[test]
fn exact_numeric_updates_share_all_limits_and_every_interruption_checkpoint() {
    let query = prepare("MATCH (a)-[:R]->(b) RETURN a,AVG(b.p) AS m,SUM(DISTINCT b.p) AS s,AVG(DISTINCT b.p) AS d GROUP BY a HAVING m < s OR d = m ORDER BY d DESC LIMIT 1");
    let scalars = [CanonicalScalar::Int(1), CanonicalScalar::Int(5), CanonicalScalar::Int(9)];
    let edges = [(VId(1), R, VId(10)), (VId(1), R, VId(10)), (VId(1), R, VId(11)),
        (VId(2), R, VId(11)), (VId(2), R, VId(12))];
    let run = |policy| query.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), policy, || Ok::<_, ()>(()));
    let measured = run(wide()).unwrap();
    let exact = GqlQueryPolicy::new(5, 1, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), measured);
    for policy in [GqlQueryPolicy::new(4, 1, u64::MAX, u64::MAX), GqlQueryPolicy::new(5, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(5, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 1, u64::MAX, measured.evaluator.scratch_entries - 1)] { assert!(run(policy).is_err()); }
    let mut total = 0;
    query.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), wide(), || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = query.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), wide(), || {
                at += 1; if at == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
    let reversed = query.execute_governed(5, [], edges.into_iter().rev(), |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(reversed.value, measured.value);
    let late = prepare("MATCH (n) RETURN AVG(DISTINCT n.p) LIMIT 0");
    assert!(matches!(late.execute_governed(2, [VId(1), VId(2)], [], |_, _| Ok::<_, &str>(true),
        |vid, _| if vid == VId(1) { Ok(Some(&scalars[0])) } else { Err("late source failure") },
        wide(), || Ok::<_, ()>(())), Err(GqlQueryError::Source(GraphAggregateError::Source("late source failure")))));
}
