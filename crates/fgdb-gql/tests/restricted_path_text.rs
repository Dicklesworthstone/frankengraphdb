//! Vertex-restricted native paths use the shared GLA, scope and row consumers.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GlaOperator, GraphColumn, GraphPatternBuilder, GraphValue, GraphValueRow,
    GraphWalkSearch, PreparedGraphPattern,
};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
    GraphWalkBounds, PreparedGraphAggregateText, PreparedGraphSetText, PreparedGraphText,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, EId, VId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
type Edge = (VId, RelationId, VId);
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn execute<C>(
    pattern: &PreparedGraphPattern<GraphValueRow>,
    vertices: &[VId],
    edges: &[Edge],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), C>> {
    let properties = vertices
        .iter()
        .map(|v| (*v, CanonicalScalar::Int(v.0 as i64)))
        .collect::<BTreeMap<_, _>>();
    pattern.plan().execute_governed_with_properties(
        (vertices.len() + edges.len()) as u64,
        vertices.iter().copied(),
        edges.iter().copied(),
        |v, predicates| {
            Ok(predicates
                .iter()
                .all(|predicate| predicate.matches(&[], &[(P, properties[&v].clone())])))
        },
        |v, key| Ok(properties.get(&v).filter(|_| key == P)),
        policy,
        checkpoint,
    )
}
fn run(text: &str, vertices: &[VId], edges: &[Edge]) -> Vec<GraphValueRow> {
    execute(&prepare(text), vertices, edges, wide(), || Ok::<_, ()>(()))
        .unwrap()
        .value
}
fn endpoints(rows: &[GraphValueRow]) -> Vec<Option<VId>> {
    rows.iter().map(|row| row.values()[0].as_vertex()).collect()
}

#[test]
fn native_modes_equal_typed_atoms_and_keep_distinct_transcripts() {
    let mut transcripts = BTreeSet::new();
    for (mode, search) in [
        ("WALK", GraphWalkSearch::All),
        ("ACYCLIC", GraphWalkSearch::Acyclic),
        ("SIMPLE", GraphWalkSearch::Simple),
    ] {
        let native = prepare(&format!("MATCH {mode} (a)-[:R*0..4]->(b) RETURN ALL a,b"));
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap();
        builder.vertex("b").unwrap();
        let bounds = GraphWalkBounds::new(0, 4).unwrap();
        match mode {
            "ACYCLIC" => {
                builder
                    .acyclic_walk("a", R, GlaDirection::Forward, "b", bounds)
                    .unwrap();
            }
            "SIMPLE" => {
                builder
                    .simple_walk("a", R, GlaDirection::Forward, "b", bounds)
                    .unwrap();
            }
            _ => {
                builder
                    .walk("a", R, GlaDirection::Forward, "b", bounds)
                    .unwrap();
            }
        }
        let typed = builder
            .prepare_values(
                &[GraphColumn::vertex("a", "a"), GraphColumn::vertex("b", "b")],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        assert_eq!(native, typed);
        assert!(native.plan().operators().iter().any(|op|
            matches!(op, GlaOperator::VarLengthExpand { search: actual, .. } if *actual == search)));
        assert!(transcripts.insert(native.canonical_bytes()));
    }
}

#[test]
fn all_directions_bounds_and_parallel_occurrences_match_full_path_enumeration() {
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
        (VId(3), R, VId(1)),
        (VId(2), R, VId(2)),
    ];
    for direction in [
        GlaDirection::Forward,
        GlaDirection::Reverse,
        GlaDirection::Undirected,
    ] {
        let mut oriented = Vec::new();
        for &(a, _, b) in &edges {
            match direction {
                GlaDirection::Forward => oriented.push((a, b)),
                GlaDirection::Reverse => oriented.push((b, a)),
                GlaDirection::Undirected => {
                    oriented.push((a, b));
                    if a != b {
                        oriented.push((b, a));
                    }
                }
            }
        }
        for maximum in 0..=4 {
            for minimum in 0..=maximum {
                for mode in ["ACYCLIC", "SIMPLE"] {
                    let mut expected = Vec::new();
                    for &source in &vertices {
                        let mut layer = vec![vec![source]];
                        for depth in 0..=maximum {
                            if depth >= minimum {
                                for path in &layer {
                                    let checked = if mode == "SIMPLE"
                                        && path.len() > 1
                                        && path.first() == path.last()
                                    {
                                        &path[..path.len() - 1]
                                    } else {
                                        path.as_slice()
                                    };
                                    if checked.iter().collect::<BTreeSet<_>>().len()
                                        == checked.len()
                                    {
                                        expected.push((source, *path.last().unwrap()));
                                    }
                                }
                            }
                            let mut next = Vec::new();
                            for path in layer {
                                for &(_, destination) in
                                    oriented.iter().filter(|(a, _)| Some(a) == path.last())
                                {
                                    let mut extended = path.clone();
                                    extended.push(destination);
                                    next.push(extended);
                                }
                            }
                            layer = next;
                        }
                    }
                    let atom = match direction {
                        GlaDirection::Forward => format!("(a)-[:R*{minimum}..{maximum}]->(b)"),
                        GlaDirection::Reverse => format!("(a)<-[:R*{minimum}..{maximum}]-(b)"),
                        GlaDirection::Undirected => format!("(a)-[:R*{minimum}..{maximum}]-(b)"),
                    };
                    let rows = run(
                        &format!("MATCH {mode} {atom} RETURN ALL a,b"),
                        &vertices,
                        &edges,
                    );
                    let mut actual = rows
                        .iter()
                        .map(|row| {
                            (
                                row.values()[0].as_vertex().unwrap(),
                                row.values()[1].as_vertex().unwrap(),
                            )
                        })
                        .collect::<Vec<_>>();
                    actual.sort();
                    expected.sort();
                    assert_eq!(actual, expected, "{mode} {atom}");
                }
            }
        }
    }
}

