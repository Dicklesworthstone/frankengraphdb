use super::*;
use crate::algebra::{GraphValue, IntegerComparison};
use crate::{GraphIntegerBinary as Binary, GraphIntegerExpression, GraphIntegerOp as Op,
    GraphIntegerErrorKind, GraphSetValue};
use fgdb_types::{CanonicalScalar, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(4);
fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn row(value: Option<i64>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
        value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))])
}
fn z(rows: &[(Option<i64>, i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.iter().map(|(v, n)| (row(*v), ZWeight::from_i128(*n))), LIMBS, &mut allow).unwrap()
}
fn spec(q: GraphSetQuantifier) -> RowProjectionSpec {
    let parity = GraphIntegerExpression::prepare(&[Op::Column(0), Op::Literal(Some(2)),
        Op::Binary(Binary::Remainder)]).unwrap();
    RowProjectionSpec::new(vec![GraphSetColumnType::Scalar],
        vec![GraphSetProjection::new("parity", GraphSetValue::Integer(parity))], q).unwrap()
}
fn apply(state: &mut IncrementalRowProjection, delta: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    state.prepare(delta, LIMBS, None, &mut allow).unwrap().commit()
}
fn bag(code: usize) -> ZSet<GraphValueRow> {
    let mut code = code;
    z(&[Some(0), Some(2), Some(3), None].map(|value| {
        let count = (code % 3) as i128; code /= 3; (value, count)
    }))
}
// A primitive full-input grouping oracle, independent of expression bytecode,
// delta mapping, the positive-support kernel and prepared publication.
fn oracle(input: &ZSet<GraphValueRow>, q: GraphSetQuantifier) -> BTreeMap<GraphValueRow, i128> {
    let mut output = BTreeMap::new();
    for (r, count) in input.iter() {
        let value = match r.values()[0] {
            GraphValue::Scalar(CanonicalScalar::Int(value)) => Some(value % 2),
            GraphValue::Scalar(CanonicalScalar::Null) => None,
            _ => unreachable!("fixture domain"),
        };
        *output.entry(row(value)).or_insert(0) += count.to_i128().unwrap();
    }
    if q == GraphSetQuantifier::Distinct { for count in output.values_mut() { *count = 1; } }
    output
}
fn plain(rows: &ZSet<GraphValueRow>) -> BTreeMap<GraphValueRow, i128> {
    rows.iter().map(|(row, count)| (row.clone(), count.to_i128().unwrap())).collect()
}

#[test]
fn all_small_bag_transitions_match_complete_recomputation_and_derivative_integration() {
    for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        for before in 0..81 {
            for after in 0..81 {
                let (old, new) = (bag(before), bag(after));
                let mut state = IncrementalRowProjection::new(spec(q));
                let mut delivered = apply(&mut state, &old);
                assert_eq!(plain(&delivered), oracle(&old, q));
                let difference = new.minus(&old, LIMBS, &mut allow).unwrap();
                let delta = apply(&mut state, &difference);
                delivered.integrate(&delta, LIMBS, &mut allow).unwrap();
                assert_eq!(plain(state.rows()), oracle(&new, q), "{q:?}: {before} -> {after}");
                assert_eq!(delivered, *state.rows());
                assert_eq!(state.total(), &state.rows().total_weight(LIMBS, &mut allow).unwrap());
                apply(&mut state, &difference.negated(LIMBS, &mut allow).unwrap());
                assert_eq!(plain(state.rows()), oracle(&old, q));
            }
        }
    }
}

