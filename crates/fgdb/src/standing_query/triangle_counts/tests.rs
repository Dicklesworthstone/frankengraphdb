use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{CoordinateEntry, DeltaRow};
use fgdb_types::{DatabaseSecurityNamespaceId, EId, ObjectId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn build(snapshot: &crate::Snapshot) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_snapshot(snapshot, RelationId(1), &mut meter).unwrap()
}

fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.triangles, before.triangles);
    assert_eq!(actual.counts, before.counts);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.failure, before.failure);
    assert_eq!(actual.policy, before.policy);
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
        let before = build(&baseline);
        assert!(before.delta().is_none());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_edge(EId(10));
        change.delete_vertex(VId(1));
        change.add_edge(EId(15), VId(0), VId(3), vec![]);
        change.add_edge(EId(16), VId(3), VId(2), vec![]);
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
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
    let mut state = Counts::new(&mut meter).unwrap();
    for vertex in [VId(0), VId(1), VId(u128::MAX)] {
        state.vertices.insert(
            vertex,
            Counters {
                triangles,
                incidences,
            },
        );
        state
            .rows
            .accumulate(
                row(vertex, triangles, &mut meter).unwrap(),
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
                .prepare(&vertices, &edges, &triangles, &mut meter)
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
        .prepare(&empty_v, &empty_e, &empty_t, &mut meter)
        .unwrap()
        .commit();
    assert!(state.last_delta.as_ref().unwrap().is_empty());
    assert_eq!(state.rows.len(), 3);
    state
        .prepare(
            &zset([(a, -1), (b, -1), (c, -1)]),
            &empty_e,
            &empty_t,
            &mut meter,
        )
        .unwrap()
        .commit();
    assert!(state.vertices.is_empty() && state.rows.is_empty());
    assert_eq!(state.last_delta.as_ref().unwrap().len(), 3);
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