#[test]
fn optional_required_and_existence_scopes_preserve_null_and_bag_semantics() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(2), R, VId(3)),
    ];
    let head = "MATCH (a) WHERE a.p=1 OPTIONAL MATCH";
    assert_eq!(
        endpoints(&run(
            &format!("{head} ACYCLIC (a)-[:R*1..3]->(b) RETURN b"),
            &vertices,
            &edges
        )),
        vec![None]
    );
    assert_eq!(
        endpoints(&run(
            &format!("{head} SIMPLE (a)-[:R*1..3]->(b) RETURN b"),
            &vertices,
            &edges
        )),
        vec![Some(VId(1)), Some(VId(1))]
    );
    assert!(
        run(
            &format!("{head} ACYCLIC (a)-[:R*1..3]->(b) MATCH ACYCLIC (b)-[:R*0]->(c) RETURN c"),
            &vertices,
            &edges
        )
        .is_empty()
    );
    for (mode, expected) in [
        ("ACYCLIC", vec![Some(VId(2))]),
        ("SIMPLE", vec![Some(VId(1)), Some(VId(2))]),
    ] {
        let text = format!("MATCH (a) WHERE EXISTS {{ MATCH {mode} (a)-[:R*1..3]->(b) }} RETURN a");
        assert_eq!(endpoints(&run(&text, &vertices, &edges)), expected);
    }
    let text = "MATCH (a) WHERE NOT EXISTS { MATCH ACYCLIC (a)-[:R*1..3]->(b) } RETURN a";
    assert_eq!(
        endpoints(&run(text, &vertices, &edges)),
        vec![Some(VId(1)), Some(VId(3))]
    );
    let path = [(VId(1), R, VId(2)), (VId(2), R, VId(3))];
    assert_eq!(
        endpoints(&run(
            "MATCH ACYCLIC (a)-[:R*1..2]->(b) WHERE a.p=1 AND b.p=3 RETURN b",
            &vertices,
            &path
        )),
        vec![Some(VId(3))],
        "rejected endpoints must remain possible transit vertices"
    );
}

