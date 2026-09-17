//! Public-API composition: arranged join -> DISTINCT -> grouped aggregate ->
//! materialized Z-set. Every participant prepares before any one publishes.
//! These are in-process algebra ticks, not durable graph commits/subscriptions.

use fgdb_delta_types::zset::aggregate::{AggregateDelta, AggregateError, IncrementalAggregate};
use fgdb_delta_types::zset::incremental::{IncrementalDistinct, IncrementalJoin};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use std::collections::{BTreeMap, BTreeSet};

const LIMBS: LimbLimit = LimbLimit::new(16);
type Left = ZSet<(i32, i32)>;
type Right = ZSet<(i32, Option<i128>)>;
type Summary = (i32, i128, i128, Option<i128>, Option<i128>, Option<i128>);

fn allow(_: ZSetEvent) -> Result<(), usize> { Ok(()) }
fn z<T: Ord>(rows: impl IntoIterator<Item = (T, i128)>) -> ZSet<T> {
    ZSet::from_updates(rows.into_iter().map(|(key, w)| (key, ZWeight::from_i128(w))), LIMBS, &mut allow).unwrap()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Circuit {
    join: IncrementalJoin<i32, i32, Option<i128>>,
    distinct: IncrementalDistinct<(i32, i32, Option<i128>)>,
    aggregate: IncrementalAggregate<i32>,
    view: AggregateDelta<i32>,
}

impl Circuit {
    fn tick(
        &mut self,
        left: &Left,
        right: &Right,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), usize>,
    ) -> Result<AggregateDelta<i32>, AggregateError<usize>> {
        let join = self.join.prepare(left, right, LIMBS, control)?;
        let distinct = self.distinct.prepare(join.delta(), LIMBS, control)?;
        let projected = distinct.delta().map(|&(key, _, value)| Ok((key, value)), LIMBS, control)?;
        let aggregate = self.aggregate.prepare(&projected, LIMBS, control)?;
        let view = self.view.prepare_update(aggregate.delta(), LIMBS, control)?;
        // A final cancellation check AFTER every participant has prepared is
        // safe. There must be no fallible work between the following commits.
        control(ZSetEvent::Work).map_err(ZSetError::Control)?;
        view.commit();
        let output = aggregate.commit();
        let _ = distinct.commit();
        let _ = join.commit();
        Ok(output)
    }
}

fn seed_inputs() -> (Left, Right) {
    (
        z([((1, 10), 2), ((1, 20), 1), ((2, 30), 1)]),
        z([((1, Some(3)), 2), ((1, None), 1), ((2, Some(7)), 1)]),
    )
}
fn seeded() -> Circuit {
    let (left, right) = seed_inputs();
    let mut state = Circuit::default();
    state.tick(&left, &right, &mut allow).unwrap();
    state
}
fn plain(view: &AggregateDelta<i32>) -> Vec<Summary> {
    view.iter().map(|((key, row), weight)| {
        assert_eq!(weight, &ZWeight::ONE);
        (*key, row.count_rows().to_i128().unwrap(), row.count_values().to_i128().unwrap(),
         row.sum().map(|sum| sum.to_i128().unwrap()), row.minimum(), row.maximum())
    }).collect()
}

// Independent full cross-product, set conversion and row aggregation. No
// incremental arrangements, retained aggregate summaries or derivative helpers.
fn oracle(left: &Left, right: &Right) -> Vec<Summary> {
    let mut unique = BTreeSet::new();
    for (&(key, source), lw) in left.iter() {
        for (&(other, value), rw) in right.iter() {
            if key == other && lw > &ZWeight::ZERO && rw > &ZWeight::ZERO {
                unique.insert((key, source, value));
            }
        }
    }
    let mut groups = BTreeMap::<i32, Vec<Option<i128>>>::new();
    for (key, _, value) in unique {
        groups.entry(key).or_default().push(value);
    }
    groups.into_iter().map(|(key, rows)| {
        let values: Vec<_> = rows.iter().filter_map(|v| *v).collect();
        (key, rows.len() as i128, values.len() as i128,
         (!values.is_empty()).then(|| values.iter().sum()),
         values.iter().copied().min(), values.iter().copied().max())
    }).collect()
}

