//! The native aggregate reducer must retain its input's element source and
//! distinguish unavailable/failed reads from a legitimate absent property.
use fgdb_delta_types::{ElementId, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryError, GqlQueryExecution,
    GqlQueryPolicy, GraphAggregateError, GraphAggregateRow, GraphExactAverage, GraphSymbol,
    GraphSymbolKind, PreparedGraphPipelineAggregateText, PreparedGraphSetAggregate,
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
    PreparedGraphPipelineAggregateText::prepare(&format!("{INPUT}{tail}"), |kind, name| {
        match (kind, name) {
            (GraphSymbolKind::Property, "value") => Some(GraphSymbol::Property(VALUE)),
            _ => None,
        }
    })
    .unwrap()
    .bind_relation_parameters(&GqlParameters::new())
    .unwrap()
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
    assert!(remaining.rows.max_result_rows().is_none_or(|limit| limit >= 3));
    Ok(GqlQueryExecution {
        value: [VId(1), VId(1), HIGH]
            .map(|id| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id)]))
            .into(),
        rows: GqlExecutionStats { snapshot_records: 3, result_rows: 3 },
        evaluator: GlaExecutionStats { work_units: 7, scratch_entries: 3 },
    })
}

fn property(element: ElementId, key: PropertyKeyId) -> Result<Option<CanonicalScalar>, &'static str> {
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
        let suffix = if collect { ", COLLECT(value) AS values" } else { "" };
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
                    [2, 2, 7].map(|n| GraphValue::Scalar(CanonicalScalar::Int(n))).into()
                ))
            );
        }
        assert_eq!(query.canonical_bytes(), bytes);
    }
}

#[test]
fn missing_properties_are_null_but_an_unavailable_reader_is_not_null() {
    let query = prepare("RETURN COUNT(*) AS rows, COUNT(value) AS present, SUM(value) AS total");
    let result = query.execute_governed_with_elements(
        policy(), no_pattern, procedure, |_, _| Ok(None), || Ok(()),
    ).unwrap();
    let row = &result.value[0];
    assert_eq!(row.values()[0].as_count(), Some(3));
    assert_eq!(row.values()[1].as_count(), Some(0));
    assert!(row.values()[2].is_null());
    assert!(query.execute_governed_with_procedures(
        policy(), no_pattern, procedure, || Ok(()),
    ).is_err());
}

#[test]
fn late_reader_failure_and_every_checkpoint_refuse_even_under_limit_zero() {
    for suffix in ["", " LIMIT 0"] {
        let query = prepare(&format!("RETURN SUM(value) AS total{suffix}"));
        let calls = Cell::new(0);
        let error = query.execute_governed_with_elements(
            policy(), no_pattern, procedure,
            |element, key| {
                calls.set(calls.get() + 1);
                if element == ElementId::Vertex(HIGH) { Err("last property refused") }
                else { property(element, key) }
            }, || Ok(()),
        ).unwrap_err();
        assert!(calls.get() >= 2);
        assert!(format!("{error}").contains("last property refused"));
        let seen = Cell::new(0usize);
        let expected = query.execute_governed_with_elements(
            policy(), no_pattern, procedure, property,
            || { seen.set(seen.get() + 1); Ok(()) },
        ).unwrap();
        assert_eq!(expected.value.len(), usize::from(suffix.is_empty()));
        for cut in 1..=seen.get() {
            let mut at = 0;
            let result = query.execute_governed_with_elements(
                policy(), no_pattern, procedure, property,
                || { at += 1; if at == cut { Err(cut) } else { Ok(()) } },
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
            expected.rows.snapshot_records, 1,
            expected.evaluator.work_units, expected.evaluator.scratch_entries,
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
