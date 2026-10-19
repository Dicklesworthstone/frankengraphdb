use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_types::{CommitSeq, EId, PurposeContexts, ids::ObjectId};

type Bounds = (u128, u64, u128, u128);

fn ordered(bounds: &[Bounds]) -> (Vec<Outgoing>, Vec<Incoming>) {
    let mut outgoing = Vec::new();
    let mut incoming = Vec::new();
    for (block, &(source, relation, first, last)) in bounds.iter().enumerate() {
        outgoing.push(Outgoing {
            source: VId(source),
            relation: RelationId(relation),
            block,
        });
        incoming.push(Incoming {
            first: VId(first),
            last: VId(last),
            source: VId(source),
            relation: RelationId(relation),
            block,
            maximum: VId(last),
        });
    }
    crate::tiered::edge_scan::sort(&mut outgoing, &mut || Ok::<_, ()>(())).unwrap();
    crate::tiered::edge_scan::sort(&mut incoming, &mut || Ok::<_, ()>(())).unwrap();
    augment(&mut incoming, &mut || Ok::<_, ()>(())).unwrap();
    (outgoing, incoming)
}

fn lookup(
    outgoing: &[Outgoing],
    incoming: &[Incoming],
    endpoint: u128,
    relation: Option<u64>,
    direction: BufferedEdgeDirection,
) -> (Vec<usize>, usize) {
    let mut candidates = Candidates::new(
        incoming.len(),
        VId(endpoint),
        relation.map(RelationId),
        direction,
    );
    let mut work = 0;
    let mut result = Vec::new();
    while let Some(block) = candidates
        .next(outgoing, incoming, &mut || {
            work += 1;
            Ok::<_, ()>(())
        })
        .unwrap()
    {
        result.push(block);
    }
    result.sort_unstable();
    (result, work)
}

fn oracle(
    bounds: &[Bounds],
    endpoint: u128,
    relation: Option<u64>,
    direction: BufferedEdgeDirection,
) -> Vec<usize> {
    bounds
        .iter()
        .enumerate()
        .filter_map(|(block, &(source, rel, first, last))| {
            let selected = relation.is_none_or(|r| r == rel)
                && match direction {
                    BufferedEdgeDirection::Outgoing => source == endpoint,
                    BufferedEdgeDirection::Incoming => first <= endpoint && endpoint <= last,
                    BufferedEdgeDirection::Undirected => {
                        source == endpoint || (first <= endpoint && endpoint <= last)
                    }
                };
            selected.then_some(block)
        })
        .collect()
}

#[test]
fn descriptor_candidates_equal_linear_oracle_with_overlaps_and_full_width_identities() {
    let mut bounds = vec![
        (0, 0, 0, 0),
        (u128::MAX, u64::MAX, 0, u128::MAX),
        (7, 1, 7, 7),
        (7, 1, 7, 7),
        (7, 2, 3, 19),
        (11, 1, 1, u128::MAX - 1),
        (12, 0, u128::MAX, u128::MAX),
    ];
    let mut seed = 0xb17c_a928_615bu128;
    for at in 0..256 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let source = seed;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let first = seed;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let last = seed;
        bounds.push(if at % 3 == 0 {
            (
                source % 31,
                at % 3,
                first.min(last) % 31,
                first.max(last) % 31 + 31,
            )
        } else {
            (source, at % 3, first.min(last), first.max(last))
        });
    }
    let (outgoing, incoming) = ordered(&bounds);
    let mut endpoints = vec![0, 1, 7, 12, 31, u128::MAX];
    for &(source, _, first, last) in &bounds {
        for value in [source, first, last] {
            endpoints.extend([value.saturating_sub(1), value, value.saturating_add(1)]);
        }
    }
    for endpoint in endpoints {
        for relation in [None, Some(0), Some(1), Some(3), Some(u64::MAX)] {
            for direction in [
                BufferedEdgeDirection::Outgoing,
                BufferedEdgeDirection::Incoming,
                BufferedEdgeDirection::Undirected,
            ] {
                let (actual, _) = lookup(&outgoing, &incoming, endpoint, relation, direction);
                assert_eq!(
                    actual,
                    oracle(&bounds, endpoint, relation, direction),
                    "{endpoint} {relation:?} {direction:?}"
                );
                assert!(
                    actual.windows(2).all(|pair| pair[0] < pair[1]),
                    "no duplicate block"
                );
            }
        }
    }
    assert_eq!(
        lookup(&[], &[], 0, None, BufferedEdgeDirection::Undirected),
        (vec![], 0)
    );
}

