//! fgdb-61p1n: the hashed edge-history merge decides exactly as the ordered
//! map it replaced. Over random block histories (restated, superseded, tied,
//! retired and future statements), both make the same control events in the
//! same order, visit the same winners at the same coordinates, and stop at
//! the same event with the same partial effects when a control call refuses.

use super::{SourceEvent, visit_edge_coordinates};
use fgdb_delta_types::RelationId;
use fgdb_strata::AdjacencyEntry;
use fgdb_types::{CommitSeq, EId, VId};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Control(SourceEvent),
    Visit(EId, CommitSeq, usize, usize),
}

/// The replaced merge, kept as the oracle.
fn ordered_map_merge<'a, E, C>(
    blocks: &'a [Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    control: &mut C,
    mut visit: impl FnMut(&'a AdjacencyEntry, usize, usize, &mut C) -> Result<(), E>,
) -> Result<(), E>
where
    C: FnMut(SourceEvent) -> Result<(), E>,
{
    let mut winners: BTreeMap<EId, (&AdjacencyEntry, usize, usize)> = BTreeMap::new();
    for (block_at, block) in blocks.iter().enumerate() {
        control(SourceEvent::Work)?;
        for (row_at, entry) in block.iter().enumerate() {
            control(SourceEvent::Work)?;
            if entry.created_at > as_of {
                continue;
            }
            match winners.get(&entry.eid) {
                Some((previous, _, _)) if previous.created_at > entry.created_at => continue,
                Some(_) => {}
                None => control(SourceEvent::ScratchEntry)?,
            }
            winners.insert(entry.eid, (entry, block_at, row_at));
        }
    }
    for (entry, block, row) in winners.into_values() {
        control(SourceEvent::Work)?;
        if entry.visible_at(as_of) {
            visit(entry, block, row, control)?;
        }
    }
    Ok(())
}