#[test]
fn collisions_keep_last_support_and_final_quotas_ignore_transient_prefixes() {
    let mut distinct = IncrementalRowProjection::new(spec(GraphSetQuantifier::Distinct));
    apply(&mut distinct, &z(&[(Some(0), 2), (Some(2), 3)]));
    assert!(apply(&mut distinct, &z(&[(Some(0), -2)])).is_empty());
    let swap = z(&[(Some(2), -3), (Some(3), 1)]);
    let delta = distinct.prepare(&swap, LIMBS, Some(1), &mut allow).unwrap().commit();
    assert_eq!(plain(&delta), BTreeMap::from([(row(Some(0)), -1), (row(Some(1)), 1)]));
    let baseline = plain(distinct.rows());
    assert!(matches!(distinct.prepare(&z(&[(Some(0), 1)]), LIMBS, Some(1), &mut allow),
        Err(RowProjectionError::ResultBudget { limit: 1 })));
    assert_eq!(plain(distinct.rows()), baseline);
    distinct.prepare(&z(&[(Some(3), -1)]), LIMBS, Some(0), &mut allow).unwrap().commit();
    assert!(distinct.rows().is_empty());
    let mut all = IncrementalRowProjection::new(spec(GraphSetQuantifier::All));
    apply(&mut all, &z(&[(Some(0), i128::MAX), (Some(2), i128::MAX)]));
    assert!(all.total().is_promoted());
    assert_eq!(all.rows().weight(&row(Some(0))), Some(all.total()));
    let inverse = z(&[(Some(0), -i128::MAX), (Some(2), -i128::MAX)]);
    all.prepare(&inverse, LIMBS, Some(0), &mut allow).unwrap().commit();
    assert!(all.rows().is_empty());
}

#[test]
fn invalid_individual_inputs_cannot_hide_behind_cancelling_projected_images() {
    for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let mut state = IncrementalRowProjection::new(spec(q));
        // Both rows project to 0, but there is no old row 0 to retract.
        assert_eq!(state.prepare(&z(&[(Some(0), -1), (Some(2), 1)]), LIMBS, None, &mut allow)
            .unwrap_err(), RowProjectionError::NegativeMultiplicity);
        assert_eq!(state, IncrementalRowProjection::new(spec(q)));
        let bad = ZSet::from_updates([(GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(u128::MAX))]),
            ZWeight::ONE)], LIMBS, &mut allow).unwrap();
        assert!(matches!(state.prepare(&bad, LIMBS, None, &mut allow), Err(RowProjectionError::InputSchema)));
        let text = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text("not an integer").unwrap())]);
        let bad = ZSet::from_updates([(text, ZWeight::ONE)], LIMBS, &mut allow).unwrap();
        assert!(matches!(state.prepare(&bad, LIMBS, None, &mut allow),
            Err(RowProjectionError::Expression { column: 0, error: GraphIntegerError {
                kind: GraphIntegerErrorKind::NonInteger, .. } })));
        assert!(state.rows().is_empty());
    }
    assert!(matches!(RowProjectionSpec::new(vec![], vec![GraphSetProjection::new("x", GraphSetValue::Column(0))],
        GraphSetQuantifier::All), Err(RowProjectionBuildError::Projection(GraphSetProjectionError::UnknownInput { .. }))));
    assert!(matches!(RowProjectionSpec::new(vec![GraphSetColumnType::Scalar], vec![
        GraphSetProjection::new("x", GraphSetValue::Column(0)), GraphSetProjection::new("x", GraphSetValue::Column(0))],
        GraphSetQuantifier::All), Err(RowProjectionBuildError::Projection(GraphSetProjectionError::DuplicateName { .. }))));
}

#[test]
fn every_checkpoint_drop_and_unwind_preserves_all_arrangements() {
    for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
        let seed = || { let mut state = IncrementalRowProjection::new(spec(q));
            apply(&mut state, &z(&[(Some(0), 2), (Some(3), 1)])); state };
        let delta = z(&[(Some(0), -2), (Some(2), 3), (None, 1)]);
        let mut success = seed();
        let mut calls = 0;
        let expected = success.prepare(&delta, LIMBS, None, &mut |_| {
            calls += 1; Ok::<_, usize>(())
        }).unwrap().commit();
        for stop in 1..=calls {
            let mut state = seed(); let mut seen = 0;
            assert_eq!(state.prepare(&delta, LIMBS, None, &mut |_| {
                seen += 1; if seen == stop { Err(stop) } else { Ok(()) }
            }).unwrap_err(), RowProjectionError::Delta(ZSetError::Control(stop)));
            assert_eq!(seen, stop); assert_eq!(state, seed());
            assert_eq!(apply(&mut state, &delta), expected); assert_eq!(state, success);
        }
        let mut state = seed();
        { let pending = state.prepare(&delta, LIMBS, None, &mut allow).unwrap(); assert_eq!(pending.delta(), &expected); }
        assert_eq!(state, seed());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _pending = state.prepare(&delta, LIMBS, None, &mut allow).unwrap();
            panic!("downstream interruption");
        }));
        assert_eq!(state, seed());
    }
}

