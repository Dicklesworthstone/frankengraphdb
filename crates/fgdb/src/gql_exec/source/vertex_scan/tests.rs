use super::*;
use crate::gql_exec::source::visible_history_row;
use fgdb_delta_types::PropertyKeyId;
use fgdb_strata::vertex::{VertexPatchRows, VertexRow, decode_patch, encode_patch};
use fgdb_types::{CanonicalScalar, CommitSeq};
use std::collections::BTreeMap;

fn patch(mut rows: Vec<VertexRow>) -> VertexPatchRows {
    rows.sort_by_key(|row| (row.vid, row.created_at));
    decode_patch(&encode_patch(&rows).unwrap()).unwrap()
}

fn history_index(patches: &[VertexPatchRows]) -> IndexMap<VId, History> {
    let mut histories = IndexMap::new();
    let mut work = 0;
    for (patch, rows) in patches.iter().enumerate() {
        for (at, row) in rows.iter().enumerate() {
            let history = histories
                .get(&row.vid)
                .cloned()
                .unwrap_or_else(IndexMap::new)
                .insert((row.created_at, patch, at), (), &mut work);
            histories = histories.insert(row.vid, history, &mut work);
        }
    }
    histories
}

fn histories(seed: u64) -> Vec<VertexPatchRows> {
    let mut state = seed + 1;
    let mut random = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut live = BTreeMap::new();
    for id in 0..33_u128 {
        let vid = VId(match id {
            32 => u128::MAX,
            id if id % 2 == 0 => id,
            id => (id << 99) | id,
        });
        live.insert(
            vid,
            VertexRow {
                vid,
                birth_ordinal: id as u64,
                created_at: CommitSeq(1),
                retired_at: None,
                labels: vec![],
                props: vec![(PropertyKeyId(1), CanonicalScalar::Int(1).into())],
            },
        );
    }
    let mut patches = vec![patch(live.values().cloned().collect())];
    for step in 1..8_u64 {
        let at = CommitSeq(3 * step + 1);
        let mut changed = Vec::new();
        let identities: Vec<_> = live.keys().copied().collect();
        for vid in identities {
            let choice = random();
            if choice % 3 != 0 {
                continue;
            }
            let mut previous = live.remove(&vid).unwrap();
            previous.retired_at = Some(at);
            changed.push(previous.clone());
            if choice % 5 != 0 {
                previous.created_at = at;
                previous.retired_at = None;
                previous.props[0].1 = CanonicalScalar::Int(step as i64).into();
                changed.push(previous.clone());
                live.insert(vid, previous);
            }
        }
        // Equal-sequence copies in a later patch must win over the original.
        // This is also a compacted generation's ordinary restatement shape.
        if step % 2 == 0 {
            changed.extend(live.values().filter(|row| row.created_at != at).cloned());
        }
        patches.push(patch(changed));
    }
    patches
}

fn oracle(patches: &[VertexPatchRows], vid: VId, at: CommitSeq) -> Option<&VertexRow> {
    patches
        .iter()
        .enumerate()
        .flat_map(|(patch, rows)| {
            rows.iter()
                .enumerate()
                .map(move |(row, value)| ((value.created_at, patch, row), value))
        })
        .filter(|(_, row)| row.vid == vid && row.created_at <= at)
        .max_by_key(|(coordinate, _)| *coordinate)
        .map(|(_, row)| row)
        .filter(|row| row.retired_at.is_none_or(|retired| at < retired))
}

#[test]
fn ordered_histories_and_reused_winners_match_an_independent_oracle_at_every_cut() {
    for seed in 0..48 {
        let patches = histories(seed);
        let index = history_index(&patches);
        let expected: Vec<_> = index.iter().map(|(vid, _)| *vid).collect();
        for at in 0..=25 {
            let mut scan = VertexHistoryScanCursor::default();
            let mut actual = Vec::new();
            while let Some(vid) = scan.next(&index, &mut |_| Ok::<_, ()>(())).unwrap() {
                actual.push(vid);
                let history = scan.history(vid).expect("current root has a history");
                let row =
                    visible_history_row(&patches, history, CommitSeq(at), &mut |_| Ok::<_, ()>(()))
                        .unwrap();
                assert_eq!(
                    row,
                    oracle(&patches, vid, CommitSeq(at)),
                    "seed={seed} at={at}"
                );
                assert!(
                    scan.history(VId(vid.0 ^ 1)).is_none(),
                    "a probe must still seek"
                );
            }
            assert_eq!(actual, expected);
            assert!(scan.history(VId(u128::MAX)).is_none(), "EOF drops the hint");
        }
    }
}

