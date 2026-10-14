use super::*;
use crate::GraphSetOperand;
use crate::algebra::IntegerComparison;
use fgdb_types::{EId, VId};

const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner,
    RowJoinKind::Left,
    RowJoinKind::Right,
    RowJoinKind::Full,
    RowJoinKind::Semi,
    RowJoinKind::Anti,
];
fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}
fn row(key: Option<i64>, value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        [key, value]
            .into_iter()
            .map(|value| {
                GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
            })
            .collect(),
    )
}
fn spec(keyed: bool, kind: RowJoinKind) -> RowJoinSpec {
    let types = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed {
        RowJoinSpec::new(&types, &types, &[(0, 0)])
    } else {
        RowJoinSpec::cross(&types, &types)
    }
    .unwrap();
    spec.with_kind(kind)
        .with_predicate(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1),
            comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(3),
        }])
        .unwrap()
}
fn occurrences(
    spec: &RowJoinSpec,
    left: &[GraphValueRow],
    right: &[GraphValueRow],
) -> Vec<GraphValueRow> {
    let mut output = Vec::new();
    for a in left {
        let mut found = false;
        for b in right {
            if spec.test_pair_with_control(a, b, &mut allow).unwrap() {
                found = true;
                if !matches!(spec.kind(), RowJoinKind::Semi | RowJoinKind::Anti) {
                    output.push(
                        spec.project_output_with_control(Some(a), Some(b), &mut allow)
                            .unwrap(),
                    );
                }
            }
        }
        if matches!(
            spec.kind(),
            RowJoinKind::Left | RowJoinKind::Full | RowJoinKind::Anti
        ) && !found
            || spec.kind() == RowJoinKind::Semi && found
        {
            output.push(
                spec.project_output_with_control(Some(a), None, &mut allow)
                    .unwrap(),
            );
        }
    }
    if matches!(spec.kind(), RowJoinKind::Right | RowJoinKind::Full) {
        for b in right {
            if !left
                .iter()
                .any(|a| spec.test_pair_with_control(a, b, &mut allow).unwrap())
            {
                output.push(
                    spec.project_output_with_control(None, Some(b), &mut allow)
                        .unwrap(),
                );
            }
        }
    }
    output.sort();
    output
}

#[test]
fn borrowed_pairs_agree_with_native_weighted_joins_for_all_six_kinds() {
    let limbs = LimbLimit::new(4);
    let mut checked = 0;
    for code in 0..81 {
        let mut code = code;
        let mut counts = [0; 4];
        for count in &mut counts {
            *count = code % 3;
            code /= 3;
        }
        let left = [
            vec![row(Some(1), Some(10)); counts[0]],
            vec![row(None, Some(30)); counts[1]],
        ]
        .concat();
        let right = [
            vec![row(Some(1), Some(20)); counts[2]],
            vec![row(Some(2), None); counts[3]],
        ]
        .concat();
        let bag = |rows: &[GraphValueRow]| {
            ZSet::from_updates(
                rows.iter().cloned().map(|row| (row, ZWeight::from_i128(1))),
                limbs,
                &mut |_| Ok::<_, usize>(()),
            )
            .unwrap()
        };
        for keyed in [false, true] {
            for kind in KINDS {
                let spec = spec(keyed, kind);
                let mut native = IncrementalRowJoin::new(spec.clone());
                let result = native
                    .prepare(&bag(&left), &bag(&right), limbs, None, &mut |_| {
                        Ok::<_, usize>(())
                    })
                    .unwrap()
                    .commit();
                let mut expected = Vec::new();
                for (row, weight) in result.iter() {
                    for _ in 0..weight.to_i128().unwrap() {
                        expected.push(row.clone());
                    }
                }
                assert_eq!(
                    occurrences(&spec, &left, &right),
                    expected,
                    "{kind:?} keyed={keyed}"
                );
                checked += 1;
            }
        }
    }
    assert_eq!(checked, 972);
}

