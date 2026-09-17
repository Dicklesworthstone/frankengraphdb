//! Native graph creation is compiled into the typed insertion kernel. Tests
//! inspect complete generated intents, not just successful syntax admission.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::*;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphInsertTextErrorKind,
    GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind, PreparedGraphInsertText,
    PreparedGraphMutationText,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, EId, VId};
use std::cell::Cell;
use std::collections::BTreeMap;

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const COPY: LabelId = LabelId(9);
type Props = BTreeMap<(VId, PropertyKeyId), CanonicalScalar>;
type Triple = (VId, RelationId, VId);
type ResultOf = Result<GraphInsertBatch, GqlQueryError<GraphInsertError<(), ()>, ()>>;
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(COPY)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn policy() -> GraphInsertPolicy {
    GraphInsertPolicy::new(
        GqlQueryPolicy::new(1_000, 1_000, 2_000_000, 1_000_000),
        1_000,
        1_000,
    )
}
fn prepare(text: &str) -> PreparedGraphInsert {
    PreparedGraphInsertText::prepare(text, R, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn identity(request: GraphInsertRequest) -> Result<ElementId, ()> {
    Ok(match request {
        GraphInsertRequest::Vertex { row, vertex } => {
            ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128))
        }
        GraphInsertRequest::Edge { row, edge } => {
            ElementId::Edge(EId(1_000 + row as u128 * 16 + edge as u128))
        }
    })
}
fn run(plan: &PreparedGraphInsert, vertices: &[VId], edges: &[Triple], props: &Props) -> ResultOf {
    plan.execute_governed(
        policy(),
        |selection, allowance| {
            selection.plan().execute_governed_with_properties(
                (vertices.len() + edges.len()) as u64,
                vertices.iter().copied(),
                edges.iter().copied(),
                |vid, predicates| {
                    Ok(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed(
                            [],
                            props.iter().filter_map(|(&(owner, key), value)| {
                                (owner == vid).then_some((key, value))
                            }),
                        )
                    }))
                },
                |vid, key| Ok(props.get(&(vid, key))),
                allowance,
                || Ok(()),
            )
        },
        identity,
        || Ok(()),
    )
}

#[test]
fn native_creation_freezes_case_properties_and_reuses_one_catalog_and_argument_contract() {
    let text = "MATCH (n)-[:R]->(m) WHERE n.p >= $floor \
        CREATE (copy:Copy {q: CASE WHEN n.p IS NOT NULL THEN n.p+$step ELSE 1/0 END,p:n.p}), \
        (n)-[:R {q:$step}]->(copy),(copy)-[:R]->(m)";
    let calls = Cell::new(0);
    let template = PreparedGraphInsertText::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.get(), 4);
    assert_eq!(
        template
            .parameter_schema()
            .iter()
            .find(|spec| spec.name == "step")
            .unwrap()
            .occurrences,
        2
    );
    let args = GqlParameters::new()
        .with_int64("floor", 0)
        .unwrap()
        .with_int64("step", 1)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    assert_eq!(query, template.bind_parameters(&args).unwrap());
    assert_eq!(calls.get(), 4, "binding must not resolve or parse again");
    let edges = [
        (VId(1), R, VId(2)),
        (VId(1), R, VId(2)),
        (VId(2), R, VId(3)),
    ];
    let props = Props::from([
        ((VId(1), P), CanonicalScalar::Int(10)),
        ((VId(2), P), CanonicalScalar::Int(20)),
    ]);
    let result = run(&query, &[VId(1), VId(2), VId(3)], &edges, &props).unwrap();
    assert_eq!(
        (
            result.stats().created_vertices,
            result.stats().created_edges
        ),
        (3, 6)
    );
    let mut expected = Vec::new();
    for (row, (source, _, destination)) in edges.into_iter().enumerate() {
        let vertex = VId(100 + row as u128 * 16);
        let value = if source == VId(1) { 10 } else { 20 };
        expected.push(GraphInsertIntent::Vertex {
            vertex,
            labels: vec![COPY],
            properties: vec![
                (P, CanonicalScalar::Int(value)),
                (Q, CanonicalScalar::Int(value + 1)),
            ],
        });
        expected.push(GraphInsertIntent::Edge {
            edge: EId(1_000 + row as u128 * 16),
            relation: R,
            source,
            destination: vertex,
            properties: vec![(Q, CanonicalScalar::Int(1))],
        });
        expected.push(GraphInsertIntent::Edge {
            edge: EId(1_001 + row as u128 * 16),
            relation: R,
            source: vertex,
            destination,
            properties: vec![],
        });
    }
    assert_eq!(result.intents(), expected);
    assert_eq!(
        query.canonical_bytes(),
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    assert!(matches!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .kind,
        GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)
    ));
}