#[test]
fn disjoint_routing_is_logarithmic_and_wide_incoming_ranges_do_not_inflate_outgoing_seeks() {
    let count: usize = 4096;
    let bounds: Vec<_> = (0..count)
        .rev()
        .map(|at| {
            (
                at as u128 * 4,
                (at % 3) as u64,
                at as u128 * 4,
                at as u128 * 4 + 1,
            )
        })
        .collect();
    let (outgoing, incoming) = ordered(&bounds);
    for endpoint in [0, 1, 2, 17, 1023, 4097, 16381, 16382, u128::MAX] {
        for direction in [
            BufferedEdgeDirection::Outgoing,
            BufferedEdgeDirection::Incoming,
        ] {
            let (actual, visits) = lookup(&outgoing, &incoming, endpoint, None, direction);
            assert_eq!(actual, oracle(&bounds, endpoint, None, direction));
            assert!(
                visits <= 2 * (count.ilog2() as usize + 1),
                "{endpoint}: {visits}"
            );
        }
    }
    let wide: Vec<_> = bounds
        .iter()
        .map(|&(source, relation, _, _)| (source, relation, 0, u128::MAX))
        .collect();
    let (outgoing, incoming) = ordered(&wide);
    for endpoint in [0, 1, 1024, u128::MAX] {
        let (_, work) = lookup(
            &outgoing,
            &incoming,
            endpoint,
            None,
            BufferedEdgeDirection::Outgoing,
        );
        assert!(work <= count.ilog2() as usize + 2);
    }
    let (actual, work) = lookup(
        &outgoing,
        &incoming,
        7,
        None,
        BufferedEdgeDirection::Incoming,
    );
    assert_eq!(actual, (0..count).collect::<Vec<_>>());
    assert_eq!(work, count);
    assert!(size_of::<Candidates>() <= 2048, "no allocated overlap bag");
}

#[test]
fn every_descriptor_seek_control_can_refuse_before_inspection() {
    let bounds = [
        (7, 1, 0, 100),
        (7, 2, 7, 7),
        (8, 1, 0, 9),
        (0, 1, 0, u128::MAX),
    ];
    let (outgoing, incoming) = ordered(&bounds);
    for direction in [
        BufferedEdgeDirection::Outgoing,
        BufferedEdgeDirection::Incoming,
        BufferedEdgeDirection::Undirected,
    ] {
        for endpoint in [0, 7, 8, u128::MAX] {
            let (_, total) = lookup(&outgoing, &incoming, endpoint, None, direction);
            for stop in 1..=total {
                let mut cursor = Candidates::new(incoming.len(), VId(endpoint), None, direction);
                let mut calls = 0;
                loop {
                    match cursor.next(&outgoing, &incoming, &mut || {
                        calls += 1;
                        if calls == stop { Err(stop) } else { Ok(()) }
                    }) {
                        Ok(Some(_)) => {}
                        Err(at) => {
                            assert_eq!((at, calls), (stop, stop));
                            break;
                        }
                        Ok(None) => panic!("refusal {stop} skipped"),
                    }
                }
            }
        }
    }
}

fn ordered_edges(bounds: &[(u128, u128)]) -> Vec<EdgeRange> {
    let mut ranges: Vec<_> = bounds
        .iter()
        .enumerate()
        .map(|(block, &(first, last))| EdgeRange {
            first: EId(first),
            last: EId(last),
            block,
            maximum: EId(last),
        })
        .collect();
    crate::tiered::edge_scan::sort(&mut ranges, &mut || Ok::<_, ()>(())).unwrap();
    augment_edges(&mut ranges, &mut || Ok::<_, ()>(())).unwrap();
    ranges
}

fn edge_lookup(ranges: &[EdgeRange], eid: u128) -> (Vec<usize>, usize) {
    let mut candidates = EdgeCandidates::new(ranges.len(), EId(eid));
    let mut work = 0;
    let mut result = Vec::new();
    while let Some(block) = candidates
        .next(ranges, &mut || {
            work += 1;
            Ok::<_, ()>(())
        })
        .unwrap()
    {
        result.push(block);
    }
    result.sort_unstable();
    (result, work)
}

#[test]
fn edge_intervals_equal_linear_oracle_with_overlaps_and_full_width_identities() {
    let mut bounds = vec![
        (0, 0),
        (u128::MAX, u128::MAX),
        (0, u128::MAX),
        (7, 7),
        (7, 7),
        (3, 19),
        (1, u128::MAX - 1),
    ];
    let mut seed = 0xed6e_a928_615bu128;
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
    let ranges = ordered_edges(&bounds);
    let mut probes = vec![0, 1, 7, 31, u128::MAX];
    for &(first, last) in &bounds {
        for value in [first, last] {
            probes.extend([value.saturating_sub(1), value, value.saturating_add(1)]);
        }
    }
    for eid in probes {
        let expected: Vec<_> = bounds
            .iter()
            .enumerate()
            .filter_map(|(block, &(first, last))| {
                (first <= eid && eid <= last).then_some(block)
            })
            .collect();
        let (actual, _) = edge_lookup(&ranges, eid);
        assert_eq!(actual, expected, "{eid}");
        assert!(actual.windows(2).all(|pair| pair[0] < pair[1]));
    }
    assert_eq!(edge_lookup(&[], u128::MAX), (vec![], 0));
}

