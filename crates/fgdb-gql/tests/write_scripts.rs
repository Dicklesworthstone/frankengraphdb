//! Native scripts compile to the same typed programs as explicit composition.

use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphPatternTextErrorKind, GraphSymbol, GraphSymbolKind,
    GraphWriteProgramTemplateError, GraphWriteScriptErrorKind, GraphWriteStatement,
    MAX_GRAPH_MUTATION_STATEMENTS, MAX_GRAPH_TEXT_BYTES, MAX_GRAPH_WRITE_SCRIPT_BYTES,
    PreparedGraphInsertText, PreparedGraphMutationText, PreparedGraphWriteProgram,
    PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p" | "ON") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

#[test]
fn one_native_script_dispatches_every_supported_write_kind() {
    let text = "CREATE (n:Person {p:1});\n\
        MATCH (n:Person) SET n.q=2;\n\
        MERGE (n:Person {p:3});\n\
        MERGE (n:Person {p:4}) ON CREATE SET n.q=5;\n\
        MATCH (a:Person),(b:Person) WHERE a.p=1 AND b.p=3 MERGE (a)-[:R]->(b);\n\
        MATCH (a:Person),(b:Person) WHERE a.p=3 AND b.p=4 MERGE (a)-[e:R]->(b) ON MATCH SET e.q=6 ON CREATE SET e.q=7;\n";
    let script = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
    let program = script.bind_parameters(&GqlParameters::new()).unwrap();
    assert!(matches!(program.statements(), [GraphWriteStatement::Insert(_),
        GraphWriteStatement::Mutation(_), GraphWriteStatement::VertexMerge(_),
        GraphWriteStatement::VertexUpsert(_), GraphWriteStatement::EdgeMerge(_),
        GraphWriteStatement::EdgeUpsert(_)]));
    assert_eq!(script.script(), text);
    for (index, template) in script.statements().iter().enumerate() {
        assert_eq!(&text[script.statement_span(index).unwrap()], template.statement());
    }
    assert_eq!(script.statement_span(6), None);
    assert!(!format!("{script:?} {program:?}").contains("Person"));
}

#[test]
fn shared_catalog_and_arguments_match_explicit_program_composition() {
    let calls = RefCell::new(BTreeSet::new());
    let text = "CREATE (n:Person {p:$value});MATCH (n:Person) SET n.q=$value";
    let script = PreparedGraphWriteScript::prepare(text, R, |kind, name| {
        assert!(calls.borrow_mut().insert((kind, name.to_owned())), "catalog resolved twice");
        symbols(kind, name)
    }).unwrap();
    assert_eq!(calls.borrow().len(), 3);
    let args = GqlParameters::new().with_int64("value", 7).unwrap();
    let expected = PreparedGraphWriteProgram::prepare(vec![
        PreparedGraphInsertText::prepare("CREATE (n:Person {p:$value})", R, symbols).unwrap()
            .bind_parameters(&args).unwrap().into(),
        PreparedGraphMutationText::prepare("MATCH (n:Person) SET n.q=$value", R, symbols).unwrap()
            .bind_parameters(&args).unwrap().into(),
    ]).unwrap();
    assert_eq!(script.bind_parameters(&args).unwrap(), expected);
    assert_eq!(script.bind_parameters(&args).unwrap().canonical_bytes(), expected.canonical_bytes());
    assert_eq!(script.parameter_schema()[0].occurrences, 2);
    assert_eq!(calls.borrow().len(), 3, "rebinding must not call the catalog");
}

