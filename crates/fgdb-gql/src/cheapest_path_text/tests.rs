use super::*;
use crate::{GraphCheapestPathError, GraphCostPath, GraphPathCostError};
use fgdb_types::{CanonicalScalar, EId};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(7);
const W: PropertyKeyId = PropertyKeyId(9);
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "ROAD") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "weight" | "cost") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}
fn text(selector: &str, mode: &str, direction: GlaDirection, interval: &str) -> String {
    let left = if direction == GlaDirection::Reverse {
        "<-"
    } else {
        "-"
    };
    let right = if direction == GlaDirection::Forward {
        "->"
    } else {
        "-"
    };
    format!(
        "MATCH p = {selector} {mode} (s){left}[e:ROAD*{interval}]{right}(t) COST e.weight RETURN p"
    )
}
fn prepare(text: &str) -> PreparedGraphCheapestPathText {
    PreparedGraphCheapestPathText::prepare(text, resolve).unwrap()
}
fn args(values: &[(&str, u64)]) -> GqlParameters {
    let mut args = GqlParameters::new();
    for &(name, value) in values {
        args.insert(name, GqlParameterValue::UInt64(value)).unwrap();
    }
    args
}
type Edge = (EId, VId, RelationId, VId);
type Answer = (i128, Vec<(EId, VId)>);
fn run(
    q: &BoundGraphCheapestPathQuery,
    vertices: &[VId],
    edges: &[Edge],
    weights: &BTreeMap<EId, CanonicalScalar>,
) -> Result<Vec<GraphCostPath>, GraphCheapestPathError<()>> {
    match q.ranked_count() {
        Some(count) => q.query().execute_k_with_control(
            count,
            vertices.iter().copied(),
            edges.iter().copied(),
            |eid, _| Ok(weights.get(&eid)),
            |_| Ok(()),
        ),
        None => q
            .query()
            .execute_with_control(
                vertices.iter().copied(),
                edges.iter().copied(),
                |eid, _| Ok(weights.get(&eid)),
                |_| Ok(()),
            )
            .map(|row| row.into_iter().collect()),
    }
}
fn plain(rows: &[GraphCostPath]) -> Vec<Answer> {
    rows.iter()
        .map(|row| (row.cost(), row.path().steps().to_vec()))
        .collect()
}

#[test]
fn selectors_modes_directions_symbols_and_wide_anchors_lower_exactly_once() {
    for (mode, expected_mode) in [
        ("WALK", GraphCheapestPathMode::Walk),
        ("TRAIL", GraphCheapestPathMode::Trail),
        ("ACYCLIC", GraphCheapestPathMode::Acyclic),
        ("SIMPLE", GraphCheapestPathMode::Simple),
    ] {
        for direction in [
            GlaDirection::Forward,
            GlaDirection::Reverse,
            GlaDirection::Undirected,
        ] {
            for (selector, count) in [("ANY CHEAPEST", None), ("CHEAPEST 7", Some(7))] {
                let input = text(selector, mode, direction, "1..3") + " AS route";
                let mut calls = Vec::new();
                let q = PreparedGraphCheapestPathText::prepare(&input, |kind, name: &str| {
                    calls.push((kind, name.to_owned()));
                    resolve(kind, name)
                })
                .unwrap();
                assert_eq!(
                    calls,
                    vec![
                        (GraphSymbolKind::Relation, "ROAD".into()),
                        (GraphSymbolKind::Property, "weight".into())
                    ]
                );
                assert_eq!(
                    (q.source_variable(), q.target_variable(), q.column_name()),
                    ("s", "t", "route")
                );
                assert!(q.parameter_schema().is_empty());
                for (source, target) in [
                    (VId(0), VId(u128::MAX)),
                    (VId(1_u128 << 100), VId(1_u128 << 120)),
                ] {
                    let bound = q.bind(source, target, &GqlParameters::new()).unwrap();
                    assert_eq!(bound.ranked_count(), count);
                    assert_eq!(bound.column_name(), "route");
                    let expected = PreparedGraphCheapestPath::new(
                        source,
                        target,
                        R,
                        direction,
                        W,
                        GraphWalkBounds::new(1, 3).unwrap(),
                    )
                    .unwrap()
                    .with_mode(expected_mode);
                    assert_eq!(bound.query().canonical_bytes(), expected.canonical_bytes());
                }
                assert_eq!(calls.len(), 2);
            }
        }
    }
    let q = prepare("match p = any cheapest (s)-[e:ROAD*0]->(t) cost e.cost return p");
    assert_eq!(
        q.bind(VId(0), VId(0), &GqlParameters::new())
            .unwrap()
            .query()
            .mode(),
        GraphCheapestPathMode::Walk
    );
}

