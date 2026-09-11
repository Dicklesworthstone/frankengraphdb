//! Branch elimination is independent of where hidden variables were declared.
//! Expected results use complete assignments over actual edge occurrences,
//! never the compact slot map or completion-message implementation.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    GraphAggregateError, GraphAggregateRow, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::{BTreeMap, BTreeSet};

type Edge = (VId, RelationId, VId);
type Atom = (usize, u64, usize);
type Plain = (Vec<VId>, u64, u64, u64, VId, VId);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        (GraphSymbolKind::Relation, "U") => Some(GraphSymbol::Relation(RelationId(4))),
        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 1000, 1_000_000, 100_000) }
fn run(query: &PreparedGraphAggregate, edges: &[Edge])
    -> Result<GqlQueryExecution<GraphAggregateRow>, GqlQueryError<GraphAggregateError<&'static str>, usize>> {
    query.execute_governed(edges.len() as u64, [], edges.iter().copied(),
        |_, _| Err("unexpected predicate"), |_, _| Err("unexpected property"),
        wide(), || Ok::<_, usize>(()))
}
fn plain(rows: &[GraphAggregateRow]) -> Vec<Plain> {
    rows.iter().map(|row| (row.keys().iter().map(|key| key.as_vertex().unwrap()).collect(),
        row.get(0).unwrap().as_count().unwrap(), row.get(1).unwrap().as_count().unwrap(),
        row.get(2).unwrap().as_count().unwrap(),
        row.get(3).unwrap().as_value().unwrap().as_vertex().unwrap(),
        row.get(4).unwrap().as_value().unwrap().as_vertex().unwrap())).collect()
}
fn matches(edge: Edge, a: VId, relation: u64, b: VId, direction: u8) -> bool {
    let (source, r, destination) = edge;
    r == RelationId(relation) && match direction {
        0 => source == a && destination == b,
        1 => source == b && destination == a,
        _ => (source == a && destination == b) || (source == b && destination == a),
    }
}
fn oracle(edges: &[Edge], atoms: &[Atom], columns: &[usize], variables: usize,
    direction: u8, identity: Option<(usize, usize, bool)>) -> Vec<Plain> {
    let mut groups: BTreeMap<Vec<VId>, (u64, BTreeSet<VId>)> = BTreeMap::new();
    for bits in 0..1_usize << variables {
        let row: Vec<_> = (0..variables).map(|at|
            if bits & (1 << at) == 0 { VId(0) } else { VId(u128::MAX) }).collect();
        if identity.is_some_and(|(a, b, equal)| (row[a] == row[b]) != equal) { continue; }
        let weight: u64 = atoms.iter().map(|&(a, r, b)|
            edges.iter().filter(|edge| matches(**edge, row[a], r, row[b], direction)).count() as u64).product();
        if weight == 0 { continue; }
        let group = groups.entry(columns.iter().map(|at| row[*at]).collect()).or_default();
        group.0 += weight;
        group.1.insert(row[0]);
    }
    groups.into_iter().map(|(key, (count, roots))|
        (key, count, count, roots.len() as u64, *roots.first().unwrap(), *roots.last().unwrap())).collect()
}
fn statement(atoms: &[Atom], columns: &[usize], direction: u8,
    identity: Option<(usize, usize, bool)>) -> String {
    let parts: Vec<_> = atoms.iter().map(|&(a, relation, b)| {
        let relation = if relation == 1 { "R" } else { "S" };
        match direction {
            0 => format!("(n{a})-[:{relation}]->(n{b})"),
            1 => format!("(n{a})<-[:{relation}]-(n{b})"),
            _ => format!("(n{a})-[:{relation}]-(n{b})"),
        }
    }).collect();
    let keys = columns.iter().map(|at| format!("n{at}")).collect::<Vec<_>>().join(",");
    let condition = identity.map_or(String::new(), |(a, b, equal)|
        format!(" WHERE n{a}{}n{b}", if equal { "=" } else { "<>" }));
    format!("MATCH {}{condition} RETURN {keys},COUNT(*) AS n,COUNT(n0) AS m,\
        COUNT(DISTINCT n0) AS d,MIN(n0) AS lo,MAX(n0) AS hi GROUP BY {keys}", parts.join(","))
}

