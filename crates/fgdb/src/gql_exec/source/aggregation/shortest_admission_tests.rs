use super::*;
use super::super::ShortestMultiplicity;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_gql::{
    GlaExecutionLimits, GlaLimitDimension, GqlBudgetDimension, GqlExecutionBudget,
    GqlQueryError, GqlQueryPolicy, GraphShortestWalkCursor, GraphWalkBounds,
};
use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const DIRECTIONS: [GlaDirection; 3] = [
    GlaDirection::Forward,
    GlaDirection::Reverse,
    GlaDirection::Undirected,
];

fn edge(id: u128, src: u128, dst: u128, at: u64) -> AdjacencyEntry {
    AdjacencyEntry {
        eid: EId(id),
        src: VId(src),
        dst: VId(dst),
        relation: R,
        created_at: CommitSeq(at),
        retired_at: None,
    }
}

// An independent full-history merge, with no maintained-index lookup or source
// closure. Later restatements of one creation sequence supersede earlier ones.
fn full_adjacency(
    blocks: &[Vec<AdjacencyEntry>],
    at: CommitSeq,
    direction: GlaDirection,
) -> BTreeMap<VId, Vec<VId>> {
    let mut winners = BTreeMap::<EId, &AdjacencyEntry>::new();
    for entry in blocks.iter().flatten() {
        if entry.created_at <= at
            && winners
                .get(&entry.eid)
                .is_none_or(|old| old.created_at <= entry.created_at)
        {
            winners.insert(entry.eid, entry);
        }
    }
    let mut adjacency = BTreeMap::<VId, Vec<VId>>::new();
    for entry in winners.into_values() {
        if !entry.visible_at(at) || entry.relation != R {
            continue;
        }
        if direction != GlaDirection::Reverse {
            adjacency.entry(entry.src).or_default().push(entry.dst);
        }
        if direction == GlaDirection::Reverse
            || (direction == GlaDirection::Undirected && entry.src != entry.dst)
        {
            adjacency.entry(entry.dst).or_default().push(entry.src);
        }
    }
    for neighbors in adjacency.values_mut() {
        neighbors.sort();
    }
    adjacency
}

// Deliberately enumerate every bounded walk, including longer alternatives to
// settled vertices. Select the first admissible depth per endpoint only after
// enumeration. This is independent of the native cursor's shared layers.
fn oracle(
    adjacency: &BTreeMap<VId, Vec<VId>>,
    source: VId,
    bounds: GraphWalkBounds,
) -> Vec<VId> {
    let mut frontier = vec![source];
    let mut selected = BTreeMap::<VId, (u32, usize)>::new();
    for depth in 0..=bounds.maximum() {
        if depth >= bounds.minimum() {
            for &endpoint in &frontier {
                let (first, count) = selected.entry(endpoint).or_insert((depth, 0));
                if *first == depth {
                    *count += 1;
                }
            }
        }
        if depth < bounds.maximum() {
            frontier = frontier
                .into_iter()
                .flat_map(|vertex| adjacency.get(&vertex).into_iter().flatten().copied())
                .collect();
        }
    }
    selected
        .into_iter()
        .flat_map(|(vertex, (_, count))| std::iter::repeat_n(vertex, count))
        .collect()
}

fn evaluate(adjacency: &BTreeMap<VId, Vec<VId>>, source: VId, bounds: GraphWalkBounds) -> Vec<VId> {
    let mut control = |_| Ok::<(), ()>(());
    let mut cursor = GraphShortestWalkCursor::new(source, bounds, Some(adjacency), &mut control)
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_with_control(&mut control).unwrap() {
        rows.push(row);
    }
    rows.sort();
    rows
}

fn evaluate_any(adjacency: &BTreeMap<VId, Vec<VId>>, source: VId, bounds: GraphWalkBounds) -> Vec<VId> {
    let mut control = |_| Ok::<(), ()>(());
    let mut cursor = GraphShortestWalkCursor::new_any(source, bounds, Some(adjacency), &mut control)
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_with_control(&mut control).unwrap() {
        rows.push(row);
    }
    rows.sort();
    rows
}

