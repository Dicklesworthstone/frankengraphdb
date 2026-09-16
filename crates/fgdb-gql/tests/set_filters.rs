//! Relational WHERE filters consume completed rows under the shared set meter.

use fgdb_delta_types::PropertyKeyId;
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValueOrder, GraphValueRow, IntegerComparison};
use fgdb_gql::{GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GqlScalarParameter,
    GraphSetExecutionError, GraphSetFilterError, GraphSetOperand as Arg,
    GraphSetPredicateOp as Op, GraphSetProjection, GraphSetQuantifier,
    GraphSetValue, PreparedGraphSet, MAX_GRAPH_SET_DEPTH};
use fgdb_types::{CanonicalScalar, VId};
use std::cell::{Cell, RefCell};

const P: PropertyKeyId = PropertyKeyId(1);
fn leaf() -> PreparedGraphSet {
    let mut b = GraphPatternBuilder::new();
    b.vertex("n").unwrap();
    b.prepare_values(&[GraphColumn::vertex("n", "n"), GraphColumn::property("p", "n", P)], 0, None)
        .unwrap().with_duplicates().into()
}
fn literal(value: CanonicalScalar) -> Arg { Arg::Literal(GqlScalarParameter::new(value).unwrap()) }
fn test(comparison: IntegerComparison, value: i64) -> Op {
    Op::Compare { left: Arg::Column(1), comparison, right: literal(CanonicalScalar::Int(value)) }
}
fn wide() -> GqlQueryPolicy { GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX) }
fn run(query: &PreparedGraphSet, values: &[CanonicalScalar], policy: GqlQueryPolicy,
    checkpoint: &mut impl FnMut() -> Result<(), usize>)
    -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GraphSetExecutionError<&'static str>, usize>> {
    let checkpoint = RefCell::new(checkpoint);
    query.execute_governed(policy, |pattern, remaining| {
        pattern.plan().execute_governed_with_properties(values.len() as u64,
            (0..values.len()).map(|at| VId(at as u128)), [],
            |_, _| Ok::<_, &'static str>(true),
            |vid, _| Ok(Some(&values[vid.0 as usize])), remaining,
            || (checkpoint.borrow_mut())())
    }, || (checkpoint.borrow_mut())())
}
fn ids(rows: &[GraphValueRow]) -> Vec<VId> {
    rows.iter().map(|row| row.get(0).unwrap().as_vertex().unwrap()).collect()
}

#[test]
fn scalar_filters_use_three_valued_comparisons_not_set_total_order() {
    let values = [CanonicalScalar::Null, CanonicalScalar::Int(1),
        CanonicalScalar::Int(2), CanonicalScalar::Bool(true)];
    for comparison in [IntegerComparison::Equal, IntegerComparison::NotEqual,
        IntegerComparison::Greater, IntegerComparison::Less,
        IntegerComparison::GreaterOrEqual, IntegerComparison::LessOrEqual] {
        for negate in [false, true] {
            let mut ops = vec![test(comparison, 1)];
            if negate { ops.push(Op::Not); }
            let expected = [1_i64, 2].into_iter().enumerate()
                .filter(|(_, n)| comparison.accepts(*n, 1) != negate)
                .map(|(at, _)| VId(at as u128 + 1)).collect::<Vec<_>>();
            assert_eq!(ids(&run(&leaf().filter(&ops).unwrap(), &values, wide(), &mut || Ok(())).unwrap().value), expected);
        }
    }
    for (is_null, expected) in [(true, vec![VId(0)]), (false, vec![VId(1), VId(2), VId(3)])] {
        let query = leaf().filter(&[Op::IsNull { operand: Arg::Column(1), is_null }]).unwrap();
        assert_eq!(ids(&run(&query, &values, wide(), &mut || Ok(())).unwrap().value), expected);
    }
}

#[test]
fn every_three_valued_boolean_pair_retains_only_true() {
    let values = [Some(false), None, Some(true)];
    let and = [[Some(false), Some(false), Some(false)], [Some(false), None, None],
        [Some(false), None, Some(true)]];
    let or = [[Some(false), None, Some(true)], [None, None, Some(true)],
        [Some(true), Some(true), Some(true)]];
    for (a, left) in values.into_iter().enumerate() {
        for (b, right) in values.into_iter().enumerate() {
            for (op, truth) in [(Op::And, and[a][b]), (Op::Or, or[a][b])] {
                for negate in [false, true] {
                    let mut code = vec![Op::Truth(left), Op::Truth(right), op.clone()];
                    if negate { code.push(Op::Not); }
                    let expected = if negate { truth.map(|value| !value) } else { truth };
                    let result = run(&leaf().filter(&code).unwrap(), &[CanonicalScalar::Int(0)], wide(), &mut || Ok(())).unwrap();
                    assert_eq!(result.value.len(), usize::from(expected == Some(true)));
                }
            }
        }
    }
}

#[test]
fn filtering_preserves_projected_bags_and_input_pages() {
    let values = [1, 2, 2, 3, 4].map(CanonicalScalar::Int);
    let filter = [test(IntegerComparison::Less, 4)];
    let paged = leaf().with_order_by(&[GraphValueOrder::descending(1)]).unwrap().with_page(0, Some(2));
    let after_page = paged.filter(&filter).unwrap();
    assert_eq!(ids(&run(&after_page, &values, wide(), &mut || Ok(())).unwrap().value), vec![VId(3)]);
    let before_page = leaf().filter(&filter).unwrap()
        .with_order_by(&[GraphValueOrder::descending(1)]).unwrap().with_page(0, Some(2));
    assert_eq!(ids(&run(&before_page, &values, wide(), &mut || Ok(())).unwrap().value), vec![VId(3), VId(1)]);
    let score = leaf().project(vec![GraphSetProjection::new("score", GraphSetValue::Column(1))], GraphSetQuantifier::All).unwrap();
    let equal = [Op::Compare { left: Arg::Column(0), comparison: IntegerComparison::Equal,
        right: literal(CanonicalScalar::Int(2)) }];
    let bag = score.filter(&equal).unwrap();
    assert_eq!(run(&bag, &values, wide(), &mut || Ok(())).unwrap().value.len(), 2);
    let distinct = bag.project(vec![GraphSetProjection::new("score", GraphSetValue::Column(0))], GraphSetQuantifier::Distinct).unwrap();
    assert_eq!(run(&distinct, &values, wide(), &mut || Ok(())).unwrap().value.len(), 1);
}

