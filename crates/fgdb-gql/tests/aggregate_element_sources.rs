//! The native aggregate reducer must retain its input's element source and
//! distinguish unavailable/failed reads from a legitimate absent property.
use fgdb_delta_types::{ElementId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionStats, GlaLimitDimension, GqlExecutionStats, GqlParameters, GqlQueryError,
    GqlQueryExecution, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow, GraphAggregateValue,
    GraphExactAverage, GraphIntegerError, GraphIntegerErrorKind, GraphSymbol, GraphSymbolKind,
    PreparedGraphAggregateText, PreparedGraphPipelineAggregateText, PreparedGraphSetAggregate,
    PreparedProcedureCall,
};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::Cell;

const VALUE: PropertyKeyId = PropertyKeyId(1);
const HIGH: VId = VId(u128::MAX);
const INPUT: &str = "CALL fixture.rows() YIELD node \
    UNWIND [node] AS item WITH item.value AS value ";
type Failure = GqlQueryError<GraphAggregateError<&'static str>, usize>;

fn prepare(tail: &str) -> PreparedGraphSetAggregate {
    prepare_with_input(INPUT, tail)
}

fn prepare_output(tail: &str) -> PreparedGraphSetAggregate {
    prepare_with_input("CALL fixture.rows() YIELD node ", tail)
}

fn prepare_with_input(input: &str, tail: &str) -> PreparedGraphSetAggregate {
    PreparedGraphPipelineAggregateText::prepare(&format!("{input}{tail}"), |kind, name| {
        match (kind, name) {
            (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
            _ => None,
        }
    })
    .unwrap()
    .bind_relation_parameters(&GqlParameters::new())
    .unwrap()
}

fn scalar_list(values: &[Option<i64>]) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::List(
        values
            .iter()
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .collect(),
    ))
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(3, 1, 1_000_000, 100_000)
}

fn no_pattern(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("an expression-produced element must use the admitted reader, not a graph scan")
}

fn procedure(
    call: &PreparedProcedureCall,
    _: &[GraphValue],
    remaining: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    assert_eq!((call.namespace(), call.name()), ("fixture", "rows"));
    assert_eq!(call.outputs(), ["node"]);
    assert!(
        remaining
            .rows
            .max_result_rows()
            .is_none_or(|limit| limit >= 3)
    );
    Ok(GqlQueryExecution {
        value: [VId(1), VId(1), HIGH]
            .map(|id| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id)]))
            .into(),
        rows: GqlExecutionStats {
            snapshot_records: 3,
            result_rows: 3,
        },
        evaluator: GlaExecutionStats {
            work_units: 7,
            scratch_entries: 3,
        },
    })
}

fn property(
    element: ElementId,
    key: PropertyKeyId,
) -> Result<Option<CanonicalScalar>, &'static str> {
    assert_eq!(key, VALUE);
    match element {
        ElementId::Vertex(VId(1)) => Ok(Some(CanonicalScalar::Int(2))),
        ElementId::Vertex(HIGH) => Ok(Some(CanonicalScalar::Int(7))),
        _ => Err("outside source"),
    }
}

