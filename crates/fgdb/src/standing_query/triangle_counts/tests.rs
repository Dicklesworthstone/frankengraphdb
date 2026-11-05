use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{CoordinateEntry, DeltaRow};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{DatabaseSecurityNamespaceId, EId, ObjectId, PurposeContexts};
use std::collections::BTreeSet;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn build(snapshot: &crate::Snapshot) -> State {
    build_as(snapshot, Output::Triangles)
}

fn build_as(snapshot: &crate::Snapshot, output: Output) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_snapshot(snapshot, Some(RelationId(1)), output, &mut meter).unwrap()
}

fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.triangles, before.triangles);
    assert_eq!(actual.counts, before.counts);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.failure, before.failure);
    assert_eq!(actual.policy, before.policy);
    assert_eq!(actual.relation, before.relation);
    assert_eq!(actual.selected, before.selected);
}

#[test]
fn every_input_kernel_counter_sink_and_final_refusal_preserves_the_accepted_generation() {
    let ((), report) = run_async_under_lab(0x7472_6301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for v in 0..4 {
            seed.create_vertex(VId(v), vec![], vec![]);
        }
        for (id, a, b) in [(10, 0, 1), (11, 1, 2), (12, 2, 0), (13, 1, 1), (14, 0, 1)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let baseline = Arc::clone(&db.snapshot);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(10));
        change.delete_vertex(VId(1));
        change.add_edge(EId(15), VId(0), VId(3), vec![]);
        change.add_edge(EId(16), VId(3), VId(2), vec![]);
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
        for output in [Output::Triangles, Output::Clustering] {
            let build = |snapshot: &crate::Snapshot| build_as(snapshot, output);
            let before = build(&baseline);
            assert!(before.delta().is_none());
            let expected = build(&db.snapshot);
            assert_eq!(expected.rows().len(), 3);
            assert_ne!(expected.rows(), before.rows());
            let mut accepted = build(&baseline);
            let mut calls = 0;
            let stats = {
                let mut checkpoint = || {
                    calls += 1;
                    Ok(())
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                accepted.maintain(&commit, batch, &mut meter).unwrap();
                meter.stats
            };
            assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
            assert_eq!(accepted.rows(), expected.rows());
            assert_eq!(accepted.counts.vertices, expected.counts.vertices);
            assert_eq!(accepted.triangles, expected.triangles);
            assert_eq!(accepted.input, expected.input);
            assert!(accepted.delta().is_some());
            for stop in 1..=calls {
                let mut state = build(&baseline);
                let mut seen = 0;
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryFailure::Interrupted)
                    } else {
                        Ok(())
                    }
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    state.maintain(&commit, batch, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                );
                assert_eq!(seen, stop);
                unchanged(&state, &before);
            }
            for (rows, work, scratch, error) in [
                (3, stats.work_units, stats.scratch_entries, None),
                (
                    2,
                    stats.work_units,
                    stats.scratch_entries,
                    Some(StandingQueryFailure::ResultBudget),
                ),
                (
                    3,
                    stats.work_units - 1,
                    stats.scratch_entries,
                    Some(StandingQueryFailure::WorkBudget),
                ),
                (
                    3,
                    stats.work_units,
                    stats.scratch_entries - 1,
                    Some(StandingQueryFailure::ScratchBudget),
                ),
            ] {
                let mut state = build(&baseline);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: GqlQueryPolicy::new(100_000, rows, work, scratch),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                let result = state.maintain(&commit, batch, &mut meter);
                if let Some(error) = error {
                    assert_eq!(result, Err(error));
                    unchanged(&state, &before);
                } else {
                    result.unwrap();
                    assert_eq!(state.rows(), expected.rows());
                    assert_eq!(state.counts.vertices, expected.counts.vertices);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn zset<T: Ord>(rows: impl IntoIterator<Item = (T, i128)>) -> ZSet<T> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, weight)| (row, ZWeight::from_i128(weight))),
        LIMBS,
        &mut |_| Ok::<_, StandingQueryFailure>(()),
    )
    .unwrap()
}

