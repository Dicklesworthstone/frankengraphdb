//! Graph-only queries use the native indexed source, never a dummy index or
//! a copy of unrelated edges. The materialized adapter is a differential
//! oracle; independent endpoint, distance and rational-score checks also apply.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::WorkBudget;
use fgdb_delta_types::SchemaEpoch;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xe2; 32]);
const R: RelationId = RelationId(1);
const HIGH: VId = VId(u128::MAX);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xe1; 32], NS, [0xe3; 32])
}

fn options() -> Options {
    let mut options = Options::text(PropertyKeyId(9));
    options.vertex_label = Some(LabelId(1));
    options.index.text = None;
    options.index.vector = None;
    options
}

fn expansion(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
    ExpansionSpec {
        seeds,
        relation: Some(R),
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}

async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    for id in [VId(1), VId(2), HIGH, VId(9)] {
        // Deliberately not text: topology-only execution must never project it.
        batch.create_vertex(
            id,
            vec![LabelId(1)],
            vec![(PropertyKeyId(9), CanonicalScalar::Bool(true))],
        );
    }
    for (eid, from, to) in [
        (1, VId(1), VId(2)),
        (2, VId(1), VId(2)),
        (3, VId(2), HIGH),
        (4, VId(2), VId(2)),
        (5, HIGH, VId(1)),
    ] {
        batch.add_edge(EId(eid), from, to, vec![]);
    }
    for eid in 100..164 {
        batch.add_edge(EId(eid), VId(9), VId(9), vec![]);
    }
    if hidden {
        batch.create_vertex(VId(90), vec![LabelId(99)], vec![]);
        batch.add_edge(EId(90), VId(1), VId(90), vec![]);
        batch.add_edge(EId(91), VId(90), HIGH, vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut forbidden = WriteBatch::new(RelationId(2));
        forbidden.add_edge(EId(92), VId(1), HIGH, vec![]);
        db.write(cx, forbidden).await.unwrap();
    }
}

#[test]
fn no_index_or_properties_and_only_reached_incidence_are_required() {
    let ((), report) = run_async_under_lab(0xbeac_3801, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), false).await;
        let options = options();
        assert!(
            options.index.validate().is_err(),
            "no synthetic enabled lane"
        );
        let q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let seeds = [VId(1)];
        let mut spec = expansion(&seeds);
        let expected = db
            .beacon_search_graph_filtered(&c.query(), &options, q, spec, |id| id == HIGH)
            .unwrap();
        assert_eq!(expected.len(), 1);
        assert_eq!(expected[0].id, HIGH);
        assert_eq!(expected[0].graph_hops, Some(2));
        assert_eq!(expected[0].graph_rank.unwrap().get(), 1);
        assert_eq!(
            (
                expected[0].score.numerator(),
                expected[0].score.denominator()
            ),
            (100, 61)
        );
        assert_eq!(expected[0].text_rank, None);
        assert_eq!(expected[0].vector_rank, None);
        spec.limits.max_input_edges = 4;
        let work = RefCell::new(WorkBudget::new(1_000_000));
        let admitted = Cell::new(0);
        let actual = evaluate_filtered(
            &db.snapshot,
            db.frontier().unwrap(),
            &options,
            q,
            spec,
            &work,
            Scan::Metered,
            |_| Ok(true),
            |_| true,
            |_| panic!("topology-only execution accessed a property"),
            |_| true,
            || {
                admitted.set(admitted.get() + 1);
                Ok(())
            },
            |id| id == HIGH,
        )
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(admitted.get(), 4);
        assert_eq!(
            db.beacon_search_graph_indexed_filtered(&c.query(), &options, q, spec, |id| id == HIGH)
                .unwrap(),
            expected
        );
        // The selected component has four reached incidences; neither the
        // hop-boundary edge nor 64 disconnected self-loops is copied/charged.
        assert!(
            db.beacon_search_graph_filtered(&c.query(), &options, q, spec, |id| id == HIGH)
                .is_err()
        );
        spec.limits.max_input_edges = 3;
        assert!(matches!(
            db.beacon_search_graph_indexed_filtered(&c.query(), &options, q, spec, |_| false),
            Err(Error::Index(BeaconError::ResourceLimit {
                resource: "indexed expansion incidence visits",
                limit: 3
            }))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn indexed_graph_only_directions_and_history_equal_the_materialized_source() {
    let ((), report) = run_async_under_lab(0xbeac_3802, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&c.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        seed(&mut db, &c.commit(), false).await;
        let at = db.frontier().unwrap();
        let pinned = db.read_session().unwrap();
        let q = GraphHybridQuery::graph_only(4, 4, 1, 60).unwrap();
        let seeds = [VId(1), VId(1), VId(999)];
        for direction in [
            ExpansionDirection::Outgoing,
            ExpansionDirection::Incoming,
            ExpansionDirection::Undirected,
        ] {
            for hops in 0..=3 {
                for include in [false, true] {
                    let mut spec = expansion(&seeds);
                    spec.direction = direction;
                    spec.max_hops = hops;
                    spec.include_seeds = include;
                    assert_eq!(
                        db.beacon_search_graph_indexed(&c.query(), &options(), q, spec)
                            .unwrap(),
                        db.beacon_search_graph(&c.query(), &options(), q, spec)
                            .unwrap(),
                        "{direction:?}, hops={hops}, include={include}",
                    );
                }
            }
        }
        let spec = expansion(&seeds);
        let before = pinned
            .beacon_search_graph_indexed(&c.query(), &options(), q, spec)
            .unwrap();
        let mut changes = WriteBatch::new(R);
        changes.delete_edge(EId(3));
        changes.add_edge(EId(u128::MAX), VId(1), VId(9), vec![]);
        db.write(&c.commit(), changes).await.unwrap();
        assert_ne!(
            db.beacon_search_graph_indexed(&c.query(), &options(), q, spec)
                .unwrap(),
            before
        );
        db.compact(&c.commit()).await.unwrap();
        let mut historical = options();
        historical.as_of = Some(at);
        assert_eq!(
            db.beacon_search_graph_indexed(&c.query(), &historical, q, spec)
                .unwrap(),
            before
        );
        historical.as_of = Some(CommitSeq(at.0 + 1));
        assert!(matches!(
            pinned.beacon_search_graph_indexed(&c.query(), &historical, q, spec),
            Err(Error::Read(ReadError::BeyondFrontier { .. }))
        ));
        drop(db);
        assert_eq!(
            pinned
                .beacon_search_graph_indexed(&c.query(), &options(), q, spec)
                .unwrap(),
            before
        );
        let db = Database::open_with_vfs(&c.commit(), vfs, &path, keys())
            .await
            .unwrap();
        historical.as_of = Some(at);
        assert_eq!(
            db.beacon_search_graph_indexed(&c.query(), &historical, q, spec)
                .unwrap(),
            before
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_hop_isolates_skip_edges_but_not_vertex_or_output_admission() {
    let ((), report) = run_async_under_lab(0xbeac_3803, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), false).await;
        let q = GraphHybridQuery::graph_only(2, 2, 1, 60).unwrap();
        let seeds = [HIGH, VId(9), VId(9), VId(999)];
        let mut spec = expansion(&seeds);
        spec.max_hops = 0;
        spec.include_seeds = true;
        spec.limits.max_input_edges = 0;
        spec.limits.max_source_scratch = 0;
        let work = RefCell::new(WorkBudget::new(1_000_000));
        let actual = evaluate_filtered(
            &db.snapshot,
            db.frontier().unwrap(),
            &options(),
            q,
            spec,
            &work,
            Scan::Metered,
            |_| Ok(true),
            |_| true,
            |_| panic!("unused property"),
            |_| true,
            || panic!("zero-hop read touched adjacency"),
            |_| true,
        )
        .unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|hit| (hit.id, hit.graph_hops))
                .collect::<Vec<_>>(),
            [(VId(9), Some(0)), (HIGH, Some(0))]
        );
        let mut denied = options();
        denied.policy.max_staging_rows = 0;
        assert!(
            db.beacon_search_graph_indexed_filtered(&c.query(), &denied, q, spec, |_| false)
                .is_err()
        );
        denied = options();
        denied.policy.max_result_rows = 1;
        assert!(matches!(
            db.beacon_search_graph_indexed(&c.query(), &denied, q, spec),
            Err(Error::Index(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: 1
            }))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_only_indexed_authority_masks_sources_and_rechecks_signed_limits() {
    let ((), report) = run_async_under_lab(0xbeac_3804, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), true).await;
        let authority = Authority::new(
            AuthKey::from_seed(3804),
            NS,
            "host-graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 4,
                max_work: 1_000_000,
                max_rows: 4,
            },
        );
        grant.labels = Scope::only([LabelId(1)]);
        grant.relations = Scope::only([R]);
        grant.properties = Scope::only([]);
        let token = authority.issue_at(&grant, 100).unwrap();
        let q = GraphHybridQuery::graph_only(4, 4, 1, 60).unwrap();
        let seeds = [VId(1), VId(90)];
        let mut spec = expansion(&seeds);
        spec.relation = None;
        spec.limits.max_input_edges = 4;
        let actual = db
            .beacon_search_graph_indexed_authorized(
                &c.query(),
                &authority,
                &token,
                "main",
                &options(),
                q,
                spec,
                || 100,
            )
            .unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|hit| (hit.id, hit.graph_hops))
                .collect::<Vec<_>>(),
            [(VId(2), Some(1)), (HIGH, Some(2))]
        );
        grant.limits.max_rows = 1;
        let short = authority.issue_at(&grant, 100).unwrap();
        assert!(
            db.beacon_search_graph_indexed_authorized(
                &c.query(),
                &authority,
                &short,
                "main",
                &options(),
                q,
                spec,
                || 100
            )
            .is_err()
        );
        assert!(
            db.beacon_search_graph_indexed_authorized(
                &c.query(),
                &authority,
                &token,
                "main",
                &options(),
                q,
                spec,
                || 1100
            )
            .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_indexed_graph_only_work_cut_refuses_even_for_empty_output() {
    struct Cut {
        calls: usize,
        stop: usize,
        units: usize,
    }
    impl WorkControl for Cut {
        fn charge(&mut self, units: usize) -> Result<(), BeaconError> {
            self.calls += 1;
            self.units += units;
            if self.calls == self.stop {
                Err(BeaconError::Cancelled)
            } else {
                Ok(())
            }
        }
    }
    let ((), report) = run_async_under_lab(0xbeac_3805, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), false).await;
        let seeds = [VId(1)];
        for k in [0, 1] {
            for accept in [false, true] {
                let q = GraphHybridQuery::graph_only(k, 1, 1, 60).unwrap();
                let run = |work: &RefCell<_>| {
                    evaluate_filtered(
                        &db.snapshot,
                        db.frontier().unwrap(),
                        &options(),
                        q,
                        expansion(&seeds),
                        work,
                        Scan::Metered,
                        |_| Ok(true),
                        |_| true,
                        |_| false,
                        |_| true,
                        || Ok(()),
                        |id| accept && id == HIGH,
                    )
                };
                let work = RefCell::new(Cut {
                    calls: 0,
                    stop: usize::MAX,
                    units: 0,
                });
                let expected = run(&work).unwrap();
                let count = work.into_inner();
                assert_eq!(expected.len(), k * usize::from(accept));
                for stop in 1..=count.calls {
                    let work = RefCell::new(Cut {
                        calls: 0,
                        stop,
                        units: 0,
                    });
                    assert!(
                        matches!(run(&work), Err(BeaconError::Cancelled)),
                        "cut {stop}, k={k}"
                    );
                    assert_eq!(work.into_inner().calls, stop);
                }
                for units in [count.units - 1, count.units] {
                    let work = RefCell::new(WorkBudget::new(units));
                    let result = evaluate_filtered(
                        &db.snapshot,
                        db.frontier().unwrap(),
                        &options(),
                        q,
                        expansion(&seeds),
                        &work,
                        Scan::Metered,
                        |_| Ok(true),
                        |_| true,
                        |_| false,
                        |_| true,
                        || Ok(()),
                        |id| accept && id == HIGH,
                    );
                    if units == count.units {
                        assert_eq!(result.unwrap(), expected);
                    } else {
                        assert!(matches!(result, Err(BeaconError::WorkBudgetExceeded)));
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_hop_indexed_kernel_scratch_uses_the_expansion_source_allowance() {
    let ((), report) = run_async_under_lab(0xbeac_3808, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit(), false).await;
        let options = options();
        let q = GraphHybridQuery::graph_only(1, 1, 100, 60).unwrap();
        let seeds = [VId(1)];
        let mut spec = expansion(&seeds);
        spec.max_hops = 0;
        spec.include_seeds = true;
        spec.limits.max_input_edges = 0;
        spec.limits.max_source_scratch = 0;
        assert!(
            db.beacon_search_graph_indexed_filtered(
                &c.query(),
                &options,
                q,
                spec,
                |_| true,
            )
            .is_err(),
            "zero-hop BFS/ranking state must not bypass the shared scratch cap"
        );
        spec.limits.max_source_scratch = 32;
        let rows = db
            .beacon_search_graph_indexed_filtered(&c.query(), &options, q, spec, |_| true)
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, VId(1));
        assert_eq!(rows[0].graph_hops, Some(0));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
