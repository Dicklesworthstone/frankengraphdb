use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{
    GraphColumn, GraphOrderError, GraphPatternBuilder, GraphValue, GraphValueOrder, GraphValueRow,
    PreparedGraphPattern, ValueProjection,
};
use fgdb_gql::{
    GqlBudgetDimension, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphAggregate, GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression,
    GraphIntegerOp, GraphSetBuildError, GraphSetColumnType, GraphSetExecutionError,
    GraphSetOperation as Op, GraphSetProjection, GraphSetQuantifier as Q, GraphSetValue,
    MAX_GRAPH_SET_OPERANDS, PreparedGraphSet, PreparedGraphSetAggregate,
};
use fgdb_types::{CanonicalF64, CanonicalPropertyValue, CanonicalScalar, EId, VId};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;

// Public Apply contracts belong beside the other public relational contracts.
// Private frame cleanup on interruption remains an executor unit test.
mod apply_tests {
    use super::*;
    use fgdb_gql::{GqlParameters, PreparedGraphText};

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }
    fn input(values: Vec<GraphValue>) -> PreparedGraphSet {
        PreparedGraphSet::singleton()
            .unwind("outer".into(), GraphSetValue::Value(list(values)))
            .unwrap()
    }
    fn arguments() -> PreparedGraphSet {
        PreparedGraphSet::arguments(vec!["arg".into()], vec![GraphSetColumnType::Any]).unwrap()
    }
    fn execute(
        query: &PreparedGraphSet,
        policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<()>, ()>>
    {
        query.execute_governed(policy, |_, _| Err(GqlQueryError::Source(())), || Ok(()))
    }
    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(0, 10_000, 1_000_000, 1_000_000)
    }
    fn plain(query: &PreparedGraphSet) -> Vec<Vec<GraphValue>> {
        execute(query, policy())
            .unwrap()
            .value
            .iter()
            .map(|row| row.values().to_vec())
            .collect()
    }
    fn echo() -> PreparedGraphSet {
        arguments()
            .project(
                vec![GraphSetProjection::new("echo", GraphSetValue::Column(0))],
                Q::All,
            )
            .unwrap()
    }

    #[test]
    fn apply_preserves_bags_null_imports_and_empty_inner_lists() {
        let values = vec![
            list(vec![int(2), int(1), int(1)]),
            list(vec![]),
            GraphValue::Scalar(CanonicalScalar::Null),
            list(vec![int(9)]),
            list(vec![int(9)]),
        ];
        let mut expected = Vec::new();
        for outer in &values {
            if let GraphValue::List(elements) = outer {
                for element in elements.iter() {
                    expected.push(vec![outer.clone(), element.clone()]);
                }
            }
        }
        let body = arguments()
            .unwind("item".into(), GraphSetValue::Column(0))
            .unwrap()
            .project(
                vec![GraphSetProjection::new("value", GraphSetValue::Column(1))],
                Q::All,
            )
            .unwrap();
        assert_eq!(
            plain(&input(values).apply(body, vec![0]).unwrap()),
            expected
        );
        let nullable = input(vec![
            int(2),
            int(2),
            GraphValue::Scalar(CanonicalScalar::Null),
        ])
        .apply(echo(), vec![0])
        .unwrap();
        assert_eq!(
            plain(&nullable),
            vec![
                vec![int(2), int(2)],
                vec![int(2), int(2)],
                vec![GraphValue::Scalar(CanonicalScalar::Null); 2],
            ]
        );
    }

    #[test]
    fn apply_runs_the_inner_page_for_each_occurrence() {
        let body = arguments()
            .unwind("item".into(), GraphSetValue::Column(0))
            .unwrap()
            .project(
                vec![GraphSetProjection::new("value", GraphSetValue::Column(1))],
                Q::All,
            )
            .unwrap()
            .with_page(1, Some(1));
        let left = vec![
            list(vec![int(4), int(5), int(6)]),
            list(vec![int(8), int(9)]),
            list(vec![int(0)]),
        ];
        assert_eq!(
            plain(&input(left.clone()).apply(body, vec![0]).unwrap()),
            vec![vec![left[0].clone(), int(5)], vec![left[1].clone(), int(9)]]
        );
    }

    #[test]
    fn nested_apply_restores_the_enclosing_argument_frame() {
        let inner_left = arguments()
            .unwind(
                "local".into(),
                GraphSetValue::Value(list(vec![int(7), int(8)])),
            )
            .unwrap();
        let nested = inner_left
            .apply(echo(), vec![1])
            .unwrap()
            .cross_join(arguments())
            .unwrap();
        let query = input(vec![int(1), int(2)]).apply(nested, vec![0]).unwrap();
        let expected: Vec<_> = [1, 2]
            .into_iter()
            .flat_map(|outer| {
                [7, 8].into_iter().map(move |local| {
                    vec![int(outer), int(outer), int(local), int(local), int(outer)]
                })
            })
            .collect();
        assert_eq!(plain(&query), expected);
    }

    #[test]
    fn apply_checks_import_positions_and_complete_argument_domains() {
        assert!(matches!(
            input(vec![]).apply(echo(), vec![1]),
            Err(GraphSetBuildError::UnknownArgument { input: 1, .. })
        ));
        let scalar =
            PreparedGraphSet::arguments(vec!["arg".into()], vec![GraphSetColumnType::Scalar])
                .unwrap();
        assert!(matches!(
            input(vec![]).apply(scalar, vec![0]),
            Err(GraphSetBuildError::ColumnType { .. })
        ));
        assert!(matches!(
            input(vec![]).apply(echo(), vec![]),
            Err(GraphSetBuildError::ColumnCount { .. })
        ));
        assert!(
            PreparedGraphSet::arguments(
                vec!["x".into(), "x".into()],
                vec![GraphSetColumnType::Any; 2]
            )
            .is_err()
        );
    }

    /// Arguments read only inside an Apply. The spill planner admits Apply
    /// and typed Arguments as a complete external plan since c1578a023
    /// (fgdb-694ga); before, it refused them, which this law used to pin.
    #[test]
    fn arguments_require_an_apply_owner_which_the_spill_planner_admits() {
        assert!(matches!(
            execute(&arguments(), policy()),
            Err(GqlQueryError::Source(
                GraphSetExecutionError::ArgumentsUnavailable
            ))
        ));
        let query = input(vec![int(1)]).apply(echo(), vec![0]).unwrap();
        assert!(!query.reads_graph() && !query.calls_procedure());
        assert!(fgdb_gql::spill_set::AsyncSpillSetPlan::compile(&query).is_ok());
    }

    #[test]
    fn apply_spends_one_cumulative_allowance_and_enforces_final_row_limit() {
        let left = input((0..32).map(int).collect());
        let left_work = execute(&left, policy()).unwrap().evaluator.work_units;
        let query = left.apply(echo(), vec![0]).unwrap();
        assert_eq!(plain(&query).len(), 32);
        // Room for the complete left plus one body, not 32 body allowances.
        let bounded = GqlQueryPolicy::new(0, 100, left_work + 40, 1_000_000);
        assert!(matches!(
            execute(&query, bounded),
            Err(GqlQueryError::Evaluator(_))
        ));
        assert!(matches!(
            execute(&query, GqlQueryPolicy::new(0, 31, 1_000_000, 1_000_000)),
            Err(GqlQueryError::Rows(_))
        ));
    }

    #[test]
    fn empty_left_never_invokes_the_body_source_but_nonempty_left_does() {
        let pattern = PreparedGraphText::prepare("MATCH (n) RETURN n", |_, _| None)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        for (count, expected_calls) in [(0, 0), (1, 1)] {
            let query = PreparedGraphSet::singleton()
                .with_page(0, Some(count))
                .apply(pattern.clone().into(), vec![])
                .unwrap();
            let mut calls = 0;
            let result = query.execute_governed(
                policy(),
                |_, _| {
                    calls += 1;
                    Err::<GqlQueryExecution<GraphValueRow>, _>(GqlQueryError::<(), ()>::Source(()))
                },
                || Ok(()),
            );
            assert_eq!(calls, expected_calls);
            if count == 0 {
                assert!(result.unwrap().value.is_empty());
            } else {
                assert!(matches!(result, Err(GqlQueryError::Source(_))));
            }
        }
    }

    #[test]
    fn apply_transcript_binds_import_order_and_differs_from_cross_join() {
        let left = input(vec![int(1)])
            .unwind("second".into(), GraphSetValue::Value(list(vec![int(2)])))
            .unwrap();
        let a = left.clone().apply(echo(), vec![0]).unwrap();
        let b = left.clone().apply(echo(), vec![1]).unwrap();
        let cross = left.cross_join(echo()).unwrap();
        assert_ne!(a.canonical_bytes(), b.canonical_bytes());
        assert_ne!(a.canonical_bytes(), cross.canonical_bytes());
        assert_eq!(a.canonical_bytes(), a.clone().canonical_bytes());
    }
}

