//! Exact aggregate terminals consume the complete native multipart relation.
//! Small independent edge-bag enumeration supplies the result oracle; actual
//! source callbacks execute the governed GLA plans, never precomputed answers.

use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphAggregateBuildError,
    GraphAggregateError, GraphAggregateRow, GraphAggregateValue,
    GraphPipelineAggregateTextErrorKind, GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
    PreparedGraphPipelineAggregateText, PreparedGraphSetAggregate,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};
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
    let values = SCORES.map(|n| CanonicalPropertyValue::Scalar(CanonicalScalar::Int(n)));
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed_with_elements(
        policy,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                (values.len() + EDGES.len()) as u64,
                (1..=4).map(VId),
                EDGES,
                |vid, tests| {
                    Ok::<_, &'static str>(
                        tests.iter().all(|test| {
                            test.matches(&[], &[(P, values[vid.0 as usize - 1].clone())])
                        }),
                    )
                },
                |vid, key| Ok((key == P).then(|| &values[vid.0 as usize - 1])),
                remaining,
                || (checkpoint.borrow_mut())(),
            )
        },
        |_, _, _| panic!("this fixture has no CALL source"),
        |element, key| {
            Ok(match element {
                ElementId::Vertex(vid) if key == P => Some(values[vid.0 as usize - 1].clone()),
                _ => None,
            })
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
    let ratio = value
        .as_average()
        .expect("exact average, not integer or float");
    assert_eq!(
        ratio.numerator() * i128::from(count),
        sum * i128::from(ratio.denominator())
    );
}
fn assert_list(value: &GraphAggregateValue, expected: &[i64]) {
    let list = value.as_value().unwrap().as_list().unwrap();
    assert_eq!(
        list,
        expected.iter().copied().map(scalar).collect::<Vec<_>>()
    );
}

#[test]
fn carried_properties_group_after_optional_matching_and_nullable_required_carries() {
    let rows = execute(
        "MATCH (a) WITH a OPTIONAL MATCH (b) WHERE a.p < b.p \
                        RETURN a.p AS score,COUNT(*) AS rows,COUNT(b) AS present ORDER BY score",
    );
    assert_eq!(rows.len(), 3);
    for (row, (score, occurrences, present)) in rows.iter().zip([(1, 3, 3), (2, 2, 2), (3, 1, 0)]) {
        assert_eq!(row.keys(), &[scalar(score)]);
        assert_eq!(row.values()[0].as_count(), Some(occurrences));
        assert_eq!(row.values()[1].as_count(), Some(present));
    }
    let rows = execute(
        "MATCH (n) WHERE n.p=3 OPTIONAL MATCH (n)-[:R]->(a) WITH a \
                        MATCH (b) RETURN a.p AS score,COUNT(*) AS rows,COUNT(a.p) AS present",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].keys(), &[GraphValue::Scalar(CanonicalScalar::Null)]);
    assert_eq!(rows[0].values()[0].as_count(), Some(4));
    assert_eq!(rows[0].values()[1].as_count(), Some(0));
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
        expected
            .entry(source)
            .or_default()
            .push(SCORES[target.0 as usize - 1]);
    }
    assert_eq!(rows.len(), expected.len());
    for (row, (source, mut scores)) in rows.iter().zip(expected) {
        scores.sort();
        let unique: Vec<_> = scores
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
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
        let scores: Vec<_> = EDGES
            .iter()
            .filter(|(s, _, _)| *s == source)
            .map(|(_, _, target)| SCORES[target.0 as usize - 1])
            .collect();
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
        if sum.is_none() {
            assert!(rows[0].values()[1].is_null());
        }
    }
    let keyed = execute("MATCH (n) WITH n LIMIT 0 MATCH (n) WITH n RETURN n,COUNT(*) AS rows");
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
    let query = prepared
        .bind_relation_parameters(&GqlParameters::new())
        .unwrap();
    let result = run(&query, wide(), &mut || Ok(())).unwrap();
    assert_eq!(result.value[0].values()[0].as_count(), Some(4));
    assert_eq!(result.rows.snapshot_records, 18);
}