#[test]
fn repeated_parameters_are_exact_typed_and_never_substituted_into_text() {
    let input = text("CHEAPEST $n", "TRAIL", GlaDirection::Forward, "$n..$n");
    let q = prepare(&input);
    assert_eq!(
        q.parameter_schema(),
        &[GqlParameterSpec {
            name: "n".into(),
            parameter_type: GqlParameterType::UInt64,
            requires_positive: false,
            occurrences: 3,
        }]
    );
    for n in [0, 1, u64::from(MAX_GRAPH_WALK_HOPS)] {
        let bound = q.bind(VId(0), VId(u128::MAX), &args(&[("n", n)])).unwrap();
        assert_eq!(bound.ranked_count(), Some(n));
        assert_eq!(
            bound.query().bounds(),
            GraphWalkBounds::new(n as u32, n as u32).unwrap()
        );
    }
    let missing = q.bind(VId(0), VId(1), &GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, input.find("$n").unwrap());
    assert_eq!(
        missing.kind,
        GraphCheapestPathTextErrorKind::Pattern(GraphPatternTextErrorKind::MissingParameter)
    );
    let signed = GqlParameters::new().with_int64("n", 1).unwrap();
    assert!(matches!(
        q.bind(VId(0), VId(1), &signed).unwrap_err().kind,
        GraphCheapestPathTextErrorKind::Pattern(
            GraphPatternTextErrorKind::ParameterTypeMismatch { .. }
        )
    ));
    assert!(q.bind(VId(0), VId(1), &args(&[("N", 1)])).is_err());
    assert!(matches!(
        q.bind(VId(0), VId(1), &args(&[("n", 1), ("extra", 1)]))
            .unwrap_err()
            .kind,
        GraphCheapestPathTextErrorKind::Pattern(GraphPatternTextErrorKind::UnexpectedArguments)
    ));
    for n in [1025, u64::MAX] {
        assert_eq!(
            q.bind(VId(0), VId(1), &args(&[("n", n)])).unwrap_err().kind,
            GraphCheapestPathTextErrorKind::InvalidHopBounds
        );
    }
    let separate = prepare(&text(
        "CHEAPEST $k",
        "WALK",
        GlaDirection::Forward,
        "$lo..$hi",
    ));
    assert!(
        separate
            .bind(VId(0), VId(1), &args(&[("k", 0), ("lo", 2), ("hi", 1)]))
            .is_err()
    );
    assert!(
        separate
            .bind(VId(0), VId(1), &args(&[("k", 0), ("lo", 0)]))
            .is_err()
    );
    assert_eq!(
        separate
            .bind(
                VId(0),
                VId(1),
                &args(&[("k", u64::MAX), ("lo", 0), ("hi", 1)])
            )
            .unwrap()
            .ranked_count(),
        Some(u64::MAX)
    );
}

