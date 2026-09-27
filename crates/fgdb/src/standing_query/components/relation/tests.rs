use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, ZWeight};
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn pair(a: Option<u128>, b: Option<u128>) -> GraphValueRow {
    let value = |id: Option<u128>| {
        id.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
            GraphValue::Vertex(VId(id))
        })
    };
    GraphValueRow::from_owned_values(vec![value(a), value(b)])
}

fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        rows.into_iter()
            .map(|(row, w)| (row, ZWeight::from_i128(w))),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}

fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}

fn build(rows: &ZSet<GraphValueRow>, strong: bool) -> State {
    let definition = Definition {
        input: 0,
        endpoints: [0, 1],
        width: 2,
    };
    let relation = if strong {
        ComponentRelation::StrongRows(definition)
    } else {
        ComponentRelation::WeakRows(definition)
    };
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_selected(rows, definition, relation, CommitSeq::ORIGIN, &mut meter).unwrap()
}

fn apply(state: &mut State, delta: &ZSet<GraphValueRow>) -> StandingQueryStats {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    state.apply_selected(delta, &mut meter).unwrap();
    meter.stats
}

fn unchanged(actual: &State, expected: &State) {
    assert_eq!(actual.input, expected.input);
    assert_eq!(actual.components, expected.components);
    assert_eq!(actual.rows, expected.rows);
    assert_eq!(actual.relational, expected.relational);
    assert_eq!(actual.frontier, expected.frontier);
    assert_eq!(actual.policy, expected.policy);
    assert_eq!(actual.stats, expected.stats);
    assert_eq!(actual.failure, expected.failure);
    assert_eq!(actual.relation, expected.relation);
}

// Independent breadth-first traversal from the complete selected source bag.
// Neither the projection helper nor either component kernel supplies the oracle.
fn oracle(rows: &ZSet<GraphValueRow>, strong: bool) -> BTreeMap<VId, VId> {
    let mut vertices = BTreeSet::new();
    let mut edges = BTreeMap::<VId, BTreeSet<VId>>::new();
    for (row, weight) in rows.iter() {
        assert!(weight > &ZWeight::ZERO);
        let mut endpoints = [None; 2];
        for (index, value) in row.values().iter().enumerate() {
            match value {
                GraphValue::Vertex(vertex) => {
                    endpoints[index] = Some(*vertex);
                    vertices.insert(*vertex);
                }
                value if value.is_null() => {}
                _ => panic!("invalid oracle fixture"),
            }
        }
        if let [Some(a), Some(b)] = endpoints {
            edges.entry(a).or_default().insert(b);
            if !strong {
                edges.entry(b).or_default().insert(a);
            }
        }
    }
    let mut reachable = BTreeMap::new();
    for &source in &vertices {
        let mut seen = BTreeSet::from([source]);
        let mut todo = std::collections::VecDeque::from([source]);
        while let Some(vertex) = todo.pop_front() {
            for &next in edges.get(&vertex).into_iter().flatten() {
                if seen.insert(next) {
                    todo.push_back(next);
                }
            }
        }
        reachable.insert(source, seen);
    }
    vertices
        .iter()
        .map(|&v| {
            let root = *vertices
                .iter()
                .find(|&&r| reachable[&v].contains(&r) && (!strong || reachable[&r].contains(&v)))
                .unwrap();
            (v, root)
        })
        .collect()
}

fn actual(rows: &ZSet<Pair>) -> BTreeMap<VId, VId> {
    rows.iter()
        .map(|(&(v, r), weight)| {
            assert_eq!(weight, &ZWeight::ONE);
            (v, r)
        })
        .collect()
}