#[test]
fn constants_preserve_walk_optional_and_edge_only_occurrences() {
    let vertices = [VId(1), VId(2)];
    let edges = [
        (VId(1), R, VId(1)),
        (VId(1), R, VId(1)),
        (VId(1), R, VId(2)),
    ];
    let query = prepare(
        "MATCH WALK (a)-[:R*0..2]->(b) \
        CREATE (x {p:7}),(y {p:NULL}),(x)-[:R]->(y),(y)-[:R]->(x),(x)-[:R]->(x)",
    );
    let result = run(&query, &vertices, &edges, &Props::new()).unwrap();
    assert_eq!(
        (
            result.stats().selection.result_rows,
            result.stats().created_vertices,
            result.stats().created_edges
        ),
        (11, 22, 33)
    );
    for (row, intents) in result.intents().chunks_exact(5).enumerate() {
        let x = VId(100 + row as u128 * 16);
        let y = VId(x.0 + 1);
        assert!(
            matches!(&intents[1], GraphInsertIntent::Vertex { properties, .. }
            if properties == &[(P, CanonicalScalar::Null)])
        );
        for (edge, source, destination) in [(2, x, y), (3, y, x), (4, x, x)] {
            assert!(
                matches!(intents[edge], GraphInsertIntent::Edge { source: s, destination: d, .. } if s == source && d == destination)
            );
        }
    }
    let optional = prepare("MATCH (a) OPTIONAL MATCH (a)-[:R]->(b) CREATE (x {p:COALESCE(b.p,0)})");
    assert_eq!(
        run(&optional, &vertices, &edges, &Props::new())
            .unwrap()
            .stats()
            .created_vertices,
        4
    );
    let edge_only = prepare("MATCH (a)-[:R]->(b) CREATE (b)-[:R {p:1}]->(a)");
    let result = run(&edge_only, &vertices, &edges, &Props::new()).unwrap();
    assert_eq!(
        (
            result.stats().created_vertices,
            result.stats().created_edges
        ),
        (0, 3)
    );
}

#[test]
fn malformed_and_overlarge_statements_refuse_before_any_catalog_observation() {
    for text in [
        "CREATE",
        "MATCH (n) CREATE",
        "MATCH (n) CREATE (n)",
        "MATCH (n) CREATE (x:Copy:Copy)",
        "CREATE (x),(x:Copy)",
        "MATCH (n) CREATE (x {p:1,p:2})",
        "MATCH (n) CREATE (x {p:n.p+})",
        "MATCH (n) CREATE (x {p:TRUE+1})",
        "MATCH (n) CREATE (x {p:CASE WHEN TRUE THEN 1 ELSE missing.p END})",
        "MATCH (n) CREATE (x {p:1}),(y {q:x.p})",
        "CREATE (x {p:x.p})",
        "CREATE (a)-[:R]-(b)",
        "CREATE (a)<-[:R]->(b)",
        "CREATE (a)-[:R*2]->(b)",
        "MATCH (n) CREATE (n:Copy)-[:R]->(x)",
        "MATCH (n) CREATE (n {})-[:R]->(x)",
        "CREATE (a)-[edge:R]->(b)",
        "CREATE (a)-[:R]->(b {q:a.p})",
        "MATCH (n) CREATE (x) RETURN x",
        "MATCH (n) CREATE (x) SET n.p=1",
    ] {
        let calls = Cell::new(0);
        assert!(
            PreparedGraphInsertText::prepare(text, R, |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            })
            .is_err(),
            "{text}"
        );
        assert_eq!(calls.get(), 0, "{text}");
    }
    let declarations = (0..=MAX_GRAPH_INSERT_DECLARATIONS)
        .map(|i| format!("(x{i})"))
        .collect::<Vec<_>>()
        .join(",");
    let calls = Cell::new(0);
    assert!(
        PreparedGraphInsertText::prepare(
            &format!("MATCH (n) CREATE {declarations}"),
            R,
            |kind, name| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            }
        )
        .is_err()
    );
    assert_eq!(calls.get(), 0);
    assert!(PreparedGraphMutationText::prepare("MATCH (n) CREATE (x)", R, symbols).is_err());
}