#[test]
fn every_small_graph_matches_unpruned_walk_enumeration_in_every_direction() {
    for mask in 0..512_u16 {
        let mut entries = Vec::new();
        for bit in 0..9 {
            if mask & (1 << bit) != 0 {
                entries.push(edge(bit, bit / 3, bit % 3, 1));
            }
        }
        // Add a parallel occurrence to half the graphs, without conflating IDs.
        if mask & 2 != 0 {
            entries.push(edge(u128::MAX, 0, 1, 1));
        }
        let blocks = vec![entries];
        let index = AdjacencyIndex::build(&blocks);
        for direction in DIRECTIONS {
            let full = full_adjacency(&blocks, CommitSeq(1), direction);
            for source in [VId(0), VId(1), VId(2)] {
                for maximum in 0..=3 {
                    let local = collect(
                        &index,
                        &blocks,
                        Scope { source, relation: R, direction, as_of: CommitSeq(1), maximum },
                        &mut |_| Ok::<(), ()>(()),
                    ).unwrap();
                    for minimum in 0..=maximum {
                        let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                        assert_eq!(evaluate(&local, source, bounds), oracle(&full, source, bounds),
                            "mask={mask} source={source:?} direction={direction:?} bounds={bounds:?}");
                        let mut expected = oracle(&full, source, bounds);
                        expected.dedup();
                        assert_eq!(evaluate_any(&local, source, bounds), expected,
                            "ANY mask={mask} source={source:?} bounds={bounds:?}");
                    }
                }
            }
        }
    }
}

#[test]
fn history_retirements_relations_and_terminal_faces_do_not_leak_into_the_closure() {
    let blocks = vec![
        vec![edge(0, 0, 1, 1), edge(1, 0, 1, 1), edge(2, 1, 2, 1),
             edge(3, 2, 3, 1), edge(u128::MAX, 0, 0, 1), edge(8, 90, 91, 1),
             AdjacencyEntry { relation: RelationId(2), ..edge(9, 0, 90, 1) }],
        vec![AdjacencyEntry { retired_at: Some(CommitSeq(2)), ..edge(0, 0, 1, 1) },
             edge(0, 0, 1, 2), edge(10, 0, 3, 3)],
        vec![AdjacencyEntry { retired_at: Some(CommitSeq(4)), ..edge(0, 0, 1, 2) }],
    ];
    let index = AdjacencyIndex::build(&blocks);
    for at in 0..=5 {
        for direction in DIRECTIONS {
            let full = full_adjacency(&blocks, CommitSeq(at), direction);
            for maximum in 0..=3 {
                let mut records = 0;
                let local = collect(&index, &blocks, Scope {
                    source: VId(0), relation: R, direction, as_of: CommitSeq(at), maximum,
                }, &mut |event| {
                    records += u64::from(event == SourceEvent::SnapshotRecord);
                    Ok::<(), ()>(())
                }).unwrap();
                for minimum in 0..=maximum {
                    let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                    assert_eq!(evaluate(&local, VId(0), bounds), oracle(&full, VId(0), bounds));
                }
                assert!(!local.contains_key(&VId(90)));
                if maximum == 0 {
                    assert!(local.is_empty());
                    assert_eq!(records, 0);
                }
                if at == 1 && maximum == 2 && direction == GlaDirection::Forward {
                    assert_eq!(local[&VId(0)], vec![VId(0), VId(1), VId(1)]);
                    assert_eq!(local[&VId(1)], vec![VId(2)]);
                    assert!(!local.contains_key(&VId(2)));
                    assert_eq!(records, 4);
                }
            }
        }
    }
}

#[test]
fn every_source_control_failure_discards_the_private_closure_and_allows_retry() {
    let blocks = vec![vec![edge(0, 0, 1, 1), edge(1, 0, 1, 1),
                           edge(2, 1, 2, 1), edge(u128::MAX, 0, 0, 1)]];
    let index = AdjacencyIndex::build(&blocks);
    for direction in DIRECTIONS {
        let scope = Scope { source: VId(0), relation: R, direction,
                            as_of: CommitSeq(1), maximum: 3 };
        let mut total = 0;
        let expected = collect(&index, &blocks, scope, &mut |_| {
            total += 1;
            Ok::<(), usize>(())
        }).unwrap();
        assert!(total > 10);
        for stop in 1..=total {
            let mut calls = 0;
            assert_eq!(collect(&index, &blocks, scope, &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            }), Err(stop));
            assert_eq!(calls, stop);
        }
        assert_eq!(collect(&index, &blocks, scope, &mut |_| Ok::<(), ()>(())).unwrap(), expected);
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x52; 32], DatabaseSecurityNamespaceId([0x53; 32]), [0x54; 32])
}