#[test]
fn complete_schema_stack_and_depth_validation_precedes_execution() {
    assert!(matches!(leaf().filter(&[]), Err(GraphSetFilterError::Empty)));
    for code in [vec![Op::And], vec![Op::Not], vec![Op::Truth(None), Op::Truth(None)]] {
        assert!(matches!(leaf().filter(&code), Err(GraphSetFilterError::InvalidStack { .. })));
    }
    assert!(matches!(leaf().filter(&[Op::IsNull { operand: Arg::Column(2), is_null: true }]),
        Err(GraphSetFilterError::UnknownInput { column: 2, .. })));
    for (right, comparison) in [(Arg::Column(1), IntegerComparison::Equal), (Arg::Column(0), IntegerComparison::Less)] {
        assert!(matches!(leaf().filter(&[Op::Compare { left: Arg::Column(0), comparison, right }]),
            Err(GraphSetFilterError::InvalidVertexComparison { .. })));
    }
    let mut deepest = leaf();
    for _ in 1..MAX_GRAPH_SET_DEPTH { deepest = deepest.filter(&[Op::Truth(Some(true))]).unwrap(); }
    assert!(matches!(deepest.filter(&[Op::Truth(Some(true))]), Err(GraphSetFilterError::SetBuild(_))));
    let mut too_many = vec![Op::Truth(Some(true))];
    for _ in 0..256 { too_many.extend([Op::Truth(Some(true)), Op::And]); }
    assert!(matches!(leaf().filter(&too_many), Err(GraphSetFilterError::TooManyPredicates { .. })));
    assert!(matches!(leaf().filter(&vec![Op::Not; 1025]), Err(GraphSetFilterError::TooManyInstructions { .. })));
}

#[test]
fn each_resource_dimension_and_every_interruption_boundary_is_shared() {
    let query = leaf().filter(&[test(IntegerComparison::Greater, 1)]).unwrap();
    let values = [1, 2, 3].map(CanonicalScalar::Int);
    let calls = Cell::new(0);
    let measured = run(&query, &values, wide(), &mut || { calls.set(calls.get() + 1); Ok(()) }).unwrap();
    let caps = [measured.rows.snapshot_records, measured.rows.result_rows,
        measured.evaluator.work_units, measured.evaluator.scratch_entries];
    assert_eq!(run(&query, &values, GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]), &mut || Ok(())).unwrap(), measured);
    for dimension in 0..4 {
        let mut short = caps; short[dimension] -= 1;
        assert!(run(&query, &values, GqlQueryPolicy::new(short[0], short[1], short[2], short[3]), &mut || Ok(())).is_err());
    }
    for stop in 1..=calls.get() {
        let mut at = 0;
        let result = run(&query, &values, wide(), &mut || { at += 1; if at == stop { Err(stop) } else { Ok(()) } });
        assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
        assert_eq!(at, stop);
    }
}

#[test]
fn false_filter_and_limit_zero_cannot_hide_source_failure_or_repeat_reads() {
    let query = leaf().filter(&[Op::Truth(Some(false))]).unwrap().with_page(0, Some(0));
    let reads = Cell::new(0);
    let sources = Cell::new(0);
    let result = query.execute_governed(wide(), |pattern, remaining| {
        sources.set(sources.get() + 1);
        pattern.plan().execute_governed_with_properties(2, [VId(0), VId(1)], [],
            |_, _| Ok::<_, &'static str>(true), |_, _| {
                reads.set(reads.get() + 1);
                if reads.get() == 2 { Err("late unreadable row") } else { Ok(None) }
            }, remaining, || Ok::<_, ()>(()))
    }, || Ok::<_, ()>(()));
    assert!(matches!(result, Err(GqlQueryError::Source(GraphSetExecutionError::Source("late unreadable row")))));
    assert_eq!(reads.get(), 2);
    assert_eq!(sources.get(), 1);
}

#[test]
fn canonical_identity_binds_predicates_and_placement_without_exposing_payloads() {
    let original = leaf();
    let bytes = original.canonical_bytes();
    let a = original.clone().filter(&[test(IntegerComparison::Equal, 712_345)]).unwrap();
    let b = original.clone().filter(&[test(IntegerComparison::NotEqual, 712_345)]).unwrap();
    let c = original.clone().filter(&[test(IntegerComparison::Equal, 712_346)]).unwrap();
    assert_ne!(a.canonical_bytes(), bytes);
    assert_ne!(a.canonical_bytes(), b.canonical_bytes());
    assert_ne!(a.canonical_bytes(), c.canonical_bytes());
    assert_eq!(original.canonical_bytes(), bytes);
    assert!(!format!("{a:?} {:?}", test(IntegerComparison::Equal, 712_345)).contains("712345"));
    let truth = [Op::Truth(Some(true))];
    assert_ne!(original.clone().with_page(1, Some(1)).filter(&truth).unwrap().canonical_bytes(),
        original.filter(&truth).unwrap().with_page(1, Some(1)).canonical_bytes());
}