fn pattern(key: u64, alias: &str) -> PreparedGraphPattern<GraphValueRow> {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .prepare_values(
            &[GraphColumn::property(alias, "n", PropertyKeyId(key))],
            0,
            None,
        )
        .unwrap()
        .with_duplicates()
}
fn query(operation: Op, quantifier: Q) -> PreparedGraphSet {
    PreparedGraphSet::from(pattern(1, "value"))
        .combine(
            operation,
            quantifier,
            PreparedGraphSet::from(pattern(2, "other_name")),
        )
        .unwrap()
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
type ResultRows = Result<
    GqlQueryExecution<GraphValueRow>,
    GqlQueryError<GraphSetExecutionError<&'static str>, usize>,
>;
fn run(
    query: &PreparedGraphSet,
    left: &[CanonicalScalar],
    right: &[CanonicalScalar],
    policy: GqlQueryPolicy,
    checkpoint: impl FnMut() -> Result<(), usize>,
) -> ResultRows {
    let checkpoint = RefCell::new(checkpoint);
    let stored = |values: &[CanonicalScalar]| -> Vec<CanonicalPropertyValue> {
        values
            .iter()
            .map(|value| CanonicalPropertyValue::from(value.clone()))
            .collect()
    };
    let (left, right) = (stored(left), stored(right));
    query.execute_governed(
        policy,
        |pattern, allowance| {
            let ValueProjection::Property { key, .. } = pattern.value_columns()[0] else {
                panic!("scalar fixture")
            };
            let values = if key == PropertyKeyId(1) {
                &left
            } else {
                &right
            };
            pattern.plan().execute_governed_with_properties(
                values.len() as u64,
                (0..values.len()).map(|at| VId(at as u128)),
                [],
                |_, _| Ok::<_, &'static str>(true),
                |vid, _| Ok(Some(&values[vid.0 as usize])),
                allowance,
                || (checkpoint.borrow_mut())(),
            )
        },
        || (checkpoint.borrow_mut())(),
    )
}
fn scalars(result: &GqlQueryExecution<GraphValueRow>) -> Vec<CanonicalScalar> {
    result
        .value
        .iter()
        .map(|row| row.get(0).unwrap().as_scalar().unwrap().clone())
        .collect()
}
fn constant(value: GraphValue) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .project(
            vec![GraphSetProjection::new(
                "value",
                GraphSetValue::Value(value),
            )],
            Q::All,
        )
        .unwrap()
}
fn dynamic(values: &[GraphValue]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "value".into(),
            GraphSetValue::List(values.iter().cloned().map(GraphSetValue::Value).collect()),
        )
        .unwrap()
}

