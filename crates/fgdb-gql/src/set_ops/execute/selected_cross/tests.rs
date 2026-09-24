use super::*;
use crate::algebra::IntegerComparison;
use crate::{GqlParameters, PreparedGraphSetText};
use fgdb_types::{CanonicalScalar, VId};

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}
fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn row(values: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(values.iter().copied().map(scalar).collect())
}
fn compare(a: usize, comparison: IntegerComparison, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison,
        right: GraphSetOperand::Column(b),
    }
}
fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    compare(a, IntegerComparison::Equal, b)
}
fn validate(width: usize, code: &[GraphSetPredicateOp]) {
    GraphSetPredicateOp::validate_schema(&vec![GraphSetColumnType::Any; width], code).unwrap();
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn sequence(values: &[Option<i64>], name: &str) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            name.into(),
            GraphSetValue::List(
                values
                    .iter()
                    .copied()
                    .map(|value| GraphSetValue::Value(scalar(value)))
                    .collect(),
            ),
        )
        .unwrap()
}
fn execute(query: &PreparedGraphSet, policy: GqlQueryPolicy) -> GqlQueryExecution<GraphValueRow> {
    query
        .execute_governed(
            policy,
            |_, _| -> Result<_, GqlQueryError<usize, usize>> {
                panic!("a source-free relation must not open a graph")
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
}

#[test]
fn selected_pairs_match_independent_occurrence_oracle_with_composite_keys_and_residuals() {
    // Duplicates are separate occurrences, and candidate order is deliberately
    // not key order. The oracle uses primitive values, not any production join.
    for mask in 0..256 {
        let left_data = [
            (Some(2), Some(3)),
            (Some(1), Some(4)),
            (Some(2), Some(3)),
            (None, Some(5)),
        ];
        let right_data = [
            (Some(2), Some(8)),
            (Some(1), Some(4)),
            (None, Some(9)),
            (Some(2), Some(3)),
        ];
        let left: Vec<_> = left_data
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, &(a, b))| row(&[a, b]))
            .collect();
        let right: Vec<_> = right_data
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << (i + 4)) != 0)
            .map(|(_, &(a, b))| row(&[a, b]))
            .collect();
        for composite in [false, true] {
            let code = if composite {
                vec![equal(2, 0), equal(1, 3), GraphSetPredicateOp::And]
            } else {
                vec![
                    equal(2, 0),
                    compare(1, IntegerComparison::Less, 3),
                    GraphSetPredicateOp::And,
                ]
            };
            validate(4, &code);
            let mut expected = Vec::new();
            for (i, &(a, b)) in left_data.iter().enumerate() {
                if mask & (1 << i) == 0 {
                    continue;
                }
                for (j, &(c, d)) in right_data.iter().enumerate() {
                    if mask & (1 << (j + 4)) == 0 {
                        continue;
                    }
                    if a.zip(c).is_some_and(|(a, c)| a == c)
                        && b.zip(d)
                            .is_some_and(|(b, d)| if composite { b == d } else { b < d })
                    {
                        expected.push(row(&[a, b, c, d]));
                    }
                }
            }
            assert_eq!(
                collect(&left, &right, &code, None, &mut allow).unwrap(),
                expected
            );
        }
    }
}

