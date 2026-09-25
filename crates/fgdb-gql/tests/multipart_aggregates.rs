//! Exact aggregate terminals consume the complete native multipart relation.
//! Small independent edge-bag enumeration supplies the result oracle; actual
//! source callbacks execute the governed GLA plans, never precomputed answers.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateBuildError, GraphAggregateError, GraphAggregateRow, GraphAggregateValue,
    GraphPipelineAggregateTextErrorKind, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphPipelineAggregateText, PreparedGraphSetAggregate,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

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
type Fault = GqlQueryError<GraphAggregateError<&'static str>, usize>;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn prepare(text: &str) -> PreparedGraphSetAggregate {
    PreparedGraphPipelineAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap()
}
fn run(
    query: &PreparedGraphSetAggregate,
    policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>,
) -> Result<GqlQueryExecution<GraphAggregateRow>, Fault> {
    let values = SCORES.map(CanonicalScalar::Int);
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(
        policy,
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
                || (checkpoint.borrow_mut())(),
            )
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn execute(text: &str) -> Vec<GraphAggregateRow> {
    run(&prepare(text), wide(), &mut || Ok(())).unwrap().value
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn assert_ratio(value: &GraphAggregateValue, sum: i128, count: u64) {
    let ratio = value.as_average().expect("exact average, not integer or float");
    assert_eq!(ratio.numerator() * i128::from(count), sum * i128::from(ratio.denominator()));
}
fn assert_list(value: &GraphAggregateValue, expected: &[i64]) {
    let list = value.as_value().unwrap().as_list().unwrap();
    assert_eq!(list, expected.iter().copied().map(scalar).collect::<Vec<_>>());
}

#[test]
fn three_part_path_counts_match_independent_multigraph_enumeration() {
    let query = prepare(
        "MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH n,m \
         MATCH (m)-[:R]->(destination) WITH n,destination \
         RETURN n,COUNT(*) AS paths,COUNT(DISTINCT destination) AS endpoints ORDER BY n",
    );
    let mut expected: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
    for &(start, _, middle) in &EDGES {
        for &(next, _, end) in &EDGES {
            if next == middle {
                expected.entry(start).or_default().push(end);
            }
        }
    }
    let result = run(&query, wide(), &mut || Ok(())).unwrap();
    assert_eq!(query.input().operand_count(), 3);
    assert_eq!(result.rows.snapshot_records, 27);
    assert_eq!(result.value.len(), expected.len());
    for (row, (start, destinations)) in result.value.iter().zip(expected) {
        assert_eq!(row.keys(), &[GraphValue::Vertex(start)]);
        assert_eq!(row.values()[0].as_count(), Some(destinations.len() as u64));
        let unique: BTreeSet<_> = destinations.into_iter().collect();
        assert_eq!(row.values()[1].as_count(), Some(unique.len() as u64));
    }
}

#[test]
fn all_eleven_functions_aggregate_the_joined_bag_not_the_first_source() {
    let rows = execute(
        "MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH n,m.p AS score \
         RETURN n,COUNT(*) AS rows,COUNT(score) AS present,COUNT(DISTINCT score) AS distincts,\
         SUM(score) AS total,SUM(DISTINCT score) AS distinct_total,\
         AVG(score) AS mean,AVG(DISTINCT score) AS distinct_mean,\
         MIN(score) AS smallest,MAX(score) AS largest,\
         COLLECT(score) AS items,COLLECT(DISTINCT score) AS unique_items ORDER BY n",
    );
    let mut expected: BTreeMap<VId, Vec<i64>> = BTreeMap::new();
    for &(source, _, target) in &EDGES {
        expected.entry(source).or_default().push(SCORES[target.0 as usize - 1]);
    }
    assert_eq!(rows.len(), expected.len());
    for (row, (source, mut scores)) in rows.iter().zip(expected) {
        scores.sort();
        let unique: Vec<_> = scores.iter().copied().collect::<BTreeSet<_>>().into_iter().collect();
        let total: i128 = scores.iter().copied().map(i128::from).sum();
        let distinct_total: i128 = unique.iter().copied().map(i128::from).sum();
        let values = row.values();
        assert_eq!(row.keys(), &[GraphValue::Vertex(source)]);
        assert_eq!(values.len(), 11);
        assert_eq!(values[0].as_count(), Some(scores.len() as u64));
        assert_eq!(values[1].as_count(), Some(scores.len() as u64));
        assert_eq!(values[2].as_count(), Some(unique.len() as u64));
        assert_eq!(values[3].as_integer(), Some(total));
        assert_eq!(values[4].as_integer(), Some(distinct_total));
        assert_ratio(&values[5], total, scores.len() as u64);
        assert_ratio(&values[6], distinct_total, unique.len() as u64);
        assert_eq!(values[7].as_value(), Some(&scalar(scores[0])));
        assert_eq!(values[8].as_value(), Some(&scalar(*scores.last().unwrap())));
        assert_list(&values[9], &scores);
        assert_list(&values[10], &unique);
    }
}

#[test]
fn optional_null_extension_precedes_count_and_computed_aggregate_arguments() {
    let rows = execute(
        "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH n,m,m.p AS score \
         RETURN n,COUNT(*) AS rows,COUNT(m) AS present,SUM(score) AS total,\
         SUM(COALESCE(score,7)) AS filled,COLLECT(score) AS items ORDER BY n",
    );
    assert_eq!(rows.len(), 4);
    for (index, row) in rows.iter().enumerate() {
        let source = VId(index as u128 + 1);
        let scores: Vec<_> = EDGES.iter().filter(|(s, _, _)| *s == source)
            .map(|(_, _, target)| SCORES[target.0 as usize - 1]).collect();
        assert_eq!(row.keys(), &[GraphValue::Vertex(source)]);
        assert_eq!(row.values()[0].as_count(), Some(scores.len().max(1) as u64));
        assert_eq!(row.values()[1].as_count(), Some(scores.len() as u64));
        if scores.is_empty() {
            assert!(row.values()[2].is_null());
            assert_eq!(row.values()[3].as_integer(), Some(7));
        } else {
            let total: i128 = scores.iter().copied().map(i128::from).sum();
            assert_eq!(row.values()[2].as_integer(), Some(total));
            assert_eq!(row.values()[3].as_integer(), Some(total));
        }
        assert_list(&row.values()[4], &scores);
    }
}

#[test]
fn input_pages_distinct_and_filters_finish_before_grouping() {
    for (tail, count, sum) in [
        ("WITH m.p AS score", 3, Some(6)),
        ("WITH DISTINCT m.p AS score", 1, Some(2)),
        ("WITH m.p AS score ORDER BY score LIMIT 1", 1, Some(2)),
        ("WITH m.p AS score WHERE score>9", 0, None),
    ] {
        let text = format!(
            "MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) \
             {tail} RETURN COUNT(*) AS rows,SUM(score) AS total"
        );
        let rows = execute(&text);
        assert_eq!(rows.len(), 1, "keyless empty input still has one group");
        assert_eq!(rows[0].values()[0].as_count(), Some(count));
        assert_eq!(rows[0].values()[1].as_integer(), sum);
        if sum.is_none() { assert!(rows[0].values()[1].is_null()); }
    }
    let keyed = execute(
        "MATCH (n) WITH n LIMIT 0 MATCH (n) WITH n RETURN n,COUNT(*) AS rows",
    );
    assert!(keyed.is_empty());
}

#[test]
fn candidate_input_counts_only_real_graph_sources_and_preserves_occurrences() {
    let text = "UNWIND [2,2,NULL] AS wanted WITH wanted MATCH (n {p:wanted}) \
                WITH n MATCH (n)-[:R]->(m) WITH m RETURN COUNT(*) AS rows";
    let prepared = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
    assert_eq!(prepared.graph_source_count(), 2);
    assert!(!prepared.is_source_free());
    assert!(prepared.requires_relational_input());
    let query = prepared.bind_relation_parameters(&GqlParameters::new()).unwrap();
    let result = run(&query, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(4));
    assert_eq!(result.rows.snapshot_records, 18);
}

#[test]
fn wide_sums_and_fractional_averages_never_narrow_to_scalar_integers() {
    let rows = execute(
        "MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) WITH m \
         RETURN SUM(9223372036854775807) AS total,AVG(9223372036854775807) AS mean",
    );
    assert_eq!(rows[0].values()[0].as_integer(), Some(i128::from(i64::MAX) * 3));
    assert_ratio(&rows[0].values()[1], i128::from(i64::MAX) * 3, 3);
    let rows = execute(
        "MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH m.p AS score RETURN AVG(score) AS mean",
    );
    let ratio = rows[0].values()[0].as_average().unwrap();
    assert_eq!((ratio.numerator(), ratio.denominator()), (11, 5));
}

#[test]
fn parameters_catalog_and_template_identity_cover_all_parts_and_final_clauses() {
    let text = "\u{2003}MATCH (n) WHERE n.p=$start WITH n LIMIT $take \
        MATCH (n)-[:R]->(m) WITH m.p AS score \
        RETURN SUM(score+$add) AS total HAVING total>$floor LIMIT $page";
    let mut seen = BTreeSet::new();
    let prepared = PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
        assert!(seen.insert((kind, name.to_owned())), "catalog resolution is shared");
        symbols(kind, name)
    }).unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(prepared.parameter_schema().len(), 5);
    let frozen = prepared.canonical_template_bytes();
    let args = GqlParameters::new().with_int64("start", 1).unwrap()
        .with_uint64("take", 1).unwrap().with_int64("add", 3).unwrap()
        .with_int64("floor", 10).unwrap();
    let missing = prepared.bind_relation_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$page").unwrap());
    let query = prepared.bind_relation_parameters(&args.with_uint64("page", 1).unwrap()).unwrap();
    let rows = run(&query, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_integer(), Some(15));
    assert_eq!(prepared.canonical_template_bytes(), frozen);
    let inner = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH n,m RETURN n,COUNT(m) AS rows", symbols,
    ).unwrap();
    let outer = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH n,m RETURN n,COUNT(m) AS rows", symbols,
    ).unwrap();
    assert_ne!(inner.canonical_template_bytes(), outer.canonical_template_bytes());
    let operators = outer.template_operators();
    assert_eq!(operators.iter().filter(|op| **op == "ScanGraphText").count(), 2);
    assert!(operators.iter().position(|op| *op == "LeftJoin").unwrap()
        < operators.iter().position(|op| *op == "Aggregate").unwrap());
}

