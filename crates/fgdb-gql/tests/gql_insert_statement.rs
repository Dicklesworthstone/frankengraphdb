//! INSERT and CREATE normalize to the same bounded insertion program.
//! These comparisons concern the admitted query surface, not full ISO conformance.

use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId};
use fgdb_gql::insertion::{
    GraphInsertBatch, GraphInsertBuildError, GraphInsertError, GraphInsertIntent,
    GraphInsertPolicy, GraphInsertRequest,
};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphInsertTextError,
    GraphInsertTextErrorKind, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramTemplateError, GraphWriteScriptErrorKind, PreparedGraphInsertText,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind, EId, VId};

const R: RelationId = RelationId(1);
const SPELLINGS: [&str; 6] = ["CREATE", "create", "CrEaTe", "INSERT", "insert", "iNsErT"];

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn equivalent_plans(
    prefix: &str,
    pattern: &str,
    declarations: &[(&str, GqlParameterType)],
    arguments: &GqlParameters,
    vertices: usize,
    edges: usize,
) {
    let create = format!("{prefix}CREATE {pattern}");
    let expected =
        PreparedGraphInsertText::prepare_with_parameter_types(&create, R, declarations, symbols)
            .unwrap()
            .bind_parameters(arguments)
            .unwrap();
    for spelling in SPELLINGS {
        let text = format!("{prefix}{spelling} {pattern}");
        let actual =
            PreparedGraphInsertText::prepare_with_parameter_types(&text, R, declarations, symbols)
                .unwrap_or_else(|error| panic!("{text}: {error}"))
                .bind_parameters(arguments)
                .unwrap_or_else(|error| panic!("{text}: {error}"));
        assert_eq!(
            actual.canonical_bytes(),
            expected.canonical_bytes(),
            "{text}"
        );
        assert_eq!(actual, expected, "{text}");
        assert_eq!(actual.vertices_per_row(), vertices, "{text}");
        assert_eq!(actual.edges_per_row(), edges, "{text}");
        assert_eq!(
            actual.selection().is_some(),
            prefix.trim_start().starts_with("MATCH")
        );
    }
}

fn query_error(error: GraphInsertTextError) -> (usize, GraphPatternTextErrorKind) {
    match error.kind {
        GraphInsertTextErrorKind::Query(kind) => (error.offset, kind),
        other => panic!("expected a typed query refusal, got {other:?}"),
    }
}

#[test]
fn standalone_labels_properties_and_typed_parameters_share_canonical_plans() {
    let payload = "λ; INSERT (not_syntax) ' CREATE";
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let arguments = GqlParameters::new()
        .with_text("payload", payload)
        .unwrap()
        .with_int64("value", -7)
        .unwrap();
    equivalent_plans(
        "\u{2003}",
        "(n:Person:Copy {q:$payload,p:$value})",
        &[
            ("payload", GqlParameterType::Scalar(text_kind)),
            ("value", GqlParameterType::Int64),
        ],
        &arguments,
        1,
        0,
    );
    equivalent_plans(
        "",
        "(:Copy {p:TRUE,q:NULL}),({p:'literal; INSERT CREATE'}),()",
        &[],
        &GqlParameters::new(),
        3,
        0,
    );
}

#[test]
fn comma_patterns_chains_cycles_and_incoming_arrows_share_canonical_plans() {
    equivalent_plans(
        "",
        "(a:Person {p:1})-[:R {q:2}]->(b {p:3})<-[:R]-(c:Copy), \
         (b)-[:R]->(a),(a)-[:R]->(),(c)-[:R]->(c),(a)",
        &[],
        &GqlParameters::new(),
        4,
        5,
    );
}

#[test]
fn matched_endpoints_and_frozen_property_reads_share_canonical_plans() {
    let arguments = GqlParameters::new()
        .with_int64("floor", 0)
        .unwrap()
        .with_int64("step", 2)
        .unwrap();
    equivalent_plans(
        "MATCH (a:Person)-[:R]->(b) WHERE a.p >= $floor ",
        "(a)-[:R {q:$step}]->(copy:Copy {p:a.p,q:CASE WHEN b.p IS NULL THEN 0 ELSE b.p+$step END}), \
         (copy)-[:R]->(b)",
        &[],
        &arguments,
        1,
        2,
    );
    equivalent_plans(
        "MATCH (a)-[:R]->(b) ",
        "(b)-[:R]->(a),(a)-[:R]->(a)",
        &[],
        &GqlParameters::new(),
        0,
        2,
    );
}

