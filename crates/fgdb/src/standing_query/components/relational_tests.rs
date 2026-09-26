//! Cross-layer consumers of the SAME maintained membership generation.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const WIDE: VId = VId(1_u128 << 100);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32])
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "score") => Some(GraphSymbol::Property(SCORE)),
        _ => None,
    }
}

fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}

fn native_pairs(rows: &ZSet<GraphValueRow>) -> BTreeMap<(VId, VId), i128> {
    rows.iter().map(|(row, weight)| match row.values() {
        [GraphValue::Vertex(v), GraphValue::Vertex(r)] => {
            ((*v, *r), weight.to_i128().unwrap())
        }
        _ => panic!("component rows are not two Vertex cells"),
    }).collect()
}

// Independent full-source BFS; never consult the maintained kernel, its
// derivative, its weak-region index, or a second invocation of that kernel.
fn oracle(db: &Database<MemVfs>, strong: bool) -> BTreeMap<VId, VId> {
    let vertices: Vec<_> = db.vertices().unwrap().into_iter().map(|row| row.vid).collect();
    let mut adjacency = BTreeMap::<VId, BTreeSet<VId>>::new();
    for record in db.edges().unwrap() {
        let edge = record.entry;
        if edge.relation == R {
            adjacency.entry(edge.src).or_default().insert(edge.dst);
            if !strong {
                adjacency.entry(edge.dst).or_default().insert(edge.src);
            }
        }
    }
    let mut reach = BTreeMap::new();
    for &vertex in &vertices {
        let mut visited = BTreeSet::new();
        let mut pending = vec![vertex];
        while let Some(next) = pending.pop() {
            if visited.insert(next) {
                pending.extend(adjacency.get(&next).into_iter().flatten().copied());
            }
        }
        reach.insert(vertex, visited);
    }
    vertices
        .iter()
        .map(|&vertex| {
            let representative = *vertices
                .iter()
                .filter(|&&other| {
                    reach[&vertex].contains(&other)
                        && (!strong || reach[&other].contains(&vertex))
                })
                .min()
                .unwrap();
            (vertex, representative)
        })
        .collect()
}

fn check_circuit(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    components: &StandingQueryHandle,
    joined: &StandingQueryHandle,
    grouped: &StandingQueryHandle,
    self_joined: &StandingQueryHandle,
) {
    let membership = oracle(db, true);
    let view = db.standing_component_rows(cx, components).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert!(view.ordered_rows().is_none());
    assert_eq!(native_pairs(view.rows()), membership.iter().map(|(&v, &r)| ((v, r), 1)).collect());
    assert_eq!(db.standing_components(cx, components).unwrap().rows().len(), membership.len());

    let scores: BTreeMap<_, _> = db.vertices().unwrap().into_iter().map(|row| {
        let Some((_, CanonicalScalar::Int(score))) = row.props.iter().find(|(key, _)| *key == SCORE)
        else { panic!("fixture score is missing or not an integer") };
        (row.vid, *score)
    }).collect();
    let actual: BTreeMap<_, _> = db.standing_join(cx, joined).unwrap().rows().iter()
        .map(|(row, weight)| match row.values() {
            [GraphValue::Vertex(v), GraphValue::Vertex(r), GraphValue::Vertex(copy),
                GraphValue::Scalar(CanonicalScalar::Int(score))] => {
                assert_eq!(v, copy);
                assert_eq!(weight, &ZWeight::ONE);
                (*v, (*r, *score))
            }
            _ => panic!("membership/property join changed its schema"),
        }).collect();
    let expected: BTreeMap<_, _> = membership
        .iter()
        .map(|(&v, &r)| (v, (r, scores[&v])))
        .collect();
    assert_eq!(actual, expected);

    let mut totals = BTreeMap::<VId, (i128, i128)>::new();
    for (&vertex, &representative) in &membership {
        let entry = totals.entry(representative).or_default();
        entry.0 += 1;
        entry.1 += i128::from(scores[&vertex]);
    }
    let actual: BTreeMap<_, _> = db.standing_reduction(cx, grouped).unwrap().rows().iter()
        .map(|(row, weight)| {
            let [GraphValue::Vertex(representative)] = row.keys() else {
                panic!("component group key lost its Vertex domain")
            };
            assert_eq!(weight, &ZWeight::ONE);
            (*representative, (row.count_rows().to_i128().unwrap(), row.sum().unwrap().to_i128().unwrap()))
        }).collect();
    assert_eq!(actual, totals);

    let actual: BTreeSet<_> = db.standing_join(cx, self_joined).unwrap().rows().iter()
        .map(|(row, weight)| match row.values() {
            [GraphValue::Vertex(a), GraphValue::Vertex(ar), GraphValue::Vertex(b), GraphValue::Vertex(br)] => {
                assert_eq!(ar, br);
                assert_eq!(weight, &ZWeight::ONE);
                (*a, *b)
            }
            _ => panic!("self join changed its Vertex schema"),
        }).collect();
    let mut expected = BTreeSet::new();
    for (&a, ar) in &membership {
        for (&b, br) in &membership {
            if ar == br { expected.insert((a, b)); }
        }
    }
    assert_eq!(actual, expected, "same-parent deltas must retain the join cross term");
    let cursor = db.standing_native_cursor(cx, components, policy()).unwrap();
    assert_eq!(cursor.collect::<Result<Vec<_>, _>>().unwrap().len(), membership.len());
}

