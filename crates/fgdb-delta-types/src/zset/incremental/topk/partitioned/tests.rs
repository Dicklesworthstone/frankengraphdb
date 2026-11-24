use super::*;

const LIMBS: LimbLimit = LimbLimit::new(16);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Key(i32, i32);

impl PartitionedKey for Key {
    type Partition = i32;

    fn partition(&self) -> i32 {
        self.0
    }
}

fn allow(_: ZSetEvent) -> Result<(), usize> {
    Ok(())
}

fn z(rows: impl IntoIterator<Item = (Key, i128)>) -> ZSet<Key> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(key, weight)| (key, ZWeight::from_i128(weight))),
        LIMBS,
        &mut allow,
    )
    .unwrap()
}

fn plain(rows: &ZSet<Key>) -> Vec<(Key, i128)> {
    rows.iter()
        .map(|(key, weight)| (key.clone(), weight.to_i128().unwrap()))
        .collect()
}

fn input(code: i128) -> ZSet<Key> {
    z([
        (Key(0, 1), code % 3),
        (Key(0, 2), code / 3 % 3),
        (Key(1, 1), code / 9 % 3),
    ])
}

// Deliberately expand small occurrences and page each independently sorted
// partition. This oracle shares neither weighted selection nor rank state.
fn batch(rows: &ZSet<Key>, offset: u64, count: u64) -> ZSet<Key> {
    let mut partitions = BTreeMap::<i32, Vec<Key>>::new();
    for (key, weight) in rows.iter() {
        for _ in 0..weight.to_i128().unwrap() {
            partitions.entry(key.0).or_default().push(key.clone());
        }
    }
    z(partitions.into_values().flat_map(|mut rows| {
        rows.sort();
        rows.into_iter()
            .skip(offset as usize)
            .take(count as usize)
            .map(|key| (key, 1))
    }))
}

#[test]
fn every_small_partitioned_transition_matches_independent_occurrence_pages() {
    for (offset, count) in [(0, 0), (0, 1), (1, 1), (1, 3)] {
        for before in 0..27 {
            for after in 0..27 {
                let initial = input(before);
                let final_input = input(after);
                let changes = final_input.minus(&initial, LIMBS, &mut allow).unwrap();
                let mut window = IncrementalPartitionedTopK::new(offset, count);
                let mut output = window.apply(&initial, LIMBS, &mut allow).unwrap();
                assert_eq!(output, batch(&initial, offset, count));
                let pending = window.prepare(&changes, LIMBS, &mut allow).unwrap();
                let expected = batch(&final_input, offset, count);
                let tentative: Vec<_> = pending
                    .rows()
                    .map(|(key, weight)| (key.clone(), weight.to_i128().unwrap()))
                    .collect();
                assert_eq!(tentative, plain(&expected));
                assert_eq!(
                    pending
                        .rows()
                        .rev()
                        .map(|(key, weight)| (key.clone(), weight.to_i128().unwrap()))
                        .collect::<Vec<_>>(),
                    plain(&expected).into_iter().rev().collect::<Vec<_>>()
                );
                output
                    .integrate(&pending.commit(), LIMBS, &mut allow)
                    .unwrap();
                assert_eq!(output, expected);
                assert_eq!(window.rows(), &expected);
                assert_eq!(
                    window.partition_count(),
                    usize::from(after % 9 != 0) + usize::from(after / 9 != 0)
                );
            }
        }
    }
}

#[test]
fn cancellation_and_drop_leave_all_partitions_and_output_unchanged() {
    let seed = z([(Key(0, 1), 2), (Key(0, 2), 1), (Key(1, 7), 1)]);
    let changes = z([(Key(0, 1), -2), (Key(1, 7), -1), (Key(2, 3), 1)]);
    let seeded = || {
        let mut window = IncrementalPartitionedTopK::new(0, 1);
        window.apply(&seed, LIMBS, &mut allow).unwrap();
        window
    };
    let mut success = seeded();
    let mut events = 0;
    success
        .apply(&changes, LIMBS, &mut |_| {
            events += 1;
            Ok::<_, usize>(())
        })
        .unwrap();
    assert_eq!(plain(success.rows()), vec![(Key(0, 2), 1), (Key(2, 3), 1)]);
    assert_eq!(success.partition_count(), 2);
    for stop in 1..=events {
        let mut window = seeded();
        let mut at = 0;
        let result = window.prepare(&changes, LIMBS, &mut |_| {
            at += 1;
            if at == stop { Err(stop) } else { Ok(()) }
        });
        assert!(
            matches!(result, Err(TopKError::Delta(ZSetError::Control(error))) if error == stop)
        );
        assert_eq!(window, seeded());
        window.apply(&changes, LIMBS, &mut allow).unwrap();
        assert_eq!(window, success);
    }
    let mut dropped = seeded();
    drop(dropped.prepare(&changes, LIMBS, &mut allow).unwrap());
    assert_eq!(dropped, seeded());
    let invalid = z([(Key(0, 9), -1), (Key(2, 1), 1)]);
    assert!(matches!(
        dropped.apply(&invalid, LIMBS, &mut allow),
        Err(TopKError::NegativeMultiplicity)
    ));
    assert_eq!(dropped, seeded());
}

#[test]
fn unchanged_partitions_are_not_visited_and_tentative_rows_iterate_both_ends() {
    fn measured(extra: i32) -> (usize, IncrementalPartitionedTopK<Key>) {
        let mut window = IncrementalPartitionedTopK::new(0, 1);
        let seed = z((0..=extra).flat_map(|group| [(Key(group, 1), 1), (Key(group, 2), 1)]));
        window.apply(&seed, LIMBS, &mut allow).unwrap();
        let changes = z([(Key(0, 1), -1)]);
        let mut events = 0;
        let pending = window
            .prepare(&changes, LIMBS, &mut |_| {
                events += 1;
                Ok::<_, usize>(())
            })
            .unwrap();
        let mut rows = pending.rows();
        assert_eq!(rows.len(), extra as usize + 1);
        assert_eq!(rows.next().map(|(key, _)| key.clone()), Some(Key(0, 2)));
        if extra > 0 {
            assert_eq!(
                rows.next_back().map(|(key, _)| key.clone()),
                Some(Key(extra, 1))
            );
        }
        while rows.next().is_some() {}
        assert_eq!(rows.len(), 0);
        assert!(rows.next_back().is_none());
        drop(rows);
        pending.commit();
        (events, window)
    }
    let (small, _) = measured(0);
    let (large, mut window) = measured(1024);
    assert_eq!(
        small, large,
        "unaffected populations add no logical visit or copy events"
    );
    window
        .apply(&z([(Key(0, 2), -1)]), LIMBS, &mut allow)
        .unwrap();
    assert_eq!(window.partition_count(), 1024);
    assert!(window.weight(&0, &Key(0, 2)).is_none());
}