#[test]
fn lazy_native_expressions_lists_and_full_width_values_keep_their_domains() {
    let safe = GraphIntegerExpression::prepare(&[Op::Column(0), Op::Literal(Some(0)),
        Op::Compare(IntegerComparison::Equal), Op::Literal(Some(9)), Op::Literal(Some(12)),
        Op::Column(0), Op::Binary(Binary::Divide), Op::Case]).unwrap();
    let projection = vec![GraphSetProjection::new("id", GraphSetValue::Column(1)),
        GraphSetProjection::new("computed", GraphSetValue::Integer(safe)),
        GraphSetProjection::new("list", GraphSetValue::List(vec![GraphSetValue::Column(0), GraphSetValue::Column(1)]))];
    let spec = RowProjectionSpec::new(vec![GraphSetColumnType::Scalar, GraphSetColumnType::Vertex],
        projection, GraphSetQuantifier::All).unwrap();
    let id = GraphValue::Vertex(VId(u128::MAX));
    let input = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(0)), id.clone()]);
    let mut state = IncrementalRowProjection::new(spec);
    let delta = ZSet::from_updates([(input, ZWeight::from_i128(7))], LIMBS, &mut allow).unwrap();
    apply(&mut state, &delta);
    let (result, weight) = state.rows().iter().next().unwrap();
    assert_eq!(result.values()[0], id);
    assert_eq!(result.values()[1], GraphValue::Scalar(CanonicalScalar::Int(9)));
    assert_eq!(result.values()[2].as_list().unwrap()[1], id);
    assert_eq!(weight, &ZWeight::from_i128(7));
    // The same list output can become another checked projection input.
    let next = RowProjectionSpec::new(state.spec().column_types().to_vec(), vec![
        GraphSetProjection::new("size", GraphSetValue::Size(Box::new(GraphSetValue::Column(2))))],
        GraphSetQuantifier::Distinct).unwrap();
    let mut next = IncrementalRowProjection::new(next);
    apply(&mut next, state.rows());
    assert_eq!(plain(next.rows()), BTreeMap::from([(row(Some(2)), 1)]));
}

#[test]
fn changed_key_work_is_independent_of_unrelated_support_and_occurrence_counts() {
    let mut measured = Vec::new();
    for size in [1, 1024] {
        for count in [1, 1_000_000] {
            let mut state = IncrementalRowProjection::new(spec(GraphSetQuantifier::Distinct));
            let mut rows = vec![(Some(0), count)];
            rows.extend((1..size).map(|i| (Some(i * 2), 1)));
            apply(&mut state, &z(&rows));
            let delta = z(&[(Some(0), -count), (Some(1), count)]);
            let mut stats = [0; 2];
            state.prepare(&delta, LIMBS, None, &mut |event| {
                stats[match event { ZSetEvent::Work => 0, ZSetEvent::ScratchEntry => 1 }] += 1;
                Ok::<_, usize>(())
            }).unwrap().commit();
            measured.push(stats);
        }
    }
    // Payload/weight limb accounting can differ for promoted arithmetic. All
    // counts here fit the fast representation; unrelated rows must not be read.
    assert_eq!(measured[0], measured[1]);
    assert_eq!(measured[2], measured[3]);
    // Presence of another parity-0 row changes one output retraction, so compare
    // two large states with the SAME support transition instead of asserting
    // equal work for genuinely different derivatives.
    let mut results = Vec::new();
    for size in [8, 1024] {
        let mut state = IncrementalRowProjection::new(spec(GraphSetQuantifier::All));
        apply(&mut state, &z(&(0..size).map(|i| (Some(i), 1)).collect::<Vec<_>>()));
        let mut stats = [0; 2];
        state.prepare(&z(&[(Some(0), -1), (Some(2048), 1)]), LIMBS, None, &mut |e| {
            stats[match e { ZSetEvent::Work => 0, ZSetEvent::ScratchEntry => 1 }] += 1; Ok::<_, usize>(())
        }).unwrap().commit();
        results.push(stats);
    }
    assert_eq!(results[0], results[1]);
}