#[test]
fn source_contract_cannot_silently_drop_a_later_graph_leaf() {
    let multiple = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n MATCH (n) WITH n RETURN COUNT(*) AS rows", symbols,
    ).unwrap();
    assert!(matches!(multiple.bind_parameters(&GqlParameters::new()).unwrap_err().kind,
        GraphPipelineAggregateTextErrorKind::Build(GraphAggregateBuildError::RequiresSingleGraphSource)));
    for (text, sources) in [
        ("RETURN COUNT(*) AS rows", 0),
        ("MATCH (n) WITH n RETURN COUNT(*) AS rows", 1),
    ] {
        let template = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
        assert_eq!(template.graph_source_count(), sources);
        assert_eq!(template.is_source_free(), sources == 0);
        assert_eq!(template.requires_relational_input(), sources != 1);
        if sources == 1 {
            assert_eq!(template.bind_parameters(&GqlParameters::new()).unwrap().canonical_bytes(),
                template.bind_relation_parameters(&GqlParameters::new()).unwrap().canonical_bytes());
        }
    }
}

#[test]
fn late_source_errors_and_limit_zero_do_not_return_a_partial_summary() {
    for text in [
        "MATCH (n) WITH n LIMIT 0 MATCH (n) WITH n RETURN COUNT(*) AS rows",
        "MATCH (n) WITH n MATCH (n) WITH n RETURN COUNT(*) AS rows LIMIT 0",
    ] {
        let query = prepare(text);
        let mut calls = 0;
        let result = query.execute_governed(wide(), |_, _| {
            calls += 1;
            if calls == 1 {
                Ok(GqlQueryExecution::<GraphValueRow> {
                    value: Vec::new(),
                    rows: fgdb_gql::GqlExecutionStats { snapshot_records: 0, result_rows: 0 },
                    evaluator: fgdb_gql::GlaExecutionStats::default(),
                })
            } else { Err(GqlQueryError::Source("last-source")) }
        }, || Ok::<_, usize>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Source("last-source"))))));
        assert_eq!(calls, 2);
    }
    let query = prepare(
        "MATCH (n) WITH n MATCH (n) WITH n RETURN SUM(1/0) AS bad LIMIT 0",
    );
    assert!(matches!(run(&query, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Projection { .. })))));
}

