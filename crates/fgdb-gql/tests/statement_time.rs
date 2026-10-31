//! Current-time functions consume one explicit execution context, never a clock.
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::csv_parameters::{
    CsvParameterErrorKind, CsvParameterLimit, CsvParameterLimits,
    decode_csv_parameters_with_context,
};
use fgdb_gql::query_write::{GraphQueryWriteBuildError, PreparedGraphQueryWrite};
use fgdb_gql::{
    BoundNativeGraphWrite, GqlMapArgumentText, GqlParameters, GqlQueryPolicy,
    GraphPatternTextErrorKind, GraphSetTextErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphBranchText, PreparedGraphSet, PreparedGraphSetText, PreparedGraphWriteChainText,
    PreparedGraphWriteScript, PreparedTemporalGraphSetText, statement_time_required,
};
use fgdb_types::{CanonicalScalar, CanonicalTimestamp};

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}

fn instant() -> CanonicalTimestamp {
    // 2015-07-21T21:40:32.142+01:00, with a deliberately non-UTC session offset.
    CanonicalTimestamp::offset_only(1_437_511_232_142_000_000, 3600).unwrap()
}

fn arguments() -> GqlParameters {
    GqlParameters::new().with_statement_time(instant()).unwrap()
}

fn execute(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    query
        .execute_governed::<(), ()>(
            GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000),
            |_, _| panic!("source-free statement must not read a graph"),
            || Ok::<_, ()>(()),
        )
        .unwrap()
        .value
}

#[test]
fn all_current_time_forms_share_the_captured_instant_and_session_offset() {
    let statement = "UNWIND [1,2,3] AS row RETURN \
        toString(date()) AS d, toString(localtime()) AS lt, toString(time()) AS t, \
        toString(localdatetime()) AS ld, toString(datetime()) AS dt, \
        timestamp() AS ms, datetime() = datetime() AS same";
    let template = PreparedGraphSetText::prepare(statement, symbols).unwrap();
    assert_eq!(template.parameter_schema().len(), 1);
    assert!(template.parameter_schema()[0].is_statement_time());
    assert_eq!(template.parameter_schema()[0].occurrences, 8);
    let bound = template.bind_parameters(&arguments()).unwrap();
    let rows = execute(&bound);
    assert_eq!(rows.len(), 3);
    for row in rows {
        for (column, expected) in [
            "2015-07-21",
            "21:40:32.142",
            "21:40:32.142+01:00",
            "2015-07-21T21:40:32.142",
            "2015-07-21T21:40:32.142+01:00",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                row.values()[column].as_scalar(),
                Some(&CanonicalScalar::ucs_basic_text(expected).unwrap())
            );
        }
        assert_eq!(
            row.values()[5].as_scalar(),
            Some(&CanonicalScalar::Int(1_437_511_232_142))
        );
        assert_eq!(
            row.values()[6].as_scalar(),
            Some(&CanonicalScalar::Bool(true))
        );
    }
}

#[test]
fn temporal10_current_time_duration_outlines_are_zero() {
    for constructor in ["date", "time", "localtime", "datetime", "localdatetime"] {
        let statement =
            format!("RETURN duration.inSeconds({constructor}(), {constructor}()) AS elapsed");
        let template = PreparedGraphSetText::prepare(&statement, symbols).unwrap();
        let rows = execute(&template.bind_parameters(&arguments()).unwrap());
        let Some(CanonicalScalar::Duration(duration)) = rows[0].values()[0].as_scalar() else {
            panic!("{constructor} must produce a duration");
        };
        assert_eq!(duration.to_string(), "PT0S", "{constructor}");
    }
}