/// A required continuation's WHERE that the graph predicate refuses, here a
/// computed graph operand against an imported column, filters after an
/// implicit `WITH *` before an aggregate RETURN too (fgdb-c6hvm): the same
/// template and rows as the explicit `WITH *` spelling. An OPTIONAL
/// continuation's WHERE belongs to its left join: it filters inside an
/// optional Apply, before the null extension.
#[test]
fn an_aggregate_reads_a_refused_continuation_where_after_the_join() {
    let template = |text: &str| {
        PreparedGraphPipelineAggregateText::prepare(text, symbols)
            .unwrap()
            .canonical_template_bytes()
    };
    // SCORES are 1, 2, 2, 3: e = 2 meets two b rows from each of two a rows.
    for (implicit, explicit, expected) in [
        (
            "MATCH (a) WITH a.p AS e MATCH (b) WHERE b.p * 1 = e \
             RETURN e, COUNT(*) AS rows ORDER BY e",
            "MATCH (a) WITH a.p AS e MATCH (b) WITH * WHERE b.p * 1 = e \
             RETURN e, COUNT(*) AS rows ORDER BY e",
            &[(1, 1), (2, 4), (3, 1)][..],
        ),
        (
            "UNWIND [1, 3] AS w MATCH (b) WHERE b.p * 1 = w RETURN w, COUNT(*) AS rows ORDER BY w",
            "UNWIND [1, 3] AS w MATCH (b) WITH * WHERE b.p * 1 = w \
             RETURN w, COUNT(*) AS rows ORDER BY w",
            &[(1, 1), (3, 1)][..],
        ),
    ] {
        assert_eq!(template(implicit), template(explicit), "{implicit}");
        let rows = execute(implicit);
        assert_eq!(rows, execute(explicit), "{implicit}");
        let observed: Vec<_> = rows
            .iter()
            .map(|row| (row.keys().to_vec(), row.values()[0].as_count()))
            .collect();
        let expected: Vec<_> = expected
            .iter()
            .map(|&(key, count)| (vec![scalar(key)], Some(count)))
            .collect();
        assert_eq!(observed, expected, "{implicit}");
    }
    // An OPTIONAL clause's WHERE over a computed graph operand filters inside
    // an optional Apply: each incoming row counts its surviving matches, or
    // its one NULL extension.
    let rows = execute(
        "MATCH (a) WITH a.p AS e OPTIONAL MATCH (b) WHERE b.p * 1 = e RETURN COUNT(*) AS rows",
    );
    let expected: u64 = SCORES
        .iter()
        .map(|&e| SCORES.iter().filter(|&&p| p == e).count().max(1) as u64)
        .sum();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_count(), Some(expected));
}

#[test]
fn wide_sums_and_fractional_averages_never_narrow_to_scalar_integers() {
    let rows = execute(
        "MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) WITH m \
         RETURN SUM(9223372036854775807) AS total,AVG(9223372036854775807) AS mean",
    );
    assert_eq!(
        rows[0].values()[0].as_integer(),
        Some(i128::from(i64::MAX) * 3)
    );
    assert_ratio(&rows[0].values()[1], i128::from(i64::MAX) * 3, 3);
    let rows =
        execute("MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH m.p AS score RETURN AVG(score) AS mean");
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
        assert!(
            seen.insert((kind, name.to_owned())),
            "catalog resolution is shared"
        );
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(prepared.parameter_schema().len(), 5);
    let frozen = prepared.canonical_template_bytes();
    let args = GqlParameters::new()
        .with_int64("start", 1)
        .unwrap()
        .with_uint64("take", 1)
        .unwrap()
        .with_int64("add", 3)
        .unwrap()
        .with_int64("floor", 10)
        .unwrap();
    let missing = prepared.bind_relation_parameters(&args).unwrap_err();
    assert_eq!(missing.offset, text.find("$page").unwrap());
    let query = prepared
        .bind_relation_parameters(&args.with_uint64("page", 1).unwrap())
        .unwrap();
    let rows = run(&query, wide(), &mut || Ok(())).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].values()[0].as_integer(), Some(15));
    assert_eq!(prepared.canonical_template_bytes(), frozen);
    let inner = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH n,m RETURN n,COUNT(m) AS rows",
        symbols,
    )
    .unwrap();
    let outer = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH n,m RETURN n,COUNT(m) AS rows",
        symbols,
    )
    .unwrap();
    assert_ne!(
        inner.canonical_template_bytes(),
        outer.canonical_template_bytes()
    );
    let operators = outer.template_operators();
    assert_eq!(
        operators
            .iter()
            .filter(|op| **op == "ScanGraphText")
            .count(),
        2
    );
    assert!(
        operators.iter().position(|op| *op == "LeftJoin").unwrap()
            < operators.iter().position(|op| *op == "Aggregate").unwrap()
    );
}

