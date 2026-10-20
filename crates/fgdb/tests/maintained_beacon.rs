//! Registered retrieval follows real durable publication, never caller-fed documents.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, DerivedPublicationStage, MAX_MAINTAINED_BEACON_INDEXES,
    MaintainedBeaconError, MaintainedBeaconIndex, MemVfs, ResidentIndexError, WriteBatch,
    WriteError,
};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{ReadOptions, ReadPolicy, Rows, Search, VectorEncoding};
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, GraphHybridQuery, HnswConfig, TextMatch,
    VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const DOC: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const EMB: PropertyKeyId = PropertyKeyId(3);
const LINKS: RelationId = RelationId(1);
type Options = ReadOptions<PropertyKeyId, LabelId>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn document(id: u128, words: &str, coordinate: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(LINKS);
    batch.create_vertex(
        VId(id),
        vec![DOC],
        vec![(BODY, text(words)), (X, CanonicalScalar::Int(coordinate))],
    );
    batch
}

fn options() -> Options {
    let mut options = Options::text(BODY);
    options.vertex_label = Some(DOC);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}

fn text_query() -> Search<'static> {
    Search::Text {
        query: "graph storage",
        k: 10,
        mode: TextMatch::Any,
    }
}

fn vector_query() -> Search<'static> {
    Search::Vector {
        query: &[0.0],
        k: 10,
        mode: VectorSearch::Exact,
    }
}

fn ids(rows: Rows) -> Vec<VId> {
    match rows {
        Rows::Text(hits) => hits.into_iter().map(|hit| hit.id).collect(),
        Rows::Vector(hits) => hits.into_iter().map(|hit| hit.id).collect(),
        Rows::Hybrid(hits) => hits.into_iter().map(|hit| hit.id).collect(),
    }
}

fn verify_current(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &MaintainedBeaconIndex,
    definition: &Options,
) {
    let pin = db.beacon_index_snapshot(cx, handle).unwrap();
    let view = db.read_session().unwrap();
    assert_eq!(pin.source_sequence(), db.frontier().unwrap());
    for query in [text_query(), vector_query()] {
        let expected = view.beacon_search(cx, definition, query).unwrap();
        assert_eq!(
            pin.search(cx, query, ReadPolicy::default()).unwrap(),
            expected,
        );
        assert_eq!(db.beacon_search(cx, definition, query).unwrap(), expected);
    }
}

fn graph_query() -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0],
            text: "graph",
            k: 10,
            vector_candidates: 10,
            text_candidates: 10,
            vector_mode: VectorSearch::Exact,
            text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 10,
        graph_weight: 1,
    }
}

fn expansion() -> ExpansionSpec<'static, RelationId> {
    ExpansionSpec {
        seeds: &[VId(1)],
        relation: Some(LINKS),
        direction: ExpansionDirection::Outgoing,
        max_hops: 2,
        include_seeds: false,
        limits: ExpansionLimits::default(),
    }
}