#[test]
fn selected_graphs_and_null_isolates_match_independent_bfs_after_every_arc_flip() {
    let candidates = [
        pair(Some(0), Some(1)),
        pair(Some(1), Some(0)),
        pair(Some(1), Some(2)),
        pair(Some(2), Some(1)),
        pair(Some(2), Some(0)),
        pair(Some(0), Some(2)),
        pair(Some(0), None),
        pair(None, Some(1)),
        pair(Some(2), Some(2)),
    ];
    for mask in 0..512_u16 {
        let before = bag(candidates
            .iter()
            .enumerate()
            .filter_map(|(bit, row)| (mask & (1 << bit) != 0).then_some((row.clone(), 2))));
        for bit in 0..candidates.len() {
            let delta = bag([(
                candidates[bit].clone(),
                if mask & (1 << bit) == 0 { 2 } else { -2 },
            )]);
            let mut after = copy(&before);
            after
                .integrate(&delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            for strong in [false, true] {
                let mut state = build(&before, strong);
                assert_eq!(actual(&state.rows), oracle(&before, strong));
                let mut delivered = copy(state.value_rows());
                apply(&mut state, &delta);
                assert_eq!(actual(&state.rows), oracle(&after, strong));
                delivered
                    .integrate(
                        state.value_delta().unwrap(),
                        LIMBS,
                        &mut |_| Ok::<_, ()>(()),
                    )
                    .unwrap();
                assert_eq!(&delivered, state.value_rows());
            }
        }
    }
}

#[test]
fn promoted_counts_preserve_isolates_until_last_support_and_validate_both_cells() {
    let huge = ZWeight::from_i128(i128::MAX)
        .checked_add(&ZWeight::ONE, LIMBS)
        .unwrap();
    let rows = ZSet::from_updates(
        [
            (
                pair(Some(0), Some(u128::MAX)),
                huge.checked_clone(LIMBS).unwrap(),
            ),
            (pair(None, Some(u128::MAX)), ZWeight::ONE),
        ],
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap();
    for strong in [false, true] {
        let mut state = build(&rows, strong);
        let nearly_all = huge
            .checked_sub(&ZWeight::ONE, LIMBS)
            .unwrap()
            .checked_neg(LIMBS)
            .unwrap();
        let delta = ZSet::from_updates(
            [(pair(Some(0), Some(u128::MAX)), nearly_all)],
            LIMBS,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(apply(&mut state, &delta).affected_vertices, 0);
        assert!(state.value_delta().unwrap().is_empty());
        apply(&mut state, &bag([(pair(Some(0), Some(u128::MAX)), -1)]));
        assert_eq!(
            actual(&state.rows),
            [(VId(u128::MAX), VId(u128::MAX))].into()
        );
        apply(&mut state, &bag([(pair(None, Some(u128::MAX)), -1)]));
        assert!(state.rows.is_empty());
        apply(&mut state, &bag([(pair(None, None), 5)]));
        assert!(state.value_delta().unwrap().is_empty());
    }
    let baseline = bag([(pair(Some(1), None), 1)]);
    for row in [
        GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(CanonicalScalar::Null),
            GraphValue::Scalar(CanonicalScalar::Int(3)),
        ]),
        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]),
    ] {
        let mut state = build(&baseline, true);
        let expected = build(&baseline, true);
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: policy(),
            stats: StandingQueryStats::default(),
            checkpoint: &mut checkpoint,
        };
        assert_eq!(
            state.apply_selected(&bag([(row, 1)]), &mut meter),
            Err(StandingQueryFailure::InvalidDelta)
        );
        unchanged(&state, &expected);
    }
    let mut state = build(&baseline, true);
    let expected = build(&baseline, true);
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    assert_eq!(
        state.apply_selected(&bag([(pair(Some(9), None), -1)]), &mut meter),
        Err(StandingQueryFailure::InvalidDelta)
    );
    unchanged(&state, &expected);
}

#[test]
fn every_checkpoint_and_exact_budget_preserves_presence_kernel_and_both_sinks() {
    let initial = bag([(pair(Some(8), Some(9)), 1), (pair(Some(10), None), 1)]);
    let delta = bag([(pair(Some(8), Some(9)), -1), (pair(Some(1), Some(2)), 1)]);
    for strong in [false, true] {
        let mut successful = build(&initial, strong);
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
        assert!(calls > 2 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut state = build(&initial, strong);
            let expected = build(&initial, strong);
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
                    state.apply_selected(&delta, &mut meter),
                    Err(StandingQueryFailure::Interrupted)
                );
            }
            assert_eq!(visited, stop);
            unchanged(&state, &expected);
        }
        for (work, scratch, count, error) in [
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
            let mut state = build(&initial, strong);
            let expected = build(&initial, strong);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(0, count, work, scratch),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                state.apply_selected(&delta, &mut meter),
                error.map_or(Ok(()), Err)
            );
            if error.is_some() {
                unchanged(&state, &expected);
            } else {
                unchanged(&state, &successful);
            }
        }
        for stop in [1, calls / 2, calls] {
            let mut state = build(&initial, strong);
            let expected = build(&initial, strong);
            let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut visited = 0;
                let mut checkpoint = || {
                    visited += 1;
                    assert_ne!(visited, stop);
                    Ok(())
                };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                state.apply_selected(&delta, &mut meter)
            }));
            assert!(unwound.is_err());
            unchanged(&state, &expected);
        }
    }
}

