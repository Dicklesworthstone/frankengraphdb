//! Exact bag-weight laws against actual edge occurrence assignments.
//! The oracle does not normalize topology or execute another compiled query.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateError, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

type Edge = (VId, RelationId, VId);
type Atom = (usize, u64, u8, usize);
type Summary = (VId, u64, u64, u64, Option<VId>, Option<VId>);

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
    PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000) }
fn run(query: &PreparedGraphAggregate, edges: &[Edge])
    -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<()>, usize>> {
    query.execute_governed(edges.len() as u64, [], edges.iter().copied(),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, usize>(()))
}
fn vertex(row: &GraphAggregateRow, at: usize) -> Option<VId> {
    row.get(at).unwrap().as_value().unwrap().as_vertex()
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Summary> {
    rows.iter().map(|row| (row.keys()[0].as_vertex().unwrap(),
        row.get(0).unwrap().as_count().unwrap(), row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_count().unwrap(), vertex(row, 3), vertex(row, 4))).collect()
}
fn oracle(edges: &[Edge], atoms: &[Atom], variables: usize) -> Vec<Summary> {
    let mut occurrences = Vec::new();
    for bits in 0..(1 << variables) {
        let assignment: Vec<_> = (0..variables).map(|at|
            if bits & (1 << at) == 0 { VId(0) } else { VId(u128::MAX) }).collect();
        let count = atoms.iter().map(|&(left, relation, direction, right)| {
            edges.iter().filter(|&&(source, r, destination)| {
                let (a, b) = (assignment[left], assignment[right]);
                r == RelationId(relation) && match direction {
                    0 => source == a && destination == b,
                    1 => destination == a && source == b,
                    _ => (source == a && destination == b) || (source == b && destination == a),
                }
            }).count()
        }).product::<usize>();
        for _ in 0..count { occurrences.push((assignment[0], assignment[1])); }
    }
    let mut groups: BTreeMap<VId, Vec<VId>> = BTreeMap::new();
    for (a, b) in occurrences { groups.entry(a).or_default().push(b); }
    groups.into_iter().map(|(a, values)| {
        let support: BTreeSet<_> = values.iter().copied().collect();
        (a, values.len() as u64, values.len() as u64, support.len() as u64,
            support.first().copied(), support.last().copied())
    }).collect()
}

#[test]
fn weighted_counts_and_support_match_independent_multigraph_assignments() {
    type Case<'a> = (&'a str, &'a [Atom], usize);
    let cases: [Case<'_>; 5] = [
        ("MATCH (a)-[:R]->(b)-[:S]->(c)", &[(0, 1, 0, 1), (1, 2, 0, 2)], 3),
        ("MATCH (a)-[:R]->(b)<-[:R]-(c),(c)-[:S]->(a)",
            &[(0, 1, 0, 1), (1, 1, 1, 2), (2, 2, 0, 0)], 3),
        ("MATCH (a)-[:R]-(b)-[:S]-(c)-[:R]-(a)",
            &[(0, 1, 2, 1), (1, 2, 2, 2), (2, 1, 2, 0)], 3),
        // Same-relation mixed orientation deliberately takes the ordinary path.
        ("MATCH (a)-[:R]->(b)-[:R]-(c)", &[(0, 1, 0, 1), (1, 1, 2, 2)], 3),
        ("MATCH (a)-[:R]->(b),(a)-[:R]->(b),(b)-[:S]->(a)",
            &[(0, 1, 0, 1), (0, 1, 0, 1), (1, 2, 0, 0)], 2),
    ];
    let universe: Vec<_> = [RelationId(1), RelationId(2)].into_iter().flat_map(|r|
        [VId(0), VId(u128::MAX)].into_iter().flat_map(move |a|
            [VId(0), VId(u128::MAX)].into_iter().map(move |b| (a, r, b)))).collect();
    let prepared: Vec<_> = cases.iter().map(|(head, _, _)| prepare(&format!(
        "{head} RETURN a,COUNT(*) AS n,COUNT(b) AS m,COUNT(DISTINCT b) AS d,MIN(b) AS lo,MAX(b) AS hi GROUP BY a"))).collect();
    for mask in 0..256_usize {
        let mut edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge).collect();
        if let Some(first) = edges.first().copied() { edges.extend([first, first]); }
        for ((_, atoms, variables), query) in cases.iter().zip(&prepared) {
            assert_eq!(plain(&run(query, &edges).unwrap().value), oracle(&edges, atoms, *variables),
                "mask={mask}, atoms={atoms:?}");
        }
    }
}

fn repeated(count: usize, tail: &str) -> String {
    format!("MATCH (a){}{tail}", "-[:R]->(a)".repeat(count))
}

