//! Text expressions use the shared typed, governed relational evaluator.
use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetExecutionError,
    GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_types::{CanonicalPropertyValue, CanonicalScalar, VId};

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
    let input = &CanonicalPropertyValue::from(input.clone());
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
    // The comma form counts scalars from 0 (openCypher, fgdb-qkeq2): the
    // combining accent is the scalar at offset 3.
    assert_eq!(scalar("SUBSTRING(n.text,2,2)", &input), text("a\u{301}"));
    assert_eq!(scalar("SUBSTRING(n.text,3,1)", &input), text("\u{301}"));
    assert_eq!(scalar("SUBSTRING(n.text,9,2)", &input), text(""));
    assert_eq!(scalar("SUBSTRING(n.text,2,0)", &input), text(""));
    assert_eq!(
        scalar("SUBSTRING(n.text FROM 2 FOR 2)", &input),
        text("🦀a")
    );
}

/// fgdb-qkeq2, owner ruling option (a): SUBSTRING splits by spelling. The
/// openCypher comma form substring(s, start [, length]) counts from 0 and
/// its length is optional (TCK String1 [1]). GQL/SQL SUBSTRING(s FROM start
/// [FOR length]) counts from 1. On the same text the two agree exactly when
/// the FROM start is one more, yet compile to different programs. A
/// negative start is refused by the comma form, as Neo4j refuses it, and
/// clamped by the FROM form, as before. Expectations come from Rust's own
/// scalar iterator, never from the engine.
#[test]
fn substring_comma_form_is_zero_based_and_from_form_is_one_based() {
    let digits = text("0123456789");
    for (expression, expected) in [
        // TCK String1 [1] and the bead's own cases.
        ("substring(n.text, 1)", "123456789"),
        ("substring(n.text, 1, 3)", "123"),
        ("substring(n.text, 0, 3)", "012"),
        ("SubString(n.text, 9)", "9"),
        ("substring(n.text, 10)", ""),
        ("SUBSTRING(n.text FROM 1)", "0123456789"),
        ("SUBSTRING(n.text FROM 1 FOR 3)", "012"),
        ("SUBSTRING(n.text FROM 2 FOR 3)", "123"),
        ("SUBSTRING(n.text FROM 11)", ""),
        // Positions -3..1 of the FROM form hold only position 1.
        ("SUBSTRING(n.text FROM -3 FOR 5)", "0"),
        ("SUBSTRING(n.text FROM -1)", "0123456789"),
    ] {
        assert_eq!(scalar(expression, &digits), text(expected), "{expression}");
    }
    // Five scalars: é, the crab, a, a combining acute accent, b.
    let unicode = "é🦀a\u{301}b";
    let input = text(unicode);
    let oracle = |start: usize, length: usize| {
        text(&unicode.chars().skip(start).take(length).collect::<String>())
    };
    for start in 0..=6 {
        let rest = oracle(start, usize::MAX);
        assert_eq!(scalar(&format!("substring(n.text, {start})"), &input), rest);
        assert_eq!(
            scalar(&format!("SUBSTRING(n.text FROM {})", start + 1), &input),
            rest
        );
        for length in 0..=6 {
            let expected = oracle(start, length);
            assert_eq!(
                scalar(&format!("substring(n.text, {start}, {length})"), &input),
                expected,
                "comma {start} {length}"
            );
            assert_eq!(
                scalar(
                    &format!("SUBSTRING(n.text FROM {} FOR {length})", start + 1),
                    &input
                ),
                expected,
                "FROM {start} FOR {length}"
            );
        }
    }
    for (comma, from) in [
        ("substring(n.text, 1)", "SUBSTRING(n.text FROM 1)"),
        ("substring(n.text, 1, 2)", "SUBSTRING(n.text FROM 1 FOR 2)"),
    ] {
        assert_ne!(
            prepare(&format!("MATCH (n) RETURN {comma} AS value")).canonical_bytes(),
            prepare(&format!("MATCH (n) RETURN {from} AS value")).canonical_bytes(),
            "{comma}"
        );
    }
    for expression in ["substring(n.text, -1)", "substring(n.text, -1, 3)"] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        assert!(
            matches!(
                execute(&query, &digits, policy()),
                Err(GqlQueryError::Source(GraphSetExecutionError::Projection {
                    error: fgdb_gql::GraphIntegerError {
                        kind: fgdb_gql::GraphIntegerErrorKind::InvalidSubstring,
                        ..
                    },
                    ..
                }))
            ),
            "{expression}"
        );
    }
    for expression in [
        "substring(NULL, 1)",
        "substring('a', NULL)",
        "substring('a', 0, NULL)",
        "SUBSTRING(NULL FROM 1)",
        "SUBSTRING('a' FROM NULL FOR 1)",
    ] {
        assert_eq!(
            scalar(expression, &digits),
            CanonicalScalar::Null,
            "{expression}"
        );
    }
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
    // A bare literal left operand takes the same scalar path in WHERE, so
    // NULL membership must also refuse to become TRUE there.
    let query = prepare("MATCH (n) WHERE NULL IN ['Ab'] RETURN n");
    assert!(
        execute(&query, &text("Ab"), policy())
            .unwrap()
            .value
            .is_empty()
    );
    let query = prepare("MATCH (n) WHERE 'Ab' IN [NULL, 'other'] RETURN n");
    assert!(
        execute(&query, &text("Ab"), policy())
            .unwrap()
            .value
            .is_empty()
    );
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

