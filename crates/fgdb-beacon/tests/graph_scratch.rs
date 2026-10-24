//! Allocation admission on the real expansion cursor and graph-fusion path.
//! Expected distances/counts are independent of the private kernel state.

use fgdb_beacon::expansion::{
    ExpansionCursor, ExpansionGraph, ExpansionLimits, ExpansionNeighbors, ExpansionRequest,
    GraphHit, expand_from_membership_filtered,
};
use fgdb_beacon::{BeaconError, GraphHybridQuery, WorkBudget, WorkControl};
use fgdb_types::VId;

struct Control {
    budget: WorkBudget,
    limit: usize,
    used: usize,
    calls: usize,
}

impl Control {
    fn new(limit: usize) -> Self {
        Self {
            budget: WorkBudget::new(1_000_000),
            limit,
            used: 0,
            calls: 0,
        }
    }
}

impl WorkControl for Control {
    fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
        self.budget.charge(units)
    }

    fn reserve_scratch(&mut self, entries: usize) -> Result<(), BeaconError> {
        self.calls += 1;
        self.charge(entries)?;
        if entries > self.limit.saturating_sub(self.used) {
            return Err(BeaconError::ResourceLimit {
                resource: "test scratch",
                limit: self.limit,
            });
        }
        self.used += entries;
        Ok(())
    }
}

struct Source {
    arcs: Vec<(VId, VId)>,
    opened: Vec<VId>,
    swallow: bool,
}

impl Source {
    fn new(arcs: impl IntoIterator<Item = (VId, VId)>) -> Self {
        Self {
            arcs: arcs.into_iter().collect(),
            opened: Vec::new(),
            swallow: false,
        }
    }
}

impl ExpansionNeighbors for Source {
    fn visit_neighbors(
        &mut self,
        vertex: VId,
        work: &mut dyn WorkControl,
        visit: &mut dyn FnMut(VId, &mut dyn WorkControl) -> Result<(), BeaconError>,
    ) -> Result<(), BeaconError> {
        self.opened.push(vertex);
        for &(from, to) in &self.arcs {
            if from == vertex {
                let result = visit(to, work);
                if !self.swallow {
                    result?;
                }
            }
        }
        Ok(())
    }
}

fn corpus(ids: &[VId]) -> ExpansionGraph {
    let mut graph = ExpansionGraph::new(ExpansionLimits::default());
    for &id in ids {
        graph.insert_vertex(id, &mut WorkBudget::new(1000)).unwrap();
    }
    graph
}

#[test]
fn visits_and_both_rankers_share_the_same_allowance() {
    let high = VId(u128::MAX);
    let graph = corpus(&[VId(0), VId(1), high]);
    let run = |limit, accept, k| {
        let mut work = Control::new(limit);
        let mut source = Source::new([
            (VId(0), VId(1)),
            (VId(0), VId(1)),
            (VId(1), high),
            (high, VId(0)),
        ]);
        let result = expand_from_membership_filtered(
            &[VId(0), VId(0), VId(99)],
            2,
            false,
            2,
            ExpansionLimits::default(),
            &graph,
            &mut source,
            |_, _| Ok(accept),
            &mut work,
        )
        .and_then(|hits| {
            if accept {
                assert_eq!(
                    hits,
                    [
                        GraphHit {
                            id: VId(1),
                            hops: 1
                        },
                        GraphHit { id: high, hops: 2 },
                    ]
                );
            }
            GraphHybridQuery::graph_only(k, 2, 100, 60)
                .unwrap()
                .fuse_graph_only(&hits, |_| true, &mut work)
        });
        (result, work.used, source.opened)
    };
    // Three distances + three queue entries + two heap slots + two graph rows;
    // fusion adds two seen IDs, two ranks, two evidence entries, one heap slot
    // and one result. Duplicate seeds/parallel paths allocate no new visits.
    let (result, used, opened) = run(18, true, 1);
    let rows = result.unwrap();
    assert_eq!(used, 18);
    assert_eq!(opened, [VId(0), VId(1)]);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].id, rows[0].graph_hops), (VId(1), Some(1)));
    assert_eq!(
        (rows[0].score.numerator(), rows[0].score.denominator()),
        (100, 61)
    );
    for limit in 0..18 {
        let (result, used, _) = run(limit, true, 1);
        assert!(
            matches!(
                result,
                Err(BeaconError::ResourceLimit {
                    resource: "test scratch",
                    ..
                })
            ),
            "{limit}"
        );
        assert!(used <= limit);
    }
    assert_eq!(run(6, false, 1).1, 6);
    assert!(run(6, false, 1).0.unwrap().is_empty());
    assert!(run(5, false, 1).0.is_err(), "reject-all still traverses");
    assert!(run(10, true, 0).0.unwrap().is_empty());
    assert!(
        run(9, true, 0).0.is_err(),
        "zero final k does not bypass expansion"
    );
}

#[test]
fn heap_replacements_reuse_slots_in_expansion_and_fusion() {
    let graph = corpus(&[VId(0), VId(7), VId(8), VId(9)]);
    let mut source = Source::new([(VId(9), VId(8)), (VId(8), VId(0)), (VId(9), VId(7))]);
    let mut work = Control::new(10);
    let hits = expand_from_membership_filtered(
        &[VId(9)],
        2,
        false,
        1,
        ExpansionLimits::default(),
        &graph,
        &mut source,
        |_, _| Ok(true),
        &mut work,
    )
    .unwrap();
    // VId order ranks (0,2) first; (7,1) replaces it in the one-slot heap.
    assert_eq!(
        hits,
        [GraphHit {
            id: VId(7),
            hops: 1
        }]
    );
    assert_eq!(work.used, 2 * 4 + 1 + 1);
    let population = [
        GraphHit {
            id: VId(9),
            hops: 0,
        },
        GraphHit {
            id: VId(0),
            hops: 1,
        },
        GraphHit {
            id: VId(7),
            hops: 1,
        },
    ];
    let mut work = Control::new(11);
    let rows = GraphHybridQuery::graph_only(1, 3, 100, 60)
        .unwrap()
        .fuse_graph_only(&population, |_| true, &mut work)
        .unwrap();
    // Fusion walks evidence in VId order: the best rank (VId 9) arrives last.
    assert_eq!(rows[0].id, VId(9));
    assert_eq!(work.used, 3 * 3 + 1 + 1);
}

