//! Durable relation runs retain authoritative origins and feed the existing
//! native, authorized and Prism readers after a cold checkpoint selection.

#![recursion_limit = "256"]

use asupersync::fs::Vfs;
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{
    Database, DatabaseKeys, DatabaseState, DerivedPublicationStage, EmbeddedReadView, MemVfs,
    PreparedNativeRead, QueryResult, ReadError, RebuildError, SealPublicationError,
    SealedProjectionOpenError, SealedReadView, WriteBatch, WriteError,
};
use fgdb_chronicle::commit::COMMIT_LOG_NAME;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_prism::{
    FnxCallSpec, FnxMemoryLimits, FnxParameters, FnxReadOptions, FnxSealedReadError,
    ParallelEdgePolicy,
};
use fgdb_strata::root::runs::decode_root_runs;
use fgdb_strata::store::{BlockStore, RootReadError, RootReadLimits, StoreError};
use fgdb_strata::tiered::sealed::{AdjRunError, OriginBirthOrder, SealedError, SealedLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};

const KEY: [u8; 32] = [0xd8; 32];
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xd9; 32]);
const R: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(99);
const P: PropertyKeyId = PropertyKeyId(1);
const EDGE: EId = EId((1_u128 << 120) | 5);
const BFS: &str = "CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R', \
    parallel => 'collapse') YIELD vertex AS n, distance \
    MATCH (n) RETURN n.p AS p, distance ORDER BY p";
const READS: [&str; 4] = [
    "MATCH (a)-[e:R]->(b) RETURN e.p AS weight, a.p AS source, b.p AS target \
     ORDER BY weight, source, target",
    "MATCH (n:L) RETURN n.p AS p ORDER BY p",
    BFS,
    "MATCH (a)-[e:Other]->(b) RETURN e.p AS weight, b.p AS target ORDER BY target",
];

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(KEY, NS, [0xda; 32])
}

fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Other") => Some(GraphSymbol::Relation(OTHER)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 1_000_000)
}

fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1 << 24,
        max_result_bytes: 1 << 24,
    }
}

fn options(cut: CommitSeq) -> FnxReadOptions {
    let mut options = FnxReadOptions::whole_graph(Some(cut));
    options.selection.relations = Some(R).into();
    options.projection.parallel_edges = ParallelEdgePolicy::CollapseUnit;
    options
}

fn grant(restricted: bool) -> Grant {
    let mut grant = Grant::read_only(
        "main",
        10_000,
        QueryLimits {
            max_nodes: 10_000,
            max_work: 10_000_000,
            max_rows: 10_000,
        },
    );
    grant.labels = if restricted {
        Scope::only([L])
    } else {
        Scope::All
    };
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

async fn fixture(cx: &CommitCx, vfs: &MemVfs) -> (Database<MemVfs>, EmbeddedReadView) {
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut initial = WriteBatch::new(R);
    for id in [1, 2, 3, 4, 99] {
        initial.create_vertex(
            VId(id),
            vec![if id == 99 { HIDDEN } else { L }],
            vec![(P, CanonicalScalar::Int(id as i64))],
        );
    }
    for (eid, src, dst, property) in [
        (EDGE, 1, 2, 7),
        (EId(2), 1, 2, 8),
        (EId(3), 2, 3, 9),
        (EId(4), 2, 2, 10),
        (EId(5), 1, 99, 11),
        (EId(6), 99, 3, 12),
    ] {
        initial.add_edge(
            eid,
            VId(src),
            VId(dst),
            vec![(P, CanonicalScalar::Int(property))],
        );
    }
    assert_eq!(db.write(cx, initial).await.unwrap(), CommitSeq(1));
    let mut other = WriteBatch::new(OTHER);
    other.add_edge(EId(7), VId(3), VId(1), vec![(P, CanonicalScalar::Int(20))]);
    assert_eq!(db.write(cx, other).await.unwrap(), CommitSeq(2));
    let historical = db.read_session().unwrap();

    let mut update = WriteBatch::new(R);
    update.set_edge_property(EDGE, P, Some(CanonicalScalar::Int(70)));
    update.delete_edge(EId(2));
    update.delete_edge(EId(3));
    update.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(200)));
    update.add_edge(EId(8), VId(3), VId(4), vec![(P, CanonicalScalar::Int(30))]);
    assert_eq!(db.write(cx, update).await.unwrap(), CommitSeq(3));
    (db, historical)
}

async fn cold(vfs: &MemVfs, cx: &CommitCx, query: &QueryCx, cut: CommitSeq) -> SealedReadView {
    Database::open_sealed_read_with_vfs(
        cx,
        query,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        Some(cut),
        R,
        SealedLimits::default(),
        RootReadLimits::default(),
    )
    .await
    .unwrap()
}

fn answers(view: &EmbeddedReadView, query: &QueryCx) -> Vec<QueryResult> {
    READS
        .iter()
        .map(|text| {
            view.query(query, text, &GqlParameters::new(), catalog, policy())
                .unwrap()
        })
        .collect()
}

fn sealed_answers(view: &SealedReadView, query: &QueryCx) -> Vec<QueryResult> {
    READS
        .iter()
        .map(|text| {
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(text, &params, catalog).unwrap();
            view.execute_prepared(query, &prepared, &params, policy(), memory())
                .unwrap()
        })
        .collect()
}