#[test]
fn six_operations_use_complete_gla_bags_and_null_set_equality() {
    use CanonicalScalar::{Int, Null};
    let left = [Int(2), Null, Int(1), Int(2), Null];
    let right = [Int(3), Null, Int(2)];
    for (operation, quantifier, mut expected) in [
        (
            Op::Union,
            Q::All,
            vec![Null, Null, Null, Int(1), Int(2), Int(2), Int(2), Int(3)],
        ),
        (Op::Union, Q::Distinct, vec![Null, Int(1), Int(2), Int(3)]),
        (Op::Intersect, Q::All, vec![Null, Int(2)]),
        (Op::Intersect, Q::Distinct, vec![Null, Int(2)]),
        (Op::Except, Q::All, vec![Null, Int(1), Int(2)]),
        (Op::Except, Q::Distinct, vec![Int(1)]),
    ] {
        let query = query(operation, quantifier);
        let result = run(&query, &left, &right, wide(), || Ok(())).unwrap();
        expected.sort();
        assert_eq!(scalars(&result), expected, "{operation:?}/{quantifier:?}");
        assert_eq!(result.rows.snapshot_records, 8);
        assert_eq!(result.rows.result_rows as usize, expected.len());
        assert_eq!(query.columns(), &["value"]);
    }
}

