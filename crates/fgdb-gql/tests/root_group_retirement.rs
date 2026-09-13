//! Root-group retirement is exercised through the public text and GLA path.
//! Expected groups come from owned edge occurrences, not another prepared plan.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateRow,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText};
use fgdb_types::{CanonicalScalar, VId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        _ => None,
    }
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn prepare(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols).unwrap().bind_parameters(&GqlParameters::new()).unwrap()
}

#[derive(Clone)]
struct Expected {
    root: VId,
    count: u64,
    distinct: u64,
    sum: Option<i128>,
    mean: Option<(i128, u64)>,
    unique_sum: Option<i128>,
    unique_mean: Option<(i128, u64)>,
}
fn summary(root: VId, inputs: &[Option<i64>]) -> Expected {
    let values: Vec<_> = inputs.iter().flatten().copied().collect();
    let unique: BTreeSet<_> = values.iter().copied().collect();
    let sum = (!values.is_empty()).then(|| values.iter().map(|n| i128::from(*n)).sum::<i128>());
    let unique_sum = (!unique.is_empty()).then(|| unique.iter().map(|n| i128::from(*n)).sum::<i128>());
    Expected { root, count: inputs.len() as u64, distinct: unique.len() as u64, sum,
        mean: sum.map(|sum| (sum, values.len() as u64)), unique_sum,
        unique_mean: unique_sum.map(|sum| (sum, unique.len() as u64)) }
}
fn rank(a: &Expected, b: &Expected) -> Ordering {
    match (a.mean, b.mean) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        // The independent fixture has at most twelve small integer inputs.
        (Some((a, na)), Some((b, nb))) => (b * i128::from(na)).cmp(&(a * i128::from(nb))),
    }.then_with(|| a.root.cmp(&b.root))
}
fn check(rows: &[GraphAggregateRow], expected: &[Expected]) {
    assert_eq!(rows.len(), expected.len());
    for (row, expected) in rows.iter().zip(expected) {
        assert_eq!(row.keys()[0].as_vertex(), Some(expected.root));
        assert_eq!(row.get(0).unwrap().as_count(), Some(expected.count));
        assert_eq!(row.get(1).unwrap().as_count(), Some(expected.distinct));
        assert_eq!(row.get(2).unwrap().as_integer(), expected.sum);
        assert_eq!(row.get(4).unwrap().as_integer(), expected.unique_sum);
        for (column, ratio) in [(3, expected.mean), (5, expected.unique_mean)] {
            match ratio {
                None => assert!(row.get(column).unwrap().is_null()),
                Some((sum, count)) => {
                    let actual = row.get(column).unwrap().as_average().unwrap();
                    assert_eq!(actual.numerator() * i128::from(count), sum * i128::from(actual.denominator()));
                }
            }
        }
    }
}

#[test]
fn oriented_multigraph_pages_match_independent_complete_group_enumeration() {
    let universe = [(VId(1), R, VId(1)), (VId(1), R, VId(2)), (VId(1), R, VId(2)),
        (VId(2), R, VId(1)), (VId(2), R, VId(2)), (VId(2), R, VId(3))];
    for (direction, arrow) in [(0, "-[:R]->"), (1, "<-[:R]-"), (2, "-[:R]-")] {
        let text = format!("MATCH (a:L){arrow}(b) RETURN a,COUNT(*) AS n,COUNT(DISTINCT b.p) AS d,\
            SUM(b.p) AS total,AVG(b.p) AS mean,SUM(DISTINCT b.p) AS unique_total,AVG(DISTINCT b.p) AS unique_mean \
            GROUP BY a HAVING COUNT(b.p) >= 2 OR mean IS NULL ORDER BY mean DESC NULLS LAST,a SKIP $off LIMIT $take");
        let template = PreparedGraphAggregateText::prepare(&text, symbols).unwrap();
        for mask in 0..64 {
            let edges: Vec<_> = universe.iter().enumerate().filter(|(at, _)| mask & (1 << at) != 0)
                .map(|(_, edge)| *edge).collect();
            for mut encoded in 0..27 {
                let raw: Vec<_> = (0..3).map(|_| { let value = [None, Some(-7), Some(9)][encoded % 3]; encoded /= 3; value }).collect();
                let scalars: Vec<_> = raw.iter().map(|value| value.map(CanonicalScalar::Int)).collect();
                let mut groups: BTreeMap<VId, Vec<Option<i64>>> = BTreeMap::new();
                for &(src, _, dst) in &edges {
                    let oriented = if direction == 1 { vec![(dst, src)] }
                        else if direction == 2 && src != dst { vec![(src, dst), (dst, src)] }
                        else { vec![(src, dst)] };
                    for (a, b) in oriented { groups.entry(a).or_default().push(raw[b.0 as usize - 1]); }
                }
                let mut expected: Vec<_> = groups.iter().filter(|(_, values)| {
                    let count = values.iter().flatten().count(); count == 0 || count >= 2
                }).map(|(root, values)| summary(*root, values)).collect();
                expected.sort_by(rank);
                for (offset, count) in [(0, 0), (0, 3), (1, 1)] {
                    let args = GqlParameters::new().with_uint64("off", offset).unwrap().with_uint64("take", count).unwrap();
                    let query = template.bind_parameters(&args).unwrap();
                    let result = query.execute_governed(edges.len() as u64, [], edges.iter().rev().copied(),
                        |_, _| Ok::<_, ()>(true), |vid, _| Ok(scalars[vid.0 as usize - 1].as_ref()), wide(), || Ok::<_, ()>(())).unwrap();
                    let page: Vec<_> = expected.iter().skip(offset as usize).take(count as usize).cloned().collect();
                    check(&result.value, &page);
                }
            }
        }
    }
}