#[test]
fn source_contract_cannot_silently_drop_a_later_graph_leaf() {
    let multiple = PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n MATCH (n) WITH n RETURN COUNT(*) AS rows",
        symbols,
    )
    .unwrap();
    assert!(matches!(
        multiple
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphPipelineAggregateTextErrorKind::Build(
            GraphAggregateBuildError::RequiresSingleGraphSource
        )
    ));
    for (text, sources) in [
        ("RETURN COUNT(*) AS rows", 0),
        ("MATCH (n) WITH n RETURN COUNT(*) AS rows", 1),
    ] {
        let template = PreparedGraphPipelineAggregateText::prepare(text, symbols).unwrap();
        assert_eq!(template.graph_source_count(), sources);
        assert_eq!(template.is_source_free(), sources == 0);
        assert_eq!(template.requires_relational_input(), sources != 1);
        if sources == 1 {
            assert_eq!(
                template
                    .bind_parameters(&GqlParameters::new())
                    .unwrap()
                    .canonical_bytes(),
                template
                    .bind_relation_parameters(&GqlParameters::new())
                    .unwrap()
                    .canonical_bytes()
            );
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
        let result = query.execute_governed(
            wide(),
            |_, _| {
                calls += 1;
                if calls == 1 {
                    Ok(GqlQueryExecution::<GraphValueRow> {
                        value: Vec::new(),
                        rows: fgdb_gql::GqlExecutionStats {
                            snapshot_records: 0,
                            result_rows: 0,
                        },
                        evaluator: fgdb_gql::GlaExecutionStats::default(),
                    })
                } else {
                    Err(GqlQueryError::Source("last-source"))
                }
            },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                GraphSetExecutionError::Source("last-source")
            )))
        ));
        assert_eq!(calls, 2);
    }
    let query = prepare("MATCH (n) WITH n MATCH (n) WITH n RETURN SUM(1/0) AS bad LIMIT 0");
    assert!(matches!(
        run(&query, wide(), &mut || Ok(())),
        Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
            GraphSetExecutionError::Projection { .. }
        )))
    ));
}

#[test]
fn exact_cumulative_quotas_and_every_cancellation_checkpoint_span_join_and_grouping() {
    let query = prepare(
        "MATCH (n) WITH n ORDER BY n LIMIT 1 MATCH (n)-[:R]->(m) \
         WITH m.p AS score RETURN SUM(score+1) AS total",
    );
    let mut checkpoints = 0;
    let measured = run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(measured.value[0].values()[0].as_integer(), Some(9));
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(())
            )
            .is_err()
        );
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

/// openCypher: RETURN after a MATCH reads every visible binding. A
/// continuation's aggregate RETURN therefore aggregates exactly what the same
/// RETURN after `WITH *` does; the first part still needs its WITH.
#[test]
fn a_continuation_aggregate_return_reads_its_bindings_through_the_implied_with_star() {
    for (implied, star) in [
        (
            "MATCH (n) WITH n MATCH (n) RETURN COUNT(*) AS rows",
            "MATCH (n) WITH n MATCH (n) WITH * RETURN COUNT(*) AS rows",
        ),
        (
            "MATCH (a) WITH a MATCH (b) WHERE a = b RETURN COUNT(b) AS c",
            "MATCH (a) WITH a MATCH (b) WHERE a = b WITH * RETURN COUNT(b) AS c",
        ),
        (
            "MATCH (n) WITH n MATCH (n)-[:R]->(m) RETURN n, COUNT(m) AS out ORDER BY n",
            "MATCH (n) WITH n MATCH (n)-[:R]->(m) WITH * RETURN n, COUNT(m) AS out ORDER BY n",
        ),
        (
            "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) RETURN n, COUNT(m) AS out ORDER BY n",
            "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH * RETURN n, COUNT(m) AS out ORDER BY n",
        ),
    ] {
        assert_eq!(execute(implied), execute(star), "{implied}");
    }
    for text in [
        "MATCH (n) WITH n MATCH (n) RETURN COUNT(*) AS rows",
        "MATCH (a) WITH a MATCH (b) WHERE a = b RETURN COUNT(b) AS c",
    ] {
        let rows = execute(text);
        assert_eq!(rows.len(), 1, "{text}");
        assert_eq!(rows[0].values()[0].as_count(), Some(4), "{text}");
    }
    // Out-degree per vertex from the independent edge bag; vertex 4 has none
    // and keeps its group through the OPTIONAL null extension.
    let degrees = execute(
        "MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) RETURN n, COUNT(m) AS out ORDER BY n",
    );
    assert_eq!(degrees.len(), 4);
    for (row, vid) in degrees.iter().zip(1..=4) {
        let out = EDGES
            .iter()
            .filter(|(start, _, _)| *start == VId(vid))
            .count();
        assert_eq!(row.keys(), &[GraphValue::Vertex(VId(vid))]);
        assert_eq!(row.values()[0].as_count(), Some(out as u64));
    }
    assert!(
        PreparedGraphPipelineAggregateText::prepare(
            "MATCH (n) RETURN COUNT(*) AS rows MATCH (m) RETURN COUNT(*) AS more",
            symbols
        )
        .is_err()
    );
}

