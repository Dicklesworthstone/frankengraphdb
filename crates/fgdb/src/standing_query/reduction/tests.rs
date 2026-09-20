use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::{
    GqlParameters, GraphSetColumnType, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "v") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn build(input: &ZSet<GraphValueRow>, at: CommitSeq) -> State {
    let spec = RowAggregateSpec::new(&[GraphSetColumnType::Scalar; 2], &[0], 1).unwrap();
    let mut state = State {
        input: 0,
        group_names: vec!["k".into()],
        operator: IncrementalRowAggregate::new(spec),
        last_delta: None,
        policy: policy(),
        frontier: at,
        stats: StandingQueryStats::default(),
        failure: None,
    };
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state.apply(input, &mut meter).unwrap();
    state.last_delta = None;
    state.stats = meter.stats;
    state
}
fn unchanged(left: &State, right: &State) {
    assert_eq!(left.operator, right.operator);
    assert_eq!(left.last_delta, right.last_delta);
    assert_eq!(left.input, right.input);
    assert_eq!(left.group_names, right.group_names);
    assert_eq!(left.policy, right.policy);
    assert_eq!(left.frontier, right.frontier);
    assert_eq!(left.failure, right.failure);
    assert_eq!(left.stats, right.stats);
}

#[test]
fn registry_refusal_at_every_checkpoint_preserves_all_state_and_rejects_a_new_parent_baseline() {
    let ((), report) = run_async_under_lab(0x7261_0501, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let keys = DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        );
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(
            VId(1),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(1)),
                (PropertyKeyId(2), CanonicalScalar::Int(10)),
            ],
        );
        let basis = db.write(&commit, seed).await.unwrap();
        let query = PreparedGraphText::prepare("MATCH(n:L) RETURN n.k AS k,n.v AS v", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let source = db.register_standing_rows(&cx, query, policy()).unwrap();
        assert_eq!(source.index, 0);
        let copy = |rows: &ZSet<GraphValueRow>| {
            rows.checked_clone(LIMBS, &mut |_| Ok::<_, StandingQueryFailure>(()))
                .unwrap()
        };
        let old = copy(db.standing_rows(&cx, &source).unwrap().rows());
        let before = build(&old, basis);
        let mut next = WriteBatch::new(RelationId(1));
        next.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(-3)));
        next.create_vertex(
            VId(2),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(2)),
                (PropertyKeyId(2), CanonicalScalar::Int(4)),
            ],
        );
        let at = db.write(&commit, next).await.unwrap();
        let new = copy(db.standing_rows(&cx, &source).unwrap().rows());
        let expected = build(&new, at);
        let batch = db.delta_since(basis).unwrap().next().unwrap();
        let mut complete = build(&old, basis);
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
            complete
                .maintain(batch, &db.standing_queries, &mut meter)
                .unwrap();
            meter.stats
        };
        assert_eq!(complete.operator, expected.operator);
        assert!(calls > 1 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut state = build(&old, basis);
            let mut visited = 0;
            {
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
                    state.maintain(batch, &db.standing_queries, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                );
            }
            assert_eq!(visited, stop);
            unchanged(&state, &before);
        }
        for (work, scratch, groups, error) in [
            (stats.work_units, stats.scratch_entries, 2, None),
            (
                stats.work_units - 1,
                stats.scratch_entries,
                2,
                Some(StandingQueryFailure::WorkBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries - 1,
                2,
                Some(StandingQueryFailure::ScratchBudget),
            ),
            (
                stats.work_units,
                stats.scratch_entries,
                1,
                Some(StandingQueryFailure::ResultBudget),
            ),
        ] {
            let mut state = build(&old, basis);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(1000, groups, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            let result = state.maintain(batch, &db.standing_queries, &mut meter);
            if let Some(error) = error {
                assert_eq!(result, Err(error));
                unchanged(&state, &before);
            } else {
                result.unwrap();
                assert_eq!(state.operator, expected.operator);
            }
        }
        assert!(matches!(
            db.prepare_standing_reduction(
                &cx,
                0,
                &[0],
                1,
                GqlQueryPolicy::new(1, 1000, 1_000_000, 1_000_000),
                1
            ),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
        assert!(
            db.prepare_standing_reduction(
                &cx,
                0,
                &[0],
                1,
                GqlQueryPolicy::new(2, 1000, 1_000_000, 1_000_000),
                1
            )
            .is_ok()
        );
        db.rebuild_standing_query(&cx, &source, policy()).unwrap();
        let batch = db.delta_since(basis).unwrap().next().unwrap();
        let mut state = build(&old, basis);
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        assert_eq!(
            state.maintain(batch, &db.standing_queries, &mut meter),
            Err(StandingQueryFailure::DependencyUnavailable)
        );
        unchanged(&state, &before);
        assert_eq!(db.frontier().unwrap(), at);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
