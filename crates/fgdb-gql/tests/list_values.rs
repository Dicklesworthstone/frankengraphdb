//! Ordered, heterogeneous list values retain element identities and boundaries.
use fgdb_gql::algebra::GraphValue;
use fgdb_types::CanonicalScalar;

fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

#[test]
fn nested_list_encoding_preserves_order_nulls_and_boundaries() {
    let list = GraphValue::List(vec![
        int(7),
        GraphValue::Scalar(CanonicalScalar::Null),
        GraphValue::List(vec![int(2), int(3)].into_boxed_slice()),
    ].into_boxed_slice());
    assert_eq!(list.as_list().unwrap()[0], int(7));
    assert!(list.as_list().unwrap()[1].is_null());
    assert_eq!(list.canonical_bytes().unwrap(), list.clone().canonical_bytes().unwrap());
    let reordered = GraphValue::List(vec![
        GraphValue::Scalar(CanonicalScalar::Null), int(7),
        GraphValue::List(vec![int(2), int(3)].into_boxed_slice()),
    ].into_boxed_slice());
    let flattened = GraphValue::List(vec![int(7),
        GraphValue::Scalar(CanonicalScalar::Null), int(2), int(3),
    ].into_boxed_slice());
    assert_ne!(list.canonical_bytes().unwrap(), reordered.canonical_bytes().unwrap());
    assert_ne!(list.canonical_bytes().unwrap(), flattened.canonical_bytes().unwrap());
    assert_ne!(GraphValue::List(Box::new([])).canonical_bytes().unwrap(),
        GraphValue::Scalar(CanonicalScalar::Null).canonical_bytes().unwrap());
}

#[test]
fn list_parameters_retain_type_and_reject_scalar_binding() {
    use fgdb_gql::{GqlParameterType, GqlParameters, PreparedGraphSetText};
    let template = PreparedGraphSetText::prepare_with_parameter_types(
        "MATCH (n) RETURN $xs AS xs", &[("xs", GqlParameterType::List)], |_, _| None,
    ).unwrap();
    assert_eq!(template.parameter_schema()[0].parameter_type, GqlParameterType::List);
    let arguments = GqlParameters::new().with_list("xs", vec![int(1),
        GraphValue::List(vec![int(2)].into_boxed_slice())]).unwrap();
    let bound = template.bind_parameters(&arguments).unwrap();
    let other = GqlParameters::new().with_list("xs", vec![int(1),int(2)]).unwrap();
    assert_ne!(bound.canonical_bytes(), template.bind_parameters(&other).unwrap().canonical_bytes());
    assert_ne!(arguments.canonical_bytes(), other.canonical_bytes());
    assert!(template.bind_parameters(&GqlParameters::new().with_int64("xs", 1).unwrap()).is_err());
}

fn execute(text: &str, policy: fgdb_gql::GqlQueryPolicy) -> Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<()>, ()>,
> {
    let prepared = fgdb_gql::PreparedGraphSetText::prepare(text, |_, _| None).unwrap()
        .bind_parameters(&fgdb_gql::GqlParameters::new()).unwrap();
    prepared.execute_governed(policy, |pattern, budget| {
        pattern.plan().execute_governed_with_properties(
            1, [fgdb_types::VId(1)], [], |_, _| Ok::<_, ()>(true),
            |_, _| Ok(None), budget, || Ok::<_, ()>(()),
        )
    }, || Ok::<_, ()>(()))
}

#[test]
fn list_growth_and_unwind_fanout_cannot_hide_behind_limit_zero() {
    use fgdb_gql::{GlaLimitDimension, GqlQueryError, GqlQueryPolicy};
    let wide = (0..256).map(|n| n.to_string()).collect::<Vec<_>>().join(",");
    let ample = GqlQueryPolicy::new(100, 1000, 1_000_000, 1_000_000);
    let small_list = "MATCH (n) RETURN [] AS xs LIMIT 0";
    let large_list = format!("MATCH (n) RETURN [{wide}] AS xs LIMIT 0");
    let baseline = execute(small_list, ample).unwrap().evaluator.scratch_entries;
    let tight = GqlQueryPolicy::new(100, 1000, 1_000_000, baseline + 32);
    assert!(execute(small_list, tight).unwrap().value.is_empty());
    assert!(matches!(execute(&large_list, tight), Err(GqlQueryError::Evaluator(error))
        if error.dimension == GlaLimitDimension::ScratchEntries));
    let small_unwind = "MATCH (n) WITH [1] AS xs UNWIND xs AS x RETURN x LIMIT 0";
    let large_unwind = format!("MATCH (n) WITH [{wide}] AS xs UNWIND xs AS x RETURN x LIMIT 0");
    let construction = format!("MATCH (n) RETURN [{wide}] AS xs LIMIT 0");
    let construction_cost = execute(&construction, ample).unwrap().evaluator.scratch_entries;
    let tight = GqlQueryPolicy::new(100, 1000, 1_000_000, construction_cost + 64);
    assert!(execute(&construction, tight).unwrap().value.is_empty());
    assert!(execute(small_unwind, tight).unwrap().value.is_empty());
    assert!(matches!(execute(&large_unwind, tight), Err(GqlQueryError::Evaluator(error))
        if error.dimension == GlaLimitDimension::ScratchEntries));
}