const R: RelationId = RelationId(1);
const ENABLED: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "enabled") => Some(GraphSymbol::Property(ENABLED)),
        _ => None,
    }
}
fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    source: &StandingQueryHandle,
    child: &StandingQueryHandle,
    strong: bool,
) {
    let selected = db.standing_rows(cx, source).unwrap();
    let result = db.standing_components(cx, child).unwrap();
    assert_eq!(result.frontier(), db.frontier().unwrap());
    assert_eq!(actual(result.rows()), oracle(selected.rows(), strong));
}

#[test]
fn query_filters_drive_components_and_replay_without_base_edge_mutation() {
    let ((), report) = run_async_under_lab(0x6371_7201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for v in [0, 1, 2, u128::MAX] {
            seed.create_vertex(VId(v), vec![], vec![(ENABLED, CanonicalScalar::Int(1))]);
        }
        for (id, a, b) in [(10, 0, 1), (11, 0, 1), (12, 1, 2), (13, 2, 0)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let source = db
            .register_standing_native(
                &cx,
                "MATCH (a)-[:R]->(b) WHERE a.enabled > 0 RETURN a AS s, b AS t",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let strong = db
            .register_standing_strong_components_from_rows(&cx, &source, [0, 1], policy())
            .unwrap();
        let weak = db
            .register_standing_components_from_rows(&cx, &source, [0, 1], policy())
            .unwrap();
        let all = db
            .register_standing_strong_components(&cx, R, policy())
            .unwrap();
        assert_eq!(db.standing_component_count(&cx, &strong).unwrap(), 1);
        assert_eq!(
            db.standing_component(&cx, &strong, VId(u128::MAX)).unwrap(),
            None
        );
        assert_eq!(
            db.standing_component(&cx, &all, VId(u128::MAX)).unwrap(),
            Some(VId(u128::MAX))
        );
        let nested = db
            .register_standing_components_from_rows(&cx, &strong, [0, 1], policy())
            .unwrap();
        let mut sub = db.open_standing_subscription(&cx, &strong).unwrap();
        sub.enable_replay(&mut db, &cx, 16, 1000, 10000, policy())
            .unwrap();
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        let mut delivered = copy(baseline.rows());
        sub.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        for tick in 0..5 {
            let mut batch = WriteBatch::new(R);
            match tick {
                0 => {
                    batch.set_vertex_property(VId(2), ENABLED, Some(CanonicalScalar::Int(0)));
                }
                1 => {
                    batch.set_vertex_property(VId(2), ENABLED, Some(CanonicalScalar::Int(1)));
                }
                2 => {
                    batch.delete_edge(EId(10));
                }
                3 => {
                    batch.delete_edge(EId(11));
                }
                _ => {
                    batch.delete_vertex(VId(1));
                }
            }
            let before = copy(db.standing_components(&cx, &strong).unwrap().rows());
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, batch).unwrap();
            assert_eq!(
                db.standing_components(&cx, &strong).unwrap().rows(),
                &before
            );
            txn.commit(&mut db, &commit).await.unwrap();
            check(&db, &cx, &source, &strong, true);
            check(&db, &cx, &source, &weak, false);
            assert_eq!(
                db.standing_components(&cx, &nested).unwrap().rows(),
                db.standing_components(&cx, &strong).unwrap().rows()
            );
            if tick == 0 {
                assert!(db.edge(EId(13)).unwrap().is_some());
                assert_eq!(db.standing_component_count(&cx, &strong).unwrap(), 3);
                assert_eq!(db.standing_component_count(&cx, &all).unwrap(), 2);
            }
            if tick == 2 {
                assert!(
                    db.standing_component_delta(&cx, &strong)
                        .unwrap()
                        .unwrap()
                        .rows()
                        .is_empty()
                );
            }
            expected.push(db.standing_native_bag(&cx, &strong, policy()).unwrap());
        }
        for (at, rows) in expected {
            let frame = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!(frame.frontier(), at);
            delivered
                .integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(delivered, rows);
            sub.acknowledge(frame.receipt()).unwrap();
        }
        assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
        db.compact(&commit).await.unwrap();
        db.rebuild_standing_query(&cx, &strong, policy()).unwrap();
        check(&db, &cx, &source, &strong, true);
        assert!(db.standing_component_delta(&cx, &strong).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn schema_quota_and_parent_failure_are_explicit_and_rebuild_keeps_the_selected_mode() {
    let ((), report) = run_async_under_lab(0x6371_7202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let source = db
            .register_standing_native(
                &cx,
                "MATCH (a)-[:R]->(b) RETURN a,b",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let scalar = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n.enabled",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let count = db.standing_queries.len();
        assert!(matches!(
            db.register_standing_components_from_rows(&cx, &scalar, [0, 0], policy()),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.register_standing_components_from_rows(&cx, &source, [0, 2], policy()),
            Err(StandingQueryError::Unsupported)
        ));
        assert_eq!(db.standing_queries.len(), count);
        let tight = db
            .register_standing_strong_components_from_rows(
                &cx,
                &source,
                [0, 1],
                GqlQueryPolicy::new(100, 1, 100_000, 100_000),
            )
            .unwrap();
        let healthy = db
            .register_standing_strong_components_from_rows(&cx, &source, [1, 0], policy())
            .unwrap();
        let dependent = db
            .register_standing_components_from_rows(&cx, &tight, [0, 1], policy())
            .unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        let at = db.write(&commit, seed).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        // A parent rebaseline cannot fill a skipped derivative, even though
        // the parent's frontier is exactly the tick the consumer requested.
        let mut lagging = build(&ZSet::new(), true);
        let old_lagging = build(&ZSet::new(), true);
        db.rebuild_standing_query(&cx, &source, policy()).unwrap();
        {
            let batch = db.delta_since(CommitSeq::ORIGIN).unwrap().next().unwrap();
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: policy(),
                stats: StandingQueryStats::default(),
                checkpoint: &mut checkpoint,
            };
            assert_eq!(
                lagging.maintain_with_sources(&commit, batch, &db.standing_queries, &mut meter),
                Err(StandingQueryFailure::DependencyUnavailable)
            );
        }
        unchanged(&lagging, &old_lagging);
        assert!(matches!(
            db.standing_components(&cx, &tight),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert!(matches!(
            db.standing_components(&cx, &dependent),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        assert_eq!(db.standing_component_count(&cx, &healthy).unwrap(), 2);
        let before = copy(db.standing_components(&cx, &healthy).unwrap().rows());
        assert!(
            db.rebuild_standing_query(&cx, &healthy, GqlQueryPolicy::new(0, 2, 100_000, 100_000))
                .is_err()
        );
        assert_eq!(
            db.standing_components(&cx, &healthy).unwrap().rows(),
            &before
        );
        db.rebuild_standing_query(&cx, &tight, GqlQueryPolicy::new(1, 2, 100_000, 100_000))
            .unwrap();
        assert_eq!(db.standing_component_count(&cx, &tight).unwrap(), 2);
        assert!(db.standing_component_delta(&cx, &tight).unwrap().is_none());
        db.rebuild_standing_query(&cx, &dependent, policy())
            .unwrap();
        assert_eq!(db.standing_component_count(&cx, &dependent).unwrap(), 2);
        let vertices = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let loops = db
            .register_standing_components_from_rows(&cx, &vertices, [0, 0], policy())
            .unwrap();
        assert_eq!(db.standing_component_count(&cx, &loops).unwrap(), 2);
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.register_standing_components_from_rows(&cx, &source, [0, 1], policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(foreign.standing_queries.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
