//! Terminal top-prefix selection, compared with independent full-sort results.
//! This is not early traversal termination or storage/spill memory governance.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GlaDirection, GraphPatternBuilder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText};
use fgdb_types::{CanonicalScalar, VId};

const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(10_000, 10_000, u64::MAX, u64::MAX) }

#[test]
fn scalar_tuple_and_value_pages_match_independent_sorted_multigraph_projections() {
    let high = VId(1_u128 << 100);
    let universe = [(VId(0), RelationId(1), high), (VId(0), RelationId(1), high),
        (high, RelationId(1), VId(0)), (VId(2), RelationId(1), VId(0)),
        (high, RelationId(1), high), (VId(2), RelationId(1), high)];
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("a").unwrap(); builder.vertex("b").unwrap();
    builder.edge("a", RelationId(1), GlaDirection::Forward, "b").unwrap();
    for mask in 0..64 {
        let edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge).collect();
        for distinct in [false, true] {
            let mut scalar_expected: Vec<_> = edges.iter().map(|edge| edge.2).collect();
            let mut tuple_expected: Vec<_> = edges.iter().map(|edge| vec![edge.2, edge.0]).collect();
            scalar_expected.sort_unstable(); tuple_expected.sort();
            if distinct { scalar_expected.dedup(); tuple_expected.dedup(); }
            for offset in 0..3_u64 {
                for count in 0..3_u64 {
                    let scalar = builder.prepare("b", offset, Some(count)).unwrap();
                    let scalar = if distinct { scalar } else { scalar.with_duplicates() };
                    let tuple = builder.prepare_bindings(&["b", "a"], offset, Some(count)).unwrap();
                    let tuple = if distinct { tuple } else { tuple.with_duplicates() };
                    let value = query(&format!("MATCH (a)-[:R]->(b) RETURN {} b,a SKIP {offset} LIMIT {count}",
                        if distinct { "DISTINCT" } else { "ALL" }));
                    let rows = scalar.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
                    assert_eq!(rows, scalar_expected.iter().copied().skip(offset as usize).take(count as usize).collect::<Vec<_>>());
                    let rows = tuple.plan().execute([], edges.iter().copied(), |_, _| Ok::<_, ()>(true)).unwrap();
                    let expected: Vec<_> = tuple_expected.iter().skip(offset as usize).take(count as usize).cloned().collect();
                    assert_eq!(rows.iter().map(|row| row.values().to_vec()).collect::<Vec<_>>(), expected);
                    let rows = value.plan().execute_governed_with_properties(edges.len() as u64, [], edges.iter().copied(),
                        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap();
                    assert_eq!(rows.value.iter().map(|row| row.values().iter().map(|v| v.as_vertex().unwrap()).collect::<Vec<_>>())
                        .collect::<Vec<_>>(), expected);
                }
            }
        }
    }
}

#[test]
fn small_bag_page_avoids_thousands_of_payload_copies_but_reads_every_candidate() {
    let pattern = query("MATCH (n) RETURN n.p AS payload SKIP 2 LIMIT 3");
    let payload = CanonicalScalar::bytes(vec![7; 4096]).unwrap();
    let mut reads = 0;
    let frozen = pattern.canonical_bytes();
    let result = pattern.plan().execute_governed_with_properties(4096, (0..4096).map(VId), [],
        |_, _| Ok::<_, ()>(true), |_, _| { reads += 1; Ok(Some(&payload)) },
        GqlQueryPolicy::new(4096, 3, 1_000_000, 5 * (1 + 1 + 64)), || Ok::<_, ()>(())).unwrap();
    assert_eq!(reads, 4096);
    assert_eq!(result.value.len(), 3);
    assert_eq!(result.evaluator.scratch_entries, 330);
    assert!(result.value.iter().all(|row| row.get(0).unwrap().as_scalar() == Some(&payload)));
    assert_eq!(pattern.canonical_bytes(), frozen);
    let all = query("MATCH (n) RETURN n.p AS payload");
    assert!(matches!(all.plan().execute_governed_with_properties(4096, (0..4096).map(VId), [],
        |_, _| Ok::<_, ()>(true), |_, _| Ok(Some(&payload)),
        GqlQueryPolicy::new(4096, 4096, 1_000_000, 330), || Ok::<_, ()>(())),
        Err(GqlQueryError::Evaluator(_))));
}

