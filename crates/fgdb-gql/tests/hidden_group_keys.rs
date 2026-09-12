//! Hidden GROUP BY inputs are real dependencies, not implicit output columns.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphColumn, GraphPatternBuilder, IntegerComparison};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregate,
    GraphAggregateColumn, GraphAggregateError, GraphAggregateFilter, GraphAggregateOrder,
    GraphAggregateTest, GraphAggregateTextSlot, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, VId};
use std::collections::BTreeMap;

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
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn args(minimum: i64, off: u64, take: u64) -> GqlParameters {
    GqlParameters::new().with_int64("minimum", minimum).unwrap()
        .with_uint64("off", off).unwrap().with_uint64("take", take).unwrap()
}

#[test]
fn hidden_keys_bind_once_and_public_slots_do_not_use_evaluation_indices() {
    let text = "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS n,b AS target,b AS again \
        GROUP BY a.p,a,b HAVING a.p >= $minimum AND n > 0 \
        ORDER BY a.p DESC,a,b SKIP $off LIMIT $take";
    let mut calls = BTreeMap::new();
    let template = PreparedGraphAggregateText::prepare(text, |kind, name| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls.values().all(|count| *count == 1));
    assert_eq!(template.columns(), &["n", "target", "again"]);
    assert_eq!(template.output_slots(), &[
        GraphAggregateTextSlot::Aggregate(0), GraphAggregateTextSlot::GroupKey(0),
        GraphAggregateTextSlot::GroupKey(0),
    ]);
    let bound = template.bind_parameters(&args(3, 1, 2)).unwrap();
    assert_eq!(bound.key_columns(), &["target"]);
    assert_eq!(bound.evaluation_key_columns(), &["__fgdb_group_0", "__fgdb_group_1", "target"]);
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap(); builder.vertex("b").unwrap();
    builder.edge("a", R, GlaDirection::Forward, "b").unwrap();
    let input = builder.prepare_values(&[
        GraphColumn::property("__fgdb_group_0", "a", P),
        GraphColumn::vertex("__fgdb_group_1", "a"), GraphColumn::vertex("target", "b"),
    ], 0, None).unwrap().with_duplicates();
    let expected = PreparedGraphAggregate::prepare(input, &[0, 1, 2], &[
        GraphAggregate::count_rows("n"),
    ], 1, Some(2)).unwrap().with_key_output_columns(&[2]).unwrap().with_result_clauses(&[
        GraphAggregateFilter { column: GraphAggregateColumn::GroupKey(0),
            test: GraphAggregateTest::Integer { comparison: IntegerComparison::GreaterOrEqual, value: 3 } },
        GraphAggregateFilter { column: GraphAggregateColumn::Aggregate(0),
            test: GraphAggregateTest::Integer { comparison: IntegerComparison::Greater, value: 0 } },
    ], &[
        GraphAggregateOrder::descending(GraphAggregateColumn::GroupKey(0)),
        GraphAggregateOrder::ascending(GraphAggregateColumn::GroupKey(1)),
        GraphAggregateOrder::ascending(GraphAggregateColumn::GroupKey(2)),
    ]).unwrap();
    assert_eq!(bound, expected);
    let frozen = bound.canonical_bytes();
    assert_ne!(template.bind_parameters(&args(4, 0, 1)).unwrap().canonical_bytes(), frozen);
    assert_eq!(template.bind_parameters(&args(3, 1, 2)).unwrap(), bound);
    assert_eq!(bound.canonical_bytes(), frozen);
    assert!(calls.values().all(|count| *count == 1));
    let visible = "MATCH (a) RETURN a,COUNT(*) AS n GROUP BY a";
    assert_eq!(prepare(visible), prepare(&visible.replace("RETURN ", "RETURN DISTINCT ")));
}