/// A carried vertex's property read past a continuation's explicit WITH is
/// an element read: as a grouping key or in an aggregate argument it reads
/// the stored property, whether or not the continuation re-matched the
/// vertex by name (fgdb-x87sw).
#[test]
fn a_carried_vertex_property_feeds_an_aggregate_past_a_continuation_with() {
    let score = |vid: VId| SCORES[vid.0 as usize - 1];
    // (key, occurrences) in key order.
    let groups = |keys: Vec<i64>| {
        let mut groups: BTreeMap<i64, u64> = BTreeMap::new();
        for key in keys {
            *groups.entry(key).or_default() += 1;
        }
        groups.into_iter().collect::<Vec<_>>()
    };
    let counted = |text: &str| {
        execute(text)
            .iter()
            .map(|row| {
                let [GraphValue::Scalar(CanonicalScalar::Int(key))] = row.keys() else {
                    panic!("{text}: one integer key, got {:?}", row.keys());
                };
                (*key, row.values()[0].as_count().expect("a count"))
            })
            .collect::<Vec<_>>()
    };
    // Each a meets all four b.
    let pairs = groups((1..=4).flat_map(|a| [score(VId(a)); 4]).collect());
    for text in [
        "MATCH (a) WITH a MATCH (b) WITH a, b RETURN a.p AS score, COUNT(b) AS n ORDER BY score",
        "MATCH (a) WITH a MATCH (b) WITH b, a RETURN a.p AS score, COUNT(*) AS n ORDER BY score",
    ] {
        assert_eq!(counted(text), pairs, "{text}");
    }
    // One occurrence per out-edge of the re-matched a, with or without a
    // WHERE over an imported value, which filters after the implicit WITH *.
    let out = groups(EDGES.iter().map(|&(a, _, _)| score(a)).collect());
    for text in [
        "MATCH (a) WITH a MATCH (a)-[:R]->(c) WITH a, c \
         RETURN a.p AS score, COUNT(c) AS n ORDER BY score",
        "MATCH (a) WITH a, 1 AS k MATCH (a)-[:R]->(c) WHERE k + 0 > 0 \
         RETURN a.p AS score, COUNT(c) AS n ORDER BY score",
    ] {
        assert_eq!(counted(text), out, "{text}");
    }
    let high = groups(
        EDGES
            .iter()
            .filter(|&&(_, _, c)| score(c) == 3)
            .map(|&(a, _, _)| score(a))
            .collect(),
    );
    assert_eq!(
        counted(
            "MATCH (a) WITH a, [3] AS xs MATCH (a)-[:R]->(c) WHERE c.p IN xs \
             RETURN a.p AS score, COUNT(c) AS n ORDER BY score"
        ),
        high
    );
    // In an aggregate argument: the start scores summed per end score.
    let mut totals: BTreeMap<i64, i128> = BTreeMap::new();
    for &(a, _, c) in &EDGES {
        *totals.entry(score(c)).or_default() += i128::from(score(a));
    }
    let rows = execute(
        "MATCH (a) WITH a MATCH (a)-[:R]->(c) WITH a, c \
         RETURN c.p AS score, SUM(a.p) AS total ORDER BY score",
    );
    assert_eq!(rows.len(), totals.len());
    for (row, (key, total)) in rows.iter().zip(totals) {
        assert_eq!(row.keys(), &[scalar(key)]);
        assert_eq!(row.values()[0].as_integer(), Some(total));
    }
}