#[test]
fn generated_statements_preserve_alias_equivalence_across_three_seeds() {
    for seed in [1_u64, 0x5eed, 0xc0ffee] {
        let mut state = seed;
        for case in 0..12 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            let value = (state >> 32) % 1_000;
            let label = if state & 1 == 0 { "Person" } else { "Copy" };
            let name = format!("n{seed}_{case}");
            let (prefix, pattern, vertices, edges) = match case % 6 {
                0 => ("", format!("({name}:{label} {{p:{value},q:$step}})"), 1, 0),
                1 => (
                    "",
                    format!("({name} {{p:{value}}}),(:{label}),({name})"),
                    2,
                    0,
                ),
                2 => (
                    "",
                    format!("({name}:{label})-[:R {{p:{value}}}]->()<-[:R]-({name})"),
                    2,
                    2,
                ),
                3 => (
                    "",
                    format!("({name} {{p:{value}}})-[:R]->({name}),(:{label})"),
                    2,
                    1,
                ),
                4 => (
                    "MATCH (a:Person) WHERE a.p >= $step ",
                    format!("(a)-[:R]->({name}:{label} {{p:a.p+{value}}})"),
                    1,
                    1,
                ),
                _ => (
                    "MATCH (a)-[:R]->(b) ",
                    format!("(a)<-[:R {{q:{value}}}]-(b),({name}:{label} {{p:$step}})"),
                    1,
                    1,
                ),
            };
            let arguments = if prefix.contains("$step") || pattern.contains("$step") {
                GqlParameters::new()
                    .with_int64("step", (seed % 17) as i64)
                    .unwrap()
            } else {
                GqlParameters::new()
            };
            equivalent_plans(prefix, &pattern, &[], &arguments, vertices, edges);
        }
    }
}

#[test]
fn semicolon_scripts_share_program_bytes_with_mixed_insert_spellings() {
    let arguments = GqlParameters::new().with_int64("value", 7).unwrap();
    let create = "CREATE (a:Person {p:$value,q:'λ; INSERT CREATE'});\n\u{2003}\
        MATCH (a:Person) CREATE (a)-[:R]->(:Copy {p:a.p});\nCREATE ()";
    let expected = PreparedGraphWriteScript::prepare(create, R, symbols)
        .unwrap()
        .bind_parameters(&arguments)
        .unwrap();
    for trailing in ["", ";"] {
        let text = format!(
            "insert (a:Person {{p:$value,q:'λ; INSERT CREATE'}});\n\u{2003}\
             MATCH (a:Person) iNsErT (a)-[:R]->(:Copy {{p:a.p}});\nINSERT (){trailing}"
        );
        let actual = PreparedGraphWriteScript::prepare(&text, R, symbols)
            .unwrap()
            .bind_parameters(&arguments)
            .unwrap();
        assert_eq!(actual.canonical_bytes(), expected.canonical_bytes());
        assert_eq!(actual, expected);
    }
}

