//! Private summaries are computed by the ordinary aggregate, never by a
//! second query or a post-hoc pass over returned rows.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate, GraphAggregateColumn,
    GraphAggregateError, GraphAggregateFilter, GraphAggregateOrder, GraphAggregateTest,
    GraphAggregateTextSlot, GraphSymbol, GraphSymbolKind, PreparedGraphAggregate,
    PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "Owner") => Some(GraphSymbol::Label(LabelId(1))),
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
fn arguments(minimum: i64, floor: i64, off: u64, take: u64) -> GqlParameters {
    GqlParameters::new().with_int64("minimum", minimum).unwrap()
        .with_int64("floor", floor).unwrap().with_uint64("off", off).unwrap()
        .with_uint64("take", take).unwrap()
}

#[test]
fn internal_calls_share_typed_state_but_never_extend_the_returned_schema() {
    let text = "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS paths,a AS owner GROUP BY a \
        HAVING COUNT(*) >= $minimum AND AVG(b.p) > $floor \
        ORDER BY AVG_INT(ALL b.p) DESC,SUM(DISTINCT b.p) DESC,owner SKIP $off LIMIT $take";
    let mut calls = BTreeMap::new();
    let template = PreparedGraphAggregateText::prepare(text, |kind, name| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(template.columns(), &["paths", "owner"]);
    assert_eq!(template.output_slots(), &[
        GraphAggregateTextSlot::Aggregate(0), GraphAggregateTextSlot::GroupKey(0),
    ]);
    let bound = template.bind_parameters(&arguments(2, 0, 1, 3)).unwrap();
    assert_eq!(bound.evaluation_aggregate_columns().len(), 3);
    assert_eq!(bound.aggregate_columns(), &["paths"]);
    assert_eq!(bound.input_pattern().columns(), &["owner", "__fgdb_hidden_0"]);
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap(); builder.vertex("b").unwrap();
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let input = builder.prepare_values(&[
        GraphColumn::vertex("owner", "a"),
        GraphColumn::property("__fgdb_hidden_0", "b", P),
    ], 0, None).unwrap().with_duplicates();
    let expected = PreparedGraphAggregate::prepare(input, &[0], &[
        GraphAggregate::count_rows("paths"),
        GraphAggregate::average_int("__fgdb_hidden_0", 1),
        GraphAggregate::sum_int_distinct("__fgdb_hidden_1", 1),
    ], 1, Some(3)).unwrap().with_aggregate_output_prefix(1).unwrap().with_result_clauses(&[
        GraphAggregateFilter { column: GraphAggregateColumn::Aggregate(0),
            test: GraphAggregateTest::Integer { comparison: IntegerComparison::GreaterOrEqual, value: 2 } },
        GraphAggregateFilter { column: GraphAggregateColumn::Aggregate(1),
            test: GraphAggregateTest::Integer { comparison: IntegerComparison::Greater, value: 0 } },
    ], &[
        GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(1)),
        GraphAggregateOrder::descending(GraphAggregateColumn::Aggregate(2)),
        GraphAggregateOrder::ascending(GraphAggregateColumn::GroupKey(0)),
    ]).unwrap();
    assert_eq!(bound, expected);
    let bytes = bound.canonical_bytes();
    assert_ne!(template.bind_parameters(&arguments(3, 1, 0, 2)).unwrap().canonical_bytes(), bytes);
    assert_eq!(template.bind_parameters(&arguments(2, 0, 1, 3)).unwrap(), bound);
    assert_eq!(bound.canonical_bytes(), bytes);
    assert!(calls.values().all(|count| *count == 1));
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert!(template.bind_parameters(&arguments(2, 0, 1, 3).with_int64("extra", 1).unwrap()).is_err());
}