#[test]
fn illegal_aliases_aggregate_stages_and_total_depth_refuse_before_catalog() {
    // An aggregate WITH stage is legal since fgdb-ezgeq. It drops `n`, so the
    // second MATCH binds a fresh `n` over all four vertices of the one-row
    // stage: four rows, not a refusal.
    let staged =
        execute("MATCH (n) WITH COUNT(*) AS count MATCH (n) WITH n RETURN COUNT(*) AS rows");
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].values()[0].as_count(), Some(4));
    let carried =
        execute("MATCH (n) WITH n OPTIONAL MATCH (n)-[:R]->(m) WITH n.p AS kept RETURN SUM(kept)");
    assert_eq!(carried.len(), 1);
    assert_eq!(carried[0].values()[0].as_integer(), Some(10));
    for text in [
        "MATCH (n) WITH n MATCH (n) WITH n RETURN SUM(n) AS bad",
        "MATCH (n) WITH n MATCH (n) WITH n AS kept RETURN n,COUNT(*) AS rows",
        "MATCH (n) WITH n MATCH (n) WITH n RETURN COUNT(*) AS rows HAVING missing>0",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
    let text = format!(
        "MATCH (n){} WITH n RETURN COUNT(*) AS rows",
        " WITH n MATCH (n)".repeat(fgdb_gql::MAX_GRAPH_SET_DEPTH)
    );
    let mut calls = 0;
    assert!(
        PreparedGraphPipelineAggregateText::prepare(&text, |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls, 0);
}

/// UNWIND range() streams its members (fgdb-85aal). Past the 65,536-member
/// list bound the materialized list refuses with Overflow, so these counts
/// can only come from the streamed range: the counting fold and the row
/// executor both read it. Below the bound it equals the materialized list,
/// and a materialized range is still bounded.
/// An OPTIONAL MATCH whose WHERE reads the incoming row counts each row's
/// surviving matches, and zero where only the NULL extension stands
/// (fgdb-2ht8c, LDBC SNB IC5).
#[test]
fn an_optional_match_where_over_imported_values_counts_surviving_matches() {
    let score = |vid: VId| SCORES[vid.0 as usize - 1];
    let rows = execute(
        "MATCH (s) WHERE s.p = 2 WITH collect(s) AS ss MATCH (a) WITH a, ss \
         OPTIONAL MATCH (a)-[:R]->(b) WHERE b IN ss RETURN a, COUNT(b) AS n ORDER BY a",
    );
    assert_eq!(rows.len(), 4);
    for (row, a) in rows.iter().zip(1..=4) {
        let surviving = EDGES
            .iter()
            .filter(|&&(start, _, end)| start == VId(a) && score(end) == 2)
            .count();
        assert_eq!(row.keys(), &[GraphValue::Vertex(VId(a))]);
        assert_eq!(row.values()[0].as_count(), Some(surviving as u64));
    }
}

#[test]
fn unwind_range_streams_past_the_list_value_bound() {
    let rows = execute("UNWIND range(1, 70000) AS i RETURN COUNT(*) AS rows");
    assert_eq!(rows[0].values()[0].as_count(), Some(70_000));
    let rows = execute("UNWIND range(1, 70000) AS i RETURN COUNT(*) AS rows, SUM(i) AS total");
    assert_eq!(rows[0].values()[0].as_count(), Some(70_000));
    assert_eq!(rows[0].values()[1].as_integer(), Some(70_000 * 70_001 / 2));
    // Every third member of a descending range, then a filter stage:
    // 100000, 99997, ..., 1 is 33,334 members, the even ones at even steps.
    let rows = execute(
        "UNWIND range(100000, 1, -3) AS i WITH i WHERE i % 2 = 0 \
         RETURN COUNT(*) AS rows, MIN(i) AS low, MAX(i) AS high",
    );
    assert_eq!(rows[0].values()[0].as_count(), Some(16_667));
    assert_eq!(rows[0].values()[1].as_value(), Some(&scalar(4)));
    assert_eq!(rows[0].values()[2].as_value(), Some(&scalar(100_000)));
    // range() bounds are row values: each source row unwinds its own range.
    let rows = execute("MATCH (n) UNWIND range(1, n.p * 30000) AS i RETURN COUNT(*) AS rows");
    assert_eq!(
        rows[0].values()[0].as_count(),
        Some(SCORES.iter().map(|p| *p as u64 * 30_000).sum())
    );
    for (streamed, listed) in [
        (
            "UNWIND range(-7, 60000, 13) AS i RETURN COUNT(*) AS rows, SUM(i) AS total, MIN(i) AS low",
            "UNWIND [x IN range(-7, 60000, 13) | x] AS i \
             RETURN COUNT(*) AS rows, SUM(i) AS total, MIN(i) AS low",
        ),
        (
            "UNWIND range(5, 1) AS i RETURN COUNT(*) AS rows, SUM(i) AS total",
            "UNWIND [] AS i RETURN COUNT(*) AS rows, SUM(i) AS total",
        ),
    ] {
        assert_eq!(execute(streamed), execute(listed), "{streamed}");
    }
    // A NULL bound makes range() NULL, which unwinds nothing.
    let rows = execute("UNWIND range(1, NULL) AS i RETURN COUNT(*) AS rows");
    assert_eq!(rows[0].values()[0].as_count(), Some(0));
    for text in [
        "UNWIND range(1, 70000, 0) AS i RETURN COUNT(*) AS rows",
        "UNWIND range(1, 70000, 0) AS i RETURN SUM(i) AS total",
    ] {
        let result = run(&prepare(text), wide(), &mut || Ok(()));
        assert!(
            matches!(
                &result,
                Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                    GraphSetExecutionError::Projection { error, .. }
                ))) if error.kind == fgdb_gql::GraphIntegerErrorKind::InvalidRangeStep
            ),
            "{text}: {result:?}"
        );
    }
    let result = run(
        &prepare("UNWIND [range(1, 70000)] AS l RETURN COUNT(*) AS rows"),
        wide(),
        &mut || Ok(()),
    );
    assert!(
        matches!(
            &result,
            Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                GraphSetExecutionError::Projection { error, .. }
            ))) if error.kind == fgdb_gql::GraphIntegerErrorKind::Overflow
        ),
        "the list bound still caps a materialized range: {result:?}"
    );
}