#[test]
fn all_keeps_equal_projected_groups_but_distinct_runs_before_pagination() {
    let counts = [1_u64, 1, 2, 2, 3];
    let edges: Vec<_> = counts.iter().enumerate().flat_map(|(at, count)| {
        std::iter::repeat_n((VId(at as u128), R, VId(10)), *count as usize)
    }).collect();
    for descending in [false, true] {
        for distinct in [false, true] {
            let mut expected = counts.to_vec();
            if descending { expected.sort_by(|a, b| b.cmp(a)); }
            if distinct { expected.dedup(); }
            for offset in 0..=6 {
                for count in [0, 1, 2, 6] {
                    let text = format!("MATCH (a)-[:R]->(b) RETURN {}COUNT(*) AS n \
                        GROUP BY a ORDER BY {}a SKIP {offset} LIMIT {count}",
                        if distinct { "DISTINCT " } else { "ALL " },
                        if descending { "n DESC," } else { "" });
                    let rows = prepare(&text).execute_governed(edges.len() as u64, [], edges.iter().copied(),
                        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value;
                    assert!(rows.iter().all(|row| row.keys().is_empty() && row.values().len() == 1));
                    assert_eq!(rows.iter().map(|row| row.get(0).unwrap().as_count().unwrap()).collect::<Vec<_>>(),
                        expected.iter().copied().skip(offset).take(count).collect::<Vec<_>>(), "{text}");
                }
            }
        }
    }
    assert!(prepare("MATCH (a) RETURN COUNT(*) AS n GROUP BY a")
        .execute_governed(0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(()))
        .unwrap().value.is_empty());
    assert_eq!(prepare("MATCH (a) RETURN COUNT(*) AS n")
        .execute_governed(0, [], [], |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(()))
        .unwrap().value.len(), 1);
}

#[test]
fn distinct_averages_use_exact_values_and_optional_nulls_share_one_output() {
    let text = "MATCH (a:Owner) OPTIONAL MATCH (a)-[:R]->(b) \
        RETURN DISTINCT AVG(b.p) AS mean GROUP BY a ORDER BY a";
    let scalars = [CanonicalScalar::Int(1), CanonicalScalar::Int(3),
        CanonicalScalar::Int(2), CanonicalScalar::Null];
    let edges = [(VId(0), R, VId(10)), (VId(0), R, VId(11)), (VId(1), R, VId(12)),
        (VId(2), R, VId(13)), (VId(4), R, VId(10))];
    let rows = prepare(text).execute_governed(14,
        [VId(0), VId(1), VId(2), VId(3), VId(4), VId(10), VId(11), VId(12), VId(13)], edges,
        |vid, predicates| Ok::<_, ()>(predicates.is_empty() || vid.0 < 5),
        |vid, _| Ok(scalars.get((vid.0 - 10) as usize)), wide(), || Ok::<_, ()>(())).unwrap().value;
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.keys().is_empty()));
    let first = rows[0].get(0).unwrap().as_average().unwrap();
    assert_eq!((first.numerator(), first.denominator()), (2, 1));
    assert!(rows[1].get(0).unwrap().is_null());
    assert_eq!(rows[2].get(0).unwrap().as_average().unwrap().numerator(), 1);
}

#[test]
fn hidden_scalar_keys_use_declared_parameters_and_generated_names_cannot_capture_aliases() {
    let text = "MATCH (a) RETURN COUNT(*) AS n GROUP BY a.p HAVING a.p=$wanted ORDER BY a.p";
    let mut calls = 0;
    let template = PreparedGraphAggregateText::prepare_with_parameter_types(text,
        &[("wanted", GqlParameterType::Scalar(CanonicalScalarKind::Text))], |kind, name| {
            calls += 1; symbols(kind, name)
        }).unwrap();
    let scalars = [CanonicalScalar::ucs_basic_text("x").unwrap(), CanonicalScalar::ucs_basic_text("x").unwrap(),
        CanonicalScalar::ucs_basic_text("y").unwrap(), CanonicalScalar::Null];
    for (wanted, count) in [("x", 2), ("y", 1)] {
        let bound = template.bind_parameters(&GqlParameters::new().with_text("wanted", wanted).unwrap()).unwrap();
        let rows = bound.execute_governed(4, [VId(0), VId(1), VId(2), VId(3)], [],
            |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&scalars[vid.0 as usize])), wide(), || Ok::<_, ()>(())).unwrap().value;
        assert_eq!(rows.len(), 1); assert!(rows[0].keys().is_empty());
        assert_eq!(rows[0].get(0).unwrap().as_count(), Some(count));
    }
    assert_eq!(calls, 1);
    let query = prepare("MATCH (a) RETURN COUNT(*) AS __fgdb_group_0,COUNT(a) AS __fgdb_hidden_0 \
        GROUP BY a HAVING a IS NOT NULL ORDER BY MIN(a.p)");
    assert_eq!(query.evaluation_key_columns(), &["__fgdb_group_1"]);
    assert_eq!(query.input_pattern().columns(), &["__fgdb_group_1", "__fgdb_hidden_1"]);
    assert_eq!(query.aggregate_columns(), &["__fgdb_group_0", "__fgdb_hidden_0"]);
    let alias = prepare("MATCH (a) RETURN COUNT(*) AS a GROUP BY a HAVING a>0 ORDER BY a");
    assert_eq!(alias.having()[0].column, GraphAggregateColumn::Aggregate(0));
    assert_eq!(alias.ordering()[0].column, GraphAggregateColumn::Aggregate(0));
    let keyword = prepare("MATCH (true) RETURN COUNT(*) AS n GROUP BY true HAVING true IS NOT NULL ORDER BY true");
    assert_eq!(keyword.having()[0].column, GraphAggregateColumn::GroupKey(0));
}