#[test]
fn parameter_binding_and_per_clause_modes_do_not_reparse_or_reresolve() {
    let text = "MATCH ACYCLIC (a)-[:R*1..3]->(b) WHERE a.p=$source MATCH SIMPLE (b)<-[:R*0..2]-(c) RETURN ALL a,c SKIP $skip LIMIT $take";
    let mut calls = BTreeSet::new();
    let template = PreparedGraphText::prepare(text, |kind: GraphSymbolKind, name: &str| {
        assert!(calls.insert((kind, name.to_owned())));
        symbols(kind, name)
    })
    .unwrap();
    let args = GqlParameters::new()
        .with_int64("source", 1)
        .unwrap()
        .with_uint64("skip", 0)
        .unwrap()
        .with_uint64("take", 10)
        .unwrap();
    let a = template.bind_parameters(&args).unwrap();
    assert_eq!(a, template.bind_parameters(&args).unwrap());
    let searches = a
        .plan()
        .operators()
        .iter()
        .filter_map(|op| match op {
            GlaOperator::VarLengthExpand { search, .. } => Some(*search),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        searches,
        vec![GraphWalkSearch::Acyclic, GraphWalkSearch::Simple]
    );
    assert_eq!(calls.len(), 2);
    assert!(template.bind_parameters(&GqlParameters::new()).is_err());
    assert_eq!(template.statement(), text);
}

#[test]
fn unsupported_compound_unbounded_and_shortest_restrictions_refuse_before_catalog() {
    for text in [
        "MATCH ACYCLIC (a)-[:R*]->(b) RETURN b",
        "MATCH SIMPLE (a)-[:R*2..1]->(b) RETURN b",
        "MATCH ACYCLIC (a)-[:R*1025]->(b) RETURN b LIMIT 0",
        "MATCH SIMPLE (a)-[:R]->(b) RETURN b",
        "MATCH ACYCLIC (a) RETURN a",
        "MATCH SIMPLE (a)-[:R*1]->(b)-[:R*1]->(c) RETURN c",
        "MATCH ACYCLIC (a)-[:R*1]->(b),(x) RETURN x",
        "MATCH ANY SHORTEST ACYCLIC (a)-[:R*1..3]->(b) RETURN b",
        "MATCH ALL SHORTEST SIMPLE (a)-[:R*1..3]->(b) RETURN b",
    ] {
        let result = PreparedGraphText::prepare(
            text,
            |_: GraphSymbolKind, _: &str| -> Option<GraphSymbol> {
                panic!("catalog consulted for {text}")
            },
        );
        assert!(
            result.is_err(),
            "accepted unsupported path semantics: {text}"
        );
    }
}

#[test]
fn all_query_limits_and_every_interrupt_boundary_cover_restriction_checks() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(1)),
        (VId(2), R, VId(3)),
    ];
    for mode in ["ACYCLIC", "SIMPLE"] {
        let pattern = prepare(&format!("MATCH {mode} (a)-[:R*0..4]->(b) RETURN ALL a,b"));
        let mut calls = 0;
        let measured = execute(&pattern, &vertices, &edges, wide(), || {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            execute(
                &pattern,
                &vertices,
                &edges,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                || Ok::<_, ()>(())
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            let mut cap = caps;
            cap[dimension] -= 1;
            assert!(
                execute(
                    &pattern,
                    &vertices,
                    &edges,
                    GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]),
                    || Ok::<_, ()>(())
                )
                .is_err()
            );
        }
        for stop in 1..=calls {
            let mut count = 0;
            let result = execute(&pattern, &vertices, &edges, wide(), || {
                count += 1;
                if count == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(count, stop);
        }
    }
}

#[test]
fn aggregate_row_pipeline_and_write_script_consumers_keep_path_multiplicity() {
    let vertices = [VId(1), VId(2)];
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(1)),
    ];
    for (mode, count) in [("ACYCLIC", 3), ("SIMPLE", 7)] {
        let prefix = format!("MATCH {mode} (a)-[:R*1..3]->(b)");
        let query = PreparedGraphAggregateText::prepare(
            &format!("{prefix} RETURN COUNT(*) AS paths"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let rows = query
            .execute_governed(
                5,
                vertices,
                edges,
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok::<Option<&CanonicalScalar>, ()>(None),
                wide(),
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows[0].values()[0].as_count(), Some(count));
        let pipeline = PreparedGraphSetText::prepare(
            &format!("{prefix} WITH b AS endpoint WHERE endpoint IS NOT NULL RETURN endpoint"),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let rows = pipeline
            .execute_governed(
                wide(),
                |pattern, allowance| {
                    execute(pattern, &vertices, &edges, allowance, || Ok::<_, ()>(()))
                },
                || Ok::<_, ()>(()),
            )
            .unwrap()
            .value;
        assert_eq!(rows.len() as u64, count);
        let script = PreparedGraphWriteScript::prepare(
            &format!("{prefix} CREATE (n {{p:$value}}); MATCH (n) WHERE n.p=$value SET n.p=0"),
            R,
            symbols,
        )
        .unwrap();
        let args = GqlParameters::new().with_int64("value", 42).unwrap();
        assert_eq!(script.bind_parameters(&args).unwrap().statements().len(), 2);
    }
}

#[test]
fn zero_result_limit_does_not_mask_fallible_endpoint_property_reads() {
    for mode in ["ACYCLIC", "SIMPLE"] {
        let query = prepare(&format!(
            "MATCH {mode} (a)-[:R*1..2]->(b) RETURN b.p LIMIT 0"
        ));
        let result = query.plan().execute_governed_with_properties(
            3,
            [VId(1), VId(2)],
            [(VId(1), R, VId(2))],
            |_, _| Ok::<_, &str>(true),
            |_, _| Err::<Option<&CanonicalScalar>, _>("endpoint read"),
            wide(),
            || Ok::<_, ()>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("endpoint read"))
        ));
    }
}