#[test]
fn a_billion_occurrences_are_counted_without_a_billion_binding_visits() {
    let query = prepare(&repeated(5, " RETURN COUNT(*) AS n,COUNT(a) AS m,COUNT(DISTINCT a) AS d"));
    let edges = vec![(VId(7), RelationId(1), VId(7)); 64];
    let frozen = query.canonical_bytes();
    let measured = run(&query, &edges).unwrap();
    assert_eq!(measured.value[0].get(0).unwrap().as_count(), Some(64_u64.pow(5)));
    assert_eq!(measured.value[0].get(1).unwrap().as_count(), Some(64_u64.pow(5)));
    assert_eq!(measured.value[0].get(2).unwrap().as_count(), Some(1));
    assert_eq!(measured.rows.snapshot_records, 64, "admission still counts every source edge");
    assert!(measured.evaluator.work_units < 4096, "weighted traversal regressed to occurrences");
    assert!(measured.evaluator.scratch_entries < 256);
    assert_eq!(query.canonical_bytes(), frozen);
    let result = query.execute_governed(64, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(None),
        GqlQueryPolicy::new(63, 1, u64::MAX, u64::MAX), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Rows(_))));
}

#[test]
fn above_u64_weights_refuse_counts_but_not_support_or_doomed_prefixes() {
    let edges = [(VId(0), RelationId(1), VId(0)); 2];
    let count = prepare(&repeated(64, " RETURN COUNT(*) AS n"));
    assert!(matches!(run(&count, &edges), Err(GqlQueryError::Source(
        GraphAggregateError::ArithmeticOverflow { aggregate: 0 }))));
    let support = prepare(&repeated(64, " RETURN COUNT(DISTINCT a) AS n,MIN(a) AS lo,MAX(a) AS hi"));
    let result = run(&support, &edges).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(1));
    assert_eq!(vertex(&result.value[0], 1), Some(VId(0)));
    assert_eq!(vertex(&result.value[0], 2), Some(VId(0)));
    let doomed = prepare(&repeated(63, "-[:S]->(a) RETURN COUNT(*) AS n"));
    let edges = [(VId(0), RelationId(1), VId(0)); 3];
    assert_eq!(run(&doomed, &edges).unwrap().value[0].get(0).unwrap().as_count(), Some(0));
}

#[test]
fn group_accumulation_overflow_is_checked_after_exact_per_binding_weights() {
    let edges = [(VId(0), RelationId(1), VId(0)), (VId(0), RelationId(1), VId(0)),
        (VId(u128::MAX), RelationId(1), VId(u128::MAX)),
        (VId(u128::MAX), RelationId(1), VId(u128::MAX))];
    let grouped = prepare(&repeated(63, " RETURN a,COUNT(*) AS n GROUP BY a"));
    let rows = run(&grouped, &edges).unwrap().value;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.get(0).unwrap().as_count() == Some(1_u64 << 63)));
    let global = prepare(&repeated(63, " RETURN COUNT(*) AS n"));
    assert!(matches!(run(&global, &edges), Err(GqlQueryError::Source(
        GraphAggregateError::ArithmeticOverflow { aggregate: 0 }))));
}

#[test]
fn property_sources_and_unused_node_scan_edges_keep_the_ordinary_contract() {
    let query = prepare("MATCH (a)-[:R]->(b) RETURN COUNT(b.n) AS n");
    let edges = [(VId(0), RelationId(1), VId(1)); 2];
    let scalar = CanonicalScalar::Int(7);
    let mut calls = 0;
    let result = query.execute_governed(2, [], edges, |_, _| Ok::<_, &str>(true), |_, _| {
        calls += 1;
        if calls == 2 { Err("second occurrence source failure") } else { Ok(Some(&scalar)) }
    }, wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source(
        "second occurrence source failure")))));
    assert_eq!(calls, 2, "property-bearing aggregates must not factor away fallible reads");
    let query = prepare("MATCH (a) RETURN COUNT(*) AS n");
    let mut consumed = 0;
    let edges = edges.into_iter().inspect(|_| consumed += 1);
    let result = query.execute_governed(2, [VId(0), VId(1)], edges,
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(2));
    assert_eq!(consumed, 0, "vertex-only aggregation must not consume a topology iterator");
}

#[test]
fn weighted_preprocessing_and_output_share_exact_limits_and_all_checkpoints() {
    let query = prepare("MATCH (a)-[:R]->(b)-[:S]->(a) RETURN a,COUNT(*) AS n GROUP BY a HAVING n >= 1 ORDER BY n DESC,a ASC LIMIT 1");
    let edges = [(VId(0), RelationId(1), VId(1)), (VId(0), RelationId(1), VId(1)),
        (VId(1), RelationId(2), VId(0)), (VId(1), RelationId(2), VId(0))];
    let mut calls = 0;
    let mut consumed = 0;
    let measured = query.execute_governed(4, [], edges.into_iter().inspect(|_| consumed += 1),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || {
            calls += 1; Ok::<_, usize>(())
        }).unwrap();
    assert_eq!(consumed, 4);
    assert_eq!(measured.value[0].get(0).unwrap().as_count(), Some(4));
    let exact = GqlQueryPolicy::new(4, 1, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    let rerun = |policy| query.execute_governed(4, [], edges, |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None), policy, || Ok::<_, usize>(()));
    assert_eq!(rerun(exact).unwrap(), measured);
    for policy in [GqlQueryPolicy::new(4, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(4, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(4, 1, u64::MAX, measured.evaluator.scratch_entries - 1)] {
        assert!(rerun(policy).is_err());
    }
    for stop in 1..=calls {
        let mut at = 0;
        let result = query.execute_governed(4, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || {
            at += 1; if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}
