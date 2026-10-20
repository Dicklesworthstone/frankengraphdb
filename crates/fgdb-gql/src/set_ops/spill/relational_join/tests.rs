use super::*;
use crate::algebra::IntegerComparison;
use crate::{
    GqlParameters, GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind,
    PreparedGraphText,
};
use fgdb_delta_types::PropertyKeyId;
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS value", |kind, _: &str| {
        (kind == GraphSymbolKind::Property).then_some(GraphSymbol::Property(PropertyKeyId(1)))
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
    .with_duplicates()
    .into()
}
#[test]
fn all_join_kinds_have_a_sealed_external_source_and_canonical_finish() {
    for kind in [
        RowJoinKind::Inner,
        RowJoinKind::Left,
        RowJoinKind::Right,
        RowJoinKind::Full,
        RowJoinKind::Semi,
        RowJoinKind::Anti,
    ] {
        for keyed in [false, true] {
            // `n.p` is a property column, which is Stored (fgdb-kr8j1).
            let types = [GraphSetColumnType::Stored];
            let spec = if keyed {
                RowJoinSpec::new(&types, &types, &[(0, 0)])
            } else {
                RowJoinSpec::cross(&types, &types)
            }
            .unwrap()
            .with_kind(kind)
            .with_predicate(&[GraphSetPredicateOp::Compare {
                left: GraphSetOperand::Column(0),
                comparison: IntegerComparison::LessOrEqual,
                right: GraphSetOperand::Column(1),
            }])
            .unwrap();
            let query = leaf()
                .with_page(1, Some(2))
                .join(leaf().with_page(0, Some(3)), spec)
                .unwrap();
            let plan = AsyncSpillSetPlan::compile(&query).unwrap();
            let AsyncSpillSetSourcePlan::RowJoin(join) = plan.source() else {
                panic!("row join source");
            };
            assert_eq!(join.kind(), kind);
            assert_eq!(join.left().stages().last().unwrap().offset(), 1);
            assert_eq!(join.right().stages().last().unwrap().count(), Some(3));
            assert!(plan.source_pattern().is_none());
            assert_eq!(plan.source_columns(), query.columns());
        }
    }
}

fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}

fn probe_plan(kind: RowJoinKind, typed: bool, code: &[GraphSetPredicateOp]) -> AsyncSpillSetPlan {
    let types = [GraphSetColumnType::Stored];
    let spec = if typed {
        RowJoinSpec::new(&types, &types, &[(0, 0)])
    } else {
        RowJoinSpec::cross(&types, &types)
    }
    .unwrap()
    .with_kind(kind)
    .with_predicate(code)
    .unwrap();
    AsyncSpillSetPlan::compile(&leaf().join(leaf(), spec).unwrap()).unwrap()
}

fn compiled_join(plan: &AsyncSpillSetPlan) -> &SpillSetJoinPlan {
    let AsyncSpillSetSourcePlan::RowJoin(join) = plan.source() else {
        panic!("row join source");
    };
    join
}

fn allow(_: GlaExecutionEvent) -> Result<(), usize> {
    Ok(())
}

#[test]
fn inferred_probe_admission_preserves_boolean_boundaries_and_physical_stage_counts() {
    use GraphSetPredicateOp::{And, Not, Or, Truth};
    for kind in [
        RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
        RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
    ] {
        for (code, eligible) in [
            (vec![equal(0, 1)], true),
            (vec![equal(1, 0)], true),
            (vec![equal(0, 1), Truth(None), And], true),
            (vec![equal(0, 1), Truth(Some(false)), And, equal(1, 0), Or], true),
            (vec![equal(0, 1), Truth(Some(true)), Or], false),
            (vec![equal(0, 1), Not], false),
            (vec![equal(0, 1), Not, Not], false),
            (vec![Truth(Some(true))], false),
        ] {
            let plan = probe_plan(kind, false, &code);
            let join = compiled_join(&plan);
            assert!(join.keys().is_empty(), "inference must not modify typed keys");
            assert_eq!(join.supports_merge_filtered(), eligible);
            assert!(!join.supports_merge_pairs());
            assert!(!join.supports_merge_presence());
            let keys = join.probe_keys_with_control(&mut allow).unwrap();
            assert_eq!(keys, if eligible { vec![(0, 0)] } else { vec![] });
            assert_eq!(plan.stage_count(), plan.stages().len()
                + join.left().stage_count() + join.right().stage_count()
                + if eligible { 3 } else { 2 });
        }
    }
}

