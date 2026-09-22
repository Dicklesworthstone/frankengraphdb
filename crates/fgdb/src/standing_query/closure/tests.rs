use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn empty(width: usize, endpoints: [usize; 2]) -> State {
    State {
        input: 0, endpoints, width,
        columns: vec!["source".into(), "destination".into()],
        operator: IncrementalReachability::new(), rows: ZSet::new(), last_delta: None,
        policy: policy(), frontier: CommitSeq::ORIGIN,
        stats: StandingQueryStats::default(), failure: None,
    }
}
fn pair(a: u128, b: u128) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(a)), GraphValue::Vertex(VId(b))])
}
fn bag(rows: impl IntoIterator<Item = (GraphValueRow, i128)>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(rows.into_iter().map(|(row, count)| (row, ZWeight::from_i128(count))),
        LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn copy(rows: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn apply(state: &mut State, delta: &ZSet<GraphValueRow>) -> StandingQueryStats {
    let mut checkpoint = || Ok(());
    let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
    state.prepare(delta, &mut meter).unwrap().commit();
    meter.stats
}
fn unchanged(a: &State, b: &State) {
    assert_eq!(a.operator, b.operator);
    assert_eq!(a.rows, b.rows);
    assert_eq!(a.last_delta, b.last_delta);
    assert_eq!(a.frontier, b.frontier);
    assert_eq!(a.failure, b.failure);
    assert_eq!(a.stats, b.stats);
    assert_eq!(a.policy, b.policy);
    assert_eq!(a.input, b.input);
    assert_eq!(a.width, b.width);
    assert_eq!(a.endpoints, b.endpoints);
    assert_eq!(a.columns, b.columns);
}

// Independent traversal of the fully integrated source rows, not operator
// internals, differential reachability or its retained arrangement.
fn oracle(rows: &ZSet<GraphValueRow>, endpoints: [usize; 2]) -> ZSet<GraphValueRow> {
    let mut edges: BTreeMap<VId, BTreeSet<VId>> = BTreeMap::new();
    for (row, count) in rows.iter() {
        assert!(count > &ZWeight::ZERO);
        let (GraphValue::Vertex(a), GraphValue::Vertex(b)) =
            (&row.values()[endpoints[0]], &row.values()[endpoints[1]]) else { continue; };
        edges.entry(*a).or_default().insert(*b);
    }
    let mut output = Vec::new();
    for source in edges.keys() {
        let mut seen = BTreeSet::new();
        let mut pending: Vec<VId> = edges[source].iter().copied().collect();
        while let Some(destination) = pending.pop() {
            if !seen.insert(destination) { continue; }
            if let Some(next) = edges.get(&destination) { pending.extend(next); }
        }
        output.extend(seen.into_iter().map(|destination| (pair(source.0, destination.0), 1)));
    }
    bag(output)
}
fn graph(mask: u8) -> ZSet<GraphValueRow> {
    let mut rows = Vec::new();
    let mut bit = 0;
    for source in 0..3_u128 {
        for destination in 0..3_u128 {
            if source == destination { continue; }
            if mask & (1 << bit) != 0 {
                // Nonleading endpoint slots and different irrelevant payloads
                // deliberately collide at the projected edge identity.
                for payload in 1..=2_i64 {
                    rows.push((GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(VId(u128::MAX - destination)),
                        GraphValue::Scalar(CanonicalScalar::Int(payload)),
                        GraphValue::Vertex(VId(u128::MAX - source)),
                    ]), i128::from(payload)));
                }
            }
            bit += 1;
        }
    }
    bag(rows)
}

#[test]
fn all_small_graph_transitions_match_complete_source_oracle() {
    for old in 0..64_u8 {
        for new in 0..64_u8 {
            let initial = graph(old);
            let next = graph(new);
            let mut state = empty(3, [2, 0]);
            apply(&mut state, &initial);
            assert_eq!(state.rows, oracle(&initial, [2, 0]));
            let mut changes = Vec::new();
            for (row, weight) in initial.iter() {
                changes.push((row.clone(), weight.checked_neg(LIMBS).unwrap()));
            }
            for (row, weight) in next.iter() {
                changes.push((row.clone(), weight.checked_clone(LIMBS).unwrap()));
            }
            let delta = ZSet::from_updates(changes, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            let mut delivered = copy(&state.rows);
            apply(&mut state, &delta);
            delivered.integrate(state.delta().unwrap(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(delivered, state.rows);
            assert_eq!(state.rows, oracle(&next, [2, 0]));
            assert!(state.rows.iter().all(|(_, weight)| weight == &ZWeight::ONE));
        }
    }
}

#[test]
fn every_prepare_checkpoint_unwind_and_exact_quota_boundary_is_atomic() {
    let initial = bag([(pair(8, 9), 1)]);
    let changes = bag([(pair(8, 9), -1), (pair(1, 2), 1)]);
    let mut before = empty(2, [0, 1]);
    apply(&mut before, &initial);
    let mut state = empty(2, [0, 1]);
    apply(&mut state, &initial);
    let mut calls = 0;
    let stats;
    {
        let mut checkpoint = || { calls += 1; Ok(()) };
        let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        drop(state.prepare(&changes, &mut meter).unwrap());
        stats = meter.stats;
    }
    unchanged(&before, &state);
    for stop in 1..=calls {
        let mut seen = 0;
        {
            let mut checkpoint = || {
                seen += 1;
                if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
            };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            assert_eq!(state.prepare(&changes, &mut meter).map(drop), Err(StandingQueryFailure::Interrupted));
        }
        assert_eq!(seen, stop);
        unchanged(&before, &state);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut seen = 0;
            let mut checkpoint = || { seen += 1; assert_ne!(seen, stop); Ok(()) };
            let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
            state.prepare(&changes, &mut meter).map(drop)
        }));
        assert!(result.is_err());
        unchanged(&before, &state);
    }
    for (limit, work, scratch, error) in [
        (0, stats.work_units, stats.scratch_entries, Some(StandingQueryFailure::ResultBudget)),
        (1, stats.work_units - 1, stats.scratch_entries, Some(StandingQueryFailure::WorkBudget)),
        (1, stats.work_units, stats.scratch_entries - 1, Some(StandingQueryFailure::ScratchBudget)),
        (1, stats.work_units, stats.scratch_entries, None),
    ] {
        let mut checkpoint = || Ok(());
        let mut meter = Meter {
            policy: GqlQueryPolicy::new(0, limit, work, scratch),
            stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
        };
        assert_eq!(state.prepare(&changes, &mut meter).map(drop).err(), error);
        unchanged(&before, &state);
    }
    // The lower-sorted inserted pair must not transiently exceed quota one.
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: GqlQueryPolicy::new(0, 1, stats.work_units, stats.scratch_entries),
        stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
    };
    state.prepare(&changes, &mut meter).unwrap().commit();
    assert_eq!(state.rows, bag([(pair(1, 2), 1)]));
}