#[test]
fn edge_interval_point_work_is_logarithmic_or_proportional_to_overlaps() {
    let count: usize = 4096;
    let bounds: Vec<_> = (0..count)
        .rev()
        .map(|at| (at as u128 * 4, at as u128 * 4 + 1))
        .collect();
    let ranges = ordered_edges(&bounds);
    for eid in [0, 1, 2, 17, 1023, 4097, 16381, 16382, u128::MAX] {
        let (actual, work) = edge_lookup(&ranges, eid);
        let expected: Vec<_> = bounds
            .iter()
            .enumerate()
            .filter_map(|(block, &(first, last))| {
                (first <= eid && eid <= last).then_some(block)
            })
            .collect();
        assert_eq!(actual, expected);
        assert!(work <= 2 * (count.ilog2() as usize + 1), "{eid}: {work}");
    }
    let wide = ordered_edges(&vec![(0, u128::MAX); count]);
    let (actual, work) = edge_lookup(&wide, 7);
    assert_eq!(actual, (0..count).collect::<Vec<_>>());
    assert_eq!(work, count);
    assert!(size_of::<EdgeCandidates>() <= 2048, "no allocated overlap bag");
}

#[test]
fn every_edge_interval_control_can_refuse_before_inspection() {
    let ranges = ordered_edges(&[(7, 7), (0, 9), (0, u128::MAX), (3, 19)]);
    for eid in [0, 7, 8, 20, u128::MAX] {
        let (_, total) = edge_lookup(&ranges, eid);
        for stop in 1..=total {
            let mut cursor = EdgeCandidates::new(ranges.len(), EId(eid));
            let mut calls = 0;
            loop {
                match cursor.next(&ranges, &mut || {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                }) {
                    Ok(Some(_)) => {}
                    Err(at) => {
                        assert_eq!((at, calls), (stop, stop));
                        break;
                    }
                    Ok(None) => panic!("refusal {stop} skipped"),
                }
            }
        }
    }
}

#[test]
fn routing_construction_is_charged_before_allocation_and_every_refusal_refunds() {
    let (_, report) = run_async_under_lab(0xb0_13, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let extent =
            crate::tiered::buffer::ExtentKey::new(ObjectId([7; 32]), 0, 1, [8; 32]).unwrap();
        let mut blocks: Vec<_> = (0..17)
            .rev()
            .map(|at| BlockDescriptor {
                block: extent,
                properties: None,
                rows: 2,
                summary: Some(super::super::BlockSummary {
                    first_edge: (EId(at), CommitSeq(1)),
                    last_edge: EId(at),
                    source: VId(at * 4),
                    relation: RelationId((at % 3) as u64),
                    first_destination: VId(at * 4),
                    last_destination: VId(at * 4 + 1),
                }),
            })
            .collect();
        blocks.push(BlockDescriptor {
            block: extent,
            properties: None,
            summary: None,
            rows: 0,
        });
        let bytes = blocks.len()
            * (size_of::<Outgoing>() + size_of::<Incoming>() + size_of::<EdgeRange>());
        let short = MemoryPool::new(bytes - 1, 0).unwrap();
        let mut calls = 0;
        assert!(matches!(
            BlockDirectory::new(&blocks, &cx, &short, &mut || {
                calls += 1;
                Ok(())
            }),
            Err(BufferedReadError::Memory(
                MemoryError::ResourceExhausted { .. }
            ))
        ));
        assert_eq!((calls, short.used()), (0, 0));
        let pool = MemoryPool::new(bytes, 0).unwrap();
        let directory = BlockDirectory::new(&blocks, &cx, &pool, &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            (
                directory.outgoing.len(),
                directory.incoming.len(),
                directory.edges.len(),
            ),
            (17, 17, 17)
        );
        assert_eq!(pool.used(), bytes);
        drop(directory);
        assert_eq!(pool.used(), 0);
        for stop in 1..=calls {
            let mut seen = 0;
            let failure = BlockDirectory::new(&blocks, &cx, &pool, &mut || {
                seen += 1;
                if seen == stop {
                    Err(BufferedReadError::Limit {
                        resource: "block directory test control",
                        requested: seen,
                        limit: stop - 1,
                    })
                } else {
                    Ok(())
                }
            });
            assert!(matches!(failure, Err(BufferedReadError::Limit { .. })));
            assert_eq!((seen, pool.used()), (stop, 0));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