#[test]
fn inferred_numeric_groups_are_candidates_not_typed_conditions_or_predicate_truth() {
    use core::cmp::Ordering;
    use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar as S};
    let row = |v| GraphValueRow::from_owned_values(vec![GraphValue::Scalar(v)]);
    let int = row(S::Int(7));
    let float = row(S::Float(CanonicalF64::new(7.0)));
    let decimal = row(S::Decimal(CanonicalDecimal::from_integer(7).unwrap()));
    let plan = probe_plan(RowJoinKind::Full, false, &[equal(0, 1)]);
    let join = compiled_join(&plan);
    let keys = join.probe_keys_with_control(&mut allow).unwrap();
    assert_eq!(join.compare_probe_keys_with_control(&int, &float, &keys, &mut allow).unwrap(), Ordering::Equal);
    assert!(join.matches_pair_with_control(&int, &float, &mut allow).unwrap());
    assert_eq!(join.compare_probe_keys_with_control(&int, &decimal, &keys, &mut allow).unwrap(), Ordering::Equal);
    assert!(!join.matches_pair_with_control(&int, &decimal, &mut allow).unwrap());
    let null = row(S::Null);
    assert_eq!(join.compare_probe_keys_with_control(&int, &null, &keys, &mut allow).unwrap(), Ordering::Less);
    assert!(!join.matches_pair_with_control(&null, &null, &mut allow).unwrap());
    let large = row(S::Int(9_007_199_254_740_993));
    let rounded = row(S::Float(CanonicalF64::new(9_007_199_254_740_992.0)));
    assert_eq!(join.compare_probe_keys_with_control(&large, &rounded, &keys, &mut allow).unwrap(), Ordering::Greater);
    assert!(!join.matches_pair_with_control(&large, &rounded, &mut allow).unwrap());

    let explicit = probe_plan(RowJoinKind::Full, true, &[equal(0, 1)]);
    let explicit = compiled_join(&explicit);
    assert_eq!(explicit.keys(), &[(0, 0)]);
    assert_eq!(explicit.compare_probe_keys_with_control(&int, &float, &keys, &mut allow).unwrap(), Ordering::Less);
    assert!(!explicit.matches_pair_with_control(&int, &float, &mut allow).unwrap());
}

#[test]
fn every_accepted_scalar_equality_is_in_one_candidate_group() {
    use core::cmp::Ordering;
    use fgdb_types::{CanonicalDecimal, CanonicalF64, CanonicalScalar as S};
    let values = [
        S::Null, S::Bool(false), S::Int(i64::MIN), S::Int(-7), S::Int(0), S::Int(7),
        S::Int(9_007_199_254_740_993), S::Int(i64::MAX),
        S::Float(CanonicalF64::new(f64::NEG_INFINITY)),
        S::Float(CanonicalF64::new(-7.0)), S::Float(CanonicalF64::new(-0.0)),
        S::Float(CanonicalF64::new(7.0)), S::Float(CanonicalF64::new(7.5)),
        S::Float(CanonicalF64::new(9_223_372_036_854_775_808.0)),
        S::Float(CanonicalF64::new(f64::INFINITY)), S::Float(CanonicalF64::new(f64::NAN)),
        S::Decimal(CanonicalDecimal::from_integer(7).unwrap()), S::ucs_basic_text("7").unwrap(),
    ];
    let plan = probe_plan(RowJoinKind::Inner, false, &[equal(0, 1)]);
    let join = compiled_join(&plan);
    let keys = join.probe_keys_with_control(&mut allow).unwrap();
    for a in &values {
        for b in &values {
            let a = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(a.clone())]);
            let b = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(b.clone())]);
            if join.matches_pair_with_control(&a, &b, &mut allow).unwrap() {
                assert_eq!(join.compare_probe_keys_with_control(&a, &b, &keys, &mut allow).unwrap(), Ordering::Equal);
            }
        }
    }
}

#[test]
fn probe_preparation_and_comparison_refusals_are_terminal_and_bad_columns_do_not_panic() {
    use fgdb_types::{CanonicalF64, CanonicalScalar as S};
    let plan = probe_plan(RowJoinKind::Anti, false, &[equal(0, 1)]);
    let join = compiled_join(&plan);
    let left = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(S::Int(7))]);
    let right = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(S::Float(CanonicalF64::new(7.0)))]);
    let exercise = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), usize>| {
        let mut controlled = |event| control(event);
        let keys = join.probe_keys_with_control(&mut controlled)?;
        join.compare_probe_keys_with_control(&left, &right, &keys, &mut controlled)
    };
    let mut calls = 0;
    exercise(&mut |_| { calls += 1; Ok(()) }).unwrap();
    for stop in 1..=calls {
        let mut seen = 0;
        assert_eq!(exercise(&mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(SpillSetRowError::Control(stop)));
        assert_eq!(seen, stop);
    }
    assert!(matches!(join.compare_probe_keys_with_control(&left, &right, &[(9, 0)], &mut allow),
        Err(SpillSetRowError::Native(GraphSetExecutionError::Join(RowJoinError::InputSchema { side: 0 })))));
    assert!(matches!(join.compare_probe_keys_with_control(&left, &GraphValueRow::unit(), &[(0, 0)], &mut allow),
        Err(SpillSetRowError::Native(GraphSetExecutionError::Join(RowJoinError::InputSchema { side: 1 })))));
}