#[test]
fn committed_mutations_and_transaction_completion_advance_both_lanes_and_preserve_pins() {
    let ((), report) = run_async_under_lab(0xbeac_4001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 0)).await.unwrap();
        db.write(&commit, document(2, "storage", 5)).await.unwrap();
        let mut excluded = document(3, "graph storage", 9);
        excluded.set_vertex_label(VId(3), DOC, false);
        db.write(&commit, excluded).await.unwrap();
        let mut definition = options();
        let handle = db
            .register_beacon_index(&cx, &definition, ReadPolicy::default())
            .unwrap();
        let old = db.beacon_index_snapshot(&cx, &handle).unwrap();
        let before = old
            .search(&cx, vector_query(), ReadPolicy::default())
            .unwrap();
        assert_eq!(ids(before.clone()), [VId(1), VId(2)]);

        // Registration owns its definition; caller edits cannot redirect it.
        definition.projection.vector = vec![PropertyKeyId(99)];
        definition.vertex_label = Some(LabelId(99));
        let mut change = WriteBatch::new(LINKS);
        change.set_vertex_property(VId(1), BODY, Some(text("storage graph")));
        change.set_vertex_property(VId(1), X, Some(CanonicalScalar::Int(10)));
        change.delete_vertex(VId(2));
        change.set_vertex_label(VId(3), DOC, true);
        let mut txn = db.begin(&contexts.txn()).unwrap();
        txn.write_ordered(&mut db, vec![change, document(4, "graph", 1)])
            .unwrap();
        assert_eq!(
            db.beacon_index_snapshot(&cx, &handle)
                .unwrap()
                .source_sequence(),
            old.source_sequence()
        );
        txn.finish(&mut db, &commit).await.unwrap();

        verify_current(&db, &cx, &handle, &options());
        assert_eq!(
            ids(db
                .search_beacon_index(&cx, &handle, vector_query(), ReadPolicy::default())
                .unwrap()),
            [VId(4), VId(3), VId(1)]
        );
        let status = db.beacon_index_status(&cx, &handle).unwrap();
        assert_eq!(status.source_sequence, status.database_sequence);
        assert_eq!(status.last_refresh.commits, 1);
        assert_eq!(status.last_refresh.touched_vertices, 4);
        assert!(status.failure.is_none());
        assert_eq!(
            old.search(&cx, vector_query(), ReadPolicy::default())
                .unwrap(),
            before
        );

        db.write(&commit, document(5, "storage", 2)).await.unwrap();
        verify_current(&db, &cx, &handle, &options());
        assert_eq!(old.stats().documents, 2);
        assert_eq!(
            db.beacon_index_snapshot(&cx, &handle)
                .unwrap()
                .stats()
                .documents,
            4
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_only_and_noop_commits_advance_graph_source_without_rebuilding_search_lanes() {
    let ((), report) = run_async_under_lab(0xbeac_4002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for id in 1..=3 {
            db.write(&commit, document(id, "graph", id as i64))
                .await
                .unwrap();
        }
        let maintenance = ReadPolicy {
            max_source_scratch: 0,
            max_staging_rows: 0,
            ..ReadPolicy::default()
        };
        let handle = db
            .register_beacon_graph_index(&cx, &options(), maintenance)
            .unwrap();
        let old = db.beacon_index_snapshot(&cx, &handle).unwrap();
        let old_rows = old
            .search_graph(&cx, graph_query(), expansion(), ReadPolicy::default())
            .unwrap();
        assert!(old_rows.iter().all(|hit| hit.graph_hops.is_none()));
        let mut edges = WriteBatch::new(LINKS);
        edges.add_edge(EId(10), VId(1), VId(2), vec![]);
        edges.add_edge(EId(20), VId(2), VId(3), vec![]);
        db.write(&commit, edges).await.unwrap();
        let current = db.beacon_index_snapshot(&cx, &handle).unwrap();
        assert_eq!(current.stats(), old.stats());
        let rows = current
            .search_graph(&cx, graph_query(), expansion(), ReadPolicy::default())
            .unwrap();
        let mut hops: Vec<_> = rows
            .iter()
            .filter_map(|hit| hit.graph_hops.map(|n| (hit.id, n)))
            .collect();
        hops.sort();
        assert_eq!(hops, [(VId(2), 1), (VId(3), 2)]);
        assert_eq!(
            rows,
            db.beacon_search_graph_indexed(&cx, &options(), graph_query(), expansion())
                .unwrap()
        );

        let mut noop = WriteBatch::new(LINKS);
        noop.ensure_vertex(VId(1), vec![], vec![]);
        let seq = db.write(&commit, noop).await.unwrap();
        let status = db.beacon_index_status(&cx, &handle).unwrap();
        assert_eq!(status.source_sequence, seq);
        assert_eq!(status.last_refresh.touched_vertices, 0);
        assert!(status.failure.is_none());
        db.compact(&commit).await.unwrap();
        assert_eq!(
            db.search_beacon_graph_index(
                &cx,
                &handle,
                graph_query(),
                expansion(),
                ReadPolicy::default()
            )
            .unwrap(),
            rows
        );
        assert_eq!(
            old.search_graph(&cx, graph_query(), expansion(), ReadPolicy::default())
                .unwrap(),
            old_rows
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bad_projection_and_budget_failure_fence_only_their_index_until_explicit_rebuild() {
    let ((), report) = run_async_under_lab(0xbeac_4003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 0)).await.unwrap();
        let fragile = db
            .register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let old = db.beacon_index_snapshot(&cx, &fragile).unwrap();
        let text_only = Options::text(BODY);
        let healthy = db
            .register_beacon_index(&cx, &text_only, ReadPolicy::default())
            .unwrap();
        let exhausted = db
            .register_beacon_index(
                &cx,
                &text_only,
                ReadPolicy {
                    max_work_units: 0,
                    ..ReadPolicy::default()
                },
            )
            .unwrap();
        let mut bad = WriteBatch::new(LINKS);
        bad.set_vertex_property(VId(1), BODY, Some(text("storage storage")));
        bad.set_vertex_property(VId(1), X, Some(text("not a vector coordinate")));
        let committed = db.write(&commit, bad).await.unwrap();
        assert_eq!(
            db.frontier().unwrap(),
            committed,
            "derived failure cannot abort a durable write"
        );
        for handle in [&fragile, &exhausted] {
            assert!(matches!(
                db.beacon_index_snapshot(&cx, handle),
                Err(MaintainedBeaconError::Unavailable { .. })
            ));
            let status = db.beacon_index_status(&cx, handle).unwrap();
            assert_eq!(status.source_sequence, old.source_sequence());
            assert_eq!(status.failure.unwrap().at, committed);
        }
        assert_eq!(
            db.search_beacon_index(&cx, &healthy, text_query(), ReadPolicy::default())
                .unwrap(),
            db.beacon_search(&cx, &text_only, text_query()).unwrap()
        );
        assert!(
            db.rebuild_beacon_index(&cx, &fragile, ReadPolicy::default(), ReadPolicy::default())
                .is_err()
        );
        assert_eq!(
            db.beacon_index_status(&cx, &fragile)
                .unwrap()
                .failure
                .unwrap()
                .at,
            committed
        );

        let mut repair = WriteBatch::new(LINKS);
        repair.set_vertex_property(VId(1), X, Some(CanonicalScalar::Int(7)));
        let current = db.write(&commit, repair).await.unwrap();
        assert!(matches!(
            db.beacon_index_snapshot(&cx, &fragile),
            Err(MaintainedBeaconError::Unavailable { .. })
        ));
        for handle in [&fragile, &exhausted] {
            assert_eq!(
                db.rebuild_beacon_index(&cx, handle, ReadPolicy::default(), ReadPolicy::default())
                    .unwrap(),
                current
            );
            assert!(
                db.beacon_index_status(&cx, handle)
                    .unwrap()
                    .failure
                    .is_none()
            );
        }
        verify_current(&db, &cx, &fragile, &options());
        assert_eq!(
            ids(old
                .search(&cx, vector_query(), ReadPolicy::default())
                .unwrap()),
            [VId(1)]
        );
        db.write(&commit, document(2, "graph", 1)).await.unwrap();
        verify_current(&db, &cx, &fragile, &options());
        assert_eq!(
            db.beacon_index_snapshot(&cx, &exhausted)
                .unwrap()
                .source_sequence(),
            db.frontier().unwrap()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn packed_embeddings_follow_commits_without_mutating_retained_ann_topology() {
    let ((), report) = run_async_under_lab(0xbeac_4004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let packed = |value: f32| CanonicalScalar::bytes(value.to_le_bytes().to_vec()).unwrap();
        let mut initial = document(1, "graph", 0);
        initial.set_vertex_property(VId(1), EMB, Some(packed(0.0)));
        db.write(&commit, initial).await.unwrap();
        let mut definition = options();
        definition.projection.vector = vec![EMB];
        definition.projection.encoding = VectorEncoding::PackedF32Le;
        let handle = db
            .register_beacon_index(&cx, &definition, ReadPolicy::default())
            .unwrap();
        let old = db.beacon_index_snapshot(&cx, &handle).unwrap();
        let approximate = Search::Vector {
            query: &[0.0],
            k: 10,
            mode: VectorSearch::Approximate { ef_search: 32 },
        };
        let old_ann = old.search(&cx, approximate, ReadPolicy::default()).unwrap();
        let mut changed = document(2, "storage", 0);
        changed.set_vertex_property(VId(1), EMB, Some(packed(8.0)));
        changed.set_vertex_property(VId(2), EMB, Some(packed(1.0)));
        db.write(&commit, changed).await.unwrap();
        verify_current(&db, &cx, &handle, &definition);
        assert_eq!(
            ids(db
                .search_beacon_index(&cx, &handle, vector_query(), ReadPolicy::default())
                .unwrap()),
            [VId(2), VId(1)]
        );
        assert_eq!(
            old.search(&cx, approximate, ReadPolicy::default()).unwrap(),
            old_ann
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn registration_is_bounded_current_and_owner_scoped_and_removed_slots_do_not_alias() {
    let ((), report) = run_async_under_lab(0xbeac_4005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, document(1, "graph", 0)).await.unwrap();
        let mut historical = Options::text(BODY);
        historical.as_of = Some(fgdb_types::CommitSeq::ORIGIN);
        assert!(matches!(
            db.register_beacon_index(&cx, &historical, ReadPolicy::default()),
            Err(MaintainedBeaconError::NotCurrent { .. })
        ));
        let definition = Options::text(BODY);
        let mut handles = Vec::new();
        for _ in 0..MAX_MAINTAINED_BEACON_INDEXES {
            handles.push(
                db.register_beacon_index(&cx, &definition, ReadPolicy::default())
                    .unwrap(),
            );
        }
        assert!(
            matches!(db.register_beacon_index(&cx, &definition, ReadPolicy::default()), Err(MaintainedBeaconError::RegistrationLimit { limit }) if limit == MAX_MAINTAINED_BEACON_INDEXES)
        );
        let removed = handles[0].clone();
        let old = db.beacon_index_snapshot(&cx, &removed).unwrap();
        db.unregister_beacon_index(&cx, &removed).unwrap();
        let replacement = db
            .register_beacon_index(&cx, &definition, ReadPolicy::default())
            .unwrap();
        assert!(matches!(
            db.beacon_index_snapshot(&cx, &removed),
            Err(MaintainedBeaconError::UnknownHandle)
        ));
        assert!(matches!(
            db.unregister_beacon_index(&cx, &removed),
            Err(MaintainedBeaconError::UnknownHandle)
        ));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.beacon_index_snapshot(&cx, &replacement),
            Err(MaintainedBeaconError::ForeignHandle)
        ));
        for handle in &handles[1..] {
            db.unregister_beacon_index(&cx, handle).unwrap();
        }
        db.write(&commit, document(2, "storage", 1)).await.unwrap();
        assert_eq!(
            db.beacon_index_snapshot(&cx, &replacement)
                .unwrap()
                .stats()
                .documents,
            2
        );
        assert_eq!(old.stats().documents, 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ambiguous_publication_fences_current_search_and_recovery_does_not_transfer_registrations() {
    let ((), report) = run_async_under_lab(0xbeac_4006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        db.write(&commit, document(1, "graph", 0)).await.unwrap();
        let handle = db
            .register_beacon_graph_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let old = db.beacon_index_snapshot(&cx, &handle).unwrap();
        assert!(matches!(
            db.write_with_publication_failure(
                &commit,
                document(2, "storage", 1),
                DerivedPublicationStage::PublishPartitionRoot
            )
            .await,
            Err(WriteError::CommittedNeedsRecovery { .. }),
        ));
        assert!(matches!(
            db.beacon_index_snapshot(&cx, &handle),
            Err(MaintainedBeaconError::Resident(ResidentIndexError::Source(
                _
            )))
        ));
        assert!(
            db.rebuild_beacon_index(&cx, &handle, ReadPolicy::default(), ReadPolicy::default())
                .is_err()
        );
        assert_eq!(old.stats().documents, 1);
        drop(db);
        let mut reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            reopened.beacon_index_snapshot(&cx, &handle),
            Err(MaintainedBeaconError::ForeignHandle)
        ));
        let fresh = reopened
            .register_beacon_graph_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        verify_current(&reopened, &cx, &fresh, &options());
        assert_eq!(
            reopened
                .beacon_index_snapshot(&cx, &fresh)
                .unwrap()
                .stats()
                .documents,
            2
        );
        assert_eq!(
            ids(old
                .search(&cx, vector_query(), ReadPolicy::default())
                .unwrap()),
            [VId(1)]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn call_hybrid_search_and_database_beacon_search_reuse_maintained_index_across_commits() {
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
    let symbols = |kind: GraphSymbolKind, name: &str| match (kind, name) {
        (GraphSymbolKind::Label, "Doc") => Some(GraphSymbol::Label(DOC)),
        (GraphSymbolKind::Relation, "LINKS") => Some(GraphSymbol::Relation(LINKS)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        _ => None,
    };
    let ((), report) = run_async_under_lab(0xbeac_4007, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed_batch = WriteBatch::new(LINKS);
        for id in 1..=64_u128 {
            seed_batch.create_vertex(
                VId(id),
                vec![DOC],
                vec![
                    (
                        BODY,
                        text(if id <= 4 {
                            "graph storage engine"
                        } else {
                            "background filler document"
                        }),
                    ),
                    (X, CanonicalScalar::Int(id as i64)),
                ],
            );
        }
        seed_batch.add_edge(EId(1), VId(1), VId(2), vec![]);
        seed_batch.add_edge(EId(2), VId(2), VId(3), vec![]);
        db.write(&commit, seed_batch).await.unwrap();

        let statement_two_lane = "CALL hybrid.search(\
            label => 'Doc', text => 'graph', text_property => 'body', \
            vector => [1.0], vector_properties => ['x'], \
            eligible => [2, 3, 4], k => 2, candidates => 4\
        ) YIELD node, score, vector_rank, text_rank \
        RETURN node, score, vector_rank, text_rank";
        let statement_three_lane = "CALL hybrid.search(\
            label => 'Doc', text => 'graph', text_property => 'body', \
            vector => [1.0], vector_properties => ['x'], \
            seeds => [1], relation => 'LINKS', max_hops => 2, \
            eligible => [2, 3], k => 2, candidates => 4, graph_candidates => 4\
        ) YIELD node, score, graph_hops \
        RETURN node, score, graph_hops";

        let generous = GqlQueryPolicy::new(1_000, 100, 1_000_000, 100_000);
        // Tight policy: zero snapshot_records (which forbids any one-shot
        // corpus scan/staging) and bounded search work budget.
        let tight = GqlQueryPolicy::new(0, 10, 2_000, 64);

        // Before registration, Database::query must scan all 64 vertices and fails `tight`.
        assert!(
            db.query(&cx, statement_two_lane, &GqlParameters::new(), symbols, tight)
                .is_err()
        );

        let handle = db
            .register_beacon_graph_index(&cx, &options(), ReadPolicy::default())
            .unwrap();

        // After registration, Database::query reuses the maintained index and fits `tight`,
        // while the un-cached EmbeddedReadView still requires the full scan and fails `tight`.
        let view = db.read_session().unwrap();
        assert!(
            view.query(&cx, statement_two_lane, &GqlParameters::new(), symbols, tight)
                .is_err()
        );
        let expected_two_lane = view
            .query(
                &cx,
                statement_two_lane,
                &GqlParameters::new(),
                symbols,
                generous,
            )
            .unwrap();
        let actual_two_lane = db
            .query(&cx, statement_two_lane, &GqlParameters::new(), symbols, tight)
            .unwrap();
        assert_eq!(actual_two_lane, expected_two_lane);

        let expected_three_lane = view
            .query(
                &cx,
                statement_three_lane,
                &GqlParameters::new(),
                symbols,
                generous,
            )
            .unwrap();
        let actual_three_lane = db
            .query(
                &cx,
                statement_three_lane,
                &GqlParameters::new(),
                symbols,
                tight,
            )
            .unwrap();
        assert_eq!(actual_three_lane, expected_three_lane);

        // Mutate vertex 4 and add edge 2 -> 4, advancing the maintained index.
        let mut update = WriteBatch::new(LINKS);
        update.set_vertex_property(VId(4), X, Some(CanonicalScalar::Int(1)));
        update.add_edge(EId(3), VId(2), VId(4), vec![]);
        db.write(&commit, update).await.unwrap();

        let view_after = db.read_session().unwrap();
        let expected_after = view_after
            .query(
                &cx,
                statement_two_lane,
                &GqlParameters::new(),
                symbols,
                generous,
            )
            .unwrap();
        let actual_after = db
            .query(&cx, statement_two_lane, &GqlParameters::new(), symbols, tight)
            .unwrap();
        assert_eq!(actual_after, expected_after);
        assert_ne!(actual_after, actual_two_lane);

        // Database::beacon_search_filtered and search_beacon_index_filtered also reuse the pin.
        let mut tight_options = options();
        tight_options.policy.max_staging_rows = 0;
        tight_options.policy.max_work_units = 500;
        assert!(
            view_after
                .beacon_search_filtered(&cx, &tight_options, vector_query(), |id| id == VId(4))
                .is_err()
        );
        let filtered_rows = db
            .beacon_search_filtered(&cx, &tight_options, vector_query(), |id| id == VId(4))
            .unwrap();
        assert_eq!(ids(filtered_rows.clone()), [VId(4)]);
        assert_eq!(
            db.search_beacon_index_filtered(
                &cx,
                &handle,
                vector_query(),
                tight_options.policy,
                |id| id == VId(4),
            )
            .unwrap(),
            filtered_rows
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