#[test]
fn creation_relations_are_independent_of_defaults_and_catalog_key_aliases_fail_closed() {
    let query = prepare("MATCH (n) CREATE (n)-[:S]->(n),(n)-[:R]->(n)");
    let batch = run(&query, &[VId(1)], &[], &Props::new()).unwrap();
    assert_eq!(
        batch.intents(),
        &[
            GraphInsertIntent::Edge {
                edge: EId(1_000),
                relation: RelationId(2),
                source: VId(1),
                destination: VId(1),
                properties: vec![],
            },
            GraphInsertIntent::Edge {
                edge: EId(1_001),
                relation: R,
                source: VId(1),
                destination: VId(1),
                properties: vec![],
            },
        ]
    );
    let alias =
        PreparedGraphInsertText::prepare("MATCH (n) CREATE (x {p:1,q:2})", R, |kind, name| {
            if kind == GraphSymbolKind::Property {
                Some(GraphSymbol::Property(P))
            } else {
                symbols(kind, name)
            }
        })
        .unwrap_err();
    assert!(matches!(
        alias.kind,
        GraphInsertTextErrorKind::Build(GraphInsertBuildError::DuplicateProperty { .. })
    ));
    let query = prepare("MATCH (a)-[:S]->(b) CREATE (a)-[:R]->(b)");
    assert_eq!(
        query.relation(),
        R,
        "MATCH relations never choose the write coordinate"
    );
    let batch = run(
        &query,
        &[VId(1), VId(2)],
        &[(VId(1), RelationId(2), VId(2))],
        &Props::new(),
    )
    .unwrap();
    assert_eq!(
        batch.intents(),
        &[GraphInsertIntent::Edge {
            edge: EId(1_000),
            relation: R,
            source: VId(1),
            destination: VId(2),
            properties: vec![],
        }]
    );
}

#[test]
fn scalar_payloads_and_original_utf8_offsets_survive_binding_without_interpolation() {
    let payload = "private '}) CREATE (x) --";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let text = "\u{2003}MATCH (n) CREATE (x:Copy {p:$payload,q:$value})";
    let template = PreparedGraphInsertText::prepare_with_parameter_types(
        text,
        R,
        &[("payload", GqlParameterType::Scalar(kind))],
        symbols,
    )
    .unwrap();
    assert_eq!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .offset,
        text.find('$').unwrap()
    );
    let arguments = GqlParameters::new()
        .with_text("payload", payload)
        .unwrap()
        .with_int64("value", -7)
        .unwrap();
    let plan = template.bind_parameters(&arguments).unwrap();
    let batch = run(&plan, &[VId(1)], &[], &Props::new()).unwrap();
    assert!(
        matches!(&batch.intents()[0], GraphInsertIntent::Vertex { properties, .. }
        if properties == &[(P, CanonicalScalar::ucs_basic_text(payload).unwrap()), (Q, CanonicalScalar::Int(-7))])
    );
    assert!(!format!("{template:?} {plan:?} {batch:?}").contains(payload));
    assert!(matches!(
        template
            .bind_parameters(&arguments.with_int64("extra", 2).unwrap())
            .unwrap_err()
            .kind,
        GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::UnexpectedArguments)
    ));
    for at in (0..text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphInsertText::prepare_with_parameter_types(
            &text[..at],
            R,
            &[("payload", GqlParameterType::Scalar(kind))],
            symbols,
        );
    }
}

#[test]
fn missing_optional_endpoints_are_not_silently_turned_into_partial_creations() {
    let query =
        prepare("MATCH (n) OPTIONAL MATCH (n)-[:R]->(m) CREATE (copy {p:1}),(m)-[:R]->(copy)");
    assert!(matches!(
        run(&query, &[VId(1)], &[], &Props::new()),
        Err(GqlQueryError::Source(GraphInsertError::NullEndpoint { .. }))
    ));
    let query = prepare("MATCH (n) WHERE n.p < 0 CREATE (copy {p:1/0})");
    let result = run(
        &query,
        &[VId(1)],
        &[],
        &Props::from([((VId(1), P), CanonicalScalar::Int(2))]),
    )
    .unwrap();
    assert!(
        result.intents().is_empty(),
        "no matches means no property execution or allocations"
    );
}