#[test]
fn canonical_keys_and_borrowed_on_keep_distinct_null_domains() {
    let types = [GraphSetColumnType::Scalar; 2];
    let keyed = RowJoinSpec::new(&types, &types, &[(0, 0), (1, 1)]).unwrap();
    let a = row(Some(1), None);
    assert!(!keyed.test_pair_with_control(&a, &a, &mut allow).unwrap());
    let cross = RowJoinSpec::cross(&types, &types).unwrap();
    assert!(cross.test_pair_with_control(&a, &a, &mut allow).unwrap());
    let negative = cross
        .with_predicate(&[
            GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(1),
                comparison: IntegerComparison::Equal,
                right: GraphSetOperand::Column(3),
            },
            GraphSetPredicateOp::Not,
        ])
        .unwrap();
    assert!(!negative.test_pair_with_control(&a, &a, &mut allow).unwrap());
    let b = row(Some(1), Some(7));
    assert!(!negative.test_pair_with_control(&a, &b, &mut allow).unwrap());
    assert!(keyed.test_pair_with_control(&b, &b, &mut allow).unwrap());
    assert!(
        !keyed
            .test_pair_with_control(&b, &row(Some(1), Some(8)), &mut allow)
            .unwrap()
    );
}

#[test]
fn full_width_identity_keys_and_nested_payloads_survive_outer_projection() {
    let types = [
        GraphSetColumnType::Vertex,
        GraphSetColumnType::Edge,
        GraphSetColumnType::Any,
    ];
    let spec = RowJoinSpec::new(&types, &types, &[(0, 0), (1, 1)])
        .unwrap()
        .with_kind(RowJoinKind::Full);
    let a = GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(u128::MAX)),
        GraphValue::Edge(EId(1_u128 << 100)),
        GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("payload 🦀").unwrap()),
            ]
            .into_boxed_slice(),
        ),
    ]);
    assert!(spec.test_pair_with_control(&a, &a, &mut allow).unwrap());
    let matched = spec
        .project_output_with_control(Some(&a), Some(&a), &mut allow)
        .unwrap();
    assert_eq!(matched.values(), [a.values(), a.values()].concat());
    let missing_left = spec
        .project_output_with_control(None, Some(&a), &mut allow)
        .unwrap();
    assert!(missing_left.values()[..3].iter().all(GraphValue::is_null));
    assert_eq!(&missing_left.values()[3..], a.values());
    let missing_right = spec
        .project_output_with_control(Some(&a), None, &mut allow)
        .unwrap();
    assert_eq!(&missing_right.values()[..3], a.values());
    assert!(missing_right.values()[3..].iter().all(GraphValue::is_null));
}

#[test]
fn invalid_rows_and_impossible_output_shapes_never_reach_an_unchecked_projection() {
    let a = row(Some(1), Some(2));
    let bad = GraphValueRow::unit();
    for kind in KINDS {
        let spec = spec(true, kind);
        assert_eq!(
            spec.test_pair_with_control(&a, &bad, &mut allow),
            Err(RowJoinError::InputSchema { side: 1 })
        );
        assert_eq!(
            spec.validate_input_with_control(0, &bad, &mut allow),
            Err(RowJoinError::InputSchema { side: 0 })
        );
        assert_eq!(
            spec.validate_input_with_control(2, &a, &mut allow),
            Err(RowJoinError::InputSchema { side: 2 })
        );
        assert_eq!(
            spec.project_output_with_control(None, None, &mut allow),
            Err(RowJoinError::InvalidResult)
        );
    }
    let semi = spec(false, RowJoinKind::Semi);
    assert_eq!(
        semi.project_output_with_control(Some(&a), Some(&a), &mut allow),
        Err(RowJoinError::InvalidResult)
    );
    // A genuine matched row containing NULL is not an absent side.
    let full = spec(false, RowJoinKind::Full);
    let null = row(None, None);
    assert_eq!(
        full.project_output_with_control(Some(&null), Some(&null), &mut allow)
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn every_borrowed_match_and_projection_checkpoint_preserves_control_failures() {
    let spec = spec(true, RowJoinKind::Full);
    let a = row(Some(1), Some(2));
    let b = row(Some(1), Some(3));
    for project in [false, true] {
        let mut calls = 0;
        let mut record = |_| {
            calls += 1;
            Ok::<_, usize>(())
        };
        if project {
            spec.project_output_with_control(Some(&a), None, &mut record)
                .unwrap();
        } else {
            assert!(spec.test_pair_with_control(&a, &b, &mut record).unwrap());
        }
        for stop in 0..calls {
            let mut at = 0;
            let mut control = |_| {
                let current = at;
                at += 1;
                if current == stop { Err(stop) } else { Ok(()) }
            };
            let error = if project {
                spec.project_output_with_control(Some(&a), None, &mut control)
                    .unwrap_err()
            } else {
                spec.test_pair_with_control(&a, &b, &mut control)
                    .unwrap_err()
            };
            assert_eq!(error, RowJoinError::Delta(ZSetError::Control(stop)));
            assert_eq!(at, stop + 1);
        }
    }
}
