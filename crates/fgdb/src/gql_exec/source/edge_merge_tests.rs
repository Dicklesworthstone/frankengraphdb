//! fgdb-61p1n: the hashed edge-history merge decides exactly as the ordered
//! map it replaced. Over random block histories (restated, superseded, tied,
//! retired and future statements), both make the same control events in the
//! same order, visit the same winners at the same coordinates, and stop at
//! the same event with the same partial effects when a control call refuses.

use super::{SourceEvent, visit_edge_coordinates, visit_edge_coordinates_in_relation};
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

fn run_selected(
    blocks: &[Vec<AdjacencyEntry>],
    as_of: CommitSeq,
    relation: Option<RelationId>,
    stop: usize,
) -> (Vec<Step>, Result<(), usize>) {
    let log = RefCell::new(Vec::new());
    let calls = Cell::new(0);
    let result = visit_edge_coordinates_in_relation(
        blocks,
        as_of,
        relation,
        &mut |event| {
            calls.set(calls.get() + 1);
            log.borrow_mut().push(Step::Control(event));
            if calls.get() == stop {
                Err(stop)
            } else {
                Ok(())
            }
        },
        |entry, block, row, control| {
            log.borrow_mut()
                .push(Step::Visit(entry.eid, entry.created_at, block, row));
            control(SourceEvent::SnapshotRecord)
        },
    );
    (log.into_inner(), result)
}

#[test]
fn relation_selection_preserves_the_full_merge_winners_and_refusal_prefixes() {
    let mut state = 0x61b1_ed6e_5eed_0003;
    for _ in 0..500 {
        let pool = 1 + next(&mut state) % 24;
        let colliding = next(&mut state).is_multiple_of(3);
        let blocks: Vec<Vec<AdjacencyEntry>> = (0..1 + next(&mut state) % 5)
            .map(|_| {
                (0..next(&mut state) % 14)
                    .map(|_| {
                        let raw = next(&mut state) % pool;
                        let created = next(&mut state) % 8;
                        let retired = next(&mut state)
                            .is_multiple_of(3)
                            .then(|| created + 1 + next(&mut state) % 3);
                        let mut entry = statement(eid(raw, colliding), created, retired);
                        // The source invariant used by the optimization:
                        // every statement of an EId has the same relation.
                        entry.relation = RelationId(1 + raw % 3);
                        entry
                    })
                    .collect()
            })
            .collect();
        let as_of = CommitSeq(next(&mut state) % 10);
        let (full, verdict) = run(false, &blocks, as_of, 0);
        assert_eq!(verdict, Ok(()));
        assert_eq!(
            run_selected(&blocks, as_of, None, 0),
            (full.clone(), Ok(()))
        );
        for relation in [RelationId(1), RelationId(2), RelationId(3), RelationId(4)] {
            let expected: Vec<_> = full
                .iter()
                .filter(|step| match **step {
                    Step::Visit(_, _, block, row) => blocks[block][row].relation == relation,
                    Step::Control(_) => false,
                })
                .copied()
                .collect();
            let (actual, verdict) = run_selected(&blocks, as_of, Some(relation), 0);
            assert_eq!(verdict, Ok(()));
            assert_eq!(
                actual
                    .iter()
                    .filter(|step| matches!(step, Step::Visit(..)))
                    .copied()
                    .collect::<Vec<_>>(),
                expected,
                "relation {relation:?} at {as_of:?}"
            );
            let mut calls = 0;
            for (at, step) in actual.iter().enumerate() {
                if matches!(step, Step::Control(_)) {
                    calls += 1;
                    // A control refusal returns at that exact event, with
                    // no subsequently visited row or observed property.
                    assert_eq!(
                        run_selected(&blocks, as_of, Some(relation), calls),
                        (actual[..=at].to_vec(), Err(calls))
                    );
                }
            }
        }
    }
}

#[test]
fn relation_selection_retains_only_selected_scratch_and_charges_every_raw_row() {
    let mut unrelated: Vec<_> = (1..=4096).map(|id| statement(EId(id), 1, None)).collect();
    for entry in &mut unrelated {
        entry.relation = RelationId(2);
    }
    let selected = vec![
        statement(EId(0), 1, None),
        statement(EId(u128::MAX), 1, None),
    ];
    let successors = vec![
        statement(EId(0), 1, Some(3)),
        statement(EId(0), 3, None),
        statement(EId(u128::MAX), 1, Some(2)),
        statement(EId(4097), 5, None),
    ];
    let blocks = vec![unrelated, selected, successors];
    let as_of = CommitSeq(4);
    let raw_work = blocks.len() + blocks.iter().map(Vec::len).sum::<usize>();
    let (full, full_result) = run(false, &blocks, as_of, 0);
    let (selected, selected_result) = run_selected(&blocks, as_of, Some(RelationId(1)), 0);
    assert_eq!((full_result, selected_result), (Ok(()), Ok(())));
    let count = |steps: &[Step], event| {
        steps
            .iter()
            .filter(|&&step| step == Step::Control(event))
            .count()
    };
    assert_eq!(count(&full, SourceEvent::ScratchEntry), 4098);
    assert_eq!(count(&selected, SourceEvent::ScratchEntry), 2);
    assert_eq!(count(&selected, SourceEvent::Work), raw_work + 2);
    assert_eq!(count(&selected, SourceEvent::SnapshotRecord), 1);
    let (absent, absent_result) = run_selected(&blocks, as_of, Some(RelationId(3)), 0);
    assert_eq!(absent_result, Ok(()));
    assert_eq!(count(&absent, SourceEvent::Work), raw_work);
    assert_eq!(count(&absent, SourceEvent::ScratchEntry), 0);
    assert_eq!(count(&absent, SourceEvent::SnapshotRecord), 0);
}