#[test]
fn boolean_proof_never_extracts_an_equality_from_only_one_or_arm_or_beneath_not() {
    let eq = equal(0, 1);
    let yes = GraphSetPredicateOp::Truth(Some(true));
    let no = GraphSetPredicateOp::Truth(Some(false));
    let unknown = GraphSetPredicateOp::Truth(None);
    for (code, expected) in [
        (vec![eq.clone()], vec![(0, 0)]),
        (
            vec![eq.clone(), yes.clone(), GraphSetPredicateOp::Or],
            vec![],
        ),
        (
            vec![eq.clone(), unknown.clone(), GraphSetPredicateOp::Or],
            vec![],
        ),
        (vec![eq.clone(), GraphSetPredicateOp::Not], vec![]),
        (
            vec![
                eq.clone(),
                GraphSetPredicateOp::Not,
                GraphSetPredicateOp::Not,
            ],
            vec![],
        ),
        (
            vec![eq.clone(), yes.clone(), GraphSetPredicateOp::And],
            vec![(0, 0)],
        ),
        (
            vec![
                eq.clone(),
                no.clone(),
                GraphSetPredicateOp::And,
                equal(1, 0),
                unknown.clone(),
                GraphSetPredicateOp::And,
                GraphSetPredicateOp::Or,
            ],
            vec![(0, 0)],
        ),
    ] {
        validate(2, &code);
        assert_eq!(required_keys(1, &code, None, &mut allow).unwrap(), expected);
        let left = [row(&[None]), row(&[Some(1)]), row(&[Some(2)])];
        let right = [row(&[Some(2)]), row(&[None]), row(&[Some(1)])];
        // The ordinary row filter is an independent execution path from the
        // borrowed-pair/index path, including eager three-valued evaluation.
        let mut original = Vec::new();
        for l in &left {
            for r in &right {
                let pair = copy_pair(l, r, None, &mut allow).unwrap();
                if GraphSetPredicateOp::evaluate_row_with_control(&code, &pair, &mut allow).unwrap()
                {
                    original.push(pair);
                }
            }
        }
        assert_eq!(
            collect(&left, &right, &code, None, &mut allow).unwrap(),
            original
        );
    }
}