#[test]
fn carried_existence_values_group_without_multiplying_outer_occurrences() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
                  UNWIND [0,0] AS occurrence";
    for (name, grouping) in [
        ("outgoing", ""),
        ("outgoing", " GROUP BY outgoing"),
        ("__fg_boundary_0", " GROUP BY __fg_boundary_0"),
    ] {
        let rows = execute(&format!(
            "{prefix} RETURN EXISTS((x)-[:R]->()) AS {name}, \
             count(*) AS rows,count(DISTINCT x) AS vertices, \
             sum(CASE WHEN EXISTS((x)<-[:R]-()) THEN 1 ELSE 0 END) AS incoming \
             {grouping} ORDER BY {name}"
        ));
        assert_eq!(rows.len(), 2);
        for (row, (outgoing, count, vertices, incoming)) in
            rows.iter().zip([(false, 2, 1, 2), (true, 6, 3, 4)])
        {
            assert_eq!(
                row.keys(),
                &[GraphValue::Scalar(CanonicalScalar::Bool(outgoing))]
            );
            assert_eq!(row.values()[0].as_count(), Some(count));
            assert_eq!(row.values()[1].as_count(), Some(vertices));
            assert_eq!(row.values()[2].as_integer(), Some(incoming));
        }
    }
}

#[test]
fn carried_existence_aggregate_arguments_rebind_any_values_and_empty_inputs() {
    let text = "UNWIND $items AS x \
        RETURN count(*) AS rows,count(EXISTS((x)-[:R]->())) AS predicates, \
        sum(CASE WHEN EXISTS((x)-[:R]->()) THEN 1 ELSE 0 END) AS linked, \
        collect(EXISTS((x)-[:R]->())) AS flags";
    let mut seen = BTreeSet::new();
    let template = PreparedGraphPipelineAggregateText::prepare(text, |kind, name| {
        assert!(
            seen.insert((kind, name.to_owned())),
            "every probe shares the statement's resolver cache"
        );
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(template.graph_source_count(), 3);
    assert!(template.requires_relational_input());
    let frozen = template.canonical_template_bytes();
    for items in [
        vec![
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(4)),
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(1)),
            GraphValue::Edge(fgdb_types::EId(1)),
            GraphValue::List(vec![].into()),
        ],
        vec![GraphValue::Vertex(VId(2)), GraphValue::Vertex(VId(3))],
        vec![],
    ] {
        let mut expected: Vec<_> = items
            .iter()
            .map(|item| {
                GraphValue::Scalar(CanonicalScalar::Bool(matches!(
                    item,
                    GraphValue::Vertex(id) if EDGES.iter().any(|&(from, _, _)| from == *id)
                )))
            })
            .collect();
        let linked = expected
            .iter()
            .filter(|value| **value == GraphValue::Scalar(CanonicalScalar::Bool(true)))
            .count();
        let arguments = GqlParameters::new()
            .with_list("items", items.clone())
            .unwrap();
        let query = template.bind_relation_parameters(&arguments).unwrap();
        let execution = run(&query, wide(), &mut || Ok(())).unwrap();
        assert_eq!(execution.value.len(), 1);
        let row = &execution.value[0];
        assert!(row.keys().is_empty());
        assert_eq!(row.values()[0].as_count(), Some(items.len() as u64));
        assert_eq!(row.values()[1].as_count(), Some(items.len() as u64));
        if items.is_empty() {
            assert!(row.values()[2].is_null());
        } else {
            assert_eq!(row.values()[2].as_integer(), Some(linked as i128));
        }
        let mut actual = row.values()[3]
            .as_value()
            .unwrap()
            .as_list()
            .unwrap()
            .to_vec();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }
    assert_eq!(template.canonical_template_bytes(), frozen);
    assert!(
        template
            .bind_relation_parameters(&GqlParameters::new())
            .is_err()
    );
}