#[test]
fn durable_runs_reopen_native_authorized_and_prism_history_without_changing_origins() {
    let ((), report) = run_async_under_lab(0xd870_0001, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, historical) = fixture(&cx, &vfs).await;
        let current = db.read_session().unwrap();
        let source_root = db.partition_root().unwrap();
        let versions = db.element_versions().unwrap().clone();
        db.seal_relation(
            &cx,
            &query,
            R,
            CommitSeq(1),
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        assert_ne!(db.partition_root().unwrap(), source_root);
        assert_eq!(db.element_versions().unwrap(), &versions);
        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let run = store
            .get_published_adj_run(
                &query,
                db.partition_root().unwrap(),
                R,
                CommitSeq(3),
                SealedLimits::default(),
                RootReadLimits::default().max_root_bytes,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.header().scope.source_root, source_root);
        assert_eq!(
            run.origin(EDGE),
            Some(OriginBirthOrder::new(CommitSeq(1), 6, 0, EDGE))
        );
        assert_eq!(
            run.origin(EId(2)),
            Some(OriginBirthOrder::new(CommitSeq(1), 7, 0, EId(2)))
        );
        let mut row = run
            .partition()
            .row(&query, VId(1), R, CommitSeq(3))
            .unwrap();
        let changed = std::iter::from_fn(|| row.next(&query).transpose())
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .find(|edge| edge.entry.eid == EDGE)
            .unwrap();
        assert_eq!(changed.entry.created_at, CommitSeq(3));
        assert_eq!(run.origin(EDGE).unwrap().commit_seq(), CommitSeq(1));
        let live = db
            .prepare_sealed_read(
                &query,
                Some(CommitSeq(3)),
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(sealed_answers(&live, &query), answers(&current, &query));

        // Current-only resealing would fit this budget. Reusing the published
        // run must instead admit its complete retained historical image.
        let current_image = store
            .seal_relation(
                &query,
                db.partition_root().unwrap(),
                CommitSeq(3),
                R,
                SealedLimits::default(),
            )
            .await
            .unwrap();
        let retained_count = run.partition().incidence_count();
        let current_count = current_image.incidence_count();
        assert!(retained_count > current_count);
        let limits = SealedLimits {
            max_incidences: current_count,
            ..SealedLimits::default()
        };
        let error = db
            .prepare_sealed_read(
                &query,
                Some(CommitSeq(3)),
                R,
                limits,
                RootReadLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                FnxSealedReadError::Seal(SealedError::Limit {
                    resource: "incidences",
                    requested,
                    limit,
                }) if requested == retained_count && limit == current_count
            ),
            "{error:?}",
        );
        let current_options = options(CommitSeq(3));
        let error = db
            .prism_sealed_projection_with_source_limits(
                &query,
                current_options.as_of,
                current_options.selection,
                current_options.projection,
                current_options.projection_limits,
                current_options.source_limits,
                limits,
                RootReadLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                FnxSealedReadError::Seal(SealedError::Limit {
                    resource: "incidences",
                    requested,
                    limit,
                }) if requested == retained_count && limit == current_count
            ),
            "{error:?}",
        );
        let measured =
            std::sync::Arc::new(fgdb_types::context::SimulationCheckpointProbe::new(None));
        let observed = query.with_checkpoint_probe(std::sync::Arc::clone(&measured));
        let measured_view = db
            .prepare_sealed_read(
                &observed,
                Some(CommitSeq(3)),
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            sealed_answers(&measured_view, &query),
            answers(&current, &query)
        );
        assert!(measured.calls() > 2);
        let published_root = db.partition_root().unwrap();
        for stop in [1, measured.calls() / 2, measured.calls()] {
            let probe = std::sync::Arc::new(fgdb_types::context::SimulationCheckpointProbe::new(
                Some(stop),
            ));
            let interrupted = query.with_checkpoint_probe(std::sync::Arc::clone(&probe));
            assert!(
                db.prepare_sealed_read(
                    &interrupted,
                    Some(CommitSeq(3)),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .is_err()
            );
            assert_eq!(probe.calls(), stop);
            assert_eq!(db.partition_root().unwrap(), published_root);
            assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        }
        drop(db);
        let mut db = Database::open_with_vfs(&cx, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        assert_eq!(db.element_versions().unwrap(), &versions);

        let authority =
            Authority::new(AuthKey::from_seed(0xd871), NS, "graph", SchemaEpoch(0), 1).unwrap();
        let bfs = FnxCallSpec::bind(
            "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance",
            &FnxParameters::new(),
        )
        .unwrap();
        let mut pinned = Vec::new();
        for (cut, oracle) in [(CommitSeq(2), historical), (CommitSeq(3), current)] {
            let view = cold(&vfs, &cx, &query, cut).await;
            let expected = answers(&oracle, &query);
            assert_eq!(sealed_answers(&view, &query), expected);
            let live = db
                .prepare_sealed_read(
                    &query,
                    Some(cut),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            assert_eq!(sealed_answers(&live, &query), expected);
            for restricted in [false, true] {
                let token = authority.issue_at(&grant(restricted), 100).unwrap();
                let mut authorized = view
                    .authorized_read_session(
                        &query,
                        &authority,
                        &token,
                        "main",
                        catalog,
                        policy(),
                        memory(),
                        || 100,
                    )
                    .unwrap();
                let mut decoded = oracle
                    .authorized_read_session(
                        &query,
                        &authority,
                        &token,
                        "main",
                        catalog,
                        policy(),
                        || 100,
                    )
                    .unwrap();
                let mut live_authorized = live
                    .authorized_read_session(
                        &query,
                        &authority,
                        &token,
                        "main",
                        catalog,
                        policy(),
                        memory(),
                        || 100,
                    )
                    .unwrap();
                for text in READS {
                    let expected = decoded.query(&query, text, &GqlParameters::new()).unwrap();
                    assert_eq!(
                        authorized
                            .query(&query, text, &GqlParameters::new())
                            .unwrap(),
                        expected,
                        "cut={cut:?}, restricted={restricted}, query={text}",
                    );
                    assert_eq!(
                        live_authorized
                            .query(&query, text, &GqlParameters::new())
                            .unwrap(),
                        expected,
                        "live cut={cut:?}, restricted={restricted}, query={text}",
                    );
                }
            }
            let options = options(cut);
            let live_graph = db
                .prism_sealed_projection_with_source_limits(
                    &query,
                    Some(cut),
                    options.selection,
                    options.projection,
                    options.projection_limits,
                    options.source_limits,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            // Equality of answers alone would also admit resealing. Preserve
            // the actual stored image's original root, floor and fingerprint.
            assert_eq!(
                live_graph.unmasked_source().unwrap().anchor(),
                run.partition().anchor(),
            );
            let graph = Database::open_prism_sealed_projection_with_vfs(
                &cx,
                &query,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                options,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
            let sealed = bfs
                .execute_sealed(&query, &graph, options.execution_limits, memory())
                .unwrap();
            assert_eq!(
                bfs.execute_sealed(&query, &live_graph, options.execution_limits, memory())
                    .unwrap()
                    .rows,
                sealed.rows,
            );
            assert_eq!(
                sealed.rows,
                oracle
                    .execute_fnx(&query, &bfs, options)
                    .unwrap()
                    .analytics
                    .rows
            );
            pinned.push((view, expected));
        }

        // A vertex-only commit leaves R's edges untouched and carries R's run
        // forward in V8; an edge commit on R bounds verified_through so the
        // latest cut falls back to Tier D while historical cuts retain Tier R.
        let mut advance = WriteBatch::new(R);
        advance.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(300)));
        assert_eq!(db.write(&cx, advance).await.unwrap(), CommitSeq(4));
        let carried = store
            .get_published_adj_run(
                &query,
                db.partition_root().unwrap(),
                R,
                CommitSeq(4),
                SealedLimits::default(),
                RootReadLimits::default().max_root_bytes,
            )
            .await
            .unwrap()
            .expect("vertex-only commit preserves R's persistent run");
        assert_eq!(carried.header().scope, run.header().scope);
        assert_eq!(
            carried.partition().anchor().verified_through(),
            CommitSeq(4)
        );
        let newest = cold(&vfs, &cx, &query, CommitSeq(4)).await;
        assert_eq!(
            sealed_answers(&newest, &query),
            answers(&db.read_session().unwrap(), &query)
        );
        let mut edge_advance = WriteBatch::new(R);
        edge_advance.add_edge(EId(99), VId(3), VId(1), vec![(P, CanonicalScalar::Int(40))]);
        assert_eq!(db.write(&cx, edge_advance).await.unwrap(), CommitSeq(5));
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    db.partition_root().unwrap(),
                    R,
                    CommitSeq(5),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    db.partition_root().unwrap(),
                    R,
                    CommitSeq(4),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_some()
        );
        let newest_after_edge = cold(&vfs, &cx, &query, CommitSeq(5)).await;
        assert_eq!(
            sealed_answers(&newest_after_edge, &query),
            answers(&db.read_session().unwrap(), &query)
        );
        drop(db);
        for (view, expected) in pinned {
            assert_eq!(sealed_answers(&view, &query), expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_floor_falls_back_to_history_but_corrupt_attached_run_refuses_current_reads() {
    let ((), report) = run_async_under_lab(0xd870_0002, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, historical) = fixture(&cx, &vfs).await;
        let run_id = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(3),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            sealed_answers(&cold(&vfs, &cx, &query, CommitSeq(2)).await, &query),
            answers(&historical, &query),
        );
        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let run_path = store.path(run_id.0);
        let original = vfs.read(&run_path).await.unwrap();
        vfs.write(&run_path, b"corrupt durable adjacency run")
            .await
            .unwrap();
        let error = Database::open_sealed_read_with_vfs(
            &cx,
            &query,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            Some(CommitSeq(3)),
            R,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, SealedProjectionOpenError::Store(_)),
            "{error:?}"
        );
        let error = Database::open_prism_sealed_projection_with_vfs(
            &cx,
            &query,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            options(CommitSeq(3)),
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, SealedProjectionOpenError::Store(_)),
            "{error:?}"
        );
        for reopen in [false, true] {
            if reopen {
                drop(db);
                db = Database::open_with_vfs(&cx, vfs.clone(), vfs.database_dir(), keys())
                    .await
                    .unwrap();
            }
            let error = db
                .prepare_sealed_read(
                    &query,
                    Some(CommitSeq(3)),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, FnxSealedReadError::Seal(SealedError::Store(_))),
                "live native reopen={reopen}: {error:?}",
            );
            let options = options(CommitSeq(3));
            let error = db
                .prism_sealed_projection_with_source_limits(
                    &query,
                    options.as_of,
                    options.selection,
                    options.projection,
                    options.projection_limits,
                    options.source_limits,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, FnxSealedReadError::Seal(SealedError::Store(_))),
                "live Prism reopen={reopen}: {error:?}",
            );
            // This cut is explicitly outside the attachment's coverage. Its
            // retained Tier-D history remains usable even if the run is bad.
            let earlier = db
                .prepare_sealed_read(
                    &query,
                    Some(CommitSeq(2)),
                    R,
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                sealed_answers(&earlier, &query),
                answers(&historical, &query)
            );
        }
        assert_eq!(
            vfs.read(&run_path).await.unwrap(),
            b"corrupt durable adjacency run"
        );
        vfs.write(&run_path, &original).await.unwrap();
        assert_eq!(
            sealed_answers(&cold(&vfs, &cx, &query, CommitSeq(3)).await, &query),
            answers(&db.read_session().unwrap(), &query),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn durable_vertex_directory_still_admits_unselected_edge_history_and_source_limits() {
    let ((), report) = run_async_under_lab(0xd870_0003, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, _) = fixture(&cx, &vfs).await;
        db.seal_relation(
            &cx,
            &query,
            R,
            CommitSeq(1),
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        let limits = RootReadLimits {
            max_incidences: 0,
            ..RootReadLimits::default()
        };
        let error = Database::open_prism_sealed_projection_with_vfs(
            &cx,
            &query,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            options(CommitSeq(3)),
            SealedLimits::default(),
            limits,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                error,
                SealedProjectionOpenError::Source(RootReadError::Limit {
                    resource: "source incidences",
                    limit: 0,
                    ..
                })
            ),
            "{error:?}"
        );
        let error = db
            .prepare_sealed_read(
                &query,
                Some(CommitSeq(3)),
                R,
                SealedLimits::default(),
                limits,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                FnxSealedReadError::Seal(SealedError::Limit {
                    resource: "source incidences",
                    limit: 0,
                    ..
                })
            ),
            "{error:?}",
        );
        let options = options(CommitSeq(3));
        let error = db
            .prism_sealed_projection_with_source_limits(
                &query,
                options.as_of,
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
                SealedLimits::default(),
                limits,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                FnxSealedReadError::Seal(SealedError::Limit {
                    resource: "source incidences",
                    limit: 0,
                    ..
                })
            ),
            "{error:?}",
        );

        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let root = store
            .get_root(&cx, db.partition_root().unwrap())
            .await
            .unwrap();
        let mut unrelated = None;
        for reference in root.blocks {
            let bytes = store
                .get_bytes(&cx, fgdb_strata::DeltaBlockVersion(reference.block_id))
                .await
                .unwrap();
            if fgdb_strata::decode_block(&bytes)
                .unwrap()
                .iter()
                .any(|entry| entry.relation == OTHER)
            {
                unrelated = Some((store.path(reference.block_id), bytes));
                break;
            }
        }
        let (path, original) = unrelated.expect("fixture contains an unselected relation block");
        vfs.write(&path, b"damaged unselected edge history")
            .await
            .unwrap();
        let error = Database::open_prism_sealed_projection_with_vfs(
            &cx,
            &query,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            options,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                error,
                SealedProjectionOpenError::Source(RootReadError::Store(_))
            ),
            "{error:?}"
        );
        let error = db
            .prepare_sealed_read(
                &query,
                Some(CommitSeq(3)),
                R,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, FnxSealedReadError::Seal(SealedError::Store(_))),
            "{error:?}",
        );
        let error = db
            .prism_sealed_projection_with_source_limits(
                &query,
                options.as_of,
                options.selection,
                options.projection,
                options.projection_limits,
                options.source_limits,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, FnxSealedReadError::Seal(SealedError::Store(_))),
            "{error:?}",
        );
        assert_eq!(
            vfs.read(&path).await.unwrap(),
            b"damaged unselected edge history"
        );
        vfs.write(&path, &original).await.unwrap();
        Database::open_prism_sealed_projection_with_vfs(
            &cx,
            &query,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            options,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn seal_source_image_and_envelope_refusals_preserve_the_healthy_generation() {
    let ((), report) = run_async_under_lab(0xd870_0004, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, _) = fixture(&cx, &vfs).await;
        let root = db.partition_root().unwrap();
        let manifest = db.manifest().unwrap();
        let expected = answers(&db.read_session().unwrap(), &query);
        let versions = db.element_versions().unwrap().clone();
        let slot_path = vfs.database_dir().join("manifest.root");
        let slot = vfs.read(&slot_path).await.unwrap();
        let markers = vfs
            .read(&vfs.database_dir().join(COMMIT_LOG_NAME))
            .await
            .unwrap();
        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let partition = store
            .seal_relation(&query, root, CommitSeq(1), R, SealedLimits::default())
            .await
            .unwrap();
        let image_bytes = partition
            .encode(&query, SealedLimits::default())
            .unwrap()
            .len();
        drop(partition);

        let cases = [
            (
                SealedLimits::default(),
                RootReadLimits {
                    max_source_bytes: 0,
                    ..RootReadLimits::default()
                },
            ),
            (
                SealedLimits {
                    max_rows: 0,
                    ..SealedLimits::default()
                },
                RootReadLimits::default(),
            ),
            (
                SealedLimits {
                    max_incidences: 1,
                    ..SealedLimits::default()
                },
                RootReadLimits::default(),
            ),
            // The exact FGSI image fits, but its durable origin/envelope bytes
            // must also fit the same total run-byte allowance.
            (
                SealedLimits {
                    max_image_bytes: image_bytes,
                    ..SealedLimits::default()
                },
                RootReadLimits::default(),
            ),
        ];
        for (case, (sealing, source)) in cases.into_iter().enumerate() {
            let error = db
                .seal_relation(&cx, &query, R, CommitSeq(1), sealing, source)
                .await
                .unwrap_err();
            if case == 3 {
                let SealPublicationError::Publication(RebuildError::Store(
                    StoreError::MalformedAdjRun(error),
                )) = error
                else {
                    panic!("expected complete run byte refusal, got {error:?}");
                };
                assert!(matches!(
                    *error,
                    AdjRunError::Sealed(SealedError::Limit {
                        resource: "durable run bytes",
                        ..
                    })
                ));
            } else {
                assert!(
                    matches!(
                        error,
                        SealPublicationError::Source(SealedError::Limit { .. })
                    ),
                    "{error:?}"
                );
            }
            assert_eq!(
                db.state(),
                DatabaseState::Healthy {
                    published_frontier: CommitSeq(3)
                }
            );
            assert_eq!(db.frontier().unwrap(), CommitSeq(3));
            assert_eq!(db.partition_root().unwrap(), root);
            assert_eq!(db.manifest().unwrap(), manifest);
            assert_eq!(db.element_versions().unwrap(), &versions);
            assert_eq!(vfs.read(&slot_path).await.unwrap(), slot);
            assert_eq!(
                vfs.read(&vfs.database_dir().join(COMMIT_LOG_NAME))
                    .await
                    .unwrap(),
                markers
            );
            assert_eq!(answers(&db.read_session().unwrap(), &query), expected);
        }
        db.seal_relation(
            &cx,
            &query,
            R,
            CommitSeq(1),
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            sealed_answers(&cold(&vfs, &cx, &query, CommitSeq(3)).await, &query),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn relation_republication_preserves_other_attachments_and_one_flattened_base() {
    let ((), report) = run_async_under_lab(0xd870_0005, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, _) = fixture(&cx, &vfs).await;
        let base = db.partition_root().unwrap();
        let markers = vfs
            .read(&vfs.database_dir().join(COMMIT_LOG_NAME))
            .await
            .unwrap();
        let first = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(1),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let second = db
            .seal_relation(
                &cx,
                &query,
                OTHER,
                CommitSeq(2),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let combined = db.partition_root().unwrap();
        let repeated = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(1),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(repeated, first);
        assert_eq!(db.partition_root().unwrap(), combined);
        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let bytes = store.get_root_bytes(&cx, combined).await.unwrap();
        let membership = decode_root_runs(&bytes).unwrap().unwrap();
        assert_eq!(
            membership
                .runs
                .iter()
                .map(|run| (run.relation, run.run, run.floor))
                .collect::<Vec<_>>(),
            vec![(R, first.0, CommitSeq(1)), (OTHER, second.0, CommitSeq(2))]
        );
        assert!(decode_root_runs(membership.base_frame).unwrap().is_none());
        assert_eq!(
            membership.base_frame,
            store.get_root_bytes(&cx, base).await.unwrap()
        );
        assert_eq!(
            store
                .get_root_run_source(&cx, combined, RootReadLimits::default().max_root_bytes)
                .await
                .unwrap(),
            base
        );

        let replacement = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(3),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_ne!(replacement, first);
        let bytes = store
            .get_root_bytes(&cx, db.partition_root().unwrap())
            .await
            .unwrap();
        let membership = decode_root_runs(&bytes).unwrap().unwrap();
        assert_eq!(
            membership
                .runs
                .iter()
                .map(|run| (run.relation, run.run, run.floor))
                .collect::<Vec<_>>(),
            vec![
                (R, replacement.0, CommitSeq(3)),
                (OTHER, second.0, CommitSeq(2))
            ]
        );
        assert!(decode_root_runs(membership.base_frame).unwrap().is_none());
        for relation in [R, OTHER] {
            let run = store
                .get_published_adj_run(
                    &query,
                    db.partition_root().unwrap(),
                    relation,
                    CommitSeq(3),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .unwrap();
            assert_eq!(run.header().scope.source_root, base);
        }
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        assert_eq!(
            vfs.read(&vfs.database_dir().join(COMMIT_LOG_NAME))
                .await
                .unwrap(),
            markers
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_failed_seal_root_slot_publication_fences_the_handle_until_authoritative_recovery() {
    let ((), report) = run_async_under_lab(0xd870_0006, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, _) = fixture(&cx, &vfs).await;
        let pinned = db.read_session().unwrap();
        let expected = answers(&pinned, &query);
        let slot_path = vfs.database_dir().join("manifest.root");
        let slot = vfs.read(&slot_path).await.unwrap();
        let markers = vfs
            .read(&vfs.database_dir().join(COMMIT_LOG_NAME))
            .await
            .unwrap();
        // The real MemVfs/RootStore publication path sees external slot loss.
        // Run and manifest staging can complete, but no slot evidence exists.
        vfs.write(&slot_path, b"torn slot").await.unwrap();
        let error = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(1),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                error,
                SealPublicationError::Publication(RebuildError::Slot(
                    fgdb_chronicle::StoreError::MalformedFile { .. }
                ))
            ),
            "{error:?}"
        );
        let DatabaseState::NeedsAuthoritativeRecovery(recovery) = db.state() else {
            panic!("failed root publication must fence the handle");
        };
        assert_eq!(
            recovery.failed_stage,
            DerivedPublicationStage::PublishRootSlot
        );
        assert_eq!(recovery.durable_frontier, CommitSeq(3));
        assert_eq!(recovery.published_frontier, CommitSeq(3));
        assert!(matches!(db.frontier(), Err(ReadError::RecoveryRequired(_))));
        let mut refused = WriteBatch::new(R);
        refused.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(999)));
        assert!(matches!(
            db.write(&cx, refused).await,
            Err(WriteError::RecoveryRequired(_))
        ));
        assert_eq!(answers(&pinned, &query), expected);
        assert_eq!(
            vfs.read(&vfs.database_dir().join(COMMIT_LOG_NAME))
                .await
                .unwrap(),
            markers
        );
        vfs.write(&slot_path, &slot).await.unwrap();
        let mut db = db.recover_authoritatively(&cx).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(3));
        assert_eq!(answers(&db.read_session().unwrap(), &query), expected);
        db.seal_relation(
            &cx,
            &query,
            R,
            CommitSeq(1),
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            sealed_answers(&cold(&vfs, &cx, &query, CommitSeq(3)).await, &query),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn scrub_rechecks_admitted_runs_and_fences_held_and_reopened_writers() {
    let ((), report) = run_async_under_lab(0xd870_0007, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        for reopen in [false, true] {
            for damage_parent in [false, true] {
                let vfs = MemVfs::new().unwrap();
                let (mut db, _) = fixture(&cx, &vfs).await;
                let run_id = db
                    .seal_relation(
                        &cx,
                        &query,
                        R,
                        CommitSeq(1),
                        SealedLimits::default(),
                        RootReadLimits::default(),
                    )
                    .await
                    .unwrap();
                if reopen {
                    drop(db);
                    db = Database::open_with_vfs(&cx, vfs.clone(), vfs.database_dir(), keys())
                        .await
                        .unwrap();
                }
                let root = db.partition_root().unwrap();
                let pinned = db.read_session().unwrap();
                let expected = answers(&pinned, &query);
                let store = BlockStore::open_existing_for_read_with_vfs(
                    &cx,
                    vfs.clone(),
                    vfs.database_dir(),
                    KEY,
                    NS,
                )
                .await
                .unwrap();
                // Both the selected metadata and the payload have already been
                // admitted before storage changes. The sweep must reread them.
                assert!(
                    store
                        .get_published_adj_run(
                            &query,
                            root,
                            R,
                            CommitSeq(3),
                            SealedLimits::default(),
                            RootReadLimits::default().max_root_bytes,
                        )
                        .await
                        .unwrap()
                        .is_some()
                );
                let run_path = store.path(run_id.0);
                let mut damaged = vfs.read(&run_path).await.unwrap();
                let middle = damaged.len() / 2;
                damaged[middle] ^= 0x80;
                vfs.write(&run_path, &damaged).await.unwrap();
                if damage_parent {
                    let root_path = store.path(root.0);
                    let mut damaged_root = vfs.read(&root_path).await.unwrap();
                    let middle = damaged_root.len() / 2;
                    damaged_root[middle] ^= 0x80;
                    vfs.write(&root_path, &damaged_root).await.unwrap();
                }

                let summary = db.scrub(&cx).await.unwrap();
                assert!(summary.lost.is_empty());
                assert_eq!(summary.block_lost.len(), 1);
                assert_eq!(summary.block_lost[0].object_id, run_id.0);
                assert!(!summary.block_clean.contains(&run_id.0));
                if damage_parent {
                    assert_eq!(summary.metadata_lost.len(), 1);
                    assert_eq!(summary.metadata_lost[0].object_id, root.0);
                } else {
                    assert!(summary.metadata_lost.is_empty());
                }
                assert!(matches!(
                    db.state(),
                    DatabaseState::NeedsAuthoritativeRecovery(_)
                ));
                assert!(matches!(db.frontier(), Err(ReadError::RecoveryRequired(_))));
                assert!(matches!(
                    db.read_session(),
                    Err(ReadError::RecoveryRequired(_))
                ));
                assert_eq!(answers(&pinned, &query), expected);
                assert_eq!(vfs.read(&run_path).await.unwrap(), damaged);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_seal_commits_persist_runs_across_reopen_other_relations_and_scrub() {
    let ((), report) = run_async_under_lab(0xd870_0008, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, _) = fixture(&cx, &vfs).await;
        let base_3 = db.partition_root().unwrap();
        let r_run_id = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(1),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();

        // Commit 4 mutates OTHER and vertex properties; R's run must persist in V8.
        let mut c4 = WriteBatch::new(OTHER);
        c4.add_edge(
            EId(50),
            VId(1),
            VId(4),
            vec![(P, CanonicalScalar::Int(500))],
        );
        c4.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(101)));
        assert_eq!(db.write(&cx, c4).await.unwrap(), CommitSeq(4));

        // Reopen writable database from V8 checkpoint and write Commit 5 (vertex creation).
        drop(db);
        let mut db = Database::open_with_vfs(&cx, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut c5 = WriteBatch::new(OTHER);
        c5.create_vertex(VId(5), vec![L], vec![(P, CanonicalScalar::Int(5))]);
        assert_eq!(db.write(&cx, c5).await.unwrap(), CommitSeq(5));

        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let root_5 = db.partition_root().unwrap();
        let root_5_bytes = store.get_root_bytes(&cx, root_5).await.unwrap();
        assert_eq!(
            u16::from_be_bytes([root_5_bytes[4], root_5_bytes[5]]),
            fgdb_strata::root::runs::ROOT_FORMAT_V8
        );
        let r_at_5 = store
            .get_published_adj_run(
                &query,
                root_5,
                R,
                CommitSeq(5),
                SealedLimits::default(),
                RootReadLimits::default().max_root_bytes,
            )
            .await
            .unwrap()
            .expect("R run persists across post-seal commits and reopen");
        assert_eq!(r_at_5.header().scope.source_root, base_3);
        assert_eq!(r_at_5.header().scope.publication, CommitSeq(3));
        assert_eq!(r_at_5.partition().anchor().verified_through(), CommitSeq(5));
        assert_eq!(
            r_at_5.partition().anchor().admitted_envelope_root(),
            Some(root_5)
        );

        // Live and cold Prism projections at Commit 5 must consume the persistent run directly.
        let opts_5 = options(CommitSeq(5));
        let live_proj = db
            .prism_sealed_projection_with_source_limits(
                &query,
                Some(CommitSeq(5)),
                opts_5.selection,
                opts_5.projection,
                opts_5.projection_limits,
                opts_5.source_limits,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            live_proj
                .unmasked_source()
                .unwrap()
                .anchor()
                .scope()
                .source_root,
            base_3
        );
        let cold_proj = Database::open_prism_sealed_projection_with_vfs(
            &cx,
            &query,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            opts_5,
            SealedLimits::default(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            cold_proj
                .unmasked_source()
                .unwrap()
                .anchor()
                .scope()
                .source_root,
            base_3
        );

        // Seal OTHER at Commit 5 while R remains carried from Commit 3.
        let other_run_id = db
            .seal_relation(
                &cx,
                &query,
                OTHER,
                CommitSeq(1),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        assert_ne!(other_run_id, r_run_id);

        // Commit 6 mutates R: R stays verified_through Commit 5 while OTHER advances to Commit 6.
        let mut c6 = WriteBatch::new(R);
        c6.add_edge(
            EId(60),
            VId(4),
            VId(5),
            vec![(P, CanonicalScalar::Int(600))],
        );
        assert_eq!(db.write(&cx, c6).await.unwrap(), CommitSeq(6));
        let root_6 = db.partition_root().unwrap();
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    root_6,
                    R,
                    CommitSeq(6),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    root_6,
                    R,
                    CommitSeq(5),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_some()
        );
        let other_at_6 = store
            .get_published_adj_run(
                &query,
                root_6,
                OTHER,
                CommitSeq(6),
                SealedLimits::default(),
                RootReadLimits::default().max_root_bytes,
            )
            .await
            .unwrap()
            .expect("OTHER run persists across R's mutation at Commit 6");
        assert_eq!(
            other_at_6.partition().anchor().verified_through(),
            CommitSeq(6)
        );

        // Commit 7 mutates OTHER: R's verified_through must NOT jump over Commit 6!
        let mut c7 = WriteBatch::new(OTHER);
        c7.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(55)));
        assert_eq!(db.write(&cx, c7).await.unwrap(), CommitSeq(7));
        let root_7 = db.partition_root().unwrap();
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    root_7,
                    R,
                    CommitSeq(7),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    root_7,
                    R,
                    CommitSeq(5),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .get_published_adj_run(
                    &query,
                    root_7,
                    OTHER,
                    CommitSeq(7),
                    SealedLimits::default(),
                    RootReadLimits::default().max_root_bytes,
                )
                .await
                .unwrap()
                .is_some()
        );

        let summary = db.scrub(&cx).await.unwrap();
        assert!(summary.lost.is_empty());
        assert!(summary.block_lost.is_empty());
        assert!(summary.metadata_lost.is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn published_fgrn_v2_dual_face_and_post_seal_delta_overlay_serve_both_directions_across_reopen() {
    let ((), report) = run_async_under_lab(0xd870_0099, |runtime| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&runtime);
        let cx = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let (mut db, _) = fixture(&cx, &vfs).await;

        let run_id = db
            .seal_relation(
                &cx,
                &query,
                R,
                CommitSeq(1),
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();

        let store = BlockStore::open_existing_for_read_with_vfs(
            &cx,
            vfs.clone(),
            vfs.database_dir(),
            KEY,
            NS,
        )
        .await
        .unwrap();
        let raw_run_bytes = store.get_adj_run_bytes(&cx, run_id).await.unwrap();
        assert_eq!(
            u16::from_le_bytes([raw_run_bytes[4], raw_run_bytes[5]]),
            fgdb::ADJ_RUN_FORMAT_V2,
            "published durable runs must use FGRN-v2 dual-face format",
        );
        let loaded_run = store
            .get_published_adj_run(
                &query,
                db.partition_root().unwrap(),
                R,
                CommitSeq(3),
                SealedLimits::default(),
                RootReadLimits::default().max_root_bytes,
            )
            .await
            .unwrap()
            .expect("sealed run covers CommitSeq(3)");
        assert!(loaded_run.partition().has_durable_incoming());
        let inc_idx = loaded_run
            .partition()
            .incoming_index(
                &query,
                fgdb_strata::tiered::sealed::IncomingIndexLimits::default(),
            )
            .unwrap();
        assert_eq!(
            inc_idx.stats().charged_workspace_bytes,
            inc_idx.stats().charged_resident_bytes,
        );

        // Apply post-seal mutations on R across two commits (add_edge, delete_edge, set_edge_property).
        let mut c4 = WriteBatch::new(R);
        c4.add_edge(
            EId(401),
            VId(4),
            VId(1),
            vec![(P, CanonicalScalar::Int(41))],
        );
        c4.delete_edge(EId(4));
        c4.set_edge_property(EDGE, P, Some(CanonicalScalar::Int(700)));
        assert_eq!(db.write(&cx, c4).await.unwrap(), CommitSeq(4));

        let mut c5 = WriteBatch::new(R);
        c5.add_edge(
            EId(501),
            VId(2),
            VId(4),
            vec![(P, CanonicalScalar::Int(51))],
        );
        assert_eq!(db.write(&cx, c5).await.unwrap(), CommitSeq(5));

        // Reopen cold from disk so zero process-local state survives.
        drop(db);
        let db = Database::open_with_vfs(&cx, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();

        // Direct StrataNeighborCursor overlay verification at both CommitSeq(4) and CommitSeq(5).
        for cut in [CommitSeq(4), CommitSeq(5)] {
            let overlay = db
                .open_sealed_delta_overlay(
                    &query,
                    R,
                    Some(cut),
                    SealedLimits::default(),
                    RootReadLimits::default(),
                )
                .await
                .unwrap()
                .expect("carried FGRN-v2 run + post-seal Tier-D delta overlay must open");
            assert_eq!(overlay.verified_through(), CommitSeq(3));
            assert_eq!(overlay.as_of(), cut);
            assert!(overlay.base_run().partition().has_durable_incoming());

            let fresh = store
                .seal_relation(
                    &query,
                    db.partition_root().unwrap(),
                    cut,
                    R,
                    SealedLimits::default(),
                )
                .await
                .unwrap();
            let fresh_inc = fresh
                .incoming_index(
                    &query,
                    fgdb_strata::tiered::sealed::IncomingIndexLimits::default(),
                )
                .unwrap();

            for v in [VId(1), VId(2), VId(3), VId(4), VId(99)] {
                let mut ov_out = overlay.outgoing_cursor(&query, v).unwrap();
                let mut fr_out = fresh.row(&query, v, R, cut).unwrap();
                loop {
                    match (ov_out.next(&query).unwrap(), fr_out.next(&query).unwrap()) {
                        (Some(a), Some(b)) => {
                            assert_eq!(a.entry, b.entry);
                            assert_eq!(a.properties, b.properties);
                        }
                        (None, None) => break,
                        other => panic!("outgoing overlay mismatch at {cut:?} v={v:?}: {other:?}"),
                    }
                }

                let mut ov_in = overlay.incoming_cursor(&query, v).unwrap();
                let mut fr_in = fresh_inc.row(&query, v, R, cut).unwrap();
                loop {
                    match (ov_in.next(&query).unwrap(), fr_in.next(&query).unwrap()) {
                        (Some(a), Some(b)) => {
                            assert_eq!(a.entry, b.entry);
                            assert_eq!(a.properties, b.properties);
                        }
                        (None, None) => break,
                        other => panic!("incoming overlay mismatch at {cut:?} v={v:?}: {other:?}"),
                    }
                }
            }

            let materialized = overlay
                .materialize_partition(&query, SealedLimits::default())
                .unwrap();
            assert_eq!(materialized.anchor(), fresh.anchor());
            assert!(materialized.has_durable_incoming());
        }

        // Verify Directed, Reversed, and Undirected --sealed CALL fnx.* match EmbeddedReadView bit-for-bit!
        let live_view = db.read_session().unwrap();
        for dir in [
            fgdb_prism::Directedness::Directed,
            fgdb_prism::Directedness::Reversed,
            fgdb_prism::Directedness::Undirected,
        ] {
            let mut opts = options(CommitSeq(5));
            opts.projection.directedness = dir;
            let call = FnxCallSpec::bind(
                "CALL fnx.single_source_shortest_path_length(1) YIELD node, distance",
                &FnxParameters::new(),
            )
            .unwrap();
            let expected = live_view.execute_fnx(&query, &call, opts).unwrap();
            let actual = db
                .execute_fnx_sealed(&query, &call, opts, memory(), SealedLimits::default())
                .await
                .unwrap();
            assert_eq!(
                actual.analytics.certificate.adapter,
                fgdb_prism::AdapterPath::CompressedCursor,
                "post-seal delta overlay must stay on CompressedCursor for direction={dir:?}",
            );
            assert_eq!(
                actual.analytics.columns, expected.analytics.columns,
                "direction={dir:?} columns"
            );
            assert_eq!(
                actual.analytics.rows, expected.analytics.rows,
                "direction={dir:?} rows"
            );
            assert_eq!(
                actual.analytics.certificate.result_digest,
                expected.analytics.certificate.result_digest,
                "direction={dir:?} result_digest",
            );
            assert_eq!(
                actual.selection, expected.selection,
                "direction={dir:?} selection"
            );

            let cold_proj = Database::open_prism_sealed_projection_with_vfs(
                &cx,
                &query,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                opts,
                SealedLimits::default(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
            let cold_exec = call
                .execute_sealed(&query, &cold_proj, opts.execution_limits, memory())
                .unwrap();
            assert_eq!(
                fgdb_prism::FnxReadResult::bind_selection(cold_exec, opts.selection),
                actual,
                "cold open_prism_sealed_projection mismatch for {dir:?}",
            );
        }

        let cold_sealed_view = cold(&vfs, &cx, &query, CommitSeq(5)).await;
        assert_eq!(
            sealed_answers(&cold_sealed_view, &query),
            answers(&live_view, &query),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
