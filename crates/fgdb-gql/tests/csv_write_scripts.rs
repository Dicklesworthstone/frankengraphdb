//! CSV preparation is equivalent to explicit native batch preparation.
//! These tests do not claim that binding alone executes or commits a program.

use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::csv_parameters::{
    CsvParameterErrorKind, CsvParameterLimit, CsvParameterLimits, decode_csv_parameters,
};
use fgdb_gql::csv_write_script::GraphWriteScriptCsvError;
use fgdb_gql::{
    GqlParameterType, GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteScriptBatchError,
    GraphWriteStatement, PreparedGraphWriteScript,
};
use fgdb_types::CanonicalScalarKind;
use std::cell::Cell;
use std::error::Error;

const R: RelationId = RelationId(1);

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn insertion() -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare("CREATE (n {p:$key,q:$value})", R, symbols).unwrap()
}

fn record(key: i64, value: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("key", key)
        .unwrap()
        .with_int64("value", value)
        .unwrap()
}

fn csv(records: usize) -> String {
    let mut input = String::from("key,value\n");
    for key in 0..records {
        input.push_str(&format!("{key},{}\n", key + 10));
    }
    input
}

fn csv_error(error: GraphWriteScriptCsvError) -> fgdb_gql::csv_parameters::CsvParameterError {
    match error {
        GraphWriteScriptCsvError::Csv(source) => source,
        GraphWriteScriptCsvError::Binding(source) => panic!("expected CSV error: {source}"),
    }
}

#[test]
fn permuted_headers_preserve_complete_native_program_bytes() {
    let script = insertion();
    let actual = script
        .bind_csv("value,key\r\n10,1\r\n20,2\r\n", CsvParameterLimits::default())
        .unwrap();
    let expected = script
        .bind_parameter_sets(&[record(1, 10), record(2, 20)])
        .unwrap();
    assert_eq!(actual.program(), expected.program());
    assert_eq!(actual.program().canonical_bytes(), expected.program().canonical_bytes());
    assert_eq!(actual.argument_sets(), 2);
    assert_eq!(actual.statement_range(0), Some(0..1));
    assert_eq!(actual.statement_range(1), Some(1..2));
    assert_eq!(actual.statement_range(2), None);
    assert_eq!(actual.statement_range(usize::MAX), None);
    assert_eq!(actual.location(2), None);
    assert_eq!(actual.location(usize::MAX), None);
}

#[test]
fn multiple_statements_are_record_major_and_keep_original_script_coordinates() {
    let text = "CREATE (n {p:$key});\n\u{2003}MATCH (n) WHERE n.p=$key SET n.q=$value";
    let calls = Cell::new(0);
    let script = PreparedGraphWriteScript::prepare(text, R, |kind, name| {
        calls.set(calls.get() + 1);
        symbols(kind, name)
    })
    .unwrap();
    let resolved = calls.get();
    let arguments = [record(1, 11), record(2, 12), record(3, 13)];
    let batch = script
        .bind_csv("key,value\n1,11\n2,12\n3,13", CsvParameterLimits::default())
        .unwrap();
    assert_eq!(batch.argument_sets(), 3);
    assert_eq!(batch.program().statements().len(), 6);
    for (index, args) in arguments.iter().enumerate() {
        let range = batch.statement_range(index).unwrap();
        let expected = script.bind_parameters(args).unwrap();
        assert_eq!(&batch.program().statements()[range.clone()], expected.statements());
        for flat in range {
            let location = batch.location(flat).unwrap();
            assert_eq!((location.argument_set, location.statement), (index, flat % 2));
            assert_eq!(location.span, script.statement_span(flat % 2).unwrap());
        }
    }
    assert_eq!(calls.get(), resolved, "CSV rebinding must not resolve symbols");
    assert_eq!(script.script(), text);
}