#[test]
fn grouped_payloads_are_copied_only_for_the_final_page_and_all_source_rows_are_read() {
    let query = prepare("MATCH (a)-[:R]->(b) RETURN a,MIN(b.p) AS payload GROUP BY a ORDER BY a DESC SKIP 1 LIMIT 2");
    let edges: Vec<_> = (0..16).map(|id| (VId(id), R, VId(id + 100))).collect();
    let small = CanonicalScalar::bytes(Vec::new()).unwrap();
    let large = CanonicalScalar::bytes(vec![7; 8192]).unwrap();
    let run = |value: &CanonicalScalar| {
        let mut edge_reads = 0; let mut property_reads = 0;
        let result = query.execute_governed(16, [], edges.iter().rev().copied().inspect(|_| { edge_reads += 1; }),
            |_, _| Ok::<_, ()>(true), |_, _| { property_reads += 1; Ok(Some(value)) }, wide(), || Ok::<_, ()>(())).unwrap();
        assert_eq!(edge_reads, 16); assert_eq!(property_reads, 16);
        result
    };
    let a = run(&small); let b = run(&large);
    assert_eq!(b.evaluator.scratch_entries - a.evaluator.scratch_entries, 2 * 8192 / 64);
    assert_eq!(b.value.iter().map(|row| row.keys()[0].as_vertex().unwrap()).collect::<Vec<_>>(), vec![VId(14), VId(13)]);
    assert!(b.value.iter().all(|row| row.get(0).unwrap().as_value().unwrap().as_scalar() == Some(&large)));
}