#[test]
fn component_property_joins_groups_and_shared_parent_joins_follow_one_commit() {
    let ((), report) = run_async_under_lab(0x636f_7101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (v, score) in [(VId(1), 5), (VId(2), 7), (VId(3), 11), (WIDE, 13)] {
            seed.create_vertex(v, vec![], vec![(SCORE, CanonicalScalar::Int(score))]);
        }
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        seed.add_edge(EId(11), VId(2), VId(3), vec![]);
        db.write(&commit, seed).await.unwrap();
        let strong = db.register_standing_strong_components(&cx, R, policy()).unwrap();
        let weak = db.register_standing_components(&cx, R, policy()).unwrap();
        let props = db.register_standing_native(
            &cx, "MATCH (n) RETURN n AS v, n.score AS score",
            &GqlParameters::new(), symbols, policy(),
        ).unwrap();
        let joined = db.register_standing_join(&cx, &strong, &props, &[(0, 0)], policy()).unwrap();
        let grouped = db.register_standing_reduction(&cx, &joined, &[1], 3, policy()).unwrap();
        let self_joined = db.register_standing_join(&cx, &strong, &strong, &[(1, 1)], policy()).unwrap();
        let intersection = db.register_standing_set(&cx, &strong, &weak, SetOperation::IntersectDistinct, policy()).unwrap();
        assert_eq!(db.standing_native_columns(&cx, &strong).unwrap()
            .iter().map(String::as_str).collect::<Vec<_>>(), vec!["vertex", "component"]);
        assert!(db.standing_component_delta(&cx, &strong).unwrap().is_none());
        check_circuit(&db, &cx, &strong, &joined, &grouped, &self_joined);
        for tick in 0..6 {
            let before = copy(db.standing_component_rows(&cx, &strong).unwrap().rows());
            let mut batch = WriteBatch::new(R);
            match tick {
                0 => { batch.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(70))); }
                1 => {
                    batch.add_edge(EId(12), VId(3), VId(1), vec![]);
                    batch.set_vertex_property(VId(3), SCORE, Some(CanonicalScalar::Int(110)));
                }
                2 => { batch.add_edge(EId(13), VId(3), VId(1), vec![]); }
                3 => { batch.delete_edge(EId(12)); }
                4 => {
                    batch.delete_edge(EId(13));
                    batch.set_vertex_property(VId(2), SCORE, Some(CanonicalScalar::Int(-7)));
                }
                _ => { batch.delete_vertex(VId(2)); }
            }
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            txn.write(&mut db, batch).unwrap();
            assert_eq!(db.standing_component_rows(&cx, &strong).unwrap().rows(), &before);
            assert_eq!(db.frontier().unwrap(), basis);
            txn.commit(&mut db, &commit).await.unwrap();
            check_circuit(&db, &cx, &strong, &joined, &grouped, &self_joined);
            let delta = db.standing_component_delta(&cx, &strong).unwrap().unwrap();
            if matches!(tick, 0 | 2 | 3) { assert!(delta.rows().is_empty()); }
            let mut delivered = before;
            delivered.integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(&delivered, db.standing_component_rows(&cx, &strong).unwrap().rows());
            let weak_membership = oracle(&db, false);
            let strong_membership = oracle(&db, true);
            let expected: BTreeMap<_, _> = strong_membership
                .iter()
                .filter_map(|(&v, &r)| {
                    (weak_membership.get(&v) == Some(&r)).then_some(((v, r), 1_i128))
                })
                .collect();
            assert_eq!(native_pairs(db.standing_set(&cx, &intersection).unwrap().rows()), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_retains_empty_ticks_splits_and_cascades_across_consumer_lag() {
    let ((), report) = run_async_under_lab(0x636f_7102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for v in [VId(0), WIDE, VId(u128::MAX)] { seed.create_vertex(v, vec![], vec![]); }
        seed.add_edge(EId(1), VId(0), WIDE, vec![]);
        seed.add_edge(EId(2), WIDE, VId(0), vec![]);
        let basis = db.write(&commit, seed).await.unwrap();
        let source = db.register_standing_strong_components(&cx, R, policy()).unwrap();
        let mut subscription = db.open_standing_subscription(&cx, &source).unwrap();
        subscription.enable_replay(&mut db, &cx, 8, 1000, 10000, policy()).unwrap();
        let baseline = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!(baseline.frontier(), basis);
        let mut delivered = copy(baseline.rows());
        subscription.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        let mut cuts = Vec::new();
        for tick in 0..3 {
            let mut batch = WriteBatch::new(R);
            match tick {
                0 => { batch.set_vertex_property(WIDE, SCORE, Some(CanonicalScalar::Int(17))); }
                1 => { batch.delete_edge(EId(2)); }
                _ => { batch.delete_vertex(WIDE); }
            }
            cuts.push(db.write(&commit, batch).await.unwrap());
            expected.push(db.standing_native_bag(&cx, &source, policy()).unwrap().1);
        }
        for (tick, frontier) in cuts.into_iter().enumerate() {
            let frame = subscription.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!(frame.frontier(), frontier);
            if tick == 0 { assert!(frame.rows().is_empty()); }
            delivered.integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(delivered, expected[tick]);
            subscription.acknowledge(frame.receipt()).unwrap();
        }
        assert!(subscription.poll(&db, &cx, policy()).unwrap().is_none());
        db.compact(&commit).await.unwrap();
        db.rebuild_standing_query(&cx, &source, policy()).unwrap();
        assert!(db.standing_component_delta(&cx, &source).unwrap().is_none());
        assert_eq!(db.standing_native_bag(&cx, &source, policy()).unwrap().1, delivered);
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert!(matches!(db.standing_component_rows(&cx, &source), Err(StandingQueryError::ForeignHandle)));
        let current = db.register_standing_strong_components(&cx, R, policy()).unwrap();
        assert!(db.standing_component_delta(&cx, &current).unwrap().is_none());
        assert_eq!(db.standing_native_bag(&cx, &current, policy()).unwrap().1, delivered);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn build(snapshot: &crate::Snapshot, relation: ComponentRelation) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(),
        stats: StandingQueryStats::default(),
        checkpoint: &mut checkpoint,
    };
    State::from_snapshot(snapshot, relation, &mut meter).unwrap()
}

fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.components, before.components);
    assert_eq!(actual.rows, before.rows);
    assert_eq!(actual.relational, before.relational);
    assert_eq!(actual.relation, before.relation);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.policy, before.policy);
    assert_eq!(actual.failure, before.failure);
}

