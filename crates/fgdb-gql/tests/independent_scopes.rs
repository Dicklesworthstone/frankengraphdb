//! Independent clauses use the same scoped visitor as correlated matches.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphColumn, GraphExistence, GraphMatchClause, GraphPatternBuilder,
    GraphValueRow, IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const ROOT: LabelId = LabelId(1);
const FLAG: LabelId = LabelId(2);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, RelationId, VId);
type Row = Vec<Option<VId>>;
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Root") => Some(GraphSymbol::Label(ROOT)),
        (GraphSymbolKind::Label, "Flag") => Some(GraphSymbol::Label(FLAG)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn node(name: &str, label: Option<LabelId>) -> GraphPatternBuilder {
    let mut b = GraphPatternBuilder::new();
    b.vertex(name).unwrap();
    if let Some(label) = label {
        b.filter(name, VertexPredicate::HasLabel(label)).unwrap();
    }
    b
}
fn matches(vid: VId, predicates: &[VertexPredicate]) -> bool {
    let labels = if vid.0 < 2 {
        vec![ROOT]
    } else if (10..12).contains(&vid.0) {
        vec![FLAG]
    } else {
        vec![]
    };
    predicates
        .iter()
        .all(|p| p.matches(&labels, &[(P, CanonicalScalar::Int(vid.0 as i64))]))
}
fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn rows(values: &[GraphValueRow]) -> Vec<Row> {
    values
        .iter()
        .map(|row| row.values().iter().map(|value| value.as_vertex()).collect())
        .collect()
}
fn run(plan: &PreparedGraphPattern<GraphValueRow>, vertices: &[VId], edges: &[Edge]) -> Vec<Row> {
    rows(
        &plan
            .plan()
            .execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vid, p| Ok::<_, ()>(matches(vid, p)),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value,
    )
}

#[test]
fn independent_probes_match_typed_definitions_and_rebind_without_resolution() {
    let text =
        "MATCH (a:Root) WHERE EXISTS { MATCH (f:Flag) WHERE f.p >= $floor } RETURN a LIMIT $take";
    let mut calls = BTreeMap::new();
    let template = PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
        *calls.entry((kind, name.to_owned())).or_insert(0) += 1;
        symbols(kind, name)
    })
    .unwrap();
    let args = |floor| {
        GqlParameters::new()
            .with_int64("floor", floor)
            .unwrap()
            .with_uint64("take", 2)
            .unwrap()
    };
    let bound = template.bind_parameters(&args(10)).unwrap();
    let root = node("a", Some(ROOT));
    let mut child = node("f", Some(FLAG));
    child
        .filter(
            "f",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::GreaterOrEqual,
                value: 10,
            },
        )
        .unwrap();
    let typed = root
        .prepare_values_with_existence(
            &[GraphExistence::exists(&child)],
            &[GraphColumn::vertex("a", "a")],
            0,
            Some(2),
        )
        .unwrap()
        .with_duplicates();
    assert_eq!(bound, typed);
    assert_eq!(calls.len(), 3);
    assert!(calls.values().all(|n| *n == 1));
    let frozen = bound.canonical_bytes();
    let vertices = [VId(0), VId(1), VId(10), VId(11)];
    assert_eq!(
        run(&bound, &vertices, &[]),
        vec![vec![Some(VId(0))], vec![Some(VId(1))]]
    );
    assert!(
        run(
            &template.bind_parameters(&args(12)).unwrap(),
            &vertices,
            &[]
        )
        .is_empty()
    );
    assert_eq!(bound.canonical_bytes(), frozen);
    assert!(calls.values().all(|n| *n == 1));
    let anti = query("MATCH (a:Root) WHERE NOT EXISTS { MATCH (f:Flag) } RETURN a");
    assert!(run(&anti, &vertices, &[]).is_empty());
    assert_eq!(run(&anti, &vertices[..2], &[]).len(), 2);
    assert!(run(&bound, &[], &[]).is_empty());
    assert!(run(&anti, &[], &[]).is_empty());
}