#[test]
fn default_sixty_four_expanded_statements_and_explicit_larger_admission() {
    let script = insertion();
    assert_eq!(
        script.bind_csv(&csv(64), CsvParameterLimits::default()).unwrap()
            .program().statements().len(),
        64
    );
    let error = csv_error(script.bind_csv(&csv(65), CsvParameterLimits::default()).unwrap_err());
    assert_eq!(error.record, 65);
    assert_eq!(error.kind, CsvParameterErrorKind::Limit {
        dimension: CsvParameterLimit::Records,
        limit: 64,
        observed: 65,
    });
    let larger = script
        .bind_csv_with_statement_limit(&csv(65), CsvParameterLimits::default(), 65)
        .unwrap();
    let expected_arguments = (0..65).map(|key| record(key, key + 10)).collect::<Vec<_>>();
    let expected = script.bind_parameter_sets_with_limit(&expected_arguments, 65).unwrap();
    assert_eq!(larger.program(), expected.program());
    assert_eq!(larger.argument_sets(), 65);
    assert_eq!(larger.program().statements().len(), 65);
    assert!(matches!(
        script.bind_parameter_sets(&expected_arguments),
        Err(GraphWriteScriptBatchError::TooManyStatements { limit: 64, observed: 65 })
    ));
}

#[test]
fn default_cap_counts_the_product_not_records_alone() {
    let script = PreparedGraphWriteScript::prepare(
        "CREATE (n {p:$key}); MATCH (n) WHERE n.p=$key SET n.q=$value",
        R,
        symbols,
    )
    .unwrap();
    assert_eq!(script.bind_csv(&csv(32), CsvParameterLimits::default()).unwrap()
        .program().statements().len(), 64);
    let error = csv_error(script.bind_csv(&csv(33), CsvParameterLimits::default()).unwrap_err());
    assert_eq!(error.kind, CsvParameterErrorKind::Limit {
        dimension: CsvParameterLimit::Records,
        limit: 32,
        observed: 33,
    });
    assert!(script.bind_csv_with_statement_limit(&csv(33), CsvParameterLimits::default(), 65).is_err());
    assert_eq!(script.bind_csv_with_statement_limit(&csv(33), CsvParameterLimits::default(), 66)
        .unwrap().program().statements().len(), 66);
}

#[test]
fn zero_or_insufficient_statement_allowance_refuses_before_decoding_values() {
    let script = insertion();
    let source = "key,value\n\"unterminated";
    let error = csv_error(script
        .bind_csv_with_statement_limit(source, CsvParameterLimits::default(), 0)
        .unwrap_err());
    assert_eq!(error.record, 1);
    assert_eq!(error.kind, CsvParameterErrorKind::Limit {
        dimension: CsvParameterLimit::Records,
        limit: 0,
        observed: 1,
    });
    let two = PreparedGraphWriteScript::prepare(
        "CREATE (n {p:$key});MATCH (n) SET n.q=$value", R, symbols,
    ).unwrap();
    assert_eq!(csv_error(two.bind_csv_with_statement_limit(source, CsvParameterLimits::default(), 1)
        .unwrap_err()).kind, error.kind);
}

#[test]
fn oversized_final_record_is_refused_before_parsing_or_type_conversion() {
    let script = insertion();
    let mut input = csv(64);
    input.push_str("\"unterminated sensitive data");
    let error = csv_error(script.bind_csv(&input, CsvParameterLimits::default()).unwrap_err());
    assert_eq!(error.record, 65);
    assert!(matches!(error.kind, CsvParameterErrorKind::Limit {
        dimension: CsvParameterLimit::Records, ..
    }));
    assert!(!format!("{error:?} {error}").contains("sensitive"));
}

#[test]
fn caller_record_and_transcript_bounds_remain_stricter_than_program_admission() {
    let script = insertion();
    let defaults = CsvParameterLimits::default();
    let input = csv(2);
    let records = decode_csv_parameters(&input, script.parameter_schema(), defaults).unwrap();
    let transcript_bytes: usize = records.iter().map(GqlParameters::canonical_byte_len).sum();
    let error = csv_error(script.bind_csv_with_statement_limit(&input, CsvParameterLimits {
        max_records: 1, ..defaults
    }, usize::MAX).unwrap_err());
    assert!(matches!(error.kind, CsvParameterErrorKind::Limit {
        dimension: CsvParameterLimit::Records, limit: 1, ..
    }));
    let error = csv_error(script.bind_csv(&input, CsvParameterLimits {
        max_parameter_bytes: transcript_bytes - 1, ..defaults
    }).unwrap_err());
    assert_eq!(error.record, 2);
    assert!(matches!(error.kind, CsvParameterErrorKind::Limit {
        dimension: CsvParameterLimit::ParameterBytes, ..
    }));
    assert!(script.bind_csv(&input, CsvParameterLimits {
        max_parameter_bytes: transcript_bytes, ..defaults
    }).is_ok());
}

