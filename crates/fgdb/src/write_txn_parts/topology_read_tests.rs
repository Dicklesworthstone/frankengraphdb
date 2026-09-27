use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{DeltaRow, PropertyKeyId};
use fgdb_types::{DatabaseSecurityNamespaceId, ObjectId, PurposeContexts};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, edges: bool) {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(0))],
        );
    }
    if edges {
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(2), vec![]);
        batch.add_edge(EId(12), VId(3), VId(1), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if edges {
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(20), VId(1), VId(4), vec![]);
        other.add_edge(EId(21), VId(4), VId(1), vec![]);
        db.write(cx, other).await.unwrap();
    }
}

fn neighbours(txn: &WriteTxn, db: &Database<MemVfs>, incoming: bool) -> Vec<VId> {
    if incoming {
        txn.in_neighbours(db, VId(1), R).unwrap()
    } else {
        txn.neighbours(db, VId(1), R).unwrap()
    }
}

fn assert_read_conflict(result: Result<EmbeddedTxnCompletion, WriteTxnError>) {
    assert!(matches!(
        &result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01",
            ..
        }))
    ), "{result:?}");
}

#[test]
fn both_directions_ignore_unobserved_payloads_and_other_topology_domains() {
    let ((), report) = run_async_under_lab(0xad7a_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for incoming in [false, true] {
            for case in 0..9 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx, true).await;
                let mut txn = db.begin(&txcx).unwrap();
                let before = neighbours(&txn, &db, incoming);
                let observed = if incoming { EId(12) } else { EId(10) };
                let opposite = if incoming { EId(10) } else { EId(12) };
                let mut change = WriteBatch::new(if case == 4 { S } else { R });
                match case {
                    0 => { change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7))); }
                    1 => { change.set_edge_property(observed, P, Some(CanonicalScalar::Int(7))); }
                    2 => { change.set_vertex_property(before[0], P, Some(CanonicalScalar::Int(7))); }
                    3 => { change.set_vertex_label(VId(1), LabelId(9), true); }
                    4 => { change.add_edge(EId(50), VId(1), VId(1), vec![]); }
                    5 => {
                        let (src, dst) = if incoming { (1, 4) } else { (4, 1) };
                        change.add_edge(EId(50), VId(src), VId(dst), vec![]);
                    }
                    6 => { change.add_edge(EId(50), VId(2), VId(4), vec![]); }
                    7 => { change.delete_edge(if incoming { EId(21) } else { EId(20) }); }
                    _ => { change.delete_edge(opposite); }
                }
                db.write(&cx, change).await.unwrap();
                assert_eq!(neighbours(&txn, &db, incoming), before);
                assert!(txn.read_set.borrow().is_empty());
                assert!(txn.match_expansions.borrow().is_empty());
                assert!(!txn.scanned_edges.get());
                let frontier = db.frontier().unwrap();
                assert!(matches!(
                    txn.finish(&mut db, &cx).await.unwrap(),
                    EmbeddedTxnCompletion::ReadClosed { validated_through, .. }
                        if validated_through == frontier
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(txn.point_reads.borrow().is_empty());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_insertions_retirements_cascades_and_self_loops_still_conflict() {
    let ((), report) = run_async_under_lab(0xad7a_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for incoming in [false, true] {
            for case in 0..5 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx, true).await;
                let mut txn = db.begin(&txcx).unwrap();
                let before = neighbours(&txn, &db, incoming);
                let mut change = WriteBatch::new(R);
                match case {
                    0 => {
                        let (src, dst) = if incoming { (4, 1) } else { (1, 4) };
                        change.add_edge(EId(50), VId(src), VId(dst), vec![]);
                    }
                    1 => { change.delete_edge(if incoming { EId(12) } else { EId(10) }); }
                    2 => { change.delete_vertex(before[0]); }
                    3 => { change.delete_vertex(VId(1)); }
                    _ => { change.add_edge(EId(50), VId(1), VId(1), vec![]); }
                }
                db.write(&cx, change).await.unwrap();
                // Historical answers remain pinned even after retirement.
                assert_eq!(neighbours(&txn, &db, incoming), before);
                let frontier = db.frontier().unwrap();
                assert_read_conflict(txn.finish(&mut db, &cx).await);
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
                assert!(txn.point_reads.borrow().is_empty());
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_gaps_and_all_parallel_edges_survive_rollback_and_value_restoration() {
    let ((), report) = run_async_under_lab(0xad7a_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for incoming in [false, true] {
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx, false).await;
            let mut txn = db.begin(&txcx).unwrap();
            txn.savepoint(&db, "before-read").unwrap();
            assert!(neighbours(&txn, &db, incoming).is_empty());
            txn.rollback_to_savepoint(&db, "before-read").unwrap();
            txn.release_savepoint(&db, "before-read").unwrap();
            let (src, dst) = if incoming { (2, 1) } else { (1, 2) };
            let mut insert = WriteBatch::new(R);
            insert.add_edge(EId(50), VId(src), VId(dst), vec![]);
            db.write(&cx, insert).await.unwrap();
            let mut remove = WriteBatch::new(R);
            remove.delete_edge(EId(50));
            db.write(&cx, remove).await.unwrap();
            assert!(db.edges().unwrap().is_empty());
            // Checking just today's adjacency would incorrectly accept this ABA.
            assert_read_conflict(txn.finish(&mut db, &cx).await);
        }
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx, true).await;
        let mut txn = db.begin(&txcx).unwrap();
        txn.savepoint(&db, "prefix").unwrap();
        let mut hide = WriteBatch::new(R);
        hide.delete_edge(EId(10));
        hide.delete_edge(EId(11));
        txn.write(&mut db, hide).unwrap();
        assert!(txn.neighbours(&db, VId(1), R).unwrap().is_empty());
        txn.rollback_to_savepoint(&db, "prefix").unwrap();
        txn.release_savepoint(&db, "prefix").unwrap();
        for eid in [EId(10), EId(11)] {
            assert!(txn.point_reads.borrow().contains(
                ElementId::Edge(eid), PointReadField::EdgeTopology,
            ));
        }
        let mut remove = WriteBatch::new(R);
        remove.delete_edge(EId(10));
        db.write(&cx, remove).await.unwrap();
        assert_eq!(db.neighbours(VId(1), R).unwrap(), vec![VId(2)]);
        assert_read_conflict(txn.finish(&mut db, &cx).await);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn topology_derived_field_rebase_matches_serial_execution_and_reopens() {
    let ((), report) = run_async_under_lab(0xad7a_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&cx, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &cx, true).await;
        let mut serial = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut serial, &cx, true).await;
        let mut txn = db.begin(&txcx).unwrap();
        let neighbours = txn.neighbours(&db, VId(1), R).unwrap();
        assert_eq!(neighbours, vec![VId(2)]);
        let mut write = WriteBatch::new(R);
        write.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(neighbours.len() as i64)));
        txn.write(&mut db, write.clone()).unwrap();
        let mut other = WriteBatch::new(S);
        other.set_vertex_label(VId(1), LabelId(9), true);
        other.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(44)));
        other.add_edge(EId(50), VId(1), VId(3), vec![]);
        db.write(&cx, other.clone()).await.unwrap();
        serial.write(&cx, other).await.unwrap();
        let frontier = db.frontier().unwrap();
        // Mutation independence is still an explicit choice, not a relaxed
        // ordinary write conflict rule. The original R read must survive it.
        let seq = txn.commit_disjoint_fields_rebased(&mut db, &cx, 1).await.unwrap();
        assert_eq!(seq, CommitSeq(frontier.0 + 1));
        assert_eq!(db.delta_since(frontier).unwrap().count(), 1);
        serial.write(&cx, write).await.unwrap();
        let expected_vertices = serial.vertices().unwrap();
        let expected_edges = serial.edges().unwrap();
        assert_eq!(db.vertices().unwrap(), expected_vertices);
        assert_eq!(db.edges().unwrap(), expected_edges);
        assert!(txn.point_reads.borrow().is_empty());
        drop(db);
        let db = Database::open_with_vfs(&cx, vfs, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), seq);
        assert_eq!(db.vertices().unwrap(), expected_vertices);
        assert_eq!(db.edges().unwrap(), expected_edges);
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projection_reads_never_remove_prior_or_later_broad_and_field_reads() {
    let ((), report) = run_async_under_lab(0xad7a_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for topology_first in [false, true] {
            for case in 0..5 {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx, true).await;
                let mut txn = db.begin(&txcx).unwrap();
                if topology_first { neighbours(&txn, &db, false); }
                let mut change = WriteBatch::new(R);
                match case {
                    0 => {
                        txn.vertex(&db, VId(1)).unwrap();
                        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
                    }
                    1 => {
                        txn.edge(&db, EId(10)).unwrap();
                        change.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
                    }
                    2 => {
                        txn.edge_property(&db, EId(10), P).unwrap();
                        change.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(9)));
                    }
                    3 => {
                        txn.vertex_has_label(&db, VId(1), LabelId(1)).unwrap();
                        change.set_vertex_label(VId(1), LabelId(1), false);
                    }
                    _ => {
                        txn.edges(&db).unwrap();
                        change.add_edge(EId(50), VId(3), VId(4), vec![]);
                    }
                }
                if !topology_first { neighbours(&txn, &db, false); }
                db.write(&cx, change).await.unwrap();
                assert_read_conflict(txn.finish(&mut db, &cx).await);
            }
        }
        assert_eq!(txcx.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn negative_topology_write_skew_refuses_the_second_committer_in_both_orders() {
    let ((), report) = run_async_under_lab(0xad7a_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        for incoming in [false, true] {
            for reverse in [false, true] {
                let mut db = Database::open_memory(&cx, keys()).await.unwrap();
                seed(&mut db, &cx, false).await;
                let mut left = db.begin(&txcx).unwrap();
                let mut right = db.begin(&txcx).unwrap();
                assert!(left.adjacency_neighbours(&db, VId(1), R, incoming).unwrap().is_empty());
                assert!(right.adjacency_neighbours(&db, VId(3), R, incoming).unwrap().is_empty());
                let mut a = WriteBatch::new(R);
                let mut b = WriteBatch::new(R);
                let (asrc, adst, bsrc, bdst) = if incoming { (4, 3, 2, 1) } else { (3, 4, 1, 2) };
                a.add_edge(EId(50), VId(asrc), VId(adst), vec![]);
                b.add_edge(EId(60), VId(bsrc), VId(bdst), vec![]);
                left.write(&mut db, a).unwrap();
                right.write(&mut db, b).unwrap();
                let (first, second) = if reverse { (&mut right, &mut left) } else { (&mut left, &mut right) };
                first.commit_append_only_rebased(&mut db, &cx, 1).await.unwrap();
                let frontier = db.frontier().unwrap();
                let result = second.commit_append_only_rebased(&mut db, &cx, 1).await;
                assert!(matches!(result,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))
                ));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert_eq!(db.edges().unwrap().len(), 1);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refresh_retains_the_range_and_interruption_discards_only_terminal_work() {
    let ((), report) = run_async_under_lab(0xad7a_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&cx, keys()).await.unwrap();
        seed(&mut db, &cx, true).await;
        let mut txn = db.begin(&txcx).unwrap();
        neighbours(&txn, &db, false);
        let mut update = WriteBatch::new(R);
        update.set_edge_property(EId(10), P, Some(CanonicalScalar::Int(5)));
        db.write(&cx, update).await.unwrap();
        assert_eq!(txn.refresh_snapshot(&db, &txcx).unwrap(), db.frontier().unwrap());
        assert!(txn.point_reads.borrow().contains(
            ElementId::Vertex(VId(1)),
            PointReadField::Adjacency { relation: R, incoming: false },
        ));
        let mut insert = WriteBatch::new(R);
        insert.add_edge(EId(50), VId(1), VId(4), vec![]);
        db.write(&cx, insert).await.unwrap();
        assert_read_conflict(txn.finish(&mut db, &cx).await);

        // Walk every validation checkpoint of a successful topology read close.
        // A scalable cascade over unobserved edges must also stay interruptible.
        let mut total = 0;
        for stop in std::iter::once(None).chain((1..).map(Some)) {
            if stop.is_some_and(|stop| stop > total) { break; }
            let mut db = Database::open_memory(&cx, keys()).await.unwrap();
            seed(&mut db, &cx, true).await;
            let mut txn = db.begin(&txcx).unwrap();
            // Incoming at vertex 2 observes 10/11, not 12 or the S edges.
            assert_eq!(txn.in_neighbours(&db, VId(2), R).unwrap(), vec![VId(1)]);
            let mut cascade = WriteBatch::new(S);
            cascade.delete_vertex(VId(4));
            db.write(&cx, cascade).await.unwrap();
            let frontier = db.frontier().unwrap();
            let mut calls = 0;
            let result = txn.complete_controlled(&mut db, &cx, None, false, || {
                calls += 1;
                if stop == Some(calls) { Err(WriteTxnError::NoPreparedWrite) } else { Ok(()) }
            }).await;
            if let Some(stop) = stop {
                assert!(matches!(result, Err(WriteTxnError::NoPreparedWrite)));
                assert_eq!(calls, stop);
                assert_eq!(txn.state(), EmbeddedTxnState::Aborted);
            } else {
                assert!(result.is_ok());
                total = calls;
                assert!(total > 5);
            }
            assert!(txn.point_reads.borrow().is_empty());
            assert_eq!(db.frontier().unwrap(), frontier);
            assert_eq!(txcx.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn logical_topology_domains_cover_every_small_graph_transition() {
    // An independent concrete-edge oracle. It knows no storage layout and
    // recomputes result sets; it must never see a changed answer with no conflict.
    type Edge = (EId, VId, RelationId, VId);
    let universe: Vec<Edge> = (0..9_u128).map(|id| {
        (EId(id), VId(id / 3), RelationId((id % 2) as u64 + 1), VId(id % 3))
    }).collect();
    let before_version = ObjectId([0; 32]);
    for mask in 0..512_u16 {
        let edges: Vec<_> = universe.iter().copied()
            .filter(|(eid, _, _, _)| mask & (1_u16 << (eid.0 as u32)) != 0).collect();
        for anchor in [VId(0), VId(1), VId(2)] {
            for relation in [R, S] {
                for incoming in [false, true] {
                    let selected = |edge: &Edge| edge.2 == relation
                        && (if incoming { edge.3 } else { edge.1 }) == anchor;
                    let answer = |state: &[Edge]| -> std::collections::BTreeSet<VId> {
                        state.iter().filter(|edge| selected(edge))
                            .map(|edge| if incoming { edge.1 } else { edge.3 }).collect()
                    };
                    let mut reads = PointReads::default();
                    reads.record_adjacency(anchor, relation, incoming,
                        edges.iter().filter(|edge| selected(edge)).map(|edge| edge.0));
                    let before = answer(&edges);
                    for src in [VId(0), VId(1), VId(2)] {
                        for dst in [VId(0), VId(1), VId(2)] {
                            for r in [R, S] {
                                let row = DeltaRow::CreateEdge {
                                    eid: EId(100), birth_ordinal: 1, src, relation: r, dst,
                                    canonical_key: None, props: vec![], valid_time: None,
                                };
                                let conflict = reads.conflict(&row, &mut || Ok(())).unwrap();
                                let mut after = edges.clone();
                                after.push((EId(100), src, r, dst));
                                assert_eq!(conflict.is_some(), selected(after.last().unwrap()));
                                assert!(answer(&after) == before || conflict.is_some());
                            }
                        }
                    }
                    for edge in &edges {
                        let row = DeltaRow::DeleteEdge { eid: edge.0, before_version };
                        let conflict = reads.conflict(&row, &mut || Ok(())).unwrap();
                        let after: Vec<_> = edges.iter().copied().filter(|e| e.0 != edge.0).collect();
                        assert_eq!(conflict.is_some(), selected(edge));
                        assert!(answer(&after) == before || conflict.is_some());
                    }
                    for victim in [VId(0), VId(1), VId(2)] {
                        let row = DeltaRow::DeleteVertex {
                            vid: victim, before_version,
                            sorted_retired_incident_edges: edges.iter()
                                .filter(|e| e.1 == victim || e.3 == victim).map(|e| e.0).collect(),
                        };
                        let after: Vec<_> = edges.iter().copied()
                            .filter(|e| e.1 != victim && e.3 != victim).collect();
                        let conflict = reads.conflict(&row, &mut || Ok(())).unwrap();
                        assert!(answer(&after) == before || conflict.is_some());
                        assert_eq!(conflict.is_some(), victim == anchor || edges.iter()
                            .any(|e| selected(e) && (e.1 == victim || e.3 == victim)));
                    }
                }
            }
        }
    }
}