#[test]
fn templates_are_instant_free_and_rebinding_changes_only_the_execution() {
    let template = PreparedGraphSetText::prepare(
        "RETURN datetime() AS instant, timestamp() + 1 AS next_ms",
        symbols,
    )
    .unwrap();
    let schema = template.parameter_schema().to_vec();
    let definition = template.canonical_template_bytes();
    let missing = template.bind_parameters(&GqlParameters::new()).unwrap_err();
    assert_eq!(missing.offset, 7);
    assert_eq!(
        missing.kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::StatementTimeRequired)
    );
    let first = template.bind_parameters(&arguments()).unwrap();
    let later = CanonicalTimestamp::offset_only(1_437_511_233_142_000_000, 3600).unwrap();
    let second_arguments = arguments().with_statement_time(later.clone()).unwrap();
    let second = template.bind_parameters(&second_arguments).unwrap();
    assert_eq!(template.parameter_schema(), schema);
    assert_eq!(template.canonical_template_bytes(), definition);
    assert_ne!(first.canonical_bytes(), second.canonical_bytes());
    assert_eq!(
        execute(&first)[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::Timestamp(instant()))
    );
    assert_eq!(
        execute(&second)[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::Timestamp(later))
    );
    assert_eq!(
        execute(&second)[0].values()[1].as_scalar(),
        Some(&CanonicalScalar::Int(1_437_511_233_143))
    );
    assert_eq!(
        first.canonical_bytes(),
        template
            .bind_parameters(&arguments())
            .unwrap()
            .canonical_bytes()
    );
    let extra = arguments().with_int64("unused", 1).unwrap();
    assert_eq!(
        template.bind_parameters(&extra).unwrap_err().kind,
        GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::UnexpectedArguments)
    );
    // A shared context may accompany a child that has no current-time call.
    let plain = PreparedGraphSetText::prepare("RETURN 1 AS n", symbols).unwrap();
    assert_eq!(
        plain
            .bind_parameters(&arguments())
            .unwrap()
            .canonical_bytes(),
        plain
            .bind_parameters(&GqlParameters::new())
            .unwrap()
            .canonical_bytes()
    );
}

#[test]
fn compound_reads_and_argument_rewrites_preserve_one_context() {
    let union = PreparedGraphSetText::prepare(
        "RETURN datetime() AS t UNION ALL RETURN datetime() AS t",
        symbols,
    )
    .unwrap();
    let rows = execute(&union.bind_parameters(&arguments()).unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0], rows[1]);

    let branch =
        PreparedGraphBranchText::prepare("RETURN datetime() AS t AT BRANCH $branch").unwrap();
    let supplied = arguments().with_text("branch", "main").unwrap();
    let selected = branch.bind_parameters(&supplied).unwrap();
    assert!(selected.parameters().is_empty());
    assert_eq!(selected.parameters().statement_time(), Some(&instant()));
    let template = PreparedGraphSetText::prepare(selected.statement(), symbols).unwrap();
    assert_eq!(
        execute(&template.bind_parameters(selected.parameters()).unwrap()).len(),
        1
    );

    let supplied = arguments()
        .with_map(
            "m",
            vec![("p".into(), GraphValue::Scalar(CanonicalScalar::Int(7)))],
        )
        .unwrap();
    let (lowered, derived) =
        GqlMapArgumentText::lower_fields("RETURN $m.p AS p, datetime() AS t", &supplied).unwrap();
    assert_eq!(derived.statement_time(), Some(&instant()));
    let declarations = derived.parameter_types().collect::<Vec<_>>();
    let template =
        PreparedGraphSetText::prepare_with_parameter_types(lowered.text(), &declarations, symbols)
            .unwrap();
    assert_eq!(
        execute(&template.bind_parameters(&derived).unwrap())[0].values()[0].as_scalar(),
        Some(&CanonicalScalar::Int(7))
    );

    let temporal = PreparedTemporalGraphSetText::prepare(
        "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $seq RETURN datetime() AS t",
        symbols,
    )
    .unwrap();
    let supplied = arguments().with_uint64("seq", 1).unwrap();
    assert_eq!(temporal.bind_parameters(&supplied).unwrap().as_of().0, 1);
}