#[test]
fn native_quoted_tokens_keep_separators_and_keywords_inside_values() {
    let text = "CREATE (n {p:'one; ''ON CREATE''; MERGE'});MATCH (n) SET n.q=';';";
    let script = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
    assert_eq!(script.statements().len(), 2);
    let expected = PreparedGraphWriteProgram::prepare(vec![
        PreparedGraphInsertText::prepare("CREATE (n {p:'one; ''ON CREATE''; MERGE'})", R, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap().into(),
        PreparedGraphMutationText::prepare("MATCH (n) SET n.q=';'", R, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap().into(),
    ]).unwrap();
    assert_eq!(script.bind_parameters(&GqlParameters::new()).unwrap(), expected);
}

#[test]
fn declarations_are_global_but_only_local_subsets_reach_statement_compilers() {
    let payload = "'); MERGE (escape); --";
    let text_kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
    let text = "CREATE (n {p:$name});MATCH (n) SET n.q=$count";
    let script = PreparedGraphWriteScript::prepare_with_parameter_types(text, R,
        &[("name", GqlParameterType::Scalar(text_kind)), ("count", GqlParameterType::Int64)], symbols).unwrap();
    let args = GqlParameters::new().with_text("name", payload).unwrap().with_int64("count", 8).unwrap();
    let bound = script.bind_parameters(&args).unwrap();
    assert_eq!(bound.statements().len(), 2);
    assert!(!format!("{script:?} {bound:?}").contains(payload));
    assert!(script.bind_parameters(&GqlParameters::new().with_int64("name", 1).unwrap()
        .with_int64("count", 8).unwrap()).is_err());
    for declarations in [vec![("missing", GqlParameterType::Int64)],
        vec![("name", GqlParameterType::Int64), ("name", GqlParameterType::Int64)]] {
        let calls = Cell::new(0);
        assert!(PreparedGraphWriteScript::prepare_with_parameter_types(text, R, &declarations, |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).is_err());
        assert_eq!(calls.get(), 0);
    }
}

#[test]
fn preparation_and_binding_errors_report_original_utf8_script_offsets() {
    let text = "CREATE (n {p:'λ'});\n\u{2003}MATCH (n) SET n.q=$missing";
    let script = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
    let error = script.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(error.statement, Some(1));
    assert_eq!(error.offset, text.find('$').unwrap());
    let bad = "CREATE (n {p:'λ'});\n\u{2003}MATCH (n) SET n.unknown=1";
    let error = PreparedGraphWriteScript::prepare(bad, R, symbols).unwrap_err();
    assert_eq!(error.statement, Some(1));
    assert_eq!(error.offset, bad.find("unknown").unwrap());
    let extra = GqlParameters::new().with_int64("extra", 3).unwrap();
    assert!(matches!(script.bind_parameters(&extra).unwrap_err().kind,
        GraphWriteScriptErrorKind::Program(GraphWriteProgramTemplateError::Program(
            fgdb_gql::GraphMutationProgramTemplateError::UnexpectedArguments))));
}

#[test]
fn malformed_framing_and_nonwrite_commands_never_reach_the_catalog() {
    for text in ["", "  ", ";", "CREATE (n);;", "CREATE (n); ;CREATE (m)",
        "CREATE (n {p:'unfinished});CREATE (m)", "CREATE (n;CREATE (m)",
        "CREATE (n];CREATE (m)", "CREATE (n);BEGIN", "CREATE (n);COMMIT",
        "CREATE (n);MATCH (n) RETURN n", "CREATE (n);MATCH (n) DELETE n"] {
        let calls = Cell::new(0);
        assert!(PreparedGraphWriteScript::prepare(text, R, |kind, name| {
            calls.set(calls.get() + 1); symbols(kind, name)
        }).is_err(), "{text}");
        assert_eq!(calls.get(), 0, "{text}");
    }
    assert!(PreparedGraphWriteScript::prepare("CREATE (n);\n\t", R, symbols).is_ok());
}

#[test]
fn native_prefix_parsing_does_not_confuse_property_names_with_write_clauses() {
    let text = "MATCH (a) WHERE a.ON=1 MERGE (a)-[:R]->(a);\n\
        MATCH (a) WHERE a.ON=1 MERGE (a)-[e:R]->(a) ON CREATE SET e.q=2";
    let program = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap();
    assert!(matches!(program.statements(), [GraphWriteStatement::EdgeMerge(_), GraphWriteStatement::EdgeUpsert(_)]));
}

#[test]
fn whole_script_and_per_statement_limits_are_checked_before_catalog_access() {
    let exact = vec!["CREATE (n)"; MAX_GRAPH_MUTATION_STATEMENTS].join(";");
    assert_eq!(PreparedGraphWriteScript::prepare(&exact, R, symbols).unwrap().statements().len(), 64);
    let calls = Cell::new(0);
    let mut resolve = |kind, name: &str| { calls.set(calls.get() + 1); symbols(kind, name) };
    let too_many = format!("{exact};CREATE (n)");
    assert!(matches!(PreparedGraphWriteScript::prepare(&too_many, R, &mut resolve).unwrap_err().kind,
        GraphWriteScriptErrorKind::TooManyStatements { limit: 64, observed: 65 }));
    let oversized = format!("{}CREATE (n)", " ".repeat(MAX_GRAPH_TEXT_BYTES));
    assert!(matches!(PreparedGraphWriteScript::prepare(&oversized, R, &mut resolve).unwrap_err().kind,
        GraphWriteScriptErrorKind::DefinitionTooLarge { limit: MAX_GRAPH_TEXT_BYTES, .. }));
    assert!(matches!(PreparedGraphWriteScript::prepare(&" ".repeat(MAX_GRAPH_WRITE_SCRIPT_BYTES + 1), R, &mut resolve).unwrap_err().kind,
        GraphWriteScriptErrorKind::DefinitionTooLarge { limit: MAX_GRAPH_WRITE_SCRIPT_BYTES, .. }));
    let tokens = format!("CREATE (n {{p:{}}})", vec!["1"; fgdb_gql::MAX_GRAPH_TEXT_TOKENS].join("+"));
    assert!(matches!(PreparedGraphWriteScript::prepare(&tokens, R, &mut resolve).unwrap_err().kind,
        GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::TooManyTokens)));
    assert_eq!(calls.get(), 0);
}