#[test]
fn inline_call_aggregates_consume_carried_existence_slots_inside_their_own_scope() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    let rows = execute(&format!(
        "{prefix} CALL (x) {{ RETURN collect(EXISTS((x)-[:R]->())) AS flags }} \
         RETURN x,count(*) AS rows,collect(flags) AS flags ORDER BY x"
    ));
    assert_eq!(rows.len(), 4);
    for (row, id) in rows.iter().zip(1..=4) {
        assert_eq!(row.keys(), &[GraphValue::Vertex(VId(id))]);
        assert_eq!(row.values()[0].as_count(), Some(1));
        let expected = GraphValue::List(
            vec![GraphValue::Scalar(CanonicalScalar::Bool(
                EDGES.iter().any(|&(from, _, _)| from == VId(id)),
            ))]
            .into(),
        );
        assert_eq!(
            row.values()[1].as_value().unwrap().as_list().unwrap(),
            &[expected]
        );
    }
    let text = format!(
        "{prefix} CALL (x) {{ \
         RETURN collect(EXISTS((x)-[:R]->())) AS flags,count(*) + __fg_boundary_0 AS leaked \
         }} RETURN count(*) AS rows"
    );
    let mut calls = 0;
    assert!(
        PreparedGraphPipelineAggregateText::prepare(&text, |kind, name| {
            calls += 1;
            symbols(kind, name)
        })
        .is_err()
    );
    assert_eq!(calls, 0);
}