fn run(
    query: &PreparedGraphSetAggregate,
    policy: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphAggregateRow>, Failure> {
    query.execute_governed_with_elements(policy, no_pattern, procedure, property, || Ok(()))
}

#[test]
fn folded_and_collection_inputs_read_elements_and_preserve_exact_occurrences() {
    for collect in [false, true] {
        let suffix = if collect {
            ", COLLECT(value) AS values"
        } else {
            ""
        };
        let query = prepare(&format!(
            "RETURN COUNT(*) AS n, COUNT(DISTINCT value) AS unique_values, \
             SUM(value) AS total, AVG(value) AS mean{suffix}"
        ));
        let bytes = query.canonical_bytes();
        let result = run(&query, policy()).unwrap();
        assert_eq!(result.rows.snapshot_records, 3);
        assert_eq!(result.rows.result_rows, 1);
        let row = &result.value[0];
        assert_eq!(row.values()[0].as_count(), Some(3));
        assert_eq!(row.values()[1].as_count(), Some(2));
        assert_eq!(row.values()[2].as_integer(), Some(11));
        assert_eq!(row.values()[3].as_average(), GraphExactAverage::new(11, 3));
        if collect {
            assert_eq!(
                row.values()[4].as_value(),
                Some(&GraphValue::List(
                    [2, 2, 7]
                        .map(|n| GraphValue::Scalar(CanonicalScalar::Int(n)))
                        .into()
                ))
            );
        }
        assert_eq!(query.canonical_bytes(), bytes);
    }
}

#[test]
fn missing_properties_are_null_but_an_unavailable_reader_is_not_null() {
    let query = prepare("RETURN COUNT(*) AS rows, COUNT(value) AS present, SUM(value) AS total");
    let result = query
        .execute_governed_with_elements(policy(), no_pattern, procedure, |_, _| Ok(None), || Ok(()))
        .unwrap();
    let row = &result.value[0];
    assert_eq!(row.values()[0].as_count(), Some(3));
    assert_eq!(row.values()[1].as_count(), Some(0));
    assert!(row.values()[2].is_null());
    assert!(
        query
            .execute_governed_with_procedures(policy(), no_pattern, procedure, || Ok(()),)
            .is_err()
    );
}

#[test]
fn late_reader_failure_and_every_checkpoint_refuse_even_under_limit_zero() {
    for suffix in ["", " LIMIT 0"] {
        let query = prepare(&format!("RETURN SUM(value) AS total{suffix}"));
        let calls = Cell::new(0);
        let error = query
            .execute_governed_with_elements(
                policy(),
                no_pattern,
                procedure,
                |element, key| {
                    calls.set(calls.get() + 1);
                    if element == ElementId::Vertex(HIGH) {
                        Err("last property refused")
                    } else {
                        property(element, key)
                    }
                },
                || Ok(()),
            )
            .unwrap_err();
        assert!(calls.get() >= 2);
        assert!(format!("{error}").contains("last property refused"));
        let seen = Cell::new(0usize);
        let expected = query
            .execute_governed_with_elements(policy(), no_pattern, procedure, property, || {
                seen.set(seen.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(expected.value.len(), usize::from(suffix.is_empty()));
        for cut in 1..=seen.get() {
            let mut at = 0;
            let result = query.execute_governed_with_elements(
                policy(),
                no_pattern,
                procedure,
                property,
                || {
                    at += 1;
                    if at == cut { Err(cut) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(n)) if n == cut));
        }
        assert_eq!(run(&query, policy()).unwrap().value, expected.value);
    }
}

#[test]
fn source_expression_and_reducer_work_share_exact_and_short_allowances() {
    for suffix in ["", ", COLLECT(value) AS values"] {
        let query = prepare(&format!("RETURN SUM(value) AS total{suffix}"));
        let expected = run(&query, policy()).unwrap();
        let exact = GqlQueryPolicy::new(
            expected.rows.snapshot_records,
            1,
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries,
        );
        let replay = run(&query, exact).unwrap();
        assert_eq!(replay.value, expected.value);
        assert_eq!(replay.rows, expected.rows);
        assert_eq!(replay.evaluator, expected.evaluator);
        for short in [
            GqlQueryPolicy::new(2, 1, 1_000_000, 100_000),
            GqlQueryPolicy::new(3, 0, 1_000_000, 100_000),
            GqlQueryPolicy::new(3, 1, expected.evaluator.work_units - 1, 100_000),
            GqlQueryPolicy::new(3, 1, 1_000_000, expected.evaluator.scratch_entries - 1),
        ] {
            assert!(run(&query, short).is_err());
        }
    }
}

#[test]
fn post_group_collected_elements_keep_duplicates_full_identities_and_first_occurrence_order() {
    let query = prepare_output(
        "RETURN COLLECT(node) AS ids, \
         [item IN COLLECT(node) | item.value] AS values, \
         [item IN COLLECT(DISTINCT node) | item.value] AS unique_values",
    );
    let bytes = query.canonical_bytes();
    let mut requested = Vec::new();
    let result = query
        .execute_governed_with_elements(
            policy(),
            no_pattern,
            procedure,
            |element, key| {
                requested.push(element);
                property(element, key)
            },
            || Ok(()),
        )
        .unwrap();
    assert_eq!(result.rows.snapshot_records, 3);
    assert_eq!(result.rows.result_rows, 1);
    assert_eq!(
        result.value[0].values(),
        &[
            GraphAggregateValue::Value(GraphValue::List(
                [VId(1), VId(1), HIGH].map(GraphValue::Vertex).into()
            )),
            scalar_list(&[Some(2), Some(2), Some(7)]),
            scalar_list(&[Some(2), Some(7)]),
        ]
    );
    assert_eq!(
        requested,
        [VId(1), VId(1), HIGH, VId(1), HIGH].map(ElementId::Vertex),
        "only post-group property expressions read the host, in result/list order"
    );
    assert_eq!(query.canonical_bytes(), bytes);
}

#[test]
fn post_group_missing_properties_are_null_and_unavailable_readers_keep_the_typed_error() {
    for suffix in ["", " LIMIT 0"] {
        let query = prepare_output(&format!(
            "RETURN [item IN COLLECT(node) | item.value] AS values{suffix}"
        ));
        let mut reads = 0;
        let result = query
            .execute_governed_with_elements(
                policy(),
                no_pattern,
                procedure,
                |element, key| {
                    assert_eq!(key, VALUE);
                    reads += 1;
                    // Both stored scalar null and an absent property produce
                    // null, retaining every collected element's occurrence.
                    Ok((element == ElementId::Vertex(HIGH)).then_some(CanonicalScalar::Null))
                },
                || Ok(()),
            )
            .unwrap();
        assert_eq!(reads, 3, "{suffix}");
        if suffix.is_empty() {
            assert_eq!(result.value[0].values(), &[scalar_list(&[None; 3])]);
        } else {
            assert!(result.value.is_empty());
        }
        assert!(matches!(
            query.execute_governed_with_procedures(policy(), no_pattern, procedure, || Ok(())),
            Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression {
                    column: 0,
                    error: GraphIntegerError {
                        kind: GraphIntegerErrorKind::IncompatibleOperands,
                        ..
                    },
                }
            ))
        ));
    }
}

#[test]
fn post_group_late_property_failure_and_every_checkpoint_refuse_without_a_result_prefix() {
    for suffix in ["", " LIMIT 0"] {
        let query = prepare_output(&format!(
            "RETURN [item IN COLLECT(node) | item.value] AS values{suffix}"
        ));
        let mut requested = Vec::new();
        let result = query.execute_governed_with_elements(
            policy(),
            no_pattern,
            procedure,
            |element, key| {
                requested.push(element);
                if element == ElementId::Vertex(HIGH) {
                    Err("last output property refused")
                } else {
                    property(element, key)
                }
            },
            || Ok(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphAggregateError::Source(
                "last output property refused"
            )))
        ));
        assert_eq!(requested, [VId(1), VId(1), HIGH].map(ElementId::Vertex));

        let calls = Cell::new(0);
        let expected = query
            .execute_governed_with_elements(policy(), no_pattern, procedure, property, || {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap();
        assert_eq!(expected.value.len(), usize::from(suffix.is_empty()));
        for stop in 1..=calls.get() {
            let at = Cell::new(0);
            let interrupted = Cell::new(false);
            let result = query.execute_governed_with_elements(
                policy(),
                no_pattern,
                procedure,
                |element, key| {
                    assert!(
                        !interrupted.get(),
                        "an interrupted output read cannot resume"
                    );
                    property(element, key)
                },
                || {
                    at.set(at.get() + 1);
                    if at.get() == stop {
                        interrupted.set(true);
                        Err(stop)
                    } else {
                        Ok(())
                    }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(at.get(), stop, "{suffix}, stop={stop}");
        }
        assert_eq!(run(&query, policy()).unwrap(), expected);
    }
}

#[test]
fn post_group_property_work_and_payload_share_the_original_source_and_result_allowances() {
    let query = prepare_output(
        "RETURN [item IN COLLECT(node) | item.value] AS values, \
         [item IN COLLECT(DISTINCT node) | item.value] AS unique_values",
    );
    let expected = run(&query, policy()).unwrap();
    let limits = [
        expected.rows.snapshot_records,
        expected.rows.result_rows,
        expected.evaluator.work_units,
        expected.evaluator.scratch_entries,
    ];
    assert_eq!((limits[0], limits[1]), (3, 1));
    assert!(
        limits[2] > 7 && limits[3] > 3,
        "the procedure and output both spend work"
    );
    assert_eq!(
        run(
            &query,
            GqlQueryPolicy::new(limits[0], limits[1], limits[2], limits[3])
        )
        .unwrap(),
        expected
    );
    for dimension in 0..4 {
        let mut short = limits;
        short[dimension] -= 1;
        let error = run(
            &query,
            GqlQueryPolicy::new(short[0], short[1], short[2], short[3]),
        )
        .unwrap_err();
        if dimension < 2 {
            assert!(matches!(error, GqlQueryError::Rows(_)), "{error:?}");
        } else {
            assert!(matches!(error, GqlQueryError::Evaluator(_)), "{error:?}");
        }
    }
}

#[test]
fn post_group_binary_payloads_reserve_each_occurrence_even_outside_the_output_page() {
    let bytes = vec![0xa5; 513];
    let payload = CanonicalScalar::bytes(bytes.clone()).unwrap();
    // Three procedure occurrences each retain all 513 supplied bytes. The
    // public 64-byte payload unit requires at least 3 * 9 extra scratch units;
    // this lower bound is independent of the evaluator's measured counters.
    let minimum_extra_scratch = 3 * u64::try_from(bytes.len().div_ceil(64)).unwrap();
    assert_eq!(minimum_extra_scratch, 27);
    for suffix in ["", " LIMIT 0"] {
        let query = prepare_output(&format!(
            "RETURN [item IN COLLECT(node) | item.value] AS values{suffix}"
        ));
        let execute = |value: &CanonicalScalar, allowance| {
            query.execute_governed_with_elements(
                allowance,
                no_pattern,
                procedure,
                |_, key| {
                    assert_eq!(key, VALUE);
                    Ok(Some(value.clone()))
                },
                || Ok(()),
            )
        };
        let small = execute(&CanonicalScalar::Int(2), policy()).unwrap();
        let large = execute(&payload, policy()).unwrap();
        assert!(
            large.evaluator.scratch_entries
                >= small.evaluator.scratch_entries + minimum_extra_scratch,
            "{suffix}: every retained binary occurrence spends its payload scratch"
        );
        if suffix.is_empty() {
            assert_eq!(
                large.value[0].values(),
                &[GraphAggregateValue::Value(GraphValue::List(
                    vec![GraphValue::Scalar(CanonicalScalar::bytes(bytes.clone()).unwrap()); 3]
                        .into()
                ))]
            );
        } else {
            assert!(large.value.is_empty());
        }
        let exact = GqlQueryPolicy::new(
            large.rows.snapshot_records,
            large.rows.result_rows,
            large.evaluator.work_units,
            large.evaluator.scratch_entries,
        );
        assert_eq!(execute(&payload, exact).unwrap(), large);
        let short = GqlQueryPolicy::new(
            large.rows.snapshot_records,
            large.rows.result_rows,
            large.evaluator.work_units,
            large.evaluator.scratch_entries - 1,
        );
        assert!(matches!(
            execute(&payload, short),
            Err(GqlQueryError::Evaluator(error))
                if error.dimension == GlaLimitDimension::ScratchEntries
        ));
    }
}

#[test]
fn native_output_element_reads_preserve_the_raw_collectors_input_order_and_empty_group() {
    let prepare_native = |text| {
        PreparedGraphAggregateText::prepare(text, |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
            _ => None,
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
    };
    let raw = prepare_native("MATCH (node) RETURN COLLECT(node) AS ids");
    let projected = prepare_native(
        "MATCH (node) RETURN COLLECT(node) AS ids, \
         [item IN COLLECT(node) | item.value] AS values",
    );
    for vertices in [vec![HIGH, VId(1)], vec![]] {
        let source_records = vertices.len() as u64;
        let collected = raw
            .execute_governed(
                source_records,
                vertices.iter().copied(),
                std::iter::empty::<(VId, RelationId, VId)>(),
                |_, _| Ok::<_, &'static str>(true),
                |_, _| Ok(None),
                policy(),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        let mut reads = Vec::new();
        let actual = projected
            .execute_governed_with_element_reads(
                source_records,
                vertices.iter().copied(),
                std::iter::empty::<(VId, RelationId, VId)>(),
                |_, _| Ok(true),
                |_, _| Ok(None),
                |element, key| {
                    reads.push(element);
                    property(element, key)
                },
                policy(),
                || Ok::<_, usize>(()),
            )
            .unwrap();
        assert_eq!(actual.rows.snapshot_records, source_records);
        assert_eq!(actual.rows.result_rows, 1);
        assert_eq!(actual.value[0].values()[0], collected.value[0].values()[0]);
        assert_eq!(
            collected.value[0].values()[0],
            GraphAggregateValue::Value(GraphValue::List(
                vertices.iter().copied().map(GraphValue::Vertex).collect()
            )),
            "native binding visitation must not sort the projected property values"
        );
        let values: Vec<_> = vertices
            .iter()
            .map(|vid| if *vid == HIGH { Some(7) } else { Some(2) })
            .collect();
        assert_eq!(actual.value[0].values()[1], scalar_list(&values));
        assert_eq!(
            reads,
            vertices
                .into_iter()
                .map(ElementId::Vertex)
                .collect::<Vec<_>>()
        );
    }
}
