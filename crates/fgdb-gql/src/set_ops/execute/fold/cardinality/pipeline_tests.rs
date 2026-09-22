use super::*;
use crate::{GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp};
use fgdb_types::CanonicalScalar;

fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn no_source(
    _: &PreparedGraphPattern<GraphValueRow>,
    _: GqlQueryPolicy,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<&'static str, usize>> {
    panic!("source-free relation opened a graph")
}
fn scalar(n: i64) -> GraphSetValue {
    GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))
}
fn list(n: usize) -> GraphSetValue {
    GraphSetValue::List((0..n).map(|i| scalar(i as i64)).collect())
}
fn divide_zero() -> GraphSetValue {
    GraphSetValue::Integer(GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Literal(Some(1)), GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
    ]).unwrap())
}
fn eager(query: &PreparedGraphSet) -> SetResult<GqlQueryExecution<GraphValueRow>, &'static str, usize> {
    query.execute_governed(wide(), no_source, || Ok(()))
}
fn counted(query: &PreparedGraphSet) -> SetResult<Option<u64>, &'static str, usize> {
    query.count_governed(wide(), no_source, || Ok(())).map(|(count, _, _)| count)
}

#[test]
fn nested_constant_unwind_with_aliases_reduces_without_generating_trillion_rows() {
    let mut input = PreparedGraphSet::singleton();
    for at in 0..8 {
        input = input.unwind(format!("x{at}"), list(32)).unwrap();
    }
    let query = input.project(vec![
        GraphSetProjection::new("last", GraphSetValue::Column(7)),
        GraphSetProjection::new("first", GraphSetValue::Column(0)),
        GraphSetProjection::new("literal", scalar(9)),
    ], GraphSetQuantifier::All).unwrap();
    assert!(query.has_factorized_cardinality());
    let (count, rows, used) = query.count_governed(
        GqlQueryPolicy::new(0, 0, 20_000, 5_000), no_source, || Ok(()),
    ).unwrap();
    assert_eq!(count, Some(1_u64 << 40));
    assert_eq!(rows.result_rows, 0);
    assert!(used.work_units < 20_000);
}

#[test]
fn constant_definitions_do_not_share_cached_cardinality_and_keep_each_local_page() {
    for first in 0..5 {
        for second in 0..5 {
            for skip in [0, 1, u64::MAX] {
                for limit in [None, Some(0), Some(3)] {
                    let query = PreparedGraphSet::singleton()
                        .unwind("a".into(), list(first)).unwrap().with_page(1, Some(3))
                        .unwind("b".into(), list(second)).unwrap()
                        .project(vec![GraphSetProjection::new("a", GraphSetValue::Column(0))],
                            GraphSetQuantifier::All).unwrap().with_page(skip, limit);
                    assert_eq!(counted(&query).unwrap(), Some(eager(&query).unwrap().value.len() as u64));
                }
            }
        }
    }
}

#[test]
fn constant_expressions_are_evaluated_not_replaced_by_syntactic_list_length() {
    let bad_list = GraphSetValue::List(vec![scalar(1), divide_zero()]);
    for input_count in [0, 1, 3] {
        for page in [0, 1] {
            let input = PreparedGraphSet::singleton().unwind("a".into(), list(input_count)).unwrap();
            for value in [bad_list.clone(), scalar(7), divide_zero()] {
                let query = input.clone().unwind("b".into(), value).unwrap().with_page(0, Some(page));
                match eager(&query) {
                    Ok(expected) => assert_eq!(counted(&query).unwrap(), Some(expected.value.len() as u64)),
                    Err(error) => assert_eq!(counted(&query).unwrap_err(), error),
                }
            }
        }
    }
    // The existing list-index interpreter, not a duplicate constant evaluator.
    let indexed = GraphSetValue::Index {
        list: Box::new(GraphSetValue::List(vec![list(2), list(5)])),
        index: Box::new(scalar(1)),
    };
    let query = PreparedGraphSet::singleton().unwind("x".into(), indexed).unwrap();
    assert_eq!(counted(&query).unwrap(), Some(5));
}

#[test]
fn fallible_projections_and_dependent_unwind_remain_value_sensitive() {
    let input = PreparedGraphSet::singleton().unwind("a".into(), list(3)).unwrap();
    let bad = input.clone().project(vec![GraphSetProjection::new("bad", divide_zero())],
        GraphSetQuantifier::All).unwrap().with_page(0, Some(0));
    assert!(!bad.has_factorized_cardinality());
    assert_eq!(counted(&bad).unwrap_err(), eager(&bad).unwrap_err());
    let nested = PreparedGraphSet::singleton().unwind("lists".into(),
        GraphSetValue::List(vec![list(1), list(3), list(0)]),
    ).unwrap().unwind("item".into(), GraphSetValue::Column(0)).unwrap();
    assert!(!nested.has_factorized_cardinality());
    assert_eq!(counted(&nested).unwrap(), Some(4));
    let list_projection = input.project(vec![GraphSetProjection::new("nested",
        GraphSetValue::List(vec![GraphSetValue::Column(0)])),
    ], GraphSetQuantifier::All).unwrap();
    assert!(!list_projection.has_factorized_cardinality());
}

#[test]
fn upstream_failure_precedes_constant_failure_even_when_output_page_is_empty() {
    let input = PreparedGraphSet::singleton().unwind("lists".into(),
        GraphSetValue::List(vec![list(1), scalar(7)]),
    ).unwrap().unwind("item".into(), GraphSetValue::Column(0)).unwrap();
    let query = input.unwind("bad".into(), GraphSetValue::List(vec![divide_zero()]))
        .unwrap().with_page(0, Some(0));
    let error = counted(&query).unwrap_err();
    assert!(matches!(&error, GqlQueryError::Source(GraphSetExecutionError::Projection {
        row: 1, column: 1, ..
    })));
    assert_eq!(error, eager(&query).unwrap_err());
}