#[test]
fn key_only_optional_groups_match_independent_hidden_filter_rank_and_page_oracles() {
    let universe = [(VId(1), R, VId(10)), (VId(1), R, VId(10)),
        (VId(1), R, VId(11)), (VId(2), R, VId(11)), (VId(2), R, VId(12))];
    let vertices = [VId(1), VId(2), VId(3), VId(10), VId(11), VId(12)];
    for (descending, null_first) in [(true, false), (false, true)] {
        let text = format!("MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) \
            RETURN a,a AS repeated GROUP BY a \
            HAVING COUNT(b.p) > 1 OR AVG(b.p) IS NULL \
            ORDER BY AVG(DISTINCT b.p) {} NULLS {},a SKIP $off LIMIT $take",
            if descending { "DESC" } else { "ASC" }, if null_first { "FIRST" } else { "LAST" });
        let template = PreparedGraphAggregateText::prepare(&text, symbols).unwrap();
        assert_eq!(template.columns(), &["a", "repeated"]);
        assert_eq!(template.output_slots(), &[GraphAggregateTextSlot::GroupKey(0); 2]);
        for mask in 0..32 {
            let edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, edge)| *edge).collect();
            for code in 0..27 {
                let mut encoded = code;
                let values: Vec<_> = (0..3).map(|_| {
                    let value = [None, Some(-3_i64), Some(7)][encoded % 3]; encoded /= 3; value
                }).collect();
                let scalars: Vec<_> = values.iter().map(|n| n.map(CanonicalScalar::Int)).collect();
                let mut expected = Vec::new();
                for owner in [VId(1), VId(2), VId(3)] {
                    let bag: Vec<_> = edges.iter().filter(|edge| edge.0 == owner)
                        .filter_map(|edge| values[(edge.2.0 - 10) as usize]).collect();
                    if bag.len() <= 1 && !bag.is_empty() { continue; }
                    let unique: BTreeSet<_> = bag.into_iter().collect();
                    let mean = (!unique.is_empty()).then(|| (
                        unique.iter().map(|n| i128::from(*n)).sum::<i128>(), unique.len() as i128,
                    ));
                    expected.push((owner, mean));
                }
                expected.sort_by(|a, b| {
                    let cmp = match (a.1, b.1) {
                        (None, None) => Ordering::Equal,
                        (None, Some(_)) => if null_first { Ordering::Less } else { Ordering::Greater },
                        (Some(_), None) => if null_first { Ordering::Greater } else { Ordering::Less },
                        (Some((a, na)), Some((b, nb))) => {
                            let cmp = (a * nb).cmp(&(b * na));
                            if descending { cmp.reverse() } else { cmp }
                        }
                    };
                    cmp.then_with(|| a.0.cmp(&b.0))
                });
                for (offset, count) in [(0, 0), (0, 3), (1, 1), (4, 1)] {
                    let args = GqlParameters::new().with_uint64("off", offset).unwrap()
                        .with_uint64("take", count).unwrap();
                    let query = template.bind_parameters(&args).unwrap();
                    assert_eq!(query.evaluation_aggregate_columns().len(), 3);
                    assert!(query.aggregate_columns().is_empty());
                    let result = query.execute_governed((vertices.len() + edges.len()) as u64,
                        vertices, edges.iter().copied(), |vid, _| Ok::<_, ()>(vid.0 < 10),
                        |vid, _| Ok(scalars[(vid.0 - 10) as usize].as_ref()), wide(), || Ok::<_, ()>(())).unwrap();
                    assert!(result.value.iter().all(|row| row.values().is_empty() && row.keys().len() == 1));
                    let actual: Vec<_> = result.value.iter().map(|row| row.keys()[0].as_vertex().unwrap()).collect();
                    let page: Vec<_> = expected.iter().skip(offset as usize).take(count as usize).map(|row| row.0).collect();
                    assert_eq!(actual, page, "mask={mask} code={code} off={offset} take={count}");
                }
            }
        }
    }
}