#[test]
fn repeated_vertex_variables_enforce_identity_and_debug_does_not_export_secrets() {
    let input = "MATCH secret_path = CHEAPEST $private_count SIMPLE (v)-[e:ROAD*0..4]->(v) COST e.weight RETURN secret_path";
    let q = prepare(input);
    let mismatch = q
        .bind(
            VId(1_u128 << 100),
            VId(u128::MAX),
            &args(&[("private_count", 0)]),
        )
        .unwrap_err();
    assert_eq!(
        mismatch.kind,
        GraphCheapestPathTextErrorKind::AnchorMismatch
    );
    assert_eq!(mismatch.offset, input.rfind("(v)").unwrap() + 1);
    let bound = q
        .bind(
            VId(u128::MAX),
            VId(u128::MAX),
            &args(&[("private_count", u64::MAX)]),
        )
        .unwrap();
    for rendered in [
        format!("{q:?}"),
        format!("{bound:?}"),
        format!("{mismatch:?}"),
        mismatch.to_string(),
    ] {
        for secret in [
            "secret_path",
            "private_count",
            &u128::MAX.to_string(),
            &u64::MAX.to_string(),
        ] {
            assert!(
                !rendered.contains(secret),
                "diagnostic leaked a data operand"
            );
        }
    }
}

#[test]
fn malformed_or_unsupported_statements_refuse_before_any_catalog_access() {
    let base = text("ANY CHEAPEST", "WALK", GlaDirection::Forward, "1..4");
    let invalid = [
        base.replace("ANY CHEAPEST", "ALL CHEAPEST"),
        base.replace("ANY CHEAPEST", "CHEAPEST -1"),
        base.replace("1..4", "4..1"),
        base.replace("1..4", "0..1025"),
        base.replace("1..4", "$lo..1025"),
        base.replace("1..4", "1025..$hi"),
        base.replace("1..4", "1.."),
        base.replace("1..4", ""),
        base.replace("1..4", "1.4"),
        base.replace("1..4", "0..18446744073709551616"),
        base.replace("(s)-[", "(s)<-["),
        base.replace("(s)", "(s:Label)"),
        base.replace("COST e.weight", "COST s.weight"),
        base.replace("COST e.weight", "COST e.weight + 1"),
        base.replace("RETURN p", "RETURN e"),
        base.replace("RETURN p", "RETURN DISTINCT p"),
        base.replace("(s)", "(p)"),
        base.replace("[e:", "[s:"),
        base.clone() + " ORDER BY p",
        base.clone() + " LIMIT 1",
        base.clone() + " SKIP 0",
        base.clone() + " MATCH (n) RETURN n",
        base.clone() + ";",
        base.clone() + " RETURN p",
    ];
    for input in invalid {
        let refused =
            PreparedGraphCheapestPathText::prepare(&input, |_, _: &str| -> Option<GraphSymbol> {
                panic!("catalog observed invalid syntax");
            });
        assert!(refused.is_err(), "accepted unsupported statement: {input}");
    }
    // The shared lexer validates the entire input, not just the selected prefix.
    let oversized = " ".repeat(crate::MAX_GRAPH_TEXT_BYTES + 1);
    let too_many = "( ".repeat(crate::MAX_GRAPH_TEXT_TOKENS + 1);
    let long_name = base.replace(
        "ROAD",
        &"X".repeat(crate::algebra::MAX_PATTERN_NAME_BYTES + 1),
    );
    for input in [oversized, too_many, long_name] {
        assert!(
            PreparedGraphCheapestPathText::prepare(&input, |_, _: &str| -> Option<GraphSymbol> {
                panic!("lexical refusal reached catalog");
            })
            .is_err()
        );
    }
    let unicode = format!(
        "\u{2003}{}",
        base.replace("COST e.weight", "COST wrong.weight")
    );
    assert_eq!(
        PreparedGraphCheapestPathText::prepare(&unicode, resolve)
            .unwrap_err()
            .offset,
        unicode.find("wrong").unwrap()
    );
}