#[test]
fn scalar_domains_are_not_coerced_and_large_payloads_are_metered() {
    let int = CanonicalScalar::Int(1);
    let float = CanonicalScalar::Float(CanonicalF64::new(1.0));
    let text = CanonicalScalar::ucs_basic_text(&"secret payload ".repeat(1024)).unwrap();
    let left = [int.clone(), text.clone()];
    let right = [float.clone(), text.clone()];
    let result = run(
        &query(Op::Union, Q::Distinct),
        &left,
        &right,
        wide(),
        || Ok(()),
    )
    .unwrap();
    let mut expected = vec![int, float, text];
    expected.sort();
    assert_eq!(scalars(&result), expected);
    assert!(result.evaluator.work_units > 500);
    assert!(!format!("{result:?}").contains("secret payload"));
}

#[test]
fn open_columns_compose_with_graph_scalars_without_changing_typed_bag_equality() {
    use CanonicalScalar::{Int, Null};
    let left = [Null, Int(1), Int(1), Int(2)];
    let right = [
        Null,
        CanonicalScalar::Float(CanonicalF64::new(1.0)),
        Int(1),
        Int(3),
    ];
    let concrete = PreparedGraphSet::from(pattern(1, "value"));
    let open = dynamic(
        &right
            .iter()
            .cloned()
            .map(GraphValue::Scalar)
            .collect::<Vec<_>>(),
    );
    let mut domain: Vec<_> = left.iter().chain(&right).cloned().collect();
    domain.sort();
    domain.dedup();
    for operation in [Op::Union, Op::Intersect, Op::Except] {
        for quantifier in [Q::All, Q::Distinct] {
            for reverse in [false, true] {
                let (first, second, first_values, second_values) = if reverse {
                    (&open, &concrete, &right[..], &left[..])
                } else {
                    (&concrete, &open, &left[..], &right[..])
                };
                let combined = first
                    .clone()
                    .combine(operation, quantifier, second.clone())
                    .unwrap();
                assert_eq!(combined.column_types(), &[GraphSetColumnType::Any]);
                let mut expected = Vec::new();
                for value in &domain {
                    let mut l = first_values.iter().filter(|v| *v == value).count();
                    let mut r = second_values.iter().filter(|v| *v == value).count();
                    if quantifier == Q::Distinct {
                        l = usize::from(l > 0);
                        r = usize::from(r > 0);
                    }
                    let count = match operation {
                        Op::Union if quantifier == Q::Distinct => usize::from(l + r > 0),
                        Op::Union => l + r,
                        Op::Intersect => l.min(r),
                        Op::Except => l.saturating_sub(r),
                    };
                    expected.extend(std::iter::repeat_n(value.clone(), count));
                }
                let result = run(&combined, &left, &[], wide(), || Ok(())).unwrap();
                assert_eq!(
                    scalars(&result),
                    expected,
                    "{operation:?}/{quantifier:?}/{reverse}"
                );
                assert_eq!(result.rows.snapshot_records, left.len() as u64);
            }
        }
    }
    assert_eq!(concrete.column_types(), &[GraphSetColumnType::Stored]);
    assert_eq!(open.column_types(), &[GraphSetColumnType::Any]);
}