#[test]
fn malformed_final_record_drops_prefix_and_does_not_change_reusable_script() {
    let script = insertion();
    let prior = script.bind_csv(&csv(2), CsvParameterLimits::default()).unwrap();
    let frozen = prior.program().canonical_bytes();
    for suffix in ["not-an-integer,3", "3", "3,4,5", "\"unfinished"] {
        let mut input = csv(2);
        input.push_str(suffix);
        let error = csv_error(script.bind_csv(&input, CsvParameterLimits::default()).unwrap_err());
        assert_eq!(error.record, 3);
        assert_eq!(prior.program().canonical_bytes(), frozen);
        assert_eq!(script.bind_csv(&csv(2), CsvParameterLimits::default()).unwrap()
            .program().canonical_bytes(), frozen);
    }
}

#[test]
fn text_payloads_nulls_empty_strings_and_multiline_fields_bind_as_values() {
    let text = "CREATE (n {p:$key,q:$text})";
    let script = PreparedGraphWriteScript::prepare_with_parameter_types(
        text,
        R,
        &[("text", GqlParameterType::Scalar(CanonicalScalarKind::Text))],
        symbols,
    )
    .unwrap();
    let input = "text,key\n\"'; MATCH (n) DETACH DELETE n; --\",1\n\"multi\n雪\",2\n\\N,3\n\"\\N\",4\n\"\",5";
    let actual = script.bind_csv(input, CsvParameterLimits::default()).unwrap();
    let expected = script.bind_parameter_sets(&[
        GqlParameters::new().with_int64("key", 1).unwrap()
            .with_text("text", "'; MATCH (n) DETACH DELETE n; --").unwrap(),
        GqlParameters::new().with_int64("key", 2).unwrap()
            .with_text("text", "multi\n雪").unwrap(),
        GqlParameters::new().with_int64("key", 3).unwrap().with_null("text").unwrap(),
        GqlParameters::new().with_int64("key", 4).unwrap().with_text("text", "\\N").unwrap(),
        GqlParameters::new().with_int64("key", 5).unwrap().with_text("text", "").unwrap(),
    ]).unwrap();
    assert_eq!(actual.program(), expected.program());
    assert_eq!(actual.program().canonical_bytes(), expected.program().canonical_bytes());
    assert_eq!(actual.program().statements().len(), 5);
    assert!(actual.program().statements().iter().all(|statement| {
        matches!(statement, GraphWriteStatement::Insert(_))
    }));
    assert_eq!(script.script(), text);
}

#[test]
fn empty_header_only_parameterless_and_invalid_header_inputs_are_not_noops() {
    let script = insertion();
    for input in ["", "key,value\n", "key,key\n1,2", "key,unknown\n1,2", "key\n1"] {
        assert!(script.bind_csv(input, CsvParameterLimits::default()).is_err());
    }
    let parameterless = PreparedGraphWriteScript::prepare("CREATE (n)", R, symbols).unwrap();
    let error = csv_error(parameterless.bind_csv("key\n1", CsvParameterLimits::default()).unwrap_err());
    assert_eq!(error.kind, CsvParameterErrorKind::EmptySchema);
}

#[test]
fn csv_error_source_retains_original_input_coordinates_without_values() {
    let script = insertion();
    let input = "key,value\n1,2\n3,secret";
    let error = script.bind_csv(input, CsvParameterLimits::default()).unwrap_err();
    let cause = error.source().unwrap();
    assert!(!format!("{error:?} {error} {cause}").contains("secret"));
    let cause = cause.downcast_ref::<fgdb_gql::csv_parameters::CsvParameterError>().unwrap();
    assert_eq!((cause.record, cause.column), (2, Some(1)));
    assert_eq!(cause.offset, input.find("secret").unwrap());
}

#[test]
fn utf8_bom_and_record_endings_do_not_change_program_identity() {
    let script = insertion();
    let expected = script.bind_csv("key,value\n1,2\n", CsvParameterLimits::default()).unwrap();
    for input in ["\u{feff}key,value\r\n1,2\r\n", "key,value\r\n1,2", "\"key\",\"value\"\n\"1\",\"2\""] {
        let actual = script.bind_csv(input, CsvParameterLimits::default()).unwrap();
        assert_eq!(actual.program().canonical_bytes(), expected.program().canonical_bytes());
    }
}
