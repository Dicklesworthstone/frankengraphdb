use super::*;
use crate::{DatabaseKeys, DatabaseState, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_beacon::expansion::ExpansionLimits;
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::panic::{AssertUnwindSafe, catch_unwind};

const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const TEXT: PropertyKeyId = PropertyKeyId(1);
const VECTOR: PropertyKeyId = PropertyKeyId(2);
const HIGH: VId = VId(u128::MAX);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn options() -> Options {
    let mut options = Options::text(TEXT);
    options.vertex_label = Some(L);
    options.projection.vector = vec![VECTOR];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options.index.max_segments = 1;
    options
}
fn request() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0],
            text: "graph",
            k: 4,
            vector_candidates: 8,
            text_candidates: 8,
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::new(60, 1, 1).unwrap(),
        },
        graph_candidates: 8,
        graph_weight: 100,
    }
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
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    for (id, x) in [(VId(0), 0), (HIGH, 8), (VId(9), 6), (VId(99), 0)] {
        batch.create_vertex(
            id,
            vec![if id == VId(99) { LabelId(2) } else { L }],
            vec![
                (TEXT, CanonicalScalar::ucs_basic_text("graph").unwrap()),
                (VECTOR, CanonicalScalar::Int(x)),
            ],
        );
    }
    // This vertex has neither modality, but is a necessary transit node/hit.
    batch.create_vertex(VId(1), vec![L], vec![]);
    for (eid, src, dst) in [
        (0, VId(0), VId(1)),
        (1, VId(0), VId(1)),
        (2, VId(1), HIGH),
        (3, HIGH, VId(0)),
        (4, VId(1), VId(1)),
        (5, VId(0), VId(99)),
        (6, VId(99), VId(9)),
    ] {
        batch.add_edge(EId(eid), src, dst, vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
fn search(index: &PinnedIndex, cx: &QueryCx) -> Vec<GraphHybridHit> {
    index
        .search_graph(cx, request(), expansion(), ReadPolicy::default())
        .unwrap()
}
fn assert_same_pin(a: &PinnedIndex, b: &PinnedIndex) {
    assert_eq!(a.source_sequence(), b.source_sequence());
    assert_eq!(a.stats(), b.stats());
    assert!(Arc::ptr_eq(
        &a.graph_source.as_ref().unwrap().snapshot,
        &b.graph_source.as_ref().unwrap().snapshot
    ));
}

#[test]
fn reusable_lanes_match_native_fusion_without_staging_or_full_vertex_admission() {
    let ((), report) = run_async_under_lab(0xbeac_5101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        let resident = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        assert!(resident.has_graph_source());
        let pinned = resident.snapshot();
        assert!(pinned.has_graph_source());
        for direction in [
            ExpansionDirection::Outgoing,
            ExpansionDirection::Incoming,
            ExpansionDirection::Undirected,
        ] {
            for hops in 0..=3 {
                let mut e = expansion();
                e.direction = direction;
                e.max_hops = hops;
                for include in [false, true] {
                    e.include_seeds = include;
                    let expected = db
                        .beacon_search_graph_indexed(&c.query(), &options(), request(), e)
                        .unwrap();
                    let policy = ReadPolicy {
                        max_staging_rows: 0,
                        ..ReadPolicy::default()
                    };
                    assert_eq!(
                        pinned
                            .search_graph(&c.query(), request(), e, policy)
                            .unwrap(),
                        expected
                    );
                }
            }
        }
        let expected = search(&pinned, &c.query());
        assert_eq!(
            expected.iter().find(|r| r.id == VId(1)).unwrap().graph_hops,
            Some(1)
        );
        assert_eq!(
            expected.iter().find(|r| r.id == HIGH).unwrap().graph_hops,
            Some(2)
        );
        assert!(expected.iter().all(|r| r.id != VId(99)));
        let mut local = expansion();
        local.limits.max_vertices = 3; // four selected vertices, only three visited
        assert_eq!(
            pinned
                .search_graph(&c.query(), request(), local, ReadPolicy::default())
                .unwrap(),
            expected
        );
        assert!(
            db.beacon_search_graph_indexed(&c.query(), &options(), request(), local)
                .is_err(),
            "the one-shot profile still admits its entire selected directory"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_refresh_never_combines_old_documents_with_a_newer_topology_head() {
    let ((), report) = run_async_under_lab(0xbeac_5102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&c.commit(), vfs.clone(), &path, keys())
            .await
            .unwrap();
        let first = seed(&mut db, &c.commit()).await;
        let mut resident = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        let pinned = resident.snapshot();
        let old = search(&pinned, &c.query());
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(2));
        change.add_edge(EId(7), VId(1), VId(9), vec![]);
        change.set_vertex_property(HIGH, VECTOR, Some(CanonicalScalar::Int(2)));
        let second = db.write(&c.commit(), change).await.unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(9));
        change.set_vertex_label(VId(99), L, true);
        let third = db.write(&c.commit(), change).await.unwrap();
        let mut historical = options();
        historical.as_of = Some(second);
        let expected = db
            .beacon_search_graph_indexed(&c.query(), &historical, request(), expansion())
            .unwrap();
        let report = resident
            .refresh(&c.query(), &db, Some(second), ReadPolicy::default())
            .unwrap();
        assert_eq!((report.from, report.through), (first, second));
        assert_eq!(resident.graph_source.as_ref().unwrap().frontier(), third);
        assert_eq!(search(&resident.snapshot(), &c.query()), expected);
        assert_eq!(search(&pinned, &c.query()), old);
        assert_ne!(expected, old);
        let prepared_at_history = db
            .prepare_beacon_graph_index(&c.query(), &historical)
            .unwrap();
        assert_eq!(
            search(&prepared_at_history.snapshot(), &c.query()),
            expected
        );
        resident
            .refresh(&c.query(), &db, None, ReadPolicy::default())
            .unwrap();
        let live = db
            .beacon_search_graph_indexed(&c.query(), &options(), request(), expansion())
            .unwrap();
        assert_eq!(search(&resident.snapshot(), &c.query()), live);
        let middle = prepared_at_history.snapshot();
        drop(db);
        assert_eq!(search(&pinned, &c.query()), old);
        assert_eq!(search(&middle, &c.query()), expected);
        assert_eq!(search(&resident.snapshot(), &c.query()), live);
        let db = Database::open_with_vfs(&c.commit(), vfs, &path, keys())
            .await
            .unwrap();
        assert!(!resident.belongs_to(&db));
        assert!(matches!(
            resident.refresh(&c.query(), &db, None, ReadPolicy::default()),
            Err(Error::ForeignDatabase)
        ));
        let reopened = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        assert_eq!(search(&reopened.snapshot(), &c.query()), live);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_only_refresh_changes_graph_ranks_without_document_rebuild() {
    let ((), report) = run_async_under_lab(0xbeac_5103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        let mut resident = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        let old = resident.snapshot();
        let old_rows = search(&old, &c.query());
        let mut edges = WriteBatch::new(RelationId(2));
        edges.add_edge(EId(8), VId(0), VId(9), vec![]);
        let target = db.write(&c.commit(), edges).await.unwrap();
        let policy = ReadPolicy {
            max_staging_rows: 0,
            ..ReadPolicy::default()
        };
        let report = resident.refresh(&c.query(), &db, None, policy).unwrap();
        assert_eq!(report.touched_vertices, 0);
        assert_eq!(resident.source_sequence(), target);
        assert_eq!(resident.snapshot().stats(), old.stats());
        let mut all = expansion();
        all.relation = None;
        let actual = resident
            .search_graph(&c.query(), request(), all, policy)
            .unwrap();
        assert_eq!(
            actual,
            db.beacon_search_graph_indexed(&c.query(), &options(), request(), all)
                .unwrap()
        );
        assert_eq!(
            actual.iter().find(|r| r.id == VId(9)).unwrap().graph_hops,
            Some(1)
        );
        assert_eq!(search(&old, &c.query()), old_rows);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[derive(Default)]
struct Cut {
    calls: usize,
    stop: Option<usize>,
    unwind: bool,
}
impl WorkControl for Cut {
    fn charge(&mut self, _: usize) -> Result<(), BeaconError> {
        self.calls += 1;
        if self.stop == Some(self.calls) {
            assert!(!self.unwind, "injected coherent graph generation unwind");
            return Err(BeaconError::Cancelled);
        }
        Ok(())
    }
}

#[test]
fn every_refresh_refusal_and_unwind_retains_the_old_three_lane_generation() {
    let ((), report) = run_async_under_lab(0xbeac_5104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        let original = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        let old = original.snapshot();
        let expected_old = search(&old, &c.query());
        let mut change = WriteBatch::new(R);
        change.delete_edge(EId(2));
        change.add_edge(EId(7), VId(1), VId(9), vec![]);
        change.set_vertex_property(HIGH, VECTOR, Some(CanonicalScalar::Int(1)));
        let target = db.write(&c.commit(), change).await.unwrap();
        let expected_new = db
            .beacon_search_graph_indexed(&c.query(), &options(), request(), expansion())
            .unwrap();
        let mut successful = original.clone();
        let work = RefCell::new(Cut::default());
        successful
            .refresh_with_work(&c.query(), &db, None, ReadPolicy::default(), &work)
            .unwrap();
        let calls = work.into_inner().calls;
        assert_eq!(search(&successful.snapshot(), &c.query()), expected_new);
        for stop in 1..=calls {
            let mut candidate = original.clone();
            let work = RefCell::new(Cut {
                stop: Some(stop),
                ..Cut::default()
            });
            assert!(
                matches!(
                    candidate.refresh_with_work(
                        &c.query(),
                        &db,
                        None,
                        ReadPolicy::default(),
                        &work
                    ),
                    Err(Error::Index(BeaconError::Cancelled))
                ),
                "cut {stop}"
            );
            assert_eq!(work.borrow().calls, stop);
            assert_same_pin(&candidate.snapshot(), &old);
            assert_eq!(search(&candidate.snapshot(), &c.query()), expected_old);
        }
        for stop in [1, calls / 2, calls - 1, calls] {
            let mut candidate = original.clone();
            let work = RefCell::new(Cut {
                stop: Some(stop),
                unwind: true,
                calls: 0,
            });
            assert!(
                catch_unwind(AssertUnwindSafe(|| candidate.refresh_with_work(
                    &c.query(),
                    &db,
                    None,
                    ReadPolicy::default(),
                    &work
                )))
                .is_err()
            );
            assert_same_pin(&candidate.snapshot(), &old);
            assert_eq!(search(&candidate.snapshot(), &c.query()), expected_old);
            candidate
                .refresh(&c.query(), &db, None, ReadPolicy::default())
                .unwrap();
            assert_eq!(candidate.source_sequence(), target);
            assert_eq!(search(&candidate.snapshot(), &c.query()), expected_new);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_search_control_boundary_refuses_without_mutating_or_poisoning_the_pin() {
    let ((), report) = run_async_under_lab(0xbeac_5105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        let resident = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        let pin = resident.snapshot();
        let work = RefCell::new(Cut::default());
        let expected = pin
            .search_graph_with_work(
                &c.query(),
                request(),
                expansion(),
                ReadPolicy::default(),
                &work,
            )
            .unwrap();
        let calls = work.into_inner().calls;
        assert!(calls > 10);
        for stop in 1..=calls {
            let work = RefCell::new(Cut {
                stop: Some(stop),
                ..Cut::default()
            });
            assert!(
                matches!(
                    pin.search_graph_with_work(
                        &c.query(),
                        request(),
                        expansion(),
                        ReadPolicy::default(),
                        &work
                    ),
                    Err(Error::Index(BeaconError::Cancelled))
                ),
                "cut {stop}"
            );
            assert_eq!(work.borrow().calls, stop);
            assert_same_pin(&pin, &resident.snapshot());
        }
        assert_eq!(search(&pin, &c.query()), expected);
        for dimension in 0..6 {
            let mut e = expansion();
            let mut p = ReadPolicy::default();
            match dimension {
                0 => e.limits.max_vertices = 0,
                1 => e.limits.max_visited_vertices = 0,
                2 => e.limits.max_input_edges = 0,
                3 => e.limits.max_source_scratch = 0,
                4 => p.max_source_scratch = 0,
                _ => p.max_result_rows = 0,
            }
            assert!(matches!(
                pin.search_graph(&c.query(), request(), e, p),
                Err(Error::Index(BeaconError::ResourceLimit { .. }))
            ));
        }
        assert_eq!(search(&pin, &c.query()), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_retention_is_opt_in_and_disabled_lanes_never_consume_graph_inputs() {
    let ((), report) = run_async_under_lab(0xbeac_5106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        let ordinary = db.prepare_beacon_index(&c.query(), &options()).unwrap();
        assert!(!ordinary.has_graph_source());
        let pin = ordinary.snapshot();
        assert!(!pin.has_graph_source());
        assert!(matches!(
            pin.search_graph(&c.query(), request(), expansion(), ReadPolicy::default()),
            Err(Error::Index(BeaconError::Disabled("resident graph source")))
        ));
        let mut q = request();
        q.graph_weight = 0;
        let mut e = expansion();
        e.limits = ExpansionLimits {
            max_vertices: 0,
            max_input_edges: 0,
            max_visited_vertices: 0,
            max_seed_ids: 0,
            max_source_scratch: 0,
        };
        let policy = ReadPolicy {
            max_source_scratch: 0,
            max_staging_rows: 0,
            ..ReadPolicy::default()
        };
        let expected = pin
            .index
            .hybrid_search_graph(q, &[], &mut fgdb_beacon::WorkBudget::new(1_000_000))
            .unwrap();
        assert_eq!(
            pin.search_graph(&c.query(), q, e, policy).unwrap(),
            expected
        );
        let enabled = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        assert_eq!(
            enabled.search_graph(&c.query(), q, e, policy).unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_health_and_history_refusals_cannot_rebind_a_retained_graph() {
    let ((), report) = run_async_under_lab(0xbeac_5107, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut db, &c.commit()).await;
        let mut index = db
            .prepare_beacon_graph_index(&c.query(), &options())
            .unwrap();
        let pin = index.snapshot();
        let expected = search(&pin, &c.query());
        let mut other = Database::open_memory(&c.commit(), keys()).await.unwrap();
        seed(&mut other, &c.commit()).await;
        assert!(matches!(
            index.refresh(&c.query(), &other, None, ReadPolicy::default()),
            Err(Error::ForeignDatabase)
        ));
        assert!(
            index
                .refresh(
                    &c.query(),
                    &db,
                    Some(CommitSeq(u64::MAX)),
                    ReadPolicy::default()
                )
                .is_err()
        );
        assert!(matches!(
            index.refresh(&c.query(), &db, Some(CommitSeq(0)), ReadPolicy::default()),
            Err(Error::BeforeSource { .. })
        ));
        let healthy = db.state;
        db.state = DatabaseState::CommitOutcomeUnknown {
            published_frontier: pin.source_sequence(),
        };
        assert!(matches!(
            db.prepare_beacon_graph_index(&c.query(), &options()),
            Err(Error::Source(ReadError::CommitOutcomeUnknown { .. }))
        ));
        assert!(matches!(
            index.refresh(&c.query(), &db, None, ReadPolicy::default()),
            Err(Error::Source(ReadError::CommitOutcomeUnknown { .. }))
        ));
        assert_same_pin(&index.snapshot(), &pin);
        assert_eq!(search(&pin, &c.query()), expected);
        db.state = healthy;
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(7), VId(0), VId(9), vec![]);
        let second = db.write(&c.commit(), change).await.unwrap();
        let before_retention = Arc::clone(&db.snapshot);
        Arc::make_mut(&mut db.snapshot)
            .delta_index
            .retire_prefix(second)
            .unwrap();
        assert!(matches!(
            index.refresh(&c.query(), &db, None, ReadPolicy::default()),
            Err(Error::Source(ReadError::DeltaCursorRetired { .. }))
        ));
        assert_same_pin(&index.snapshot(), &pin);
        assert_eq!(search(&pin, &c.query()), expected);
        db.snapshot = before_retention;
        index
            .refresh(&c.query(), &db, None, ReadPolicy::default())
            .unwrap();
        assert_eq!(index.source_sequence(), second);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
