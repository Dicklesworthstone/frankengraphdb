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

fn fusion_index() -> fgdb_beacon::IndexSnapshot {
    use fgdb_beacon::{BeaconIndex, DistanceMetric, HnswConfig, IndexConfig, IndexDocument};

    BeaconIndex::build(
        IndexConfig {
            vector: Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean).exact_only()),
            ..IndexConfig::default()
        },
        [
            (1, 0.0, "red"),
            (2, 1.0, "blue"),
            (3, 2.0, "red"),
            (4, 3.0, "red"),
        ]
        .map(|(id, coordinate, text)| IndexDocument {
            id: VId(id),
            vector: Some(vec![coordinate]),
            text: Some(text.to_owned()),
        }),
        &mut WorkBudget::new(1_000_000),
    )
    .unwrap()
    .snapshot()
}

fn fusion_query() -> fgdb_beacon::ExactHybridQuery<'static> {
    use fgdb_beacon::{ExactHybridQuery, ExactRrfProfile, TextMatch, VectorSearch};

    ExactHybridQuery {
        vector: &[0.0],
        text: "red",
        boolean_text: None,
        k: 3,
        vector_candidates: 2,
        text_candidates: 2,
        vector_mode: VectorSearch::Exact,
        text_mode: TextMatch::Any,
        profile: ExactRrfProfile::default(),
    }
}

struct CancelScratch {
    calls: usize,
    stop: usize,
}

impl WorkControl for CancelScratch {
    fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
        Ok(())
    }

    fn reserve_scratch(&mut self, _: usize) -> Result<(), BeaconError> {
        self.calls += 1;
        if self.calls == self.stop {
            Err(BeaconError::Cancelled)
        } else {
            Ok(())
        }
    }
}

fn check_fusion_scratch<T: core::fmt::Debug + PartialEq>(
    entries: usize,
    mut run: impl FnMut(&mut dyn WorkControl) -> Result<T, BeaconError>,
) -> T {
    let mut work = Control::new(entries);
    let expected = run(&mut work).unwrap();
    assert_eq!(work.used, entries);
    for limit in 0..entries {
        let mut short = Control::new(limit);
        assert_eq!(
            run(&mut short).unwrap_err(),
            BeaconError::ResourceLimit {
                resource: "test scratch",
                limit,
            }
        );
        assert!(short.used <= limit);
    }
    for stop in 1..=work.calls {
        let mut cancelled = CancelScratch { calls: 0, stop };
        assert_eq!(run(&mut cancelled).unwrap_err(), BeaconError::Cancelled);
        assert_eq!(cancelled.calls, stop);
    }
    // Every failed attempt discarded its private prefix. The same immutable
    // snapshot still returns the complete native answer under work-only control.
    assert_eq!(run(&mut WorkBudget::new(1_000_000)).unwrap(), expected);
    expected
}

#[test]
fn indexed_fusion_admits_the_union_once_before_ranking_and_output() {
    let snapshot = fusion_index();
    let query = fusion_query();
    // Vector [1,2], text [1,3]: U=3. Three evidence entries, three
    // retained heap slots and three result rows: U + 2*min(k,U) = 9.
    let rows = check_fusion_scratch(9, |work| {
        snapshot.hybrid_search_exact_fusion(query, |_| true, work)
    });
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(1), VId(2), VId(3)]
    );
    assert_eq!(
        (rows[0].score.numerator(), rows[0].score.denominator()),
        (2, 61)
    );
    // Disjoint singleton populations: both equal 1/61, ordered by VId.
    let disjoint = fgdb_beacon::ExactHybridQuery {
        text: "blue",
        k: 2,
        vector_candidates: 1,
        text_candidates: 1,
        ..query
    };
    let rows = check_fusion_scratch(6, |work| {
        snapshot.hybrid_search_exact_fusion(disjoint, |_| true, work)
    });
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(1), VId(2)]
    );
    // VId 3 replaces the first heap occupant; replacement is not new scratch.
    // Vector [3,2,4], text [1,3,4]: U=4, k=1, so six entries suffice.
    let replacing = fgdb_beacon::ExactHybridQuery {
        vector: &[2.0],
        k: 1,
        vector_candidates: 3,
        text_candidates: 3,
        ..query
    };
    let rows = check_fusion_scratch(6, |work| {
        snapshot.hybrid_search_exact_fusion(replacing, |_| true, work)
    });
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, VId(3));
}