#[test]
fn simultaneous_changes_and_summary_neutral_ticks_reach_one_coherent_materialized_view() {
    let (mut left, mut right) = seed_inputs();
    let mut state = seeded();
    assert_eq!(plain(&state.view), vec![(1, 4, 2, Some(6), Some(3), Some(3)), (2, 1, 1, Some(7), Some(7), Some(7))]);
    let dl = z([((1, 10), 1)]);
    let dr = z([((1, Some(3)), 1)]);
    assert!(state.tick(&dl, &dr, &mut allow).unwrap().is_empty());
    left.integrate(&dl, LIMBS, &mut allow).unwrap();
    right.integrate(&dr, LIMBS, &mut allow).unwrap();
    assert_eq!(state.distinct.counts().weight(&(1, 10, Some(3))), Some(&ZWeight::from_i128(9)));
    let frozen = state.view.checked_clone(LIMBS, &mut allow).unwrap();

    // Both sides retract on the SAME tick. The cross term and retained counts
    // from the preceding no-output tick must both participate in this result.
    let dl = z([((1, 10), -3)]);
    let dr = z([((1, Some(3)), -3), ((1, Some(5)), 1)]);
    state.tick(&dl, &dr, &mut allow).unwrap();
    left.integrate(&dl, LIMBS, &mut allow).unwrap();
    right.integrate(&dr, LIMBS, &mut allow).unwrap();
    assert_eq!(plain(&state.view), oracle(&left, &right));
    assert_eq!(plain(&state.view)[0], (1, 2, 1, Some(5), Some(5), Some(5)));
    assert_ne!(frozen, state.view);
    let remove = left.negated(LIMBS, &mut allow).unwrap();
    state.tick(&remove, &Right::new(), &mut allow).unwrap();
    assert!(state.view.is_empty());
    assert_eq!(state.aggregate.group_count(), 0);
    assert!(state.distinct.counts().is_empty());
    assert!(state.tick(&Left::new(), &Right::new(), &mut allow).unwrap().is_empty());
}

#[test]
fn every_pipeline_refusal_including_final_sink_admission_rolls_back_and_can_retry() {
    let dl = z([((1, 10), -2), ((2, 40), 1)]);
    let dr = z([((1, Some(3)), -1), ((1, Some(5)), 1), ((2, None), 1)]);
    let mut expected = seeded();
    let mut total = 0;
    let output = expected.tick(&dl, &dr, &mut |_| {
        total += 1;
        Ok::<_, usize>(())
    }).unwrap();
    for stop in 1..=total {
        let mut state = seeded();
        let mut seen = 0;
        assert_eq!(state.tick(&dl, &dr, &mut |_| {
            seen += 1;
            if seen == stop { Err(stop) } else { Ok(()) }
        }), Err(AggregateError::ZSet(ZSetError::Control(stop))));
        assert_eq!(seen, stop);
        assert_eq!(state, seeded());
        assert_eq!(state.tick(&dl, &dr, &mut allow).unwrap(), output);
        assert_eq!(state, expected);
    }
}