#[test]
fn promoted_parallel_support_null_endpoints_and_malformed_rows() {
    let mut state = empty(2, [0, 1]);
    let huge = ZWeight::from_i128(i128::MAX).checked_add(&ZWeight::ONE, LIMBS).unwrap();
    let input = ZSet::from_updates([(pair(1, 2), huge.checked_clone(LIMBS).unwrap())],
        LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
    apply(&mut state, &input);
    assert_eq!(state.rows, bag([(pair(1, 2), 1)]));
    let nearly_all = huge.checked_sub(&ZWeight::ONE, LIMBS).unwrap().checked_neg(LIMBS).unwrap();
    let delta = ZSet::from_updates([(pair(1, 2), nearly_all)], LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
    apply(&mut state, &delta);
    assert!(state.delta().unwrap().is_empty());
    apply(&mut state, &bag([(pair(1, 2), -1)]));
    assert!(state.rows.is_empty());
    assert_eq!(state.delta().unwrap(), &bag([(pair(1, 2), -1)]));

    let null = GraphValue::Scalar(CanonicalScalar::Null);
    let nullable = GraphValueRow::from_owned_values(vec![null.clone(), GraphValue::Vertex(VId(2))]);
    apply(&mut state, &bag([(nullable, 3)]));
    assert!(state.rows.is_empty());
    for invalid in [
        GraphValueRow::from_owned_values(vec![null, GraphValue::Scalar(CanonicalScalar::Int(2))]),
        GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]),
    ] {
        let before = copy(&state.rows);
        let last = copy(state.delta().unwrap());
        let mut checkpoint = || Ok(());
        let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
        assert_eq!(state.prepare(&bag([(invalid, 1)]), &mut meter).map(drop), Err(StandingQueryFailure::InvalidDelta));
        assert_eq!(state.rows, before);
        assert_eq!(state.delta().unwrap(), &last);
    }
    let mut checkpoint = || Ok(());
    let mut meter = Meter { policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint };
    assert_eq!(state.prepare(&bag([(pair(3, 4), -1)]), &mut meter).map(drop), Err(StandingQueryFailure::InvalidDelta));
    apply(&mut state, &bag([(pair(u128::MAX, u128::MAX), 1)]));
    assert_eq!(state.rows, bag([(pair(u128::MAX, u128::MAX), 1)]));
}