#[test]
fn dynamic_scalar_vertex_and_incompatible_keys_use_the_same_predicate_equality_law() {
    let values = vec![
        scalar(None),
        scalar(Some(1)),
        scalar(Some(2)),
        GraphValue::Scalar(CanonicalScalar::Bool(true)),
        GraphValue::Scalar(CanonicalScalar::ucs_basic_text("1").unwrap()),
        GraphValue::Scalar(CanonicalScalar::bytes(vec![1]).unwrap()),
        GraphValue::Vertex(VId(1)),
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::List(vec![scalar(Some(1))].into_boxed_slice()),
    ];
    let left: Vec<_> = values
        .iter()
        .cloned()
        .map(|value| GraphValueRow::from_owned_values(vec![value]))
        .collect();
    let right: Vec<_> = values
        .into_iter()
        .rev()
        .map(|value| GraphValueRow::from_owned_values(vec![value]))
        .collect();
    let code = [equal(0, 1)];
    validate(2, &code);
    let actual = collect(&left, &right, &code, None, &mut allow).unwrap();
    let expected: Vec<_> = left
        .iter()
        .filter(|row| {
            !row.values()[0].is_null()
                && matches!(
                    &row.values()[0],
                    GraphValue::Scalar(_) | GraphValue::Vertex(_)
                )
        })
        .map(|row| GraphValueRow::from_owned_values(vec![row.values()[0].clone(); 2]))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn source_free_pages_preserve_left_major_child_order_and_product_barriers() {
    let left = sequence(&[Some(2), Some(1), Some(2), None], "a");
    let right = sequence(&[Some(1), Some(2), Some(2), None], "b");
    let code = [equal(0, 1)];
    let product = left.clone().cross_join(right.clone()).unwrap();
    let query = product
        .clone()
        .nested()
        .unwrap()
        .filter(&code)
        .unwrap()
        .with_page(1, Some(3));
    let before = query.canonical_bytes();
    assert!(query.filtered_cross_inputs().is_some());
    assert_eq!(
        execute(&query, policy()).value,
        vec![
            row(&[Some(2), Some(2)]),
            row(&[Some(1), Some(1)]),
            row(&[Some(2), Some(2)])
        ]
    );
    assert_eq!(query.canonical_bytes(), before);
    // Moving WHERE below this product page would manufacture a surviving row.
    let barrier = product.clone().with_page(0, Some(1)).filter(&code).unwrap();
    assert!(barrier.filtered_cross_inputs().is_none());
    assert!(execute(&barrier, policy()).value.is_empty());
    let ordered = product
        .with_order_by(&[GraphValueOrder::ascending(0)])
        .unwrap()
        .filter(&code)
        .unwrap();
    assert!(ordered.filtered_cross_inputs().is_none());
    let local_pages = left
        .with_page(1, Some(2))
        .cross_join(right.with_page(0, Some(2)))
        .unwrap()
        .filter(&code)
        .unwrap();
    assert_eq!(
        execute(&local_pages, policy()).value,
        vec![row(&[Some(1), Some(1)]), row(&[Some(2), Some(2)])]
    );
}

#[test]
fn large_selective_join_has_linear_scratch_and_subquadratic_scalar_work() {
    for n in [128_i64, 1024] {
        let left: Vec<_> = (0..n).rev().map(|v| row(&[Some(v)])).collect();
        let right: Vec<_> = (0..n).map(|v| row(&[Some(v)])).collect();
        let mut work = 0;
        let mut scratch = 0;
        let rows = collect(&left, &right, &[equal(0, 1)], None, &mut |event| {
            match event {
                GlaExecutionEvent::ScratchEntry => scratch += 1,
                _ => work += 1,
            }
            Ok::<_, usize>(())
        })
        .unwrap();
        assert_eq!(rows.len(), n as usize);
        assert_eq!(rows[0], row(&[Some(n - 1), Some(n - 1)]));
        assert!(scratch < 10 * n as usize, "retained scratch {scratch}");
        assert!(work < 200 * n as usize, "scalar/index work {work}");
        if n == 1024 {
            assert!(work < (n * n / 3) as usize);
        }
    }
    // A non-equality predicate still gets allocation-free rejected candidates.
    let rows: Vec<_> = (0..256).map(|v| row(&[Some(v)])).collect();
    let mut scratch = 0;
    let output = collect(
        &rows,
        &rows,
        &[GraphSetPredicateOp::Truth(Some(false))],
        None,
        &mut |event| {
            scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, usize>(())
        },
    )
    .unwrap();
    assert!(output.is_empty());
    assert_eq!(scratch, 0);
}

#[test]
fn every_index_predicate_and_copy_refusal_leaves_no_result_and_is_retryable() {
    let left = [
        row(&[Some(3)]),
        row(&[Some(1)]),
        row(&[Some(1)]),
        row(&[None]),
    ];
    let right = [
        row(&[Some(1)]),
        row(&[Some(3)]),
        row(&[Some(1)]),
        row(&[None]),
    ];
    let code = [equal(0, 1)];
    let mut count = 0;
    let expected = collect(&left, &right, &code, None, &mut |_| {
        count += 1;
        Ok::<_, usize>(())
    })
    .unwrap();
    for stop in 1..=count {
        let mut calls = 0;
        assert_eq!(
            collect(&left, &right, &code, None, &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            }),
            Err(stop)
        );
        assert_eq!(calls, stop);
    }
    assert_eq!(
        collect(&left, &right, &code, None, &mut allow).unwrap(),
        expected
    );
}

#[test]
fn empty_left_and_outer_limit_zero_never_erase_right_source_failures() {
    let leaf = || {
        let mut builder = crate::algebra::GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        PreparedGraphSet::from(
            builder
                .prepare_values(&[crate::algebra::GraphColumn::vertex("n", "n")], 0, None)
                .unwrap(),
        )
    };
    let query = leaf()
        .cross_join(leaf())
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap()
        .with_page(0, Some(0));
    let mut calls = 0;
    let result = query.execute_governed(
        policy(),
        |_, _| {
            calls += 1;
            if calls == 1 {
                Ok(GqlQueryExecution {
                    value: vec![],
                    rows: GqlExecutionStats {
                        snapshot_records: 0,
                        result_rows: 0,
                    },
                    evaluator: GlaExecutionStats::default(),
                })
            } else {
                Err(GqlQueryError::Source(71_usize))
            }
        },
        || Ok::<_, usize>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(GraphSetExecutionError::Source(71)))
    ));
    assert_eq!(calls, 2);
}