type IdentifiedEdge = (EId, VId, RelationId, VId);
type Route = (VId, Vec<(EId, VId)>);
fn captured<C>(
    plan: &PreparedGraphPattern<GraphValueRow>,
    vertices: &[VId],
    edges: &[IdentifiedEdge],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), C>> {
    plan.plan().execute_governed_with_identified_properties(
        (vertices.len() + edges.len()) as u64,
        vertices.iter().copied(),
        edges.iter().copied(),
        |_, _| Ok(true),
        |_, _| Ok(None),
        policy,
        checkpoint,
    )
}
fn routes(rows: &[GraphValueRow]) -> Vec<Route> {
    rows.iter()
        .map(|row| {
            let path = row.get(0).unwrap().as_path().unwrap();
            (path.start(), path.steps().to_vec())
        })
        .collect()
}

#[test]
fn captured_restrictions_preserve_real_edges_against_unpruned_walk_filtering() {
    let vertices = [VId(1), VId(2), VId(3), VId(4)];
    let edges = [
        (EId(90), VId(1), R, VId(1)),
        (EId(11), VId(1), R, VId(2)),
        (EId(12), VId(1), R, VId(2)),
        (EId(7), VId(2), R, VId(3)),
        (EId(80), VId(3), R, VId(1)),
        (EId(3), VId(2), R, VId(2)),
    ];
    for direction in 0..3 {
        let mut oriented = Vec::new();
        for &(edge, source, _, target) in &edges {
            if direction == 1 {
                oriented.push((edge, target, source));
            } else {
                oriented.push((edge, source, target));
                if direction == 2 && source != target {
                    oriented.push((edge, target, source));
                }
            }
        }
        for maximum in 0..=4 {
            for minimum in 0..=maximum {
                for mode in ["ACYCLIC", "SIMPLE"] {
                    let mut expected = Vec::new();
                    for &start in &vertices {
                        let mut layer: Vec<Vec<(EId, VId)>> = vec![Vec::new()];
                        for depth in 0..=maximum {
                            if depth >= minimum {
                                for steps in &layer {
                                    let nodes: Vec<_> = core::iter::once(start)
                                        .chain(steps.iter().map(|step| step.1))
                                        .collect();
                                    let check = if mode == "SIMPLE"
                                        && !steps.is_empty()
                                        && nodes.first() == nodes.last()
                                    {
                                        &nodes[..nodes.len() - 1]
                                    } else {
                                        nodes.as_slice()
                                    };
                                    if check.iter().collect::<BTreeSet<_>>().len() == check.len() {
                                        expected.push((start, steps.clone()));
                                    }
                                }
                            }
                            let mut next = Vec::new();
                            for steps in layer {
                                let end = steps.last().map_or(start, |step| step.1);
                                for &(edge, _, target) in
                                    oriented.iter().filter(|(_, source, _)| *source == end)
                                {
                                    let mut child = steps.clone();
                                    child.push((edge, target));
                                    next.push(child);
                                }
                            }
                            layer = next;
                        }
                    }
                    let atom = match direction {
                        0 => format!("(a)-[:R*{minimum}..{maximum}]->(b)"),
                        1 => format!("(a)<-[:R*{minimum}..{maximum}]-(b)"),
                        _ => format!("(a)-[:R*{minimum}..{maximum}]-(b)"),
                    };
                    let plan = prepare(&format!("MATCH p = {mode} {atom} RETURN ALL p"));
                    let mut actual = routes(
                        &captured(&plan, &vertices, &edges, wide(), || Ok::<_, ()>(()))
                            .unwrap()
                            .value,
                    );
                    actual.sort();
                    expected.sort();
                    assert_eq!(actual, expected, "{mode} {atom}");
                    let reversed = edges.iter().copied().rev().collect::<Vec<_>>();
                    assert_eq!(
                        routes(
                            &captured(&plan, &vertices, &reversed, wide(), || Ok::<_, ()>(()))
                                .unwrap()
                                .value
                        ),
                        actual
                    );
                }
            }
        }
    }
}