#[test]
fn newly_supported_forms_have_exact_creation_effects_not_merely_relaxed_rejections() {
    for (text, explicit, vertices, edges) in [
        ("CREATE (x)", "MATCH (n) CREATE (x)", 1, 0),
        ("CREATE ()", "MATCH (n) CREATE (x)", 1, 0),
        ("MATCH (n) CREATE (x),(x)", "MATCH (n) CREATE (x)", 1, 0),
        (
            "MATCH (n) CREATE (n)-[:R]->(unknown)",
            "MATCH (n) CREATE (unknown),(n)-[:R]->(unknown)",
            1,
            1,
        ),
        (
            "MATCH (n) CREATE (n)-[:R]->(n),(x)",
            "MATCH (n) CREATE (x),(n)-[:R]->(n)",
            1,
            1,
        ),
        (
            "MATCH (n) CREATE (n)-[:R]->(x {p:1})",
            "MATCH (n) CREATE (x {p:1}),(n)-[:R]->(x)",
            1,
            1,
        ),
        (
            "MATCH (n) CREATE (x {p:1})-[:R]->(n)",
            "MATCH (n) CREATE (x {p:1}),(x)-[:R]->(n)",
            1,
            1,
        ),
        (
            "MATCH (n) CREATE (n)<-[:R]-(n)",
            "MATCH (n) CREATE (n)-[:R]->(n)",
            0,
            1,
        ),
    ] {
        let actual = run(&prepare(text), &[VId(1)], &[], &Props::new()).unwrap();
        let expected = run(&prepare(explicit), &[VId(1)], &[], &Props::new()).unwrap();
        assert_eq!(actual.intents(), expected.intents(), "{text}");
        assert_eq!(
            (
                actual.stats().created_vertices,
                actual.stats().created_edges
            ),
            (vertices, edges)
        );
    }
}

#[test]
fn inline_chains_incoming_arrows_cycles_and_anonymous_nodes_bind_exact_endpoints() {
    let query = prepare(
        "CREATE (a:Copy {p:1})-[:R {p:10}]->(b {p:2})<-[:R]-(c {p:3}), \
        (b)-[:R]->(a),(a)-[:R]->(),(:Copy)",
    );
    assert!(query.selection().is_none());
    let result: ResultOf = query.execute_governed(
        policy(),
        |_, _| panic!("no graph source"),
        identity,
        || Ok(()),
    );
    let result = result.unwrap();
    let mut expected = Vec::new();
    for (index, labels, properties) in [
        (0, vec![COPY], vec![(P, CanonicalScalar::Int(1))]),
        (1, vec![], vec![(P, CanonicalScalar::Int(2))]),
        (2, vec![], vec![(P, CanonicalScalar::Int(3))]),
        (3, vec![], vec![]),
        (4, vec![COPY], vec![]),
    ] {
        expected.push(GraphInsertIntent::Vertex {
            vertex: VId(100 + index),
            labels,
            properties,
        });
    }
    for (index, source, destination, properties) in [
        (0, 100, 101, vec![(P, CanonicalScalar::Int(10))]),
        (1, 102, 101, vec![]),
        (2, 101, 100, vec![]),
        (3, 100, 103, vec![]),
    ] {
        expected.push(GraphInsertIntent::Edge {
            edge: EId(1_000 + index),
            relation: R,
            source: VId(source),
            destination: VId(destination),
            properties,
        });
    }
    assert_eq!(result.intents(), expected);
    assert_eq!(
        (
            result.stats().created_vertices,
            result.stats().created_edges
        ),
        (5, 4)
    );
    assert_eq!(
        (
            result.stats().selection.snapshot_records,
            result.stats().selection.result_rows
        ),
        (0, 1)
    );
}

