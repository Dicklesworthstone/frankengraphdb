//! Typed connected patterns use real snapshot/overlay readers and one GLA.
//! The oracle enumerates vertex assignments over ordinary owned storage rows;
//! it never calls GLA lowering, its predicates, or the borrowed query sources.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DerivedPublicationStage, EdgeRecord, GqlError, MemVfs,
    ReadError, VertexRow, WriteBatch, WriteError, WriteTxnError,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{
    GlaDirection, GraphPatternBuilder, IntegerComparison, PreparedGraphPattern, VertexPredicate,
};
use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeSet;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const T: RelationId = RelationId(3);
const U: RelationId = RelationId(4);
const L: LabelId = LabelId(1);
const N: PropertyKeyId = PropertyKeyId(1);
const HIGH: VId = VId((1_u128 << 96) + 7);
const NAMES: [&str; 4] = ["a", "b", "c", "d"];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xd1; 32], DatabaseSecurityNamespaceId([0xd2; 32]), [0xd3; 32])
}

fn generous() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000)
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut first = WriteBatch::new(R);
    for (vid, n) in [(VId(1), 1), (VId(2), 2), (VId(3), 3), (VId(4), 4), (VId(5), 5), (HIGH, 9)] {
        first.create_vertex(vid, if vid == VId(5) { vec![] } else { vec![L] },
            vec![(N, CanonicalScalar::Int(n))]);
    }
    for (eid, src, dst) in [(10, VId(1), VId(2)), (11, VId(1), VId(2)),
        (12, VId(2), VId(3)), (13, VId(3), VId(4)), (14, VId(4), HIGH)] {
        first.add_edge(EId(eid), src, dst, vec![]);
    }
    db.write(cx, first).await.unwrap();
    for (relation, rows) in [
        (S, vec![(20, VId(4), VId(3)), (21, VId(2), VId(4))]),
        (T, vec![(30, VId(4), VId(1)), (31, VId(3), VId(4))]),
        (U, vec![(40, VId(1), VId(3))]),
    ] {
        let mut batch = WriteBatch::new(relation);
        for (eid, src, dst) in rows { batch.add_edge(EId(eid), src, dst, vec![]); }
        db.write(cx, batch).await.unwrap();
    }
}

#[derive(Clone, Copy)]
struct Atom(usize, RelationId, GlaDirection, usize);

struct Case {
    atoms: Vec<Atom>,
    filtered: bool,
    unequal: bool,
}

impl Case {
    fn prepare(&self, projected: usize, offset: u64, count: Option<u64>) -> PreparedGraphPattern {
        let mut b = GraphPatternBuilder::new();
        for name in NAMES { b.vertex(name).unwrap(); }
        for &Atom(s, r, direction, d) in &self.atoms {
            b.edge(NAMES[s], r, direction, NAMES[d]).unwrap();
        }
        if self.filtered {
            b.filter("a", VertexPredicate::HasLabel(L)).unwrap();
            b.filter("c", VertexPredicate::IntegerProperty {
                key: N, comparison: IntegerComparison::GreaterOrEqual, value: 3,
            }).unwrap();
        }
        if self.unequal { b.identity("a", "d", false).unwrap(); }
        b.prepare(NAMES[projected], offset, count).unwrap()
    }

    fn expected(&self, vertices: &[VertexRow], edges: &[EdgeRecord], projected: usize) -> Vec<VId> {
        let mut out = BTreeSet::new();
        for a in vertices {
            for b in vertices {
                for c in vertices {
                    for d in vertices {
                        let bound = [a, b, c, d];
                        if self.filtered && (!a.labels.contains(&L) || !c.props.iter().any(|(key, scalar)| {
                            *key == N && matches!(scalar, CanonicalScalar::Int(value) if *value >= 3)
                        })) { continue; }
                        if self.unequal && a.vid == d.vid { continue; }
                        let complete = self.atoms.iter().all(|&Atom(s, r, direction, d)| {
                            edges.iter().any(|row| {
                                let edge = row.entry;
                                if edge.relation != r { return false; }
                                let left = bound[s].vid;
                                let right = bound[d].vid;
                                match direction {
                                    GlaDirection::Forward => edge.src == left && edge.dst == right,
                                    GlaDirection::Reverse => edge.dst == left && edge.src == right,
                                    GlaDirection::Undirected => (edge.src == left && edge.dst == right)
                                        || (edge.dst == left && edge.src == right),
                                }
                            })
                        });
                        if complete { out.insert(bound[projected].vid); }
                    }
                }
            }
        }
        out.into_iter().collect()
    }
}

