use super::*;
use crate::csv_parameters::{CsvParameterError, CsvParameterErrorKind, CsvParameterLimit};
use crate::{
    GlaLimitDimension, GqlParameterType, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol,
    GraphSymbolKind, GraphWriteChainStep,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::CanonicalScalarKind;

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "N") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Relation, "E") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}

fn template(prefix: &str) -> PreparedGraphWriteChainText {
    PreparedGraphWriteChainText::prepare_with_parameter_types(
        &format!("UNWIND $rows AS row {prefix} MERGE (a:N {{id:row.src}}) MERGE (b:N {{id:row.dst}}) MERGE (a)-[:E]->(b)"),
        RelationId(1),
        &[("rows", GqlParameterType::List)],
        symbols,
    ).unwrap()
}

fn schema(kind: GqlParameterType) -> Vec<GqlParameterSpec> {
    ["src", "dst"]
        .into_iter()
        .map(|name| GqlParameterSpec {
            name: name.into(),
            parameter_type: kind,
            requires_positive: false,
            occurrences: 1,
        })
        .collect()
}

fn bind(
    template: &PreparedGraphWriteChainText,
    schema: &[GqlParameterSpec],
    input: &[u8],
    chunk: usize,
    limits: CsvParameterLimits,
) -> Result<(PreparedGraphWriteChain, usize), GraphWriteChainCsvStreamError<&'static str>> {
    let mut at = 0;
    template.bind_csv_rows_stream_controlled(
        "rows",
        schema,
        limits,
        |buffer| {
            let count = buffer.len().min(chunk).min(input.len() - at);
            buffer[..count].copy_from_slice(&input[at..at + count]);
            at += count;
            Ok(count)
        },
        || Ok(()),
    )
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}

#[test]
fn row_relation_accepts_22000_two_column_records_without_widening_list_bounds() {
    let template = template("");
    let mut input = String::from("src,dst\n");
    for row in 0..22_000 {
        input.push_str(&format!("{row},{}\n", row + 1));
    }
    let (chain, records) = bind(
        &template,
        &schema(GqlParameterType::Int64),
        input.as_bytes(),
        79,
        CsvParameterLimits {
            max_records: CsvParameterLimits::HARD.max_records,
            ..CsvParameterLimits::default()
        },
    )
    .unwrap();
    assert_eq!(records, 22_000);
    let rows = chain.initial_rows.as_ref().unwrap();
    assert_eq!(rows.len(), records);
    // The former representation still correctly refuses the same data.
    let scalar_list = rows.iter().map(|row| row.values()[0].clone()).collect();
    assert!(GqlParameters::new().with_list("rows", scalar_list).is_err());
    assert!(rows.iter().all(|row| row.values()[0].validate_bounds()));
}

#[test]
fn every_chunk_boundary_preserves_unicode_quotes_nulls_and_record_order() {
    let template = template("");
    let schema = schema(GqlParameterType::Scalar(CanonicalScalarKind::Text));
    let input = "\u{feff}dst,src\r\n\"snow 雪🙂, \"\"quoted\"\"\r\nline\",z\r\n\\N,a\r\n\"\\N\",b\r\n\"\",c\r\nlast,d";
    let expected =
        crate::csv_parameters::decode_csv_parameters(input, &schema, CsvParameterLimits::default())
            .unwrap();
    for chunk in 1..=input.len() + 1 {
        let (chain, count) = bind(
            &template,
            &schema,
            input.as_bytes(),
            chunk,
            CsvParameterLimits::default(),
        )
        .unwrap();
        assert_eq!(count, expected.len());
        for (row, parameters) in chain.initial_rows.as_ref().unwrap().iter().zip(&expected) {
            let (keys, values) = row.values()[0].as_map().unwrap();
            for (key, actual) in keys.iter().zip(values) {
                let Some(GqlParameterValue::Scalar(expected)) = parameters.get(key) else {
                    panic!("text schema")
                };
                assert_eq!(
                    actual,
                    &GraphValue::Scalar(expected.value().clone()),
                    "chunk {chunk}"
                );
            }
        }
    }
}

#[test]
fn remaining_with_and_unwind_stages_keep_source_occurrence_order() {
    for (prefix, repetitions) in [("WITH row", 1), ("UNWIND [9, 7] AS repetition WITH row", 2)] {
        let (chain, _) = bind(
            &template(prefix),
            &schema(GqlParameterType::Int64),
            b"src,dst\n3,30\n1,10\n2,20\n",
            3,
            CsvParameterLimits::default(),
        )
        .unwrap();
        let initial = chain
            .initial_rows_governed::<(), ()>(policy(), || Ok(()))
            .unwrap()
            .unwrap();
        let GraphWriteChainStep::VertexMerge(merge) = &chain.steps()[0] else {
            panic!("first MERGE")
        };
        let selected = merge
            .select_governed::<(), (), ()>(
                Some(initial.value),
                policy(),
                |_, _| panic!("no graph source"),
                || Ok(()),
            )
            .unwrap();
        let sources: Vec<_> = selected
            .value
            .iter()
            .map(|row| {
                let (keys, values) = row.values()[0].as_map().unwrap();
                let at = keys.iter().position(|key| key.as_ref() == "src").unwrap();
                values[at].as_scalar().unwrap().clone()
            })
            .collect();
        let expected: Vec<_> = [3, 1, 2]
            .into_iter()
            .flat_map(|n| std::iter::repeat_n(CanonicalScalar::Int(n), repetitions))
            .collect();
        assert_eq!(sources, expected, "{prefix}");
    }
}