#[test]
fn bound_node_redeclarations_and_malformed_syntax_have_identical_typed_refusals() {
    for (prefix, pattern) in [
        ("MATCH (n) ", "(n:Copy)-[:R]->(x)"),
        ("MATCH (n) ", "(n {})-[:R]->(x)"),
        ("", "(x),(x:Copy)"),
        ("", "(x:Copy:Copy)"),
        ("", "(x {p:1,p:2})"),
        ("", "(a)-[:R]-(b)"),
        ("", "(a)<-[:R]->(b)"),
        ("", "(a)-[:R*2]->(b)"),
        ("", "(a)-[edge:R]->(b)"),
        ("", "(x"),
        ("", ""),
        ("", "(x) RETURN x"),
    ] {
        let create = format!("{prefix}CREATE {pattern}");
        let expected =
            query_error(PreparedGraphInsertText::prepare(&create, R, symbols).unwrap_err());
        assert!(
            matches!(expected.1, GraphPatternTextErrorKind::Expected(_)),
            "{create}"
        );
        for spelling in SPELLINGS {
            let text = format!("{prefix}{spelling} {pattern}");
            let actual =
                query_error(PreparedGraphInsertText::prepare(&text, R, symbols).unwrap_err());
            assert_eq!(actual, expected, "{text}");
        }
    }
    for spelling in SPELLINGS {
        let text = format!("MATCH (n) {spelling} (n)");
        let error = PreparedGraphInsertText::prepare(&text, R, symbols).unwrap_err();
        assert_eq!(error.offset, text.find(spelling).unwrap());
        assert!(matches!(
            error.kind,
            GraphInsertTextErrorKind::Build(GraphInsertBuildError::Empty)
        ));

        let text = format!("{spelling} (a)-[:S]->(b)");
        let prepared = PreparedGraphInsertText::prepare(&text, R, symbols).unwrap();
        let bound = prepared.bind_parameters(&GqlParameters::new()).unwrap();
        let result: Result<GraphInsertBatch, GqlQueryError<GraphInsertError<(), ()>, ()>> = bound
            .execute_governed(
                GraphInsertPolicy::new(GqlQueryPolicy::new(0, 1, 100_000, 100_000), 2, 1),
                |_, _| panic!("standalone insertion must not scan a graph source"),
                |request| {
                    Ok(match request {
                        GraphInsertRequest::Vertex { row, vertex } => {
                            ElementId::Vertex(VId(100 + row as u128 * 16 + vertex as u128))
                        }
                        GraphInsertRequest::Edge { row, edge } => {
                            ElementId::Edge(EId(200 + row as u128 * 16 + edge as u128))
                        }
                    })
                },
                || Ok(()),
            );
        assert_eq!(
            result.unwrap().intents(),
            &[
                GraphInsertIntent::Vertex {
                    vertex: VId(100),
                    labels: vec![],
                    properties: vec![],
                },
                GraphInsertIntent::Vertex {
                    vertex: VId(101),
                    labels: vec![],
                    properties: vec![],
                },
                GraphInsertIntent::Edge {
                    edge: EId(200),
                    relation: RelationId(2),
                    source: VId(100),
                    destination: VId(101),
                    properties: vec![],
                },
            ],
            "{text}"
        );
    }
}

#[test]
fn typed_parameter_refusals_preserve_kinds_and_original_offsets() {
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("value").unwrap());
    let wrong = GqlParameters::new().with_int64("payload", 7).unwrap();
    let good = GqlParameters::new().with_text("payload", "value").unwrap();
    let extra = good.with_int64("extra", 1).unwrap();
    for spelling in SPELLINGS {
        let text = format!("\u{2003}{spelling} (n {{p:$payload}})");
        let template = PreparedGraphInsertText::prepare_with_parameter_types(
            &text,
            R,
            &[("payload", GqlParameterType::Scalar(kind))],
            symbols,
        )
        .unwrap();
        for (arguments, expected, offset) in [
            (
                GqlParameters::new(),
                GraphPatternTextErrorKind::MissingParameter,
                text.find('$').unwrap(),
            ),
            (
                wrong.clone(),
                GraphPatternTextErrorKind::ParameterTypeMismatch {
                    expected: GqlParameterType::Scalar(kind),
                    found: GqlParameterType::Int64,
                },
                text.find('$').unwrap(),
            ),
            (
                extra.clone(),
                GraphPatternTextErrorKind::UnexpectedArguments,
                text.len(),
            ),
        ] {
            assert_eq!(
                query_error(template.bind_parameters(&arguments).unwrap_err()),
                (offset, expected)
            );
        }
    }
}

#[test]
fn script_refusals_preserve_statement_index_and_utf8_offsets() {
    let mut expected = None;
    for spelling in SPELLINGS {
        let text =
            format!("{spelling} (a {{p:'λ'}});\n\u{2003}MATCH (n) {spelling} (n:Copy)-[:R]->(x);");
        let error = PreparedGraphWriteScript::prepare(&text, R, symbols).unwrap_err();
        assert_eq!(error.statement, Some(1));
        assert_eq!(error.offset, text.find("n:Copy").unwrap());
        let GraphWriteScriptErrorKind::Program(GraphWriteProgramTemplateError::InsertBind {
            statement,
            source,
        }) = error.kind
        else {
            panic!("expected insertion refusal in the second script statement");
        };
        assert_eq!(statement, 1);
        let actual = query_error(source);
        assert!(matches!(actual.1, GraphPatternTextErrorKind::Expected(_)));
        if let Some(expected) = &expected {
            assert_eq!(&actual, expected);
        } else {
            expected = Some(actual);
        }
    }
}