#[test]
fn private_aliases_cannot_capture_public_names_and_calls_preserve_distinctness() {
    let query = prepare("MATCH (a) RETURN a AS __fgdb_hidden_0,COUNT(*) AS __fgdb_hidden_1 \
        GROUP BY a HAVING MIN(a.p) IS NULL ORDER BY MIN(a.p),__fgdb_hidden_1");
    assert_eq!(query.evaluation_aggregate_columns().len(), 2);
    assert_eq!(query.aggregate_columns(), &["__fgdb_hidden_1"]);
    assert_eq!(query.input_pattern().columns(), &["__fgdb_hidden_0", "__fgdb_hidden_2"]);
    assert_eq!(query.ordering()[1].column, GraphAggregateColumn::Aggregate(0));
    let query = prepare("MATCH (a)-[:R]->(b) RETURN a GROUP BY a \
        HAVING COUNT(DISTINCT b.p) <= COUNT(b.p) AND SUM(b.p) >= SUM_INT(ALL b.p) \
        ORDER BY AVG(DISTINCT b.p)");
    assert_eq!(query.evaluation_aggregate_columns().len(), 4);
    assert_eq!(query.input_pattern().columns().len(), 2);
    let ordinary = prepare("MATCH (a) RETURN COUNT(*) AS x,COUNT(*) AS y HAVING x >= 1 ORDER BY y");
    let unchanged = prepare("MATCH (a) RETURN COUNT(*) AS x,COUNT(*) AS y HAVING COUNT(*) >= 1 ORDER BY y");
    assert_eq!(ordinary.canonical_bytes(), unchanged.canonical_bytes());
    assert_eq!(unchanged.evaluation_aggregate_columns().len(), 2);
    assert_eq!(unchanged.clone().with_aggregate_output_prefix(2).unwrap().canonical_bytes(), unchanged.canonical_bytes());
    let alias = prepare("MATCH (a) RETURN a AS owner,COUNT(*) AS a GROUP BY a HAVING a > 0 ORDER BY MIN(a.p),a");
    assert_eq!(alias.having()[0].column, GraphAggregateColumn::Aggregate(0));
    assert_eq!(alias.ordering()[1].column, GraphAggregateColumn::Aggregate(0));
}

#[test]
fn computed_width_and_malformed_private_references_refuse_before_catalog_access() {
    for text in [
        "MATCH (a) RETURN a GROUP BY a", "MATCH (a) RETURN COUNT(*) GROUP BY missing",
        "MATCH (a) RETURN a GROUP BY a HAVING SUM(*) > 0",
        "MATCH (a) RETURN a GROUP BY a ORDER BY AVG(COUNT(a.p))",
        "MATCH (a) RETURN a GROUP BY a ORDER BY SUM(a.p),SUM_INT(ALL a.p)",
        "MATCH (a) RETURN a GROUP BY a ORDER BY a.p",
        "MATCH (a) RETURN COUNT(*) HAVING MIN(a.p) IS NULL ORDER BY __fgdb_hidden_0",
        "MATCH (a) RETURN a GROUP BY a HAVING COUNT(x.p) > 0",
        "MATCH (a) RETURN a GROUP BY a HAVING COUNT(DISTINCT *) > 0",
        "MATCH (a) RETURN a GROUP BY a HAVING AVG(a.p) >= $x LIMIT $x",
    ] {
        let mut calls = 0;
        assert!(PreparedGraphAggregateText::prepare(text, |kind, name| {
            calls += 1; symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    for hidden in [64, 65] {
        let order = (0..hidden).map(|at| format!("SUM(a.p{})", at + 1)).collect::<Vec<_>>().join(",");
        let mut calls = 0;
        let result = PreparedGraphAggregateText::prepare(&format!("MATCH (a) RETURN a GROUP BY a ORDER BY {order}"), |kind, name| {
            calls += 1;
            if kind == GraphSymbolKind::Property {
                Some(GraphSymbol::Property(PropertyKeyId(name.strip_prefix('p')?.parse().ok()?)))
            } else { symbols(kind, name) }
        });
        if hidden == 64 {
            let query = result.unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            assert_eq!(query.evaluation_aggregate_columns().len(), 64);
            assert_eq!(query.input_pattern().columns().len(), 65);
            assert!(query.aggregate_columns().is_empty());
        } else { assert!(result.is_err()); assert_eq!(calls, 0); }
    }
}

#[test]
fn hidden_inputs_and_invalid_numeric_domains_cannot_be_masked_by_zero_output() {
    let good = CanonicalScalar::Int(7);
    let bad = CanonicalScalar::Bool(true);
    for suffix in ["HAVING FALSE ORDER BY SUM(a.p) LIMIT 0", "HAVING TRUE OR AVG(a.p) IS NULL LIMIT 0"] {
        let query = prepare(&format!("MATCH (a) RETURN COUNT(*) AS n {suffix}"));
        let result = query.execute_governed(1, [VId(1)], [], |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&bad)), wide(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::NonIntegerSum { aggregate: 1 }
            | GraphAggregateError::NonIntegerAverage { aggregate: 1 }))));
        let result = query.execute_governed(2, [VId(1), VId(2)], [], |_, _| Ok::<_, &str>(true),
            |vid, _| if vid == VId(1) { Ok(Some(&good)) } else { Err("late hidden read") }, wide(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("late hidden read")))));
    }
    for (tail, rows) in [("HAVING AVG(a.p) IS NULL", 1), ("HAVING NOT (AVG(a.p) = 0)", 0)] {
        let query = prepare(&format!("MATCH (a) RETURN COUNT(*) AS n {tail} ORDER BY AVG(a.p)"));
        let empty = query.execute_governed(0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(empty.value.len(), rows);
        if rows == 1 { assert_eq!(empty.value[0].values().len(), 1); assert_eq!(empty.value[0].get(0).unwrap().as_count(), Some(0)); }
    }
}