#[test]
fn malformed_hidden_references_and_full_evaluation_width_refuse_before_catalog_reads() {
    for text in [
        "MATCH (a) RETURN COUNT(*) AS n GROUP BY a,a",
        "MATCH (a) RETURN a.p,COUNT(*) AS n GROUP BY a",
        "MATCH (a) RETURN COUNT(*) AS n GROUP BY a HAVING a.p>0",
        "MATCH (a) RETURN COUNT(*) AS n GROUP BY a.p ORDER BY a",
        "MATCH (a) RETURN COUNT(*) AS n GROUP BY a ORDER BY __fgdb_group_0",
        "MATCH (a) RETURN COUNT(*) AS n GROUP BY a HAVING b IS NOT NULL",
        "MATCH (a) RETURN a.p AS category,COUNT(*) AS n GROUP BY a.p,a ORDER BY category,a.p",
        "MATCH (a) RETURN COUNT(*) AS n GROUP BY a.p HAVING a.p>$x LIMIT $x",
    ] {
        let mut calls = 0;
        assert!(PreparedGraphAggregateText::prepare(text, |kind, name| {
            calls += 1; symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls, 0);
    }
    for width in [64, 65] {
        let keys = (0..width).map(|at| format!("a.p{}", at + 1)).collect::<Vec<_>>().join(",");
        let text = format!("MATCH (a) RETURN COUNT(*) AS n GROUP BY {keys}");
        let mut calls = 0;
        let result = PreparedGraphAggregateText::prepare(&text, |kind, name| {
            calls += 1;
            if kind == GraphSymbolKind::Property {
                Some(GraphSymbol::Property(PropertyKeyId(name.strip_prefix('p')?.parse().ok()?)))
            } else { symbols(kind, name) }
        });
        if width == 64 {
            let bound = result.unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            assert_eq!(bound.evaluation_key_columns().len(), 64);
            assert!(bound.key_columns().is_empty());
        } else { assert!(result.is_err()); assert_eq!(calls, 0); }
    }
}

#[test]
fn distinct_uses_exact_shared_budgets_and_every_interruption_checkpoint() {
    let query = prepare("MATCH (a) RETURN DISTINCT COUNT(*) AS n GROUP BY a.p ORDER BY a.p LIMIT 2");
    let values = [9, 9, 1, 2, 2].map(CanonicalScalar::Int);
    let run = |policy| query.execute_governed(5, [VId(0), VId(1), VId(2), VId(3), VId(4)], [],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[vid.0 as usize])), policy, || Ok::<_, usize>(()));
    let complete = run(wide()).unwrap();
    assert_eq!(complete.value.iter().map(|row| row.get(0).unwrap().as_count().unwrap()).collect::<Vec<_>>(), vec![1, 2]);
    let work = complete.evaluator.work_units; let scratch = complete.evaluator.scratch_entries;
    assert_eq!(run(GqlQueryPolicy::new(5, 2, work, scratch)).unwrap(), complete);
    for policy in [GqlQueryPolicy::new(4, 2, u64::MAX, u64::MAX), GqlQueryPolicy::new(5, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(5, 2, work - 1, u64::MAX), GqlQueryPolicy::new(5, 2, u64::MAX, scratch - 1)] {
        assert!(run(policy).is_err());
    }
    let mut total = 0;
    query.execute_governed(5, [VId(0), VId(1), VId(2), VId(3), VId(4)], [],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[vid.0 as usize])), wide(),
        || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = query.execute_governed(5, [VId(0), VId(1), VId(2), VId(3), VId(4)], [],
            |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[vid.0 as usize])), wide(),
            || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
    let query = prepare("MATCH (a) RETURN DISTINCT COUNT(*) AS n GROUP BY a.p HAVING a.p>0 LIMIT 0");
    let invalid = CanonicalScalar::Bool(true);
    let result = query.execute_governed(1, [VId(0)], [], |_, _| Ok::<_, ()>(true),
        |_, _| Ok(Some(&invalid)), wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::NonIntegerHaving { .. }))));
    let result = query.execute_governed(1, [VId(0)], [], |_, _| Ok::<_, &str>(true),
        |_, _| Err::<Option<&CanonicalScalar>, _>("hidden grouping read"), wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("hidden grouping read")))));
}