/// Run one merge, refusing the `stop`-th control call (one-based; 0 never).
/// The visit spends a control call of its own, as real admission does.
fn run(
    hashed: bool,
    blocks: &[Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    stop: usize,
) -> (Vec<Step>, Result<(), usize>) {
    let log = RefCell::new(Vec::new());
    let calls = Cell::new(0);
    let mut control = |event| {
        calls.set(calls.get() + 1);
        log.borrow_mut().push(Step::Control(event));
        if calls.get() == stop {
            Err(stop)
        } else {
            Ok(())
        }
    };
    let visit = |entry: &AdjacencyEntry,
                 block,
                 row,
                 control: &mut dyn FnMut(SourceEvent) -> Result<(), usize>| {
        log.borrow_mut()
            .push(Step::Visit(entry.eid, entry.created_at, block, row));
        control(SourceEvent::SnapshotRecord)
    };
    let result = if hashed {
        visit_edge_coordinates(blocks, as_of, &mut control, |entry, block, row, control| {
            visit(entry, block, row, control)
        })
    } else {
        ordered_map_merge(blocks, as_of, &mut control, |entry, block, row, control| {
            visit(entry, block, row, control)
        })
    };
    (log.into_inner(), result)
}

/// xorshift64*: deterministic, dependency-free.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

fn eid(raw: u64, colliding: bool) -> EId {
    // This high half zeroes the mixed product whatever the low half is: every
    // such EId lands in one bucket, so all but a probe run overflow.
    if colliding {
        EId((0xc2b2_ae3d_27d4_eb4f_u128 << 64) | u128::from(raw))
    } else {
        EId(u128::from(raw).wrapping_mul(0x9e37_79b9_7f4a_7c15_f39c_c060_5ced_c835))
    }
}

fn statement(eid: EId, created: u64, retired: Option<u64>) -> AdjacencyEntry {
    AdjacencyEntry {
        src: VId(1),
        relation: RelationId(1),
        dst: VId(2),
        eid,
        created_at: CommitSeq(created),
        retired_at: retired.map(CommitSeq),
    }
}

fn agree(blocks: &[Vec<AdjacencyEntry>], as_of: CommitSeq, state: &mut u64) -> usize {
    let (expected, verdict) = run(false, blocks, as_of, 0);
    assert_eq!(verdict, Ok(()));
    assert_eq!(run(true, blocks, as_of, 0), (expected.clone(), Ok(())));
    let controls = expected
        .iter()
        .filter(|step| matches!(step, Step::Control(_)))
        .count();
    // Every refusal point of a small history, a sample of a large one.
    let stops: Vec<usize> = if controls <= 48 {
        (1..=controls).collect()
    } else {
        (0..24)
            .map(|_| 1 + (next(state) as usize) % controls)
            .chain([1, controls])
            .collect()
    };
    for stop in stops {
        let refused = run(false, blocks, as_of, stop);
        assert_eq!(refused.1, Err(stop));
        assert_eq!(run(true, blocks, as_of, stop), refused, "stop {stop}");
    }
    expected
        .iter()
        .filter(|step| matches!(step, Step::Visit(..)))
        .count()
}

#[test]
fn the_hashed_merge_decides_exactly_as_the_ordered_map() {
    let mut state = 0x61b1_ed6e_5eed_0001;
    let (mut visits, mut restated, mut hidden) = (0, 0, 0);
    for _ in 0..4_000 {
        let pool = 1 + next(&mut state) % 24;
        let colliding = next(&mut state).is_multiple_of(3);
        let blocks: Vec<Vec<AdjacencyEntry>> = (0..1 + next(&mut state) % 5)
            .map(|_| {
                (0..next(&mut state) % 14)
                    .map(|_| {
                        let created = next(&mut state) % 8;
                        let retired = next(&mut state)
                            .is_multiple_of(3)
                            .then(|| created + 1 + next(&mut state) % 3);
                        statement(eid(next(&mut state) % pool, colliding), created, retired)
                    })
                    .collect()
            })
            .collect();
        let rows: usize = blocks.iter().map(Vec::len).sum();
        let as_of = CommitSeq(next(&mut state) % 10);
        let visited = agree(&blocks, as_of, &mut state);
        visits += visited;
        restated += usize::from(rows > pool as usize);
        hidden += usize::from(visited < rows);
    }
    assert!(
        visits > 4_000 && restated > 1_000 && hidden > 1_000,
        "{visits} {restated} {hidden}"
    );
}

/// Identities a host might assign, `(a, a + 1)` and small counters, spread:
/// none needs the ordered overflow. A fold that xors the halves would put the
/// first family into a few dozen buckets.
#[test]
fn structured_identities_spread_without_overflow() {
    let anchor = statement(EId(0), 0, None);
    for structured in [true, false] {
        let mut winners: Vec<super::EdgeWinner<'_>> = Vec::new();
        let mut index = super::WinnerIndex::default();
        for at in 0..200_000u128 {
            let eid = if structured {
                EId((at << 64) | (at + 1))
            } else {
                EId(at)
            };
            assert_eq!(index.find(eid, &winners), None);
            index.insert(eid, &winners);
            winners.push((eid, &anchor, 0, 0));
        }
        assert!(index.overflow.is_empty(), "{}", index.overflow.len());
        for (at, &(eid, ..)) in winners.iter().enumerate().step_by(97) {
            assert_eq!(index.find(eid, &winners), Some(at));
        }
    }
}

/// Thousands of winners through every growth of the index, a third of them in
/// one bucket (so they overflow), restated later with a newer and an older
/// statement.
#[test]
fn the_hashed_merge_survives_growth_and_a_full_collision_cluster() {
    let mut state = 0x61b1_ed6e_5eed_0002;
    let first: Vec<AdjacencyEntry> = (0..6_000u64)
        .map(|at| statement(eid(at, at.is_multiple_of(3)), 2, None))
        .collect();
    let newer = (0..6_000u64)
        .step_by(4)
        .map(|at| statement(eid(at, at.is_multiple_of(3)), 5, Some(7)))
        .collect();
    let older = (0..6_000u64)
        .step_by(5)
        .map(|at| statement(eid(at, at.is_multiple_of(3)), 1, None))
        .collect();
    let blocks = vec![first, newer, older];
    for as_of in [0, 1, 2, 4, 5, 6, 7, 9] {
        let visited = agree(&blocks, CommitSeq(as_of), &mut state);
        let expected = match as_of {
            0 => 0,
            1 => 1_200,
            7 | 9 => 4_500,
            _ => 6_000,
        };
        assert_eq!(visited, expected, "cut {as_of}");
    }
}