#[test]
fn resolver_domains_are_checked_without_casting_or_fallback() {
    let input = text("ANY CHEAPEST", "WALK", GlaDirection::Forward, "0..1");
    for fail_domain in [GraphSymbolKind::Relation, GraphSymbolKind::Property] {
        let mut calls = 0;
        let error = PreparedGraphCheapestPathText::prepare(&input, |kind, name: &str| {
            calls += 1;
            if kind == fail_domain {
                None
            } else {
                resolve(kind, name)
            }
        })
        .unwrap_err();
        assert_eq!(
            calls,
            if fail_domain == GraphSymbolKind::Relation {
                1
            } else {
                2
            }
        );
        assert_eq!(
            error.kind,
            GraphCheapestPathTextErrorKind::Pattern(GraphPatternTextErrorKind::UnknownSymbol(
                fail_domain
            ))
        );
    }
    let error =
        PreparedGraphCheapestPathText::prepare(&input, |_, _: &str| Some(GraphSymbol::Property(W)))
            .unwrap_err();
    assert!(matches!(
        error.kind,
        GraphCheapestPathTextErrorKind::Pattern(GraphPatternTextErrorKind::WrongSymbolKind { .. })
    ));
}

#[test]
fn bound_identity_pins_semantics_and_ignores_literal_versus_argument_spelling() {
    let template = prepare(&text(
        "CHEAPEST $k",
        "TRAIL",
        GlaDirection::Forward,
        "$lo..$hi",
    ));
    let literal = prepare(&text("CHEAPEST 3", "TRAIL", GlaDirection::Forward, "0..4"));
    let values = args(&[("k", 3), ("lo", 0), ("hi", 4)]);
    let base = template
        .bind(VId(0), VId(u128::MAX), &values)
        .unwrap()
        .canonical_bytes();
    assert_eq!(
        base,
        literal
            .bind(VId(0), VId(u128::MAX), &GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    );
    let mut identities = BTreeSet::from([base]);
    for input in [
        text("CHEAPEST 2", "TRAIL", GlaDirection::Forward, "0..4"),
        text("ANY CHEAPEST", "TRAIL", GlaDirection::Forward, "0..4"),
        text("CHEAPEST 3", "WALK", GlaDirection::Forward, "0..4"),
        text("CHEAPEST 3", "TRAIL", GlaDirection::Reverse, "0..4"),
        text("CHEAPEST 3", "TRAIL", GlaDirection::Forward, "1..4"),
        text("CHEAPEST 3", "TRAIL", GlaDirection::Forward, "0..4") + " AS route",
    ] {
        identities.insert(
            prepare(&input)
                .bind(VId(0), VId(u128::MAX), &GqlParameters::new())
                .unwrap()
                .canonical_bytes(),
        );
    }
    identities.insert(
        template
            .bind(VId(1), VId(u128::MAX), &values)
            .unwrap()
            .canonical_bytes(),
    );
    assert_eq!(identities.len(), 8);
}

// Oracle expands ALL bounded walks without prefix dominance/pruning. Only
// complete routes are filtered by the mode, sorted, and limited afterward.
fn oracle(
    q: &BoundGraphCheapestPathQuery,
    edges: &[Edge],
    weights: &BTreeMap<EId, CanonicalScalar>,
) -> Vec<Answer> {
    let query = q.query();
    let mut layer = vec![(query.source(), 0_i128, Vec::<(EId, VId)>::new())];
    let mut rows = Vec::new();
    for depth in 0..=query.bounds().maximum() {
        if depth >= query.bounds().minimum() {
            for (end, cost, steps) in &layer {
                if *end != query.target() {
                    continue;
                }
                let allowed = match query.mode() {
                    GraphCheapestPathMode::Walk => true,
                    GraphCheapestPathMode::Trail => {
                        steps
                            .iter()
                            .map(|&(eid, _)| eid)
                            .collect::<BTreeSet<_>>()
                            .len()
                            == steps.len()
                    }
                    mode => {
                        let mut vertices: Vec<_> = std::iter::once(query.source())
                            .chain(steps.iter().map(|&(_, vid)| vid))
                            .collect();
                        if mode == GraphCheapestPathMode::Simple
                            && vertices.len() > 1
                            && vertices.last() == Some(&query.source())
                        {
                            vertices.pop();
                        }
                        vertices.iter().collect::<BTreeSet<_>>().len() == vertices.len()
                    }
                };
                if allowed {
                    rows.push((*cost, steps.clone()));
                }
            }
        }
        if depth == query.bounds().maximum() {
            break;
        }
        let mut next = Vec::new();
        for (end, cost, steps) in layer {
            for &(eid, from, relation, to) in edges {
                if relation != query.relation() {
                    continue;
                }
                let CanonicalScalar::Int(weight) = &weights[&eid] else {
                    panic!("integer fixture");
                };
                let mut destinations = Vec::new();
                if query.direction() != GlaDirection::Reverse && from == end {
                    destinations.push(to);
                }
                if query.direction() != GlaDirection::Forward
                    && to == end
                    && (query.direction() != GlaDirection::Undirected || from != to)
                {
                    destinations.push(from);
                }
                for destination in destinations {
                    let mut child = steps.clone();
                    child.push((eid, destination));
                    next.push((destination, cost + i128::from(*weight), child));
                }
            }
        }
        layer = next;
    }
    rows.sort();
    rows.truncate(usize::try_from(q.ranked_count().unwrap_or(1)).unwrap_or(usize::MAX));
    rows
}

#[test]
fn parsed_bound_queries_match_independent_complete_routes_in_every_mode_and_direction() {
    let vertices = [VId(0), VId(1)];
    let fixture = [(1, 0, 0, -2), (2, 0, 1, 4), (3, 0, 1, 4), (4, 1, 0, -7)];
    for mask in 0..16 {
        let selected: Vec<_> = fixture
            .iter()
            .enumerate()
            .filter(|(at, _)| mask & (1 << at) != 0)
            .map(|(_, edge)| *edge)
            .collect();
        let edges: Vec<_> = selected
            .iter()
            .map(|&(eid, from, to, _)| (EId(eid), VId(from), R, VId(to)))
            .collect();
        let weights: BTreeMap<_, _> = selected
            .iter()
            .map(|&(eid, _, _, weight)| (EId(eid), CanonicalScalar::Int(weight)))
            .collect();
        for mode in ["WALK", "TRAIL", "ACYCLIC", "SIMPLE"] {
            for direction in [
                GlaDirection::Forward,
                GlaDirection::Reverse,
                GlaDirection::Undirected,
            ] {
                for selector in ["ANY CHEAPEST", "CHEAPEST $k"] {
                    let q = prepare(&text(selector, mode, direction, "$lo..$hi"));
                    for (lo, hi) in [(0, 0), (0, 3), (2, 3), (3, 3)] {
                        let mut values = args(&[("lo", lo), ("hi", hi)]);
                        if selector.starts_with("CHEAPEST") {
                            values = values.with_uint64("k", 3).unwrap();
                        }
                        for target in vertices {
                            let bound = q.bind(VId(0), target, &values).unwrap();
                            assert_eq!(
                                plain(&run(&bound, &vertices, &edges, &weights).unwrap()),
                                oracle(&bound, &edges, &weights)
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn zero_ranked_count_still_admits_the_complete_selected_cost_domain() {
    let bound = prepare(&text("CHEAPEST 0", "ACYCLIC", GlaDirection::Forward, "0"))
        .bind(VId(99), VId(100), &GqlParameters::new())
        .unwrap();
    let edges = [(EId(1), VId(0), R, VId(1))];
    assert_eq!(
        run(&bound, &[VId(0), VId(1)], &edges, &BTreeMap::new()),
        Err(GraphCheapestPathError::Cost(
            GraphPathCostError::MissingWeight
        ))
    );
    let weights = BTreeMap::from([(EId(1), CanonicalScalar::Int(i64::MIN))]);
    assert!(
        run(&bound, &[VId(0), VId(1)], &edges, &weights)
            .unwrap()
            .is_empty()
    );
}