#[test]
fn current_time_binds_create_set_merge_foreach_and_bulk_write_shapes() {
    for statement in [
        "CREATE (n:L {p: datetime(), q: timestamp()})",
        "MATCH (n:L) SET n.p = datetime(), n.q = date()",
        "MERGE (n:L {p: 7}) ON CREATE SET n.q = datetime() ON MATCH SET n.q = datetime()",
        "CREATE (n:L {p: datetime()}); MATCH (n:L) SET n.q = timestamp()",
    ] {
        BoundNativeGraphWrite::bind(statement, &arguments(), RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        assert!(
            BoundNativeGraphWrite::bind(statement, &GqlParameters::new(), RelationId(1), symbols)
                .is_err(),
            "{statement}"
        );
    }
    for statement in [
        "CREATE (n:L {p: datetime()}) WITH n SET n.q = timestamp() RETURN n.q",
        "UNWIND [1,2] AS x CREATE (n:L {p: x}) FOREACH (y IN [1,2] | SET n.q = datetime()) RETURN n.q",
    ] {
        let chain = PreparedGraphWriteChainText::prepare(statement, RelationId(1), symbols)
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
        chain
            .bind_parameters(&arguments())
            .unwrap_or_else(|error| panic!("{statement}: {error:?}"));
    }
    let supplied = arguments()
        .with_list(
            "rows",
            vec![
                GraphValue::map(vec![(
                    "p".into(),
                    GraphValue::Scalar(CanonicalScalar::Int(1)),
                )])
                .unwrap(),
                GraphValue::map(vec![(
                    "p".into(),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                )])
                .unwrap(),
            ],
        )
        .unwrap();
    let batch = BoundNativeGraphWrite::bind(
        "UNWIND $rows AS row MATCH (n:L {p: row.p}) SET n.q = datetime()",
        &supplied,
        RelationId(1),
        symbols,
    )
    .unwrap();
    assert_eq!(batch.input_records(), Some(2));
}

#[test]
fn lexical_capture_hint_ignores_values_names_dotted_calls_and_explain() {
    for statement in [
        "RETURN date()",
        "RETURN DATETIME /* comment */ ( /* comment */ )",
        "MATCH (n) SET n.p = timestamp()",
        "CREATE (n); MATCH (n) SET n.p = datetime()",
        "PROFILE RETURN localtime()",
        "RETURN 'date()' AS text, datetime() AS t",
    ] {
        assert!(statement_time_required(statement), "{statement}");
    }
    for statement in [
        "RETURN 'date()' AS text",
        "RETURN `datetime` AS value",
        "RETURN `datetime`()",
        "RETURN 1 // datetime()",
        "RETURN datetime('2020-01-01')",
        "RETURN duration()",
        "CALL custom.datetime()",
        "RETURN date.statement()",
        "/* now */ EXPLAIN RETURN datetime()",
    ] {
        assert!(!statement_time_required(statement), "{statement}");
    }
    let comment = "x".repeat(40_000);
    assert!(statement_time_required(&format!(
        "/*{comment}*/ CREATE (n); /*{comment}*/ MATCH (n) SET n.p = datetime()"
    )));
}

#[test]
fn csv_and_query_fed_writes_share_context_without_exposing_a_clock_column() {
    let script = PreparedGraphWriteScript::prepare(
        "CREATE (n:L {p: $value, q: datetime()})",
        RelationId(1),
        symbols,
    )
    .unwrap();
    let context = arguments().with_int64("ignored_context_name", 99).unwrap();
    let input = "value\n1\n2\n";
    let rows = decode_csv_parameters_with_context(
        input,
        script.parameter_schema(),
        CsvParameterLimits::default(),
        &context,
    )
    .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter()
            .all(|row| row.len() == 1 && row.statement_time() == Some(&instant()))
    );
    let bytes = rows
        .iter()
        .map(GqlParameters::canonical_byte_len)
        .sum::<usize>();
    let limits = CsvParameterLimits {
        max_parameter_bytes: bytes,
        ..CsvParameterLimits::default()
    };
    let expected = script
        .bind_csv_with_context(input, &context, limits, 2)
        .unwrap();
    let tight = decode_csv_parameters_with_context(
        input,
        script.parameter_schema(),
        CsvParameterLimits {
            max_parameter_bytes: bytes - 1,
            ..limits
        },
        &context,
    )
    .unwrap_err();
    assert!(matches!(
        tight.kind,
        CsvParameterErrorKind::Limit {
            dimension: CsvParameterLimit::ParameterBytes,
            ..
        }
    ));
    let mut at = 0;
    let (streamed, records) = script
        .bind_csv_stream_with_context_controlled(
            &context,
            limits,
            2,
            |buffer| {
                let count = buffer.len().min(2).min(input.len() - at);
                buffer[..count].copy_from_slice(&input.as_bytes()[at..at + count]);
                at += count;
                Ok::<_, ()>(count)
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(records, 2);
    assert_eq!(
        streamed.canonical_bytes(),
        expected.program().canonical_bytes()
    );

    let source = PreparedGraphSetText::prepare("UNWIND [1,2] AS value RETURN value", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let rows = execute(&source);
    let query =
        PreparedGraphQueryWrite::prepare_with_context(source.clone(), script.clone(), &context)
            .unwrap();
    let bound = query.bind_rows(&rows, 2).unwrap().unwrap();
    assert_eq!(
        bound.program().canonical_bytes(),
        expected.program().canonical_bytes()
    );
    assert_eq!(
        PreparedGraphQueryWrite::prepare(source, script).unwrap_err(),
        GraphQueryWriteBuildError::StatementTimeRequired
    );
}

#[test]
fn an_empty_query_fed_source_still_requires_statement_time_before_execution() {
    let source = PreparedGraphSetText::prepare("UNWIND [] AS value RETURN value", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let rows = execute(&source);
    assert!(rows.is_empty());
    let script = PreparedGraphWriteScript::prepare(
        "CREATE (n:L {p: $value, q: datetime()})",
        RelationId(1),
        symbols,
    )
    .unwrap();
    let error = PreparedGraphQueryWrite::prepare(source.clone(), script.clone()).unwrap_err();
    assert_eq!(error, GraphQueryWriteBuildError::StatementTimeRequired);
    assert_eq!(error.class(), fgdb_gql::GqlRefusalClass::InvalidValue);
    let bound =
        PreparedGraphQueryWrite::prepare_with_context(source, script, &arguments()).unwrap();
    assert!(bound.bind_rows(&rows, 0).unwrap().is_none());
}

#[test]
fn csv_chain_context_is_bound_once_beside_the_named_row_source() {
    let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
        "UNWIND $rows AS row CREATE (n:L {p: row.value, q: datetime()}) \
         WITH n SET n.q = datetime() RETURN n.q",
        RelationId(1),
        &[("rows", fgdb_gql::GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    let schema = [fgdb_gql::GqlParameterSpec {
        name: "value".into(),
        parameter_type: fgdb_gql::GqlParameterType::Int64,
        requires_positive: false,
        occurrences: 1,
    }];
    let input = b"value\n1\n2\n";
    let mut at = 0;
    let (chain, records) = template
        .bind_csv_rows_stream_with_context_controlled(
            "rows",
            &schema,
            &arguments(),
            CsvParameterLimits::default(),
            |buffer| {
                let count = buffer.len().min(3).min(input.len() - at);
                buffer[..count].copy_from_slice(&input[at..at + count]);
                at += count;
                Ok::<_, ()>(count)
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(records, 2);
    let rows = chain
        .initial_rows_governed::<(), ()>(GqlQueryPolicy::new(100, 100, 100_000, 100_000), || Ok(()))
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(rows.len(), 2);
    for row in rows {
        let (keys, _) = row.values()[0].as_map().unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(&*keys[0], "value");
    }
}