#[test]
fn every_refused_cursor_event_keeps_the_delivered_prefix_and_current_history() {
    let patches = histories(71);
    let index = history_index(&patches);
    let expected: Vec<_> = index.iter().map(|(vid, _)| *vid).collect();
    let mut events = Vec::new();
    let mut scan = VertexHistoryScanCursor::default();
    while scan
        .next(&index, &mut |event| {
            events.push(event);
            Ok::<_, ()>(())
        })
        .unwrap()
        .is_some()
    {}
    for refused in 0..events.len() {
        let mut scan = VertexHistoryScanCursor::default();
        let mut observed = Vec::new();
        let mut prefix = Vec::new();
        loop {
            match scan.next(&index, &mut |event| {
                observed.push(event);
                if observed.len() - 1 == refused {
                    Err(())
                } else {
                    Ok(())
                }
            }) {
                Ok(Some(vid)) => prefix.push(vid),
                Ok(None) => panic!("refusal {refused} was not observed"),
                Err(()) => break,
            }
        }
        assert_eq!(observed, events[..=refused]);
        assert_eq!(prefix, expected[..prefix.len()]);
        if let Some(&last) = prefix.last() {
            let history = scan.history(last).expect("refusal preserves the last hint");
            assert_eq!(
                visible_history_row(&patches, history, CommitSeq(25), &mut |_| Ok::<_, ()>(()))
                    .unwrap(),
                oracle(&patches, last, CommitSeq(25))
            );
        } else {
            assert!(scan.current.is_none());
        }
        while let Some(vid) = scan.next(&index, &mut |_| Ok::<_, ()>(())).unwrap() {
            prefix.push(vid);
        }
        assert_eq!(prefix, expected, "complete retry after event {refused}");
    }
}

#[test]
fn enumeration_is_linear_and_retains_only_one_lazily_reserved_spine() {
    for count in [0, 1, 64, 4096, 16384_usize] {
        let mut built = 0;
        let history = IndexMap::from_sorted(vec![((CommitSeq(1), 0, 0), ())], &mut built);
        let index = IndexMap::from_sorted(
            (0..count)
                .map(|id| (VId(id as u128), history.clone()))
                .collect(),
            &mut built,
        );
        let height = usize::from(index.height());
        let mut scan = VertexHistoryScanCursor::default();
        assert_eq!(scan.spine.capacity(), 0, "opening must not allocate");
        let (mut work, mut scratch, mut rows) = (0, 0, 0);
        while scan
            .next(&index, &mut |event| {
                match event {
                    SourceEvent::Work => work += 1,
                    SourceEvent::ScratchEntry => scratch += 1,
                    SourceEvent::SnapshotRecord => panic!("source enumeration is not a record"),
                }
                Ok::<_, ()>(())
            })
            .unwrap()
            .is_some()
        {
            rows += 1;
            assert_eq!(
                scan.spine.capacity(),
                height,
                "later pulls cannot grow the spine"
            );
            if rows == 1 {
                assert!(
                    work <= height + 1,
                    "first demand must not traverse the suffix"
                );
            }
        }
        assert_eq!(rows, count);
        assert_eq!(
            work,
            2 * count + 1,
            "one descent and one advance per identity"
        );
        assert_eq!(scratch, height, "the complete capacity is governed once");
    }
}

#[test]
fn a_new_index_generation_cannot_replace_the_pinned_spine_or_history() {
    let patches = histories(92);
    let pinned = history_index(&patches);
    let mut scan = VertexHistoryScanCursor::default();
    let first = scan
        .next(&pinned, &mut |_| Ok::<_, ()>(()))
        .unwrap()
        .unwrap();
    let mut work = 0;
    let changed =
        pinned
            .insert(first, IndexMap::new(), &mut work)
            .insert(VId(1), IndexMap::new(), &mut work);
    assert!(changed.get(&first).unwrap().0.is_none());
    assert!(scan.history(first).unwrap().0.is_some());
    let mut actual = vec![first];
    while let Some(vid) = scan.next(&pinned, &mut |_| Ok::<_, ()>(())).unwrap() {
        actual.push(vid);
    }
    assert_eq!(
        actual,
        pinned.iter().map(|(vid, _)| *vid).collect::<Vec<_>>()
    );
    assert!(!actual.contains(&VId(1)));
}