#[test]
fn exact_cumulative_quotas_and_every_cancellation_checkpoint_span_join_and_grouping() {
    let query = prepare(
        "MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) \
         WITH m.p AS score RETURN SUM(score+1) AS total",
    );
    let mut checkpoints = 0;
    let measured = run(&query, wide(), &mut || { checkpoints += 1; Ok(()) }).unwrap();
    let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
        measured.evaluator.work_units, measured.evaluator.scratch_entries];
    assert_eq!(measured.value[0].values()[0].as_integer(), Some(9));
    assert_eq!(run(&query, GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
        &mut || Ok(())).unwrap(), measured);
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(run(&query, GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
            &mut || Ok(())).is_err());
    }
    for stop in 1..=checkpoints {
        let mut seen = 0;
        let result = run(&query, wide(), &mut || {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn illegal_aliases_aggregate_stages_and_total_depth_refuse_before_catalog() {
    for text in [
        "MATCH (n) WITH n MATCH (n) RETURN COUNT(*) AS rows",
        "MATCH (n) WITH n MATCH (n) WITH n RETURN SUM(n) AS bad",
        "MATCH (n) WITH n MATCH (n) WITH n AS kept RETURN n,COUNT(*) AS rows",
        "MATCH (n) WITH COUNT(*) AS count MATCH (n) WITH n RETURN COUNT(*) AS rows",
        "MATCH (n) WITH n MATCH (n) WITH n RETURN COUNT(*) AS rows HAVING missing>0",
        "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH n.p AS bad RETURN SUM(bad)",
    ] {
        let mut calls = 0;
        assert!(PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
            calls += 1; symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls, 0, "{text}");
    }
    let text = format!("MATCH (n){} WITH n RETURN COUNT(*) AS rows",
        " WITH n MATCH (n)".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH));
    let mut calls = 0;
    assert!(PreparedGraphPipelineAggregateText::prepare(&text, |kind, name| {
        calls += 1; symbols(kind, name)
    }).is_err());
    assert_eq!(calls, 0);
}