/// fgdb-xakp1: toUpper, toLower and size() are the openCypher spellings of
/// UPPER, LOWER and CHAR_LENGTH. Inside a scalar expression toUpper and
/// toLower compile to the very same program. size() of a property is the
/// list-or-text Size node everywhere, since a property may hold a stored List
/// (fgdb-k3nub): text counts Unicode scalars exactly as CHAR_LENGTH does, a
/// list keeps its length, NULL stays NULL, and any other scalar is a typed
/// failure, inside arithmetic too.
#[test]
fn opencypher_text_function_spellings_are_the_same_operations() {
    for (cypher, gql) in [
        ("toUpper(n.text)", "UPPER(n.text)"),
        ("TOLOWER(n.text)", "LOWER(n.text)"),
    ] {
        assert_eq!(
            prepare(&format!("MATCH (n) RETURN {cypher} AS value")).canonical_bytes(),
            prepare(&format!("MATCH (n) RETURN {gql} AS value")).canonical_bytes(),
            "{cypher}"
        );
    }
    let input = text("é🦀a\u{301}");
    assert_eq!(scalar("1 + size(n.text)", &input), CanonicalScalar::Int(5));
    assert_eq!(
        scalar("1 + size(n.text)", &input),
        scalar("1 + CHAR_LENGTH(n.text)", &input)
    );
    assert_eq!(
        scalar("1 + size(n.text)", &CanonicalScalar::Null),
        CanonicalScalar::Null
    );
    assert!(matches!(
        execute(
            &prepare("MATCH (n) RETURN 1 + size(n.text) AS value"),
            &CanonicalScalar::Int(42),
            policy()
        ),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
    assert_eq!(scalar("toUpper(n.text)", &input), text("É🦀A\u{301}"));
    assert_eq!(scalar("size(n.text)", &input), CanonicalScalar::Int(4));
    assert_eq!(
        scalar("size(n.text)", &input),
        scalar("CHAR_LENGTH(n.text)", &input)
    );
    assert_eq!(scalar("size('héllo')", &input), CanonicalScalar::Int(5));
    assert_eq!(
        scalar("size([1, n.text, 'x'])", &input),
        CanonicalScalar::Int(3)
    );
    assert_eq!(
        scalar("size(n.text)", &CanonicalScalar::Null),
        CanonicalScalar::Null
    );
    // The graph pattern's WHERE and a WITH's WHERE both admit the spellings.
    for (statement, rows) in [
        ("MATCH (n) WHERE size(n.text) = 4 RETURN n.text AS value", 1),
        ("MATCH (n) WHERE size(n.text) = 9 RETURN n.text AS value", 0),
        (
            "MATCH (n) WHERE toLower(n.text) = 'é🦀a\u{301}' RETURN n.text AS value",
            1,
        ),
        (
            "MATCH (n) WITH n.text AS t WHERE size(t) = 4 RETURN t AS value",
            1,
        ),
    ] {
        let result = execute(&prepare(statement), &input, policy()).unwrap();
        assert_eq!(result.value.len(), rows, "{statement}");
    }
    // Not a count of anything: a non-text scalar fails, typed, never 0.
    let non_text = prepare("MATCH (n) RETURN size(n.text) AS value");
    assert!(matches!(
        execute(&non_text, &CanonicalScalar::Int(42), policy()),
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
    // A non-text literal refuses before execution: inside a scalar expression
    // at preparation, as CHAR_LENGTH(FALSE) does; as a Size node at binding,
    // where the relational projection is admitted.
    assert!(
        PreparedGraphSetText::prepare("MATCH (n) RETURN 1 + size(TRUE) AS value", symbols).is_err()
    );
    for literal in ["TRUE", "42"] {
        let statement = format!("MATCH (n) RETURN size({literal}) AS value");
        let template = PreparedGraphSetText::prepare(&statement, symbols).unwrap();
        assert!(
            template.bind_parameters(&GqlParameters::new()).is_err(),
            "{statement}"
        );
    }
    // Counting reads the text, charged like CHAR_LENGTH: one unit of work per
    // payload unit. Differencing against a plain read of the same property
    // isolates that charge from the source's own payload accounting.
    let plain = prepare("MATCH (n) RETURN n.text AS value");
    let work = |query: &PreparedGraphSet, input: &CanonicalScalar| {
        execute(query, input, policy())
            .unwrap()
            .evaluator
            .work_units
    };
    let (short, long) = (text("x"), text(&"x".repeat(1024)));
    assert_eq!(
        execute(&non_text, &long, policy()).unwrap().value[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::Int(1024))
    );
    let counted_long = work(&non_text, &long) - work(&plain, &long);
    let counted_short = work(&non_text, &short) - work(&plain, &short);
    assert_eq!(counted_long - counted_short, 1024 / 64 - 1);
}

/// fgdb-xakp1: SIZE over a text-valued aggregate runs in the aggregate result
/// projection's own evaluator, which counts characters exactly as the row
/// evaluator does, and still fails typed on a non-text scalar.
#[test]
fn size_of_a_text_aggregate_counts_characters_in_the_result_projection() {
    let query = fgdb_gql::PreparedGraphAggregateText::prepare(
        "MATCH (n) RETURN SIZE(MIN(n.text)) AS chars, SIZE(COLLECT(n.text)) AS items",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let run = |input: &CanonicalScalar| {
        let input = &CanonicalPropertyValue::from(input.clone());
        query.execute_governed(
            1,
            [VId(1)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, key| Ok((key == PropertyKeyId(1)).then_some(input)),
            policy(),
            || Ok::<_, ()>(()),
        )
    };
    let result = run(&text("é🦀a\u{301}")).unwrap();
    let values = result.value[0].values();
    let int = |n| {
        Some(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(
            n,
        )))
    };
    assert_eq!(values[0].as_value().cloned(), int(4));
    assert_eq!(values[1].as_value().cloned(), int(1));
    assert!(run(&CanonicalScalar::Int(42)).is_err());
}

/// fgdb-20foe: openCypher left, right, replace and length(text), and
/// substring without a length. Positions count Unicode scalars, as
/// CHAR_LENGTH does. The comma form of substring counts from 0, the
/// SUBSTRING ... FROM form from 1 (fgdb-qkeq2). NULL in any argument is
/// NULL; a negative length, or a negative comma-form start, is
/// InvalidSubstring. Each filters in WHERE, reads WITH aliases, runs in the
/// aggregate result projection, and charges the text it reads and builds.
#[test]
fn opencypher_left_right_replace_length_and_two_argument_substring() {
    // Five scalars: é, the crab, a, a combining acute accent, b.
    let input = text("é🦀a\u{301}b");
    for (expression, expected) in [
        ("left(n.text, 2)", text("é🦀")),
        ("right(n.text, 2)", text("\u{301}b")),
        ("left(n.text, 0)", text("")),
        ("right(n.text, 0)", text("")),
        ("left(n.text, 5)", input.clone()),
        ("right(n.text, 9)", input.clone()),
        ("LEFT('hello', 2)", text("he")),
        ("Right('hello', 2)", text("lo")),
        ("replace('hello', 'e', 'x')", text("hxllo")),
        ("replace('banana', 'an', 'AN')", text("bANANa")),
        // Non-overlapping occurrences, left to right.
        ("replace('aaa', 'aa', 'b')", text("ba")),
        ("replace(n.text, '🦀', 'crab')", text("écraba\u{301}b")),
        ("replace('abc', 'b', '')", text("ac")),
        ("replace('abc', 'x', 'y')", text("abc")),
        // An empty search matches at every character boundary, both ends
        // included, as Java's String.replace does for Neo4j.
        ("replace('abc', '', '-')", text("-a-b-c-")),
        ("replace('é🦀', '', '-')", text("-é-🦀-")),
        ("replace('', '', '-')", text("-")),
        ("length(n.text)", CanonicalScalar::Int(5)),
        ("length('hello')", CanonicalScalar::Int(5)),
        ("length('') + 1", CanonicalScalar::Int(1)),
        ("length(left(n.text, 3))", CanonicalScalar::Int(3)),
        ("substring('hello', 2)", text("llo")),
        ("SUBSTRING('hello' FROM 4)", text("lo")),
        ("substring(n.text, 2)", text("a\u{301}b")),
        ("substring('hello', 0)", text("hello")),
        ("substring('hello', 5)", text("")),
        ("substring('hello', 9)", text("")),
        // The results are static text: `+` concatenates them.
        ("left(upper(n.text), 1) + right('xyz', 1)", text("Éz")),
        ("replace('a', 'a', 'b') + substring('xyz', 2)", text("bz")),
    ] {
        assert_eq!(scalar(expression, &input), expected, "{expression}");
    }
    // length(text) is the CHAR_LENGTH program.
    assert_eq!(
        prepare("MATCH (n) RETURN length(n.text) AS value").canonical_bytes(),
        prepare("MATCH (n) RETURN CHAR_LENGTH(n.text) AS value").canonical_bytes()
    );
    assert_ne!(
        prepare("MATCH (n) RETURN left(n.text, 2) AS value").canonical_bytes(),
        prepare("MATCH (n) RETURN right(n.text, 2) AS value").canonical_bytes()
    );
    for expression in [
        "left(NULL, 1)",
        "left('a', NULL)",
        "right(NULL, 1)",
        "right('a', NULL)",
        "replace(NULL, 'a', 'b')",
        "replace('a', NULL, 'b')",
        "replace('a', 'a', NULL)",
        "length(NULL)",
        "substring(NULL, 1)",
        "substring('a', NULL)",
    ] {
        assert_eq!(
            scalar(expression, &input),
            CanonicalScalar::Null,
            "{expression}"
        );
    }
    for expression in [
        "left(n.text, 1)",
        "right(n.text, 1)",
        "replace(n.text, 'a', 'b')",
        "replace('a', n.text, 'b')",
        "replace('a', 'a', n.text)",
        "length(n.text)",
        "substring(n.text, 1)",
    ] {
        assert_eq!(
            scalar(expression, &CanonicalScalar::Null),
            CanonicalScalar::Null,
            "{expression}"
        );
    }
    for expression in [
        "left(n.text, -1)",
        "right(n.text, -1)",
        // The openCypher comma form refuses a negative start, as Neo4j does.
        "substring(n.text, -1)",
        "substring(n.text, -1, 2)",
        "substring(n.text, 0, -1)",
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        assert!(
            matches!(
                execute(&query, &input, policy()),
                Err(GqlQueryError::Source(GraphSetExecutionError::Projection {
                    error: fgdb_gql::GraphIntegerError {
                        kind: fgdb_gql::GraphIntegerErrorKind::InvalidSubstring,
                        ..
                    },
                    ..
                }))
            ),
            "{expression}"
        );
    }
    for expression in [
        "left(1, 1)",
        "right('a', 'b')",
        "left('a', TRUE)",
        "replace('a', 1, 'b')",
        "replace(1, 'a', 'b')",
        "length(1)",
        "substring(1, 1)",
        "substring('a', 'b')",
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
    // The graph pattern's WHERE and a WITH's WHERE and RETURN admit them.
    for (statement, rows) in [
        (
            "MATCH (n) WHERE left(n.text, 1) = 'é' RETURN n.text AS value",
            1,
        ),
        (
            "MATCH (n) WHERE right(n.text, 1) = 'é' RETURN n.text AS value",
            0,
        ),
        (
            "MATCH (n) WHERE length(n.text) = 5 RETURN n.text AS value",
            1,
        ),
        (
            "MATCH (n) WHERE length(n.text) > 5 RETURN n.text AS value",
            0,
        ),
        (
            "MATCH (n) WHERE substring(n.text, 4) = 'b' RETURN n.text AS value",
            1,
        ),
        (
            "MATCH (n) WHERE replace(n.text, 'b', '') = 'é🦀a\u{301}' RETURN n.text AS value",
            1,
        ),
        (
            "MATCH (n) WITH n.text AS t WHERE left(t, 2) = 'é🦀' AND length(t) = 5 \
             RETURN replace(t, 'b', 'B') AS value",
            1,
        ),
    ] {
        let result = execute(&prepare(statement), &input, policy()).unwrap();
        assert_eq!(result.value.len(), rows, "{statement}");
    }
    let carried = prepare(
        "MATCH (n) WITH n.text AS t RETURN replace(t, 'b', 'B') AS r, length(t) AS c, \
         right(t, 3) AS tail, substring(t, 3) AS rest",
    );
    assert_eq!(
        execute(&carried, &input, policy()).unwrap().value[0].values(),
        &[
            fgdb_gql::algebra::GraphValue::Scalar(text("é🦀a\u{301}B")),
            fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(5)),
            fgdb_gql::algebra::GraphValue::Scalar(text("a\u{301}b")),
            fgdb_gql::algebra::GraphValue::Scalar(text("\u{301}b")),
        ]
    );
    // The aggregate result projection runs the same instructions.
    let stored = CanonicalPropertyValue::from(input.clone());
    let grouped = fgdb_gql::PreparedGraphAggregateText::prepare(
        "MATCH (n) RETURN left(MIN(n.text), 2) AS l, length(MIN(n.text)) AS c, \
         replace(MAX(n.text), 'b', 'B') AS r, substring(MIN(n.text), 3) AS s",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .execute_governed(
        1,
        [VId(1)],
        [],
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(Some(&stored)),
        policy(),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    let values: Vec<_> = grouped.value[0]
        .values()
        .iter()
        .map(|value| value.as_value().cloned())
        .collect();
    let value = |scalar| Some(fgdb_gql::algebra::GraphValue::Scalar(scalar));
    assert_eq!(
        values,
        vec![
            value(text("é🦀")),
            value(CanonicalScalar::Int(5)),
            value(text("é🦀a\u{301}B")),
            value(text("\u{301}b")),
        ]
    );
    // replace reserves the text it builds before building it, and left
    // charges the text it reads: only the replacement's or input's length
    // differs between each pair.
    let short = prepare("MATCH (n) RETURN length(replace(n.text, 'a', 'ß')) AS value");
    let long = prepare(&format!(
        "MATCH (n) RETURN length(replace(n.text, 'a', '{}')) AS value",
        "ß".repeat(128)
    ));
    let small = execute(&short, &input, policy()).unwrap();
    let large = execute(&long, &input, policy()).unwrap();
    assert_eq!(
        large.value[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::Int(132))
    );
    assert!(large.evaluator.work_units > small.evaluator.work_units);
    assert!(large.evaluator.scratch_entries > small.evaluator.scratch_entries);
    let scratch_cap = GqlQueryPolicy::new(100, 100, 1_000_000, small.evaluator.scratch_entries);
    assert!(execute(&short, &input, scratch_cap).is_ok());
    assert!(matches!(
        execute(&long, &input, scratch_cap),
        Err(GqlQueryError::Evaluator(_))
    ));
    let read = prepare("MATCH (n) RETURN left(n.text, 1) AS value");
    let plain = prepare("MATCH (n) RETURN n.text AS value");
    let work = |query: &PreparedGraphSet, input: &CanonicalScalar| {
        execute(query, input, policy())
            .unwrap()
            .evaluator
            .work_units
    };
    let (short, long) = (text("x"), text(&"x".repeat(1024)));
    assert_eq!(
        (work(&read, &long) - work(&plain, &long)) - (work(&read, &short) - work(&plain, &short)),
        1024 / 64 - 1
    );
}

/// fgdb-j687q: openCypher `+` concatenates when either operand is statically
/// text, compiling to the same program `||` does, in RETURN and WHERE and
/// through chains. An operand of unknown type keeps integer addition, and a
/// text literal added to an integer is refused at preparation, as `'a' || 1`
/// is.
#[test]
fn opencypher_plus_over_text_is_concatenation() {
    for (cypher, gql) in [
        ("n.text + '!'", "n.text || '!'"),
        ("'<' + n.text + '>'", "'<' || n.text || '>'"),
        ("toUpper(n.text) + n.text", "UPPER(n.text) || n.text"),
    ] {
        assert_eq!(
            prepare(&format!("MATCH (n) RETURN {cypher} AS value")).canonical_bytes(),
            prepare(&format!("MATCH (n) RETURN {gql} AS value")).canonical_bytes(),
            "{cypher}"
        );
    }
    let input = text("ab");
    assert_eq!(scalar("'<' + n.text + '>'", &input), text("<ab>"));
    assert_eq!(scalar("1 + 2", &input), CanonicalScalar::Int(3));
    let rows = |statement: &str| {
        execute(&prepare(statement), &input, policy())
            .unwrap()
            .value
            .len()
    };
    assert_eq!(
        rows("MATCH (n) WHERE n.text + '!' = 'ab!' RETURN n.text AS value"),
        1
    );
    assert_eq!(
        rows("MATCH (n) WHERE n.text + '?' = 'ab!' RETURN n.text AS value"),
        0
    );
    // A column of unknown type is not guessed: text + integer fails typed.
    assert!(
        execute(
            &prepare("MATCH (n) RETURN n.text + 1 AS value"),
            &input,
            policy()
        )
        .is_err()
    );
    for statement in [
        "MATCH (n) RETURN 'a' + 1 AS value",
        "MATCH (n) RETURN 'a' - 'b' AS value",
    ] {
        assert!(
            PreparedGraphSetText::prepare(statement, symbols).is_err(),
            "{statement}"
        );
    }
}

/// openCypher's hexadecimal and octal integers (TCK Literals3, Literals4), GQL's
/// binary ones, and floats without integer digits (Literals5) are ordinary
/// operands of a computed projection.
#[test]
fn radix_integers_and_dot_floats_compute_in_projections() {
    let float = |value: f64| CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value));
    for (expression, expected) in [
        ("0x1F + 0o7 + 0b1", CanonicalScalar::Int(39)),
        ("0x1a2b3c4d5e6f7", CanonicalScalar::Int(460_367_961_908_983)),
        ("-0x8000000000000000", CanonicalScalar::Int(i64::MIN)),
        ("-0o1000000000000000000000", CanonicalScalar::Int(i64::MIN)),
        ("0o777777777777777777777", CanonicalScalar::Int(i64::MAX)),
        ("-0x1", CanonicalScalar::Int(-1)),
        (".5 * 2", float(1.0)),
        (".1e9", float(1e8)),
        (".1E-5", float(1e-6)),
        ("-.25", float(-0.25)),
    ] {
        assert_eq!(scalar(expression, &text("x")), expected, "{expression}");
    }
}

/// A literal may be the left operand of a WHERE comparison (`1 = n.p`,
/// `'w' < n.text`), and a comparison of two literals filters every row or
/// none (TCK ReturnSkipLimit1 `WHERE 1 = 0`).
#[test]
fn a_literal_left_operand_compares_like_the_mirrored_comparison() {
    let count = |statement: &str, input: &CanonicalScalar| {
        execute(&prepare(statement), input, policy())
            .unwrap()
            .value
            .len()
    };
    for (statement, input, rows) in [
        ("MATCH (n) WHERE 'x' = n.text RETURN n", text("x"), 1),
        ("MATCH (n) WHERE 'x' = n.text RETURN n", text("y"), 0),
        ("MATCH (n) WHERE 'w' < n.text RETURN n", text("x"), 1),
        ("MATCH (n) WHERE 'z' < n.text RETURN n", text("x"), 0),
        (
            "MATCH (n) WHERE 'x' = n.text RETURN n",
            CanonicalScalar::Null,
            0,
        ),
        ("MATCH (n) WHERE 1 = 0 RETURN n", text("x"), 0),
        ("MATCH (n) WHERE 1 = 1 RETURN n", text("x"), 1),
    ] {
        assert_eq!(count(statement, &input), rows, "{statement} over {input:?}");
    }
}

/// fgdb-o5o9t: a list quantifier, comprehension, list function or size() is
/// an operand of the scalar operators around it, read through a let slot.
/// A let's values read the enclosing row, so an enclosing list element keeps
/// its meaning inside them and in the body.
#[test]
fn composite_values_are_operands_of_scalar_operators() {
    let truth = |value: bool| CanonicalScalar::Bool(value);
    for (expression, expected) in [
        // The TCK's quantifier equivalences (Quantifier5-8).
        (
            "none(x IN [1, 2, 3] WHERE x = 2) = (NOT any(x IN [1, 2, 3] WHERE x = 2))",
            truth(true),
        ),
        (
            "all(x IN [1, 2, 3] WHERE x > 0) AND any(x IN [1, 2, 3] WHERE x = 3)",
            truth(true),
        ),
        (
            "all(x IN [1, 2, 3] WHERE x > 0) AND any(x IN [1] WHERE x = 3)",
            truth(false),
        ),
        (
            "single(x IN [1, 2, 3] WHERE x = 2) = (size([x IN [1, 2, 3] WHERE x = 2]) = 1)",
            truth(true),
        ),
        (
            "size([x IN [1, 2, 3, 4] WHERE x % 2 = 0]) + 1",
            CanonicalScalar::Int(3),
        ),
        ("10 - size(range(1, 4))", CanonicalScalar::Int(6)),
        ("head([7, 8]) * 2", CanonicalScalar::Int(14)),
        // Quantifiers inside a quantifier's predicate (Quantifier5 [1]).
        (
            "none(x IN [['abc'], ['abc', 'def']] WHERE any(y IN x WHERE y = 'ghi'))",
            truth(true),
        ),
        // size() of a list element (Quantifier1 [6]).
        (
            "none(x IN [[1, 2, 3], ['a']] WHERE size(x) = 3)",
            truth(false),
        ),
        // The body reads the element `x` one slot further out; the slot's
        // own comprehension reads it in place. Postfix `[2]` then `=`.
        (
            "[x IN [1, 2, 3] | x + size([y IN [1, 2] WHERE y < x])][2] = 5",
            truth(true),
        ),
        // Two slots: x is two slots further out in the body, and the slots
        // keep their order: 1 - 3 + 5.
        (
            "[x IN [1, 5] | size([y IN [x] WHERE y = x]) - size([z IN [x, x, x] WHERE z = x]) + x][1]",
            CanonicalScalar::Int(3),
        ),
        // A map element's entries (Quantifier1-4 [7]). A missing key is NULL,
        // so its predicate is UNKNOWN and no element decides none().
        (
            "single(x IN [{a: 2, b: 5}, {a: 4}] WHERE x.a = 2)",
            truth(true),
        ),
        ("none(x IN [{b: 5}] WHERE x.a = 2)", CanonicalScalar::Null),
        ("all(x IN [] WHERE x.a = 2)", truth(true)),
        // A lone parenthesized composite is that composite.
        ("(any(x IN [1] WHERE x = 1))", truth(true)),
        // A NULL list stays NULL through the operator.
        ("any(x IN null WHERE x = 1) OR false", CanonicalScalar::Null),
    ] {
        assert_eq!(scalar(expression, &text("x")), expected, "{expression}");
    }
    // A write's SET reads the composite in the value language too: every
    // vertex gets size([2]) + 1 = 2 (CLI read-back).
    assert!(
        fgdb_gql::PreparedGraphMutationText::prepare(
            "MATCH (n) SET n.text = size([x IN [1, 2] WHERE x > 1]) + 1",
            fgdb_delta_types::RelationId(1),
            symbols,
        )
        .is_ok()
    );
}

/// fgdb-9cawv: a list or map literal compared with `=`, `<>`, `!=` or IN is
/// one three-valued comparison: a definite mismatch is FALSE, else an
/// UNKNOWN element makes it UNKNOWN, and maps with different key sets are
/// FALSE whatever their values (TCK Comparison1 [6], [7], List5). Values of
/// different domains are unequal, as openCypher says (fgdb-v6ku6, owner
/// ruling (a), 2026-10-21): FALSE, never UNKNOWN.
#[test]
fn composite_literals_compare_three_valued() {
    let truth = |value: bool| CanonicalScalar::Bool(value);
    let null = CanonicalScalar::Null;
    for (expression, expected) in [
        // Comparison1 [6].
        ("[1, 2] = [1]", truth(false)),
        ("[null] = [1]", null.clone()),
        ("[[1]] = [[1], [null]]", truth(false)),
        ("[[1], [2]] = [[1], [null]]", null.clone()),
        ("[[1], [2, 3]] = [[1], [null]]", truth(false)),
        ("['a'] = [1]", truth(false)),
        ("[1, 2] = [1, 2]", truth(true)),
        ("[1] = null", null.clone()),
        // Comparison1 [7].
        ("{} = {}", truth(true)),
        ("{k: true} = {k: true}", truth(true)),
        ("{k: 1.0} = {k: 1.0}", truth(true)),
        ("{k: 'a', l: 2} = {k: 'a', l: 2}", truth(true)),
        ("{} = {k: null}", truth(false)),
        ("{k: null} = {}", truth(false)),
        ("{k: 1} = {k: 1, l: null}", truth(false)),
        ("{k: null, l: 1} = {l: 1}", truth(false)),
        ("{k: 1} = {l: 1}", truth(false)),
        ("{k: null} = {k: null}", null.clone()),
        ("{k: 1, l: null} = {k: null, l: 1}", null.clone()),
        ("{k: 1, l: null} = {k: 1, l: 1}", null.clone()),
        // Inequality is the negated equality, UNKNOWN staying UNKNOWN.
        ("[1, 2] <> [1]", truth(true)),
        ("[1, 2] != [1, 2]", truth(false)),
        ("{k: null} <> {k: null}", null.clone()),
        // Precedence3 [4]/[5]: additive list expressions bind inside IN.
        ("2 IN [1] + 2", truth(true)),
        ("[1] + 2 IN [[0], [1, 2]]", truth(true)),
        ("[1] + 2 IN [[0]] + [[1, 2]]", truth(true)),
        ("[1] + 2 NOT IN [[0]] + [[9]]", truth(true)),
        // List5: a composite left operand of IN.
        ("[1] IN [1, 2, [1]]", truth(true)),
        ("[[1, 2], [3, 4]] IN [5, [[1, 2], [3, 4]]]", truth(true)),
        ("[1, 2] IN [[2, 1]]", truth(false)),
        ("[] IN [1, []]", truth(true)),
        ("[] IN []", truth(false)),
        ("[null] IN [[null]]", null.clone()),
        // A lone operand IN a list that is not a literal (TCK Graph and
        // List5: 'exists' IN keys(map)): keys(), tail(), a graph property on
        // the left, and NULL membership.
        (
            "'exists' IN keys({exists: 42, notMissing: null})",
            truth(true),
        ),
        (
            "'notMissing' IN keys({exists: 42, notMissing: null})",
            truth(true),
        ),
        (
            "'missing' IN keys({exists: 42, notMissing: null})",
            truth(false),
        ),
        ("2 IN tail([1, 2])", truth(true)),
        ("1 IN tail([1, 2])", truth(false)),
        ("n.text IN keys({x: 1})", truth(true)),
        ("null IN tail([1, 2])", null.clone()),
        ("null IN tail([1])", truth(false)),
        // A list literal read through a slice or subscript (List5 [4]).
        ("3 IN [1, 2, 3][0..1]", truth(false)),
        ("1 IN [1, 2, 3][0..1]", truth(true)),
        ("3 IN [[1], [2, 3]][1]", truth(true)),
        // The comparison is one operand of the operators around it, after
        // the literal's own postfix reads.
        ("[1] = [1] AND [2] <> [3]", truth(true)),
        ("NOT [1] = [2]", truth(true)),
        // IN binds tighter than the comparison (Precedence3 [6]): the right
        // operand is the membership, unless parentheses say otherwise. A
        // list against a Boolean is FALSE across domains (fgdb-v6ku6).
        ("[1, 2] = [3, 4] IN [true, false]", truth(false)),
        ("[1, 2] = ([3, 4] IN [true, false])", truth(false)),
        ("([1, 2] = [3, 4]) IN [true, false]", truth(true)),
        ("[1, 2] = [1, 2] IN [false]", truth(false)),
        ("[1] IN [[1]] = true", truth(true)),
        ("[1, 2][0] = 1", truth(true)),
        ("{k: 1}.k = 1", truth(true)),
        // A graph property inside the literal reads the matched row.
        ("{k: n.text} = {k: 'x'}", truth(true)),
        ("[n.text] = ['y']", truth(false)),
    ] {
        assert_eq!(scalar(expression, &text("x")), expected, "{expression}");
    }
    // The TCK's bare RETURN form, over the unit row.
    let result = execute(
        &prepare("RETURN [1, 2] = [1] AS list, {k: null} = {} AS map"),
        &text("x"),
        policy(),
    )
    .unwrap();
    assert_eq!(result.value.len(), 1);
    assert_eq!(
        result.value[0]
            .values()
            .iter()
            .map(|value| value.as_scalar().unwrap().clone())
            .collect::<Vec<_>>(),
        vec![truth(false), truth(false)]
    );
    // A lone operand IN a row column's subscript or slice (List5 [1], [3]):
    // the left side stops at the IN, so reading it cannot rediscover the
    // same membership.
    let result = execute(
        &prepare(
            "WITH [[1, 2, 3]] AS list \
             RETURN 3 IN list[0] AS a, 3 IN list[0][0..1] AS b, 4 IN list[0] AS c",
        ),
        &text("x"),
        policy(),
    )
    .unwrap();
    assert_eq!(
        result.value[0]
            .values()
            .iter()
            .map(|value| value.as_scalar().unwrap().clone())
            .collect::<Vec<_>>(),
        vec![truth(true), truth(false), truth(false)]
    );
}

/// fgdb-v6ku6, owner ruling (a): openCypher answers FALSE for `=`, TRUE for
/// `<>`, and FALSE for IN between values of different domains, at every
/// level: inside lists (TCK Comparison1 [6], List3, List5) and between
/// scalars at the top. Lists compare element-wise: different lengths are
/// FALSE, a definite mismatch wins, and a NULL makes the answer NULL only
/// when no element pair is decisively unequal. Numbers are one domain
/// (1 = 1.0), and an ordering across domains stays NULL.
#[test]
fn values_of_different_domains_are_unequal_as_in_opencypher() {
    let truth = |value: bool| CanonicalScalar::Bool(value);
    let null = CanonicalScalar::Null;
    let cases = [
        // Comparison1 [6].
        ("['a'] = [1]", truth(false)),
        // List3 [1], [3], [6]; [4] and [7] stay NULL.
        ("[1, 2] = 'foo'", truth(false)),
        ("[1, 2] = [null, 'foo']", truth(false)),
        ("[[1, 2], [1, 3]] = [[1, 2], [null, 'foo']]", truth(false)),
        ("[1, 2] = [null, 2]", null.clone()),
        (
            "[[1, 2], ['foo', 'bar']] = [[1, 2], [null, 'bar']]",
            null.clone(),
        ),
        // List5 [6], [7], [8], [11], [12], [13], [18], [19], [28], [30],
        // [33], [38].
        ("[1, 2] IN [1, [1, '2']]", truth(false)),
        ("[1] IN [1, 2]", truth(false)),
        ("[1, 2] IN [1, 2]", truth(false)),
        ("[1, 2] IN [1, [2, 1]]", truth(false)),
        ("[1, 2] IN [1, [1, 2, 3]]", truth(false)),
        ("[1, 2] IN [1, [[1, 2]]]", truth(false)),
        ("[[1]] IN [2, [1]]", truth(false)),
        ("[[1, 3]] IN [2, [1, 3]]", truth(false)),
        ("[1, 2] IN [[null, 'foo']]", truth(false)),
        ("[1, 2] IN [[1, 2, null]]", truth(false)),
        ("[[1, 2], [3, 4]] IN [[[1, 2], [3, 4], null]]", truth(false)),
        ("[] IN [1, 2]", truth(false)),
        // List5 null rules that stay: [20], [22], [24], [25], [29], [31], [40].
        ("null IN [null]", null.clone()),
        ("[null] IN [null]", null.clone()),
        ("3 IN [1, null, 3]", truth(true)),
        ("4 IN [1, null, 3]", null.clone()),
        ("[1, 2] IN [[null, 2]]", null.clone()),
        ("[1, 2, null] IN [1, [1, 2, null]]", null.clone()),
        ("[] IN [1, null]", null.clone()),
        // Mixed element types against a scalar (List5 [5]).
        ("1 IN ['1', 2]", truth(false)),
        ("[1] = 1", truth(false)),
        ("[1] <> 1", truth(true)),
        ("{a: 1} = [1]", truth(false)),
        ("{a: 1} <> [1]", truth(true)),
        // Top-level scalars of unrelated kinds.
        ("1 = 'a'", truth(false)),
        ("1 <> 'a'", truth(true)),
        ("1 != 'a'", truth(true)),
        ("NOT (1 = 'a')", truth(true)),
        ("1 = true", truth(false)),
        ("1.5 <> false", truth(true)),
        ("1 IN ['a', 2]", truth(false)),
        ("'a' IN [1, 'a']", truth(true)),
        ("1 NOT IN ['a', 2]", truth(true)),
        ("2 IN ['a', null]", null.clone()),
        ("date('2020-01-01') = 1", truth(false)),
        ("date('2020-01-01') <> 'a'", truth(true)),
        ("CASE 1 WHEN 'a' THEN 'x' ELSE 'y' END", text("y")),
        ("n.text = 1", truth(false)),
        ("n.text <> 1", truth(true)),
        ("n.text IN [1, 'x']", truth(true)),
        ("n.text IN [1, true]", truth(false)),
        ("n.text IN [1, null]", null.clone()),
        // The null rules and numeric equality that must not move.
        ("1 IN [1, null]", truth(true)),
        ("2 IN [1, null]", null.clone()),
        ("null = null", null.clone()),
        ("1 = null", null.clone()),
        ("1 = 1.0", truth(true)),
        ("1 <> 1.0", truth(false)),
        ("1 IN [1.0, 'a']", truth(true)),
        // An ordering across domains stays NULL.
        ("[1] < ['a']", null.clone()),
    ];
    for (expression, expected) in &cases {
        assert_eq!(&scalar(expression, &text("x")), expected, "{expression}");
        // The TCK's bare RETURN form, over the unit row.
        if !expression.contains("n.text") {
            let result = execute(
                &prepare(&format!("RETURN {expression} AS x")),
                &text("x"),
                policy(),
            )
            .unwrap();
            assert_eq!(
                result.value[0].values()[0].as_scalar(),
                Some(expected),
                "RETURN {expression}"
            );
        }
    }
    // A stored property of another domain under WHERE: `=` drops the row,
    // `<>` and NOT `=` keep it; NULL stays UNKNOWN either way.
    for (predicate, input, kept) in [
        ("n.text = 1", text("x"), false),
        ("n.text <> 1", text("x"), true),
        ("NOT n.text = 1", text("x"), true),
        ("NOT (n.text = 1 OR n.text = true)", text("x"), true),
        ("n.text = 1 OR n.text <> true", text("x"), true),
        ("NOT n.text IN [1, true]", text("x"), true),
        ("n.text <> 'x'", CanonicalScalar::Int(1), true),
        ("NOT n.text = 'x'", CanonicalScalar::Bool(true), true),
        ("n.text <> 1", CanonicalScalar::Null, false),
        ("NOT n.text = 1", CanonicalScalar::Null, false),
        ("n.text < 1", text("x"), false),
        ("NOT n.text < 1", text("x"), false),
    ] {
        let query = prepare(&format!("MATCH (n) WHERE {predicate} RETURN n.text AS v"));
        assert_eq!(
            execute(&query, &input, policy()).unwrap().value.len(),
            usize::from(kept),
            "{predicate} over {input:?}"
        );
    }
}

/// openCypher `^` (TCK Precedence2 [2] to [4], Return2 [1]): always a float,
/// left-associative, tighter than `*`, `/` and `%` and looser than a sign. NULL
/// propagates. An integer to a non-negative integer power is exact, rounded
/// once; a fractional exponent is a typed failure, never a host `powf`.
#[test]
fn exponentiation_is_a_left_associative_float_power() {
    let float = |value: f64| CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value));
    for (expression, expected) in [
        ("4 ^ 3 * 2 ^ 3", float(512.0)),
        ("4 ^ (3 * 2) ^ 3", float(68_719_476_736.0)),
        ("4 ^ 3 / 2 ^ 3", float(8.0)),
        ("4 ^ 3 % 2 ^ 3", float(0.0)),
        ("4 ^ (3 % 2) ^ 3", float(64.0)),
        ("4 ^ 3 + 2 ^ 3", float(72.0)),
        ("4 ^ 3 - 2 ^ 3", float(56.0)),
        ("-3 ^ 2", float(9.0)),
        ("(-3) ^ 2", float(9.0)),
        ("-(3 ^ 2)", float(-9.0)),
        ("2 ^ -2", float(0.25)),
        ("1.5 ^ 2", float(2.25)),
        ("2 ^ 2.0", float(4.0)),
        ("0 ^ 0", float(1.0)),
        // 3^40 = 12157665459056928801, rounded once to the nearest binary64.
        ("3 ^ 40", float(12_157_665_459_056_928_768.0)),
        ("5 ^ (6 % null)", CanonicalScalar::Null),
        (
            "1 + (2 - (3 * (4 / (5 ^ (6 % null)))))",
            CanonicalScalar::Null,
        ),
    ] {
        assert_eq!(scalar(expression, &text("x")), expected, "{expression}");
    }
    assert!(
        execute(
            &prepare("MATCH (n) RETURN 2 ^ 0.5 AS value"),
            &text("x"),
            policy()
        )
        .is_err()
    );
}

/// A comparison chain compares neighbours, as openCypher defines it: `a < b
/// <= c` is `a < b AND b <= c`, three-valued (TCK Comparison3 [1] to [9],
/// Comparison4 [1]), never `(a < b) <= c`.
#[test]
fn comparison_chains_compare_each_neighbouring_pair() {
    let truth = |value: bool| CanonicalScalar::Bool(value);
    for (expression, expected) in [
        ("1 < 2 < 3", truth(true)),
        ("1 < 3 < 2", truth(false)),
        ("3 > 2 >= 2", truth(true)),
        ("1 < 2 > 1", truth(true)),
        ("1 = 1 = 1", truth(true)),
        ("1 < 2 < 3 < 4 <= 4", truth(true)),
        ("1 < 2 < 3 < 4 < 4", truth(false)),
        // FALSE AND UNKNOWN is FALSE; TRUE AND UNKNOWN is UNKNOWN.
        ("2 < 1 < null", truth(false)),
        ("1 < 2 < null", CanonicalScalar::Null),
        ("'a' < n.text < 'c'", truth(true)),
        ("'a' < n.text <= 'b'", truth(true)),
        ("'c' < n.text", truth(false)),
        // The chain is one operand of the Boolean operators around it.
        ("NOT 1 < 2 < 3", truth(false)),
        ("1 < 2 < 3 AND 1 = 2", truth(false)),
    ] {
        assert_eq!(scalar(expression, &text("b")), expected, "{expression}");
    }
    // The TCK's WHERE forms, over a vertex whose property is 2, 5, then 1.
    // A chain that starts at a property takes the scalar compiler too.
    for text in [
        "MATCH (n) WHERE 1 < n.text < 3 RETURN n.text AS v",
        "MATCH (n) WHERE n.text >= n.text > 1 AND n.text <> 5 RETURN n.text AS v",
    ] {
        let query = prepare(text);
        for (input, rows) in [(2, 1), (5, 0), (1, 0)] {
            let result = execute(&query, &CanonicalScalar::Int(input), policy()).unwrap();
            assert_eq!(result.value.len(), rows, "{text}: {input}");
        }
    }
}

/// openCypher dynamic map access `map[key]` (TCK Map2): a text key reads its
/// entry case-sensitively, an absent key or a NULL key is NULL, and a key that
/// is not text is a typed failure at execution.
#[test]
fn a_map_reads_an_entry_by_a_dynamic_text_key() {
    for (expression, expected) in [
        ("{name: 'Mats', Name: 'Pontus'}['name']", text("Mats")),
        ("{name: 'Mats', Name: 'Pontus'}['Name']", text("Pontus")),
        (
            "{name: 'Mats', Name: 'Pontus'}['nAMe']",
            CanonicalScalar::Null,
        ),
        ("{null: 'Mats', NULL: 'Pontus'}['NULL']", text("Pontus")),
        ("{k: 1, x: 2}[n.text]", CanonicalScalar::Int(1)),
        ("{k: 1}[null]", CanonicalScalar::Null),
        ("{k: [1, 2]}['k'][1]", CanonicalScalar::Int(2)),
    ] {
        assert_eq!(scalar(expression, &text("k")), expected, "{expression}");
    }
    assert!(
        execute(
            &prepare("MATCH (n) RETURN {k: 1}[0] AS value"),
            &text("k"),
            policy()
        )
        .is_err()
    );
}

/// A list literal's `<`, `<=`, `>` or `>=` compares in openCypher's
/// comparability order: the first unequal pair of elements decides, a NULL or
/// incomparable pair makes the comparison UNKNOWN, and an equal prefix makes
/// the shorter list the lesser (TCK Comparison2 [4], Precedence3 [6]).
#[test]
fn list_literals_order_element_by_element_three_valued() {
    let truth = |value: bool| CanonicalScalar::Bool(value);
    let null = CanonicalScalar::Null;
    for (expression, expected) in [
        // Comparison2 [4].
        ("[1, 0] >= [1]", truth(true)),
        ("[1, null] >= [1]", truth(true)),
        ("[1, 2] >= [1, null]", null.clone()),
        ("[1, 'a'] >= [1, null]", null.clone()),
        ("[1, 2] >= [3, null]", truth(false)),
        // Each operator, equal lists and the empty list.
        ("[1, 2] < [1, 3]", truth(true)),
        ("[1, 2] <= [1, 2]", truth(true)),
        ("[1, 2] < [1, 2]", truth(false)),
        ("[2] > [1, 9]", truth(true)),
        ("[] < [null]", truth(true)),
        ("[] >= []", truth(true)),
        // Numbers compare across Int and Float; nested lists by the same rule.
        ("[1.5] > [1]", truth(true)),
        ("[[1, 2]] < [[1, 3]]", truth(true)),
        ("[[1]] < [[1, 0]]", truth(true)),
        // An incomparable pair, a non-list operand and NULL are UNKNOWN.
        ("['a'] < [1]", null.clone()),
        ("[1] < true", null.clone()),
        ("[1] < null", null.clone()),
        // `<>` stays the negated equality.
        ("[1, 2] <> [1]", truth(true)),
        // Precedence3 [6]: IN binds tighter than the comparison.
        ("[1, 2] < [3, 4] IN [[3, 4], false]", null.clone()),
        ("[1, 2] >= ([3, 4] IN [[3, 4], false])", null.clone()),
        ("([1, 2] > [3, 4]) IN [true, false]", truth(true)),
        // A lone operand IN a literal with a list member reads the list as
        // a value: members of another value domain are unequal, not refused.
        ("([1, 2] = [3, 4]) IN [[3, 4], false]", truth(true)),
        ("false IN [[3, 4], false]", truth(true)),
        ("2 IN [[1], 2]", truth(true)),
        ("1 IN [[1], {a: 1}]", truth(false)),
        ("null IN [[1]]", null.clone()),
        // One operand of the Boolean operators around it.
        ("[1] < [2] AND [3] > [2]", truth(true)),
        ("NOT [1] < [2]", truth(false)),
    ] {
        assert_eq!(scalar(expression, &text("x")), expected, "{expression}");
    }
}

/// fgdb-txbg9: date(...) builds a date from one calendar's fields (over an
/// optional `date` base), from ISO 8601 text, from a date or from NULL, and a
/// date reads its fields as `d.year`. Expected days are the TCK's (Temporal1
/// [1] and [4], Temporal2 [1], Temporal5 [1] and [2]).
#[test]
fn date_builds_from_fields_text_and_a_base_and_reads_its_fields() {
    let date = |year, month, day| {
        CanonicalScalar::Date(fgdb_types::CanonicalDate::from_civil(year, month, day).unwrap())
    };
    let x = text("x");
    for (expression, expected) in [
        ("date({year: 1984, month: 10, day: 11})", date(1984, 10, 11)),
        ("date({year: 1984, month: 10})", date(1984, 10, 1)),
        (
            "date({year: 1984, week: 10, dayOfWeek: 3})",
            date(1984, 3, 7),
        ),
        ("date({year: 1984, week: 10})", date(1984, 3, 5)),
        ("date({year: 1984})", date(1984, 1, 1)),
        ("date({year: 1984, ordinalDay: 202})", date(1984, 7, 20)),
        (
            "date({year: 1984, quarter: 3, dayOfQuarter: 45})",
            date(1984, 8, 14),
        ),
        ("date({year: 1984, quarter: 3})", date(1984, 7, 1)),
        ("date({year: 1816, week: 1})", date(1816, 1, 1)),
        ("date({year: 1818, week: 53})", date(1818, 12, 28)),
        (
            "date({dayOfWeek: 2, year: 1817, week: 1})",
            date(1816, 12, 31),
        ),
        (
            "date({date: date('1816-12-30'), week: 2, dayOfWeek: 3})",
            date(1817, 1, 8),
        ),
        (
            "date({date: date('1816-12-31'), week: 2})",
            date(1817, 1, 7),
        ),
        (
            "date({date: date('1816-12-31'), year: 1817, week: 2})",
            date(1817, 1, 7),
        ),
        (
            "date({date: date('1984-10-11'), month: 3})",
            date(1984, 3, 11),
        ),
        ("date('2015-W30-2')", date(2015, 7, 21)),
        ("date('2015202')", date(2015, 7, 21)),
        ("date('2015-07')", date(2015, 7, 1)),
        ("date(date('2015-07-21'))", date(2015, 7, 21)),
        ("date(null)", CanonicalScalar::Null),
        (
            "date({year: 1984, month: 10, day: 11}).year",
            CanonicalScalar::Int(1984),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).quarter",
            CanonicalScalar::Int(4),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).week",
            CanonicalScalar::Int(41),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).weekYear",
            CanonicalScalar::Int(1984),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).ordinalDay",
            CanonicalScalar::Int(285),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).weekDay",
            CanonicalScalar::Int(4),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).dayOfQuarter",
            CanonicalScalar::Int(11),
        ),
        (
            "date({year: 1984, month: 1, day: 1}).week",
            CanonicalScalar::Int(52),
        ),
        (
            "date({year: 1984, month: 1, day: 1}).weekYear",
            CanonicalScalar::Int(1983),
        ),
        (
            "date({year: 1984, month: 1, day: 1}).weekDay",
            CanonicalScalar::Int(7),
        ),
    ] {
        assert_eq!(scalar(expression, &x), expected, "{expression}");
    }
    // Text, map fields and accessors that name no day or field refuse with
    // their typed kind, never NULL or a guess.
    for (expression, kind) in [
        ("date({month: 10})", "TemporalConstruction"),
        ("date({year: 1984, month: 13})", "TemporalConstruction"),
        (
            "date({year: 1984, month: 2, day: 30})",
            "TemporalConstruction",
        ),
        (
            "date({year: 1984, week: 1, month: 2})",
            "TemporalConstruction",
        ),
        ("date({year: 1984, hour: 1})", "TemporalConstruction"),
        ("date({year: '1984'})", "TemporalConstruction"),
        ("date('2015-13-01')", "TemporalConstruction"),
        ("date(1984)", "TemporalConstruction"),
        ("date({year: 1984}).hour", "TemporalField"),
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        let error = execute(&query, &x, policy()).unwrap_err();
        assert!(
            format!("{error:?}").contains(kind),
            "{expression}: {error:?}"
        );
    }
}

