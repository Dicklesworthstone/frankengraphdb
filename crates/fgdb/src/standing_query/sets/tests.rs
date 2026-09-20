use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn definition(label: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(
        &format!("MATCH (n:{label}) RETURN n.p AS p"),
        |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
// Reconstruct only this test's accepted base through the real algebra/sink,
// not a production Clone or a replacement graph source.
fn copy_accepted(source: &State) -> State {
    let mut copy = State {
        inputs: source.inputs,
        columns: source.columns.clone(),
        types: source.types.clone(),
        input: IncrementalSet::new(source.operation()),
        rows: ZSet::new(),
        total: ZWeight::ZERO,
        last_delta: None,
        policy: source.policy,
        frontier: source.frontier,
        stats: source.stats,
        failure: source.failure,
    };
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    copy.prepare(
        source.input.left_counts(),
        source.input.right_counts(),
        &mut meter,
    )
    .unwrap()
    .commit();
    copy.last_delta = source
        .last_delta
        .as_ref()
        .map(|d| d.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap());
    copy
}
fn unchanged(state: &State, before: &State) {
    assert_eq!(state.input, before.input);
    assert_eq!(state.rows, before.rows);
    assert_eq!(state.total, before.total);
    assert_eq!(state.last_delta, before.last_delta);
    assert_eq!(state.frontier, before.frontier);
    assert_eq!(state.policy, before.policy);
    assert_eq!(state.stats, before.stats);
    assert_eq!(state.failure, before.failure);
    assert_eq!(state.inputs, before.inputs);
}

#[test]
fn every_composition_refusal_preserves_both_inputs_result_total_and_last_delta() {
    let ((), report) = run_async_under_lab(0x7365_7410, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, label, value) in [(1, 1, 1), (2, 1, 2), (3, 2, 2)] {
            seed.create_vertex(
                VId(id),
                vec![LabelId(label)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            );
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let left = db
            .register_standing_rows(&cx, definition("L"), policy())
            .unwrap();
        let right = db
            .register_standing_rows(&cx, definition("R"), policy())
            .unwrap();
        let deps = [left.index, right.index];
        let before = db
            .prepare_standing_set(&cx, deps, SetOperation::UnionAll, policy(), 2)
            .unwrap();
        assert!(before.last_delta.is_none());
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        let at = db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(basis).unwrap().next().unwrap().clone();
        let expected = db
            .prepare_standing_set(&cx, deps, SetOperation::UnionAll, policy(), 2)
            .unwrap();
        let mut success = copy_accepted(&before);
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
            success
                .maintain(&batch, &db.standing_queries, &mut meter)
                .unwrap();
            meter.stats
        };
        assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
        assert_eq!(success.input, expected.input);
        assert_eq!(success.rows, expected.rows);
        assert_eq!(success.total, expected.total);
        assert_eq!(success.total.to_i128(), Some(3));
        for stop in 1..=calls {
            let mut state = copy_accepted(&before);
            let mut seen = 0;
            {
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
                    state.maintain(&batch, &db.standing_queries, &mut meter),
                    Err(StandingQueryFailure::Interrupted),
                    "checkpoint {stop}"
                );
            }
            assert_eq!(seen, stop);
            unchanged(&state, &before);
        }
        for (work, scratch, rows, error) in [
            (stats.work_units, stats.scratch_entries, 3, None),
            (
                stats.work_units - 1,
                stats.scratch_entries,
                3,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries - 1,
                3,
                Some(StandingQueryFailure::ScratchBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries,
                2,
                Some(StandingQueryFailure::ResultBudget),
            ),
        ] {
            let mut state = copy_accepted(&before);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(0, rows, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let result = state.maintain(&batch, &db.standing_queries, &mut meter);
            if let Some(error) = error {
                assert_eq!(result, Err(error));
                unchanged(&state, &before);
            } else {
                result.unwrap();
                assert_eq!(state.rows, expected.rows);
                assert_eq!(state.input, expected.input);
            }
        }
        let mut state = copy_accepted(&before);
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        let l = delta(&db.standing_queries[left.index]).unwrap();
        let r = delta(&db.standing_queries[right.index]).unwrap();
        drop(state.prepare(l, r, &mut meter).unwrap());
        unchanged(&state, &before);
        state
            .maintain(&batch, &db.standing_queries, &mut meter)
            .unwrap();
        assert_eq!(state.rows, expected.rows);
        // A baseline, even at the requested sequence, is not a successor delta.
        db.rebuild_standing_query(&cx, &left, policy()).unwrap();
        let mut stale = copy_accepted(&before);
        assert_eq!(
            stale.maintain(&batch, &db.standing_queries, &mut meter),
            Err(StandingQueryFailure::DependencyUnavailable)
        );
        unchanged(&stale, &before);
        assert_eq!(db.frontier().unwrap(), at);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn snapshot_support_limits_are_checked_before_baseline_installation() {
    let ((), report) = run_async_under_lab(0x7365_7411, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0xa1; 32],
            DatabaseSecurityNamespaceId([0xa2; 32]),
            [0xa3; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=8 {
            seed.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(4))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        let parent = db
            .register_standing_rows(&cx, definition("L"), policy())
            .unwrap();
        let before = db.standing_queries.len();
        assert!(matches!(
            db.register_standing_set(
                &cx,
                &parent,
                &parent,
                SetOperation::UnionAll,
                GqlQueryPolicy::new(1, 16, 100_000, 100_000)
            ),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
        assert_eq!(db.standing_queries.len(), before);
        let h = db
            .register_standing_set(
                &cx,
                &parent,
                &parent,
                SetOperation::UnionAll,
                GqlQueryPolicy::new(2, 16, 100_000, 100_000),
            )
            .unwrap();
        assert_eq!(db.standing_set(&cx, &h).unwrap().rows().len(), 1);
        assert_eq!(db.standing_set_total(&cx, &h).unwrap().to_i128(), Some(16));
        assert!(matches!(
            db.rebuild_standing_query(&cx, &h, GqlQueryPolicy::new(2, 15, 100_000, 100_000)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert_eq!(db.standing_set_total(&cx, &h).unwrap().to_i128(), Some(16));
        assert!(db.standing_set_delta(&cx, &h).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