#[test]
fn nonterminal_branches_cycles_and_observed_ancestors_match_assignment_oracle() {
    type Case<'a> = (&'a [Atom], &'a [usize], usize, Option<(usize, usize, bool)>);
    let cases: [Case<'_>; 6] = [
        (&[(0,1,1), (1,2,2), (1,1,3)], &[3], 4, None),
        (&[(0,1,1), (1,2,2), (2,2,3), (1,1,4), (4,2,0)], &[4], 5, None),
        (&[(0,1,1), (1,2,2), (1,1,3), (3,2,4), (3,1,5)], &[3,5], 6, None),
        (&[(0,1,1), (1,2,2), (1,1,3), (2,2,4)], &[4], 5, None),
        (&[(0,1,1), (1,2,2), (1,1,3)], &[3], 4, Some((2,3,false))),
        (&[(0,1,0), (0,2,1), (0,1,2)], &[2], 3, None),
    ];
    let universe: Vec<_> = [VId(0), VId(u128::MAX)].into_iter().flat_map(|a|
        [RelationId(1), RelationId(2)].into_iter().flat_map(move |r|
            [VId(0), VId(u128::MAX)].into_iter().map(move |b| (a,r,b)))).collect();
    for direction in 0..3 {
        let queries: Vec<_> = cases.iter().map(|(atoms, columns, _, identity)|
            prepare(&statement(atoms, columns, direction, *identity))).collect();
        for mask in 0..256_usize {
            let mut edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, edge)| *edge).collect();
            if let Some(first) = edges.first().copied() { edges.extend([first, first]); }
            for ((atoms, columns, variables, identity), query) in cases.iter().zip(&queries) {
                assert_eq!(plain(&run(query, &edges).unwrap().value),
                    oracle(&edges, atoms, columns, *variables, direction, *identity),
                    "mask={mask} direction={direction} atoms={atoms:?}");
            }
        }
    }
}

fn branches(count: usize) -> String {
    (0..count).map(|at| format!(",(b)-[:S]->(h{at})")).collect()
}
#[test]
fn a_billion_early_assignments_collapse_before_a_returned_cycle_vertex() {
    let mut edges = vec![(VId(0), RelationId(1), VId(1)),
        (VId(1), RelationId(3), VId(20)), (VId(20), RelationId(4), VId(0))];
    edges.extend((2..10).map(|id| (VId(1), RelationId(2), VId(id))));
    let tail = " RETURN c,COUNT(*) AS n,COUNT(DISTINCT a) AS d GROUP BY c";
    let early = prepare(&format!("MATCH (a)-[:R]->(b){},(b)-[:T]->(c)-[:U]->(a){tail}", branches(10)));
    let late = prepare(&format!("MATCH (a)-[:R]->(b)-[:T]->(c)-[:U]->(a){}{tail}", branches(10)));
    let original = early.canonical_bytes();
    let result = run(&early, &edges).unwrap();
    assert_eq!(result.value, run(&late, &edges).unwrap().value);
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.value[0].keys()[0].as_vertex(), Some(VId(20)));
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(8_u64.pow(10)));
    assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(1));
    assert!(result.evaluator.work_units < 65_536);
    assert!(result.evaluator.scratch_entries < 4096);
    assert_eq!(result.rows.snapshot_records, 11);
    assert_eq!(early.canonical_bytes(), original);
}

