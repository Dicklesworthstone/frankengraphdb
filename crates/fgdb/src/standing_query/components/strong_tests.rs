use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xc1; 32], DatabaseSecurityNamespaceId([0xc2; 32]), [0xc3; 32])
}

// Independent source oracle: BFS from every vertex followed by mutual
// reachability, not either maintained component kernel or DFS finish order.
fn oracle(db: &Database<MemVfs>, relation: RelationId) -> BTreeMap<VId, VId> {
    let vertices: BTreeSet<_> = db.vertices().unwrap().iter().map(|row| row.vid).collect();
    let edges = db.edges().unwrap();
    let mut closure = BTreeMap::new();
    for vertex in &vertices {
        let mut reached = BTreeSet::from([*vertex]);
        let mut todo = std::collections::VecDeque::from([*vertex]);
        while let Some(from) = todo.pop_front() {
            for edge in &edges {
                if edge.entry.relation == relation && edge.entry.src == from
                    && reached.insert(edge.entry.dst)
                {
                    todo.push_back(edge.entry.dst);
                }
            }
        }
        closure.insert(*vertex, reached);
    }
    vertices.iter().map(|vertex| {
        let root = vertices.iter().find(|other| {
            closure[vertex].contains(*other) && closure[*other].contains(vertex)
        }).unwrap();
        (*vertex, *root)
    }).collect()
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, relation: RelationId) {
    let expected = oracle(db, relation);
    let view = db.standing_components(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert!(view.ordered_rows().is_none());
    let actual: BTreeMap<_, _> = view.rows().iter().map(|((vertex, root), weight)| {
        assert_eq!(weight, &ZWeight::ONE);
        (*vertex, *root)
    }).collect();
    assert_eq!(actual, expected);
    assert_eq!(view.rows().len(), actual.len(), "one representative per live vertex");
    assert_eq!(db.standing_component_count(cx, handle).unwrap(),
        expected.values().collect::<BTreeSet<_>>().len());
    for (vertex, representative) in expected {
        assert_eq!(db.standing_component(cx, handle, vertex).unwrap(), Some(representative));
    }
    assert_eq!(db.standing_component(cx, handle, VId(999)).unwrap(), None);
}
fn build(snapshot: &crate::Snapshot) -> State {
    let mut checkpoint = || Ok(());
    let mut meter = Meter {
        policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
    };
    State::from_snapshot(snapshot, ComponentRelation::Strong(R), &mut meter).unwrap()
}
fn unchanged(actual: &State, before: &State) {
    assert_eq!(actual.input, before.input);
    assert_eq!(actual.components, before.components);
    assert_eq!(actual.rows, before.rows);
    assert_eq!(actual.relation, before.relation);
    assert_eq!(actual.frontier, before.frontier);
    assert_eq!(actual.stats, before.stats);
    assert_eq!(actual.policy, before.policy);
    assert_eq!(actual.failure, before.failure);
}

#[test]
fn committed_direction_changes_parallel_edges_and_cascades_match_source_oracle() {
    let ((), report) = run_async_under_lab(0x7363_6301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let strong = db.register_standing_strong_components(&cx, R, policy()).unwrap();
        let other = db.register_standing_strong_components(&cx, S, policy()).unwrap();
        let weak = db.register_standing_components(&cx, R, policy()).unwrap();
        check(&db, &cx, &strong, R);
        let mut seed = WriteBatch::new(S);
        for vertex in [0, 1, 2, 3, u128::MAX] {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let mut edges = WriteBatch::new(R);
        for (eid, src, dst) in [
            (10, 0, 1), (11, 0, 1), (12, 1, 2), (13, 0, 2),
            (14, u128::MAX, u128::MAX),
        ] {
            edges.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, edges).await.unwrap();
        assert_eq!(db.standing_component_count(&cx, &strong).unwrap(), 5);
        assert_eq!(db.standing_component_count(&cx, &weak).unwrap(), 3);
        let mut second = WriteBatch::new(S);
        second.add_edge(EId(20), VId(3), VId(u128::MAX), vec![]);
        second.add_edge(EId(21), VId(u128::MAX), VId(3), vec![]);
        db.write(&commit, second).await.unwrap();
        check(&db, &cx, &strong, R);
        check(&db, &cx, &other, S);
        assert_eq!(db.standing_components(&cx, &strong).unwrap().last_maintenance().affected_vertices, 0);

        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut cycle = WriteBatch::new(R);
        // This reverses support without changing the weak projection.
        cycle.delete_edge(EId(13));
        cycle.add_edge(EId(15), VId(2), VId(0), vec![]);
        txn.write(&mut db, cycle).unwrap();
        assert_eq!(db.standing_component_count(&cx, &strong).unwrap(), 5);
        assert_eq!(txn.commit(&mut db, &commit).await.unwrap(), CommitSeq(basis.0 + 1));
        assert_eq!(pinned.frontier(), basis);
        assert!(pinned.edge(EId(13)).unwrap().is_some());
        assert!(pinned.edge(EId(15)).unwrap().is_none());
        check(&db, &cx, &strong, R);
        assert_eq!(db.standing_component_count(&cx, &strong).unwrap(), 3);
        assert_eq!(db.standing_components(&cx, &strong).unwrap().last_maintenance().affected_vertices, 3);
        let mut properties = WriteBatch::new(R);
        properties.set_vertex_property(VId(0), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        properties.set_edge_property(EId(10), PropertyKeyId(1), Some(CanonicalScalar::Int(8)));
        db.write(&commit, properties).await.unwrap();
        assert_eq!(db.standing_components(&cx, &strong).unwrap().last_maintenance().affected_vertices, 0);
        for (eid, expected_count, affected) in [(10, 3, 0), (11, 5, 3)] {
            let mut remove = WriteBatch::new(R);
            remove.delete_edge(EId(eid));
            db.write(&commit, remove).await.unwrap();
            check(&db, &cx, &strong, R);
            assert_eq!(db.standing_component_count(&cx, &strong).unwrap(), expected_count);
            assert_eq!(db.standing_components(&cx, &strong).unwrap().last_maintenance().affected_vertices, affected);
        }
        let mut abort = db.begin(&txcx).unwrap();
        let mut abandoned = WriteBatch::new(R);
        abandoned.add_edge(EId(16), VId(0), VId(1), vec![]);
        abort.write(&mut db, abandoned).unwrap();
        abort.abort();
        check(&db, &cx, &strong, R);
        assert!(db.edge(EId(16)).unwrap().is_none());
        let mut cascade = WriteBatch::new(S);
        cascade.delete_vertex(VId(2));
        db.write(&commit, cascade).await.unwrap();
        check(&db, &cx, &strong, R);
        check(&db, &cx, &other, S);
        assert_eq!(db.standing_component(&cx, &strong, VId(2)).unwrap(), None);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn derived_failure_and_rebuild_never_downgrade_strong_connectivity() {
    let ((), report) = run_async_under_lab(0x7363_6302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let two = GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000);
        let limited = db.register_standing_strong_components(&cx, R, two).unwrap();
        let sibling = db.register_standing_components(&cx, R, policy()).unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in 0..3 {
            seed.create_vertex(VId(vertex), vec![], vec![]);
        }
        seed.add_edge(EId(1), VId(0), VId(1), vec![]);
        let seq = db.write(&commit, seed).await.unwrap();
        assert_eq!(db.frontier().unwrap(), seq, "view refusal does not undo Chronicle");
        assert_eq!(db.standing_component_count(&cx, &sibling).unwrap(), 2);
        assert!(matches!(db.standing_components(&cx, &limited),
            Err(StandingQueryError::Unavailable {
                frontier: CommitSeq::ORIGIN, reason: StandingQueryFailure::ResultBudget,
            })));
        assert!(matches!(db.rebuild_standing_query(&cx, &limited, two),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::ResultBudget))));
        assert!(matches!(db.standing_components(&cx, &limited),
            Err(StandingQueryError::Unavailable { frontier: CommitSeq::ORIGIN, .. })));
        assert_eq!(db.rebuild_standing_query(&cx, &limited, policy()).unwrap(), seq);
        check(&db, &cx, &limited, R);
        assert_eq!(db.standing_component_count(&cx, &limited).unwrap(), 3);
        let before = *db.standing_components(&cx, &limited).unwrap().last_maintenance();
        assert!(matches!(db.rebuild_standing_query(&cx, &limited,
            GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000)),
            Err(StandingQueryError::Maintenance(StandingQueryFailure::SnapshotBudget))));
        assert_eq!(db.standing_components(&cx, &limited).unwrap().last_maintenance(), &before);
        let mut close = WriteBatch::new(R);
        close.add_edge(EId(2), VId(1), VId(0), vec![]);
        db.write(&commit, close).await.unwrap();
        check(&db, &cx, &limited, R);
        assert_eq!(db.standing_component_count(&cx, &limited).unwrap(), 2);
        let mut split = WriteBatch::new(R);
        split.delete_edge(EId(2));
        db.write(&commit, split).await.unwrap();
        check(&db, &cx, &limited, R);
        assert_eq!(db.standing_component_count(&cx, &limited).unwrap(), 3);
        assert_eq!(db.standing_component_count(&cx, &sibling).unwrap(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_composed_interruption_and_exact_budget_preserves_input_kernel_and_sink() {
    let ((), report) = run_async_under_lab(0x7363_6303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for vertex in 0..5 { seed.create_vertex(VId(vertex), vec![], vec![]); }
        for (eid, src, dst) in [(1, 0, 1), (2, 1, 2), (3, 2, 0), (4, 3, 4)] {
            seed.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, seed).await.unwrap();
        let baseline = Arc::clone(&db.snapshot);
        let before = build(&baseline);
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(1));
        change.create_vertex(VId(6), vec![], vec![]);
        change.add_edge(EId(5), VId(2), VId(3), vec![]);
        change.add_edge(EId(6), VId(4), VId(2), vec![]);
        db.write(&commit, change).await.unwrap();
        let batch = db.delta_since(baseline.frontier).unwrap().next().unwrap();
        let expected = build(&db.snapshot);
        let mut success = build(&baseline);
        let mut calls = 0;
        let stats = {
            let mut checkpoint = || { calls += 1; Ok(()) };
            let mut meter = Meter {
                policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
            };
            success.maintain(&commit, batch, &mut meter).unwrap();
            meter.stats
        };
        assert_eq!(success.components, expected.components);
        assert_eq!(success.input, expected.input);
        assert_eq!(success.rows, expected.rows);
        assert!(calls > 0 && stats.work_units > 0 && stats.scratch_entries > 0);
        for stop in 1..=calls {
            let mut state = build(&baseline);
            let mut seen = 0;
            {
                let mut checkpoint = || {
                    seen += 1;
                    if seen == stop { Err(StandingQueryFailure::Interrupted) } else { Ok(()) }
                };
                let mut meter = Meter {
                    policy: policy(), stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
                };
                assert_eq!(state.maintain(&commit, batch, &mut meter), Err(StandingQueryFailure::Interrupted));
            }
            assert_eq!(seen, stop);
            unchanged(&state, &before);
        }
        for (work, scratch, rows, refusal) in [
            (stats.work_units, stats.scratch_entries, 5, None),
            (stats.work_units - 1, stats.scratch_entries, 5, Some(StandingQueryFailure::WorkBudget)),
            (stats.work_units, stats.scratch_entries - 1, 5, Some(StandingQueryFailure::ScratchBudget)),
            (stats.work_units, stats.scratch_entries, 4, Some(StandingQueryFailure::ResultBudget)),
        ] {
            let mut state = build(&baseline);
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(100_000, rows, work, scratch),
                stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
            };
            let result = state.maintain(&commit, batch, &mut meter);
            if let Some(reason) = refusal {
                assert_eq!(result, Err(reason));
                unchanged(&state, &before);
            } else {
                result.unwrap();
                assert_eq!(state.components, expected.components);
                assert_eq!(state.rows, expected.rows);
            }
        }
        let physical = db.snapshot.blocks.iter().map(|block| block.len()).sum::<usize>()
            + db.snapshot.patches.iter().map(|patch| patch.len()).sum::<usize>();
        assert!(physical > 0);
        for records in [physical as u64, physical as u64 - 1] {
            let mut checkpoint = || Ok(());
            let mut meter = Meter {
                policy: GqlQueryPolicy::new(records, 5, 10_000_000, 10_000_000),
                stats: StandingQueryStats::default(), checkpoint: &mut checkpoint,
            };
            let result = State::from_snapshot(&db.snapshot, ComponentRelation::Strong(R), &mut meter);
            if records == physical as u64 {
                assert_eq!(result.unwrap().rows, expected.rows);
            } else {
                assert!(matches!(result, Err(StandingQueryFailure::SnapshotBudget)));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compaction_failure_fences_and_reopen_keep_one_authenticated_view_boundary() {
    use crate::{DatabaseState, DerivedPublicationStage};
    use fgdb_chronicle::commit::CrashPoint;

    let ((), report) = run_async_under_lab(0x7363_6304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        for derived_failure in [false, true] {
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
            let handle = db.register_standing_strong_components(&cx, R, policy()).unwrap();
            let wrong_kind = db.register_standing_reachability(&cx, R, policy()).unwrap();
            let mut seed = WriteBatch::new(R);
            for vertex in 0..3 { seed.create_vertex(VId(vertex), vec![], vec![]); }
            seed.add_edge(EId(1), VId(0), VId(1), vec![]);
            seed.add_edge(EId(2), VId(1), VId(2), vec![]);
            db.write(&commit, seed).await.unwrap();
            assert!(matches!(db.standing_components(&cx, &wrong_kind), Err(StandingQueryError::Unsupported)));
            assert!(matches!(db.standing_reachability(&cx, &handle), Err(StandingQueryError::Unsupported)));
            db.compact(&commit).await.unwrap();
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            check(&db, &cx, &handle, R);
            assert_eq!(db.standing_component_count(&cx, &handle).unwrap(), 3);
            let admitted = db.read_session().unwrap();
            let mut next = WriteBatch::new(R);
            next.create_vertex(VId(3), vec![], vec![]);
            let prepared = db.prepare_write(next).unwrap();
            let crash = (!derived_failure).then_some(CrashPoint::AfterMarkerBeforeD2);
            let publication = derived_failure.then_some(DerivedPublicationStage::FoldCommittedTemplate);
            assert!(db.commit_template(&commit, prepared.template, crash, publication, None).await.is_err());
            assert!(matches!(db.state(), DatabaseState::CommitOutcomeUnknown { .. }
                | DatabaseState::NeedsAuthoritativeRecovery(_)));
            let count = db.standing_queries.len();
            for error in [
                db.standing_components(&cx, &handle).unwrap_err(),
                db.standing_component_count(&cx, &handle).unwrap_err(),
                db.standing_component(&cx, &handle, VId(999)).unwrap_err(),
                db.rebuild_standing_query(&cx, &handle, policy()).unwrap_err(),
                db.register_standing_strong_components(&cx, R, policy()).unwrap_err(),
            ] {
                assert!(matches!((derived_failure, error),
                    (false, StandingQueryError::Read(ReadError::CommitOutcomeUnknown { .. }))
                    | (true, StandingQueryError::Read(ReadError::RecoveryRequired(_)))));
            }
            assert_eq!(db.standing_queries.len(), count);
            assert_eq!(admitted.vertices().unwrap().len(), 3);
            drop(db);
            let mut db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
            assert!(matches!(db.standing_components(&cx, &handle), Err(StandingQueryError::ForeignHandle)));
            assert!(matches!(db.rebuild_standing_query(&cx, &handle, policy()), Err(StandingQueryError::ForeignHandle)));
            let fresh = db.register_standing_strong_components(&cx, R, policy()).unwrap();
            check(&db, &cx, &fresh, R);
            let mut cycle = WriteBatch::new(R);
            cycle.add_edge(EId(3), VId(2), VId(0), vec![]);
            db.write(&commit, cycle).await.unwrap();
            check(&db, &cx, &fresh, R);
            assert_eq!(db.standing_component(&cx, &fresh, VId(2)).unwrap(), Some(VId(0)));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