#[test]
fn native_unwind_match_selection_reaches_the_physical_path_without_new_syntax() {
    // The catalog and graph source use the normal public preparation surface.
    let prepared = PreparedGraphSetText::prepare(
        "UNWIND [2, 1, 2] AS wanted MATCH (n { p: wanted }) RETURN wanted, wanted AS value",
        |kind, name| {
            if kind == crate::GraphSymbolKind::Property && name == "p" {
                Some(crate::GraphSymbol::Property(
                    fgdb_delta_types::PropertyKeyId(1),
                ))
            } else {
                None
            }
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap();
    fn fused(query: &PreparedGraphSet) -> bool {
        if query.filtered_cross_inputs().is_some() {
            return true;
        }
        match &query.node {
            SetNode::Scope(input)
            | SetNode::Filter { input, .. }
            | SetNode::Project { input, .. }
            | SetNode::Unwind { input, .. } => fused(input),
            SetNode::CrossJoin { left, right }
            | SetNode::Join { left, right, .. }
            | SetNode::Binary { left, right, .. } => fused(left) || fused(right),
            _ => false,
        }
    }
    assert!(
        fused(&prepared),
        "native binding must reach the selected-product path"
    );
    let s1 = CanonicalScalar::Int(1);
    let s2 = CanonicalScalar::Int(2);
    let result = prepared
        .execute_governed(
            policy(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    2,
                    [VId(1), VId(2)],
                    [],
                    |_, _| Ok::<_, usize>(true),
                    |id, _| {
                        Ok(match id.0 {
                            1 => Some(&s1),
                            2 => Some(&s2),
                            _ => None,
                        })
                    },
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(
        result.value,
        vec![
            row(&[Some(2), Some(2)]),
            row(&[Some(1), Some(1)]),
            row(&[Some(2), Some(2)])
        ]
    );
}

#[test]
fn column_projection_reordering_and_repeated_columns_are_borrowed_not_materialized() {
    let product = sequence(&[Some(2), Some(1), Some(2)], "a")
        .cross_join(sequence(&[Some(1), Some(2), Some(2)], "b"))
        .unwrap();
    let projection = vec![
        GraphSetProjection::new("right_value", GraphSetValue::Column(1)),
        GraphSetProjection::new("left_value", GraphSetValue::Column(0)),
        GraphSetProjection::new("again", GraphSetValue::Column(1)),
    ];
    let selected = product
        .clone()
        .project(projection.clone(), GraphSetQuantifier::All)
        .unwrap()
        .nested()
        .unwrap()
        .filter(&[equal(1, 0), equal(2, 0), GraphSetPredicateOp::And])
        .unwrap();
    assert!(selected.filtered_cross_inputs().is_some());
    let expected = vec![
        row(&[Some(2); 3]),
        row(&[Some(2); 3]),
        row(&[Some(1); 3]),
        row(&[Some(2); 3]),
        row(&[Some(2); 3]),
    ];
    assert_eq!(execute(&selected, policy()).value, expected);
    let distinct = product
        .clone()
        .project(projection, GraphSetQuantifier::Distinct)
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap();
    assert!(distinct.filtered_cross_inputs().is_none());
    assert_eq!(
        execute(&distinct, policy()).value,
        vec![row(&[Some(1); 3]), row(&[Some(2); 3])]
    );
    // Even a constant FALSE filter must not suppress a failing child expression.
    let failing = product
        .project(
            vec![GraphSetProjection::new(
                "bad",
                GraphSetValue::Size(Box::new(GraphSetValue::Column(0))),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
        .filter(&[GraphSetPredicateOp::Truth(Some(false))])
        .unwrap();
    assert!(failing.filtered_cross_inputs().is_none());
    let result = failing.execute_governed(
        policy(),
        |_, _| -> Result<_, GqlQueryError<usize, usize>> { panic!("source-free expression") },
        || Ok::<_, usize>(()),
    );
    assert!(matches!(
        result,
        Err(GqlQueryError::Source(
            GraphSetExecutionError::Projection { .. }
        ))
    ));
}

#[test]
fn borrowed_projection_matches_materialized_eager_predicates_at_every_input_split() {
    let cells = [
        scalar(Some(2)),
        scalar(None),
        scalar(Some(1)),
        scalar(Some(2)),
    ];
    for split in 0..=cells.len() {
        let left = GraphValueRow::from_owned_values(cells[..split].to_vec());
        let right = GraphValueRow::from_owned_values(cells[split..].to_vec());
        for mapping in [vec![3, 0, 1, 2, 0], vec![1], vec![0, 3, 2, 1]] {
            let projected = GraphValueRow::from_owned_values(
                mapping.iter().map(|&i| cells[i].clone()).collect(),
            );
            for a in 0..mapping.len() {
                for b in 0..mapping.len() {
                    for op in [
                        IntegerComparison::Equal,
                        IntegerComparison::Less,
                        IntegerComparison::NotEqual,
                    ] {
                        let code = [compare(a, op, b), GraphSetPredicateOp::Not];
                        validate(mapping.len(), &code);
                        let ordinary = GraphSetPredicateOp::evaluate_row_with_control(
                            &code, &projected, &mut allow,
                        )
                        .unwrap();
                        let borrowed = GraphSetPredicateOp::evaluate_projected_pair_with_control(
                            &code,
                            &left,
                            &right,
                            Some(&mapping),
                            &mut allow,
                        )
                        .unwrap();
                        assert_eq!(borrowed, ordinary);
                    }
                }
            }
        }
    }
}

#[test]
fn final_rows_and_cumulative_allowances_still_refuse_at_inclusive_boundaries() {
    let query = sequence(&[Some(2), Some(1), Some(2)], "a")
        .cross_join(sequence(&[Some(1), Some(2)], "b"))
        .unwrap()
        .filter(&[equal(0, 1)])
        .unwrap();
    let execution = execute(&query, policy());
    assert_eq!(execution.value.len(), 3);
    let work = execution.evaluator.work_units;
    let scratch = execution.evaluator.scratch_entries;
    assert_eq!(
        execute(&query, GqlQueryPolicy::new(0, 3, work, scratch)).value,
        execution.value
    );
    for denied in [
        GqlQueryPolicy::new(0, 2, work, scratch),
        GqlQueryPolicy::new(0, 3, work - 1, scratch),
        GqlQueryPolicy::new(0, 3, work, scratch - 1),
    ] {
        let result = query.execute_governed(
            denied,
            |_, _| -> Result<_, GqlQueryError<usize, usize>> { panic!("source-free relation") },
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Rows(_)) | Err(GqlQueryError::Evaluator(_))
        ));
    }
}

/// Prepare against the one-property catalog (`p` = key 1) the tests above use.
fn prepare_p(text: &str) -> Result<PreparedGraphSet, crate::GraphSetTextError> {
    PreparedGraphSetText::prepare(text, |kind, name| {
        (kind == crate::GraphSymbolKind::Property && name == "p").then_some(
            crate::GraphSymbol::Property(fgdb_delta_types::PropertyKeyId(1)),
        )
    })
    .map(|prepared| prepared.bind_parameters(&GqlParameters::new()).unwrap())
}

/// Execute over two vertices with p(1) = 1 and p(2) = 2; the rows as a bag.
fn over_two_vertices(prepared: &PreparedGraphSet) -> Vec<GraphValueRow> {
    let (s1, s2) = (CanonicalScalar::Int(1), CanonicalScalar::Int(2));
    let mut rows = prepared
        .execute_governed(
            policy(),
            |pattern, remaining| {
                pattern.plan().execute_governed_with_properties(
                    2,
                    [VId(1), VId(2)],
                    [],
                    |_, _| Ok::<_, usize>(true),
                    |id, _| {
                        Ok(match id.0 {
                            1 => Some(&s1),
                            2 => Some(&s2),
                            _ => None,
                        })
                    },
                    remaining,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        )
        .unwrap()
        .value;
    rows.sort();
    rows
}

#[test]
fn leading_unwind_match_with_reads_graph_properties_beside_row_columns() {
    // The first WITH after UNWIND..MATCH is the graph-to-row boundary: `n.p`
    // reads a graph property there, `x` the leading row column.
    let product = prepare_p("UNWIND [1, 2] AS x MATCH (n) WITH x, n.p AS p RETURN x, p").unwrap();
    assert_eq!(
        over_two_vertices(&product),
        vec![
            row(&[Some(1), Some(1)]),
            row(&[Some(1), Some(2)]),
            row(&[Some(2), Some(1)]),
            row(&[Some(2), Some(2)]),
        ]
    );
    // Differential: the WITH..WHERE form and the correlated MATCH form, which
    // was already supported, describe the same bag.
    let with_where = prepare_p(
        "UNWIND [2, 1, 2] AS wanted MATCH (n) WITH wanted, n.p AS value WHERE wanted = value RETURN value, wanted",
    )
    .unwrap();
    let correlated = prepare_p(
        "UNWIND [2, 1, 2] AS wanted MATCH (n { p: wanted }) RETURN wanted AS value, wanted",
    )
    .unwrap();
    let expected = over_two_vertices(&correlated);
    assert_eq!(expected.len(), 3);
    assert_eq!(over_two_vertices(&with_where), expected);
    // Unaliased items take the column's own name, as at a MATCH-first WITH.
    let named = prepare_p("UNWIND [1] AS x MATCH (n) WITH x, n.p RETURN x, p").unwrap();
    assert_eq!(over_two_vertices(&named).len(), 2);
}

#[test]
fn leading_unwind_match_with_distinct_deduplicates_the_projected_rows() {
    let all = prepare_p("UNWIND [1, 1] AS x MATCH (n) WITH x, n.p AS p RETURN x, p").unwrap();
    assert_eq!(
        over_two_vertices(&all).len(),
        4,
        "control: two copies of each pair"
    );
    let distinct =
        prepare_p("UNWIND [1, 1] AS x MATCH (n) WITH DISTINCT x, n.p AS p RETURN x, p").unwrap();
    assert_eq!(
        over_two_vertices(&distinct),
        vec![row(&[Some(1), Some(1)]), row(&[Some(1), Some(2)])]
    );
}

#[test]
fn leading_unwind_match_with_refuses_malformed_neighbours() {
    for (text, why) in [
        (
            "UNWIND [1] AS x MATCH (n) WITH n.p + 1 RETURN x",
            "a computed item needs an alias",
        ),
        (
            "UNWIND [1] AS x MATCH (n) WITH missing RETURN missing",
            "an unknown name is not a column",
        ),
        (
            "UNWIND [1] AS x MATCH (n) WITH x, n.p AS x RETURN x",
            "two items cannot share an alias",
        ),
        (
            "UNWIND [1] AS x MATCH (n) WITH x, n RETURN n.p",
            "past the boundary only the projected row exists",
        ),
    ] {
        assert!(prepare_p(text).is_err(), "{why}: {text}");
    }
}

#[test]
fn leading_unwind_match_with_star_carries_leading_columns_and_named_bindings() {
    let star = prepare_p("UNWIND [1, 2] AS x MATCH (n) WITH * RETURN x").unwrap();
    assert_eq!(
        over_two_vertices(&star),
        vec![
            row(&[Some(1)]),
            row(&[Some(1)]),
            row(&[Some(2)]),
            row(&[Some(2)]),
        ]
    );
}