fn synthetic(triangles: i64, incidences: i128) -> Counts {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let mut state = Counts::new(Output::Triangles, &mut meter).unwrap();
    for vertex in [VId(0), VId(1), VId(u128::MAX)] {
        state.vertices.insert(
            vertex,
            Counters {
                triangles: u64::try_from(triangles).unwrap(),
                degree: 0,
                incidences,
            },
        );
        state
            .rows
            .accumulate(
                row(vertex, CanonicalScalar::Int(triangles), &mut meter).unwrap(),
                ZWeight::ONE,
                LIMBS,
                &mut |_| Ok::<_, StandingQueryFailure>(()),
            )
            .unwrap();
    }
    state
}

#[test]
fn numeric_liveness_and_incidence_refusals_are_atomic_and_empty_ticks_are_distinct() {
    let a = VId(0);
    let b = VId(1);
    let c = VId(u128::MAX);
    let empty_v = ZSet::new();
    let empty_e = ZSet::new();
    let empty_t = ZSet::new();
    let cases = [
        (
            i64::MAX,
            2,
            zset([]),
            zset([]),
            zset([((a, b, c), 1)]),
            StandingQueryFailure::Arithmetic,
        ),
        (
            0,
            0,
            zset([]),
            zset([]),
            zset([((a, b, c), -1)]),
            StandingQueryFailure::InvalidDelta,
        ),
        (
            0,
            0,
            zset([]),
            zset([((a, b), -1)]),
            zset([]),
            StandingQueryFailure::InvalidDelta,
        ),
        (
            0,
            1,
            zset([(a, -1)]),
            zset([]),
            zset([]),
            StandingQueryFailure::InvalidDelta,
        ),
        (
            0,
            0,
            zset([(a, -2)]),
            zset([]),
            zset([]),
            StandingQueryFailure::InvalidDelta,
        ),
        (
            0,
            0,
            zset([]),
            zset([((a, VId(9)), 1)]),
            zset([]),
            StandingQueryFailure::InvalidDelta,
        ),
        (
            0,
            0,
            zset([]),
            zset([]),
            zset([((c, b, a), 1)]),
            StandingQueryFailure::InvalidDelta,
        ),
        (
            0,
            0,
            zset([]),
            zset([]),
            zset([((a, b, c), 2)]),
            StandingQueryFailure::InvalidDelta,
        ),
    ];
    for (number, incidence, vertices, edges, triangles, error) in cases {
        let before = synthetic(number, incidence);
        let mut state = synthetic(number, incidence);
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        assert_eq!(
            state
                .prepare(
                    &vertices,
                    &edges,
                    std::iter::empty(),
                    &triangles,
                    &mut meter
                )
                .map(CountsUpdate::commit),
            Err(error)
        );
        assert_eq!(state, before);
    }
    let mut state = synthetic(0, 0);
    assert!(state.last_delta.is_none());
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state
        .prepare(&empty_v, &empty_e, std::iter::empty(), &empty_t, &mut meter)
        .unwrap()
        .commit();
    assert!(state.last_delta.as_ref().unwrap().is_empty());
    assert_eq!(state.rows.len(), 3);
    state
        .prepare(
            &zset([(a, -1), (b, -1), (c, -1)]),
            &empty_e,
            std::iter::empty(),
            &empty_t,
            &mut meter,
        )
        .unwrap()
        .commit();
    assert!(state.vertices.is_empty() && state.rows.is_empty());
    assert_eq!(state.last_delta.as_ref().unwrap().len(), 3);
}