#[test]
fn maximum_width_overflow_support_and_zero_completion_survive_remapping() {
    let edges: Vec<_> = std::iter::once((VId(0), RelationId(1), VId(1)))
        .chain((2..10).map(|id| (VId(1), RelationId(2), VId(id))))
        .chain(std::iter::once((VId(1), RelationId(4), VId(20)))).collect();
    // 64 edge atoms and 65 binding positions; the last one is retained.
    let head = format!("MATCH (a)-[:R]->(b){},(b)-[:U]->(c)", branches(62));
    let support = prepare(&format!("{head} RETURN COUNT(DISTINCT c) AS n,MIN(c) AS lo,MAX(c) AS hi"));
    let result = run(&support, &edges).unwrap();
    assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(1));
    assert_eq!(result.value[0].get(1).unwrap().as_value().unwrap().as_vertex(), Some(VId(20)));
    let count = prepare(&format!("{head} RETURN COUNT(c) AS n"));
    assert!(matches!(run(&count, &edges), Err(GqlQueryError::Source(
        GraphAggregateError::ArithmeticOverflow { aggregate: 0 }))));
    // An impossible branch before the retained result annihilates overflow.
    let zero = prepare(&format!("MATCH (a)-[:R]->(b){},(b)-[:T]->(missing),(b)-[:U]->(c) \
        RETURN COUNT(c) AS n", branches(61)));
    assert_eq!(run(&zero, &edges).unwrap().value[0].get(0).unwrap().as_count(), Some(0));
}

#[test]
fn branch_partition_source_and_output_share_exact_limits_and_checkpoints() {
    let query = prepare("MATCH (a)-[:R]->(b),(b)-[:S]->(h),(b)-[:T]->(c)-[:U]->(a) \
        RETURN c,COUNT(*) AS n GROUP BY c HAVING n>0 ORDER BY n DESC,c LIMIT 1");
    let edges = [(VId(0),RelationId(1),VId(1)), (VId(1),RelationId(2),VId(2)),
        (VId(1),RelationId(2),VId(3)), (VId(1),RelationId(3),VId(4)),
        (VId(4),RelationId(4),VId(0))];
    let mut calls = 0;
    let mut consumed = 0;
    let measured = query.execute_governed(5, [], edges.into_iter().inspect(|_| consumed += 1),
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || { calls += 1; Ok::<_, usize>(()) }).unwrap();
    assert_eq!(consumed, 5);
    assert_eq!(measured.value[0].get(0).unwrap().as_count(), Some(2));
    let exact = GqlQueryPolicy::new(5, 1, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    let rerun = |policy| query.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true),
        |_, _| Ok(None), policy, || Ok::<_, usize>(()));
    assert_eq!(rerun(exact).unwrap(), measured);
    for policy in [GqlQueryPolicy::new(5, 0, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(5, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(5, 1, u64::MAX, measured.evaluator.scratch_entries - 1)] {
        assert!(rerun(policy).is_err());
    }
    for stop in 1..=calls {
        let mut at = 0;
        let result = query.execute_governed(5, [], edges, |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || {
            at += 1; if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn observed_branches_keep_per_occurrence_property_errors_even_with_limit_zero() {
    let head = "MATCH (a)-[:R]->(b),(b)-[:S]->(h),(b)-[:T]->(c)";
    let edges = [(VId(0),RelationId(1),VId(1)), (VId(0),RelationId(1),VId(1)),
        (VId(1),RelationId(2),VId(2)), (VId(1),RelationId(3),VId(3))];
    let scalar = CanonicalScalar::Int(7);
    for tail in [" RETURN c,COUNT(h.n) AS n GROUP BY c LIMIT 0",
        " WHERE h.n=c.n RETURN c,COUNT(*) AS n GROUP BY c LIMIT 0"] {
        let query = prepare(&format!("{head}{tail}"));
        let mut reads = 0;
        let result = query.execute_governed(4, [], edges, |_, _| Ok::<_, &str>(true), |_, _| {
            reads += 1;
            if reads == 2 { Err("second source read") } else { Ok(Some(&scalar)) }
        }, wide(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("second source read")))));
        assert_eq!(reads, 2);
    }
    let query = prepare(&format!("{head} WHERE h.n=7 RETURN c,COUNT(*) AS n GROUP BY c"));
    let result = query.execute_governed(4, [], edges, |_, _| Err::<bool, _>("predicate source"),
        |_, _| Ok(None), wide(), || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphAggregateError::Source("predicate source")))));
}
