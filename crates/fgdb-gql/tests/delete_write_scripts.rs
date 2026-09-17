//! DELETE remains a distinct native statement through script and batch binding.
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteProgramTemplateError,
    GraphWriteScriptBatchError, GraphWriteScriptErrorKind, GraphWriteStatement,
    GraphWriteTemplateStatement, PreparedGraphDeleteText, PreparedGraphInsertText,
    PreparedGraphWriteProgram, PreparedGraphWriteProgramTemplate, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
use std::cell::Cell;

const R: RelationId = RelationId(1);
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}

#[test]
fn native_delete_shares_catalog_and_parameters_and_matches_explicit_composition() {
    let calls = Cell::new(0);
    let text = "CREATE (n {p:$key}); MATCH (n) WHERE n.p=$key DELETE n";
    let script = PreparedGraphWriteScript::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        &script.statements()[1],
        GraphWriteTemplateStatement::Delete(_)
    ));
    assert_eq!(script.parameter_schema()[0].occurrences, 2);
    let args = GqlParameters::new().with_int64("key", 7).unwrap();
    let bound = script.bind_parameters(&args).unwrap();
    let creation = PreparedGraphInsertText::prepare("CREATE (n {p:$key})", R, symbols).unwrap();
    let deletion =
        PreparedGraphDeleteText::prepare("MATCH (n) WHERE n.p=$key DELETE n", R, symbols).unwrap();
    let explicit =
        PreparedGraphWriteProgramTemplate::prepare(vec![creation.into(), deletion.into()]).unwrap();
    assert_eq!(bound, explicit.bind_parameters(&args).unwrap());
    assert!(matches!(
        &bound.statements()[1],
        GraphWriteStatement::Delete(_)
    ));
    assert_eq!(calls.get(), 1, "binding cannot re-enter the catalog");
    let detach =
        PreparedGraphWriteScript::prepare("MATCH (n) WHERE n.p=$key DETACH DELETE n", R, symbols)
            .unwrap()
            .bind_parameters(&args)
            .unwrap();
    let plain = PreparedGraphWriteProgram::prepare(vec![bound.statements()[1].clone()]).unwrap();
    assert_ne!(plain.canonical_bytes(), detach.canonical_bytes());
}

#[test]
fn delete_binding_errors_retain_original_utf8_offset_and_statement_index() {
    let text = "CREATE (n {p:'λ'});\n MATCH (n) WHERE n.p=$value DELETE n";
    let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text("value").unwrap());
    let script = PreparedGraphWriteScript::prepare_with_parameter_types(
        text,
        R,
        &[("value", GqlParameterType::Scalar(kind))],
        symbols,
    )
    .unwrap();
    for args in [
        GqlParameters::new(),
        GqlParameters::new().with_int64("value", 9).unwrap(),
    ] {
        let error = script.bind_parameters(&args).unwrap_err();
        assert_eq!(error.statement, Some(1));
        assert_eq!(error.offset, text.find('$').unwrap());
        assert!(matches!(
            error.kind,
            GraphWriteScriptErrorKind::Program(GraphWriteProgramTemplateError::DeleteBind {
                statement: 1,
                ..
            })
        ));
    }
    assert!(
        script
            .bind_parameters(&GqlParameters::new().with_text("value", "λ").unwrap())
            .is_ok()
    );
}

#[test]
fn malformed_delete_is_not_reinterpreted_as_detach_or_a_read() {
    for text in [
        "CREATE (n);MATCH (n) DELETE missing",
        "MATCH (n) DELETE n,n",
        "MATCH (n) DELETE n.p",
        "MATCH (n) DELETE n RETURN n",
    ] {
        assert!(
            PreparedGraphWriteScript::prepare(text, R, symbols).is_err(),
            "{text}"
        );
    }
    let text = "CREATE (n);\nMATCH (n) DELETE missing";
    let error = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap_err();
    assert_eq!(error.statement, Some(1));
    assert_eq!(error.offset, text.find("missing").unwrap());
}

#[test]
fn delete_parameter_batches_keep_record_mapping_and_reject_a_late_bad_record() {
    let text = "MATCH (n) WHERE n.p=$key DELETE n; CREATE (n {p:$key})";
    let script = PreparedGraphWriteScript::prepare(text, R, symbols).unwrap();
    let values = GqlParameters::new().with_int64("key", 1).unwrap();
    let batch = script
        .bind_parameter_sets(&[values.clone(), values.clone()])
        .unwrap();
    assert!(matches!(
        &batch.program().statements()[2],
        GraphWriteStatement::Delete(_)
    ));
    let location = batch.location(2).unwrap();
    assert_eq!((location.argument_set, location.statement), (1, 0));
    assert_eq!(location.span, script.statement_span(0).unwrap());
    let error = script
        .bind_parameter_sets(&[values, GqlParameters::new()])
        .unwrap_err();
    assert!(
        matches!(error, GraphWriteScriptBatchError::Arguments { argument_set: 1, source }
        if source.statement == Some(0) && source.offset == text.find('$').unwrap())
    );
}