#[test]
fn zero_and_full_pages_never_hide_late_predicate_or_projected_property_failures() {
    for count in [0, 1] {
        let pattern = query(&format!("MATCH (n) RETURN n,n.p LIMIT {count}"));
        let scalar = CanonicalScalar::Int(1);
        let mut reads = 0;
        let result = pattern.plan().execute_governed_with_properties(2, [VId(0), VId(9)], [],
            |_, _| Ok::<_, &'static str>(true), |vid, _| {
                reads += 1;
                if vid == VId(9) { Err("late projection") } else { Ok(Some(&scalar)) }
            }, wide(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source("late projection"))));
        assert_eq!(reads, 2);
        let pattern = query(&format!("MATCH (n) WHERE n.p > 0 RETURN n LIMIT {count}"));
        let result = pattern.plan().execute_governed_with_properties(2, [VId(0), VId(9)], [],
            |vid, _| if vid == VId(9) { Err("late predicate") } else { Ok(true) },
            |_, _| Ok(None), wide(), || Ok::<_, ()>(()));
        assert!(matches!(result, Err(GqlQueryError::Source("late predicate"))));
    }
}

#[test]
fn page_selection_preserves_nullable_occurrences_and_unpaginated_aggregate_input() {
    let pattern = query("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN b,a SKIP 1 LIMIT 3");
    let edges = [(VId(1), RelationId(1), VId(9)), (VId(1), RelationId(1), VId(9))];
    let rows = pattern.plan().execute_governed_with_properties(5, [VId(1), VId(2), VId(3)], edges,
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value;
    assert_eq!(rows.len(), 3);
    assert!(rows[0].get(0).unwrap().is_null());
    assert_eq!(rows[0].get(1).unwrap().as_vertex(), Some(VId(3)));
    assert_eq!(rows[1], rows[2]);
    assert_eq!(rows[1].get(0).unwrap().as_vertex(), Some(VId(9)));
    let aggregate = PreparedGraphAggregateText::prepare(
        "MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) RETURN COUNT(*) AS paths,COUNT(b) AS present LIMIT 1",
        symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
    let rows = aggregate.execute_governed(5, [VId(1), VId(2), VId(3)], edges,
        |_, _| Ok::<_, ()>(true), |_, _| Ok(None), wide(), || Ok::<_, ()>(())).unwrap().value;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(4));
    assert_eq!(rows[0].get(1).unwrap().as_count(), Some(2));
}

#[test]
fn exact_page_limits_and_every_interruption_return_no_partial_success() {
    let pattern = query("MATCH (n) RETURN n.p,n SKIP 1 LIMIT 2");
    let values = [CanonicalScalar::Int(8), CanonicalScalar::Int(5), CanonicalScalar::Int(1), CanonicalScalar::Null];
    let vertices = [VId(0), VId(1), VId(2), VId(3)];
    let run = |policy| pattern.plan().execute_governed_with_properties(4, vertices, [],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[vid.0 as usize])), policy, || Ok::<_, usize>(()));
    let measured = run(wide()).unwrap();
    let exact = GqlQueryPolicy::new(4, 2, measured.evaluator.work_units, measured.evaluator.scratch_entries);
    assert_eq!(run(exact).unwrap(), measured);
    assert!(matches!(run(GqlQueryPolicy::new(4, 2, measured.evaluator.work_units - 1, u64::MAX)), Err(GqlQueryError::Evaluator(_))));
    assert!(matches!(run(GqlQueryPolicy::new(4, 2, u64::MAX, measured.evaluator.scratch_entries - 1)), Err(GqlQueryError::Evaluator(_))));
    assert!(matches!(run(GqlQueryPolicy::new(4, 1, u64::MAX, u64::MAX)), Err(GqlQueryError::Rows(_))));
    let mut total = 0;
    pattern.plan().execute_governed_with_properties(4, vertices, [], |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(Some(&values[vid.0 as usize])), wide(), || { total += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=total {
        let mut calls = 0;
        let result = pattern.plan().execute_governed_with_properties(4, vertices, [], |_, _| Ok::<_, ()>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])), wide(), || {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(calls, stop);
    }
}