#[test]
fn standalone_parameters_keep_exact_kinds_offsets_occurrences_and_lazy_case() {
    let payload = "secret '}) CREATE (:Copy) --";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let text = "\u{2003}CREATE (a:Copy {p:$payload,q:CASE WHEN $n=0 THEN 0 ELSE 100/$n END})-[:R {q:$n}]->(:Copy)";
    let calls = Cell::new(0);
    let template = PreparedGraphInsertText::prepare_with_parameter_types(
        text,
        R,
        &[("payload", GqlParameterType::Scalar(kind))],
        |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        },
    )
    .unwrap();
    assert_eq!(calls.get(), 4);
    assert_eq!(template.statement(), text);
    assert_eq!(
        template
            .parameter_schema()
            .iter()
            .find(|spec| spec.name == "n")
            .unwrap()
            .occurrences,
        3
    );
    let args = GqlParameters::new()
        .with_text("payload", payload)
        .unwrap()
        .with_int64("n", 0)
        .unwrap();
    let query = template.bind_parameters(&args).unwrap();
    let frozen = query.canonical_bytes();
    let result: ResultOf = query.execute_governed(
        policy(),
        |_, _| panic!("no graph source"),
        identity,
        || Ok(()),
    );
    let batch = result.unwrap();
    assert!(
        matches!(&batch.intents()[0], GraphInsertIntent::Vertex { properties, .. }
        if properties == &[(P, CanonicalScalar::ucs_basic_text(payload).unwrap()), (Q, CanonicalScalar::Int(0))])
    );
    assert_eq!(calls.get(), 4);
    assert_eq!(
        frozen,
        template.bind_parameters(&args).unwrap().canonical_bytes()
    );
    assert_eq!(
        template
            .bind_parameters(&GqlParameters::new())
            .unwrap_err()
            .offset,
        text.find('$').unwrap()
    );
    let wrong = GqlParameters::new()
        .with_int64("payload", 7)
        .unwrap()
        .with_int64("n", 0)
        .unwrap();
    assert!(matches!(
        template.bind_parameters(&wrong).unwrap_err().kind,
        GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::ParameterTypeMismatch { .. })
    ));
    let extra = args.with_int64("unexpected", 1).unwrap();
    assert_eq!(
        template.bind_parameters(&extra).unwrap_err().offset,
        text.len()
    );
    assert!(!format!("{template:?} {query:?} {batch:?}").contains(payload));
    for at in (0..text.len()).filter(|at| text.is_char_boundary(*at)) {
        let _ = PreparedGraphInsertText::prepare_with_parameter_types(
            &text[..at],
            R,
            &[("payload", GqlParameterType::Scalar(kind))],
            symbols,
        );
    }
}

#[test]
fn connected_creation_caps_count_new_declarations_not_node_references() {
    let text = format!(
        "CREATE {}",
        (0..MAX_GRAPH_INSERT_DECLARATIONS)
            .map(|index| format!("(n{index})"))
            .collect::<Vec<_>>()
            .join(",")
    );
    let query = prepare(&format!("{text},(n0)"));
    assert_eq!(query.vertices_per_row(), MAX_GRAPH_INSERT_DECLARATIONS);
    assert_eq!(query.edges_per_row(), 0);
    let mut chain = "CREATE (n0)".to_owned();
    for index in 1..128 {
        chain.push_str(&format!("-[:R]->(n{index})"));
    }
    chain.push_str("-[:R]->(n0)");
    let query = prepare(&chain);
    assert_eq!(
        (query.vertices_per_row(), query.edges_per_row()),
        (128, 128)
    );
    for too_large in [
        format!("{text},()"),
        format!("{chain}-[:R]->()"),
        format!("{chain}-[:R]->(n0)"),
    ] {
        let calls = Cell::new(0);
        let error = PreparedGraphInsertText::prepare(&too_large, R, |kind, name| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap_err();
        assert!(matches!(
            error.kind,
            GraphInsertTextErrorKind::Build(GraphInsertBuildError::TooManyDeclarations { .. })
        ));
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn standalone_bad_properties_fail_even_with_an_empty_database_before_allocation() {
    let query = prepare("CREATE (good {p:7})-[:R]->(bad {p:1/0})");
    let calls = Cell::new(0);
    let result: ResultOf = query.execute_governed(
        policy(),
        |_, _| panic!("no graph source"),
        |request| {
            calls.set(calls.get() + 1);
            identity(request)
        },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphInsertError::Arithmetic {
            row: 0,
            declaration: 1,
            ..
        }))
    ));
    assert_eq!(calls.get(), 0);
    let matched = prepare("MATCH (n) CREATE (good {p:7})-[:R]->(bad {p:1/0})");
    let result = run(&matched, &[], &[], &Props::new()).unwrap();
    assert!(result.intents().is_empty());
    assert_eq!(result.stats().selection.result_rows, 0);
}
