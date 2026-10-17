use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch, WriteError};
use asupersync::lab::run_async_under_lab;
use fgdb_beacon::expansion::ExpansionLimits;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts,
};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const HIGH: VId = VId(u128::MAX);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn options() -> Options {
    let mut options = Options::text(PropertyKeyId(1));
    options.vertex_label = Some(L);
    // Invalid as an index configuration, valid as a topology-only request.
    options.index.text = None;
    options.index.vector = None;
    options
}

fn query() -> GraphHybridQuery<'static> {
    GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap()
}

fn expansion() -> ExpansionSpec<'static, RelationId> {
    ExpansionSpec {
        seeds: &[VId(0)],
        relation: Some(R),
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}

async fn fixture(cx: &CommitCx, disconnected: usize) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in [VId(0), VId(1), HIGH, VId(9)] {
        // Graph-only retrieval must never interpret this as text/vector data.
        batch.create_vertex(
            id,
            vec![L],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-1))],
        );
    }
    for id in 0..disconnected {
        batch.create_vertex(VId(1000 + id as u128), vec![L], vec![]);
    }
    for (eid, a, b) in [
        (EId(0), VId(0), VId(1)),
        (EId(1), VId(0), VId(1)),
        (EId(2), VId(1), HIGH),
        (EId(3), HIGH, VId(0)),
    ] {
        batch.add_edge(eid, a, b, vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn demand_bounds_probed_membership_not_the_committed_population() {
    let ((), report) = run_async_under_lab(0xbeac_7201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 1024).await;
        let txn = db.begin(&c.txn()).unwrap();
        let before = db.frontier().unwrap();
        let mut options = options();
        options.policy.max_staging_rows = 3;
        options.index.max_documents = 3;
        let mut spec = expansion();
        spec.limits.max_vertices = 3;
        spec.limits.max_input_edges = 3;
        let hits = txn
            .beacon_search_graph_demand_filtered(&db, &c.query(), &options, query(), spec, |id| {
                id == HIGH
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, HIGH);
        assert_eq!(hits[0].graph_hops, Some(2));
        assert_eq!(hits[0].graph_rank.unwrap().get(), 1);
        assert_eq!(
            (hits[0].score.numerator(), hits[0].score.denominator()),
            (100, 61)
        );
        assert!(!txn.scanned_vertices.get());
        assert!(txn.scanned_vertex_labels.borrow().is_empty());
        assert!(!txn.scanned_edges.get());
        for id in [VId(0), VId(1), HIGH] {
            let reads = txn.point_reads.borrow();
            assert!(reads.contains(ElementId::Vertex(id), PointReadField::VertexExistence));
            assert!(reads.contains(ElementId::Vertex(id), PointReadField::Label(L)));
        }
        assert!(txn.read_set.borrow().is_empty());
        assert!(!txn.point_reads.borrow().contains(
            ElementId::Vertex(VId(1000)),
            PointReadField::VertexExistence,
        ));
        assert!(
            !txn.point_reads.borrow().contains(
                ElementId::Vertex(HIGH),
                PointReadField::Adjacency {
                    relation: R,
                    incoming: false
                },
            ),
            "the hop-boundary vertex's adjacency was not read"
        );
        // The old indexed profile still stages every selected vertex. A
        // silent fallback cannot pass the same three-entry admission bound.
        assert!(
            txn.beacon_search_graph_indexed_filtered(
                &db,
                &c.query(),
                &options,
                query(),
                spec,
                |id| id == HIGH,
            )
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        txn.abort();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_labels_aliases_cascades_and_parallel_edges_match_both_other_sources() {
    let ((), report) = run_async_under_lab(0xbeac_7202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "before").unwrap();
        let mut batch = WriteBatch::new(R);
        batch.ensure_edge_by_triple(EId(100), VId(0), VId(1), vec![]);
        batch.delete_edge(EId(0));
        batch.delete_vertex(HIGH);
        batch.create_vertex(VId(3), vec![L], vec![]);
        batch.create_vertex(VId(4), vec![], vec![]);
        batch.set_vertex_label(VId(4), L, true);
        batch.add_edge(EId(u128::MAX), VId(1), VId(3), vec![]);
        batch.add_edge(EId(40), VId(3), VId(4), vec![]);
        batch.add_edge(EId(41), VId(4), VId(0), vec![]);
        batch.create_vertex(VId(7), vec![L], vec![]);
        batch.delete_vertex(VId(7));
        txn.write(&mut db, batch).unwrap();
        let query = GraphHybridQuery::graph_only(8, 8, 1, 60).unwrap();
        for direction in [
            ExpansionDirection::Outgoing,
            ExpansionDirection::Incoming,
            ExpansionDirection::Undirected,
        ] {
            for hops in 0..=3 {
                for include in [false, true] {
                    let mut spec = expansion();
                    spec.direction = direction;
                    spec.max_hops = hops;
                    spec.include_seeds = include;
                    let allowed = |id| id != VId(1);
                    let actual = txn
                        .beacon_search_graph_demand_filtered(
                            &db,
                            &c.query(),
                            &options(),
                            query,
                            spec,
                            allowed,
                        )
                        .unwrap();
                    assert_eq!(
                        actual,
                        txn.beacon_search_graph_indexed_filtered(
                            &db,
                            &c.query(),
                            &options(),
                            query,
                            spec,
                            allowed,
                        )
                        .unwrap()
                    );
                    assert_eq!(
                        actual,
                        txn.beacon_search_graph_filtered(
                            &db,
                            &c.query(),
                            &options(),
                            query,
                            spec,
                            allowed,
                        )
                        .unwrap()
                    );
                }
            }
        }
        assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(7))));
        assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(100))));
        let expected = txn
            .beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query,
                expansion(),
                |id| id == VId(3),
            )
            .unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].graph_hops, Some(2));
        txn.commit(&mut db, &c.commit()).await.unwrap();
        assert_eq!(
            db.beacon_search_graph_demand_filtered(
                &c.query(),
                &options(),
                query,
                expansion(),
                |id| id == VId(3),
            )
            .unwrap(),
            expected
        );
        assert!(matches!(
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query,
                expansion(),
                |_| true,
            ),
            Err(ReadError::Read(WriteTxnError::Finished))
        ));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_basis_and_rollback_preserve_rejected_transit_and_absent_seed_conflicts() {
    let ((), report) = run_async_under_lab(0xbeac_7203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "before-read").unwrap();
        let original = txn
            .beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |id| id == HIGH,
            )
            .unwrap();
        let mut local = WriteBatch::new(R);
        local.set_vertex_label(VId(1), L, false);
        txn.write(&mut db, local).unwrap();
        assert!(
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |id| id == HIGH,
            )
            .unwrap()
            .is_empty()
        );
        txn.rollback_to_savepoint(&db, "before-read").unwrap();
        let mut winner = WriteBatch::new(R);
        winner.set_vertex_label(VId(1), L, false);
        db.write(&c.commit(), winner).await.unwrap();
        assert_eq!(
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |id| id == HIGH,
            )
            .unwrap(),
            original
        );
        assert!(matches!(
            txn.finish(&mut db, &c.commit()).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ));

        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "missing").unwrap();
        let mut spec = expansion();
        spec.seeds = &[VId(77)];
        spec.max_hops = 0;
        spec.include_seeds = true;
        assert!(
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query(),
                spec,
                |_| false,
            )
            .unwrap()
            .is_empty()
        );
        assert!(!txn.scanned_edges.get());
        txn.rollback_to_savepoint(&db, "missing").unwrap();
        let mut winner = WriteBatch::new(R);
        winner.create_vertex(VId(77), vec![L], vec![]);
        db.write(&c.commit(), winner).await.unwrap();
        assert!(matches!(
            txn.finish(&mut db, &c.commit()).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reject_all_and_zero_k_keep_resource_refusals_without_finishing_the_transaction() {
    let ((), report) = run_async_under_lab(0xbeac_7204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0).await;
        let other = fixture(&c.commit(), 0).await;
        let txn = db.begin(&c.txn()).unwrap();
        let before = db.frontier().unwrap();
        for k in [0, 1] {
            let query = GraphHybridQuery::graph_only(k, 1, 1, 60).unwrap();
            for case in 0..5 {
                let mut options = options();
                let mut spec = expansion();
                match case {
                    0 => options.policy.max_work_units = 0,
                    1 => options.policy.max_source_scratch = 0,
                    2 => options.policy.max_staging_rows = 2,
                    3 => spec.limits.max_input_edges = 2,
                    _ => spec.limits.max_visited_vertices = 2,
                }
                assert!(
                    txn.beacon_search_graph_demand_filtered(
                        &db,
                        &c.query(),
                        &options,
                        query,
                        spec,
                        |_| false,
                    )
                    .is_err(),
                    "k={k}, case={case}"
                );
            }
        }
        let mut historical = options();
        historical.as_of = Some(CommitSeq(before.0 + 1));
        assert!(matches!(
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &historical,
                query(),
                expansion(),
                |_| true,
            ),
            Err(ReadError::Index(BeaconError::InvalidQuery(_)))
        ));
        assert!(matches!(
            txn.beacon_search_graph_demand_filtered(
                &other,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |_| true,
            ),
            Err(ReadError::Read(WriteTxnError::WrongDatabase))
        ));
        assert_eq!(
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |id| id == HIGH,
            )
            .unwrap()
            .len(),
            1
        );
        assert_eq!(db.frontier().unwrap(), before);
        txn.abort();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_observer_cut_refuses_atomically_and_retry_preserves_staged_effects() {
    use std::cell::Cell;
    let ((), report) = run_async_under_lab(0xbeac_7205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0).await;
        let before = db.frontier().unwrap();
        // Each run gets fresh witnesses, so the event trace is identical up
        // to its cut. A later retry on that same txn must still see the edit.
        let run = |stop, accept| {
            let mut txn = db.begin(&c.txn()).unwrap();
            let mut batch = WriteBatch::new(R);
            batch.delete_edge(EId(2));
            batch.create_vertex(VId(3), vec![L], vec![]);
            batch.add_edge(EId(30), VId(1), VId(3), vec![]);
            txn.write(&mut db, batch).unwrap();
            let calls = Cell::new(0_usize);
            let observe = |_| {
                let n = calls.get() + 1;
                calls.set(n);
                if n == stop {
                    Err(BeaconError::Cancelled)
                } else {
                    Ok(())
                }
            };
            let result = txn.beacon_search_graph_demand_observed(
                &db,
                &c.query(),
                &options(),
                query(),
                expansion(),
                Some(&observe),
                |_| Ok(()),
                |id, _| accept && id == VId(3),
                |result| result,
            );
            let seen = calls.get();
            let retried = txn
                .beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    query(),
                    expansion(),
                    |id| id == VId(3),
                )
                .unwrap();
            assert_eq!(retried.len(), 1);
            assert_eq!(retried[0].id, VId(3));
            assert_eq!(retried[0].graph_hops, Some(2));
            txn.abort();
            (result, seen)
        };
        let mut run = run;
        for accept in [false, true] {
            let (success, calls) = run(usize::MAX, accept);
            assert_eq!(success.unwrap().len(), usize::from(accept));
            assert!(calls > 10);
            for stop in 1..=calls {
                let (result, seen) = run(stop, accept);
                assert!(
                    matches!(result, Err(ReadError::Index(BeaconError::Cancelled))),
                    "{stop}"
                );
                assert_eq!(seen, stop);
            }
        }
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn assert_read_conflict<T: core::fmt::Debug>(result: Result<T, WriteTxnError>) {
    assert!(
        matches!(
            &result,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ),
        "expected original-basis read conflict, got {result:?}"
    );
}

#[test]
fn reached_topology_allows_disjoint_writes_ignored_fields_and_unexpanded_edges() {
    let ((), report) = run_async_under_lab(0xbeac_7301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for case in 0..8 {
            let mut db = fixture(&c.commit(), 0).await;
            let mut txn = db.begin(&c.txn()).unwrap();
            let mut opts = options();
            if case == 7 {
                opts.vertex_label = None;
            }
            let baseline = txn
                .beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &opts,
                    query(),
                    expansion(),
                    |id| id == HIGH,
                )
                .unwrap();
            assert_eq!(
                (baseline.len(), baseline[0].id, baseline[0].graph_hops),
                (1, HIGH, Some(2))
            );
            assert!(txn.read_set.borrow().is_empty());
            assert!(!txn.scanned_edges.get());
            // Live incumbent: the complete-directory profile still retains
            // its original graph-wide witness on the exact same database.
            let mut incumbent = (case == 0).then(|| db.begin(&c.txn()).unwrap());
            if let Some(incumbent) = &incumbent {
                assert_eq!(
                    incumbent
                        .beacon_search_graph_indexed_filtered(
                            &db,
                            &c.query(),
                            &opts,
                            query(),
                            expansion(),
                            |id| id == HIGH,
                        )
                        .unwrap(),
                    baseline
                );
            }
            let mut write = WriteBatch::new(if case == 6 { RelationId(2) } else { R });
            match case {
                0 => write.add_edge(EId(90), VId(9), VId(9), vec![]),
                1 => write.set_vertex_property(
                    VId(1),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(99)),
                ),
                2 => write.set_edge_property(
                    EId(0),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(99)),
                ),
                3 => write.set_vertex_label(VId(1), LabelId(2), true),
                4 => write.add_edge(EId(90), HIGH, VId(9), vec![]),
                5 => write.add_edge(EId(90), VId(9), VId(0), vec![]),
                6 => write.add_edge(EId(90), VId(0), VId(9), vec![]),
                _ => write.set_vertex_label(VId(1), L, false),
            };
            db.write(&c.commit(), write).await.unwrap();
            assert_eq!(
                txn.beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &opts,
                    query(),
                    expansion(),
                    |id| id == HIGH,
                )
                .unwrap(),
                baseline,
                "case {case}: pinned result changed"
            );
            assert_eq!(
                txn.refresh_snapshot(&db, &c.txn()).unwrap(),
                db.frontier().unwrap()
            );
            assert_eq!(
                txn.beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &opts,
                    query(),
                    expansion(),
                    |id| id == HIGH,
                )
                .unwrap(),
                baseline,
                "case {case}: irrelevant write changed result"
            );
            txn.finish(&mut db, &c.commit()).await.unwrap();
            if let Some(incumbent) = &mut incumbent {
                assert_read_conflict(incumbent.finish(&mut db, &c.commit()).await);
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_adjacency_ranges_cover_both_directions_and_previously_unseen_relations() {
    let ((), report) = run_async_under_lab(0xbeac_7302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for direction in [
            ExpansionDirection::Outgoing,
            ExpansionDirection::Incoming,
            ExpansionDirection::Undirected,
        ] {
            for requested in [Some(R), None] {
                for relation in [R, RelationId(2)] {
                    // All edges are new identities; relation 2 has NO initial
                    // edges. A set of observed relation IDs cannot pass this.
                    for (src, dst) in [
                        (VId(9), VId(0)),
                        (VId(0), VId(9)),
                        (VId(9), VId(9)),
                        (VId(0), VId(1)),
                    ] {
                        let mut db = fixture(&c.commit(), 0).await;
                        let mut txn = db.begin(&c.txn()).unwrap();
                        txn.savepoint(&db, "before").unwrap();
                        let mut spec = expansion();
                        spec.seeds = &[VId(9)];
                        spec.max_hops = 1;
                        spec.direction = direction;
                        spec.relation = requested;
                        assert!(
                            txn.beacon_search_graph_demand_filtered(
                                &db,
                                &c.query(),
                                &options(),
                                query(),
                                spec,
                                |_| false,
                            )
                            .unwrap()
                            .is_empty()
                        );
                        assert!(!txn.scanned_edges.get());
                        txn.rollback_to_savepoint(&db, "before").unwrap();
                        let mut write = WriteBatch::new(relation);
                        write.add_edge(EId(u128::MAX), src, dst, vec![]);
                        db.write(&c.commit(), write).await.unwrap();
                        let affected = requested.is_none_or(|wanted| wanted == relation)
                            && match direction {
                                ExpansionDirection::Outgoing => src == VId(9),
                                ExpansionDirection::Incoming => dst == VId(9),
                                ExpansionDirection::Undirected => src == VId(9) || dst == VId(9),
                            };
                        let result = txn.finish(&mut db, &c.commit()).await;
                        if affected {
                            assert_read_conflict(result);
                        } else {
                            assert!(
                                result.is_ok(),
                                "{direction:?} {requested:?} {relation:?} {src:?}->{dst:?}: {result:?}"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parallel_retirement_cascades_and_selected_label_restoration_remain_conflicts() {
    let ((), report) = run_async_under_lab(0xbeac_7303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for case in 0..7 {
            let mut db = fixture(&c.commit(), 0).await;
            if case == 6 {
                let mut bridge = WriteBatch::new(R);
                bridge.add_edge(EId(9), VId(0), VId(9), vec![]);
                bridge.set_vertex_label(VId(9), L, false);
                db.write(&c.commit(), bridge).await.unwrap();
            }
            let mut txn = db.begin(&c.txn()).unwrap();
            txn.savepoint(&db, "before").unwrap();
            let target = if case == 6 { VId(9) } else { HIGH };
            let baseline = txn
                .beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    query(),
                    expansion(),
                    |id| id == target,
                )
                .unwrap();
            assert_eq!(baseline.len(), usize::from(case != 6));
            txn.rollback_to_savepoint(&db, "before").unwrap();
            let basis = txn.basis();
            let mut write = WriteBatch::new(R);
            match case {
                0 => write.delete_edge(EId(0)), // another parallel edge preserves the same hops
                1 => write.delete_edge(EId(2)),
                2 => write.delete_vertex(HIGH), // retirement in a vertex cascade
                3 => write.delete_vertex(VId(1)),
                4 => write.set_vertex_label(VId(1), L, false),
                5 => write.set_vertex_label(HIGH, L, false),
                _ => write.set_vertex_label(VId(9), L, true), // rejected membership becomes present
            };
            db.write(&c.commit(), write).await.unwrap();
            if case == 4 {
                let mut restore = WriteBatch::new(R);
                restore.set_vertex_label(VId(1), L, true);
                db.write(&c.commit(), restore).await.unwrap();
            }
            assert_eq!(
                txn.beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    query(),
                    expansion(),
                    |id| id == target,
                )
                .unwrap(),
                baseline
            );
            assert_read_conflict(txn.refresh_snapshot(&db, &c.txn()));
            assert_eq!(
                txn.basis(),
                basis,
                "refused refresh preserves the old basis"
            );
            assert_read_conflict(txn.finish(&mut db, &c.commit()).await);
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_refusal_unwind_and_retry_never_erase_the_conservative_failure_witness() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    let ((), report) = run_async_under_lab(0xbeac_7304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for unwind in [false, true] {
            let mut db = fixture(&c.commit(), 0).await;
            let mut txn = db.begin(&c.txn()).unwrap();
            txn.savepoint(&db, "before").unwrap();
            if unwind {
                assert!(
                    catch_unwind(AssertUnwindSafe(|| {
                        let _ = txn.beacon_search_graph_demand_filtered(
                            &db,
                            &c.query(),
                            &options(),
                            query(),
                            expansion(),
                            |_| panic!("endpoint predicate unwind"),
                        );
                    }))
                    .is_err()
                );
            } else {
                let mut short = options();
                short.policy.max_source_scratch = 1; // only half of a point witness can fit
                assert!(matches!(
                    txn.beacon_search_graph_demand_filtered(
                        &db,
                        &c.query(),
                        &short,
                        query(),
                        expansion(),
                        |_| false,
                    ),
                    Err(ReadError::Index(BeaconError::ResourceLimit { .. }))
                ));
            }
            assert!(txn.point_reads.borrow().1.is_some());
            assert_eq!(
                txn.beacon_search_graph_demand_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    query(),
                    expansion(),
                    |id| id == HIGH,
                )
                .unwrap()
                .len(),
                1
            );
            txn.rollback_to_savepoint(&db, "before").unwrap();
            assert!(txn.point_reads.borrow().1.is_some());
            let mut write = WriteBatch::new(R);
            write.set_vertex_property(VId(9), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
            db.write(&c.commit(), write).await.unwrap();
            assert_read_conflict(txn.finish(&mut db, &c.commit()).await);
        }
        // Initial owner/cut refusal reveals no source and installs no witness.
        let mut db = fixture(&c.commit(), 0).await;
        let other = fixture(&c.commit(), 0).await;
        let txn = db.begin(&c.txn()).unwrap();
        assert!(matches!(
            txn.beacon_search_graph_demand_filtered(
                &other,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |_| false,
            ),
            Err(ReadError::Read(WriteTxnError::WrongDatabase))
        ));
        assert!(txn.point_reads.borrow().is_empty());
        txn.abort();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn demand_never_narrows_prior_full_getters_scans_or_explicit_property_reads() {
    let ((), report) = run_async_under_lab(0xbeac_7305, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        for prior in 0..3 {
            let mut db = fixture(&c.commit(), 0).await;
            let mut txn = db.begin(&c.txn()).unwrap();
            match prior {
                0 => {
                    txn.vertex(&db, VId(1)).unwrap();
                }
                1 => {
                    txn.beacon_search_graph_indexed_filtered(
                        &db,
                        &c.query(),
                        &options(),
                        query(),
                        expansion(),
                        |_| false,
                    )
                    .unwrap();
                }
                _ => {
                    txn.vertex_property(&db, VId(1), PropertyKeyId(1)).unwrap();
                }
            }
            txn.beacon_search_graph_demand_filtered(
                &db,
                &c.query(),
                &options(),
                query(),
                expansion(),
                |id| id == HIGH,
            )
            .unwrap();
            let mut write = WriteBatch::new(R);
            if prior == 1 {
                write.add_edge(EId(90), VId(9), VId(9), vec![]);
            } else {
                write.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
            }
            db.write(&c.commit(), write).await.unwrap();
            assert_read_conflict(txn.finish(&mut db, &c.commit()).await);
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