#[test]
fn hidden_extrema_are_borrowed_without_copying_their_return_payloads() {
    let bytes = CanonicalScalar::bytes(vec![7; 8192]).unwrap();
    let hidden = prepare("MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n GROUP BY a ORDER BY MIN(b.p),a LIMIT 1");
    let visible = prepare("MATCH (a)-[:R]->(b) RETURN a,COUNT(*) AS n,MIN(b.p) AS m GROUP BY a ORDER BY m,a LIMIT 1");
    let edges = [(VId(1), R, VId(10)), (VId(2), R, VId(10))];
    let run = |query: &PreparedGraphAggregate| query.execute_governed(2, [], edges,
        |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&bytes)), wide(), || Ok::<_, ()>(())).unwrap();
    let a = run(&hidden); let b = run(&visible);
    assert_eq!(a.value[0].keys(), b.value[0].keys());
    assert_eq!(a.value[0].get(0), b.value[0].get(0));
    assert!(a.value[0].get(1).is_none());
    assert!(b.evaluator.scratch_entries >= a.evaluator.scratch_entries + 128);
}

#[test]
fn hidden_aggregation_uses_all_resource_limits_and_every_checkpoint() {
    let query = prepare("MATCH (a)-[:R]->(b) RETURN a GROUP BY a \
        HAVING COUNT(b.p) >= 1 ORDER BY AVG(b.p) DESC,SUM(DISTINCT b.p) DESC,a LIMIT 1");
    let scalars = [CanonicalScalar::Int(1), CanonicalScalar::Int(5), CanonicalScalar::Int(9)];
    let edges = [(VId(1), R, VId(10)), (VId(1), R, VId(10)), (VId(1), R, VId(11)),
        (VId(2), R, VId(11)), (VId(2), R, VId(12))];
    let run = |policy| query.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), policy, || Ok::<_, ()>(()));
    let complete = run(wide()).unwrap();
    assert_eq!(complete.value[0].keys()[0].as_vertex(), Some(VId(2)));
    assert!(complete.value[0].values().is_empty());
    let work = complete.evaluator.work_units; let scratch = complete.evaluator.scratch_entries;
    assert_eq!(run(GqlQueryPolicy::new(5, 1, work, scratch)).unwrap(), complete);
    for policy in [GqlQueryPolicy::new(4, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(5, 0, u64::MAX, u64::MAX), GqlQueryPolicy::new(5, 1, work - 1, u64::MAX),
        GqlQueryPolicy::new(5, 1, u64::MAX, scratch - 1)] { assert!(run(policy).is_err()); }
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
    assert_eq!(reversed.value, complete.value);
}

#[test]
fn hidden_topology_counts_still_refuse_overflow_before_having_or_limit() {
    let atoms = vec!["(a)-[:R]->(b)"; 64].join(",");
    let query = prepare(&format!("MATCH {atoms} RETURN a GROUP BY a HAVING TRUE OR COUNT(*) >= 0 LIMIT 0"));
    let result = query.execute_governed(2, [], [(VId(1), R, VId(2)); 2],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::ArithmeticOverflow { aggregate: 0 }))));
}