#[test]
fn clustering_keeps_exact_count_rounding_and_commits_counters_when_score_bits_stay_equal() {
    let sample = Counters {
        triangles: u64::MAX,
        degree: (1_u64 << 53) + 1,
        incidences: i128::from((1_u64 << 53) + 1),
    };
    let CanonicalScalar::Float(score) = Output::Clustering.value(sample).unwrap() else {
        panic!("clustering float");
    };
    // Widening the exact d*(d-1) before conversion gives precisely 2^-41.
    // Multiplying rounded f64 degrees instead gives the next larger float.
    assert_eq!(score.to_bits(), 0x3d60_0000_0000_0000);
    assert_ne!(
        score.to_bits(),
        (2.0 * sample.triangles as f64 / (sample.degree as f64 * (sample.degree as f64 - 1.0)))
            .to_bits(),
    );
    assert_eq!(
        Output::Triangles.value(sample),
        Err(StandingQueryFailure::Arithmetic)
    );
    for degree in [0, 1, 2, u64::MAX] {
        assert_eq!(
            Output::Clustering
                .value(Counters {
                    triangles: 0,
                    degree,
                    incidences: i128::from(degree)
                })
                .unwrap(),
            CanonicalScalar::Float(CanonicalF64::new(0.0)),
        );
    }
    assert_eq!(
        Output::Clustering.value(Counters {
            triangles: 2,
            degree: 2,
            incidences: 2
        }),
        Err(StandingQueryFailure::InvalidDelta),
    );

    // Numeric-state fixture, not a graph-source oracle: the first exact count
    // increment rounds to the same coefficient. The following increment must
    // use that accepted integer state and then emit its changed score bits.
    let counters = Counters {
        triangles: 1_u64 << 53,
        degree: 1_u64 << 32,
        incidences: 1_i128 << 32,
    };
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    let mut state = Counts::new(Output::Clustering, &mut meter).unwrap();
    for vertex in [VId(0), VId(1), VId(2)] {
        state.vertices.insert(vertex, counters);
        state
            .rows
            .accumulate(
                row(
                    vertex,
                    Output::Clustering.value(counters).unwrap(),
                    &mut meter,
                )
                .unwrap(),
                ZWeight::ONE,
                LIMBS,
                &mut |_| Ok::<_, StandingQueryFailure>(()),
            )
            .unwrap();
    }
    let triangle = zset([((VId(0), VId(1), VId(2)), 1)]);
    for increment in 1..=2 {
        state
            .prepare(
                &ZSet::new(),
                &ZSet::new(),
                std::iter::empty(),
                &triangle,
                &mut meter,
            )
            .unwrap()
            .commit();
        assert!(
            state
                .vertices
                .values()
                .all(|value| value.triangles == counters.triangles + increment)
        );
        assert_eq!(
            state.last_delta.as_ref().unwrap().len(),
            if increment == 1 { 0 } else { 6 }
        );
    }
}

