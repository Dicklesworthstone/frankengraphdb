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
        assert!(txn.scanned_edges.get());
        for id in [VId(0), VId(1), HIGH] {
            assert!(txn.read_set.borrow().contains(&ElementId::Vertex(id)));
        }
        assert!(
            !txn.read_set
                .borrow()
                .contains(&ElementId::Vertex(VId(1000)))
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
