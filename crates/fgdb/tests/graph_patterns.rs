//! Typed connected patterns use real snapshot/overlay readers and one GLA.
//! The oracle enumerates vertex assignments over ordinary owned storage rows;
//! it never calls GLA lowering, its predicates, or the borrowed query sources.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DerivedPublicationStage, EdgeRecord, GqlError, MemVfs, ReadError,
    VertexRow, WriteBatch, WriteError, WriteTxnError,
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
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}

fn generous() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000)
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut first = WriteBatch::new(R);
    for (vid, n) in [
        (VId(1), 1),
        (VId(2), 2),
        (VId(3), 3),
        (VId(4), 4),
        (VId(5), 5),
        (HIGH, 9),
    ] {
        first.create_vertex(
            vid,
            if vid == VId(5) { vec![] } else { vec![L] },
            vec![(N, CanonicalScalar::Int(n))],
        );
    }
    for (eid, src, dst) in [
        (10, VId(1), VId(2)),
        (11, VId(1), VId(2)),
        (12, VId(2), VId(3)),
        (13, VId(3), VId(4)),
        (14, VId(4), HIGH),
    ] {
        first.add_edge(EId(eid), src, dst, vec![]);
    }
    db.write(cx, first).await.unwrap();
    for (relation, rows) in [
        (S, vec![(20, VId(4), VId(3)), (21, VId(2), VId(4))]),
        (T, vec![(30, VId(4), VId(1)), (31, VId(3), VId(4))]),
        (U, vec![(40, VId(1), VId(3))]),
    ] {
        let mut batch = WriteBatch::new(relation);
        for (eid, src, dst) in rows {
            batch.add_edge(EId(eid), src, dst, vec![]);
        }
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
        for name in NAMES {
            b.vertex(name).unwrap();
        }
        for &Atom(s, r, direction, d) in &self.atoms {
            b.edge(NAMES[s], r, direction, NAMES[d]).unwrap();
        }
        if self.filtered {
            b.filter("a", VertexPredicate::HasLabel(L)).unwrap();
            b.filter(
                "c",
                VertexPredicate::IntegerProperty {
                    key: N,
                    comparison: IntegerComparison::GreaterOrEqual,
                    value: 3,
                },
            )
            .unwrap();
        }
        if self.unequal {
            b.identity("a", "d", false).unwrap();
        }
        b.prepare(NAMES[projected], offset, count).unwrap()
    }

    /// Every edge that serves some atom of some complete binding: a bound
    /// source must admit at least these, and never more than the snapshot.
    fn witnesses(&self, vertices: &[VertexRow], edges: &[EdgeRecord]) -> BTreeSet<EId> {
        let mut used = BTreeSet::new();
        for a in vertices {
            for b in vertices {
                for c in vertices {
                    for d in vertices {
                        let bound = [a, b, c, d];
                        if self.filtered
                            && (!a.labels.contains(&L) || !c.props.iter().any(|(key, scalar)| {
                                *key == N
                                    && matches!(scalar, CanonicalScalar::Int(value) if *value >= 3)
                            }))
                        {
                            continue;
                        }
                        if self.unequal && a.vid == d.vid {
                            continue;
                        }
                        let serving = |&Atom(s, r, direction, d): &Atom| {
                            edges
                                .iter()
                                .filter(move |row| {
                                    let edge = row.entry;
                                    let (left, right) = (bound[s].vid, bound[d].vid);
                                    edge.relation == r
                                        && match direction {
                                            GlaDirection::Forward => {
                                                edge.src == left && edge.dst == right
                                            }
                                            GlaDirection::Reverse => {
                                                edge.dst == left && edge.src == right
                                            }
                                            GlaDirection::Undirected => {
                                                (edge.src == left && edge.dst == right)
                                                    || (edge.dst == left && edge.src == right)
                                            }
                                        }
                                })
                                .map(|row| row.entry.eid)
                                .collect::<Vec<_>>()
                        };
                        let per_atom: Vec<_> = self.atoms.iter().map(serving).collect();
                        if per_atom.iter().all(|eids| !eids.is_empty()) {
                            used.extend(per_atom.into_iter().flatten());
                        }
                    }
                }
            }
        }
        used
    }
    fn expected(&self, vertices: &[VertexRow], edges: &[EdgeRecord], projected: usize) -> Vec<VId> {
        let mut out = BTreeSet::new();
        for a in vertices {
            for b in vertices {
                for c in vertices {
                    for d in vertices {
                        let bound = [a, b, c, d];
                        if self.filtered
                            && (!a.labels.contains(&L) || !c.props.iter().any(|(key, scalar)| {
                                *key == N
                                    && matches!(scalar, CanonicalScalar::Int(value) if *value >= 3)
                            }))
                        {
                            continue;
                        }
                        if self.unequal && a.vid == d.vid {
                            continue;
                        }
                        let complete = self.atoms.iter().all(|&Atom(s, r, direction, d)| {
                            edges.iter().any(|row| {
                                let edge = row.entry;
                                if edge.relation != r {
                                    return false;
                                }
                                let left = bound[s].vid;
                                let right = bound[d].vid;
                                match direction {
                                    GlaDirection::Forward => edge.src == left && edge.dst == right,
                                    GlaDirection::Reverse => edge.dst == left && edge.src == right,
                                    GlaDirection::Undirected => {
                                        (edge.src == left && edge.dst == right)
                                            || (edge.dst == left && edge.src == right)
                                    }
                                }
                            })
                        });
                        if complete {
                            out.insert(bound[projected].vid);
                        }
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
        Case {
            atoms: vec![Atom(0, R, F, 1), Atom(1, R, F, 2), Atom(2, R, F, 3)],
            filtered: false,
            unequal: false,
        },
        Case {
            atoms: vec![
                Atom(0, R, F, 1),
                Atom(2, S, Rv, 3),
                Atom(2, R, Rv, 1),
                Atom(3, T, F, 0),
                Atom(0, U, F, 2),
            ],
            filtered: true,
            unequal: true,
        },
        Case {
            atoms: vec![
                Atom(0, R, F, 1),
                Atom(1, S, F, 3),
                Atom(0, U, F, 2),
                Atom(2, T, F, 3),
            ],
            filtered: false,
            unequal: true,
        },
        Case {
            atoms: vec![
                Atom(0, R, Both, 1),
                Atom(2, R, Both, 3),
                Atom(1, R, Both, 2),
                Atom(3, T, Both, 0),
            ],
            filtered: true,
            unequal: false,
        },
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
                let live = db
                    .execute_graph_pattern_governed(&query_cx, &pattern, generous())
                    .unwrap();
                assert_eq!(live.value, expected);
                // An unbound root scans every edge. A label-bound root admits
                // its per-step closure (and the endpoint rows it reads): every
                // witness edge, at most the whole snapshot.
                if case.filtered {
                    let witnesses = case.witnesses(&vertices, &edges).len() as u64;
                    let snapshot = (vertices.len() + edges.len()) as u64;
                    assert!(
                        (witnesses..=snapshot).contains(&live.rows.snapshot_records),
                        "{witnesses} <= {} <= {snapshot}",
                        live.rows.snapshot_records,
                    );
                } else {
                    assert_eq!(live.rows.snapshot_records, edges.len() as u64);
                }
                assert_eq!(live.rows.result_rows, expected.len() as u64);
                assert_eq!(
                    db.execute_graph_pattern_governed_at(&query_cx, &pattern, basis, generous())
                        .unwrap(),
                    live
                );
                assert_eq!(
                    pinned
                        .execute_graph_pattern_governed(&query_cx, &pattern, generous())
                        .unwrap(),
                    live
                );
                let overlay = txn
                    .execute_graph_pattern_governed(&db, &query_cx, &pattern, generous())
                    .unwrap();
                assert_eq!(overlay.value, expected);
                // The transaction overlay keeps its conservative edge scan; only
                // the owner source narrows a bound root to its closure.
                if case.filtered {
                    assert_eq!(overlay.rows.snapshot_records, edges.len() as u64);
                    assert_eq!(overlay.rows.result_rows, live.rows.result_rows);
                    assert!(live.rows.snapshot_records <= overlay.rows.snapshot_records);
                } else {
                    assert_eq!(overlay.rows, live.rows);
                }
                let paged = case.prepare(projection, 1, Some(1));
                assert_eq!(
                    db.execute_graph_pattern_governed(&query_cx, &paged, generous())
                        .unwrap()
                        .value,
                    expected.into_iter().skip(1).take(1).collect::<Vec<_>>()
                );
            }
        }
        let long = cases()[0].prepare(3, 0, None);
        assert_eq!(
            db.execute_graph_pattern_governed(&query_cx, &long, generous())
                .unwrap()
                .value,
            vec![VId(4), HIGH]
        );
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
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &cx).await;
        let basis = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let cases = cases();
        let patterns: Vec<_> = cases.iter().map(|case| case.prepare(3, 0, None)).collect();
        let before: Vec<_> = cases
            .iter()
            .map(|case| case.expected(&db.vertices().unwrap(), &db.edges().unwrap(), 3))
            .collect();
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
        let after: Vec<_> = cases
            .iter()
            .map(|case| case.expected(&vertices, &edges, 3))
            .collect();
        assert_ne!(before, after);
        for ((pattern, expected), historical) in patterns.iter().zip(&after).zip(&before) {
            let run = txn
                .execute_graph_pattern_governed(&db, &query_cx, pattern, generous())
                .unwrap();
            assert_eq!(run.value, *expected);
            assert_eq!(run.rows.snapshot_records, edges.len() as u64);
            assert_eq!(
                txn.execute_graph_pattern_governed(&db, &query_cx, pattern, generous())
                    .unwrap(),
                run
            );
            assert_eq!(
                db.execute_graph_pattern_governed(&query_cx, pattern, generous())
                    .unwrap()
                    .value,
                *historical
            );
        }
        let published = txn.commit(&mut db, &cx).await.unwrap();
        for ((pattern, expected), historical) in patterns.iter().zip(&after).zip(&before) {
            assert_eq!(
                db.execute_graph_pattern_governed(&query_cx, pattern, generous())
                    .unwrap()
                    .value,
                *expected
            );
            assert_eq!(
                db.execute_graph_pattern_governed_at(&query_cx, pattern, basis, generous())
                    .unwrap()
                    .value,
                *historical
            );
            assert_eq!(
                pinned
                    .execute_graph_pattern_governed(&query_cx, pattern, generous())
                    .unwrap()
                    .value,
                *historical
            );
        }
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(4));
        db.write(&cx, cascade).await.unwrap();
        let last: Vec<_> = cases
            .iter()
            .map(|case| case.expected(&db.vertices().unwrap(), &db.edges().unwrap(), 3))
            .collect();
        db.compact(&cx).await.unwrap();
        drop(db);
        let reopened = Database::open_with_vfs(&cx, vfs, &path, keys())
            .await
            .unwrap();
        for (((pattern, current), previous), historical) in
            patterns.iter().zip(&last).zip(&after).zip(&before)
        {
            assert_eq!(
                reopened
                    .execute_graph_pattern_governed(&query_cx, pattern, generous())
                    .unwrap()
                    .value,
                *current
            );
            assert_eq!(
                reopened
                    .execute_graph_pattern_governed_at(&query_cx, pattern, published, generous())
                    .unwrap()
                    .value,
                *previous
            );
            assert_eq!(
                reopened
                    .execute_graph_pattern_governed_at(&query_cx, pattern, basis, generous())
                    .unwrap()
                    .value,
                *historical
            );
            assert_eq!(
                pinned
                    .execute_graph_pattern_governed(&query_cx, pattern, generous())
                    .unwrap()
                    .value,
                *historical
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn indexed_overlay_vertex_pair(
    left: Option<i64>,
    right: Option<i64>,
) -> PreparedGraphPattern<fgdb_gql::algebra::GraphBindingRow> {
    let mut builder = GraphPatternBuilder::new();
    for (name, value) in [("a", left), ("b", right)] {
        builder
            .vertex(name)
            .unwrap()
            .filter(name, VertexPredicate::HasLabel(L))
            .unwrap();
        if let Some(value) = value {
            builder
                .filter(
                    name,
                    VertexPredicate::IntegerProperty {
                        key: N,
                        comparison: IntegerComparison::Equal,
                        value,
                    },
                )
                .unwrap();
        }
    }
    builder.prepare_bindings(&["a", "b"], 0, None).unwrap()
}

#[test]
fn independent_indexed_roots_admit_the_union_and_preserve_staged_products() {
    let ((), report) = run_async_under_lab(0xc0a4_0014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed_indexed_overlay_chain(&mut db, &commit).await;
        let bounded = GqlQueryPolicy::new(16, 16, 1_000_000, 1_000_000);
        let pair = indexed_overlay_vertex_pair(Some(100), Some(200));
        let mut txn = db.begin(&txcx).unwrap();
        let initial = txn
            .execute_graph_pattern_governed(&db, &cx, &pair, bounded)
            .unwrap();
        assert_eq!(initial.rows.snapshot_records, 2);
        assert_eq!(initial.value.len(), 1);
        assert_eq!(initial.value[0].values(), &[VId(100), VId(200)]);
        txn.savepoint(&db, "before-root-edits").unwrap();

        let new_left = VId(u128::MAX - 1);
        let new_right = VId(u128::MAX);
        let mut edits = WriteBatch::new(R);
        edits.set_vertex_property(VId(100), N, Some(CanonicalScalar::Int(900)));
        edits.set_vertex_property(VId(200), N, Some(CanonicalScalar::Int(900)));
        edits.set_vertex_property(VId(400), N, Some(CanonicalScalar::Int(100)));
        edits.set_vertex_property(VId(500), N, Some(CanonicalScalar::Int(200)));
        edits.set_vertex_label(VId(550), L, true);
        edits.create_vertex(new_left, vec![L], vec![(N, CanonicalScalar::Int(100))]);
        edits.create_vertex(new_right, vec![L], vec![(N, CanonicalScalar::Int(200))]);
        txn.write(&mut db, edits).unwrap();
        for delete_right in [false, true] {
            if delete_right {
                let mut deletion = WriteBatch::new(R);
                deletion.delete_vertex(VId(500));
                txn.write(&mut db, deletion).unwrap();
            }
            let result = txn
                .execute_graph_pattern_governed(&db, &cx, &pair, bounded)
                .unwrap();
            let actual = result
                .value
                .iter()
                .map(|row| (row.values()[0], row.values()[1]))
                .collect::<BTreeSet<_>>();
            let mut expected = BTreeSet::new();
            for left in [VId(400), VId(550), new_left] {
                for right in [VId(500), new_right] {
                    if !delete_right || right != VId(500) {
                        expected.insert((left, right));
                    }
                }
            }
            assert_eq!(actual, expected);
            assert_eq!(result.value.len(), expected.len());
            assert!(result.rows.snapshot_records <= 7);
        }
        txn.rollback_to_savepoint(&db, "before-root-edits").unwrap();
        let restored = txn
            .execute_graph_pattern_governed(&db, &cx, &pair, bounded)
            .unwrap();
        assert_eq!(restored.value, initial.value);
        assert_eq!(restored.rows.snapshot_records, 2);
        assert!(matches!(
            txn.execute_graph_pattern_governed(
                &db,
                &cx,
                &indexed_overlay_vertex_pair(Some(100), None),
                bounded,
            ),
            Err(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::SnapshotRecords
        ));
        let mut single = GraphPatternBuilder::new();
        single
            .vertex("a")
            .unwrap()
            .filter("a", VertexPredicate::HasLabel(L))
            .unwrap()
            .filter(
                "a",
                VertexPredicate::IntegerProperty {
                    key: N,
                    comparison: IntegerComparison::Equal,
                    value: 100,
                },
            )
            .unwrap();
        let single = single.prepare("a", 0, None).unwrap();
        let result = txn
            .execute_graph_pattern_governed(
                &db,
                &cx,
                &single,
                GqlQueryPolicy::new(1, 1, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert_eq!(result.value, vec![VId(100)]);
        assert_eq!(result.rows.snapshot_records, 1);
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_indexed_root_keeps_empty_and_refused_membership_observations() {
    let ((), report) = run_async_under_lab(0xc0a4_0015, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for (left, right, refuse, entrant) in [
            (100, 200, false, 100),
            (100, 200, false, 200),
            (9_999, 200, false, 9_999),
            (100, 9_999, false, 9_999),
            (100, 200, true, 700),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed_indexed_overlay_chain(&mut db, &commit).await;
            let pair = indexed_overlay_vertex_pair(Some(left), Some(right));
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "before-query").unwrap();
            let result = txn.execute_graph_pattern_governed(
                &db,
                &cx,
                &pair,
                GqlQueryPolicy::new(16, if refuse { 0 } else { 16 }, 1_000_000, 1_000_000),
            );
            if refuse {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Rows(error))
                        if error.dimension == GqlBudgetDimension::ResultRows
                ));
                // This success cannot complete the failed call's domains.
                // The later value 700 matches neither completed predicate.
                txn.execute_graph_pattern_governed(&db, &cx, &pair, generous())
                    .unwrap();
            } else {
                assert_eq!(
                    result.unwrap().value.len(),
                    usize::from(left == 100 && right == 200)
                );
            }
            txn.rollback_to_savepoint(&db, "before-query").unwrap();
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(VId(500), N, Some(CanonicalScalar::Int(entrant)));
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(
                txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn completed_indexed_roots_ignore_unrelated_changes_and_keep_prior_broad_reads() {
    let ((), report) = run_async_under_lab(0xc0a4_0016, |root| async move {
        use fgdb_types::EmbeddedTxnCompletion;

        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for (change, prior_broad_read) in [(0, false), (1, false), (2, false), (2, true)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed_indexed_overlay_chain(&mut db, &commit).await;
            if change == 1 {
                let mut setup = WriteBatch::new(R);
                setup.set_vertex_label(VId(500), L, false);
                db.write(&commit, setup).await.unwrap();
            }
            let pair = indexed_overlay_vertex_pair(Some(100), Some(200));
            let mut txn = db.begin(&txcx).unwrap();
            if prior_broad_read {
                // A later indexed query must never erase an earlier full scan.
                assert_eq!(txn.vertices(&db).unwrap().len(), 600);
            }
            let result = txn
                .execute_graph_pattern_governed(
                    &db,
                    &cx,
                    &pair,
                    GqlQueryPolicy::new(16, 16, 1_000_000, 1_000_000),
                )
                .unwrap();
            assert_eq!(result.value.len(), 1);
            assert_eq!(result.rows.snapshot_records, 2);

            let mut winner = WriteBatch::new(R);
            match change {
                0 => {
                    winner.set_vertex_property(VId(500), N, Some(CanonicalScalar::Int(700)));
                }
                1 => {
                    winner.set_vertex_label(VId(500), L, true);
                }
                _ => {
                    winner.create_vertex(VId(700), vec![L], vec![(N, CanonicalScalar::Int(700))]);
                }
            }
            db.write(&commit, winner).await.unwrap();
            let completion = txn.finish(&mut db, &commit).await;
            if prior_broad_read {
                assert!(matches!(
                    completion,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
            } else {
                assert!(
                    matches!(completion, Ok(EmbeddedTxnCompletion::ReadClosed { .. })),
                    "unrelated change {change}: {completion:?}",
                );
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
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
            let exact = GqlQueryPolicy::new(
                measured.rows.snapshot_records,
                1,
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries,
            );
            assert_eq!(run(exact).unwrap(), measured);
            assert!(
                matches!(run(GqlQueryPolicy::new(measured.rows.snapshot_records - 1, 1, 1_000_000, 1_000_000)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords)
            );
            assert!(
                matches!(run(GqlQueryPolicy::new(measured.rows.snapshot_records, 0, 1_000_000, 1_000_000)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 1)
            );
            for (work, scratch, dimension) in [
                (
                    measured.evaluator.work_units - 1,
                    measured.evaluator.scratch_entries,
                    GlaLimitDimension::WorkUnits,
                ),
                (
                    measured.evaluator.work_units,
                    measured.evaluator.scratch_entries - 1,
                    GlaLimitDimension::ScratchEntries,
                ),
            ] {
                let limit = if dimension == GlaLimitDimension::WorkUnits {
                    work
                } else {
                    scratch
                };
                assert!(
                    matches!(run(GqlQueryPolicy::new(measured.rows.snapshot_records, 1, work, scratch)),
                    Err(GqlQueryError::Evaluator(error)) if error.dimension == dimension
                        && error.limit == limit && error.observed == u128::from(limit) + 1)
                );
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
        builder
            .vertex("a")
            .unwrap()
            .filter("a", VertexPredicate::HasLabel(L))
            .unwrap();
        let pattern = builder.prepare("a", 0, None).unwrap();
        for kind in 0..3 {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            assert!(matches!(
                txn.execute_graph_pattern_governed(
                    &db,
                    &query_cx,
                    &pattern,
                    GqlQueryPolicy::new(0, 0, 1_000, 1_000)
                ),
                Err(GqlQueryError::Rows(_))
            ));
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
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
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
        for name in NAMES {
            b.vertex(name).unwrap();
        }
        for i in 0..4 {
            b.edge(NAMES[i], R, GlaDirection::Forward, NAMES[(i + 1) % 4])
                .unwrap();
        }
        let pattern = b.prepare("d", 0, Some(1)).unwrap();
        for add_cycle in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut stage = WriteBatch::new(R);
            stage.create_vertex(VId(99), vec![], vec![]);
            txn.write(&mut db, stage).unwrap();
            // Source-start succeeds and records the edge-table witness, then
            // the next metadata event exhausts work before any graph result.
            assert!(matches!(
                txn.execute_graph_pattern_governed(
                    &db,
                    &query_cx,
                    &pattern,
                    GqlQueryPolicy::new(100, 1, 1, 1_000)
                ),
                Err(GqlQueryError::Evaluator(_))
            ));
            let mut winner = WriteBatch::new(R);
            for id in 1..=4 {
                winner.create_vertex(VId(id), vec![], vec![]);
            }
            if add_cycle {
                for id in 1..=4 {
                    winner.add_edge(EId(id), VId(id), VId(id % 4 + 1), vec![]);
                }
            }
            db.write(&cx, winner).await.unwrap();
            let result = txn.commit(&mut db, &cx).await;
            if add_cycle {
                assert!(matches!(
                    result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01",
                        ..
                    }))
                ));
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
        assert!(matches!(
            db.execute_graph_pattern_governed_at(&query_cx, &pattern, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        assert!(matches!(
            pinned.execute_graph_pattern_governed_at(&query_cx, &pattern, future, zero),
            Err(GqlQueryError::Source(GqlError::Read(
                ReadError::BeyondFrontier { .. }
            )))
        ));
        let txn = db.begin(&txn_cx).unwrap();
        assert!(matches!(
            txn.execute_graph_pattern_governed(&foreign, &query_cx, &pattern, zero),
            Err(GqlQueryError::Source(WriteTxnError::WrongDatabase))
        ));
        txn.abort();
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(77), vec![], vec![]);
        assert!(matches!(
            db.write_with_publication_failure(
                &cx,
                winner,
                DerivedPublicationStage::FoldCommittedTemplate
            )
            .await,
            Err(WriteError::CommittedNeedsRecovery { .. })
        ));
        assert!(matches!(
            db.execute_graph_pattern_governed(&query_cx, &pattern, zero),
            Err(GqlQueryError::Source(GqlError::Read(
                ReadError::RecoveryRequired(_)
            )))
        ));
        assert!(
            pinned
                .execute_graph_pattern_governed(&query_cx, &pattern, generous())
                .unwrap()
                .value
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

async fn seed_indexed_overlay_chain(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=600_u128 {
        let value = if id == 550 {
            100
        } else {
            i64::try_from(id).unwrap()
        };
        batch.create_vertex(
            VId(id),
            if id == 550 { vec![] } else { vec![L] },
            vec![(N, CanonicalScalar::Int(value))],
        );
        if id > 1 {
            batch.add_edge(EId(id), VId(id - 1), VId(id), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
}

fn indexed_overlay_pattern(value: Option<i64>, direction: GlaDirection) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    for name in ["a", "b", "c"] {
        builder.vertex(name).unwrap();
    }
    builder.edge("a", R, direction, "b").unwrap();
    builder.edge("b", R, direction, "c").unwrap();
    builder.filter("a", VertexPredicate::HasLabel(L)).unwrap();
    if let Some(value) = value {
        builder
            .filter(
                "a",
                VertexPredicate::IntegerProperty {
                    key: N,
                    comparison: IntegerComparison::Equal,
                    value,
                },
            )
            .unwrap();
    }
    builder.prepare("c", 0, None).unwrap()
}

#[test]
fn indexed_overlay_expansions_admit_only_reached_rows_in_every_direction() {
    let ((), report) = run_async_under_lab(0xc0a4_0010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed_indexed_overlay_chain(&mut db, &commit).await;
        let txn = db.begin(&txcx).unwrap();
        let bounded = GqlQueryPolicy::new(16, 16, 1_000_000, 1_000_000);
        // Typed patterns default to REPEATABLE ELEMENTS: an undirected
        // two-hop path can reuse its first edge and return to vertex 100.
        for (direction, expected) in [
            (GlaDirection::Forward, vec![VId(102)]),
            (GlaDirection::Reverse, vec![VId(98)]),
            (GlaDirection::Undirected, vec![VId(98), VId(100), VId(102)]),
        ] {
            let pattern = indexed_overlay_pattern(Some(100), direction);
            let result = txn
                .execute_graph_pattern_governed(&db, &cx, &pattern, bounded)
                .unwrap();
            assert_eq!(result.value, expected, "{direction:?}");
            assert!(result.rows.snapshot_records <= 16, "{result:?}");
            assert_eq!(
                db.execute_graph_pattern_governed(&cx, &pattern, bounded)
                    .unwrap()
                    .value,
                expected,
            );
        }
        let full = indexed_overlay_pattern(None, GlaDirection::Forward);
        assert!(matches!(
            txn.execute_graph_pattern_governed(&db, &cx, &full, bounded),
            Err(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::SnapshotRecords
        ));
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_overlay_closure_includes_staged_roots_edges_and_cascades_before_rollback() {
    let ((), report) = run_async_under_lab(0xc0a4_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed_indexed_overlay_chain(&mut db, &commit).await;
        let pattern = indexed_overlay_pattern(Some(100), GlaDirection::Forward);
        let bounded = GqlQueryPolicy::new(32, 16, 1_000_000, 1_000_000);
        let new_root = VId(u128::MAX - 1);
        let new_leaf = VId(u128::MAX);
        let mut txn = db.begin(&txcx).unwrap();
        txn.savepoint(&db, "before").unwrap();
        let mut stage = WriteBatch::new(R);
        stage.set_vertex_property(VId(100), N, Some(CanonicalScalar::Int(900)));
        stage.set_vertex_property(VId(400), N, Some(CanonicalScalar::Int(100)));
        // Vertex 550 already has the numeric key; gaining this label alone
        // brings its existing two-hop path into the indexed root domain.
        stage.set_vertex_label(VId(550), L, true);
        stage.create_vertex(new_root, vec![L], vec![(N, CanonicalScalar::Int(100))]);
        stage.create_vertex(new_leaf, vec![L], vec![(N, CanonicalScalar::Int(777))]);
        stage.delete_edge(EId(102));
        stage.add_edge(EId(u128::MAX - 1), new_root, VId(101), vec![]);
        stage.add_edge(EId(u128::MAX), VId(101), new_leaf, vec![]);
        txn.write(&mut db, stage).unwrap();
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, bounded)
                .unwrap()
                .value,
            vec![VId(402), VId(552), new_leaf],
        );
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(101));
        txn.write(&mut db, cascade).unwrap();
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, bounded)
                .unwrap()
                .value,
            vec![VId(402), VId(552)],
        );
        txn.rollback_to_savepoint(&db, "before").unwrap();
        assert_eq!(
            txn.execute_graph_pattern_governed(&db, &cx, &pattern, bounded)
                .unwrap()
                .value,
            vec![VId(102)],
        );
        assert!(db.vertex(new_root).unwrap().is_none());
        assert!(db.vertex(new_leaf).unwrap().is_none());
        txn.abort();
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_overlay_root_membership_survives_empty_results_and_late_refusal() {
    let ((), report) = run_async_under_lab(0xc0a4_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for (value, refuse) in [(100, false), (9_999, false), (100, true)] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed_indexed_overlay_chain(&mut db, &commit).await;
            let pattern = indexed_overlay_pattern(Some(value), GlaDirection::Forward);
            let mut txn = db.begin(&txcx).unwrap();
            let bounded =
                GqlQueryPolicy::new(16, if refuse { 0 } else { 16 }, 1_000_000, 1_000_000);
            let result = txn.execute_graph_pattern_governed(&db, &cx, &pattern, bounded);
            if refuse {
                assert!(matches!(
                    result,
                    Err(GqlQueryError::Rows(error))
                        if error.dimension == GqlBudgetDimension::ResultRows
                ));
                // A later success cannot complete the earlier refused read.
                txn.execute_graph_pattern_governed(&db, &cx, &pattern, generous())
                    .unwrap();
            } else {
                assert_eq!(
                    result.unwrap().value,
                    if value == 100 { vec![VId(102)] } else { vec![] },
                );
            }
            // No edge changes, and vertex 500 was outside the admitted closure.
            // Its existing two-hop path enters the query through this field.
            let mut winner = WriteBatch::new(R);
            winner.set_vertex_property(VId(500), N, Some(CanonicalScalar::Int(value)));
            db.write(&commit, winner).await.unwrap();
            assert!(matches!(
                txn.finish(&mut db, &commit).await,
                Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                    law: "FG-LAW-FCW-READ-01",
                    ..
                }))
            ));
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_overlay_native_match_write_commits_only_the_bounded_target() {
    let ((), report) = run_async_under_lab(0xc0a4_0013, |root| async move {
        use fgdb::QueryResult;
        use fgdb_delta_types::ElementId;
        use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy};
        use fgdb_types::EmbeddedTxnCompletion;
        use std::convert::Infallible;

        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        for (statement, target) in [
            (
                "MATCH (a:Node {n:$start})-[:R]->(b)-[:R]->(c) SET c.n=c.n+1",
                VId(102),
            ),
            (
                "MATCH (a:Node)-[:R]->(b:Node {n:$start})-[:R]->(c) SET c.n=c.n+1",
                VId(101),
            ),
            (
                "MATCH (a:Node)<-[:R]-(b:Node {n:$start})<-[:R]-(c) SET c.n=c.n+1",
                VId(99),
            ),
        ] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed_indexed_overlay_chain(&mut db, &commit).await;
            let basis = db.frontier().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let params = GqlParameters::new().with_int64("start", 100).unwrap();
            let policy = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(16, 16, 1_000_000, 1_000_000),
                1,
                0,
                0,
            );
            let result = txn
                .query_write(
                    &mut db,
                    &cx,
                    statement,
                    &params,
                    |kind, name| match (kind, name) {
                        (GraphSymbolKind::Label, "Node") => Some(GraphSymbol::Label(L)),
                        (GraphSymbolKind::Property, "n") => Some(GraphSymbol::Property(N)),
                        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
                        _ => None,
                    },
                    R,
                    policy,
                    |_| -> Result<ElementId, Infallible> {
                        panic!("MATCH SET must not allocate graph identities")
                    },
                )
                .unwrap();
            let QueryResult::Write {
                receipt,
                completion,
            } = result
            else {
                panic!("MATCH SET must return its ordinary write receipt")
            };
            assert!(completion.is_none());
            assert_eq!(receipt.stats().completed_statements, 1);
            let old = i64::try_from(target.0).unwrap();
            assert_eq!(
                txn.vertex(&db, target).unwrap().unwrap().props,
                vec![(N, CanonicalScalar::Int(old + 1))],
            );
            assert_eq!(
                db.vertex(target).unwrap().unwrap().props,
                vec![(N, CanonicalScalar::Int(old))],
            );
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(matches!(
                txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::WriteCommitted { commit_seq }
                    if commit_seq == CommitSeq(basis.0 + 1)
            ));
            for row in db.vertices().unwrap() {
                let expected = if row.vid == target {
                    old + 1
                } else if row.vid == VId(550) {
                    100
                } else {
                    i64::try_from(row.vid.0).unwrap()
                };
                assert_eq!(row.props, vec![(N, CanonicalScalar::Int(expected))]);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