#[test]
fn independent_edge_scopes_match_exhaustive_occurrence_oracles() {
    let universe = [
        (VId(0), R, VId(0)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(0)),
        (VId(1), R, VId(2)),
    ];
    let vertices = [VId(0), VId(1), VId(2), VId(3)];
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        for closed in [false, true] {
            let root = node("a", Some(ROOT));
            let mut child = node("x", None);
            if !closed {
                child.vertex("y").unwrap();
            }
            child
                .edge("x", R, direction, if closed { "x" } else { "y" })
                .unwrap();
            for kind in 0..3 {
                let clause = match kind {
                    0 => GraphMatchClause::optional(&child),
                    1 => GraphMatchClause::exists(&child),
                    _ => GraphMatchClause::not_exists(&child),
                };
                let columns = if kind == 0 {
                    vec![GraphColumn::vertex("a", "a"), GraphColumn::vertex("x", "x")]
                } else {
                    vec![GraphColumn::vertex("a", "a")]
                };
                for (offset, count) in [(0, None), (0, Some(0)), (1, Some(3))] {
                    let plan = root
                        .prepare_values_with_clauses(&[clause], &columns, offset, count)
                        .unwrap()
                        .with_duplicates();
                    for mut code in 0..81 {
                        let mut edges = Vec::new();
                        for edge in universe {
                            for _ in 0..code % 3 {
                                edges.push(edge);
                            }
                            code /= 3;
                        }
                        let mut inner = Vec::new();
                        for &(s, _, d) in &edges {
                            let oriented = match direction {
                                GlaDirection::Forward => vec![(s, d)],
                                GlaDirection::Reverse => vec![(d, s)],
                                GlaDirection::Undirected if s != d => vec![(s, d), (d, s)],
                                _ => vec![(s, d)],
                            };
                            for (x, y) in oriented {
                                if !closed || x == y {
                                    inner.push(x);
                                }
                            }
                        }
                        let mut expected = Vec::new();
                        for a in [VId(0), VId(1)] {
                            if kind == 0 {
                                if inner.is_empty() {
                                    expected.push(vec![Some(a), None]);
                                } else {
                                    for &x in &inner {
                                        expected.push(vec![Some(a), Some(x)]);
                                    }
                                }
                            } else if inner.is_empty() == (kind == 2) {
                                expected.push(vec![Some(a)]);
                            }
                        }
                        expected.sort();
                        let expected: Vec<_> = expected
                            .into_iter()
                            .skip(offset as usize)
                            .take(count.map_or(usize::MAX, |n| n as usize))
                            .collect();
                        assert_eq!(
                            run(&plan, &vertices, &edges),
                            expected,
                            "{direction:?}, closed={closed}, kind={kind}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn independent_optional_ignores_unrelated_nulls_and_exports_real_correlations() {
    let vertices = [VId(0), VId(1), VId(10), VId(11)];
    let edges = [(VId(10), R, VId(11)); 2];
    let plan = query(
        "MATCH (a:Root) OPTIONAL MATCH (a)-[:R]->(b) \
        OPTIONAL MATCH (f:Flag) OPTIONAL MATCH (f)-[:R]->(x) RETURN a,b,f,x",
    );
    let mut expected = Vec::new();
    for a in [VId(0), VId(1)] {
        for _ in 0..2 {
            expected.push(vec![Some(a), None, Some(VId(10)), Some(VId(11))]);
        }
        expected.push(vec![Some(a), None, Some(VId(11)), None]);
    }
    assert_eq!(run(&plan, &vertices, &edges), expected);
    let root = node("a", Some(ROOT));
    let flag = node("f", Some(FLAG));
    let absent = node("z", Some(LabelId(99)));
    let rejected = root
        .prepare_values_with_clauses(
            &[
                GraphMatchClause::optional(&flag),
                GraphMatchClause::exists(&absent),
            ],
            &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("f", "f")],
            0,
            None,
        )
        .unwrap();
    assert!(
        run(&rejected, &vertices, &edges).is_empty(),
        "a later refusal cannot invent an OPTIONAL null row"
    );
    let no_flags = query("MATCH (a:Root) OPTIONAL MATCH (f:Flag) RETURN a,f");
    assert_eq!(
        run(&no_flags, &vertices[..2], &[]),
        vec![vec![Some(VId(0)), None], vec![Some(VId(1)), None]]
    );
}

#[test]
fn independent_walk_scopes_keep_zero_hop_isolates_and_mixed_root_multiplicity() {
    let plan = query(
        "MATCH (a:Root)-[:S]->(b) OPTIONAL MATCH WALK (x:Flag)-[:R*0..2]->(y) RETURN a,b,x,y",
    );
    assert!(!plan.plan().scans_edges());
    assert!(plan.plan().reads_edges());
    assert_eq!(plan.required_vertex_label(), None);
    let vertices = [VId(0), VId(1), VId(10), VId(11)];
    let edges = [
        (VId(0), S, VId(1)),
        (VId(0), S, VId(1)),
        (VId(10), R, VId(11)),
        (VId(10), R, VId(11)),
    ];
    let mut expected = Vec::new();
    for _ in 0..2 {
        for (x, y) in [(10, 10), (10, 11), (10, 11), (11, 11)] {
            expected.push(vec![Some(VId(0)), Some(VId(1)), Some(VId(x)), Some(VId(y))]);
        }
    }
    expected.sort();
    assert_eq!(run(&plan, &vertices, &edges), expected);
    assert_eq!(run(&plan, &vertices, &edges[..2]).len(), 4);
}

#[test]
fn independent_probes_stop_at_first_witness_but_never_convert_errors_to_absence() {
    let root = node("a", Some(ROOT));
    let mut child = node("f", Some(FLAG));
    child
        .filter(
            "f",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Greater,
                value: 0,
            },
        )
        .unwrap();
    for kind in 0..3 {
        let clause = match kind {
            0 => GraphMatchClause::optional(&child),
            1 => GraphMatchClause::exists(&child),
            _ => GraphMatchClause::not_exists(&child),
        };
        let plan = root
            .prepare_values_with_clauses(&[clause], &[GraphColumn::vertex("a", "a")], 0, Some(0))
            .unwrap();
        for witness in [false, true] {
            let mut observed = Vec::new();
            let result = plan.plan().execute_governed_with_properties(
                3,
                [VId(0), VId(10), VId(11)],
                [],
                |vid, predicates| {
                    if predicates
                        .iter()
                        .any(|p| matches!(p, VertexPredicate::IntegerProperty { .. }))
                        && vid.0 >= 10
                    {
                        observed.push(vid);
                        if vid == VId(11) {
                            return Err("unreadable independent candidate");
                        }
                        return Ok(witness);
                    }
                    Ok(matches(vid, predicates))
                },
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            );
            if witness && kind != 0 {
                assert!(result.unwrap().value.is_empty());
                assert_eq!(observed, vec![VId(10)]);
            } else {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Source("unreadable independent candidate"))
                ));
                assert_eq!(observed, vec![VId(10), VId(11)]);
            }
        }
    }
}