#[test]
fn every_composed_checkpoint_and_exact_budget_preserves_both_membership_outputs() {
    let ((), report) = run_async_under_lab(0x636f_7103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for v in 1..=3 { seed.create_vertex(VId(v), vec![], vec![]); }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();
        let snapshot = Arc::clone(&db.snapshot);
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(2), VId(2), VId(1), vec![]);
        change.add_edge(EId(3), VId(2), VId(3), vec![]);
        change.add_edge(EId(4), VId(3), VId(1), vec![]);
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(snapshot.frontier).unwrap().next().unwrap();
        for relation in [ComponentRelation::Weak(R), ComponentRelation::Strong(R)] {
            let before = build(&snapshot, relation);
            let expected = build(&db.snapshot, relation);
            let mut successful = build(&snapshot, relation);
            let mut calls = 0;
            let stats = {
                let mut checkpoint = || { calls += 1; Ok(()) };
                let mut meter = Meter {
                    policy: policy(),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                successful.maintain(&commit, batch, &mut meter).unwrap();
                meter.stats
            };
            assert_eq!(successful.rows, expected.rows);
            assert_eq!(successful.value_rows(), expected.value_rows());
            assert!(successful.value_delta().is_some());
            assert!(calls > 1 && stats.work_units > 0 && stats.scratch_entries > 0);
            for stop in 1..=calls {
                let mut state = build(&snapshot, relation);
                let mut seen = 0;
                {
                    let mut checkpoint = || {
                        seen += 1;
                        if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                    };
                    let mut meter = Meter {
                        policy: policy(),
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    assert_eq!(state.maintain(&commit, batch, &mut meter), Err(StandingQueryFailure::Interrupted));
                }
                assert_eq!(seen, stop);
                unchanged(&state, &before);
            }
            for (work, scratch, count, failure) in [
                (stats.work_units, stats.scratch_entries, 3, None),
                (stats.work_units - 1, stats.scratch_entries, 3, Some(StandingQueryFailure::WorkBudget)),
                (stats.work_units, stats.scratch_entries - 1, 3, Some(StandingQueryFailure::ScratchBudget)),
                (stats.work_units, stats.scratch_entries, 2, Some(StandingQueryFailure::ResultBudget)),
            ] {
                let mut state = build(&snapshot, relation);
                let mut checkpoint = || Ok(());
                let mut meter = Meter {
                    policy: GqlQueryPolicy::new(100_000, count, work, scratch),
                    stats: StandingQueryStats::default(),
                    checkpoint: &mut checkpoint,
                };
                let result = state.maintain(&commit, batch, &mut meter);
                assert_eq!(result, failure.map_or(Ok(()), Err));
                if failure.is_some() { unchanged(&state, &before); }
                else {
                    assert_eq!(state.rows, expected.rows);
                    assert_eq!(state.value_rows(), expected.value_rows());
                }
            }
            for stop in [1, calls / 2, calls] {
                let mut state = build(&snapshot, relation);
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let mut calls = 0;
                    let mut checkpoint = || { calls += 1; assert_ne!(calls, stop); Ok(()) };
                    let mut meter = Meter {
                        policy: policy(),
                        stats: StandingQueryStats::default(),
                        checkpoint: &mut checkpoint,
                    };
                    state.maintain(&commit, batch, &mut meter)
                }));
                assert!(panicked.is_err());
                unchanged(&state, &before);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn component_failure_and_new_baselines_do_not_become_empty_dependency_deltas() {
    let ((), report) = run_async_under_lab(0x636f_7104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let limited_policy = GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000);
        let limited = db.register_standing_strong_components(&cx, R, limited_policy).unwrap();
        let healthy = db.register_standing_strong_components(&cx, R, policy()).unwrap();
        let child = db.register_standing_set(&cx, &limited, &limited, SetOperation::UnionDistinct, policy()).unwrap();
        let tight_child = db.register_standing_set(&cx, &healthy, &healthy,
            SetOperation::UnionDistinct, limited_policy).unwrap();
        let mut seed = WriteBatch::new(R);
        for v in 1..=3 { seed.create_vertex(VId(v), vec![], vec![]); }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed.add_edge(EId(2), VId(2), VId(1), vec![]);
        let at = db.write(&commit, seed).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(db.standing_component_rows(&cx, &limited),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::ResultBudget, .. })));
        assert!(matches!(db.standing_component_delta(&cx, &limited),
            Err(StandingQueryError::Unavailable { .. })));
        assert!(matches!(db.standing_set(&cx, &child),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::DependencyUnavailable, .. })));
        assert!(matches!(db.standing_set(&cx, &tight_child),
            Err(StandingQueryError::Unavailable { reason: StandingQueryFailure::ResultBudget, .. })));
        assert_eq!(db.standing_component_rows(&cx, &healthy).unwrap().rows().len(), 3);
        assert!(db.rebuild_standing_query(&cx, &child, policy()).is_err());
        db.rebuild_standing_query(&cx, &limited, policy()).unwrap();
        assert!(db.standing_component_delta(&cx, &limited).unwrap().is_none());
        assert!(matches!(
            db.standing_native_delta(&cx, &limited, CommitSeq::ORIGIN, policy()),
            Err(StandingQueryError::DeltaUnavailable { .. })
        ));
        assert!(matches!(db.standing_set(&cx, &child), Err(StandingQueryError::Unavailable { .. })));
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert!(db.standing_set_delta(&cx, &child).unwrap().is_none());
        assert_eq!(db.standing_set(&cx, &child).unwrap().rows(),
            db.standing_component_rows(&cx, &limited).unwrap().rows());
        let before = copy(db.standing_component_rows(&cx, &limited).unwrap().rows());
        assert!(db.rebuild_standing_query(&cx, &limited, limited_policy).is_err());
        assert_eq!(db.standing_component_rows(&cx, &limited).unwrap().rows(), &before);
        // A succeeding empty tick is still a new, complete dependency update.
        let mut no_topology = WriteBatch::new(R);
        no_topology.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(2)));
        db.write(&commit, no_topology).await.unwrap();
        assert!(db.standing_component_delta(&cx, &limited).unwrap().unwrap().rows().is_empty());
        assert!(db.standing_set_delta(&cx, &child).unwrap().unwrap().rows().is_empty());
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(foreign.register_standing_set(&cx, &healthy, &healthy,
            SetOperation::UnionDistinct, policy()), Err(StandingQueryError::ForeignHandle)));
        assert!(matches!(db.standing_component_rows(&cx, &child), Err(StandingQueryError::Unsupported)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
