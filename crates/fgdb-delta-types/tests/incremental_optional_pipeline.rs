//! Public composition: optional equijoin -> nullable grouped aggregates -> sink.
//! All participants prepare before any publication, including the null-extension
//! retractions caused by first/last witness transitions.

use fgdb_delta_types::zset::aggregate::{AggregateDelta, IncrementalAggregate};
use fgdb_delta_types::zset::incremental::presence::IncrementalLeftJoin;
use fgdb_delta_types::{LimbLimit, ZSet, ZSetEvent, ZWeight};
use std::collections::BTreeMap;

const LIMBS: LimbLimit = LimbLimit::new(16);
type Left = ZSet<(i32, i32)>;
type Right = ZSet<(i32, Option<i128>)>;
type Summary = (i128, i128, Option<i128>, Option<i128>, Option<i128>);

fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}
fn z<T: Ord + Clone>(rows: &[(T, i128)]) -> ZSet<T> {
    ZSet::from_updates(
        rows.iter()
            .map(|(k, w)| (k.clone(), ZWeight::from_i128(*w))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}

#[derive(Debug, PartialEq, Eq)]
struct Pipeline {
    optional: IncrementalLeftJoin<i32, i32, Option<i128>>,
    aggregate: IncrementalAggregate<i32>,
    sink: AggregateDelta<i32>,
}
impl Pipeline {
    fn new() -> Self {
        Self {
            optional: IncrementalLeftJoin::new(),
            aggregate: IncrementalAggregate::new(),
            sink: ZSet::new(),
        }
    }
    fn tick(
        &mut self,
        left: &Left,
        right: &Right,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), usize>,
    ) -> Result<(), ()> {
        let optional = self
            .optional
            .prepare(left, right, LIMBS, control)
            .map_err(|_| ())?;
        // A matched nullable payload and an absent right binding both supply a
        // null aggregate operand, but their occurrence counts remain different.
        let projected = optional
            .delta()
            .map(
                |(group, _, value)| Ok((*group, value.flatten())),
                LIMBS,
                control,
            )
            .map_err(|_| ())?;
        let aggregate = self
            .aggregate
            .prepare(&projected, LIMBS, control)
            .map_err(|_| ())?;
        let sink = self
            .sink
            .prepare_update(aggregate.delta(), LIMBS, control)
            .map_err(|_| ())?;
        control(ZSetEvent::Work).map_err(|_| ())?;
        let _ = optional.commit();
        let _ = aggregate.commit();
        sink.commit();
        Ok(())
    }
    fn summaries(&self) -> BTreeMap<i32, Summary> {
        assert_eq!(self.sink.len(), self.aggregate.group_count());
        self.sink
            .iter()
            .map(|((group, values), weight)| {
                assert_eq!(weight, &ZWeight::ONE);
                assert_eq!(self.aggregate.get(group), Some(values.as_ref()));
                (
                    *group,
                    (
                        values.count_rows().to_i128().unwrap(),
                        values.count_values().to_i128().unwrap(),
                        values.sum().map(|sum| sum.to_i128().unwrap()),
                        values.minimum(),
                        values.maximum(),
                    ),
                )
            })
            .collect()
    }
}
fn seed() -> Pipeline {
    let mut pipeline = Pipeline::new();
    pipeline
        .tick(
            &z(&[((1, 10), 2), ((2, 20), 1)]),
            &z(&[((1, Some(7)), 2), ((1, None), 1)]),
            &mut allow,
        )
        .unwrap();
    pipeline
}

#[test]
fn nullable_counts_and_extrema_follow_witness_loss_and_simultaneous_left_changes() {
    let mut state = seed();
    assert_eq!(
        state.summaries(),
        BTreeMap::from([
            (1, (6, 4, Some(28), Some(7), Some(7))),
            (2, (1, 0, None, None, None)),
        ])
    );
    let old_sink = state.sink.checked_clone(LIMBS, &mut allow).unwrap();
    let left = z(&[((1, 10), -1), ((1, 11), 3)]);
    let right = z(&[((1, Some(7)), -2), ((1, None), -1), ((2, Some(9)), 2)]);
    state.tick(&left, &right, &mut allow).unwrap();
    assert_eq!(
        state.summaries(),
        BTreeMap::from([
            (1, (4, 0, None, None, None)),
            (2, (2, 2, Some(18), Some(9), Some(9))),
        ])
    );
    assert_eq!(
        old_sink,
        seed().sink,
        "previous output generation was mutated"
    );
    // Replacing a right value at the same key updates extrema but must not
    // create an intermediate unmatched aggregate generation.
    state
        .tick(
            &ZSet::new(),
            &z(&[((2, Some(9)), -2), ((2, Some(-4)), 2)]),
            &mut allow,
        )
        .unwrap();
    assert_eq!(
        state.summaries().get(&2),
        Some(&(2, 2, Some(-8), Some(-4), Some(-4)))
    );
}

#[test]
fn every_pipeline_refusal_including_after_sink_preparation_can_retry_the_same_tick() {
    let left = z(&[((1, 10), -1), ((1, 11), 3)]);
    let right = z(&[((1, Some(7)), -2), ((1, None), -1), ((2, Some(9)), 2)]);
    let mut expected = seed();
    let mut calls = 0;
    expected
        .tick(&left, &right, &mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap();
    for stop in 1..=calls {
        let mut state = seed();
        let mut seen = 0;
        assert!(
            state
                .tick(&left, &right, &mut |_| {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                })
                .is_err()
        );
        assert_eq!(seen, stop);
        assert_eq!(state, seed());
        state.tick(&left, &right, &mut allow).unwrap();
        assert_eq!(state, expected);
    }
}

// Expand only these tiny test bags, then evaluate ordinary outer-join/aggregate
// definitions. No incremental operator, witness count or aggregate state is used.
fn recompute(
    left: &BTreeMap<(i32, i32), i128>,
    right: &BTreeMap<(i32, Option<i128>), i128>,
) -> BTreeMap<i32, Summary> {
    let mut rows = BTreeMap::<i32, Vec<Option<i128>>>::new();
    for (&(key, _), &count) in left {
        for _ in 0..count {
            let mut found = false;
            for (&(other, value), &copies) in right {
                if key == other {
                    for _ in 0..copies {
                        rows.entry(key).or_default().push(value);
                        found = true;
                    }
                }
            }
            if !found {
                rows.entry(key).or_default().push(None);
            }
        }
    }
    rows.into_iter()
        .map(|(key, rows)| {
            let values: Vec<_> = rows.iter().filter_map(|value| *value).collect();
            let summary = (
                rows.len() as i128,
                values.len() as i128,
                (!values.is_empty()).then(|| values.iter().sum()),
                values.iter().min().copied(),
                values.iter().max().copied(),
            );
            (key, summary)
        })
        .collect()
}

#[test]
fn repeated_optional_and_aggregate_ticks_match_independent_whole_bag_recomputation() {
    let mut state = Pipeline::new();
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    let mut random = 0x9175_u64;
    let mut next = || {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        random >> 32
    };
    for _ in 0..500 {
        let lk = ((next() % 5) as i32, (next() % 4) as i32);
        let rk = (
            (next() % 5) as i32,
            match next() % 4 {
                0 => None,
                n => Some(n as i128 - 2),
            },
        );
        let lc = (next() % 4) as i128;
        let rc = (next() % 4) as i128;
        let dl = z(&[(lk, lc - left.get(&lk).copied().unwrap_or(0))]);
        let dr = z(&[(rk, rc - right.get(&rk).copied().unwrap_or(0))]);
        state.tick(&dl, &dr, &mut allow).unwrap();
        if lc == 0 {
            left.remove(&lk);
        } else {
            left.insert(lk, lc);
        }
        if rc == 0 {
            right.remove(&rk);
        } else {
            right.insert(rk, rc);
        }
        assert_eq!(state.summaries(), recompute(&left, &right));
    }
}