fn cases() -> Vec<Case> {
    use GlaDirection::{Forward as F, Reverse as Rv, Undirected as Both};
    vec![
        Case { atoms: vec![Atom(0, R, F, 1), Atom(1, R, F, 2), Atom(2, R, F, 3)], filtered: false, unequal: false },
        Case { atoms: vec![Atom(0, R, F, 1), Atom(2, S, Rv, 3), Atom(2, R, Rv, 1),
            Atom(3, T, F, 0), Atom(0, U, F, 2)], filtered: true, unequal: true },
        Case { atoms: vec![Atom(0, R, F, 1), Atom(1, S, F, 3), Atom(0, U, F, 2),
            Atom(2, T, F, 3)], filtered: false, unequal: true },
        Case { atoms: vec![Atom(0, R, Both, 1), Atom(2, R, Both, 3), Atom(1, R, Both, 2),
            Atom(3, T, Both, 0)], filtered: true, unequal: false },
    ]
}

#[test]
fn connected_patterns_match_independent_assignments_on_live_pinned_and_transaction_reads() {
    let ((), report) = run_async_under_lab(0xc0a4_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let txn = db.begin(&txn_cx).unwrap();
        let vertices = db.vertices().unwrap();
        let edges = db.edges().unwrap();
        for case in cases() {
            for projection in 0..4 {
                let pattern = case.prepare(projection, 0, None);
                let expected = case.expected(&vertices, &edges, projection);
                let live = db.execute_graph_pattern_governed(&query_cx, &pattern, generous()).unwrap();
                assert_eq!(live.value, expected);
                assert_eq!(live.rows.snapshot_records, edges.len() as u64);
                assert_eq!(live.rows.result_rows, expected.len() as u64);
                assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, generous()).unwrap(), live);
                assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, &pattern, generous()).unwrap(), live);
                let overlay = txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, generous()).unwrap();
                assert_eq!(overlay.value, expected);
                assert_eq!(overlay.rows, live.rows);
                let paged = case.prepare(projection, 1, Some(1));
                assert_eq!(db.execute_graph_pattern_governed(&query_cx, &paged, generous()).unwrap().value,
                    expected.into_iter().skip(1).take(1).collect::<Vec<_>>());
            }
        }
        let long = cases()[0].prepare(3, 0, None);
        assert_eq!(db.execute_graph_pattern_governed(&query_cx, &long, generous()).unwrap().value, vec![VId(4), HIGH]);
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_patterns_keep_their_basis_and_survive_commit_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xc0a4_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let cases = cases();
        let patterns: Vec<_> = cases.iter().map(|case| case.prepare(3, 0, None)).collect();
        let before: Vec<_> = cases.iter().map(|case| case.expected(&db.vertices().unwrap(), &db.edges().unwrap(), 3)).collect();
        let mut txn = db.begin(&txn_cx).unwrap();
        let mut stage = WriteBatch::new(R);
        stage.delete_edge(EId(12));
        stage.set_vertex_property(VId(3), N, Some(CanonicalScalar::Int(0)));
        stage.create_vertex(VId(9), vec![L], vec![(N, CanonicalScalar::Int(9))]);
        stage.add_edge(EId(50), HIGH, VId(9), vec![]);
        stage.ensure_edge_by_triple(EId(999), VId(1), VId(2), vec![]);
        stage.create_vertex(VId(88), vec![L], vec![]);
        stage.delete_vertex(VId(88));
        txn.write(&mut db, stage).unwrap();
        let vertices = txn.vertices(&db).unwrap();
        let edges = txn.edges(&db).unwrap();
        assert!(!edges.iter().any(|row| row.entry.eid == EId(999)));
        let after: Vec<_> = cases.iter().map(|case| case.expected(&vertices, &edges, 3)).collect();
        assert_ne!(before, after);
        for ((pattern, expected), historical) in patterns.iter().zip(&after).zip(&before) {
            let run = txn.execute_graph_pattern_governed(&db, &query_cx, pattern, generous()).unwrap();
            assert_eq!(run.value, *expected);
            assert_eq!(run.rows.snapshot_records, edges.len() as u64);
            assert_eq!(txn.execute_graph_pattern_governed(&db, &query_cx, pattern, generous()).unwrap(), run);
            assert_eq!(db.execute_graph_pattern_governed(&query_cx, pattern, generous()).unwrap().value, *historical);
        }
        let published = txn.commit(&mut db, &cx).await.unwrap();
        for ((pattern, expected), historical) in patterns.iter().zip(&after).zip(&before) {
            assert_eq!(db.execute_graph_pattern_governed(&query_cx, pattern, generous()).unwrap().value, *expected);
            assert_eq!(db.execute_graph_pattern_governed_at(&query_cx, pattern, basis, generous()).unwrap().value, *historical);
            assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, pattern, generous()).unwrap().value, *historical);
        }
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(4));
        db.write(&cx, cascade).await.unwrap();
        let last: Vec<_> = cases.iter().map(|case| case.expected(&db.vertices().unwrap(), &db.edges().unwrap(), 3)).collect();
        db.compact(&cx).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        for (((pattern, current), previous), historical) in patterns.iter().zip(&last).zip(&after).zip(&before) {
            assert_eq!(reopened.execute_graph_pattern_governed(&query_cx, pattern, generous()).unwrap().value, *current);
            assert_eq!(reopened.execute_graph_pattern_governed_at(&query_cx, pattern, published, generous()).unwrap().value, *previous);
            assert_eq!(reopened.execute_graph_pattern_governed_at(&query_cx, pattern, basis, generous()).unwrap().value, *historical);
            assert_eq!(pinned.execute_graph_pattern_governed(&query_cx, pattern, generous()).unwrap().value, *historical);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_four_limits_remain_shared_across_source_and_long_pattern_execution() {
    let ((), report) = run_async_under_lab(0xc0a4_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let txn = db.begin(&txn_cx).unwrap();
        let pattern = cases()[1].prepare(3, 0, None);
        for overlay in [false, true] {
            // Normalize only the source error for this shared test harness.
            let run = |policy| {
                if overlay {
                    txn.execute_graph_pattern_governed(&db, &query_cx, &pattern, policy)
                        .map_err(|error| error.map_source(|error| error.to_string()))
                } else {
                    db.execute_graph_pattern_governed(&query_cx, &pattern, policy)
                        .map_err(|error| error.map_source(|error| error.to_string()))
                }
            };
            let measured = run(generous()).unwrap();
            assert_eq!(measured.value, vec![VId(4)]);
            let exact = GqlQueryPolicy::new(measured.rows.snapshot_records, 1,
                measured.evaluator.work_units, measured.evaluator.scratch_entries);
            assert_eq!(run(exact).unwrap(), measured);
            assert!(matches!(run(GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 1, 1_000_000, 1_000_000)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords));
            assert!(matches!(run(GqlQueryPolicy::new(measured.rows.snapshot_records, 0, 1_000_000, 1_000_000)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 1));
            for (work, scratch, dimension) in [
                (measured.evaluator.work_units - 1, measured.evaluator.scratch_entries, GlaLimitDimension::WorkUnits),
                (measured.evaluator.work_units, measured.evaluator.scratch_entries - 1, GlaLimitDimension::ScratchEntries),
            ] {
                let limit = if dimension == GlaLimitDimension::WorkUnits { work } else { scratch };
                assert!(matches!(run(GqlQueryPolicy::new(measured.rows.snapshot_records, 1, work, scratch)),
                    Err(GqlQueryError::Evaluator(error)) if error.dimension == dimension
                        && error.limit == limit && error.observed == u128::from(limit) + 1));
            }
        }
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn node_pattern_refusal_retains_label_scoped_phantoms_not_a_global_fence() {
    let ((), report) = run_async_under_lab(0xc0a4_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("a").unwrap().filter("a", VertexPredicate::HasLabel(L)).unwrap();
        let pattern = builder.prepare("a", 0, None).unwrap();
        for kind in 0..3 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            assert!(matches!(txn.execute_graph_pattern_governed(&db, &query_cx, &pattern,
                GqlQueryPolicy::new(0, 0, 1_000, 1_000)), Err(GqlQueryError::Rows(_))));
            let mut winner = WriteBatch::new(R);
            winner.create_vertex(VId(77), if kind == 1 { vec![L] } else { vec![] }, vec![]);
            db.write(&cx, winner).await.unwrap();
            if kind == 2 {
                let mut label = WriteBatch::new(R);
                label.set_vertex_label(VId(77), L, true);
                db.write(&cx, label).await.unwrap();
            }
            let result = txn.commit(&mut db, &cx).await;
            if kind == 0 {
                result.expect("an unrelated unlabeled insertion is not a label phantom");
                assert!(db.vertex(VId(99)).unwrap().is_some());
            } else {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
                assert!(db.vertex(VId(99)).unwrap().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_new_disconnected_cycle_conflicts_after_the_pattern_source_refuses() {
    let ((), report) = run_async_under_lab(0xc0a4_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut b = GraphPatternBuilder::new();
        for name in NAMES { b.vertex(name).unwrap(); }
        for i in 0..4 { b.edge(NAMES[i], R, GlaDirection::Forward, NAMES[(i + 1) % 4]).unwrap(); }
        let pattern = b.prepare("d", 0, Some(1)).unwrap();
        for add_cycle in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            // Source-start succeeds and records the edge-table witness, then
            // the next metadata event exhausts work before any graph result.
            assert!(matches!(txn.execute_graph_pattern_governed(&db, &query_cx, &pattern,
                GqlQueryPolicy::new(100, 1, 1, 1_000)), Err(GqlQueryError::Evaluator(_))));
            let mut winner = WriteBatch::new(R);
            for id in 1..=4 { winner.create_vertex(VId(id), vec![], vec![]); }
            if add_cycle {
                for id in 1..=4 { winner.add_edge(EId(id), VId(id), VId(id % 4 + 1), vec![]); }
            }
            db.write(&cx, winner).await.unwrap();
            let result = txn.commit(&mut db, &cx).await;
            if add_cycle {
                assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01", ..
                }))));
            } else {
                result.expect("an edge pattern is not a fence on unrelated vertex insertion");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_future_and_recovery_fences_precede_pattern_resource_refusals() {
    let ((), report) = run_async_under_lab(0xc0a4_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let query_cx = contexts.query();
        let txn_cx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx).await;
        let foreign = Database::open_memory(&cx, keys()).await.unwrap();
        let pattern = cases()[0].prepare(3, 0, Some(0));
        let pinned = db.read_session().unwrap();
        let zero = GqlQueryPolicy::new(0, 0, 0, 0);
        let future = CommitSeq(db.frontier().unwrap().0 + 1);
        assert!(matches!(db.execute_graph_pattern_governed_at(&query_cx, &pattern, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        assert!(matches!(pinned.execute_graph_pattern_governed_at(&query_cx, &pattern, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::BeyondFrontier { .. })))));
        let txn = db.begin(&txn_cx).unwrap();
        assert!(matches!(txn.execute_graph_pattern_governed(&foreign, &query_cx, &pattern, zero),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))));
        txn.abort();
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(77), vec![], vec![]);
        assert!(matches!(db.write_with_publication_failure(&cx, winner,
            DerivedPublicationStage::FoldCommittedTemplate).await, Err(WriteError::CommittedNeedsRecovery { .. })));
        assert!(matches!(db.execute_graph_pattern_governed(&query_cx, &pattern, zero),
            Err(GqlQueryError::Source(GqlError::Read(ReadError::RecoveryRequired(_))))));
        assert!(pinned.execute_graph_pattern_governed(&query_cx, &pattern, generous()).unwrap().value.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