#[test]
fn a_swallowed_allocation_refusal_cannot_resume_traversal() {
    let graph = corpus(&[VId(0), VId(1), VId(2)]);
    let mut source = Source::new([(VId(0), VId(1)), (VId(0), VId(2))]);
    source.swallow = true;
    let mut work = Control::new(3);
    let error = expand_from_membership_filtered(
        &[VId(0)],
        1,
        true,
        1,
        ExpansionLimits::default(),
        &graph,
        &mut source,
        |_, _| panic!("failed traversal cannot rank"),
        &mut work,
    )
    .unwrap_err();
    assert_eq!(
        error,
        BeaconError::ResourceLimit {
            resource: "test scratch",
            limit: 3
        }
    );
    assert_eq!(work.used, 2);
    assert_eq!(work.calls, 2, "no reservation for the later neighbor");
    assert_eq!(source.opened, [VId(0)]);
}

#[test]
fn reservation_refusal_and_unwind_precede_distance_retention() {
    for unwind in [false, true] {
        let seeds = [VId(0)];
        let mut work = Control::new(1);
        let mut cursor =
            ExpansionCursor::new(&seeds, 0, true, 1, ExpansionLimits::default(), &mut work)
                .unwrap();
        assert_eq!(
            cursor.request(&mut work).unwrap(),
            ExpansionRequest::Seed(VId(0))
        );
        if unwind {
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    cursor.resolve_membership(true, || panic!("injected admission unwind"))
                }))
                .is_err()
            );
        } else {
            assert!(
                cursor
                    .resolve_membership(true, || work.reserve_scratch(2))
                    .is_err()
            );
        }
        assert_eq!(cursor.visited_vertices(), 0);
        assert!(cursor.request(&mut work).is_err());
        assert!(cursor.finish(|_, _| Ok(true), &mut work).is_err());
    }
}

#[test]
fn absent_and_disabled_searches_do_not_reserve_visits_or_result_buffers() {
    let graph = corpus(&[VId(0)]);
    for (seed, candidates) in [(VId(99), 1), (VId(0), 0)] {
        let mut work = Control::new(0);
        let mut source = Source::new([]);
        let hits = expand_from_membership_filtered(
            &[seed],
            10,
            true,
            candidates,
            ExpansionLimits::default(),
            &graph,
            &mut source,
            |_, _| panic!("no selected endpoint"),
            &mut work,
        )
        .unwrap();
        assert!(hits.is_empty());
        assert_eq!(work.calls, 0);
        assert!(source.opened.is_empty());
    }
    let mut work = Control::new(0);
    assert!(
        GraphHybridQuery::graph_only(1, 1, 1, 60)
            .unwrap()
            .fuse_graph_only(&[], |_| true, &mut work)
            .unwrap()
            .is_empty()
    );
    assert_eq!(work.calls, 0);
}

#[test]
fn scratch_bounded_execution_matches_independent_min_plus_oracle() {
    let ids = [VId(0), VId(1_u128 << 80), VId(u128::MAX)];
    let graph = corpus(&ids);
    for bits in 0_u16..512 {
        let mut distance = [[99_u32; 3]; 3];
        let mut arcs = Vec::new();
        for (from, distances) in distance.iter_mut().enumerate() {
            distances[from] = 0;
            for (to, value) in distances.iter_mut().enumerate() {
                if bits & (1 << (3 * from + to)) != 0 {
                    *value = (*value).min(1);
                    arcs.push((ids[from], ids[to]));
                }
            }
        }
        for via in 0..3 {
            for from in 0..3 {
                for to in 0..3 {
                    distance[from][to] =
                        distance[from][to].min(distance[from][via] + distance[via][to]);
                }
            }
        }
        for mask in 0_u8..8 {
            for include in [false, true] {
                let mut expected: Vec<_> = (0..3)
                    .filter_map(|to| {
                        let hops = distance[0][to];
                        (hops <= 2 && (include || hops != 0) && mask & (1 << to) != 0)
                            .then_some((hops, ids[to]))
                    })
                    .collect();
                expected.sort();
                expected.truncate(2);
                let expected: Vec<_> = expected
                    .into_iter()
                    .map(|(hops, id)| GraphHit { id, hops })
                    .collect();
                let visited = distance[0].iter().filter(|&&hops| hops <= 2).count();
                let expansion_slots = 2 * visited + 2 * expected.len();
                let mut source = Source::new(arcs.iter().rev().copied());
                let mut work = Control::new(expansion_slots);
                let hits = expand_from_membership_filtered(
                    &[ids[0], ids[0], VId(99)],
                    2,
                    include,
                    2,
                    ExpansionLimits::default(),
                    &graph,
                    &mut source,
                    |id, _| {
                        Ok(ids
                            .iter()
                            .enumerate()
                            .any(|(i, &v)| v == id && mask & (1 << i) != 0))
                    },
                    &mut work,
                )
                .unwrap();
                assert_eq!(hits, expected, "{bits} {mask} {include}");
                assert_eq!(work.used, expansion_slots);
            }
        }
    }
}