#[test]
fn widened_subtrees_keep_their_open_domain_through_scopes_and_projections() {
    let int = GraphValue::Scalar(CanonicalScalar::Int(1));
    let map = GraphValue::map(vec![("n".into(), int.clone())]).unwrap();
    let list = GraphValue::List(vec![int.clone()].into_boxed_slice());
    let inner = constant(int.clone())
        .combine(Op::Union, Q::All, dynamic(std::slice::from_ref(&map)))
        .unwrap();
    assert_eq!(inner.column_types(), &[GraphSetColumnType::Any]);
    let projected = inner
        .nested()
        .unwrap()
        .project(
            vec![GraphSetProjection::new("value", GraphSetValue::Column(0))],
            Q::All,
        )
        .unwrap();
    assert_eq!(projected.column_types(), &[GraphSetColumnType::Any]);
    let concrete = constant(list.clone());
    assert_eq!(concrete.column_types(), &[GraphSetColumnType::List]);
    for reverse in [false, true] {
        let (left, right) = if reverse {
            (&concrete, &projected)
        } else {
            (&projected, &concrete)
        };
        let nested = left
            .clone()
            .combine(Op::Union, Q::All, right.clone())
            .unwrap();
        assert_eq!(nested.column_types(), &[GraphSetColumnType::Any]);
        let result = run(&nested, &[], &[], wide(), || Ok(())).unwrap();
        let mut actual: Vec<_> = result
            .value
            .iter()
            .map(|row| row.get(0).unwrap().clone())
            .collect();
        let mut expected = vec![int.clone(), map.clone(), list.clone()];
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(result.rows.snapshot_records, 0);
    }
}

#[test]
fn different_concrete_domains_still_refuse_even_with_nulls_and_empty_pages() {
    let scalar = GraphValue::Scalar(CanonicalScalar::Int(1));
    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let vertex = GraphValue::Vertex(VId(1));
    let edge = GraphValue::Edge(EId(1));
    let list = GraphValue::List(Vec::new().into_boxed_slice());
    for (left, right) in [
        (scalar, vertex.clone()),
        (list, null.clone()),
        (vertex.clone(), edge),
        (null, vertex),
    ] {
        let left = constant(left).with_page(0, Some(0));
        let right = constant(right).with_page(0, Some(0));
        for operation in [Op::Union, Op::Intersect, Op::Except] {
            for quantifier in [Q::All, Q::Distinct] {
                for (left, right) in [(&left, &right), (&right, &left)] {
                    assert_eq!(
                        left.clone()
                            .combine(operation, quantifier, right.clone())
                            .unwrap_err(),
                        GraphSetBuildError::ColumnType {
                            column: 0,
                            left: left.column_types()[0],
                            right: right.column_types()[0],
                        },
                    );
                }
            }
        }
    }
}

#[test]
fn open_output_keeps_child_source_admission_and_late_expression_errors() {
    let open = dynamic(&[GraphValue::Scalar(CanonicalScalar::Null)]);
    let concrete = PreparedGraphSet::from(pattern(1, "value"));
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let wrong_source = builder
        .prepare_values(&[GraphColumn::vertex("value", "n")], 0, None)
        .unwrap();
    for (left, right) in [(&concrete, &open), (&open, &concrete)] {
        let query = left
            .clone()
            .combine(Op::Union, Q::All, right.clone())
            .unwrap()
            .with_page(0, Some(0));
        let result: ResultRows = query.execute_governed(
            wide(),
            |_, allowance| {
                wrong_source.plan().execute_governed_with_properties(
                    1,
                    [VId(0)],
                    [],
                    |_, _| Ok::<_, &'static str>(true),
                    |_, _| Ok(None),
                    allowance,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::InputSchema {
                operand: 0
            }))
        ));
    }
    let mixed = constant(GraphValue::Scalar(CanonicalScalar::Int(1)))
        .combine(
            Op::Union,
            Q::All,
            dynamic(&[GraphValue::List(Vec::new().into_boxed_slice())]),
        )
        .unwrap();
    // `*` refuses a List late. (`+` joins one since fgdb-bqri3: [] + 0 is
    // [0], so it no longer witnesses a late expression error.)
    let arithmetic = GraphIntegerExpression::prepare_scalar(&[
        GraphIntegerOp::ScalarColumn(0),
        GraphIntegerOp::Literal(Some(0)),
        GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
    ])
    .unwrap();
    let projected = mixed
        .project(
            vec![GraphSetProjection::new(
                "value",
                GraphSetValue::Integer(arithmetic),
            )],
            Q::All,
        )
        .unwrap()
        .with_page(0, Some(0));
    assert!(matches!(run(&projected, &[], &[], wide(), || Ok(())),
        Err(GqlQueryError::Source(GraphSetExecutionError::Projection { row: 1, column: 0, error }))
        if error.kind == GraphIntegerErrorKind::NonScalar));
}