#[test]
fn filtered_disabled_and_zero_output_fusion_retains_only_admitted_entries() {
    use fgdb_beacon::ExactRrfProfile;

    let snapshot = fusion_index();
    let query = fusion_query();
    // Filtering precedes candidate depths: vector [2,3], text [3].
    let rows = check_fusion_scratch(6, |work| {
        snapshot.hybrid_search_exact_fusion(query, |id| id == VId(2) || id == VId(3), work)
    });
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(3), VId(2)]
    );
    for (profile, expected) in [
        (ExactRrfProfile::new(60, 1, 0).unwrap(), [VId(1), VId(2)]),
        (ExactRrfProfile::new(60, 0, 1).unwrap(), [VId(1), VId(3)]),
    ] {
        let enabled = fgdb_beacon::ExactHybridQuery {
            vector: if profile.vector_weight() == 0 {
                &[f32::NAN]
            } else {
                query.vector
            },
            profile,
            k: 2,
            ..query
        };
        let rows = check_fusion_scratch(6, |work| {
            snapshot.hybrid_search_exact_fusion(enabled, |_| true, work)
        });
        assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), expected);
    }
    assert!(
        check_fusion_scratch(0, |work| {
            snapshot.hybrid_search_exact_fusion(query, |_| false, work)
        })
        .is_empty()
    );
    assert!(
        check_fusion_scratch(0, |work| {
            snapshot.hybrid_search_exact_fusion(
                fgdb_beacon::ExactHybridQuery { k: 0, ..query },
                |_| panic!("zero output does not inspect candidates"),
                work,
            )
        })
        .is_empty()
    );
}

#[test]
fn graph_fusion_shares_indexed_evidence_admission_with_all_three_lanes() {
    let snapshot = fusion_index();
    let query = GraphHybridQuery {
        retrieval: fgdb_beacon::ExactHybridQuery {
            k: 4,
            ..fusion_query()
        },
        graph_candidates: 3,
        graph_weight: 100,
    };
    let graph = [
        GraphHit {
            id: VId(2),
            hops: 0,
        },
        GraphHit {
            id: VId(3),
            hops: 1,
        },
        GraphHit {
            id: VId(9),
            hops: 1,
        },
    ];
    // Three validated graph IDs + three eligible ranks + four UNION evidence
    // entries + four heap slots + four results. IDs 2/3 are not charged twice.
    let rows = check_fusion_scratch(18, |work| {
        snapshot.hybrid_search_graph_filtered(query, &graph, |_| true, work)
    });
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(2), VId(3), VId(9), VId(1)]
    );
    // Rejected graph IDs still validate, but never acquire a rank/evidence.
    // The surviving union is [1,3,9]: 3 + 2 + 3 + 3 + 3 = 14.
    let rows = check_fusion_scratch(14, |work| {
        snapshot.hybrid_search_graph_filtered(query, &graph, |id| id != VId(2), work)
    });
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(3), VId(9), VId(1)]
    );
    // Disabling graph retrieval must still admit every index-lane evidence
    // entry. Its malformed population is unobserved, as the API requires.
    let disabled = GraphHybridQuery {
        retrieval: fusion_query(),
        graph_weight: 0,
        ..query
    };
    let duplicate = [GraphHit {
        id: VId(9),
        hops: 0,
    }; 4];
    let rows = check_fusion_scratch(9, |work| {
        snapshot.hybrid_search_graph_filtered(disabled, &duplicate, |_| true, work)
    });
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        [VId(1), VId(2), VId(3)]
    );
}
