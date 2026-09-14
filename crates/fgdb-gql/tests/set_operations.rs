use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{
    GraphColumn, GraphOrderError, GraphPatternBuilder, GraphValueOrder, GraphValueRow,
    PreparedGraphPattern, ValueProjection,
};
use fgdb_gql::{
    GqlBudgetDimension, GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphSetBuildError,
    GraphSetExecutionError, GraphSetOperation as Op, GraphSetQuantifier as Q,
    MAX_GRAPH_SET_OPERANDS, PreparedGraphSet,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, VId};
use std::cell::{Cell, RefCell};

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
    query.execute_governed(
        policy,
        |pattern, allowance| {
            let ValueProjection::Property { key, .. } = pattern.value_columns()[0] else {
                panic!("scalar fixture")
            };
            let values = if key == PropertyKeyId(1) { left } else { right };
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