fn policy(records: u64, rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy {
        rows: GqlExecutionBudget::new(records, rows),
        evaluator: GlaExecutionLimits { max_work_units: 1_000_000, max_scratch_entries: 1_000_000 },
    }
}

#[test]
fn local_shortest_queries_admit_only_reachable_edges_and_keep_history_on_reopen() {
    let ((), report) = run_async_under_lab(0x5200_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 0..=140 { seed.create_vertex(VId(id), vec![], vec![]); }
        for (id, src, dst) in [(0, 0, 1), (1, 0, 1), (2, 1, 2), (3, 2, 3)] {
            seed.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        for id in 10..140 { seed.add_edge(EId(id), VId(id), VId(id + 1), vec![]); }
        db.write(&commit, seed).await.unwrap();
        let mut other = WriteBatch::new(RelationId(2));
        other.add_edge(EId(999), VId(0), VId(10), vec![]);
        let old = db.write(&commit, other).await.unwrap();
        let pinned = db.read_session().unwrap();
        let bounds = GraphWalkBounds::new(0, 2).unwrap();
        let expected = vec![VId(0), VId(1), VId(1), VId(2), VId(2)];
        let result = db.execute_all_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, policy(3, 5),
        ).unwrap();
        assert_eq!(result.value, expected);
        assert_eq!(result.rows.snapshot_records, 3);
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(0));
        changes.add_edge(EId(500), VId(0), VId(2), vec![]);
        let live = db.write(&commit, changes).await.unwrap();
        assert_eq!(pinned.execute_all_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, policy(3, 5),
        ).unwrap().value, expected);
        assert_eq!(db.execute_all_shortest_walk_governed_at(
            &query, VId(0), R, GlaDirection::Forward, bounds, old, policy(3, 5),
        ).unwrap().value, expected);
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), live);
        assert_eq!(db.execute_all_shortest_walk_governed_at(
            &query, VId(0), R, GlaDirection::Forward, bounds, old, policy(3, 5),
        ).unwrap().value, expected);
        assert_eq!(db.execute_all_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, policy(4, 4),
        ).unwrap().value, vec![VId(0), VId(1), VId(2), VId(3)]);
        for source in [VId(0), VId(9999)] {
            let zero = db.execute_all_shortest_walk_governed(
                &query, source, R, GlaDirection::Undirected,
                GraphWalkBounds::new(0, 0).unwrap(), policy(0, 1),
            ).unwrap();
            assert_eq!(zero.rows.snapshot_records, 0);
            assert_eq!(zero.value, if source == VId(0) { vec![source] } else { vec![] });
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_allowance_covers_source_native_cursor_results_and_every_interruption() {
    let ((), report) = run_async_under_lab(0x5200_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for id in 0..=2 { seed.create_vertex(VId(id), vec![], vec![]); }
        for (id, src, dst) in [(0, 0, 1), (1, 0, 1), (2, 1, 2), (3, 0, 0)] {
            seed.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        let at = db.write(&commit, seed).await.unwrap();
        let bounds = GraphWalkBounds::new(1, 3).unwrap();
        for multiplicity in [ShortestMultiplicity::All, ShortestMultiplicity::Any] {
        let execute = |budget, stop| {
            let mut calls = 0;
            let result = super::super::execute_shortest_at(
                &db.snapshot, VId(0), R, GlaDirection::Undirected, bounds, at, multiplicity, budget, || {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                },
            );
            (result, calls)
        };
        let (result, total) = execute(policy(100, 100), usize::MAX);
        let baseline = result.unwrap();
        assert_eq!(baseline.rows.snapshot_records, 4, "undirected faces share source IDs");
        let mut exact = policy(baseline.rows.snapshot_records, baseline.rows.result_rows);
        exact.evaluator.max_work_units = baseline.evaluator.work_units;
        exact.evaluator.max_scratch_entries = baseline.evaluator.scratch_entries;
        for _ in 0..2 {
            let checked = execute(exact, usize::MAX).0.unwrap();
            assert_eq!(checked.value, baseline.value);
            assert_eq!(checked.evaluator, baseline.evaluator);
        }
        for dimension in 0..4 {
            let mut below = exact;
            match dimension {
                0 => below.rows = GqlExecutionBudget::new(baseline.rows.snapshot_records - 1, 100),
                1 => below.rows = GqlExecutionBudget::new(100, baseline.rows.result_rows - 1),
                2 => below.evaluator.max_work_units -= 1,
                _ => below.evaluator.max_scratch_entries -= 1,
            }
            match execute(below, usize::MAX).0.unwrap_err() {
                GqlQueryError::Rows(error) if dimension < 2 => assert_eq!(error.dimension,
                    if dimension == 0 { GqlBudgetDimension::SnapshotRecords } else { GqlBudgetDimension::ResultRows }),
                GqlQueryError::Evaluator(error) if dimension >= 2 => assert_eq!(error.dimension,
                    if dimension == 2 { GlaLimitDimension::WorkUnits } else { GlaLimitDimension::ScratchEntries }),
                error => panic!("wrong refusal for {dimension}: {error:?}"),
            }
        }
        for stop in 1..=total {
            let (result, calls) = execute(policy(100, 100), stop);
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(calls, stop);
        }
        assert_eq!(db.frontier().unwrap(), at);
        assert_eq!(execute(exact, usize::MAX).0.unwrap().value, baseline.value);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn any_shortest_finishes_under_one_result_allowance_despite_exponential_ties() {
    let ((), report) = run_async_under_lab(0x5200_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in 0..=40 {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        for vertex in 0..40 {
            for parallel in 0..2 {
                seed.add_edge(EId(2 * vertex + parallel), VId(vertex), VId(vertex + 1), vec![]);
            }
        }
        db.write(&commit, seed).await.unwrap();
        // Two choices at each of 40 hops: 2^40 tied WALK occurrences, but one
        // ANY endpoint. A post-hoc DISTINCT implementation exhausts this quota.
        let bounds = GraphWalkBounds::new(40, 40).unwrap();
        let allowance = policy(80, 1);
        let result = db.execute_any_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, allowance,
        ).unwrap();
        assert_eq!(result.value, vec![VId(40)]);
        assert_eq!(result.rows.snapshot_records, 80);
        assert_eq!(result.rows.result_rows, 1);
        assert!(matches!(db.execute_all_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, allowance,
        ), Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows));
        assert_eq!(db.execute_any_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, allowance,
        ).unwrap().evaluator, result.evaluator);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn any_shortest_delays_settlement_and_preserves_pinned_historical_and_empty_results() {
    let ((), report) = run_async_under_lab(0x5200_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in 0..=2 {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        seed.add_edge(EId(0), VId(0), VId(1), vec![]);
        seed.add_edge(EId(1), VId(0), VId(1), vec![]);
        seed.add_edge(EId(2), VId(1), VId(0), vec![]);
        let before = db.write(&commit, seed).await.unwrap();
        let pinned = db.read_session().unwrap();
        let bounds = GraphWalkBounds::new(2, 2).unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(2));
        changes.create_vertex(VId(3), vec![], vec![]);
        changes.add_edge(EId(3), VId(1), VId(3), vec![]);
        let after = db.write(&commit, changes).await.unwrap();
        assert_eq!(pinned.execute_any_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, policy(3, 1),
        ).unwrap().value, vec![VId(0)]);
        assert_eq!(pinned.execute_any_shortest_walk_governed_at(
            &query, VId(0), R, GlaDirection::Forward, bounds, before, policy(3, 1),
        ).unwrap().value, vec![VId(0)]);
        assert!(pinned.execute_any_shortest_walk_governed_at(
            &query, VId(0), R, GlaDirection::Forward, bounds, after, policy(3, 1),
        ).is_err());
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.execute_any_shortest_walk_governed_at(
            &query, VId(0), R, GlaDirection::Forward, bounds, before, policy(3, 1),
        ).unwrap().value, vec![VId(0)]);
        assert_eq!(db.execute_any_shortest_walk_governed(
            &query, VId(0), R, GlaDirection::Forward, bounds, policy(3, 1),
        ).unwrap().value, vec![VId(3)]);
        for (source, minimum, expected) in [
            (VId(2), 0, vec![VId(2)]),
            (VId(2), 1, vec![]),
            (VId(999), 0, vec![]),
        ] {
            let execution = db.execute_any_shortest_walk_governed(
                &query, source, R, GlaDirection::Undirected,
                GraphWalkBounds::new(minimum, 2).unwrap(), policy(0, 1),
            ).unwrap();
            assert_eq!(execution.value, expected);
            assert_eq!(execution.rows.snapshot_records, 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