#[test]
fn multiplicity_only_ticks_do_not_scan_unrelated_closure_rows() {
    let mut costs = Vec::new();
    for size in [8_u128, 128] {
        let mut state = empty(2, [0, 1]);
        apply(&mut state, &bag((0..size).map(|n| (pair(2 * n, 2 * n + 1), 1))));
        let stats = apply(&mut state, &bag([(pair(0, 1), 1)]));
        assert!(state.delta().unwrap().is_empty());
        assert_eq!(state.rows.len(), usize::try_from(size).unwrap());
        costs.push((stats.work_units, stats.scratch_entries));
    }
    assert_eq!(costs[0], costs[1]);
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "enabled") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], DatabaseSecurityNamespaceId([2; 32]), [3; 32])
}

#[test]
fn filtered_parent_closure_nested_circuit_and_acknowledged_replay_share_commits() {
    let ((), report) = run_async_under_lab(0x6c05_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=4 {
            seed.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]);
        }
        for (id, a, b) in [(1, 1, 2), (2, 1, 2), (3, 2, 3), (4, 3, 1), (5, 3, 4)] {
            seed.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        let basis = db.write(&commit, seed).await.unwrap();
        let parent = db.register_standing_native(&cx,
            "MATCH (a)-[:R]->(b) WHERE a.enabled > 0 RETURN a AS s, b AS t",
            &GqlParameters::new(), symbols, policy()).unwrap();
        let closure = db.register_standing_closure(&cx, &parent, [0, 1], policy()).unwrap();
        let nested = db.register_standing_closure(&cx, &closure, [0, 1], policy()).unwrap();
        let columns = db.standing_native_columns(&cx, &closure).unwrap();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0], "source");
        assert_eq!(columns[1], "destination");
        let mut sub = db.open_standing_subscription(&cx, &closure).unwrap();
        sub.enable_replay(&mut db, &cx, 16, 1000, 10000, policy()).unwrap();
        let baseline = sub.poll(&db, &cx, policy()).unwrap().unwrap();
        assert_eq!(baseline.frontier(), basis);
        let mut delivered = baseline.rows().checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
        sub.acknowledge(baseline.receipt()).unwrap();
        let mut expected = Vec::new();
        let mut cuts = Vec::new();
        for tick in 0..4 {
            let mut batch = WriteBatch::new(RelationId(1));
            match tick {
                0 => { batch.delete_edge(EId(1)); }
                1 => { batch.delete_edge(EId(2)); }
                2 => { batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(0))); }
                _ => { batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(1))); }
            }
            cuts.push(db.write(&commit, batch).await.unwrap());
            let source = db.standing_rows(&cx, &parent).unwrap();
            let want = oracle(source.rows(), [0, 1]);
            assert_eq!(db.standing_closure(&cx, &closure).unwrap().rows(), &want);
            assert_eq!(db.standing_closure(&cx, &nested).unwrap().rows(), &want);
            expected.push(db.standing_native_bag(&cx, &closure, policy()).unwrap().1);
            assert!(db.standing_native_query(&cx, &closure, policy()).is_ok());
        }
        for (tick, at) in cuts.into_iter().enumerate() {
            let frame = sub.poll(&db, &cx, policy()).unwrap().unwrap();
            assert_eq!(frame.frontier(), at);
            if tick == 0 { assert!(frame.rows().is_empty()); }
            delivered.integrate(frame.rows(), LIMBS, &mut |_| Ok::<_, ()>(())).unwrap();
            assert_eq!(delivered, expected[tick]);
            sub.acknowledge(frame.receipt()).unwrap();
        }
        assert!(sub.poll(&db, &cx, policy()).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_closure_never_rejects_commit_and_rebuild_resets_delta_baseline() {
    let ((), report) = run_async_under_lab(0x6c05_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=3 { seed.create_vertex(VId(id), vec![], vec![]); }
        seed.add_edge(EId(1), VId(1), VId(2), vec![]);
        let basis = db.write(&commit, seed).await.unwrap();
        let source = db.register_standing_native(&cx, "MATCH (a)-[:R]->(b) RETURN a,b",
            &GqlParameters::new(), symbols, policy()).unwrap();
        let tight = db.register_standing_closure(&cx, &source, [0, 1],
            GqlQueryPolicy::new(100, 1, 100_000, 100_000)).unwrap();
        let sibling = db.register_standing_closure(&cx, &source, [1, 0], policy()).unwrap();
        let count = db.standing_queries.len();
        assert!(matches!(db.register_standing_closure(&cx, &source, [0, 8], policy()), Err(StandingQueryError::Unsupported)));
        assert_eq!(db.standing_queries.len(), count);
        let mut next = WriteBatch::new(RelationId(1));
        next.add_edge(EId(2), VId(2), VId(3), vec![]);
        let at = db.write(&commit, next).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(db.standing_closure(&cx, &tight), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget,
        }) if frontier == basis));
        assert_eq!(db.standing_closure(&cx, &sibling).unwrap().rows().len(), 3);
        assert!(db.rebuild_standing_query(&cx, &tight, GqlQueryPolicy::new(0, 10, 10000, 10000)).is_err());
        assert!(matches!(db.standing_closure(&cx, &tight), Err(StandingQueryError::Unavailable { .. })));
        db.rebuild_standing_query(&cx, &tight, policy()).unwrap();
        assert_eq!(db.standing_closure(&cx, &tight).unwrap().rows().len(), 3);
        assert!(matches!(db.standing_native_delta(&cx, &tight, basis, policy()), Err(StandingQueryError::DeltaUnavailable { .. })));
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(foreign.register_standing_closure(&cx, &source, [0, 1], policy()), Err(StandingQueryError::ForeignHandle)));
        assert!(foreign.standing_queries.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_input_schema_is_not_inferred_from_values_and_same_slot_is_an_explicit_edge() {
    let ((), report) = run_async_under_lab(0x6c05_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        let scalar = db.register_standing_native(&cx, "MATCH (n) RETURN n.enabled", &params, symbols, policy()).unwrap();
        let count = db.standing_queries.len();
        assert!(matches!(db.register_standing_closure(&cx, &scalar, [0, 0], policy()), Err(StandingQueryError::Unsupported)));
        assert_eq!(db.standing_queries.len(), count);
        let vertices = db.register_standing_native(&cx, "MATCH (n) RETURN n", &params, symbols, policy()).unwrap();
        let closure = db.register_standing_closure(&cx, &vertices, [0, 0], policy()).unwrap();
        assert!(db.standing_closure(&cx, &closure).unwrap().rows().is_empty());
        let mut add = WriteBatch::new(RelationId(1));
        add.create_vertex(VId(u128::MAX), vec![], vec![]);
        db.write(&commit, add).await.unwrap();
        assert_eq!(db.standing_closure(&cx, &closure).unwrap().rows(), &bag([(pair(u128::MAX, u128::MAX), 1)]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unavailable_parent_fences_recursion_until_parent_then_child_are_rebuilt() {
    let ((), report) = run_async_under_lab(0x6c05_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(RelationId(1));
        first.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, first).await.unwrap();
        let source = db.register_standing_native(&cx, "MATCH (n) RETURN n", &GqlParameters::new(), symbols,
            GqlQueryPolicy::new(100, 1, 100000, 100000)).unwrap();
        let child = db.register_standing_closure(&cx, &source, [0, 0], policy()).unwrap();
        let mut next = WriteBatch::new(RelationId(1));
        next.create_vertex(VId(2), vec![], vec![]);
        let at = db.write(&commit, next).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(db.standing_closure(&cx, &child), Err(StandingQueryError::Unavailable {
            reason: StandingQueryFailure::DependencyUnavailable, ..
        })));
        assert!(db.rebuild_standing_query(&cx, &child, policy()).is_err());
        db.rebuild_standing_query(&cx, &source, policy()).unwrap();
        assert!(matches!(db.standing_closure(&cx, &child), Err(StandingQueryError::Unavailable { .. })));
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert_eq!(db.standing_closure(&cx, &child).unwrap().rows(), &bag([(pair(1, 1), 1), (pair(2, 2), 1)]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