#[test]
fn clustering_degree_only_tick_work_is_independent_of_unrelated_isolates() {
    let ((), report) = run_async_under_lab(0x7472_6304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = || {
            DatabaseKeys::new(
                [0x97; 32],
                DatabaseSecurityNamespaceId([0x98; 32]),
                [0x99; 32],
            )
        };
        let mut previous = None;
        for isolates in [0, 1000] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 0..4 + isolates {
                seed.create_vertex(VId(id), vec![], vec![]);
            }
            for (id, a, b) in [(1, 0, 1), (2, 1, 2), (3, 2, 0)] {
                seed.add_edge(EId(id), VId(a), VId(b), vec![]);
            }
            let start = db.write(&commit, seed).await.unwrap();
            let mut state = build_as(&db.snapshot, Output::Clustering);
            let mut change = WriteBatch::new(RelationId(1));
            change.add_edge(EId(4), VId(0), VId(3), vec![]);
            db.write(&commit, change).await.unwrap();
            let batch = db.delta_since(start).unwrap().next().unwrap();
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            state.maintain(&commit, batch, &mut meter).unwrap();
            assert_eq!(state.delta().unwrap().len(), 2);
            assert_eq!(state.triangles.total(), &ZWeight::ONE);
            assert_eq!(
                state.rows(),
                build_as(&db.snapshot, Output::Clustering).rows()
            );
            if let Some(before) = previous {
                assert_eq!(
                    meter.stats, before,
                    "an ordinary tick must not rebuild rows or scan unrelated retained vertices"
                );
            }
            previous = Some(meter.stats);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_selected_endpoints_cascades_and_unselected_edge_ids_never_publish() {
    let ((), report) = run_async_under_lab(0x7472_6302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x94; 32],
            DatabaseSecurityNamespaceId([0x95; 32]),
            [0x96; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for vertex in 0..4 {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        for (id, a, b) in [(10, 0, 1), (11, 1, 2), (12, 2, 0), (13, 3, 3)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(20), VId(0), VId(3), vec![]);
        db.write(&commit, other).await.unwrap();
        let baseline = Arc::clone(&db.snapshot);
        let before = build(&baseline);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(0));
        change.add_edge(EId(14), VId(2), VId(3), vec![]);
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();

        // These deliberately malformed decoded-input fixtures exercise the
        // local algebra laws. Chronicle/Strata own complete authentication and
        // cross-relation cascade/version validation before normal publication.
        for malformed in 0..4 {
            let mut entries = batch.coordinate_entries().to_vec();
            let mut changed = false;
            for entry in &mut entries {
                for row in &mut entry.rows {
                    match (malformed, row) {
                        (
                            0,
                            DeltaRow::DeleteVertex {
                                vid,
                                sorted_retired_incident_edges,
                                ..
                            },
                        ) if *vid == VId(0) => {
                            let size = sorted_retired_incident_edges.len();
                            sorted_retired_incident_edges.retain(|edge| *edge != EId(10));
                            changed = sorted_retired_incident_edges.len() < size;
                        }
                        (1, DeltaRow::CreateEdge { eid, dst, .. }) if *eid == EId(14) => {
                            *dst = VId(9);
                            changed = true;
                        }
                        (
                            2,
                            DeltaRow::DeleteVertex {
                                vid,
                                sorted_retired_incident_edges,
                                ..
                            },
                        ) if *vid == VId(0) => {
                            sorted_retired_incident_edges.push(EId(13));
                            sorted_retired_incident_edges.sort();
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            if malformed == 3 {
                let deletion = DeltaRow::DeleteEdge {
                    eid: EId(9999),
                    before_version: ObjectId([7; 32]),
                };
                if let Some(entry) = entries
                    .iter_mut()
                    .find(|entry| entry.relation == RelationId(2))
                {
                    entry.rows.push(deletion);
                } else {
                    let basis = &entries[0];
                    entries.push(CoordinateEntry {
                        graph: basis.graph,
                        branch: basis.branch,
                        relation: RelationId(2),
                        schema_epoch: basis.schema_epoch,
                        schema_transition: None,
                        rows: vec![deletion],
                    });
                }
                changed = true;
            }
            assert!(
                changed,
                "malformed fixture {malformed} must change its intended source row"
            );
            let malformed_batch = LogicalDeltaBatch::from_parts_for_test(
                entries,
                *batch.source_template_digest(),
                batch.commit_marker_identity(),
                batch.commit_seq(),
                batch.frontier(),
            );
            let mut state = build(&baseline);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.maintain(&commit, &malformed_batch, &mut meter),
                Err(StandingQueryFailure::InvalidDelta),
                "case {malformed}"
            );
            unchanged(&state, &before);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn selected_pair(left: Option<u128>, right: Option<u128>) -> GraphValueRow {
    let value = |id: Option<u128>| {
        id.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
            GraphValue::Vertex(VId(id))
        })
    };
    GraphValueRow::from_owned_values(vec![value(left), value(right)])
}

fn selected_definition(input: usize) -> Definition {
    Definition {
        input,
        endpoints: [0, 1],
        width: 2,
    }
}

fn build_selected(rows: &ZSet<GraphValueRow>, output: Output) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_selected(
        rows,
        selected_definition(0),
        output,
        CommitSeq::ORIGIN,
        &mut meter,
    )
    .unwrap()
}

fn apply_selected(state: &mut State, delta: &ZSet<GraphValueRow>) -> StandingQueryStats {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state.apply_selected(delta, &mut meter).unwrap();
    meter.stats
}

// Independent complete-source oracle: enumerate unordered pairs of each
// vertex's distinct neighbors. Neither project_rows nor the triangle or
// counter kernels supplies this expectation.
fn selected_oracle(rows: &ZSet<GraphValueRow>, output: Output) -> ZSet<GraphValueRow> {
    let mut graph = BTreeMap::<VId, BTreeSet<VId>>::new();
    for (row, weight) in rows.iter() {
        assert_eq!(row.len(), 2);
        assert!(weight > &ZWeight::ZERO);
        let endpoint = |value: &GraphValue| {
            if let GraphValue::Vertex(vertex) = value {
                Some(*vertex)
            } else {
                assert!(value.is_null());
                None
            }
        };
        let left = endpoint(&row.values()[0]);
        let right = endpoint(&row.values()[1]);
        for vertex in left.into_iter().chain(right) {
            graph.entry(vertex).or_default();
        }
        if let (Some(a), Some(b)) = (left, right)
            && a != b
        {
            graph.get_mut(&a).unwrap().insert(b);
            graph.get_mut(&b).unwrap().insert(a);
        }
    }
    zset(graph.iter().map(|(vertex, neighbors)| {
        let neighbors: Vec<_> = neighbors.iter().copied().collect();
        let mut triangles = 0_usize;
        for (index, a) in neighbors.iter().enumerate() {
            for b in &neighbors[index + 1..] {
                triangles += usize::from(graph[a].contains(b));
            }
        }
        let value = match output {
            Output::Triangles => CanonicalScalar::Int(i64::try_from(triangles).unwrap()),
            Output::Clustering => {
                let degree = neighbors.len();
                let score = if degree < 2 {
                    0.0
                } else {
                    (2 * triangles) as f64 / (degree * (degree - 1)) as f64
                };
                CanonicalScalar::Float(CanonicalF64::new(score))
            }
        };
        (
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(*vertex),
                GraphValue::Scalar(value),
            ]),
            1,
        )
    }))
}

#[test]
fn selected_triangles_and_clustering_match_all_four_vertex_graphs_and_each_edge_flip() {
    let pairs = [(0, 1), (2, 0), (0, 3), (1, 2), (3, 1), (2, 3)];
    for mask in 0..64_u8 {
        let initial = zset(
            (0..4)
                .map(|vertex| (selected_pair(Some(vertex), Some(vertex)), 1))
                .chain([(selected_pair(None, Some(u128::MAX)), 3)])
                .chain(pairs.iter().enumerate().filter_map(|(bit, &(a, b))| {
                    (mask & (1 << bit) != 0).then_some((selected_pair(Some(a), Some(b)), 2))
                })),
        );
        for (bit, &(a, b)) in pairs.iter().enumerate() {
            let delta = zset([(
                selected_pair(Some(a), Some(b)),
                if mask & (1 << bit) == 0 { 2 } else { -2 },
            )]);
            let mut after = initial
                .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            after
                .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            for output in [Output::Triangles, Output::Clustering] {
                let mut state = build_selected(&initial, output);
                assert!(state.delta().is_none());
                assert_eq!(state.rows(), &selected_oracle(&initial, output));
                let mut delivered = state
                    .rows()
                    .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                apply_selected(&mut state, &delta);
                assert_eq!(state.rows(), &selected_oracle(&after, output));
                delivered
                    .integrate(state.delta().unwrap(), LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&delivered, state.rows());
            }
        }
    }
}

#[test]
fn selected_parallel_opposite_and_null_support_retire_only_the_last_vertex_witness() {
    let initial = zset([
        (selected_pair(Some(0), Some(0)), 1),
        (selected_pair(Some(1), Some(1)), 1),
        (selected_pair(Some(0), Some(1)), 2),
        (selected_pair(Some(1), Some(0)), 1),
        (selected_pair(Some(1), Some(2)), 1),
        (selected_pair(Some(2), Some(0)), 1),
        (selected_pair(None, Some(u128::MAX)), 4),
        (selected_pair(None, None), 1),
    ]);
    let changes = [
        (selected_pair(Some(0), Some(1)), -1),
        (selected_pair(Some(0), Some(1)), -1),
        (selected_pair(Some(1), Some(0)), -1),
        (selected_pair(Some(1), Some(2)), -1),
        (selected_pair(Some(1), Some(1)), -1),
        (selected_pair(None, Some(u128::MAX)), -3),
        (selected_pair(None, Some(u128::MAX)), -1),
        (selected_pair(Some(2), Some(0)), -1),
        (selected_pair(Some(0), Some(0)), -1),
    ];
    for output in [Output::Triangles, Output::Clustering] {
        let mut state = build_selected(&initial, output);
        let mut source = initial
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        for (tick, change) in changes.iter().enumerate() {
            let delta = zset([change.clone()]);
            source
                .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            apply_selected(&mut state, &delta);
            assert_eq!(state.rows(), &selected_oracle(&source, output));
            if matches!(tick, 0 | 1 | 5) {
                assert!(state.delta().unwrap().is_empty());
            }
        }
        assert!(state.rows().is_empty());
        assert_eq!(source.len(), 1, "NULL/NULL never supplies a vertex");
        apply_selected(&mut state, &zset([(selected_pair(None, None), -1)]));
        assert!(state.delta().unwrap().is_empty());
    }
}

#[test]
fn selected_presence_topology_and_counters_roll_back_at_every_checkpoint_and_exact_quota() {
    let initial = zset([
        (selected_pair(Some(0), Some(0)), 1),
        (selected_pair(Some(1), Some(1)), 1),
        (selected_pair(Some(2), Some(2)), 1),
        (selected_pair(Some(3), Some(3)), 1),
        (selected_pair(Some(0), Some(1)), 2),
        (selected_pair(Some(1), Some(0)), 1),
        (selected_pair(Some(1), Some(2)), 1),
        (selected_pair(Some(2), Some(0)), 1),
        (selected_pair(None, Some(4)), 1),
    ]);
    let delta = zset([
        (selected_pair(Some(1), Some(1)), -1),
        (selected_pair(Some(0), Some(1)), -2),
        (selected_pair(Some(1), Some(0)), -1),
        (selected_pair(Some(1), Some(2)), -1),
        (selected_pair(Some(0), Some(3)), 1),
        (selected_pair(Some(3), Some(2)), 1),
        (selected_pair(None, Some(4)), -1),
    ]);
    let mut final_source = initial
        .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    final_source
        .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
        .unwrap();
    for output in [Output::Triangles, Output::Clustering] {
        let before = build_selected(&initial, output);
        let mut successful = build_selected(&initial, output);
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || {
                calls += 1;
                Ok(())
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            successful.apply_selected(&delta, &mut meter).unwrap();
            meter.stats
        };
        assert_eq!(successful.rows(), &selected_oracle(&final_source, output));
        assert_eq!(successful.rows().len(), 3);
        assert!(calls > 2 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut state = build_selected(&initial, output);
            let mut visited = 0;
            let mut checkpoint = || {
                visited += 1;
                if visited == stop {
                    Err(StandingQueryFailure::Interrupted)
                } else {
                    Ok(())
                }
            };
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&delta, &mut meter),
                Err(StandingQueryFailure::Interrupted)
            );
            assert_eq!(visited, stop);
            unchanged(&state, &before);
        }
        for (rows, work, scratch, error) in [
            (3, stats.work_units, stats.scratch_entries, None),
            (
                2,
                stats.work_units,
                stats.scratch_entries,
                Some(StandingQueryFailure::ResultBudget),
            ),
            (
                3,
                stats.work_units - 1,
                stats.scratch_entries,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                3,
                stats.work_units,
                stats.scratch_entries - 1,
                Some(StandingQueryFailure::ScratchBudget),
            ),
        ] {
            let mut state = build_selected(&initial, output);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(0, rows, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&delta, &mut meter),
                error.map_or(Ok(()), Err)
            );
            unchanged(
                &state,
                if error.is_some() {
                    &before
                } else {
                    &successful
                },
            );
        }
        for malformed in [
            selected_pair(Some(99), None),
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(0))]),
            GraphValueRow::from_owned_values(vec![
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Int(7)),
            ]),
        ] {
            let mut state = build_selected(&initial, output);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&zset([(malformed, -1)]), &mut meter),
                Err(StandingQueryFailure::InvalidDelta)
            );
            unchanged(&state, &before);
        }
    }
}

