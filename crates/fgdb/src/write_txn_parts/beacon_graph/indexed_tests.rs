//! Native-history/overlay differential checks plus literal distance and limit
//! controls. The materialized engine is not the only oracle.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch, WriteError};
use asupersync::lab::run_async_under_lab;
use fgdb_beacon::expansion::ExpansionLimits;
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const TEXT: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId(u128::MAX);

fn options() -> Options {
    let mut options = Options::text(TEXT);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}

fn props(x: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![
        (TEXT, CanonicalScalar::ucs_basic_text("graph").unwrap()),
        (X, CanonicalScalar::Int(x)),
    ]
}

async fn fixture(cx: &CommitCx, disconnected: u128) -> Database<MemVfs> {
    let mut db = Database::open_memory(
        cx,
        DatabaseKeys::new(
            [0x91; 32],
            DatabaseSecurityNamespaceId([0x92; 32]),
            [0x93; 32],
        ),
    )
    .await
    .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, x) in [
        (VId(0), 0),
        (VId(1), 1),
        (VId(2), 2),
        (HIGH, 10),
        (VId(9), 9),
    ] {
        batch.create_vertex(id, vec![L], props(x));
    }
    for (id, a, b) in [
        (0, VId(0), VId(1)),
        (1, VId(0), VId(1)),
        (2, VId(1), VId(2)),
        (3, VId(2), HIGH),
        (4, HIGH, VId(0)),
        (5, VId(1), VId(1)),
    ] {
        batch.add_edge(EId(id), a, b, vec![]);
    }
    for id in 100..100 + disconnected {
        batch.add_edge(EId(id), VId(9), VId(9), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn spec(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
    ExpansionSpec {
        seeds,
        relation: Some(R),
        direction: ExpansionDirection::Outgoing,
        max_hops: 3,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}

fn changes() -> Vec<WriteBatch> {
    let mut changes = WriteBatch::new(R);
    changes.ensure_edge_by_triple(EId(88), VId(0), VId(1), vec![]);
    changes.delete_edge(EId(0));
    changes.delete_vertex(VId(2));
    changes.create_vertex(VId(4), vec![L], props(4));
    changes.add_edge(EId(6), VId(1), VId(4), vec![]);
    changes.add_edge(EId(u128::MAX), VId(4), HIGH, vec![]);
    changes.set_edge_property(EId(1), TEXT, Some(CanonicalScalar::Bool(true)));
    changes.set_vertex_property(VId(1), X, Some(CanonicalScalar::Int(0)));
    changes.add_edge(EId(30), VId(0), HIGH, vec![]);
    changes.delete_edge(EId(30));
    let mut other = WriteBatch::new(RelationId(2));
    other.add_edge(EId(7), VId(0), HIGH, vec![]);
    vec![changes, other]
}

#[test]
fn prepared_aliases_cascades_and_extreme_ids_share_the_exact_overlay() {
    let ((), report) = run_async_under_lab(0xbeac_4101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 64).await;
        let basis = db.frontier().unwrap();
        let seeds = [VId(0)];
        let query = GraphHybridQuery::graph_only(8, 8, 100, 60).unwrap();
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "before").unwrap();
        txn.write_ordered(&mut db, changes()).unwrap();
        let rows = txn
            .beacon_search_graph_indexed(&db, &c.query(), &options(), query, spec(&seeds))
            .unwrap();
        assert_eq!(
            rows.iter()
                .map(|hit| (hit.id, hit.graph_hops))
                .collect::<Vec<_>>(),
            [(VId(1), Some(1)), (VId(4), Some(2)), (HIGH, Some(3))]
        );
        assert_eq!(
            (rows[0].score.numerator(), rows[0].score.denominator()),
            (100, 61)
        );
        assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(88))));
        assert!(txn.read_set.borrow().contains(&ElementId::Edge(EId(30))));
        assert!(txn.scanned_edges.get());
        assert_ne!(
            db.beacon_search_graph_indexed(&c.query(), &options(), query, spec(&seeds))
                .unwrap(),
            rows
        );
        for direction in [
            ExpansionDirection::Outgoing,
            ExpansionDirection::Incoming,
            ExpansionDirection::Undirected,
        ] {
            for hops in 0..=4 {
                for relation in [None, Some(R), Some(RelationId(2))] {
                    for include in [false, true] {
                        let mut expansion = spec(&seeds);
                        expansion.direction = direction;
                        expansion.max_hops = hops;
                        expansion.relation = relation;
                        expansion.include_seeds = include;
                        assert_eq!(
                            txn.beacon_search_graph_indexed(
                                &db,
                                &c.query(),
                                &options(),
                                query,
                                expansion
                            )
                            .unwrap(),
                            txn.beacon_search_graph(&db, &c.query(), &options(), query, expansion)
                                .unwrap(),
                            "{direction:?} {hops} {relation:?} {include}",
                        );
                    }
                }
            }
        }
        assert_eq!(db.frontier().unwrap(), basis);
        txn.rollback_to_savepoint(&db, "before").unwrap();
        assert_eq!(
            txn.beacon_search_graph_indexed(&db, &c.query(), &options(), query, spec(&seeds))
                .unwrap(),
            db.beacon_search_graph_indexed(&c.query(), &options(), query, spec(&seeds))
                .unwrap()
        );
        assert!(
            txn.read_set.borrow().contains(&ElementId::Edge(EId(30))),
            "rollback retains reads"
        );
        txn.write_ordered(&mut db, changes()).unwrap();
        txn.commit(&mut db, &c.commit()).await.unwrap();
        assert_eq!(
            db.beacon_search_graph_indexed(&c.query(), &options(), query, spec(&seeds))
                .unwrap(),
            rows
        );
        assert!(matches!(
            txn.beacon_search_graph_indexed(&db, &c.query(), &options(), query, spec(&seeds)),
            Err(ReadError::Read(WriteTxnError::Finished))
        ));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prelimit_filter_preserves_transit_and_staged_labels_in_every_lane() {
    let ((), report) = run_async_under_lab(0xbeac_4102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 16).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.write_ordered(&mut db, changes()).unwrap();
        txn.savepoint(&db, "linked").unwrap();
        let seeds = [VId(0)];
        for (vector, text) in [(false, false), (true, false), (false, true), (true, true)] {
            let query = if vector || text {
                GraphHybridQuery {
                    retrieval: ExactHybridQuery {
                        vector: &[0.0],
                        text: "graph",
                        k: 1,
                        vector_candidates: 1,
                        text_candidates: 1,
                        vector_mode: VectorSearch::Exact,
                        text_mode: TextMatch::Any,
                        profile: ExactRrfProfile::new(60, u16::from(vector), u16::from(text))
                            .unwrap(),
                    },
                    graph_candidates: 1,
                    graph_weight: 100,
                }
            } else {
                GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap()
            };
            let rows = txn
                .beacon_search_graph_indexed_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    query,
                    spec(&seeds),
                    |id| id == HIGH,
                )
                .unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, HIGH);
            assert_eq!(rows[0].graph_hops, Some(3));
            assert_eq!(rows[0].graph_rank.map(|rank| rank.get()), Some(1));
            assert_eq!(
                rows[0].vector_rank.map(|rank| rank.get()),
                vector.then_some(1)
            );
            assert_eq!(rows[0].text_rank.map(|rank| rank.get()), text.then_some(1));
            assert_eq!(
                rows,
                txn.beacon_search_graph_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    query,
                    spec(&seeds),
                    |id| id == HIGH
                )
                .unwrap()
            );
        }
        let mut unlabel = WriteBatch::new(R);
        unlabel.set_vertex_label(VId(4), L, false);
        txn.write(&mut db, unlabel).unwrap();
        let query = GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap();
        assert!(
            txn.beacon_search_graph_indexed_filtered(
                &db,
                &c.query(),
                &options(),
                query,
                spec(&seeds),
                |id| id == HIGH
            )
            .unwrap()
            .is_empty()
        );
        txn.rollback_to_savepoint(&db, "linked").unwrap();
        assert_eq!(
            txn.beacon_search_graph_indexed_filtered(
                &db,
                &c.query(),
                &options(),
                query,
                spec(&seeds),
                |id| id == HIGH
            )
            .unwrap()[0]
                .graph_hops,
            Some(3)
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reached_incidence_limits_do_not_copy_disconnected_edges_or_hide_late_failures() {
    let ((), report) = run_async_under_lab(0xbeac_4103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 128).await;
        let txn = db.begin(&c.txn()).unwrap();
        let seeds = [VId(0)];
        let mut expansion = spec(&seeds);
        expansion.max_hops = 2;
        expansion.limits.max_input_edges = 4;
        let q = GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap();
        let rows = txn
            .beacon_search_graph_indexed_filtered(&db, &c.query(), &options(), q, expansion, |id| {
                id == VId(2)
            })
            .unwrap();
        assert_eq!((rows[0].id, rows[0].graph_hops), (VId(2), Some(2)));
        // Eager source admission sees all 134 edges; the indexed source sees
        // four selected incidences, including parallel edges and the loop.
        assert!(
            txn.beacon_search_graph_filtered(&db, &c.query(), &options(), q, expansion, |id| id
                == VId(2))
                .is_err()
        );
        expansion.limits.max_input_edges = 3;
        for accept in [true, false] {
            assert!(matches!(
                txn.beacon_search_graph_indexed_filtered(
                    &db,
                    &c.query(),
                    &options(),
                    q,
                    expansion,
                    |_| accept
                ),
                Err(ReadError::Index(BeaconError::ResourceLimit {
                    resource: "transaction indexed incidence visits",
                    limit: 3,
                }))
            ));
        }
        expansion.limits.max_input_edges = 4;
        expansion.limits.max_source_scratch = 0;
        assert!(
            txn.beacon_search_graph_indexed(&db, &c.query(), &options(), q, expansion)
                .is_err()
        );
        txn.abort();
        let txn = db.begin(&c.txn()).unwrap();
        let seeds = [HIGH, VId(9), VId(9), VId(999)];
        let mut zero = spec(&seeds);
        zero.max_hops = 0;
        zero.include_seeds = true;
        zero.limits.max_input_edges = 0;
        zero.limits.max_source_scratch = 0;
        let q = GraphHybridQuery::graph_only(2, 2, 1, 60).unwrap();
        let rows = txn
            .beacon_search_graph_indexed(&db, &c.query(), &options(), q, zero)
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [VId(9), HIGH]
        );
        assert!(
            !txn.scanned_edges.get(),
            "zero hops must not demand adjacency"
        );
        let mut bounded = options();
        bounded.policy.max_staging_rows = 0;
        assert!(
            txn.beacon_search_graph_indexed(&db, &c.query(), &bounded, q, zero)
                .is_err()
        );
        txn.abort();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_empty_reads_keep_global_edge_conflicts_after_rollback() {
    let ((), report) = run_async_under_lab(0xbeac_4104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        txn.savepoint(&db, "before-read").unwrap();
        let seeds = [VId(0)];
        let q = GraphHybridQuery::graph_only(1, 1, 1, 60).unwrap();
        let before = txn
            .beacon_search_graph_indexed(&db, &c.query(), &options(), q, spec(&seeds))
            .unwrap();
        assert!(
            txn.beacon_search_graph_indexed_filtered(
                &db,
                &c.query(),
                &options(),
                q,
                spec(&seeds),
                |_| false
            )
            .unwrap()
            .is_empty()
        );
        txn.rollback_to_savepoint(&db, "before-read").unwrap();
        let mut concurrent = WriteBatch::new(R);
        concurrent.add_edge(EId(77), VId(0), VId(9), vec![]);
        db.write(&c.commit(), concurrent).await.unwrap();
        assert_eq!(
            txn.beacon_search_graph_indexed(&db, &c.query(), &options(), q, spec(&seeds))
                .unwrap(),
            before
        );
        let frontier = db.frontier().unwrap();
        assert!(matches!(
            txn.finish(&mut db, &c.commit()).await,
            Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                law: "FG-LAW-FCW-READ-01",
                ..
            }))
        ));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_observed_checkpoint_can_refuse_with_no_prefix_or_implicit_commit() {
    let ((), report) = run_async_under_lab(0xbeac_4105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = fixture(&c.commit(), 0).await;
        let basis = db.frontier().unwrap();
        for k in [0, 1] {
            for accept in [true, false] {
                let mut run = |stop| {
                    let mut txn = db.begin(&c.txn()).unwrap();
                    txn.write_ordered(&mut db, changes()).unwrap();
                    let events = Cell::new(0);
                    let observe = |_: super::super::super::AdmissionEvent| {
                        events.set(events.get() + 1);
                        if events.get() == stop {
                            Err(BeaconError::Cancelled)
                        } else {
                            Ok(())
                        }
                    };
                    let result = txn.beacon_search_graph_with_access_observed(
                        &db,
                        &c.query(),
                        &options(),
                        GraphHybridQuery::graph_only(k, 1, 1, 60).unwrap(),
                        spec(&[VId(0)]),
                        true,
                        Some(&observe),
                        |_| Ok(()),
                        |id, _| accept && id == HIGH,
                        |result| result,
                    );
                    assert_eq!(db.frontier().unwrap(), basis);
                    txn.abort();
                    (result, events.get())
                };
                let (expected, events) = run(usize::MAX);
                assert_eq!(expected.unwrap().len(), if accept { k } else { 0 });
                assert!(events > 2);
                for stop in 1..=events {
                    let (result, seen) = run(stop);
                    assert!(
                        matches!(result, Err(ReadError::Index(BeaconError::Cancelled))),
                        "cut={stop}, k={k}"
                    );
                    assert_eq!(seen, stop, "first refusal must stop callbacks");
                }
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