/// fgdb-txbg9: a date is an operand of the scalar operators around it.
/// Dates compare chronologically, NULL propagates, toString gives the ISO
/// text, a field reads as an integer, and ORDER BY puts NULL last. A date
/// compared with a number is a typed error, never a guess.
#[test]
fn dates_compare_convert_and_order_as_values() {
    let x = text("x");
    let truth = CanonicalScalar::Bool;
    for (expression, expected) in [
        ("date('2015-07-21') < date('2015-07-22')", truth(true)),
        ("date('2015-07-21') > date('1984-10-11')", truth(true)),
        (
            "date('2015-07-21') = date({year: 2015, month: 7, day: 21})",
            truth(true),
        ),
        ("date('2015-07-21') <> date('2015-W30-2')", truth(false)),
        ("date(null) < date('2015-07-21')", CanonicalScalar::Null),
        (
            "toString(date({year: 1984, week: 10, dayOfWeek: 3}))",
            text("1984-03-07"),
        ),
        ("toString(date({year: 10000}))", text("+10000-01-01")),
        (
            "toString(date({year: -1, month: 7, day: 21}))",
            text("-0001-07-21"),
        ),
        (
            "date({year: 1984, month: 10, day: 11}).year + 1",
            CanonicalScalar::Int(1985),
        ),
        (
            "coalesce(null, date('2015-07-21'))",
            CanonicalScalar::Date(fgdb_types::CanonicalDate::from_civil(2015, 7, 21).unwrap()),
        ),
    ] {
        assert_eq!(scalar(expression, &x), expected, "{expression}");
    }
    let query = prepare("MATCH (n) RETURN date('2015-07-21') < 1 AS value");
    let error = execute(&query, &x, policy()).unwrap_err();
    assert!(
        format!("{error:?}").contains("IncompatibleOperands"),
        "{error:?}"
    );

    let ordered = execute(
        &prepare(
            "UNWIND [date('2015-07-21'), null, date('1984-10-11'), date('-0001-12-31')] AS d \
             RETURN d ORDER BY d",
        ),
        &x,
        policy(),
    )
    .unwrap();
    let rendered: Vec<String> = ordered
        .value
        .iter()
        .map(|row| match row.values()[0].as_scalar().unwrap() {
            CanonicalScalar::Date(day) => day.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        rendered,
        ["-0001-12-31", "1984-10-11", "2015-07-21", "Null"]
    );
}

/// fgdb-txbg9: localtime(...), time(...) and localdatetime(...) build from
/// fields (over `time`, `date` and `datetime` bases), from ISO 8601 text,
/// from another temporal value or from NULL. A time map's `timezone` moves a
/// base that has an offset to the same instant and puts a local one at it.
/// The values read their fields, compare (a time at an offset by its UTC
/// time of day first), convert to their java.time text and order. Expected
/// texts are the TCK's: Temporal1 [5]-[7] and [13], Temporal2 [2]-[4],
/// Temporal3 [1]-[7], Temporal5 [3]-[5] and Temporal7 [2]-[4].
#[test]
fn times_and_local_date_times_build_read_compare_and_order() {
    let x = text("x");
    let rendered = |expression: &str| match scalar(expression, &x) {
        CanonicalScalar::Date(value) => value.to_string(),
        CanonicalScalar::LocalTime(value) => value.to_string(),
        CanonicalScalar::Time(value) => value.to_string(),
        CanonicalScalar::LocalDateTime(value) => value.to_string(),
        other => format!("{other:?}"),
    };
    let local = "localtime({hour: 12, minute: 31, second: 14, nanosecond: 645876123})";
    let time = "time({hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: '+01:00'})";
    let local_dt = "localdatetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, \
                    second: 14, millisecond: 645})";
    let day = "date({year: 1984, month: 10, day: 11})";
    for (expression, expected) in [
        // Temporal1 [5], [6], [7] and [13]: fields.
        (
            "localtime({hour: 12, minute: 31, second: 14, nanosecond: 789, millisecond: 123, \
             microsecond: 456})"
                .to_owned(),
            "12:31:14.123456789",
        ),
        (
            "localtime({hour: 12, minute: 31, second: 14, microsecond: 645876})".to_owned(),
            "12:31:14.645876",
        ),
        (
            "localtime({hour: 12, minute: 31, second: 14, millisecond: 645})".to_owned(),
            "12:31:14.645",
        ),
        ("localtime({hour: 12, minute: 31})".to_owned(), "12:31"),
        ("localtime({hour: 12})".to_owned(), "12:00"),
        (
            "time({hour: 12, minute: 31, second: 14, nanosecond: 3})".to_owned(),
            "12:31:14.000000003Z",
        ),
        ("time({hour: 12})".to_owned(), "12:00Z"),
        (
            "time({hour: 12, minute: 31, second: 14, millisecond: 645, timezone: '+01:00'})"
                .to_owned(),
            "12:31:14.645+01:00",
        ),
        (
            "time({hour: 12, minute: 34, second: 56, timezone: '+02:05:59'})".to_owned(),
            "12:34:56+02:05:59",
        ),
        (
            "time({hour: 12, minute: 34, second: 56, timezone: '-02:05:07'})".to_owned(),
            "12:34:56-02:05:07",
        ),
        (
            "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, \
             nanosecond: 645876123})"
                .to_owned(),
            "1984-10-11T12:31:14.645876123",
        ),
        (
            "localdatetime({year: 1984, month: 10, day: 11})".to_owned(),
            "1984-10-11T00:00",
        ),
        (
            "localdatetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12})".to_owned(),
            "1984-03-07T12:00",
        ),
        (
            "localdatetime({year: 1984, ordinalDay: 202, hour: 12, minute: 31})".to_owned(),
            "1984-07-20T12:31",
        ),
        (
            "localdatetime({year: 1984, quarter: 3, dayOfQuarter: 45, hour: 12, minute: 31, \
             second: 14})"
                .to_owned(),
            "1984-08-14T12:31:14",
        ),
        ("localdatetime({year: 1984})".to_owned(), "1984-01-01T00:00"),
        // Temporal2 [2], [3] and [4]: text.
        ("localtime('214032.142')".to_owned(), "21:40:32.142"),
        ("localtime('21')".to_owned(), "21:00"),
        ("time('214032-0100')".to_owned(), "21:40:32-01:00"),
        ("time('2140-00:00')".to_owned(), "21:40Z"),
        ("time('22+18:00')".to_owned(), "22:00+18:00"),
        (
            "localdatetime('2015-W30-2T214032.142')".to_owned(),
            "2015-07-21T21:40:32.142",
        ),
        ("localdatetime('2015202T21')".to_owned(), "2015-07-21T21:00"),
        // Temporal3: another temporal value, and bases with overrides.
        (format!("localtime({local})"), "12:31:14.645876123"),
        (
            format!("localtime({{time: {local}, second: 42}})"),
            "12:31:42.645876123",
        ),
        (format!("localtime({time})"), "12:31:14.645876"),
        (
            format!("localtime({{time: {local_dt}, second: 42}})"),
            "12:31:42.645",
        ),
        (format!("time({local})"), "12:31:14.645876123Z"),
        (
            format!("time({{time: {local}, timezone: '+05:00'}})"),
            "12:31:14.645876123+05:00",
        ),
        (
            format!("time({{time: {time}, timezone: '+05:00'}})"),
            "16:31:14.645876+05:00",
        ),
        (
            format!("time({{time: {time}, second: 42, timezone: '+05:00'}})"),
            "16:31:42.645876+05:00",
        ),
        (
            format!("time({{time: {local_dt}, second: 42}})"),
            "12:31:42.645Z",
        ),
        (
            format!("localdatetime({{date: {day}, hour: 10, minute: 10, second: 10}})"),
            "1984-10-11T10:10:10",
        ),
        (
            format!(
                "localdatetime({{date: {local_dt}, day: 28, hour: 10, minute: 10, second: 10}})"
            ),
            "1984-03-28T10:10:10",
        ),
        (
            format!("localdatetime({{year: 1984, month: 10, day: 11, time: {time}, second: 42}})"),
            "1984-10-11T12:31:42.645876",
        ),
        (
            format!("localdatetime({{date: {day}, time: {local}}})"),
            "1984-10-11T12:31:14.645876123",
        ),
        (
            format!("localdatetime({{datetime: {local_dt}, day: 28, second: 42}})"),
            "1984-03-28T12:31:42.645",
        ),
        (
            format!("localdatetime({local_dt})"),
            "1984-03-07T12:31:14.645",
        ),
        (format!("date({local_dt})"), "1984-03-07"),
        (format!("date({{date: {local_dt}, day: 28}})"), "1984-03-28"),
        ("localtime(null)".to_owned(), "Null"),
        ("time(null)".to_owned(), "Null"),
        ("localdatetime(null)".to_owned(), "Null"),
    ] {
        assert_eq!(rendered(&expression), expected, "{expression}");
    }

    // Temporal5 [3]-[5] fields, and Temporal7 [2]-[4] comparisons: a time at
    // an offset compares by its UTC time of day first, and is equal to
    // another only with the same local time and offset.
    let int = CanonicalScalar::Int;
    let truth = CanonicalScalar::Bool;
    let november = "localdatetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, \
                    second: 14, nanosecond: 645876123})";
    for (expression, expected) in [
        (format!("{local}.hour"), int(12)),
        (format!("{local}.millisecond"), int(645)),
        (format!("{local}.microsecond"), int(645_876)),
        (format!("{local}.nanosecond"), int(645_876_123)),
        (format!("{time}.timezone"), text("+01:00")),
        (format!("{time}.offset"), text("+01:00")),
        (format!("{time}.offsetMinutes"), int(60)),
        (format!("{time}.offsetSeconds"), int(3_600)),
        (format!("{time}.minute + 1"), int(32)),
        (format!("{november}.quarter"), int(4)),
        (format!("{november}.week"), int(45)),
        (format!("{november}.ordinalDay"), int(316)),
        (format!("{november}.weekDay"), int(7)),
        (format!("{november}.dayOfQuarter"), int(42)),
        (format!("{november}.second"), int(14)),
        (
            "localtime({hour: 10, minute: 35}) < localtime({hour: 12, minute: 31})".to_owned(),
            truth(true),
        ),
        (
            "time({hour: 10, minute: 0, timezone: '+01:00'}) < time({hour: 9, minute: 35, \
             timezone: '+00:00'})"
                .to_owned(),
            truth(true),
        ),
        (
            "time('12:00+01:00') = time('11:00Z')".to_owned(),
            truth(false),
        ),
        (
            "time('21:40-00:00') = time('21:40Z')".to_owned(),
            truth(true),
        ),
        (
            "localdatetime('1980-12-11T12:31:14') < localdatetime('1984-10-11T12:31')".to_owned(),
            truth(true),
        ),
        (
            format!("{local} = localtime('12:31:14.645876123')"),
            truth(true),
        ),
        (format!("toString({time})"), text("12:31:14.645876+01:00")),
        (
            format!("toString({local_dt})"),
            text("1984-03-07T12:31:14.645"),
        ),
    ] {
        assert_eq!(scalar(&expression, &x), expected, "{expression}");
    }

    // A field outside the kind, a smaller unit without its larger one, a
    // fraction out of range, a named zone (no zone database in the value
    // language) and text of the wrong form refuse with their typed kind.
    for (expression, kind) in [
        ("localtime({minute: 31})", "TemporalConstruction"),
        ("localtime({hour: 12, second: 14})", "TemporalConstruction"),
        ("localtime({})", "TemporalConstruction"),
        ("localtime({hour: 24})", "TemporalConstruction"),
        (
            "localtime({hour: 1, minute: 1, second: 1, millisecond: 1, microsecond: 1000})",
            "TemporalConstruction",
        ),
        ("localtime({hour: 1, year: 1984})", "TemporalConstruction"),
        (
            "localtime({hour: 1, timezone: '+01:00'})",
            "TemporalConstruction",
        ),
        (
            "time({hour: 12, timezone: 'Europe/Stockholm'})",
            "TemporalConstruction",
        ),
        (
            "time({hour: 12, timezone: '+18:01'})",
            "TemporalConstruction",
        ),
        ("localdatetime({hour: 12})", "TemporalConstruction"),
        (
            "localdatetime({year: 1984, minute: 5})",
            "TemporalConstruction",
        ),
        ("localdatetime('2015-07-21 12:00')", "TemporalConstruction"),
        ("localdatetime(date('2015-07-21'))", "TemporalConstruction"),
        ("localtime(date('2015-07-21'))", "TemporalConstruction"),
        ("localtime({hour: 12}).year", "TemporalField"),
        ("localtime({hour: 12}).timezone", "TemporalField"),
        ("localdatetime({year: 1984}).offset", "TemporalField"),
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        let error = execute(&query, &x, policy()).unwrap_err();
        assert!(
            format!("{error:?}").contains(kind),
            "{expression}: {error:?}"
        );
    }

    let ordered = execute(
        &prepare(
            "UNWIND [time('12:00+01:00'), null, time('11:30Z'), time('01:00+05:00')] AS t \
             RETURN t ORDER BY t",
        ),
        &x,
        policy(),
    )
    .unwrap();
    let times: Vec<String> = ordered
        .value
        .iter()
        .map(|row| match row.values()[0].as_scalar().unwrap() {
            CanonicalScalar::Time(time) => time.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(times, ["01:00+05:00", "12:00+01:00", "11:30Z", "Null"]);
}

/// fgdb-txbg9: datetime(...) is a timestamp at a UTC offset, from fields and
/// a `timezone` offset (UTC without one), from ISO 8601 text, from
/// `epochSeconds` or `epochMillis`, or over bases. A date base never carries
/// an offset; a `time` base's offset is kept, or moved to a new `timezone`
/// at the same time of day; a `datetime` base moves to the same instant. A
/// named zone refuses: the value language has no zone database. Expected
/// texts are the TCK's (Temporal1 [9], [11] and [13], Temporal2 [5],
/// Temporal3 [8], [9] and [11], Temporal5 [6] at the equal offset `+01:00`,
/// Temporal7 [5]).
#[test]
fn datetimes_at_offsets_build_read_compare_and_convert() {
    let x = text("x");
    let as_text = |expression: &str| scalar(&format!("toString({expression})"), &x);
    let at_one = "datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: '+01:00'})";
    let time = "time({hour: 12, minute: 31, second: 14, microsecond: 645876, timezone: '+01:00'})";
    for (expression, expected) in [
        (
            "datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, \
             nanosecond: 645876123, timezone: '+01:00'})"
                .to_owned(),
            "1984-10-11T12:31:14.645876123+01:00",
        ),
        (
            "datetime({year: 1984, month: 10, day: 11, hour: 12})".to_owned(),
            "1984-10-11T12:00Z",
        ),
        (
            "datetime({year: 1984, week: 10, dayOfWeek: 3, hour: 12, minute: 31, timezone: \
             '-02:05:59'})"
                .to_owned(),
            "1984-03-07T12:31-02:05:59",
        ),
        (
            "datetime('2015-07-21T21:40:32.142+0100')".to_owned(),
            "2015-07-21T21:40:32.142+01:00",
        ),
        (
            "datetime('2015-W30-2T214032.142Z')".to_owned(),
            "2015-07-21T21:40:32.142Z",
        ),
        // A date alone is its midnight at UTC (TCK datetime('-999999999-01-01')).
        ("datetime('2015-07-21')".to_owned(), "2015-07-21T00:00Z"),
        (
            "datetime('2015T214032-0100')".to_owned(),
            "2015-01-01T21:40:32-01:00",
        ),
        (
            "datetime('20150721T21:40-01:30')".to_owned(),
            "2015-07-21T21:40-01:30",
        ),
        (
            "datetime('2015-W30T2140-02')".to_owned(),
            "2015-07-20T21:40-02:00",
        ),
        (
            "datetime('2015202T21+18:00')".to_owned(),
            "2015-07-21T21:00+18:00",
        ),
        (
            "datetime({epochSeconds: 416779, nanosecond: 999999999})".to_owned(),
            "1970-01-05T19:46:19.999999999Z",
        ),
        (
            "datetime({epochMillis: 237821673987})".to_owned(),
            "1977-07-15T13:34:33.987Z",
        ),
        (
            "datetime({date: date({year: 1984, month: 10, day: 11}), hour: 10, minute: 10, \
             second: 10, timezone: '+05:00'})"
                .to_owned(),
            "1984-10-11T10:10:10+05:00",
        ),
        (
            format!("datetime({{date: {at_one}, day: 28, hour: 10, minute: 10, second: 10}})"),
            "1984-10-28T10:10:10Z",
        ),
        (
            format!("datetime({{year: 1984, month: 10, day: 11, time: {time}}})"),
            "1984-10-11T12:31:14.645876+01:00",
        ),
        (
            format!(
                "datetime({{year: 1984, month: 10, day: 11, time: {time}, second: 42, \
                 timezone: '+05:00'}})"
            ),
            "1984-10-11T16:31:42.645876+05:00",
        ),
        (
            format!("datetime({{datetime: {at_one}, timezone: '+05:00'}})"),
            "1984-10-11T16:00+05:00",
        ),
        (
            format!("datetime({{datetime: {at_one}, day: 28, second: 42}})"),
            "1984-10-28T12:00:42+01:00",
        ),
        (
            "datetime(localdatetime('2015-07-21T21:40'))".to_owned(),
            "2015-07-21T21:40Z",
        ),
        (format!("date({at_one})"), "1984-10-11"),
        (format!("localtime({at_one})"), "12:00"),
        (format!("time({at_one})"), "12:00+01:00"),
        (format!("localdatetime({at_one})"), "1984-10-11T12:00"),
    ] {
        assert_eq!(as_text(&expression), text(expected), "{expression}");
    }
    assert_eq!(scalar("datetime(null)", &x), CanonicalScalar::Null);

    let int = CanonicalScalar::Int;
    let november = "datetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, second: 14, \
                    nanosecond: 645876123, timezone: '+01:00'})";
    for (field, expected) in [
        ("year", int(1984)),
        ("quarter", int(4)),
        ("week", int(45)),
        ("weekDay", int(7)),
        ("dayOfQuarter", int(42)),
        ("hour", int(12)),
        ("millisecond", int(645)),
        ("nanosecond", int(645_876_123)),
        ("timezone", text("+01:00")),
        ("offset", text("+01:00")),
        ("offsetMinutes", int(60)),
        ("offsetSeconds", int(3_600)),
        ("epochSeconds", int(469_020_674)),
        ("epochMillis", int(469_020_674_645)),
    ] {
        assert_eq!(
            scalar(&format!("{november}.{field}"), &x),
            expected,
            "{field}"
        );
    }
    for (expression, expected) in [
        (
            "datetime({year: 1980, month: 12, day: 11, hour: 12, minute: 31, second: 14, \
             timezone: '+00:00'}) < datetime({year: 1984, month: 10, day: 11, hour: 12, \
             minute: 31, second: 14, timezone: '+05:00'})",
            true,
        ),
        (
            "datetime('2015-07-21T21:40+01:00') < datetime('2015-07-21T21:00Z')",
            true,
        ),
        (
            "datetime('2015-07-21T21:40Z') = datetime({year: 2015, month: 7, day: 21, hour: 21, \
             minute: 40})",
            true,
        ),
    ] {
        assert_eq!(
            scalar(expression, &x),
            CanonicalScalar::Bool(expected),
            "{expression}"
        );
    }
    for expression in [
        "datetime({year: 1984, timezone: 'Europe/Stockholm'})",
        "datetime('2015-07-21T21:40:32.142[Europe/London]')",
        "datetime('2015-07-21T21:40:32.142+0100[Europe/London]')",
        "datetime({epochSeconds: 1, epochMillis: 1})",
        "datetime({epochSeconds: 1, hour: 1})",
        "datetime(date('2015-07-21'))",
        "datetime({year: 1984}).dayOfYear",
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        let error = execute(&query, &x, policy()).unwrap_err();
        assert!(
            format!("{error:?}").contains("Temporal"),
            "{expression}: {error:?}"
        );
    }
}

/// fgdb-txbg9: the clock reads `date.transaction(x)`, `.statement(x)` and
/// `.realtime(x)` of every temporal kind propagate NULL (TCK Temporal4 [13]).
/// A query context grants no clock capability, so any other argument fails
/// typed instead of reading a host clock that would break replay.
#[test]
fn clock_reads_propagate_null_and_refuse_a_real_read() {
    let x = text("x");
    for kind in ["date", "localtime", "time", "localdatetime", "datetime"] {
        for clock in ["transaction", "statement", "realtime"] {
            let call = format!("{kind}.{clock}(null)");
            assert_eq!(scalar(&call, &x), CanonicalScalar::Null, "{call}");
            let query = prepare(&format!("MATCH (n) RETURN {kind}.{clock}('UTC') AS value"));
            assert!(
                execute(&query, &x, policy()).is_err(),
                "{kind}.{clock}('UTC')"
            );
        }
    }
}

/// fgdb-txbg9: date.truncate, localtime.truncate, time.truncate,
/// localdatetime.truncate and datetime.truncate cut a value down to the start
/// of a unit, then set a map's fields over it; datetime.fromepoch and
/// datetime.fromepochmillis build a UTC datetime. A truncation map's
/// `timezone` sets the offset without moving the clock, and a sub-millisecond
/// field after a millisecond or microsecond truncation keeps the truncated
/// digits. Expected texts are the TCK's (Temporal9 [1]-[5], Temporal1 [11]).
#[test]
fn truncation_and_epoch_calls_follow_the_tck() {
    let x = text("x");
    let as_text = |expression: &str| scalar(&format!("toString({expression})"), &x);
    let local = "localtime({hour: 12, minute: 31, second: 14, nanosecond: 645876123})";
    let local_dt = "localdatetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, \
                    second: 14, nanosecond: 645876123})";
    let at_minus = "datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, \
                    nanosecond: 645876123, timezone: '-01:00'})";
    let at_plus = "datetime({year: 1984, month: 10, day: 11, hour: 12, minute: 31, second: 14, \
                   nanosecond: 645876123, timezone: '+01:00'})";
    for (expression, expected) in [
        (
            "date.truncate('millennium', date({year: 2017, month: 10, day: 11}), {day: 2})"
                .to_owned(),
            "2000-01-02",
        ),
        (
            "date.truncate('century', date({year: 1984, month: 10, day: 11}))".to_owned(),
            "1900-01-01",
        ),
        (
            "date.truncate('decade', date({year: 1984, month: 10, day: 11}), {})".to_owned(),
            "1980-01-01",
        ),
        (
            "date.truncate('weekYear', date({year: 1984, month: 2, day: 1}), {day: 5})".to_owned(),
            "1984-01-05",
        ),
        (
            "date.truncate('weekYear', localdatetime({year: 1984, month: 1, day: 1, hour: 12}))"
                .to_owned(),
            "1983-01-03",
        ),
        (
            "date.truncate('quarter', date({year: 1984, month: 11, day: 11}), {day: 2})".to_owned(),
            "1984-10-02",
        ),
        (
            "DATE.TRUNCATE('week', date({year: 1984, month: 10, day: 11}), {dayOfWeek: 2})"
                .to_owned(),
            "1984-10-09",
        ),
        (format!("date.truncate('day', {at_plus})"), "1984-10-11"),
        (
            format!("localtime.truncate('millisecond', {local}, {{nanosecond: 2}})"),
            "12:31:14.645000002",
        ),
        (
            format!("localtime.truncate('microsecond', {local}, {{nanosecond: 2}})"),
            "12:31:14.645876002",
        ),
        (
            format!("localtime.truncate('second', {local}, {{nanosecond: 2}})"),
            "12:31:14.000000002",
        ),
        (
            format!("localtime.truncate('day', {local_dt}, {{nanosecond: 2}})"),
            "00:00:00.000000002",
        ),
        (
            format!("time.truncate('hour', {at_minus}, {{timezone: '+01:00'}})"),
            "12:00+01:00",
        ),
        (format!("time.truncate('hour', {at_minus})"), "12:00-01:00"),
        (format!("time.truncate('minute', {local})"), "12:31Z"),
        (
            "localdatetime.truncate('millennium', date({year: 2017, month: 10, day: 11}), \
             {day: 2})"
                .to_owned(),
            "2000-01-02T00:00",
        ),
        (
            format!("localdatetime.truncate('second', {local_dt}, {{nanosecond: 2}})"),
            "1984-10-11T12:31:14.000000002",
        ),
        (
            format!("localdatetime.truncate('weekYear', {at_plus})"),
            "1984-01-02T00:00",
        ),
        (
            format!("datetime.truncate('day', {at_plus}, {{nanosecond: 2}})"),
            "1984-10-11T00:00:00.000000002+01:00",
        ),
        (
            "datetime.truncate('month', date({year: 1984, month: 10, day: 11}), {day: 2})"
                .to_owned(),
            "1984-10-02T00:00Z",
        ),
        (
            format!("datetime.truncate('hour', {at_minus})"),
            "1984-10-11T12:00-01:00",
        ),
        (
            format!("datetime.truncate('hour', {local_dt}, {{timezone: '+01:00'}})"),
            "1984-10-11T12:00+01:00",
        ),
        (
            format!("datetime.truncate('millisecond', {at_plus})"),
            "1984-10-11T12:31:14.645+01:00",
        ),
        (
            "datetime.fromepoch(416779, 999999999)".to_owned(),
            "1970-01-05T19:46:19.999999999Z",
        ),
        (
            "datetime.fromepochmillis(237821673987)".to_owned(),
            "1977-07-15T13:34:33.987Z",
        ),
    ] {
        assert_eq!(as_text(&expression), text(expected), "{expression}");
    }
    for expression in [
        "date.truncate(null, date('2015-07-21'))",
        "date.truncate('day', null)",
        "datetime.fromepoch(null, 1)",
    ] {
        assert_eq!(
            scalar(expression, &x),
            CanonicalScalar::Null,
            "{expression}"
        );
    }
    // A dotted call is an operand like any temporal value: compared, and
    // read through a field.
    assert_eq!(
        scalar(
            "date.truncate('month', date('2015-07-21')) = date('2015-07-01')",
            &x
        ),
        CanonicalScalar::Bool(true)
    );
    assert_eq!(
        scalar("date.truncate('month', date('2015-07-21')).day + 1", &x),
        CanonicalScalar::Int(2)
    );
    // A unit the kind does not have, an unknown unit, a base in the map, a
    // named zone and a nanosecond out of range refuse when evaluated; a
    // wrong argument count refuses when prepared.
    for expression in [
        "date.truncate('hour', date('2015-07-21'))",
        "localtime.truncate('year', localtime('12:00'))",
        "date.truncate('fortnight', date('2015-07-21'))",
        "date.truncate('day', date('2015-07-21'), {date: date('2015-07-22')})",
        "datetime.truncate('day', date('2015-07-21'), {timezone: 'Europe/Stockholm'})",
        "date.truncate('day', localtime('12:00'))",
        "datetime.fromepoch(1, 1000000000)",
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        let error = execute(&query, &x, policy()).unwrap_err();
        assert!(
            format!("{error:?}").contains("TemporalConstruction"),
            "{expression}: {error:?}"
        );
    }
    for statement in [
        "MATCH (n) RETURN date.truncate('day') AS value",
        "MATCH (n) RETURN date.truncate('day', date('2015-07-21'), {}, {}) AS value",
        "MATCH (n) RETURN datetime.fromepochmillis(1, 2) AS value",
    ] {
        assert!(
            PreparedGraphSetText::prepare(statement, symbols).is_err(),
            "{statement}"
        );
    }
}

/// fgdb-txbg9: duration(...) builds months, days and a time part from a map
/// of counts (fractions cascading down, a month as the average month) or ISO
/// 8601 text, prints openCypher's text with a sign per component, reads its
/// fields, and compares only for equality: P1M against P30D has no order,
/// so `<` is NULL. Expected texts are the TCK's (Temporal1 [12], Temporal2
/// [7], Temporal5 [7], Temporal6 [6], Temporal7 [6]).
#[test]
fn durations_build_print_read_and_compare_only_for_equality() {
    let x = text("x");
    let as_text = |expression: &str| scalar(&format!("toString({expression})"), &x);
    for (expression, expected) in [
        (
            "duration({days: 14, hours: 16, minutes: 12})",
            "P14DT16H12M",
        ),
        ("duration({months: 5, days: 1.5})", "P5M1DT12H"),
        ("duration({months: 0.75})", "P22DT19H51M49.5S"),
        ("duration({weeks: 2.5})", "P17DT12H"),
        (
            "duration({years: 12, months: 5, days: 14, hours: 16, minutes: 12, seconds: 70})",
            "P12Y5M14DT16H13M10S",
        ),
        (
            "duration({days: 14, seconds: 70, milliseconds: 1})",
            "P14DT1M10.001S",
        ),
        (
            "duration({days: 14, seconds: 70, nanoseconds: 1})",
            "P14DT1M10.000000001S",
        ),
        ("duration({minutes: 1.5, seconds: 1})", "PT1M31S"),
        ("duration({quarters: 1})", "P3M"),
        ("duration('P5M1.5D')", "P5M1DT12H"),
        ("duration('PT0.75M')", "PT45S"),
        (
            "duration('P2012-02-02T14:37:21.545')",
            "P2012Y2M2DT14H37M21.545S",
        ),
        (
            "duration({years: 12, months: 5, days: -14, hours: 16})",
            "P12Y5M-14DT16H",
        ),
        ("duration({minutes: 12, seconds: -60})", "PT11M"),
        ("duration({seconds: -2, milliseconds: 1})", "PT-1.999S"),
        ("duration({seconds: -60, milliseconds: -1})", "PT-1M-0.001S"),
        ("duration({})", "PT0S"),
        ("duration(duration('P1D'))", "P1D"),
    ] {
        assert_eq!(as_text(expression), text(expected), "{expression}");
    }
    assert_eq!(scalar("duration(null)", &x), CanonicalScalar::Null);

    // Temporal5 [7]: every field of 1 year 4 months 10 days 1:01:01.111111111.
    let int = CanonicalScalar::Int;
    let sample = "duration({years: 1, months: 4, days: 10, hours: 1, minutes: 1, seconds: 1, \
                  nanoseconds: 111111111})";
    for (field, expected) in [
        ("years", 1),
        ("quarters", 5),
        ("months", 16),
        ("weeks", 1),
        ("days", 10),
        ("hours", 1),
        ("minutes", 61),
        ("seconds", 3_661),
        ("milliseconds", 3_661_111),
        ("microseconds", 3_661_111_111),
        ("nanoseconds", 3_661_111_111_111),
        ("quartersOfYear", 1),
        ("monthsOfQuarter", 1),
        ("monthsOfYear", 4),
        ("daysOfWeek", 3),
        ("minutesOfHour", 1),
        ("secondsOfMinute", 1),
        ("millisecondsOfSecond", 111),
        ("microsecondsOfSecond", 111_111),
        ("nanosecondsOfSecond", 111_111_111),
    ] {
        assert_eq!(
            scalar(&format!("{sample}.{field}"), &x),
            int(expected),
            "{field}"
        );
    }

    // Temporal6 [6] round trip, Temporal7 [6] equality, and no order.
    let truth = |value: bool| CanonicalScalar::Bool(value);
    let x12 = "duration({years: 12, months: 5, days: 14, hours: 16, minutes: 12, seconds: 70})";
    for (expression, expected) in [
        (format!("duration(toString({x12})) = {x12}"), truth(true)),
        (
            format!(
                "{x12} = duration({{years: 12, months: 5, days: 14, hours: 16, minutes: 13, \
                 seconds: 10}})"
            ),
            truth(true),
        ),
        (
            format!(
                "{x12} = duration({{years: 12, months: 5, days: 13, hours: 40, minutes: 13, \
                 seconds: 10}})"
            ),
            truth(false),
        ),
        (
            "duration('P1M') <> duration('P30D')".to_owned(),
            truth(true),
        ),
        (
            "duration('P1D') = duration('PT24H')".to_owned(),
            truth(false),
        ),
        (
            "duration('P1M') < duration('P30D')".to_owned(),
            CanonicalScalar::Null,
        ),
        (
            "duration('PT1S') >= duration('PT1S')".to_owned(),
            CanonicalScalar::Null,
        ),
    ] {
        assert_eq!(scalar(&expression, &x), expected, "{expression}");
    }

    for expression in [
        "duration({fortnights: 1})",
        "duration({days: '1'})",
        "duration('P')",
        "duration('P1D1M')",
        "duration(1)",
        "duration(date('2015-07-21'))",
        "duration('P1D').year",
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        let error = execute(&query, &x, policy()).unwrap_err();
        assert!(
            format!("{error:?}").contains("Temporal"),
            "{expression}: {error:?}"
        );
    }
}

/// fgdb-txbg9: duration.between and its inMonths, inDays and inSeconds
/// projections. With both offsets the right side is read at the left's;
/// with both dates the months, days and time left share one sign; with a
/// side lacking a date only the times of day differ. Expected texts are the
/// TCK's (Temporal10 [1]-[7] and [9]-[11]).
#[test]
fn duration_between_follows_the_tck() {
    let x = text("x");
    let as_text = |expression: &str| scalar(&format!("toString({expression})"), &x);
    let zoned = "datetime('2014-07-21T21:40:36.143+0200')";
    for (function, lhs, rhs, expected) in [
        (
            "between",
            "date('1984-10-11')",
            "date('2015-06-24')",
            "P30Y8M13D",
        ),
        (
            "between",
            "date('1984-10-11')",
            "localdatetime('2016-07-21T21:45:22.142')",
            "P31Y9M10DT21H45M22.142S",
        ),
        (
            "between",
            "date('1984-10-11')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            "P30Y9M10DT21H40M32.142S",
        ),
        (
            "between",
            "date('1984-10-11')",
            "localtime('16:30')",
            "PT16H30M",
        ),
        (
            "between",
            "localtime('14:30')",
            "date('2015-06-24')",
            "PT-14H-30M",
        ),
        (
            "between",
            "time('14:30')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            "PT6H10M32.142S",
        ),
        ("between", "time('14:30')", "time('16:30+0100')", "PT1H"),
        (
            "between",
            "localtime('14:30')",
            "time('16:30+0100')",
            "PT2H",
        ),
        (
            "between",
            "localdatetime('2015-07-21T21:40:32.142')",
            "date('2015-06-24')",
            "P-27DT-21H-40M-32.142S",
        ),
        (
            "between",
            "localdatetime('2015-07-21T21:40:32.142')",
            "datetime('2015-07-21T21:40:32.142+0100')",
            "PT0S",
        ),
        (
            "between",
            zoned,
            "date('2015-06-24')",
            "P11M2DT2H19M23.857S",
        ),
        (
            "between",
            zoned,
            "datetime('2015-07-21T21:40:32.142+0100')",
            "P1YT59M55.999S",
        ),
        ("between", zoned, "time('16:30+0100')", "PT-4H-10M-36.143S"),
        (
            "between",
            "localdatetime('2018-01-02T10:00:00.1')",
            "localdatetime('2018-01-01T10:00:00.2')",
            "PT-23H-59M-59.9S",
        ),
        (
            "between",
            "date('-999999999-01-01')",
            "date('+999999999-12-31')",
            "P1999999998Y11M30D",
        ),
        (
            "inMonths",
            "date('1984-10-11')",
            "date('2015-06-24')",
            "P30Y8M",
        ),
        (
            "inMonths",
            "date('1984-10-11')",
            "localtime('16:30')",
            "PT0S",
        ),
        (
            "inMonths",
            "date('2018-03-11')",
            "date('2016-06-24')",
            "P-1Y-8M",
        ),
        (
            "inMonths",
            "date('2018-07-21')",
            "datetime('2016-07-21T21:40:32.142+0100')",
            "P-1Y-11M",
        ),
        (
            "inMonths",
            "datetime('2018-07-21T21:40:36.143+0500')",
            "datetime('1984-07-21T22:40:36.143+0200')",
            "P-33Y-11M",
        ),
        (
            "inDays",
            "date('1984-10-11')",
            "date('2015-06-24')",
            "P11213D",
        ),
        (
            "inDays",
            "date('1984-10-11')",
            "localdatetime('2016-07-21T21:45:22.142')",
            "P11606D",
        ),
        ("inDays", "localtime('14:30')", "date('2015-06-24')", "PT0S"),
        (
            "inSeconds",
            "date('1984-10-11')",
            "date('2015-06-24')",
            "PT269112H",
        ),
        (
            "inSeconds",
            zoned,
            "datetime('2015-07-21T21:40:32.142+0100')",
            "PT8760H59M55.999S",
        ),
        (
            "inSeconds",
            "localdatetime('2014-07-21T21:40:36.143')",
            "localdatetime('2014-07-21T21:40:36.142')",
            "PT-0.001S",
        ),
        (
            "inSeconds",
            "localdatetime('-999999999-01-01')",
            "localdatetime('+999999999-12-31T23:59:59')",
            "PT17531639991215H59M59S",
        ),
        (
            "inSeconds",
            "localtime('12:44:54.7')",
            "localtime('12:34:55.3')",
            "PT-9M-59.4S",
        ),
        (
            "inSeconds",
            "localtime('12:34:56.3')",
            "localtime('12:34:54.7')",
            "PT-1.6S",
        ),
    ] {
        let expression = format!("duration.{function}({lhs}, {rhs})");
        assert_eq!(as_text(&expression), text(expected), "{expression}");
    }
    // Temporal10 [1]: a negative time part splits into floored seconds and
    // a positive fraction.
    let reverse = "duration.between(localdatetime('2018-01-02T10:00:00.1'), \
                   localdatetime('2018-01-01T10:00:00.2'))";
    for (field, expected) in [
        ("days", 0),
        ("seconds", -86_400),
        ("nanosecondsOfSecond", 100_000_000),
    ] {
        assert_eq!(
            scalar(&format!("{reverse}.{field}"), &x),
            CanonicalScalar::Int(expected),
            "{field}"
        );
    }
    for function in ["between", "inMonths", "inDays", "inSeconds"] {
        assert_eq!(
            scalar(&format!("duration.{function}(null, null)"), &x),
            CanonicalScalar::Null,
            "{function}"
        );
    }
    let query = prepare("MATCH (n) RETURN duration.between(date('2015-07-21'), 1) AS value");
    let error = execute(&query, &x, policy()).unwrap_err();
    assert!(
        format!("{error:?}").contains("TemporalConstruction"),
        "{error:?}"
    );
}

/// fgdb-txbg9: temporal arithmetic as TCK Temporal8 [1]-[7] states it. A
/// value plus or minus a duration moves by months (the day clamped), then
/// days, then the time part: a date keeps only whole days of the time part,
/// a time of day only the time part, and an offset stays. Durations add and
/// subtract component by component, and a duration times or divided by a
/// number cascades what each whole count leaves down, truncated.
#[test]
fn temporal_arithmetic_follows_the_tck() {
    let x = text("x");
    let as_text = |expression: &str| scalar(&format!("toString({expression})"), &x);
    let durations = [
        "duration({years: 12, months: 5, days: 14, hours: 16, minutes: 12, seconds: 70, nanoseconds: 2})",
        "duration({months: 1, days: -14, hours: 16, minutes: -12, seconds: 70})",
        "duration({years: 12.5, months: 5.5, days: 14.5, hours: 16.5, minutes: 12.5, seconds: 70.5, \
         nanoseconds: 3})",
    ];
    let local_time = [
        ("04:44:24.000000003", "20:18:03.999999999"),
        ("04:20:24.000000001", "20:42:04.000000001"),
        ("22:29:27.500000004", "02:33:00.499999998"),
    ];
    let local_datetime = [
        (
            "1997-03-26T04:44:24.000000003",
            "1972-04-26T20:18:03.999999999",
        ),
        (
            "1984-10-29T04:20:24.000000001",
            "1984-09-24T20:42:04.000000001",
        ),
        (
            "1997-10-11T22:29:27.500000004",
            "1971-10-12T02:33:00.499999998",
        ),
    ];
    let clock = "hour: 12, minute: 31, second: 14, nanosecond: 1";
    let day = "year: 1984, month: 10, day: 11";
    for (at, duration) in durations.iter().enumerate() {
        let (date_sum, date_difference) = [
            ("1997-03-25", "1972-04-27"),
            ("1984-10-28", "1984-09-25"),
            ("1997-10-11", "1971-10-12"),
        ][at];
        let (time_sum, time_difference) = local_time[at];
        let (moment_sum, moment_difference) = local_datetime[at];
        for (base, sum, difference) in [
            (
                format!("date({{{day}}})"),
                date_sum.to_owned(),
                date_difference.to_owned(),
            ),
            (
                format!("localtime({{{clock}}})"),
                time_sum.to_owned(),
                time_difference.to_owned(),
            ),
            (
                format!("time({{{clock}, timezone: '+01:00'}})"),
                format!("{time_sum}+01:00"),
                format!("{time_difference}+01:00"),
            ),
            (
                format!("localdatetime({{{day}, {clock}}})"),
                moment_sum.to_owned(),
                moment_difference.to_owned(),
            ),
            (
                format!("datetime({{{day}, {clock}, timezone: '+01:00'}})"),
                format!("{moment_sum}+01:00"),
                format!("{moment_difference}+01:00"),
            ),
        ] {
            for (operator, expected) in [("+", &sum), ("-", &difference)] {
                let expression = format!("{base} {operator} {duration}");
                assert_eq!(as_text(&expression), text(expected), "{expression}");
            }
        }
        // A duration on the left of + adds the same way.
        let expression = format!("{duration} + date({{{day}}})");
        assert_eq!(as_text(&expression), text(date_sum), "{expression}");
    }
    let one = "duration({years: 12, months: 5, days: 14, hours: 16, minutes: 12, seconds: 70, \
               nanoseconds: 1})";
    for (other, sum, difference) in [
        (one, "P24Y10M28DT32H26M20.000000002S", "PT0S"),
        (
            durations[1],
            "P12Y6MT32H2M20.000000001S",
            "P12Y4M28DT24M0.000000001S",
        ),
        (
            durations[2],
            "P25Y4M43DT50H11M23.500000004S",
            "P-6M-15DT-17H-45M-3.500000002S",
        ),
    ] {
        assert_eq!(as_text(&format!("{one} + {other}")), text(sum), "{other}");
        assert_eq!(
            as_text(&format!("{one} - {other}")),
            text(difference),
            "{other}"
        );
    }
    for (number, product, quotient) in [
        (
            "1",
            "P12Y5M14DT16H13M10.000000001S",
            "P12Y5M14DT16H13M10.000000001S",
        ),
        ("2", "P24Y10M28DT32H26M20.000000002S", "P6Y2M22DT13H21M8S"),
        ("0.5", "P6Y2M22DT13H21M8S", "P24Y10M28DT32H26M20.000000002S"),
    ] {
        assert_eq!(
            as_text(&format!("{one} * {number}")),
            text(product),
            "{number}"
        );
        assert_eq!(
            as_text(&format!("{one} / {number}")),
            text(quotient),
            "{number}"
        );
    }
    assert_eq!(scalar(&format!("{one} + null"), &x), CanonicalScalar::Null);
    for (expression, kind) in [
        ("date('2015-07-21') + 1", "IncompatibleOperands"),
        (
            "date('2015-07-21') + date('2015-07-22')",
            "IncompatibleOperands",
        ),
        ("1 - duration('P1D')", "IncompatibleOperands"),
        ("duration('P1D') / 0", "DivisionByZero"),
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        let error = execute(&query, &x, policy()).unwrap_err();
        assert!(
            format!("{error:?}").contains(kind),
            "{expression}: {error:?}"
        );
    }
}

/// openCypher reverse() and split() (fgdb-13l85; TCK String3 [1], String4
/// [1]): reverse reverses a list's elements or a text's Unicode scalars;
/// split cuts a text at each delimiter, or at every scalar for an empty one.
/// NULL in either argument is NULL. Inside a scalar expression each is an
/// operand read through a let slot.
#[test]
fn reverse_and_split_over_lists_and_text() {
    use fgdb_gql::algebra::GraphValue;

    let value = |expression: &str| {
        execute(
            &prepare(&format!("MATCH (n) RETURN {expression} AS value")),
            &text("ab"),
            policy(),
        )
        .unwrap()
        .value[0]
            .values()[0]
            .clone()
    };
    let word = |value: &str| GraphValue::Scalar(text(value));
    let words =
        |values: &[&str]| GraphValue::List(values.iter().map(|value| word(value)).collect());
    let int = |value: i64| GraphValue::Scalar(CanonicalScalar::Int(value));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    for (expression, expected) in [
        ("reverse('raksO')", word("Oskar")),
        ("reverse('é🦀a')", word("a🦀é")),
        ("reverse(n.text)", word("ba")),
        (
            "reverse([1, 'x', null])",
            GraphValue::List(vec![null.clone(), word("x"), int(1)].into()),
        ),
        ("reverse([])", GraphValue::List(Vec::new().into())),
        ("reverse(null)", null.clone()),
        ("split('one1two', '1')", words(&["one", "two"])),
        ("split('a,b,,c', ',')", words(&["a", "b", "", "c"])),
        ("split('abc', '')", words(&["a", "b", "c"])),
        ("split(null, ',')", null.clone()),
        ("split('a', null)", null.clone()),
        ("size(split('one1two', '1'))", int(2)),
        ("size(reverse([1, 2])) + 1", int(3)),
        (
            "reverse(n.text) = 'ba'",
            GraphValue::Scalar(CanonicalScalar::Bool(true)),
        ),
    ] {
        assert_eq!(value(expression), expected, "{expression}");
    }
    for expression in ["reverse(1)", "split(1, ',')", "split('a', 1)"] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        assert!(
            execute(&query, &text("ab"), policy()).is_err(),
            "{expression}"
        );
    }
}

/// openCypher list concatenation (TCK List4, Precedence3 [1] to [3];
/// fgdb-c4i0d). Two lists concatenate, a list and another value append or
/// prepend it, and NULL in either operand is NULL. Element access and `*`
/// bind tighter than `+`, the chain is left-associative, and two scalars in
/// it keep the scalar rules: numbers add, texts concatenate.
#[test]
fn plus_concatenates_lists_and_appends_or_prepends_values() {
    use fgdb_gql::algebra::GraphValue;

    let value = |expression: &str| {
        execute(
            &prepare(&format!("MATCH (n) RETURN {expression} AS value")),
            &text("ab"),
            policy(),
        )
        .unwrap_or_else(|error| panic!("{expression}: {error:?}"))
        .value[0]
            .values()[0]
            .clone()
    };
    let int = |value: i64| GraphValue::Scalar(CanonicalScalar::Int(value));
    let word = |value: &str| GraphValue::Scalar(text(value));
    let truth = |value: bool| GraphValue::Scalar(CanonicalScalar::Bool(value));
    let list = |values: Vec<GraphValue>| GraphValue::List(values.into());
    let ints = |values: &[i64]| list(values.iter().map(|value| int(*value)).collect());
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    for (expression, expected) in [
        ("[1, 10, 100] + [4, 5]", ints(&[1, 10, 100, 4, 5])),
        (
            "[false, true] + false",
            list(vec![truth(false), truth(true), truth(false)]),
        ),
        ("0 + [1, 2]", ints(&[0, 1, 2])),
        ("[] + []", list(Vec::new())),
        ("[1] + [null]", list(vec![int(1), null.clone()])),
        ("[1] + null", null.clone()),
        ("null + [1]", null.clone()),
        ("[1] + 2 * 3", ints(&[1, 6])),
        ("1 + 2 + [3]", ints(&[3, 3])),
        ("[1] + 2 + 3", ints(&[1, 2, 3])),
        ("n.text + 'c' + [1]", list(vec![word("abc"), int(1)])),
        (
            "[[1], [2, 3]] + [5, [6, 7], 10][1]",
            list(vec![ints(&[1]), ints(&[2, 3]), int(6), int(7)]),
        ),
        (
            "[[1], [2, 3]] + ([5, [6, 7], 10][1])",
            list(vec![ints(&[1]), ints(&[2, 3]), int(6), int(7)]),
        ),
        (
            "[[1], [2, 3]] + [5, [6, 7], 10][1..3]",
            list(vec![ints(&[1]), ints(&[2, 3]), ints(&[6, 7]), int(10)]),
        ),
        ("reverse([1, 2]) + 3", ints(&[2, 1, 3])),
        ("[1] + reverse([2, 3])", ints(&[1, 3, 2])),
        ("size([1] + [2, 3])", int(3)),
        ("size(reverse([1, 2])) + 1", int(3)),
    ] {
        assert_eq!(value(expression), expected, "{expression}");
    }
    // A searched CASE with a list result is a value (fgdb-13l85): the first
    // TRUE condition selects, FALSE and NULL move on, no ELSE is NULL, and
    // an unselected result never executes.
    for (expression, expected) in [
        (
            "CASE WHEN n.text = 'ab' THEN [1, 2] ELSE [3] END",
            ints(&[1, 2]),
        ),
        (
            "CASE WHEN n.text = 'x' THEN [1, 2] ELSE [3] END",
            ints(&[3]),
        ),
        ("CASE WHEN n.text = 'x' THEN [1] END", null.clone()),
        (
            "CASE WHEN null THEN [1] WHEN true THEN reverse([1, 2]) END",
            ints(&[2, 1]),
        ),
        (
            "CASE WHEN true THEN [1] ELSE [1 / (size(n.text) - 2)] END",
            ints(&[1]),
        ),
        ("CASE WHEN true THEN [1] ELSE [2] END + 3", ints(&[1, 3])),
        ("size(CASE WHEN true THEN [1, 2] ELSE [] END)", int(2)),
        ("CASE WHEN false THEN [1] ELSE 5 + 1 END", int(6)),
        // The simple form compares its subject with each WHEN value.
        ("CASE n.text WHEN 'ab' THEN [1] ELSE [2] END", ints(&[1])),
        (
            "CASE n.text WHEN 'x' THEN [1] WHEN 'ab' THEN [2, 3] END",
            ints(&[2, 3]),
        ),
        ("CASE null WHEN null THEN [1] ELSE [2] END", ints(&[2])),
        // A map literal result is a composite too, in either arm.
        (
            "CASE WHEN true THEN {a: 1} ELSE null END",
            GraphValue::Map {
                keys: vec![Box::<str>::from("a")].into_boxed_slice(),
                values: vec![int(1)].into_boxed_slice(),
            },
        ),
        (
            "CASE WHEN n.text = 'x' THEN {a: 1} ELSE {b: n.text, c: [2]} END",
            GraphValue::Map {
                keys: vec![Box::<str>::from("b"), Box::from("c")].into_boxed_slice(),
                values: vec![word("ab"), ints(&[2])].into_boxed_slice(),
            },
        ),
        ("CASE n.text WHEN 'x' THEN {a: 1} END", null.clone()),
    ] {
        assert_eq!(value(expression), expected, "{expression}");
    }
    // A subscript or slice of a name is read in the value language too
    // (spill_set's `CASE WHEN true THEN {values: items, tail: items[1..]}
    // ELSE null END`); the scalar program refused both at prepare.
    for (expression, expected) in [
        (
            "CASE WHEN true THEN items[1..] ELSE null END",
            ints(&[2, 3]),
        ),
        ("CASE WHEN false THEN null ELSE items[..1] END", ints(&[1])),
        (
            "CASE WHEN true THEN {values: items, tail: items[1..]} ELSE null END",
            GraphValue::Map {
                keys: vec![Box::<str>::from("tail"), Box::from("values")].into_boxed_slice(),
                values: vec![ints(&[2, 3]), ints(&[1, 2, 3])].into_boxed_slice(),
            },
        ),
        ("CASE WHEN true THEN items[1] ELSE 0 END", int(2)),
        ("CASE WHEN false THEN items[1] ELSE 0 END", int(0)),
    ] {
        let query = prepare(&format!(
            "MATCH (n) WITH [1, 2, 3] AS items RETURN {expression} AS value"
        ));
        let result = execute(&query, &text("ab"), policy())
            .unwrap_or_else(|error| panic!("{expression}: {error:?}"));
        assert_eq!(result.value[0].values()[0], expected, "{expression}");
    }
    let unselected_fails = prepare(
        "MATCH (n) RETURN CASE WHEN false THEN [1] ELSE [1 / (size(n.text) - 2)] END AS value",
    );
    assert!(execute(&unselected_fails, &text("ab"), policy()).is_err());
    // A list never subtracts.
    for expression in ["[1] - 1", "1 - [1]", "[1] + 2 - 3"] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        assert!(
            execute(&query, &text("ab"), policy()).is_err(),
            "{expression}"
        );
    }
}

/// A scalar CASE takes whole expressions everywhere, as openCypher does: a
/// searched condition is any Boolean (TCK Quantifier9-12 select with
/// `CASE WHEN input.fixed THEN ...`), and a result or a simple-CASE operand
/// may itself compare. A NULL or FALSE condition does not select; a
/// condition that is not a Boolean is a typed failure, never a guessed truth.
#[test]
fn scalar_case_positions_are_whole_expressions() {
    use CanonicalScalar::{Bool, Int, Null};
    for (case, (expression, input, expected)) in [
        (
            "CASE WHEN n.text STARTS WITH 'a' THEN 1 ELSE 2 END",
            text("ab"),
            Int(1),
        ),
        (
            "CASE WHEN NOT n.text STARTS WITH 'a' THEN 1 ELSE 2 END",
            text("ab"),
            Int(2),
        ),
        (
            "CASE WHEN n.text IN ['x', 'ab'] THEN 'in' END",
            text("ab"),
            text("in"),
        ),
        (
            "CASE WHEN n.text IS NULL OR n.text = 'q' THEN 1 ELSE 2 END",
            Null,
            Int(1),
        ),
        ("CASE WHEN n.text THEN 1 ELSE 2 END", Bool(true), Int(1)),
        ("CASE WHEN n.text THEN 1 ELSE 2 END", Bool(false), Int(2)),
        ("CASE WHEN n.text THEN 1 ELSE 2 END", Null, Int(2)),
        (
            "CASE WHEN NOT n.text THEN 1 ELSE 2 END",
            Bool(false),
            Int(1),
        ),
        (
            "CASE WHEN n.text XOR true THEN 1 ELSE 2 END",
            Bool(true),
            Int(2),
        ),
        (
            "CASE WHEN toBoolean(n.text) THEN 1 ELSE 2 END",
            text("TRUE"),
            Int(1),
        ),
        (
            "CASE WHEN (n.text > 'a') = true THEN 1 END",
            text("b"),
            Int(1),
        ),
        (
            "CASE WHEN true THEN n.text = 'ab' END",
            text("ab"),
            Bool(true),
        ),
        (
            "CASE n.text = 'ab' WHEN true THEN 'yes' ELSE 'no' END",
            text("ab"),
            text("yes"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(scalar(expression, &input), expected, "case {case}");
    }
    assert!(
        execute(
            &prepare("MATCH (n) RETURN CASE WHEN n.text THEN 1 END AS value"),
            &Int(1),
            policy()
        )
        .is_err()
    );
}

/// openCypher toBoolean (TCK TypeConversion1): a Boolean is itself, the text
/// `true` or `false` in any case converts, other text is NULL untrimmed, and
/// NULL stays NULL. An integer is FALSE only at zero. A float is a typed
/// failure, never a guessed truth value.
#[test]
fn opencypher_to_boolean_converts_booleans_text_and_integers() {
    let truth = |value: bool| CanonicalScalar::Bool(value);
    for (case, (input, expected)) in [
        (CanonicalScalar::Bool(true), truth(true)),
        (CanonicalScalar::Bool(false), truth(false)),
        (text("true"), truth(true)),
        (text("FALSE"), truth(false)),
        (text("True"), truth(true)),
        (text(""), CanonicalScalar::Null),
        (text(" tru "), CanonicalScalar::Null),
        (text(" true"), CanonicalScalar::Null),
        (text("f alse"), CanonicalScalar::Null),
        (CanonicalScalar::Null, CanonicalScalar::Null),
        (CanonicalScalar::Int(0), truth(false)),
        (CanonicalScalar::Int(-3), truth(true)),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(scalar("toBoolean(n.text)", &input), expected, "case {case}");
    }
    assert_eq!(
        scalar("toBoolean('true') AND NOT TOBOOLEAN('false')", &text("x")),
        truth(true)
    );
    let float = CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.0));
    assert!(
        execute(
            &prepare("MATCH (n) RETURN toBoolean(n.text) AS value"),
            &float,
            policy()
        )
        .is_err()
    );
}

/// fgdb-j687q: openCypher toString / toInteger over the scalar domain. NULL
/// stays NULL. toInteger parses text as a number and truncates a fraction
/// toward zero, as the openCypher TCK expects (TypeConversion2 [4] '1.7' is 1,
/// [5] '2.9' is 2). Text that is not a finite number, or does not fit an i64,
/// is NULL. A non-scalar domain is a typed failure, never a guessed
/// rendering. toString is static text, so `+` concatenates it, and an
/// aggregate count converts through the result projection.
#[test]
fn opencypher_to_string_and_to_integer_convert_the_scalar_domain() {
    let input = text("42");
    for (expression, expected) in [
        ("toString(n.text)", text("42")),
        ("toInteger(n.text)", CanonicalScalar::Int(42)),
        ("toInteger(n.text) + 1", CanonicalScalar::Int(43)),
        ("TOSTRING(-7)", text("-7")),
        ("toString(TRUE)", text("true")),
        ("toInteger(FALSE)", CanonicalScalar::Int(0)),
        ("toInteger('x')", CanonicalScalar::Null),
        ("toInteger('3.9')", CanonicalScalar::Int(3)),
        ("toInteger('1.7')", CanonicalScalar::Int(1)),
        ("toInteger('-2.9')", CanonicalScalar::Int(-2)),
        ("toInteger('1e3')", CanonicalScalar::Int(1000)),
        ("toInteger('NaN')", CanonicalScalar::Null),
        ("toInteger('inf')", CanonicalScalar::Null),
        ("toInteger('')", CanonicalScalar::Null),
        ("toInteger('9223372036854775808')", CanonicalScalar::Null),
        ("'n=' + toString(toInteger(n.text) * 2)", text("n=84")),
        // Only toString makes these text: `+` must still concatenate.
        ("toString(1) + toString(2)", text("12")),
    ] {
        assert_eq!(scalar(expression, &input), expected, "{expression}");
    }
    assert_eq!(
        scalar("toString(n.text)", &CanonicalScalar::Null),
        CanonicalScalar::Null
    );
    // A float converts: toString's shortest round-trip text, toInteger's
    // truncation (numeric_functions_round_once_... covers the edges).
    let float = CanonicalScalar::Float(fgdb_types::CanonicalF64::new(1.5));
    assert_eq!(scalar("toString(n.text)", &float), text("1.5"));
    assert_eq!(scalar("toInteger(n.text)", &float), CanonicalScalar::Int(1));
    // toString is text: adding it to an integer is the typed Concat refusal.
    assert!(
        PreparedGraphSetText::prepare("MATCH (n) RETURN 1 + toString(2) AS value", symbols)
            .is_err()
    );
    // toInteger reads its text: one unit of work per payload unit, isolated
    // by differencing against a plain read of the same property.
    let parse = prepare("MATCH (n) RETURN toInteger(n.text) AS value");
    let plain = prepare("MATCH (n) RETURN n.text AS value");
    let work = |query: &PreparedGraphSet, input: &CanonicalScalar| {
        execute(query, input, policy())
            .unwrap()
            .evaluator
            .work_units
    };
    let (short, long) = (text("1"), text(&"1".repeat(1024)));
    assert_eq!(
        (work(&parse, &long) - work(&plain, &long)) - (work(&parse, &short) - work(&plain, &short)),
        1024 / 64 - 1
    );
    let stored = CanonicalPropertyValue::from(input.clone());
    let counted = fgdb_gql::PreparedGraphAggregateText::prepare(
        "MATCH (n) RETURN toString(COUNT(*)) AS c",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .execute_governed(
        1,
        [VId(1)],
        [],
        |_, _| Ok::<_, ()>(true),
        |_, _| Ok(Some(&stored)),
        policy(),
        || Ok::<_, ()>(()),
    )
    .unwrap();
    assert_eq!(
        counted.value[0].values()[0].as_value().cloned(),
        Some(fgdb_gql::algebra::GraphValue::Scalar(text("1")))
    );
}

/// fgdb-20foe: openCypher `text =~ 'pattern'` is a whole-string match under
/// fgdb regex profile 1. The pattern is a text literal compiled once, at
/// preparation, so a malformed or out-of-profile pattern and a non-literal
/// pattern refuse before execution. It filters in the graph pattern's WHERE,
/// yields a three-valued Boolean in RETURN, fails typed on a non-text input,
/// is part of the plan identity, and charges its bounded work.
#[test]
fn opencypher_regex_match_is_a_bounded_whole_string_match() {
    let rows = |statement: &str, input: &CanonicalScalar| {
        execute(&prepare(statement), input, policy())
            .unwrap()
            .value
            .len()
    };
    for (pattern, text_value, expected) in [
        ("A.*", "Ada", 1),
        ("A.*", "bob", 0),
        ("a", "Ada", 0),
        ("(?i)ada", "ADA", 1),
        ("[a-z]+@[a-z]+\\.com", "ada@lab.com", 1),
        ("\\d{3}", "12", 0),
    ] {
        // The regex is a text literal: its backslashes are escaped in the
        // statement, as in openCypher (fgdb-ibczc).
        let literal = pattern.replace('\\', "\\\\");
        let statement = format!("MATCH (n) WHERE n.text =~ '{literal}' RETURN n.text AS value");
        assert_eq!(rows(&statement, &text(text_value)), expected, "{statement}");
    }
    assert_eq!(
        scalar("n.text =~ 'a.c'", &text("abc")),
        CanonicalScalar::Bool(true)
    );
    assert_eq!(
        scalar("n.text =~ 'a.c'", &text("ab")),
        CanonicalScalar::Bool(false)
    );
    assert_eq!(
        scalar("n.text =~ 'a.c'", &CanonicalScalar::Null),
        CanonicalScalar::Null
    );
    assert!(
        execute(
            &prepare("MATCH (n) RETURN n.text =~ 'a' AS value"),
            &CanonicalScalar::Int(1),
            policy()
        )
        .is_err()
    );
    for statement in [
        "MATCH (n) WHERE n.text =~ '(ab' RETURN n.text AS value",
        // The regex `\bword` (a word boundary profile 1 lacks) is written with
        // an escaped backslash; `'\bword'` would be a backspace character.
        "MATCH (n) WHERE n.text =~ '\\\\bword' RETURN n.text AS value",
        "MATCH (n) WHERE n.text =~ n.text RETURN n.text AS value",
        "MATCH (n) WHERE n.text =~ 1 RETURN n.text AS value",
    ] {
        assert!(
            PreparedGraphSetText::prepare(statement, symbols).is_err(),
            "{statement}"
        );
    }
    assert_eq!(
        prepare("MATCH (n) RETURN n.text =~ 'a+' AS value").canonical_bytes(),
        prepare("MATCH (n) RETURN n.text =~ 'a+' AS value").canonical_bytes()
    );
    assert_ne!(
        prepare("MATCH (n) RETURN n.text =~ 'a+' AS value").canonical_bytes(),
        prepare("MATCH (n) RETURN n.text =~ 'a*' AS value").canonical_bytes()
    );
    // The pattern is part of the template identity too: two computed
    // grouping keys differing only in their pattern stay two columns.
    let keyed = fgdb_gql::PreparedGraphPipelineAggregateText::prepare(
        "MATCH (n) WITH n.text AS t RETURN t =~ 'a.*' AS x, t =~ 'b.*' AS y, COUNT(*) AS c",
        symbols,
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    let input = CanonicalPropertyValue::from(text("ab"));
    let grouped = keyed
        .execute_governed(
            1,
            [VId(1)],
            [],
            |_, _| Ok::<_, ()>(true),
            |_, _| Ok(Some(&input)),
            policy(),
            || Ok::<_, ()>(()),
        )
        .unwrap();
    let keys = grouped.value[0].keys();
    assert_eq!(keys[0].as_scalar(), Some(&CanonicalScalar::Bool(true)));
    assert_eq!(keys[1].as_scalar(), Some(&CanonicalScalar::Bool(false)));
    // Work is charged for text length x program size, differenced against a
    // plain read of the same property.
    let matcher = prepare("MATCH (n) RETURN n.text =~ 'x*' AS value");
    let plain = prepare("MATCH (n) RETURN n.text AS value");
    let work = |query: &PreparedGraphSet, input: &CanonicalScalar| {
        execute(query, input, policy())
            .unwrap()
            .evaluator
            .work_units
    };
    let (short, long) = (text("x"), text(&"x".repeat(1024)));
    let counted_long = work(&matcher, &long) - work(&plain, &long);
    let counted_short = work(&matcher, &short) - work(&plain, &short);
    assert!(
        counted_long >= counted_short + 1024 / 64,
        "{counted_long} vs {counted_short}"
    );
}

/// openCypher toFloat, floor, ceil, round and sqrt return binary64 values
/// rounded once by the integer-only kernel; toInteger truncates a float
/// toward zero and toString prints its shortest round-trip text. NaN and the
/// infinities pass through as IEEE 754 defines; NULL is NULL.
#[test]
fn numeric_functions_round_once_and_follow_opencypher_ties_and_specials() {
    let float = |value: f64| CanonicalScalar::Float(fgdb_types::CanonicalF64::new(value));
    for (input, expression, expected) in [
        (float(2.5), "round(n.text)", float(3.0)),
        (float(-2.5), "round(n.text)", float(-2.0)),
        (float(2.4), "round(n.text)", float(2.0)),
        (float(-2.7), "floor(n.text)", float(-3.0)),
        (float(-2.7), "ceil(n.text)", float(-2.0)),
        (float(2.0), "sqrt(n.text)", float(core::f64::consts::SQRT_2)),
        (float(-1.0), "sqrt(n.text)", float(f64::NAN)),
        (float(f64::INFINITY), "sqrt(n.text)", float(f64::INFINITY)),
        (
            float(f64::NEG_INFINITY),
            "floor(n.text)",
            float(f64::NEG_INFINITY),
        ),
        (CanonicalScalar::Int(7), "toFloat(n.text)", float(7.0)),
        (CanonicalScalar::Int(7), "round(n.text)", float(7.0)),
        (text("2.5"), "toFloat(n.text)", float(2.5)),
        (text("nope"), "toFloat(n.text)", CanonicalScalar::Null),
        (
            CanonicalScalar::Null,
            "round(n.text)",
            CanonicalScalar::Null,
        ),
        (float(3.7), "toInteger(n.text)", CanonicalScalar::Int(3)),
        (float(-3.7), "toInteger(n.text)", CanonicalScalar::Int(-3)),
        (float(f64::NAN), "toInteger(n.text)", CanonicalScalar::Null),
        (float(1.0), "toString(n.text)", text("1.0")),
        (float(0.1), "toString(n.text)", text("0.1")),
        (float(1e100), "toString(n.text)", text("1e100")),
        (
            float(f64::NEG_INFINITY),
            "toString(n.text)",
            text("-Infinity"),
        ),
        (float(1.0), "-2.5", float(-2.5)),
        (float(4.0), "n.text * -1.5", float(-6.0)),
        (float(1.0), "1e5", float(1e5)),
        (float(1.0), "1.5e-3", float(1.5e-3)),
        (float(1.0), "2E+10", float(2e10)),
        (float(1.0), "-4.2e1", float(-42.0)),
        (float(3.0), "n.text * 1e2", float(300.0)),
    ] {
        assert_eq!(scalar(expression, &input), expected, "{expression}");
    }
    // An exponent literal outside binary64 is refused, never infinity.
    assert!(PreparedGraphSetText::prepare("MATCH (n) RETURN 1e999 AS value", symbols).is_err());
    // A float outside i64 overflows instead of wrapping or saturating.
    let query = prepare("MATCH (n) RETURN toInteger(n.text) AS value");
    assert!(execute(&query, &float(1e19), policy()).is_err());
    // Numeric functions refuse text (toFloat aside) and Boolean operands.
    for statement in [
        "MATCH (n) RETURN round('a') AS value",
        "MATCH (n) RETURN sqrt(TRUE) AS value",
    ] {
        assert!(
            PreparedGraphSetText::prepare(statement, symbols).is_err(),
            "{statement}"
        );
    }
    // A negative float literal also compares in a property filter.
    let filtered = prepare("MATCH (n) WHERE n.text > -0.5 RETURN n.text AS value");
    assert_eq!(
        execute(&filtered, &float(-0.25), policy())
            .unwrap()
            .value
            .len(),
        1
    );
    assert!(
        execute(&filtered, &float(-0.75), policy())
            .unwrap()
            .value
            .is_empty()
    );
    let exponent = prepare("MATCH (n) WHERE n.text < 2.5e-1 RETURN n.text AS value");
    assert_eq!(
        execute(&exponent, &float(0.125), policy())
            .unwrap()
            .value
            .len(),
        1
    );
    assert!(
        execute(&exponent, &float(0.5), policy())
            .unwrap()
            .value
            .is_empty()
    );
}

/// openCypher's math functions run fgdb-types' pinned kernels, so a result
/// is the same bits on every target: exp, log, log10, sin, cos, tan, cot,
/// asin, acos, atan, atan2, degrees, radians, haversin, sign, pi() and e().
/// Integers convert to the nearest binary64 first; sign is an Int; NULL is
/// NULL; outside a real domain is NaN; a pole or an overflow refuses.
#[test]
fn math_functions_run_the_pinned_kernels() {
    use fgdb_types::CanonicalF64;
    let float = |value: f64| CanonicalScalar::Float(CanonicalF64::new(value));
    let kernel = |result: Result<CanonicalF64, fgdb_types::FloatArithmeticError>| {
        CanonicalScalar::Float(result.expect("an admitted argument"))
    };
    let x = CanonicalF64::new(0.7);
    for (input, expression, expected) in [
        // Each function is the kernel's bits, not the host libm's.
        (float(0.7), "exp(n.text)", kernel(x.exp())),
        (float(0.7), "log(n.text)", kernel(x.ln())),
        (float(0.7), "log10(n.text)", kernel(x.log10())),
        (float(0.7), "sin(n.text)", kernel(x.sin())),
        (float(0.7), "cos(n.text)", kernel(x.cos())),
        (float(0.7), "tan(n.text)", kernel(x.tan())),
        (float(0.7), "asin(n.text)", kernel(x.asin())),
        (float(0.7), "acos(n.text)", kernel(x.acos())),
        (float(0.7), "atan(n.text)", kernel(x.atan())),
        (
            float(0.7),
            "atan2(n.text, -2.0)",
            kernel(x.atan2(CanonicalF64::new(-2.0))),
        ),
        (
            float(0.7),
            "cot(n.text)",
            kernel(CanonicalF64::new(1.0).checked_div(x.tan().unwrap())),
        ),
        (
            float(1e22),
            "sin(n.text)",
            kernel(CanonicalF64::new(1e22).sin()),
        ),
        // Exact anchors.
        (float(0.0), "exp(n.text)", float(1.0)),
        (float(1.0), "log(n.text)", float(0.0)),
        (CanonicalScalar::Int(1000), "log10(n.text)", float(3.0)),
        (float(1e-300), "log10(n.text)", float(-300.0)),
        (float(0.0), "cos(n.text)", float(1.0)),
        (float(0.0), "haversin(n.text)", float(0.0)),
        (float(1.0), "atan2(0.0, -1.0)", float(core::f64::consts::PI)),
        (
            float(1.0),
            "atan2(1, 0)",
            float(core::f64::consts::FRAC_PI_2),
        ),
        (
            float(core::f64::consts::PI),
            "degrees(n.text)",
            float(180.0),
        ),
        (
            CanonicalScalar::Int(180),
            "radians(n.text)",
            float(core::f64::consts::PI),
        ),
        (float(1.0), "pi()", float(core::f64::consts::PI)),
        (float(1.0), "e()", float(core::f64::consts::E)),
        (float(1.0), "PI() * 2", float(core::f64::consts::TAU)),
        // haversin is (1 - cos x) / 2.
        (
            float(1.0),
            "haversin(n.text)",
            kernel(
                CanonicalF64::new(1.0)
                    .checked_sub(CanonicalF64::new(1.0).cos().unwrap())
                    .and_then(|gap| gap.checked_div(CanonicalF64::new(2.0))),
            ),
        ),
        // sign is an Int, NaN's 0.
        (
            CanonicalScalar::Int(-3),
            "sign(n.text)",
            CanonicalScalar::Int(-1),
        ),
        (float(2.5), "sign(n.text)", CanonicalScalar::Int(1)),
        (float(0.0), "sign(n.text)", CanonicalScalar::Int(0)),
        (float(f64::NAN), "sign(n.text)", CanonicalScalar::Int(0)),
        // Outside the real domain is NaN; infinities follow IEEE 754.
        (float(-1.0), "log(n.text)", float(f64::NAN)),
        (float(2.0), "asin(n.text)", float(f64::NAN)),
        (float(f64::INFINITY), "sin(n.text)", float(f64::NAN)),
        (float(f64::NEG_INFINITY), "exp(n.text)", float(0.0)),
        (
            float(f64::INFINITY),
            "atan(n.text)",
            float(core::f64::consts::FRAC_PI_2),
        ),
        (
            float(f64::INFINITY),
            "degrees(n.text)",
            float(f64::INFINITY),
        ),
        (CanonicalScalar::Null, "exp(n.text)", CanonicalScalar::Null),
        (
            CanonicalScalar::Null,
            "atan2(1.0, n.text)",
            CanonicalScalar::Null,
        ),
        (CanonicalScalar::Null, "sign(n.text)", CanonicalScalar::Null),
    ] {
        assert_eq!(scalar(expression, &input), expected, "{expression}");
    }
    // A pole or an overflow refuses at execution, as 1.0 / 0.0 does.
    for (expression, input) in [
        ("log(n.text)", float(0.0)),
        ("log10(n.text)", float(0.0)),
        ("cot(n.text)", float(0.0)),
        ("exp(n.text)", float(710.0)),
    ] {
        let query = prepare(&format!("MATCH (n) RETURN {expression} AS value"));
        assert!(execute(&query, &input, policy()).is_err(), "{expression}");
    }
    // Text and Boolean operands refuse at preparation.
    for statement in [
        "MATCH (n) RETURN exp('a') AS value",
        "MATCH (n) RETURN sign(TRUE) AS value",
        "MATCH (n) RETURN pi(1) AS value",
    ] {
        assert!(
            PreparedGraphSetText::prepare(statement, symbols).is_err(),
            "{statement}"
        );
    }
    // sign is an Int operand: it adds to an integer without a float.
    assert_eq!(
        scalar("sign(n.text) + 1", &CanonicalScalar::Int(5)),
        CanonicalScalar::Int(2)
    );
    // E names a function only before `(`: a variable e still reads.
    let query = prepare("MATCH (e) RETURN e.text AS value");
    let result = execute(&query, &float(4.0), policy()).unwrap();
    assert_eq!(result.value[0].values()[0].as_scalar(), Some(&float(4.0)));
}

/// A test tzdb artifact: Europe/Stockholm with its 1984 and 2017 summer
/// time (CET +01:00, CEST +02:00 from 01:00 UTC on the switch days, as
/// zdump reports them) and Pacific/Honolulu at a fixed -10:00.
struct TestTzdb;
const TEST_TZDB: fgdb_types::ObjectId = fgdb_types::ObjectId([0x7a; 32]);
impl fgdb_types::TzdbResolver for TestTzdb {
    fn contains_tzdb(&self, tzdb_oid: &fgdb_types::ObjectId) -> bool {
        *tzdb_oid == TEST_TZDB
    }
    fn canonical_utc_offset_seconds(
        &self,
        tzdb_oid: &fgdb_types::ObjectId,
        zone_identifier: &str,
        instant_utc_nanos: i128,
    ) -> Option<i32> {
        if !self.contains_tzdb(tzdb_oid) {
            return None;
        }
        let second = instant_utc_nanos.div_euclid(1_000_000_000);
        match zone_identifier {
            "Pacific/Honolulu" => Some(-36_000),
            "Europe/Stockholm" => {
                let summers = [(449_024_400, 465_354_000), (1_490_490_000, 1_509_238_800)];
                let summer = summers
                    .iter()
                    .any(|&(start, end)| (start..end).contains(&second));
                Some(if summer { 7_200 } else { 3_600 })
            }
            _ => None,
        }
    }
}

/// A catalog with the test tzdb as its named-zone capability.
struct ZonedCatalog;
impl fgdb_gql::GraphSymbolResolver for ZonedCatalog {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        symbols(kind, name)
    }
    fn time_zones(&self) -> Option<fgdb_gql::GqlTimeZones> {
        fgdb_gql::GqlTimeZones::new(std::sync::Arc::new(TestTzdb), TEST_TZDB)
    }
}

fn zoned(expression: &str) -> Result<fgdb_types::CanonicalTimestamp, String> {
    match zoned_scalar(expression)? {
        CanonicalScalar::Timestamp(value) => Ok(value),
        other => Err(format!("{other:?}")),
    }
}

fn zoned_scalar(expression: &str) -> Result<CanonicalScalar, String> {
    let statement = format!("MATCH (n) RETURN {expression} AS value");
    let query = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        &statement,
        &[],
        &mut ZonedCatalog,
    )
    .map_err(|error| format!("{error:?}"))?
    .bind_parameters(&GqlParameters::new())
    .map_err(|error| format!("{error:?}"))?;
    let result = execute(&query, &CanonicalScalar::Null, policy()).map_err(|_| "execution")?;
    result.value[0].values()[0]
        .as_scalar()
        .cloned()
        .ok_or_else(|| "not a scalar".to_owned())
}

/// fgdb-txbg9: a named zone (`timezone: 'Europe/Stockholm'`, a text value's
/// `[zone]` suffix) resolves only through the statement's pinned tzdb, by
/// java.time's rules: the offset in force at that local time, the earlier
/// offset in an overlap, and a gap's wall time moved later by the gap. A
/// datetime base keeps its instant in a new zone; truncation keeps a zoned
/// value's zone, or places the result in a map's zone. The value records the
/// zone and the artifact. Without a tzdb, or for a zone it lacks, a name
/// refuses, and a plan that carries a tzdb records its identity.
#[test]
fn named_time_zones_resolve_through_the_pinned_tzdb_only() {
    const HOUR: i128 = 3_600 * 1_000_000_000;
    let second = |s: i128| s * 1_000_000_000;
    // (expression, UTC instant, offset seconds, zone)
    for (expression, instant, offset, zone) in [
        // After the 1984 switch back: CET.
        (
            "datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'})",
            second(466_340_400),
            3_600,
            Some("Europe/Stockholm"),
        ),
        // Summer: CEST.
        (
            "datetime({year: 1984, month: 7, day: 11, hour: 12, timezone: 'Europe/Stockholm'})",
            second(458_388_000),
            7_200,
            Some("Europe/Stockholm"),
        ),
        // 02:30 on 1984-03-25 does not exist: it becomes 03:30 CEST.
        (
            "datetime({year: 1984, month: 3, day: 25, hour: 2, minute: 30, timezone: 'Europe/Stockholm'})",
            second(449_026_200),
            7_200,
            Some("Europe/Stockholm"),
        ),
        // 02:30 on 1984-09-30 happens twice: the earlier, CEST.
        (
            "datetime({year: 1984, month: 9, day: 30, hour: 2, minute: 30, timezone: 'Europe/Stockholm'})",
            second(465_352_200),
            7_200,
            Some("Europe/Stockholm"),
        ),
        // A datetime base keeps its instant in the new zone.
        (
            "datetime({datetime: datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: '+01:00'}), timezone: 'Pacific/Honolulu'})",
            second(466_340_400),
            -36_000,
            Some("Pacific/Honolulu"),
        ),
        // A zoned base keeps its zone when its fields change.
        (
            "datetime({datetime: datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'}), month: 7})",
            second(458_388_000),
            7_200,
            Some("Europe/Stockholm"),
        ),
        // Text with a zone suffix, with and without an agreeing offset.
        (
            "datetime('1984-10-11T12:00[Europe/Stockholm]')",
            second(466_340_400),
            3_600,
            Some("Europe/Stockholm"),
        ),
        (
            "datetime('1984-10-11T12:00+01:00[Europe/Stockholm]')",
            second(466_340_400),
            3_600,
            Some("Europe/Stockholm"),
        ),
        // Truncation keeps the zone, and a map's zone places the result.
        (
            "datetime.truncate('day', datetime({year: 2017, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'}))",
            second(1_507_672_800),
            7_200,
            Some("Europe/Stockholm"),
        ),
        (
            "datetime.truncate('day', localdatetime({year: 2017, month: 10, day: 11, hour: 12}), {timezone: 'Pacific/Honolulu'})",
            second(1_507_672_800) + 12 * HOUR,
            -36_000,
            Some("Pacific/Honolulu"),
        ),
        // An offset stays an offset.
        (
            "datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: '+01:00'})",
            second(466_340_400),
            3_600,
            None,
        ),
        // A zoned time base lends its zone, resolved at the new date: 12:00
        // on 1984-03-28 is CEST (TCK Temporal3 [9], [10]).
        (
            "datetime({year: 1984, month: 3, day: 28, time: datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'})})",
            second(449_316_000),
            7_200,
            Some("Europe/Stockholm"),
        ),
        // Moved to another zone, its time of day is read at that new date.
        (
            "datetime({year: 1984, month: 3, day: 28, time: datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'}), second: 42, timezone: 'Pacific/Honolulu'})",
            second(449_316_042),
            -36_000,
            Some("Pacific/Honolulu"),
        ),
        (
            "datetime({year: 1984, month: 10, day: 11, time: datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'}), timezone: '+05:00'})",
            second(466_340_400),
            18_000,
            None,
        ),
    ] {
        let value = zoned(expression).unwrap_or_else(|error| panic!("{expression}: {error}"));
        assert_eq!(value.instant_utc_nanos(), instant, "{expression}");
        assert_eq!(value.utc_offset_seconds(), offset, "{expression}");
        assert_eq!(
            value.zone().map(|zone| zone.identifier()),
            zone,
            "{expression}"
        );
        if let Some(zone) = value.zone() {
            assert_eq!(zone.tzdb_oid(), TEST_TZDB, "{expression}");
        }
    }
    // A span with one zoned side reads the other side in that zone, at its
    // own wall time, so it counts the hour a daylight saving change adds
    // (TCK Temporal10 [8]; 2017-10-29 03:00 CEST is 02:00 CET).
    let stockholm_midnight =
        "datetime({year: 2017, month: 10, day: 29, hour: 0, timezone: 'Europe/Stockholm'})";
    let four_cet =
        "datetime({year: 2017, month: 10, day: 29, hour: 4, timezone: 'Europe/Stockholm'})";
    for (left, right, hours) in [
        (
            stockholm_midnight,
            "localdatetime({year: 2017, month: 10, day: 29, hour: 4})",
            5,
        ),
        (stockholm_midnight, "localtime({hour: 4})", 5),
        (
            "localdatetime({year: 2017, month: 10, day: 29, hour: 0})",
            four_cet,
            5,
        ),
        ("date({year: 2017, month: 10, day: 29})", four_cet, 5),
        (
            stockholm_midnight,
            "date({year: 2017, month: 10, day: 30})",
            25,
        ),
        // A fixed offset has no change to count.
        (
            "datetime({year: 2017, month: 10, day: 29, hour: 0, timezone: '+02:00'})",
            "localdatetime({year: 2017, month: 10, day: 29, hour: 4})",
            4,
        ),
    ] {
        let expression = format!("duration.inSeconds({left}, {right})");
        let expected = fgdb_types::CanonicalDuration::new(0, 0, i128::from(hours) * HOUR).unwrap();
        assert_eq!(
            zoned_scalar(&expression),
            Ok(CanonicalScalar::Duration(expected)),
            "{expression}"
        );
    }
    for refused in [
        "datetime({year: 1984, month: 10, day: 11, timezone: 'Frost/Nixon'})",
        "datetime('1984-10-11T12:00+02:00[Europe/Stockholm]')",
        "time({hour: 12, timezone: 'Europe/Stockholm'})",
    ] {
        assert!(zoned(refused).is_err(), "{refused}");
    }
    // Without a pinned tzdb a named zone refuses.
    let plain = prepare(
        "MATCH (n) RETURN datetime({year: 1984, month: 10, day: 11, timezone: 'Europe/Stockholm'}) AS value",
    );
    assert!(execute(&plain, &CanonicalScalar::Null, policy()).is_err());
    // The tzdb is part of the plan's identity.
    let text = "MATCH (n) RETURN datetime({year: 1984, month: 10, day: 11}) AS value";
    let with_zones = |text: &str| {
        PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
            text,
            &[],
            &mut ZonedCatalog,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    };
    assert_eq!(
        with_zones(text).canonical_bytes(),
        with_zones(text).canonical_bytes()
    );
    assert_ne!(
        with_zones(text).canonical_bytes(),
        prepare(text).canonical_bytes()
    );
}

/// STARTS WITH, ENDS WITH and CONTAINS over a value that is not text answer
/// NULL, lists and maps included (TCK String8/9/10 [8], fgdb-bqri3): only
/// text against text decides. An UNWIND element is dynamically typed, so each
/// row reaches the scalar program with its own kind.
#[test]
fn text_predicates_over_non_text_operands_are_null() {
    let query = prepare(
        "MATCH (n) UNWIND [1, 'ab', [], ['a'], {}, null] AS op \
         RETURN op STARTS WITH 'a' AS s, op ENDS WITH 'b' AS e, op CONTAINS '' AS c",
    );
    let result = execute(&query, &text("x"), policy()).unwrap();
    let rows: Vec<Vec<CanonicalScalar>> = result
        .value
        .iter()
        .map(|row| {
            row.values()
                .iter()
                .map(|value| value.as_scalar().expect("a Boolean or NULL").clone())
                .collect()
        })
        .collect();
    // Only 'ab' is text: every predicate holds for it, and the other five
    // rows (1, [], ['a'], {}, null) are NULL throughout.
    assert_eq!(rows.len(), 6);
    let count = |expected: CanonicalScalar| {
        rows.iter()
            .filter(|row| row.iter().all(|value| *value == expected))
            .count()
    };
    assert_eq!(count(CanonicalScalar::Bool(true)), 1);
    assert_eq!(count(CanonicalScalar::Null), 5);
}