#[test]
fn input_copy_spends_cumulative_work_and_scratch_before_rows_are_exposed() {
    let (chain, _) = bind(
        &template(""),
        &schema(GqlParameterType::Int64),
        b"src,dst\n2,3\n1,2",
        2,
        CsvParameterLimits::default(),
    )
    .unwrap();
    let admitted = chain
        .initial_rows_governed::<(), ()>(policy(), || Ok(()))
        .unwrap()
        .unwrap();
    assert!(admitted.evaluator.work_units > 0 && admitted.evaluator.scratch_entries > 0);
    assert_eq!(admitted.rows.result_rows, 0);
    for dimension in [
        GlaLimitDimension::WorkUnits,
        GlaLimitDimension::ScratchEntries,
    ] {
        let mut allowance = policy();
        allowance.evaluator.max_work_units = admitted.evaluator.work_units;
        allowance.evaluator.max_scratch_entries = admitted.evaluator.scratch_entries;
        match dimension {
            GlaLimitDimension::WorkUnits => allowance.evaluator.max_work_units -= 1,
            GlaLimitDimension::ScratchEntries => allowance.evaluator.max_scratch_entries -= 1,
        }
        assert!(
            matches!(chain.initial_rows_governed::<(), ()>(allowance, || Ok(())), Err(GqlQueryError::Evaluator(error)) if error.dimension == dimension)
        );
    }
    assert!(matches!(
        chain.initial_rows_governed::<(), _>(policy(), || Err("cancelled")),
        Err(GqlQueryError::Interrupted("cancelled"))
    ));
}

#[test]
fn late_source_error_bad_record_and_record_limit_return_no_bound_prefix() {
    let template = template("");
    let schema = schema(GqlParameterType::Int64);
    let mut read_once = false;
    let error = template
        .bind_csv_rows_stream_controlled(
            "rows",
            &schema,
            CsvParameterLimits::default(),
            |buffer| {
                if read_once {
                    return Err("source lost");
                }
                read_once = true;
                let prefix = b"src,dst\n1,2\n3,4\n";
                buffer[..prefix.len()].copy_from_slice(prefix);
                Ok(prefix.len())
            },
            || Ok(()),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        GraphWriteScriptCsvStreamError::Source("source lost")
    ));
    let bad = b"src,dst\n1,2\n3,4\n5,broken\n";
    assert!(matches!(
        bind(&template, &schema, bad, 1, CsvParameterLimits::default()),
        Err(GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
            record: 3,
            kind: CsvParameterErrorKind::InvalidValue,
            ..
        }))
    ));
    assert!(matches!(
        bind(
            &template,
            &schema,
            bad,
            1,
            CsvParameterLimits {
                max_records: 2,
                ..CsvParameterLimits::default()
            }
        ),
        Err(GraphWriteScriptCsvStreamError::Csv(CsvParameterError {
            kind: CsvParameterErrorKind::Limit {
                dimension: CsvParameterLimit::Records,
                limit: 2,
                observed: 3
            },
            ..
        }))
    ));
}

#[test]
fn invalid_source_definition_refuses_before_reading_and_uint64_never_wraps() {
    let template = template("");
    let result = template.bind_csv_rows_stream_controlled(
        "other",
        &schema(GqlParameterType::Int64),
        CsvParameterLimits::default(),
        |_| -> Result<usize, ()> { panic!("definition before source") },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GraphWriteScriptCsvStreamError::Binding(
            GraphWriteChainCsvBindingError::Definition(_)
        ))
    ));
    // The chain schema retains a shared parameter's first occurrence count;
    // a later clause reading the source parameter must still refuse.
    let reused = PreparedGraphWriteChainText::prepare_with_parameter_types(
        "UNWIND $rows AS row MERGE (a:N {id:row.src}) MERGE (b:N {id:size($rows)})",
        RelationId(1),
        &[("rows", GqlParameterType::List)],
        symbols,
    )
    .unwrap();
    let result = reused.bind_csv_rows_stream_controlled(
        "rows",
        &schema(GqlParameterType::Int64),
        CsvParameterLimits::default(),
        |_| -> Result<usize, ()> { panic!("reused source refuses before reading") },
        || Ok(()),
    );
    assert!(matches!(
        result,
        Err(GraphWriteScriptCsvStreamError::Binding(
            GraphWriteChainCsvBindingError::Definition(_)
        ))
    ));
    let bad = b"src,dst\n1,2\n3,18446744073709551615";
    assert!(matches!(
        bind(
            &template,
            &schema(GqlParameterType::UInt64),
            bad,
            1,
            CsvParameterLimits::default()
        ),
        Err(GraphWriteScriptCsvStreamError::Binding(
            GraphWriteChainCsvBindingError::RowValue {
                record: 2,
                column: 1
            }
        ))
    ));
}