#[test]
fn repeated_scans_consume_sources_once_and_share_every_limit_and_checkpoint() {
    let plan =
        query("MATCH (a:Root) OPTIONAL MATCH (x)-[:R]->(y) WHERE x.p < y.p RETURN a,y LIMIT 2");
    let vertices = [VId(0), VId(1), VId(2)];
    let edges = [
        (VId(0), R, VId(1)),
        (VId(0), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let values = [
        CanonicalScalar::Int(0),
        CanonicalScalar::Int(1),
        CanonicalScalar::Int(2),
    ];
    let vertex_reads = Cell::new(0);
    let edge_reads = Cell::new(0);
    let measured = plan
        .plan()
        .execute_governed_with_properties(
            6,
            vertices
                .into_iter()
                .inspect(|_| vertex_reads.set(vertex_reads.get() + 1)),
            edges
                .into_iter()
                .inspect(|_| edge_reads.set(edge_reads.get() + 1)),
            |vid, p| Ok::<_, ()>(matches(vid, p)),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    assert_eq!(vertex_reads.get(), 3);
    assert_eq!(edge_reads.get(), 3);
    let execute = |policy| {
        plan.plan().execute_governed_with_properties(
            6,
            vertices,
            edges,
            |vid, p| Ok::<_, ()>(matches(vid, p)),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            policy,
            || Ok::<_, ()>(()),
        )
    };
    let work = measured.evaluator.work_units;
    let scratch = measured.evaluator.scratch_entries;
    assert_eq!(
        execute(GqlQueryPolicy::new(6, 2, work, scratch)).unwrap(),
        measured
    );
    for policy in [
        GqlQueryPolicy::new(5, 2, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(6, 1, u64::MAX, u64::MAX),
        GqlQueryPolicy::new(6, 2, work - 1, u64::MAX),
        GqlQueryPolicy::new(6, 2, u64::MAX, scratch - 1),
    ] {
        assert!(execute(policy).is_err());
    }
    let mut total = 0;
    plan.plan()
        .execute_governed_with_properties(
            6,
            vertices,
            edges,
            |vid, p| Ok::<_, ()>(matches(vid, p)),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || {
                total += 1;
                Ok::<_, usize>(())
            },
        )
        .unwrap();
    for stop in 1..=total {
        let mut at = 0;
        let result = plan.plan().execute_governed_with_properties(
            6,
            vertices,
            edges,
            |vid, p| Ok::<_, ()>(matches(vid, p)),
            |vid, _| Ok(Some(&values[vid.0 as usize])),
            wide(),
            || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            },
        );
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn independent_scopes_keep_names_local_and_preserve_empty_aggregate_semantics() {
    for text in [
        "MATCH (a) WHERE EXISTS { MATCH (z) } RETURN z",
        "MATCH (a) WHERE EXISTS { MATCH (z) } AND EXISTS { MATCH (x) WHERE x.p=z.p } RETURN a",
        "MATCH (a) WHERE EXISTS { MATCH (z) WHERE EXISTS { MATCH (x) } } RETURN a",
        "MATCH (a) OPTIONAL MATCH (f) WHERE EXISTS { MATCH (x) } RETURN a",
    ] {
        let mut calls = 0;
        assert!(
            PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
                calls += 1;
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls, 0);
    }
    let reused = query(
        "MATCH (a:Root) WHERE EXISTS { MATCH (z:Flag) } AND NOT EXISTS { MATCH (z:Flag) WHERE z.p>99 } RETURN a",
    );
    assert_eq!(
        run(&reused, &[VId(0), VId(10)], &[]),
        vec![vec![Some(VId(0))]]
    );
    let text = "MATCH (a:Root) OPTIONAL MATCH (f:Flag) RETURN COUNT(*) AS n,COUNT(f) AS present";
    let aggregate = PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    for (vertices, expected) in [
        (vec![VId(0), VId(1)], (2, 0)),
        (vec![VId(0), VId(1), VId(10), VId(11)], (4, 4)),
        (vec![VId(10)], (0, 0)),
        (vec![], (0, 0)),
    ] {
        let result = aggregate
            .execute_governed(
                vertices.len() as u64,
                vertices,
                [],
                |vid, p| Ok::<_, ()>(matches(vid, p)),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(result.value.len(), 1);
        assert_eq!(result.value[0].get(0).unwrap().as_count(), Some(expected.0));
        assert_eq!(result.value[0].get(1).unwrap().as_count(), Some(expected.1));
    }
}

#[test]
fn independent_frames_keep_all_columns_and_full_width_identities() {
    let root = node("a", None);
    let names: Vec<_> = (0..64).map(|at| format!("n{at}")).collect();
    let children: Vec<_> = names.iter().map(|name| node(name, None)).collect();
    let clauses: Vec<_> = children.iter().map(GraphMatchClause::optional).collect();
    let columns: Vec<_> = std::iter::once(GraphColumn::vertex("a", "a"))
        .chain(names.iter().map(|name| GraphColumn::vertex(name, name)))
        .collect();
    let plan = root
        .prepare_values_with_clauses(&clauses, &columns, 0, None)
        .unwrap();
    assert_eq!(plan.columns().len(), 65);
    assert_eq!(
        run(&plan, &[VId(u128::MAX)], &[]),
        vec![vec![Some(VId(u128::MAX)); 65]]
    );
    assert!(
        root.prepare_values_with_clauses(
            &[GraphMatchClause::optional(&children[0]); 65],
            &[GraphColumn::vertex("a", "a")],
            0,
            None
        )
        .is_err()
    );
}