#[test]
fn all_four_limits_and_every_public_execution_checkpoint_remain_enforced() {
    let query = prepare("MATCH (a)-[:R]->(b) RETURN a,AVG(DISTINCT b.p) AS mean,COUNT(DISTINCT b.p) AS kinds \
        GROUP BY a HAVING mean IS NOT NULL ORDER BY mean DESC SKIP 1 LIMIT 2");
    let values: Vec<_> = (0..=4).map(CanonicalScalar::Int).collect();
    let edges: Vec<_> = (0..4).flat_map(|id| [(VId(id), R, VId(id)), (VId(id), R, VId(id + 1)), (VId(id), R, VId(id + 1))]).collect();
    let run = |policy| query.execute_governed(12, [], edges.iter().copied(), |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(Some(&values[vid.0 as usize])), policy, || Ok::<_, usize>(()));
    let complete = run(wide()).unwrap();
    assert_eq!(complete.value.len(), 2);
    assert_eq!(run(GqlQueryPolicy::new(12, 2, complete.evaluator.work_units, complete.evaluator.scratch_entries)).unwrap(), complete);
    for policy in [GqlQueryPolicy::new(11, 2, u64::MAX, u64::MAX), GqlQueryPolicy::new(12, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(12, 2, complete.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(12, 2, u64::MAX, complete.evaluator.scratch_entries - 1)] { assert!(run(policy).is_err()); }
    let mut events = 0;
    query.execute_governed(12, [], edges.iter().copied(), |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[vid.0 as usize])),
        wide(), || { events += 1; Ok::<_, usize>(()) }).unwrap();
    for stop in 1..=events {
        let mut at = 0;
        let result = query.execute_governed(12, [], edges.iter().copied(), |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(), || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn optional_and_existential_scopes_complete_before_the_root_is_retired() {
    let s = RelationId(2);
    let edges = [(VId(1), R, VId(10)), (VId(1), R, VId(10)), (VId(1), R, VId(11)),
        (VId(2), R, VId(12)), (VId(10), s, VId(20)), (VId(10), s, VId(20)),
        (VId(10), s, VId(21)), (VId(12), s, VId(22))];
    let values = [CanonicalScalar::Int(2), CanonicalScalar::Int(8), CanonicalScalar::Null];
    let property = |vid: VId| match vid.0 { 20 => Some(&values[0]), 21 => Some(&values[1]),
        22 => Some(&values[2]), _ => None };
    let run = |query: &PreparedGraphAggregate| query.execute_governed(8, [], edges,
        |vid, predicates| Ok::<_, ()>(predicates.iter().all(|predicate| predicate.matches_borrowed(
            (vid.0 < 10).then_some(LabelId(1)), property(vid).map(|value| (P, value)),
        ))), |vid, _| Ok(property(vid)), wide(), || Ok::<_, ()>(())).unwrap().value;
    let optional = prepare("MATCH (a:L)-[:R]->(b) OPTIONAL MATCH (b)-[:S]->(c) \
        RETURN a,COUNT(*) AS n,COUNT(c) AS present,AVG(c.p) AS mean GROUP BY a ORDER BY n DESC,a LIMIT 5");
    let rows = run(&optional);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].keys()[0].as_vertex(), Some(VId(1)));
    assert_eq!(rows[0].get(0).unwrap().as_count(), Some(7));
    assert_eq!(rows[0].get(1).unwrap().as_count(), Some(6));
    assert_eq!(rows[0].get(2).unwrap().as_average().unwrap().numerator(), 4);
    assert_eq!(rows[0].get(2).unwrap().as_average().unwrap().denominator(), 1);
    assert_eq!(rows[1].get(0).unwrap().as_count(), Some(1));
    assert_eq!(rows[1].get(1).unwrap().as_count(), Some(1));
    assert!(rows[1].get(2).unwrap().is_null());
    for (negation, expected) in [("", vec![(VId(1), 2)]), ("NOT ", vec![(VId(1), 1), (VId(2), 1)])] {
        let query = prepare(&format!("MATCH (a:L)-[:R]->(b) WHERE {negation}EXISTS {{ MATCH (b)-[:S]->(c) WHERE c.p>3 }} \
            RETURN a,COUNT(*) AS n GROUP BY a ORDER BY a LIMIT 5"));
        assert_eq!(run(&query).iter().map(|row| (row.keys()[0].as_vertex().unwrap(), row.get(0).unwrap().as_count().unwrap()))
            .collect::<Vec<_>>(), expected);
    }
}

#[test]
fn unproven_input_order_and_nontrivial_output_distinct_keep_general_grouping() {
    let low = CanonicalScalar::Int(3);
    let high = CanonicalScalar::Int(7);
    let node = prepare("MATCH (a) RETURN a,COUNT(*) AS n,AVG(a.p) AS mean GROUP BY a ORDER BY a LIMIT 5");
    let result = node.execute_governed(3, [VId(2), VId(1), VId(2)], [],
        |_, _| Ok::<_, ()>(true), |vid, _| Ok(Some(if vid == VId(1) { &low } else { &high })),
        wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.len(), 2);
    assert_eq!(result.value[0].keys()[0].as_vertex(), Some(VId(1)));
    assert_eq!(result.value[1].get(0).unwrap().as_count(), Some(2));
    assert_eq!(result.value[1].get(1).unwrap().as_average().unwrap().numerator(), 7);
    let distinct = prepare("MATCH (a)-[:R]->(b) RETURN DISTINCT AVG(b.p) AS mean GROUP BY a ORDER BY a LIMIT 2");
    let edges = [(VId(1), R, VId(10)), (VId(2), R, VId(10)), (VId(3), R, VId(11))];
    let result = distinct.execute_governed(3, [], edges, |_, _| Ok::<_, ()>(true),
        |vid, _| Ok(Some(if vid == VId(10) { &low } else { &high })), wide(), || Ok::<_, ()>(())).unwrap();
    assert_eq!(result.value.len(), 2);
    assert!(result.value.iter().all(|row| row.keys().is_empty()));
    assert_eq!(result.value.iter().map(|row| row.get(0).unwrap().as_average().unwrap().numerator())
        .collect::<Vec<_>>(), vec![3, 7]);
}
