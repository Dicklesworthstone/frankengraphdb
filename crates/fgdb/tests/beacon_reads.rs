//! Real embedded writes and historical reads; no synthetic index provenance.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_beacon::read::{ReadError, ReadOptions, Rows, Search};
use fgdb_beacon::{BeaconError, BeaconIndex, DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, IndexDocument, TextMatch, VectorSearch, WorkBudget};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

type Options = ReadOptions<PropertyKeyId, LabelId>;
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const L: LabelId = LabelId(1);
fn keys() -> DatabaseKeys { DatabaseKeys::new([0x47; 32], DatabaseSecurityNamespaceId([0x53; 32]), [0x61; 32]) }
fn text(s: &str) -> CanonicalScalar { CanonicalScalar::ucs_basic_text(s).unwrap() }
fn options() -> Options {
    let mut options = Options::text(T);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn searches() -> [Search<'static>; 3] {
    [
        Search::Text { query: "red", k: 3, mode: TextMatch::Any },
        Search::Vector { query: &[0.0], k: 3, mode: VectorSearch::Exact },
        Search::Hybrid(ExactHybridQuery {
            vector: &[0.0], text: "red", k: 3, vector_candidates: 3, text_candidates: 3,
            vector_mode: VectorSearch::Exact, text_mode: TextMatch::Any, profile: ExactRrfProfile::default(),
        }),
    ]
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, label, word, x) in [(1, L, "red", 0), (2, L, "red red", 10), (3, L, "blue", 20), (4, LabelId(99), "red", 0)] {
        batch.create_vertex(VId(id), vec![label], vec![(T, text(word)), (X, CanonicalScalar::Int(x))]);
    }
    batch
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    db.write(cx, seed()).await.unwrap();
    db
}
fn direct() -> BeaconIndex {
    BeaconIndex::build(options().index, [
        IndexDocument { id: VId(1), vector: Some(vec![0.0]), text: Some("red".into()) },
        IndexDocument { id: VId(2), vector: Some(vec![10.0]), text: Some("red red".into()) },
        IndexDocument { id: VId(3), vector: Some(vec![20.0]), text: Some("blue".into()) },
    ], &mut WorkBudget::new(1_000_000)).unwrap()
}

#[test]
fn embedded_text_vector_and_hybrid_equal_the_native_projected_corpus() {
    let ((), report) = run_async_under_lab(0xbeac_1001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        let expected = direct().snapshot();
        for query in searches() {
            let actual = db.beacon_search(&c.query(), &options(), query).unwrap();
            let oracle = query.execute(&expected, &mut WorkBudget::new(1_000_000)).unwrap();
            assert_eq!(actual, oracle);
        }
        let mut approx = searches()[1];
        if let Search::Vector { mode, .. } = &mut approx { *mode = VectorSearch::Approximate { ef_search: 32 }; }
        assert_eq!(db.beacon_search(&c.query(), &options(), approx).unwrap(),
            approx.execute(&expected, &mut WorkBudget::new(1_000_000)).unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn history_updates_deletes_labels_and_pinned_view_survive_writer_drop() {
    let ((), report) = run_async_under_lab(0xbeac_1002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit()).await;
        let old = db.read_session().unwrap();
        let mut historical = options();
        historical.as_of = Some(old.frontier());
        let before: Vec<_> = searches().into_iter().map(|q| db.beacon_search(&c.query(), &options(), q).unwrap()).collect();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), T, Some(text("green")));
        update.set_vertex_property(VId(1), X, Some(CanonicalScalar::Int(30)));
        update.delete_vertex(VId(2));
        update.set_vertex_label(VId(3), L, false);
        update.set_vertex_label(VId(4), L, true);
        db.write(&c.commit(), update).await.unwrap();
        for (q, expected) in searches().into_iter().zip(&before) {
            assert_eq!(db.beacon_search(&c.query(), &historical, q).unwrap(), *expected);
            assert_eq!(old.beacon_search(&c.query(), &options(), q).unwrap(), *expected);
        }
        let Rows::Text(now) = db.beacon_search(&c.query(), &options(), searches()[0]).unwrap() else { panic!() };
        assert_eq!(now.iter().map(|r| r.id).collect::<Vec<_>>(), [VId(4)]);
        let Rows::Vector(now) = db.beacon_search(&c.query(), &options(), searches()[1]).unwrap() else { panic!() };
        assert_eq!(now.iter().map(|r| r.id).collect::<Vec<_>>(), [VId(4), VId(1)]);
        drop(db);
        for (q, expected) in searches().into_iter().zip(before) {
            assert_eq!(old.beacon_search(&c.query(), &options(), q).unwrap(), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scalar_and_frontier_refusals_cannot_fall_back_or_return_a_prefix() {
    let ((), report) = run_async_under_lab(0xbeac_1003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit()).await;
        let mut invalid = options();
        invalid.as_of = Some(CommitSeq(u64::MAX));
        assert!(matches!(db.beacon_search(&c.query(), &invalid, searches()[0]), Err(ReadError::Read(_))));
        let mut bad = WriteBatch::new(RelationId(1));
        bad.set_vertex_property(VId(3), X, Some(CanonicalScalar::Int(16_777_217)));
        db.write(&c.commit(), bad).await.unwrap();
        assert!(matches!(db.beacon_search(&c.query(), &options(), searches()[1]), Err(ReadError::Index(BeaconError::InvalidQuery(_)))));
        // Disabled vector lane is not inspected even when its data is invalid.
        assert!(db.beacon_search(&c.query(), &options(), searches()[0]).is_ok());
        let mut missing = WriteBatch::new(RelationId(1));
        missing.set_vertex_property(VId(3), X, None);
        db.write(&c.commit(), missing).await.unwrap();
        let Rows::Vector(rows) = db.beacon_search(&c.query(), &options(), searches()[1]).unwrap() else { panic!() };
        assert_eq!(rows.len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_staging_result_and_shared_work_limits_are_enforced() {
    let ((), report) = run_async_under_lab(0xbeac_1004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit()).await;
        for (kind, limit) in [(0, 0), (1, 0), (2, 2), (3, 2)] {
            let mut o = options();
            match kind {
                0 => o.policy.max_work_units = limit,
                1 => o.policy.max_source_scratch = limit,
                2 => o.policy.max_staging_rows = limit,
                _ => o.policy.max_result_rows = limit,
            }
            assert!(matches!(db.beacon_search(&c.query(), &o, searches()[0]), Err(ReadError::Index(_))));
        }
        assert!(db.beacon_search(&c.query(), &options(), searches()[2]).is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn chronicle_rebuild_reproduces_search_without_persisting_a_second_index() {
    let ((), report) = run_async_under_lab(0xbeac_1005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let path = std::env::temp_dir().join(format!("fgdb-beacon-rebuild-{}", std::process::id()));
        let mut db = Database::create(&c.commit(), &path, keys()).await.unwrap();
        db.write(&c.commit(), seed()).await.unwrap();
        let expected: Vec<_> = searches().into_iter().map(|q| db.beacon_search(&c.query(), &options(), q).unwrap()).collect();
        drop(db);
        let db = Database::open_rebuilding(&c.commit(), &path, keys()).await.unwrap();
        for (q, rows) in searches().into_iter().zip(expected) {
            assert_eq!(db.beacon_search(&c.query(), &options(), q).unwrap(), rows);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