#[test]
fn selected_tick_work_does_not_visit_unrelated_retained_vertices() {
    for output in [Output::Triangles, Output::Clustering] {
        let mut previous = None;
        for isolates in [0, 1000] {
            let source = zset(
                (0..4 + isolates)
                    .map(|vertex| (selected_pair(Some(vertex), Some(vertex)), 1))
                    .chain([
                        (selected_pair(Some(0), Some(1)), 1),
                        (selected_pair(Some(1), Some(2)), 1),
                        (selected_pair(Some(2), Some(0)), 1),
                    ]),
            );
            let mut state = build_selected(&source, output);
            let stats = apply_selected(&mut state, &zset([(selected_pair(Some(0), Some(3)), 1)]));
            assert_eq!(state.triangles.total(), &ZWeight::ONE);
            assert_eq!(
                state.delta().unwrap().len(),
                if output == Output::Clustering { 2 } else { 0 }
            );
            if let Some(before) = previous {
                assert_eq!(stats, before);
            }
            previous = Some(stats);
        }
    }
}

#[test]
fn selected_admission_and_dependency_failures_preserve_the_accepted_generation() {
    let ((), report) = run_async_under_lab(0x7472_6307, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let keys = DatabaseKeys::new(
            [0x9a; 32],
            DatabaseSecurityNamespaceId([0x9b; 32]),
            [0x9c; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let symbols = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            _ => None,
        };
        let source = db
            .register_standing_native(
                &cx,
                "MATCH (a)-[:R]->(b) RETURN a,b",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let definition = selected_definition(source.index);
        let failed_source = db
            .register_standing_native(
                &cx,
                "MATCH (a)-[:R]->(b) RETURN a,b",
                &GqlParameters::new(),
                symbols,
                GqlQueryPolicy::new(100, 0, 100_000, 100_000),
            )
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for vertex in 0..3 {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        for (id, a, b) in [(1, 0, 1), (2, 1, 2), (3, 2, 0)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let at = db.write(&commit, seed).await.unwrap();
        for output in [Output::Triangles, Output::Clustering] {
            for (definition, allowance, failure) in [
                (
                    definition,
                    GqlQueryPolicy::new(2, 3, 100_000, 100_000),
                    StandingQueryFailure::SnapshotBudget,
                ),
                (
                    definition,
                    GqlQueryPolicy::new(3, 2, 100_000, 100_000),
                    StandingQueryFailure::ResultBudget,
                ),
                (
                    Definition {
                        width: 3,
                        ..definition
                    },
                    policy(),
                    StandingQueryFailure::DependencyUnavailable,
                ),
                (
                    selected_definition(usize::MAX),
                    policy(),
                    StandingQueryFailure::DependencyUnavailable,
                ),
                (
                    selected_definition(failed_source.index),
                    policy(),
                    StandingQueryFailure::DependencyUnavailable,
                ),
            ] {
                assert!(matches!(
                    db.prepare_standing_selected_triangle_counts(&cx, definition, output, allowance),
                    Err(StandingQueryError::Maintenance(reason)) if reason == failure
                ));
            }
            let accepted = db
                .prepare_standing_selected_triangle_counts(
                    &cx,
                    definition,
                    output,
                    GqlQueryPolicy::new(3, 3, 100_000, 100_000),
                )
                .unwrap();
            assert_eq!(accepted.selected, Some(definition));
            assert_eq!(accepted.frontier, at);
            assert!(accepted.delta().is_none());
            assert_eq!(accepted.rows().len(), 3);
            let source_rows = db.standing_rows(&cx, &source).unwrap();
            assert_eq!(
                accepted.rows(),
                &selected_oracle(source_rows.rows(), output)
            );
            let batch = db.delta_since(CommitSeq::ORIGIN).unwrap().next().unwrap();
            for bad_definition in [
                selected_definition(usize::MAX),
                selected_definition(failed_source.index),
                Definition {
                    width: 3,
                    ..definition
                },
            ] {
                let mut state = build_selected(&ZSet::new(), output);
                state.selected = Some(bad_definition);
                let mut before = build_selected(&ZSet::new(), output);
                before.selected = Some(bad_definition);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                assert_eq!(
                    state.maintain_with_sources(&commit, batch, &db.standing_queries, &mut meter,),
                    Err(StandingQueryFailure::DependencyUnavailable)
                );
                unchanged(&state, &before);
            }
        }
        db.rebuild_standing_query(&cx, &source, policy()).unwrap();
        let batch = db.delta_since(CommitSeq::ORIGIN).unwrap().next().unwrap();
        for output in [Output::Triangles, Output::Clustering] {
            let mut state = build_selected(&ZSet::new(), output);
            state.selected = Some(definition);
            let mut before = build_selected(&ZSet::new(), output);
            before.selected = Some(definition);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.maintain_with_sources(&commit, batch, &db.standing_queries, &mut meter),
                Err(StandingQueryFailure::DependencyUnavailable),
                "a rebaseline must never stand in for a missing derivative"
            );
            unchanged(&state, &before);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