#[test]
fn carried_pattern_counts_feed_exact_aggregates_and_group_by_witness_multiplicity() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
                  UNWIND [0,0] AS occurrence";
    let rows = execute(&format!(
        "{prefix} RETURN sum(COUNT {{ MATCH (x)-[:R]->() }}) AS witnesses, \
         count(COUNT {{ MATCH (x)-[:R]->() }}) AS inputs, \
         sum(CASE WHEN EXISTS((x)-[:R]->()) THEN 1 ELSE 0 END) AS linked"
    ));
    assert_eq!(rows.len(), 1);
    assert!(rows[0].keys().is_empty());
    assert_eq!(rows[0].values()[0].as_integer(), Some(10));
    assert_eq!(rows[0].values()[1].as_count(), Some(8));
    assert_eq!(rows[0].values()[2].as_integer(), Some(6));

    let rows = execute(&format!(
        "{prefix} RETURN COUNT {{ MATCH (x)-[:R]->() }} AS fanout, \
         count(*) AS occurrences,collect(x) AS vertices ORDER BY fanout"
    ));
    let expected = [
        (0, vec![VId(4), VId(4)]),
        (1, vec![VId(2), VId(2), VId(3), VId(3)]),
        (3, vec![VId(1), VId(1)]),
    ];
    assert_eq!(rows.len(), expected.len());
    for (row, (fanout, vertices)) in rows.iter().zip(expected) {
        assert_eq!(row.keys(), &[scalar(fanout)]);
        assert_eq!(row.values()[0].as_count(), Some(vertices.len() as u64));
        let mut actual = row.values()[1]
            .as_value()
            .unwrap()
            .as_list()
            .unwrap()
            .to_vec();
        actual.sort();
        assert_eq!(
            actual,
            vertices
                .into_iter()
                .map(GraphValue::Vertex)
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn carried_existence_aggregate_scopes_refuse_private_and_local_bindings_before_catalog() {
    let prefix = "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x";
    for tail in [
        "RETURN EXISTS((x)-[:R]->()) AS linked,__fg_boundary_0,count(*) AS rows",
        "RETURN EXISTS((x)-[:R]->()) AS linked,count(*) + __fg_boundary_0 AS leaked",
        "RETURN EXISTS((x)-[:R]->()) AS linked,count(*) AS rows ORDER BY __fg_boundary_0",
        "RETURN EXISTS((x)-[:R]->()) AS linked,count(*) AS rows ORDER BY count(__fg_boundary_0)",
        "RETURN EXISTS((x)-[:R]->(inside)) AS linked,count(inside) AS leaked",
        "RETURN count(EXISTS((fresh)-[:R]->())) AS unanchored",
        "RETURN count(ANY(local IN nodes WHERE EXISTS((local)-[:R]->()))) AS nested",
        "RETURN count(*) AS rows GROUP BY EXISTS((x)-[:R]->())",
        "RETURN count(*) AS rows HAVING EXISTS((x)-[:R]->())",
    ] {
        let text = format!("{prefix} {tail}");
        let mut calls = 0;
        assert!(
            PreparedGraphPipelineAggregateText::prepare(&text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0, "{text}");
    }
}

#[test]
fn carried_existence_aggregate_sources_share_budgets_and_fail_before_empty_results() {
    let template = PreparedGraphPipelineAggregateText::prepare(
        "UNWIND $items AS x \
         RETURN count(CASE WHEN false THEN EXISTS((x)-[:R]->()) ELSE true END) AS rows \
         LIMIT 0",
        symbols,
    )
    .unwrap();
    for items in [vec![], vec![GraphValue::Vertex(VId(1))]] {
        let empty = items.is_empty();
        let arguments = GqlParameters::new().with_list("items", items).unwrap();
        let query = template.bind_relation_parameters(&arguments).unwrap();
        let mut calls = 0;
        let result = query.execute_governed(
            wide(),
            |_, _| {
                calls += 1;
                Err(GqlQueryError::Source("aggregate-probe"))
            },
            || Ok::<_, usize>(()),
        );
        if empty {
            // A correlated Apply has no body invocation without an outer row.
            assert!(result.unwrap().value.is_empty());
            assert_eq!(calls, 0);
        } else {
            // LIMIT 0 cannot suppress a source failure for an existing row.
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphAggregateError::InputRelation(
                    GraphSetExecutionError::Source("aggregate-probe")
                )))
            ));
            assert_eq!(calls, 1);
        }
    }

    let query = prepare(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
         RETURN EXISTS((x)-[:R]->()) AS linked,count(*) AS rows ORDER BY linked",
    );
    let mut checkpoints = 0;
    let measured = run(&query, wide(), &mut || {
        checkpoints += 1;
        Ok(())
    })
    .unwrap();
    let caps = [
        measured.rows.snapshot_records,
        measured.rows.result_rows,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    ];
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
            &mut || Ok(())
        )
        .unwrap(),
        measured
    );
    for dimension in 0..4 {
        let mut limits = caps;
        assert!(limits[dimension] > 0);
        limits[dimension] -= 1;
        assert!(
            run(
                &query,
                GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3]),
                &mut || Ok(())
            )
            .is_err()
        );
    }
    assert!(checkpoints > 2);
    for stop in [1, checkpoints / 2, checkpoints] {
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
fn carried_pattern_comprehensions_keep_null_witnesses_through_aggregate_arguments() {
    let rows = execute(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
         UNWIND [0,0] AS occurrence \
         RETURN sum(size([(x)-[:R]->(t) | NULL])) AS witnesses, \
         count([(x)-[:R]->(t) | t]) AS lists, \
         sum(size([(x)-[:R]->(t) WHERE t.p >= 3 | t.p])) AS high",
    );
    assert_eq!(rows.len(), 1);
    assert!(rows[0].keys().is_empty());
    assert_eq!(rows[0].values()[0].as_integer(), Some(10));
    assert_eq!(rows[0].values()[1].as_count(), Some(8));
    assert_eq!(rows[0].values()[2].as_integer(), Some(2));

    let rows = execute(
        "MATCH (n) WITH collect(n) AS nodes UNWIND nodes AS x \
         CALL (x) { RETURN sum(size([(x)-[:R]->(t) | NULL])) AS degree } \
         RETURN x,sum(degree) AS total ORDER BY x",
    );
    assert_eq!(rows.len(), 4);
    for (row, (id, degree)) in rows.iter().zip([(1, 3), (2, 1), (3, 1), (4, 0)]) {
        assert_eq!(row.keys(), &[GraphValue::Vertex(VId(id))]);
        assert_eq!(row.values()[0].as_integer(), Some(degree));
    }
}

#[test]
fn list_local_graph_expressions_feed_exact_aggregates_and_inline_calls() {
    let rows = execute(
        "MATCH (n) WITH collect(n) AS nodes UNWIND [0,0] AS occurrence \
         RETURN sum(size([x IN nodes WHERE (x)-[:R]->() | NULL])) AS active, \
         count([x IN nodes | CASE WHEN (x)-[:R]->() THEN null ELSE 9 END]) AS lists",
    );
    assert_eq!(rows.len(), 1);
    assert!(rows[0].keys().is_empty());
    assert_eq!(rows[0].values()[0].as_integer(), Some(6));
    assert_eq!(rows[0].values()[1].as_count(), Some(2));

    let rows = execute(
        "MATCH (n) WITH collect(n) AS nodes UNWIND [0,0] AS occurrence \
         CALL (nodes) { \
           RETURN [x IN nodes | [(x)-[:R]->(t) | NULL]] AS nested \
         } RETURN count(nested) AS lists,sum(size(nested)) AS groups",
    );
    assert_eq!(rows.len(), 1);
    assert!(rows[0].keys().is_empty());
    assert_eq!(rows[0].values()[0].as_count(), Some(2));
    assert_eq!(rows[0].values()[1].as_integer(), Some(8));
}
