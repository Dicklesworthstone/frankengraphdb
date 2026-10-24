use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_types::{CommitSeq, PurposeContexts, ids::ObjectId};

fn ranges(bounds: &[(u128, u128)]) -> Vec<Range> {
    let mut ranges: Vec<_> = bounds
        .iter()
        .enumerate()
        .map(|(patch, &(first, last))| Range {
            first: VId(first),
            last: VId(last),
            patch,
            maximum: VId(last),
        })
        .collect();
    crate::tiered::edge_scan::sort(&mut ranges, &mut || Ok::<_, ()>(())).unwrap();
    augment(&mut ranges, &mut || Ok::<_, ()>(())).unwrap();
    ranges
}

fn lookup(ranges: &[Range], vid: u128) -> (Vec<usize>, usize) {
    let mut cursor = Candidates::new(ranges.len(), VId(vid));
    let mut visits = 0;
    let mut found = Vec::new();
    while let Some(patch) = cursor
        .next(ranges, &mut || {
            visits += 1;
            Ok::<_, ()>(())
        })
        .unwrap()
    {
        found.push(patch);
    }
    found.sort_unstable();
    (found, visits)
}

#[test]
fn interval_candidates_match_an_independent_linear_scan_for_adversarial_fences() {
    let mut bounds = vec![
        (0, 0),
        (u128::MAX, u128::MAX),
        (0, u128::MAX),
        (1, u128::MAX - 1),
        (7, 7),
        (7, 7),
        (3, 19),
        (11, 13),
        (12, 12),
    ];
    let mut seed = 0xa627_ee31_4d2bu128;
    for at in 0..256 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let first = seed;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let last = seed;
        bounds.push(if at % 3 == 0 {
            (first.min(last) % 31, first.max(last) % 31 + 31)
        } else {
            (first.min(last), first.max(last))
        });
    }
    let indexed = ranges(&bounds);
    let mut queries = vec![0, 1, 7, 12, 31, u128::MAX];
    for &(first, last) in &bounds {
        for endpoint in [first, last] {
            queries.extend([
                endpoint.saturating_sub(1),
                endpoint,
                endpoint.saturating_add(1),
            ]);
        }
    }
    for vid in queries {
        let expected: Vec<_> = bounds
            .iter()
            .enumerate()
            .filter_map(|(patch, &(first, last))| (first <= vid && vid <= last).then_some(patch))
            .collect();
        let (actual, visits) = lookup(&indexed, vid);
        assert_eq!(actual, expected, "identity {vid}");
        assert!(visits <= bounds.len(), "no subtree is revisited");
    }
    assert_eq!(lookup(&[], 0), (vec![], 0));
}

#[test]
fn disjoint_ranges_seek_logarithmically_and_broad_overlap_has_no_candidate_allocation() {
    let count: usize = 4096;
    let bounds: Vec<_> = (0..count)
        .rev()
        .map(|at| (at as u128 * 4, at as u128 * 4 + 1))
        .collect();
    let indexed = ranges(&bounds);
    for vid in [0, 1, 2, 17, 1023, 4097, 16381, 16382, u128::MAX] {
        let (actual, visits) = lookup(&indexed, vid);
        let expected: Vec<_> = bounds
            .iter()
            .enumerate()
            .filter_map(|(patch, &(first, last))| (first <= vid && vid <= last).then_some(patch))
            .collect();
        assert_eq!(actual, expected);
        assert!(
            visits <= 2 * (count.ilog2() as usize + 1),
            "{vid}: {visits}"
        );
    }
    let overlapping = ranges(&vec![(0, u128::MAX); count]);
    let (actual, visits) = lookup(&overlapping, 7);
    assert_eq!(actual, (0..count).collect::<Vec<_>>());
    assert_eq!(visits, count);
    assert!(
        size_of::<Candidates>() <= 2048,
        "lookup state has a fixed small ceiling"
    );
}

#[test]
fn every_lookup_control_prefix_refuses_before_its_node_inspection() {
    let indexed = ranges(&[(0, 100), (7, 7), (0, 9), (8, 200), (0, u128::MAX), (9, 9)]);
    for vid in [0, 7, 8, 9, 100, 101, u128::MAX] {
        let (_, total) = lookup(&indexed, vid);
        for stop in 1..=total {
            let mut cursor = Candidates::new(indexed.len(), VId(vid));
            let mut calls = 0;
            loop {
                match cursor.next(&indexed, &mut || {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                }) {
                    Ok(Some(_)) => {}
                    Err(at) => {
                        assert_eq!(at, stop);
                        assert_eq!(calls, stop);
                        break;
                    }
                    Ok(None) => panic!("refusal {stop} was skipped"),
                }
            }
        }
    }
}

#[test]
fn directory_allocation_is_admitted_and_all_construction_refusals_refund() {
    let (_, report) = run_async_under_lab(0xb0_12, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let extent =
            crate::tiered::buffer::ExtentKey::new(ObjectId([7; 32]), 0, 1, [8; 32]).unwrap();
        let mut patches: Vec<_> = (0..17)
            .rev()
            .map(|at| PatchDescriptor {
                extent,
                first: Some((VId(at * 4), CommitSeq(1))),
                last: Some(VId(at * 4 + 1)),
                rows: 2,
            })
            .collect();
        patches.push(PatchDescriptor {
            extent,
            first: None,
            last: None,
            rows: 0,
        });
        let bytes = patches.len() * size_of::<Range>();
        let short = MemoryPool::new(bytes - 1, 0).unwrap();
        let mut calls = 0;
        assert!(matches!(
            PatchDirectory::new(&patches, &cx, &short, &mut || {
                calls += 1;
                Ok(())
            }),
            Err(BufferedReadError::Memory(
                MemoryError::ResourceExhausted { .. }
            ))
        ));
        assert_eq!(calls, 0, "no directory work precedes allocation admission");
        assert_eq!(short.used(), 0);
        let pool = MemoryPool::new(bytes, 0).unwrap();
        let directory = PatchDirectory::new(&patches, &cx, &pool, &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let total = calls;
        assert_eq!(pool.used(), bytes);
        assert_eq!(
            directory.ranges.len(),
            17,
            "empty patches contribute no interval"
        );
        drop(directory);
        assert_eq!(pool.used(), 0);
        for stop in 1..=total {
            let mut calls = 0;
            let failure = PatchDirectory::new(&patches, &cx, &pool, &mut || {
                calls += 1;
                if calls == stop {
                    Err(BufferedReadError::Limit {
                        resource: "directory test control",
                        requested: calls,
                        limit: stop - 1,
                    })
                } else {
                    Ok(())
                }
            });
            assert!(matches!(failure, Err(BufferedReadError::Limit { .. })));
            assert_eq!(calls, stop);
            assert_eq!(pool.used(), 0, "construction refused at control {stop}");
        }
    });
    assert!(report.invariant_violations.is_empty(), "{report:?}");
}