#[test]
fn captured_simple_closures_and_path_functions_do_not_fabricate_longer_cycles() {
    let vertices = [VId(1), VId(2)];
    let edges = [
        (EId(9), VId(1), R, VId(1)),
        (EId(2), VId(1), R, VId(2)),
        (EId(3), VId(2), R, VId(1)),
    ];
    for (mode, count) in [("ACYCLIC", 4), ("SIMPLE", 7)] {
        let plan = prepare(&format!(
            "MATCH p = {mode} (a)-[:R*0..1024]->(b) RETURN p,PATH_LENGTH(p) AS hops,NODES(p) AS vertices,EDGES(p) AS relationships"
        ));
        let rows = captured(&plan, &vertices, &edges, wide(), || Ok::<_, ()>(()))
            .unwrap()
            .value;
        assert_eq!(rows.len(), count);
        for row in rows {
            let path = row.get(0).unwrap().as_path().unwrap();
            assert_eq!(
                row.get(1).unwrap().as_scalar(),
                Some(&CanonicalScalar::Int(path.len() as i64))
            );
            let GraphValue::Vertices(nodes) = row.get(2).unwrap() else {
                panic!("node identities");
            };
            let GraphValue::Edges(ids) = row.get(3).unwrap() else {
                panic!("edge identities");
            };
            assert_eq!(nodes.as_ref(), path.nodes().collect::<Vec<_>>());
            assert_eq!(ids.as_ref(), path.edges().collect::<Vec<_>>());
        }
        let impossible = prepare(&format!("MATCH p = {mode} (a)-[:R*3..1024]->(b) RETURN p"));
        assert!(
            captured(&impossible, &vertices, &edges, wide(), || Ok::<_, ()>(()))
                .unwrap()
                .value
                .is_empty()
        );
    }
}

#[test]
fn captured_restrictions_share_all_limits_and_every_checkpoint() {
    let vertices = [VId(1), VId(2), VId(3)];
    let edges = [
        (EId(2), VId(1), R, VId(2)),
        (EId(4), VId(1), R, VId(2)),
        (EId(8), VId(2), R, VId(1)),
        (EId(3), VId(2), R, VId(3)),
    ];
    for mode in ["ACYCLIC", "SIMPLE"] {
        let plan = prepare(&format!("MATCH p = {mode} (a)-[:R*0..4]->(b) RETURN ALL p"));
        let mut calls = 0;
        let measured = captured(&plan, &vertices, &edges, wide(), || {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
        let caps = [
            measured.rows.snapshot_records,
            measured.rows.result_rows,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        ];
        assert_eq!(
            captured(
                &plan,
                &vertices,
                &edges,
                GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]),
                || Ok::<_, ()>(())
            )
            .unwrap(),
            measured
        );
        for dimension in 0..4 {
            let mut cap = caps;
            cap[dimension] -= 1;
            assert!(
                captured(
                    &plan,
                    &vertices,
                    &edges,
                    GqlQueryPolicy::new(cap[0], cap[1], cap[2], cap[3]),
                    || Ok::<_, ()>(())
                )
                .is_err()
            );
        }
        for stop in 1..=calls {
            let mut count = 0;
            let result = captured(&plan, &vertices, &edges, wide(), || {
                count += 1;
                if count == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(count, stop);
        }
        // Supplying anonymous topology is never permission to invent edge IDs.
        assert!(matches!(
            plan.plan().execute_governed_with_properties(
                0,
                [],
                [],
                |_, _| Ok::<_, ()>(true),
                |_, _| Ok(None),
                wide(),
                || Ok::<_, ()>(())
            ),
            Err(GqlQueryError::IdentifiedEdgesRequired)
        ));
    }
}
