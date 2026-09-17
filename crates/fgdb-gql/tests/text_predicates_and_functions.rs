//! Text expressions use the shared typed, governed relational evaluator.
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalScalar, VId};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    (kind == GraphSymbolKind::Property && name == "text")
        .then_some(GraphSymbol::Property(PropertyKeyId(1)))
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}
fn prepare(statement: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(statement, symbols)
        .unwrap_or_else(|e| panic!("{statement}: {e:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
type QueryResult =
    Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, ()>>;
fn execute(
    query: &PreparedGraphSet,
    input: &CanonicalScalar,
    budget: GqlQueryPolicy,
) -> QueryResult {
    query.execute_governed(
        budget,
        |pattern, remaining| {
            pattern.plan().execute_governed_with_properties(
                1,
                [VId(1)],
                [],
                |_, predicates| {
                    Ok::<_, ()>(predicates.iter().all(|predicate| {
                        predicate.matches_borrowed([], [(PropertyKeyId(1), input)])
                    }))
                },
                |_, _| Ok(Some(input)),
                remaining,
                || Ok::<_, ()>(()),
            )
        },
        || Ok::<_, ()>(()),
    )
}
fn scalar(expression: &str, input: &CanonicalScalar) -> CanonicalScalar {
    let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
    let result = execute(&query, input, policy()).unwrap();
    assert_eq!(result.value.len(), 1);
    result.value[0].values()[0].as_scalar().unwrap().clone()
}

#[test]
fn char_length_and_substring_use_unicode_scalars_not_bytes_or_graphemes() {
    let input = text("é🦀a\u{301}");
    assert_eq!(
        scalar("CHAR_LENGTH(n.text)", &input),
        CanonicalScalar::Int(4)
    );
    assert_eq!(scalar("SUBSTRING(n.text,2,2)", &input), text("🦀a"));
    assert_eq!(scalar("SUBSTRING(n.text,4,1)", &input), text("\u{301}"));
    assert_eq!(scalar("SUBSTRING(n.text,9,2)", &input), text(""));
    assert_eq!(scalar("SUBSTRING(n.text,2,0)", &input), text(""));
    assert_eq!(
        scalar("SUBSTRING(n.text FROM 2 FOR 2)", &input),
        text("🦀a")
    );
}

#[test]
fn unicode_case_mapping_trim_and_nested_concat_preserve_text_values() {
    // Locale-independent Unicode case mapping, not ASCII-only or case folding.
    // Uppercase sharp-s expands; lowercase dotted-I preserves its combining dot.
    assert_eq!(scalar("UPPER(n.text)", &text("Straße")), text("STRASSE"));
    assert_eq!(scalar("LOWER(n.text)", &text("İÉ")), text("i\u{307}é"));
    assert_eq!(
        scalar("TRIM(n.text)", &text("\u{2003} é 🦀 \t")),
        text("é 🦀")
    );
    assert_eq!(
        scalar("LOWER(TRIM(n.text)) || '!' || UPPER('ß')", &text(" É ")),
        text("é!SS")
    );
}

#[test]
fn predicates_are_case_sensitive_and_support_empty_and_unicode_needles() {
    for (expression, expected) in [
        ("n.text STARTS WITH 'Ab'", true),
        ("n.text STARTS WITH 'ab'", false),
        ("n.text ENDS WITH '🦀'", true),
        ("n.text ENDS WITH 'É'", false),
        ("n.text CONTAINS 'bé'", true),
        ("n.text CONTAINS 'BÉ'", false),
        ("n.text CONTAINS ''", true),
        ("n.text STARTS WITH ''", true),
        ("n.text ENDS WITH ''", true),
        ("'Abé🦀' CONTAINS n.text", true),
    ] {
        assert_eq!(
            scalar(expression, &text("Abé🦀")),
            CanonicalScalar::Bool(expected),
            "{expression}"
        );
        let query = prepare(&format!("MATCH (n) WHERE {expression} RETURN n"));
        assert_eq!(
            execute(&query, &text("Abé🦀"), policy())
                .unwrap()
                .value
                .len(),
            usize::from(expected),
            "{expression}"
        );
    }
}

#[test]
fn null_operands_propagate_on_every_text_argument_and_filter_as_unknown() {
    for expression in [
        "UPPER(NULL)",
        "LOWER(NULL)",
        "TRIM(NULL)",
        "CHAR_LENGTH(NULL)",
        "SUBSTRING(NULL,1,1)",
        "SUBSTRING('a',NULL,1)",
        "SUBSTRING('a',1,NULL)",
        "NULL || 'a'",
        "'a' || NULL",
        "NULL STARTS WITH 'a'",
        "'a' STARTS WITH NULL",
        "NULL ENDS WITH 'a'",
        "'a' ENDS WITH NULL",
        "NULL CONTAINS 'a'",
        "'a' CONTAINS NULL",
    ] {
        assert_eq!(
            scalar(expression, &text("a")),
            CanonicalScalar::Null,
            "{expression}"
        );
    }
    for expression in [
        "UPPER(n.text)",
        "LOWER(n.text)",
        "TRIM(n.text)",
        "CHAR_LENGTH(n.text)",
        "SUBSTRING(n.text,1,1)",
        "n.text || 'a'",
        "'a' || n.text",
    ] {
        assert_eq!(
            scalar(expression, &CanonicalScalar::Null),
            CanonicalScalar::Null,
            "{expression}"
        );
    }
    for predicate in [
        "n.text STARTS WITH NULL",
        "NULL STARTS WITH n.text",
        "n.text ENDS WITH NULL",
        "NULL ENDS WITH n.text",
        "n.text CONTAINS NULL",
        "NULL CONTAINS n.text",
    ] {
        let query = prepare(&format!("MATCH (n) WHERE {predicate} RETURN n"));
        assert_eq!(
            execute(&query, &text("a"), policy()).unwrap().value,
            Vec::<GraphValueRow>::new(),
            "{predicate}"
        );
    }
    for predicate in [
        "n.text STARTS WITH 'a'",
        "'a' STARTS WITH n.text",
        "n.text ENDS WITH 'a'",
        "'a' ENDS WITH n.text",
        "n.text CONTAINS 'a'",
        "'a' CONTAINS n.text",
    ] {
        assert_eq!(
            scalar(predicate, &CanonicalScalar::Null),
            CanonicalScalar::Null,
            "{predicate}"
        );
        let query = prepare(&format!("MATCH (n) WHERE {predicate} RETURN n"));
        assert!(
            execute(&query, &CanonicalScalar::Null, policy())
                .unwrap()
                .value
                .is_empty(),
            "{predicate}"
        );
    }
}

#[test]
fn literal_list_membership_preserves_three_valued_equality() {
    for (expression, expected) in [
        ("n.text IN ['other','Ab',NULL]", CanonicalScalar::Bool(true)),
        ("n.text IN ['other',NULL]", CanonicalScalar::Null),
        ("n.text IN ['other']", CanonicalScalar::Bool(false)),
        (
            "UPPER(n.text) IN ['AB','OTHER']",
            CanonicalScalar::Bool(true),
        ),
        ("NULL IN ['Ab']", CanonicalScalar::Null),
    ] {
        assert_eq!(scalar(expression, &text("Ab")), expected, "{expression}");
    }
    let query =
        prepare("MATCH (n) WHERE UPPER(n.text) IN ['AB','OTHER'] RETURN LOWER(n.text) AS value");
    assert_eq!(
        execute(&query, &text("Ab"), policy()).unwrap().value[0].values()[0].as_scalar(),
        Some(&text("ab"))
    );
}

#[test]
fn static_type_errors_refuse_during_preparation() {
    for expression in [
        "UPPER(1)",
        "LOWER(TRUE)",
        "TRIM(2)",
        "CHAR_LENGTH(FALSE)",
        "SUBSTRING(1,1,1)",
        "SUBSTRING('a','bad',1)",
        "SUBSTRING('a',1,TRUE)",
        "'a' || 1",
        "1 || 'a'",
        "1 STARTS WITH 'a'",
        "'a' ENDS WITH 1",
        "1 CONTAINS 'a'",
    ] {
        let statement = format!("MATCH (n) RETURN {expression} AS value");
        let error = PreparedGraphSetText::prepare(&statement, symbols).expect_err(&statement);
        assert!(
            matches!(
                error.kind,
                fgdb_gql::GraphSetTextErrorKind::IntegerExpression(
                    fgdb_gql::GraphIntegerBuildError::OperandType { .. }
                )
            ),
            "{statement}: {error:?}"
        );
    }
}

#[test]
fn string_construction_growth_is_charged_even_when_final_output_is_an_integer() {
    // Identical input, operator tree and integer output shape. Only a prepared
    // literal grows; temporary text construction cannot hide in source charges.
    let short_query = prepare("MATCH (n) RETURN CHAR_LENGTH(UPPER(n.text || 'ß')) AS value");
    let long_query = prepare(&format!(
        "MATCH (n) RETURN CHAR_LENGTH(UPPER(n.text || '{}')) AS value",
        "ß".repeat(128)
    ));
    let input = text("x");
    let short = execute(&short_query, &input, policy()).unwrap();
    let long = execute(&long_query, &input, policy()).unwrap();
    assert_eq!(
        long.value[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::Int(257))
    );
    assert!(long.evaluator.work_units > short.evaluator.work_units);
    assert!(long.evaluator.scratch_entries > short.evaluator.scratch_entries);
    let scratch_cap = GqlQueryPolicy::new(100, 100, 1_000_000, short.evaluator.scratch_entries);
    assert!(execute(&short_query, &input, scratch_cap).is_ok());
    assert!(matches!(
        execute(&long_query, &input, scratch_cap),
        Err(GqlQueryError::Evaluator(_))
    ));
    let work_cap = GqlQueryPolicy::new(100, 100, short.evaluator.work_units, 1_000_000);
    assert!(execute(&short_query, &input, work_cap).is_ok());
    assert!(matches!(
        execute(&long_query, &input, work_cap),
        Err(GqlQueryError::Evaluator(_))
    ));
}
