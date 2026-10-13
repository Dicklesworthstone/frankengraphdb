//! Retractable exact numeric groups, independently checked against native batch
//! aggregation and hand-derived controls. No expanded bag in the maintained path.

use fgdb_delta_types::{LimbLimit, PropertyKeyId, ZSet, ZSetError, ZSetEvent, ZWeight};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue, GraphValueRow};
use fgdb_gql::row_aggregate::definition::operator::{GroupError, IncrementalGroupAggregate};
use fgdb_gql::{
    GqlQueryPolicy, GraphAggregate, GraphAggregateValue as Value, GraphExactAverage,
    GraphSetColumnType, PreparedGraphSetAggregate,
};
use fgdb_types::{CanonicalF64, CanonicalPropertyValue, CanonicalScalar, VId};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
type Operator = IncrementalGroupAggregate<PreparedGraphSetAggregate>;

fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}
fn float(value: f64) -> CanonicalScalar {
    CanonicalScalar::Float(CanonicalF64::new(value))
}
fn f(value: f64) -> Value {
    Value::Value(GraphValue::Scalar(float(value)))
}
fn average(numerator: i128, denominator: u64) -> Value {
    Value::Average(GraphExactAverage::new(numerator, denominator).unwrap())
}
fn definition() -> PreparedGraphSetAggregate {
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    let input = source
        .prepare_values(
            &[GraphColumn::property("v", "n", PropertyKeyId(1))],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    PreparedGraphSetAggregate::prepare(
        input.into(),
        &[],
        &[
            GraphAggregate::sum_int("sum", 0),
            GraphAggregate::sum_int_distinct("unique_sum", 0),
            GraphAggregate::average_int("mean", 0),
            GraphAggregate::average_int_distinct("unique_mean", 0),
        ],
        0,
        None,
    )
    .unwrap()
}
fn z(values: impl IntoIterator<Item = (CanonicalScalar, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        values.into_iter().map(|(value, weight)| {
            (
                GraphValueRow::from_owned_values(vec![GraphValue::Scalar(value)]),
                ZWeight::from_i128(weight),
            )
        }),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}
fn seed(input: &ZSet<GraphValueRow>) -> Operator {
    let mut state = Operator::new(definition(), &[GraphSetColumnType::Stored]).unwrap();
    state
        .prepare(input, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    state
}
fn values(state: &Operator) -> Vec<Value> {
    assert_eq!(state.rows().len(), 1);
    let (row, weight) = state.rows().iter().next().unwrap();
    assert_eq!(weight, &ZWeight::ONE);
    row.values().to_vec()
}
fn apply(state: &mut Operator, changes: impl IntoIterator<Item = (CanonicalScalar, i128)>) {
    state
        .prepare(&z(changes), LIMBS, None, &mut allow)
        .unwrap()
        .commit();
}

#[test]
fn exact_cancellation_and_last_float_removal_restore_integer_and_rational_domains() {
    let mut state = seed(&z([
        (float(1e16), 1),
        (float(1.0), 1),
        (float(-1e16), 1),
        (int(3), 2),
        (CanonicalScalar::Null, 4),
    ]));
    assert_eq!(values(&state), vec![f(7.0), f(4.0), f(1.4), f(1.0)]);
    apply(&mut state, [(float(1e16), -1)]);
    apply(&mut state, [(float(-1e16), -1)]);
    assert_eq!(values(&state), vec![f(7.0), f(4.0), f(7.0 / 3.0), f(2.0)]);
    apply(&mut state, [(float(1.0), -1)]);
    assert_eq!(
        values(&state),
        vec![
            Value::Integer(6),
            Value::Integer(3),
            average(3, 1),
            average(3, 1)
        ]
    );
    apply(&mut state, [(int(3), -2)]);
    assert!(values(&state).iter().all(Value::is_null));
    apply(&mut state, [(CanonicalScalar::Null, -4)]);
    assert!(values(&state).iter().all(Value::is_null));
    assert_eq!(state, seed(&ZSet::new()));
}

#[test]
fn overflow_nan_and_infinity_do_not_poison_later_finite_results() {
    let mut state = seed(&z([(float(f64::MAX), 2)]));
    assert_eq!(
        values(&state),
        vec![f(f64::INFINITY), f(f64::MAX), f(f64::MAX), f(f64::MAX)]
    );
    apply(&mut state, [(float(f64::MAX), -1)]);
    assert_eq!(values(&state), vec![f(f64::MAX); 4]);
    apply(&mut state, [(float(-f64::MAX), 1)]);
    assert_eq!(values(&state), vec![f(0.0); 4]);
    apply(
        &mut state,
        [
            (float(f64::INFINITY), 1),
            (float(f64::NEG_INFINITY), 1),
            (float(f64::NAN), 2),
        ],
    );
    assert_eq!(values(&state), vec![f(f64::NAN); 4]);
    apply(
        &mut state,
        [(float(f64::NAN), -1), (float(f64::NEG_INFINITY), -1)],
    );
    assert_eq!(values(&state), vec![f(f64::NAN); 4]);
    apply(&mut state, [(float(f64::NAN), -1)]);
    assert_eq!(values(&state), vec![f(f64::INFINITY); 4]);
    apply(&mut state, [(float(f64::INFINITY), -1)]);
    assert_eq!(values(&state), vec![f(0.0); 4]);
    assert_eq!(
        state,
        seed(&z([(float(f64::MAX), 1), (float(-f64::MAX), 1)]))
    );
}

#[test]
fn distinct_keeps_typed_identity_until_the_last_witness_retracts() {
    let mut state = seed(&z([(int(1), 2), (float(1.0), 3), (float(0.0), 1)]));
    assert_eq!(
        values(&state),
        vec![f(5.0), f(2.0), f(5.0 / 6.0), f(2.0 / 3.0)]
    );
    apply(&mut state, [(float(1.0), -2)]);
    assert_eq!(values(&state), vec![f(3.0), f(2.0), f(0.75), f(2.0 / 3.0)]);
    apply(&mut state, [(float(1.0), -1)]);
    assert_eq!(values(&state), vec![f(2.0), f(1.0), f(2.0 / 3.0), f(0.5)]);
    apply(&mut state, [(float(0.0), -1)]);
    assert_eq!(
        values(&state),
        vec![
            Value::Integer(2),
            Value::Integer(1),
            average(1, 1),
            average(1, 1)
        ]
    );
}

#[test]
fn multiplicities_are_weighted_and_ceiling_replacements_remove_before_adding() {
    let maximum = i128::from(u64::MAX);
    let original = z([(float(0.5), maximum)]);
    let mut state = seed(&original);
    assert_eq!(
        values(&state),
        vec![
            f(f64::from_bits(0x43e0_0000_0000_0000)),
            f(0.5),
            f(0.5),
            f(0.5)
        ]
    );
    let overflow = z([(float(0.25), 1)]);
    assert!(matches!(
        state.prepare(&overflow, LIMBS, None, &mut allow),
        Err(GroupError::Arithmetic)
    ));
    assert_eq!(state, seed(&original));
    // The new value sorts first, but replacing a full population is valid.
    apply(&mut state, [(float(0.25), maximum), (float(0.5), -maximum)]);
    assert_eq!(
        values(&state),
        vec![
            f(f64::from_bits(0x43d0_0000_0000_0000)),
            f(0.25),
            f(0.25),
            f(0.25)
        ]
    );
    // DISTINCT support must not convert an occurrence count to u64 first.
    let mut source = GraphPatternBuilder::new();
    source.vertex("n").unwrap();
    let input = source
        .prepare_values(
            &[GraphColumn::property("v", "n", PropertyKeyId(1))],
            0,
            None,
        )
        .unwrap()
        .with_duplicates();
    let definition = PreparedGraphSetAggregate::prepare(
        input.into(),
        &[],
        &[
            GraphAggregate::sum_int_distinct("s", 0),
            GraphAggregate::average_int_distinct("a", 0),
        ],
        0,
        None,
    )
    .unwrap();
    let mut distinct = Operator::new(definition, &[GraphSetColumnType::Stored]).unwrap();
    apply(&mut distinct, [(float(0.5), i128::MAX)]);
    assert_eq!(values(&distinct), vec![f(0.5); 2]);
}

#[test]
fn every_checkpoint_and_a_dropped_update_preserve_all_accepted_state() {
    let old = z([(float(3.0), 2), (int(2), 1)]);
    let change = z([
        (float(3.0), -2),
        (float(0.25), 4),
        (float(f64::NAN), 1),
        (int(2), -1),
    ]);
    let mut calls = 0;
    let mut measured = seed(&old);
    let update = measured
        .prepare(&change, LIMBS, None, &mut |_| {
            calls += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    drop(update);
    assert_eq!(measured, seed(&old));
    for stop in 1..=calls {
        let mut state = seed(&old);
        let mut seen = 0;
        let error = state
            .prepare(&change, LIMBS, None, &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            })
            .unwrap_err();
        assert!(matches!(error, GroupError::Delta(ZSetError::Control(at)) if at == stop));
        assert_eq!(seen, stop);
        assert_eq!(state, seed(&old), "checkpoint {stop}");
    }
    let mut expected = old.checked_clone(LIMBS, &mut allow).unwrap();
    expected.integrate(&change, LIMBS, &mut allow).unwrap();
    measured
        .prepare(&change, LIMBS, None, &mut allow)
        .unwrap()
        .commit();
    assert_eq!(measured, seed(&expected));
}

fn snapshot(input: &ZSet<GraphValueRow>) -> Vec<Value> {
    // Only this test oracle expands small occurrence bags into source vertices.
    let mut properties = BTreeMap::new();
    let mut id = 1;
    for (row, weight) in input.iter() {
        let GraphValue::Scalar(value) = &row.values()[0] else {
            panic!("test scalar")
        };
        for _ in 0..weight.to_i128().unwrap() {
            properties.insert(VId(id), CanonicalPropertyValue::from(value.clone()));
            id += 1;
        }
    }
    let result = definition()
        .execute_governed(
            GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000),
            |source, policy| {
                source.plan().execute_governed_with_properties(
                    properties.len() as u64,
                    properties.keys().copied(),
                    [],
                    |_, _| Ok::<_, ()>(true),
                    |id, _| Ok(properties.get(&id)),
                    policy,
                    || Ok::<_, usize>(()),
                )
            },
            || Ok::<_, usize>(()),
        )
        .unwrap();
    assert_eq!(result.value.len(), 1);
    result.value[0].values().to_vec()
}

#[test]
fn mixed_numeric_bag_transitions_match_native_batch_and_integrate_exact_deltas() {
    let choices = [
        int(1),
        int(i64::MAX),
        float(1.0),
        float(1e16),
        float(-1e16),
        float(f64::from_bits(1)),
        float(f64::NAN),
        float(f64::INFINITY),
        float(f64::NEG_INFINITY),
        CanonicalScalar::Null,
    ];
    let mut random = 0x17a2_353d_6419_u64;
    let mut previous = ZSet::new();
    let mut state = seed(&previous);
    let mut integrated = state.rows().checked_clone(LIMBS, &mut allow).unwrap();
    for _ in 0..256 {
        let next = z(choices.iter().map(|value| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            (value.clone(), i128::from(random % 3))
        }));
        let delta = next.minus(&previous, LIMBS, &mut allow).unwrap();
        let output = state
            .prepare(&delta, LIMBS, None, &mut allow)
            .unwrap()
            .commit();
        integrated.integrate(&output, LIMBS, &mut allow).unwrap();
        assert_eq!(&integrated, state.rows());
        assert_eq!(values(&state), snapshot(&next));
        assert_eq!(state, seed(&next));
        previous = next;
    }
}