#[test]
fn typed_nulls_keep_distinct_domain_evidence_and_exact_resource_accounting() {
    let project = |value| {
        PreparedGraphSet::singleton()
            .project(vec![GraphSetProjection::new("value", value)], Q::All)
            .unwrap()
    };
    let ordinary = project(GraphSetValue::Literal(
        GqlScalarParameter::new(CanonicalScalar::Null).unwrap(),
    ));
    assert_eq!(ordinary.column_types(), &[GraphSetColumnType::Scalar]);
    let measured = run(&ordinary, &[], &[], wide(), || Ok(())).unwrap();
    let mut definitions = BTreeSet::new();
    definitions.insert(ordinary.canonical_bytes());
    for kind in [
        GraphSetColumnType::Vertex,
        GraphSetColumnType::Scalar,
        GraphSetColumnType::Path,
        GraphSetColumnType::Vertices,
        GraphSetColumnType::Edges,
        GraphSetColumnType::Edge,
        GraphSetColumnType::List,
        GraphSetColumnType::Any,
    ] {
        let plan = project(GraphSetValue::TypedNull(kind));
        assert_eq!(plan.column_types(), &[kind]);
        assert!(definitions.insert(plan.canonical_bytes()));
        assert_eq!(plan.canonical_bytes(), plan.clone().canonical_bytes());
        let exact = GqlQueryPolicy::new(
            0,
            1,
            measured.evaluator.work_units,
            measured.evaluator.scratch_entries,
        );
        let result = run(&plan, &[], &[], exact, || Ok(())).unwrap();
        assert_eq!(result, measured, "{kind:?}");
        assert_eq!(scalars(&result), vec![CanonicalScalar::Null]);
        for policy in [
            GqlQueryPolicy::new(0, 1, measured.evaluator.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(0, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
        ] {
            assert!(matches!(
                run(&plan, &[], &[], policy, || Ok(())),
                Err(GqlQueryError::Evaluator(_))
            ));
        }
    }
    let input = project(GraphSetValue::TypedNull(GraphSetColumnType::List));
    let input = input.clone().combine(Op::Union, Q::All, input).unwrap();
    let count = PreparedGraphSetAggregate::prepare(
        input,
        &[],
        &[GraphAggregate::count_rows("rows")],
        0,
        None,
    )
    .unwrap()
    .with_output_projection(vec![
        GraphSetProjection::new("rows", GraphSetValue::Column(0)),
        GraphSetProjection::new("nothing", GraphSetValue::TypedNull(GraphSetColumnType::Any)),
    ])
    .unwrap();
    let result =
        count
            .execute_governed(
                wide(),
                |_,
                 _|
                 -> Result<
                    GqlQueryExecution<GraphValueRow>,
                    GqlQueryError<&'static str, usize>,
                > { panic!("typed constants have no graph source") },
                || Ok::<_, usize>(()),
            )
            .unwrap();
    assert_eq!(result.rows.snapshot_records, 0);
    assert_eq!(result.value.len(), 1);
    assert_eq!(result.value[0].values()[0].as_count(), Some(2));
    assert!(result.value[0].values()[1].is_null());
}

#[test]
fn final_order_page_and_child_pages_apply_at_their_own_boundaries() {
    use CanonicalScalar::{Int, Null};
    let left = [Null, Int(2), Int(2)];
    let right = [Int(1), Null];
    let ordered = query(Op::Union, Q::All)
        .with_order_by(&[GraphValueOrder::descending(0).with_nulls_first(true)])
        .unwrap()
        .with_page(1, Some(3));
    let result = run(
        &ordered,
        &left,
        &right,
        GqlQueryPolicy::new(5, 3, u64::MAX, u64::MAX),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(scalars(&result), vec![Null, Int(2), Int(2)]);
    let child = PreparedGraphSet::from(pattern(1, "value"))
        .with_order_by(&[GraphValueOrder::descending(0)])
        .unwrap()
        .with_page(0, Some(1));
    let compound = child
        .combine(
            Op::Except,
            Q::All,
            PreparedGraphSet::from(pattern(2, "value")),
        )
        .unwrap();
    let result = run(&compound, &left, &right, wide(), || Ok(())).unwrap();
    assert_eq!(scalars(&result), vec![Int(2)]);
    let empty = compound.with_page(u64::MAX, Some(u64::MAX));
    assert!(
        run(&empty, &left, &right, wide(), || Ok(()))
            .unwrap()
            .value
            .is_empty()
    );
}

#[test]
fn every_policy_dimension_is_cumulative_and_final_output_is_not_operand_output() {
    let left = vec![CanonicalScalar::Int(1); 4];
    let right = vec![CanonicalScalar::Int(1); 3];
    let query = query(Op::Except, Q::All);
    let measured = run(&query, &left, &right, wide(), || Ok(())).unwrap();
    assert_eq!(measured.value.len(), 1);
    let exact = GqlQueryPolicy::new(
        7,
        1,
        measured.evaluator.work_units,
        measured.evaluator.scratch_entries,
    );
    assert_eq!(
        run(&query, &left, &right, exact, || Ok(())).unwrap(),
        measured
    );
    for (policy, dimension) in [
        (
            GqlQueryPolicy::new(6, 1, u64::MAX, u64::MAX),
            GqlBudgetDimension::SnapshotRecords,
        ),
        (
            GqlQueryPolicy::new(7, 0, u64::MAX, u64::MAX),
            GqlBudgetDimension::ResultRows,
        ),
    ] {
        assert!(matches!(run(&query, &left, &right, policy, || Ok(())),
            Err(GqlQueryError::Rows(error)) if error.dimension == dimension && error.observed == error.limit + 1));
    }
    for policy in [
        GqlQueryPolicy::new(7, 1, measured.evaluator.work_units - 1, u64::MAX),
        GqlQueryPolicy::new(7, 1, u64::MAX, measured.evaluator.scratch_entries - 1),
    ] {
        assert!(matches!(
            run(&query, &left, &right, policy, || Ok(())),
            Err(GqlQueryError::Evaluator(_))
        ));
    }
    let empty = query.with_page(0, Some(0));
    let result = run(
        &empty,
        &left,
        &right,
        GqlQueryPolicy::new(7, 0, u64::MAX, u64::MAX),
        || Ok(()),
    )
    .unwrap();
    assert!(result.value.is_empty());
    assert_eq!(result.rows.snapshot_records, 7);
}

#[test]
fn cancellation_at_every_combined_checkpoint_never_returns_partial_rows() {
    let left = [
        CanonicalScalar::Int(3),
        CanonicalScalar::Null,
        CanonicalScalar::Int(1),
    ];
    let right = [CanonicalScalar::Int(2), CanonicalScalar::Null];
    let query = query(Op::Union, Q::Distinct).with_page(1, Some(2));
    let total = Cell::new(0);
    run(&query, &left, &right, wide(), || {
        total.set(total.get() + 1);
        Ok(())
    })
    .unwrap();
    for stop in 1..=total.get() {
        let seen = Cell::new(0);
        let result = run(&query, &left, &right, wide(), || {
            seen.set(seen.get() + 1);
            if seen.get() == stop {
                Err(stop)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
        assert_eq!(seen.get(), stop);
    }
}

#[test]
fn empty_left_and_limit_zero_do_not_hide_right_source_failure() {
    for operation in [Op::Union, Op::Intersect, Op::Except] {
        let query = query(operation, Q::Distinct).with_page(0, Some(0));
        let mut seen = 0;
        let result = query.execute_governed(
            wide(),
            |pattern, allowance| {
                seen += 1;
                if seen == 2 {
                    return Err(GqlQueryError::Source("right unavailable"));
                }
                pattern.plan().execute_governed_with_properties(
                    0,
                    [],
                    [],
                    |_, _| Ok(true),
                    |_, _| Ok(None),
                    allowance,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                "right unavailable"
            )))
        ));
        assert_eq!(seen, 2);
    }
}

#[test]
fn schema_bounds_transcripts_and_diagnostics_are_explicit() {
    let left = PreparedGraphSet::from(pattern(1, "sensitive_alias"));
    let right = PreparedGraphSet::from(pattern(2, "other"));
    let union = left
        .clone()
        .combine(Op::Union, Q::All, right.clone())
        .unwrap();
    let frozen = union.canonical_bytes();
    assert!(!format!("{union:?}").contains("sensitive_alias"));
    assert_eq!(frozen, query(Op::Union, Q::All).canonical_bytes());
    assert_ne!(
        frozen,
        union.clone().with_page(0, Some(0)).canonical_bytes()
    );
    assert_ne!(frozen, query(Op::Union, Q::Distinct).canonical_bytes());
    assert_ne!(frozen, query(Op::Except, Q::All).canonical_bytes());
    assert_ne!(
        frozen,
        right
            .clone()
            .combine(Op::Union, Q::All, left.clone())
            .unwrap()
            .canonical_bytes()
    );
    assert!(matches!(
        union.clone().with_order_by(&[]),
        Err(GraphOrderError::EmptyOrder)
    ));
    assert!(matches!(
        union.with_order_by(&[GraphValueOrder::ascending(1)]),
        Err(GraphOrderError::UnknownColumn { .. })
    ));
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    let identity = builder
        .prepare_values(&[GraphColumn::vertex("n", "n")], 0, None)
        .unwrap();
    assert!(matches!(
        left.clone().combine(Op::Union, Q::All, identity.into()),
        Err(GraphSetBuildError::ColumnType { .. })
    ));
    let pair = builder
        .prepare_values(
            &[
                GraphColumn::property("a", "n", PropertyKeyId(1)),
                GraphColumn::property("b", "n", PropertyKeyId(1)),
            ],
            0,
            None,
        )
        .unwrap();
    assert!(matches!(
        left.clone().combine(Op::Union, Q::All, pair.into()),
        Err(GraphSetBuildError::ColumnCount { .. })
    ));
    let mut largest = left.clone();
    for _ in 1..MAX_GRAPH_SET_OPERANDS {
        largest = largest.combine(Op::Union, Q::All, left.clone()).unwrap();
    }
    assert_eq!(largest.operand_count(), MAX_GRAPH_SET_OPERANDS);
    assert!(matches!(
        largest.combine(Op::Union, Q::All, left),
        Err(GraphSetBuildError::TooManyOperands { .. })
    ));
}

#[test]
fn tuple_identity_is_not_independent_column_membership() {
    use CanonicalScalar::Int;
    let make = |key| {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        PreparedGraphSet::from(
            builder
                .prepare_values(
                    &[
                        GraphColumn::property("a", "n", PropertyKeyId(key)),
                        GraphColumn::property("b", "n", PropertyKeyId(key + 1)),
                    ],
                    0,
                    None,
                )
                .unwrap()
                .with_duplicates(),
        )
    };
    let left = [[Int(1), Int(2)], [Int(2), Int(1)], [Int(1), Int(2)]];
    let right = [[Int(1), Int(1)], [Int(2), Int(2)], [Int(1), Int(2)]];
    let left = left.map(|row| row.map(CanonicalPropertyValue::from));
    let right = right.map(|row| row.map(CanonicalPropertyValue::from));
    for (operation, expected) in [
        (Op::Intersect, vec![vec![Int(1), Int(2)]]),
        (Op::Except, vec![vec![Int(2), Int(1)]]),
    ] {
        let compound = make(1).combine(operation, Q::Distinct, make(3)).unwrap();
        let result = compound
            .execute_governed(
                wide(),
                |pattern, allowance| {
                    let ValueProjection::Property { key: first, .. } = pattern.value_columns()[0]
                    else {
                        panic!("fixture")
                    };
                    let data = if first == PropertyKeyId(1) {
                        &left
                    } else {
                        &right
                    };
                    pattern.plan().execute_governed_with_properties(
                        3,
                        [VId(0), VId(1), VId(2)],
                        [],
                        |_, _| Ok::<_, ()>(true),
                        |vid, key| Ok(Some(&data[vid.0 as usize][(key.0 - first.0) as usize])),
                        allowance,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        let actual: Vec<Vec<_>> = result
            .value
            .iter()
            .map(|row| {
                row.values()
                    .iter()
                    .map(|cell| cell.as_scalar().unwrap().clone())
                    .collect()
            })
            .collect();
        assert_eq!(actual, expected);
    }
}