#[test]
fn hundreds_of_pipeline_ticks_match_independent_full_relational_recomputation() {
    let mut state = Circuit::default();
    let (mut left, mut right) = (Left::new(), Right::new());
    let mut seed = 0x6d61_7465_7269_616c_u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((seed >> 32) % 3) as i128
    };
    for _ in 0..300 {
        let mut l = Vec::new();
        let mut r = Vec::new();
        for key in 0..3 {
            for source in 0..2 { l.push(((key, source), next())); }
            for value in [None, Some(-2), Some(5)] { r.push(((key, value), next())); }
        }
        let (new_left, new_right) = (z(l), z(r));
        let dl = new_left.minus(&left, LIMBS, &mut allow).unwrap();
        let dr = new_right.minus(&right, LIMBS, &mut allow).unwrap();
        let mut integrated = state.view.checked_clone(LIMBS, &mut allow).unwrap();
        let output = state.tick(&dl, &dr, &mut allow).unwrap();
        integrated.integrate(&output, LIMBS, &mut allow).unwrap();
        assert_eq!(integrated, state.view);
        assert_eq!(plain(&state.view), oracle(&new_left, &new_right));
        assert_eq!(state.aggregate.snapshot(LIMBS, &mut allow).unwrap(), state.view);
        assert_eq!(state.join.left_rows().map(|(k,v,w)| ((*k,*v),w.to_i128().unwrap())).collect::<Vec<_>>(),
            new_left.iter().map(|(kv,w)| (*kv,w.to_i128().unwrap())).collect::<Vec<_>>());
        assert_eq!(state.join.right_rows().map(|(k,v,w)| ((*k,*v),w.to_i128().unwrap())).collect::<Vec<_>>(),
            new_right.iter().map(|(kv,w)| (*kv,w.to_i128().unwrap())).collect::<Vec<_>>());
        left = new_left;
        right = new_right;
    }
}

#[test]
fn prospective_sink_reads_handle_removal_without_falling_back_and_drop_aborts() {
    let mut sink = z([(1, 2), (2, 3), (3, 4)]);
    let before = sink.checked_clone(LIMBS, &mut allow).unwrap();
    let delta = z([(1, -2), (2, 1), (4, -7)]);
    {
        let pending = sink.prepare_update(&delta, LIMBS, &mut allow).unwrap();
        assert!(pending.weight(&1).is_none());
        assert_eq!(pending.weight(&2), Some(&ZWeight::from_i128(4)));
        assert_eq!(pending.weight(&3), Some(&ZWeight::from_i128(4)));
        assert_eq!(pending.weight(&4), Some(&ZWeight::from_i128(-7)));
        assert!(pending.weight(&5).is_none());
    }
    assert_eq!(sink, before);
    sink.prepare_update(&delta, LIMBS, &mut allow).unwrap().commit();
    assert_eq!(sink, z([(2, 4), (3, 4), (4, -7)]));
}

#[test]
fn arithmetic_failure_in_a_second_sink_cannot_publish_the_prepared_first_sink() {
    let mut first = z([(1, 7)]);
    let mut second = z([(1, 2), (2, i128::MAX)]);
    let first_before = first.checked_clone(LIMBS, &mut allow).unwrap();
    let second_before = second.checked_clone(LIMBS, &mut allow).unwrap();
    let one = z([(1, 1)]);
    let two = z([(1, 1), (2, 1)]);
    let attempt = (|| -> Result<(), ZSetError<usize>> {
        let a = first.prepare_update(&one, LIMBS, &mut allow)?;
        let b = second.prepare_update(&two, LimbLimit::new(0), &mut allow)?;
        a.commit();
        b.commit();
        Ok(())
    })();
    assert!(matches!(attempt, Err(ZSetError::Arithmetic(_))));
    assert_eq!(first, first_before);
    assert_eq!(second, second_before);
    let a = first.prepare_update(&one, LIMBS, &mut allow).unwrap();
    let b = second.prepare_update(&two, LIMBS, &mut allow).unwrap();
    a.commit();
    b.commit();
    assert_eq!(first.weight(&1), Some(&ZWeight::from_i128(8)));
    assert!(second.weight(&2).unwrap().is_promoted());
}

#[test]
fn prepared_and_immediate_sink_paths_share_controls_and_redact_payloads() {
    let delta = z([("secret-key", 919191)]);
    let mut immediate = ZSet::new();
    let mut first_events = Vec::new();
    immediate.integrate(&delta, LIMBS, &mut |e| { first_events.push(e); Ok::<_, usize>(()) }).unwrap();
    let mut prepared = ZSet::new();
    let mut second_events = Vec::new();
    let update = prepared.prepare_update(&delta, LIMBS, &mut |e| { second_events.push(e); Ok::<_, usize>(()) }).unwrap();
    let debug = format!("{update:?}");
    assert!(!debug.contains("secret-key") && !debug.contains("919191"));
    update.commit();
    assert_eq!(first_events, second_events);
    assert_eq!(immediate, prepared);
}
