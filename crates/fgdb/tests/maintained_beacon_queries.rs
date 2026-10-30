//! Ordinary retrieval must reuse only the exact admitted maintained generation.
//! Cold views are retained before registration, so the incumbent really rebuilds
//! its corpus; the optimized and incumbent answers do not share the cache path.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, QueryError, WriteBatch};
use fgdb_beacon::read::{ReadError, ReadOptions, ReadPolicy, Search};
use fgdb_beacon::{BeaconError, DistanceMetric, HnswConfig, TextMatch, VectorSearch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphSet,
    PreparedGraphSetText,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Authority, Grant, LimitDimension, QueryLimits, Restriction, Scope};
use std::sync::Arc;

const DOC: LabelId = LabelId(1);
const BODY: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const LINKS: RelationId = RelationId(1);
type Options = ReadOptions<PropertyKeyId, LabelId>;

const TEXT: &str = "CALL hybrid.search(text => 'graph storage', text_property => 'body', \
    label => 'Doc', candidates => 8, k => 4) \
    YIELD node, score, text_rank, text_score RETURN node, score, text_rank, text_score";
const HYBRID: &str = "CALL hybrid.search(text => 'graph storage', text_property => 'body', \
    vector => [0.0], vector_properties => ['x'], label => 'Doc', \
    candidates => 8, k => 4, eligible => [3, 4, 5, 7]) \
    YIELD node, score, vector_rank, text_rank, vector_distance, text_score \
    RETURN node, score, vector_rank, text_rank, vector_distance, text_score";
const PROPERTIES: &str = "CALL hybrid.search(text => 'graph storage', text_property => 'body', \
    label => 'Doc', candidates => 8, k => 4) YIELD node, score RETURN node.x AS x, score";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}

fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}

fn corpus() -> WriteBatch {
    let mut batch = WriteBatch::new(LINKS);
    for id in 1..=96_u128 {
        let body = if id % 3 == 0 {
            "graph graph storage"
        } else {
            "storage retrieval graph indexing database"
        };
        batch.create_vertex(
            VId(id),
            vec![DOC],
            vec![(BODY, text(body)), (X, CanonicalScalar::Int(id as i64))],
        );
    }
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
        k: 4,
        mode: TextMatch::Any,
    }
}

fn vector_query() -> Search<'static> {
    Search::Vector {
        query: &[0.0],
        k: 4,
        mode: VectorSearch::Exact,
    }
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Doc") => Some(GraphSymbol::Label(DOC)),
        (GraphSymbolKind::Property, "body") => Some(GraphSymbol::Property(BODY)),
        (GraphSymbolKind::Property, "x") => Some(GraphSymbol::Property(X)),
        _ => None,
    }
}

fn prepare(statement: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(statement, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 100, 100_000_000, 100_000)
}

#[test]
fn native_and_gql_reuse_exact_rows_without_rebuilding_and_pins_survive_updates() {
    let ((), report) = run_async_under_lab(0xbeac_4101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, corpus()).await.unwrap();
        let cold = db.read_session().unwrap();
        let handle = db
            .register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let pinned = db.read_session().unwrap();
        for query in [text_query(), vector_query()] {
            let expected = cold.beacon_search(&cx, &options(), query).unwrap();
            assert_eq!(db.beacon_search(&cx, &options(), query).unwrap(), expected);
            let mut query_only = options();
            query_only.policy.max_staging_rows = 0;
            assert_eq!(
                pinned.beacon_search(&cx, &query_only, query).unwrap(),
                expected,
                "a maintained search stages no native corpus rows"
            );
            assert!(cold.beacon_search(&cx, &query_only, query).is_err());
        }
        for statement in [TEXT, HYBRID] {
            let plan = prepare(statement);
            let incumbent = cold
                .execute_graph_set_governed(&cx, &plan, policy())
                .unwrap();
            let reused = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
            assert_eq!(reused.value, incumbent.value);
            assert_eq!(incumbent.rows.snapshot_records, 96);
            assert_eq!(reused.rows.snapshot_records, 0);
            assert!(
                reused.evaluator.work_units < incumbent.evaluator.work_units,
                "repeated retrieval must not pay for source projection/construction"
            );
            assert_eq!(
                pinned
                    .execute_graph_set_governed(&cx, &plan, policy())
                    .unwrap(),
                reused
            );
            assert_eq!(
                db.query(&cx, statement, &GqlParameters::new(), symbols, policy())
                    .unwrap(),
                cold.query(&cx, statement, &GqlParameters::new(), symbols, policy())
                    .unwrap()
            );
        }

        let plan = prepare(HYBRID);
        let before = pinned
            .execute_graph_set_governed(&cx, &plan, policy())
            .unwrap();
        let mut update = WriteBatch::new(LINKS);
        update.set_vertex_property(VId(3), BODY, Some(text("unrelated")));
        update.set_vertex_property(VId(3), X, Some(CanonicalScalar::Int(999)));
        update.delete_vertex(VId(4));
        update.set_vertex_label(VId(5), DOC, false);
        db.write(&commit, update).await.unwrap();
        let maintained = db.read_session().unwrap();
        let after = maintained
            .execute_graph_set_governed(&cx, &plan, policy())
            .unwrap();
        assert_eq!(after.rows.snapshot_records, 0);
        assert_ne!(after.value, before.value);
        assert_eq!(
            pinned
                .execute_graph_set_governed(&cx, &plan, policy())
                .unwrap(),
            before,
            "a later commit cannot change a retained query generation"
        );

        // Unregister exposes an actual cold incumbent at the same new cut.
        db.unregister_beacon_index(&cx, &handle).unwrap();
        let rebuilt = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert!(rebuilt.rows.snapshot_records > 0);
        assert_eq!(after.value, rebuilt.value);
        for query in [text_query(), vector_query()] {
            assert_eq!(
                maintained.beacon_search(&cx, &options(), query).unwrap(),
                db.beacon_search(&cx, &options(), query).unwrap()
            );
        }
        drop(db);
        assert_eq!(
            maintained
                .execute_graph_set_governed(&cx, &plan, policy())
                .unwrap(),
            after
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mismatch_history_ann_and_failed_maintenance_keep_the_source_path() {
    let ((), report) = run_async_under_lab(0xbeac_4102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let at = db.write(&commit, corpus()).await.unwrap();
        let cold = db.read_session().unwrap();
        let handle = db
            .register_beacon_index(
                &cx,
                &options(),
                ReadPolicy {
                    max_work_units: 0,
                    ..ReadPolicy::default()
                },
            )
            .unwrap();
        let plan = prepare(TEXT);
        let mut query_only = options();
        query_only.policy.max_staging_rows = 0;
        assert!(db.beacon_search(&cx, &query_only, text_query()).is_ok());

        let mut mismatch = query_only.clone();
        mismatch.index.text.as_mut().unwrap().k1 = 0.4;
        assert!(matches!(
            db.beacon_search(&cx, &mismatch, text_query()),
            Err(ReadError::Index(BeaconError::ResourceLimit {
                resource: "staged vertices",
                ..
            }))
        ));
        let ann = Search::Vector {
            query: &[0.0],
            k: 4,
            mode: VectorSearch::Approximate { ef_search: 8 },
        };
        assert!(matches!(
            db.beacon_search(&cx, &query_only, ann),
            Err(ReadError::Index(BeaconError::ResourceLimit {
                resource: "staged vertices",
                ..
            }))
        ));
        // The procedure has refused seeds without an explicit max_hops since
        // it landed (25cf0f73). The corpus has no edges, so the hop count
        // does not change what the graph-capable source admits.
        let graph = prepare(
            "CALL hybrid.search(text => 'graph', text_property => 'body', label => 'Doc', \
             seeds => [1], max_hops => 1, k => 4) YIELD node RETURN node",
        );
        assert_eq!(
            db.execute_graph_set_governed(&cx, &graph, policy())
                .unwrap()
                .rows
                .snapshot_records,
            96,
            "graph expansion retains its explicitly selected source"
        );

        let mut update = WriteBatch::new(LINKS);
        update.set_vertex_property(VId(1), BODY, Some(text("graph graph graph")));
        db.write(&commit, update).await.unwrap();
        assert!(
            db.beacon_index_status(&cx, &handle)
                .unwrap()
                .failure
                .is_some()
        );
        let current = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert_eq!(current.rows.snapshot_records, 96);
        let historical = db
            .execute_graph_set_governed_at(&cx, &plan, at, policy())
            .unwrap();
        assert_eq!(
            historical.value,
            cold.execute_graph_set_governed(&cx, &plan, policy())
                .unwrap()
                .value
        );
        assert_eq!(historical.rows.snapshot_records, 96);
        db.rebuild_beacon_index(&cx, &handle, ReadPolicy::default(), ReadPolicy::default())
            .unwrap();
        let repaired = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert_eq!(repaired.value, current.value);
        assert_eq!(repaired.rows.snapshot_records, 0);
        let past = db
            .execute_graph_set_governed_at(&cx, &plan, at, policy())
            .unwrap();
        assert_eq!(past.value, historical.value);
        assert_eq!(past.rows.snapshot_records, 96);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cached_calls_share_exact_parent_limits_and_preserve_every_live_refusal() {
    let ((), report) = run_async_under_lab(0xbeac_4103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, corpus()).await.unwrap();
        db.register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let plan = prepare(&format!("{HYBRID} UNION ALL {HYBRID}"));
        let expected = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert_eq!(expected.rows.snapshot_records, 0);
        assert_eq!(expected.value.len(), 8);
        let exact = GqlQueryPolicy::new(
            expected.rows.snapshot_records,
            expected.rows.result_rows,
            expected.evaluator.work_units,
            expected.evaluator.scratch_entries,
        );
        assert_eq!(
            db.execute_graph_set_governed(&cx, &plan, exact).unwrap(),
            expected
        );
        let mut short = exact;
        short.evaluator.max_work_units -= 1;
        assert!(db.execute_graph_set_governed(&cx, &plan, short).is_err());
        let mut short = exact;
        short.evaluator.max_scratch_entries -= 1;
        assert!(db.execute_graph_set_governed(&cx, &plan, short).is_err());

        // A reject-all result still checks the filter and final live gate.
        let empty = prepare(
            "CALL hybrid.search(text => 'graph', text_property => 'body', label => 'Doc', \
             eligible => [], k => 1) YIELD node RETURN node",
        );
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let accepted = db
            .execute_graph_set_governed(&cx.with_checkpoint_probe(probe.clone()), &empty, policy())
            .unwrap();
        assert!(accepted.value.is_empty());
        assert_eq!(accepted.rows.snapshot_records, 0);
        let checkpoints = probe.calls();
        assert!(checkpoints > 2);
        for stop in 1..=checkpoints {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let result = db.execute_graph_set_governed(
                &cx.with_checkpoint_probe(probe.clone()),
                &empty,
                policy(),
            );
            assert!(
                matches!(result, Err(GqlQueryError::Interrupted(_))),
                "cut {stop}: {result:?}"
            );
            assert_eq!(probe.calls(), stop, "work continued after refusal");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_queries_keep_the_transaction_corpus_when_a_committed_index_matches() {
    let ((), report) = run_async_under_lab(0xbeac_4104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let txcx = contexts.txn();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, corpus()).await.unwrap();
        db.register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let plan = prepare(TEXT);
        let committed = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        let mut txn = db.begin(&txcx).unwrap();
        let mut update = WriteBatch::new(LINKS);
        for id in 1..=96_u128 {
            update.set_vertex_property(VId(id), BODY, Some(text("unrelated")));
        }
        txn.write(&mut db, update).unwrap();
        let staged = txn
            .execute_graph_set_governed(&db, &cx, &plan, policy())
            .unwrap();
        assert!(staged.value.is_empty());
        assert!(!committed.value.is_empty());
        assert!(staged.rows.snapshot_records > 0);
        assert_eq!(
            db.execute_graph_set_governed(&cx, &plan, policy()).unwrap(),
            committed
        );
        txn.finish(&mut db, &commit).await.unwrap();
        let after = db.execute_graph_set_governed(&cx, &plan, policy()).unwrap();
        assert_eq!(after.value, staged.value);
        assert_eq!(after.rows.snapshot_records, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn authority(seed: u64) -> Authority {
    Authority::new(
        AuthKey::from_seed(seed),
        DatabaseSecurityNamespaceId([0x82; 32]),
        "graph",
        SchemaEpoch(0),
        1,
    )
    .unwrap()
}

fn full_grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        10_000,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 100_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

#[test]
fn authorized_native_calls_reuse_exact_pins_and_keep_their_snapshot_after_writes() {
    let ((), report) = run_async_under_lab(0xbeac_4105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let at = db.write(&commit, corpus()).await.unwrap();
        let issuer = authority(4105);
        let token = issuer.issue_at(&full_grant(), 100).unwrap();
        let mut cold = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let original = db.read_session().unwrap();
        let handle = db
            .register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let mut pinned = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let params = GqlParameters::new();
        for statement in [TEXT, HYBRID, PROPERTIES] {
            let cold_probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = cold
                .query(
                    &cx.with_checkpoint_probe(cold_probe.clone()),
                    statement,
                    &params,
                )
                .unwrap();
            let warm_probe = Arc::new(SimulationCheckpointProbe::new(None));
            let actual = pinned
                .query(
                    &cx.with_checkpoint_probe(warm_probe.clone()),
                    statement,
                    &params,
                )
                .unwrap();
            assert_eq!(actual, expected);
            assert!(
                warm_probe.calls() < cold_probe.calls(),
                "authorized CALL still rebuilt its corpus: {} >= {}",
                warm_probe.calls(),
                cold_probe.calls()
            );
            assert_eq!(
                db.query_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    statement,
                    &params,
                    symbols,
                    policy(),
                    || 100,
                )
                .unwrap(),
                expected
            );
            let prepared = pinned.prepare(&cx, statement, &params).unwrap();
            assert_eq!(pinned.execute(&cx, &prepared, &params).unwrap(), expected);
        }
        for query in [text_query(), vector_query()] {
            let expected = original.beacon_search(&cx, &options(), query).unwrap();
            let mut probe_only = options();
            probe_only.policy.max_staging_rows = 0;
            assert_eq!(
                db.beacon_search_authorized(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    &probe_only,
                    query,
                    || 100,
                )
                .unwrap(),
                expected,
                "the maintained authorized path must not stage a corpus"
            );
        }

        let before = pinned.query(&cx, HYBRID, &params).unwrap();
        let mut changes = WriteBatch::new(LINKS);
        changes.set_vertex_property(VId(3), BODY, Some(text("unrelated")));
        changes.set_vertex_property(VId(3), X, Some(CanonicalScalar::Int(999)));
        changes.delete_vertex(VId(4));
        db.write(&commit, changes).await.unwrap();
        let after = db
            .query_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                HYBRID,
                &params,
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        assert_ne!(after, before);
        assert_eq!(pinned.query(&cx, HYBRID, &params).unwrap(), before);

        let mut historical = options();
        historical.as_of = Some(at);
        assert_eq!(
            db.beacon_search_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &historical,
                text_query(),
                || 100,
            )
            .unwrap(),
            original
                .beacon_search(&cx, &options(), text_query())
                .unwrap()
        );
        historical.policy.max_staging_rows = 0;
        assert!(matches!(
            db.beacon_search_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                &historical,
                text_query(),
                || 100,
            ),
            Err(ReadError::Index(BeaconError::ResourceLimit {
                resource: "staged vertices",
                ..
            }))
        ));
        db.unregister_beacon_index(&cx, &handle).unwrap();
        assert_eq!(
            db.query_authorized(
                &cx,
                &issuer,
                &token,
                "main",
                HYBRID,
                &params,
                symbols,
                policy(),
                || 100,
            )
            .unwrap(),
            after,
            "a fresh rebuild at the new cut must reproduce maintained scores"
        );
        drop(db);
        assert_eq!(pinned.query(&cx, HYBRID, &params).unwrap(), before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_reuse_keeps_full_population_limits_and_never_uses_a_masked_corpus() {
    let ((), report) = run_async_under_lab(0xbeac_4106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut rows = corpus();
        // This live vertex is outside the index definition but inside the
        // signed source population. Document count is not a node-budget proxy.
        rows.create_vertex(VId(1000), vec![], vec![]);
        db.write(&commit, rows).await.unwrap();
        let issuer = authority(4106);
        let full = issuer.issue_at(&full_grant(), 100).unwrap();
        let masked = full
            .attenuate(Restriction::Properties(Scope::only([X])))
            .unwrap();
        let expected_masked = db
            .beacon_search_authorized(
                &cx,
                &issuer,
                &masked,
                "main",
                &options(),
                text_query(),
                || 100,
            )
            .unwrap();
        assert!(expected_masked.is_empty());
        db.register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        assert_eq!(
            db.beacon_search_authorized(
                &cx,
                &issuer,
                &masked,
                "main",
                &options(),
                text_query(),
                || 100,
            )
            .unwrap(),
            expected_masked
        );
        let mut no_staging = options();
        no_staging.policy.max_staging_rows = 0;
        for restriction in [
            Restriction::Labels(Scope::only([DOC])),
            Restriction::Relations(Scope::only([LINKS])),
            Restriction::Properties(Scope::only([BODY, X])),
        ] {
            let scoped = full.attenuate(restriction).unwrap();
            assert!(matches!(
                db.beacon_search_authorized(
                    &cx,
                    &issuer,
                    &scoped,
                    "main",
                    &no_staging,
                    text_query(),
                    || 100,
                ),
                Err(ReadError::Index(BeaconError::ResourceLimit {
                    resource: "staged vertices",
                    ..
                }))
            ));
        }
        let ann = Search::Vector {
            query: &[0.0],
            k: 4,
            mode: VectorSearch::Approximate { ef_search: 8 },
        };
        assert!(matches!(
            db.beacon_search_authorized(&cx, &issuer, &full, "main", &no_staging, ann, || 100,),
            Err(ReadError::Index(BeaconError::ResourceLimit {
                resource: "staged vertices",
                ..
            }))
        ));
        for (restriction, dimension) in [
            (Restriction::MaxNodes(96), LimitDimension::Nodes),
            (Restriction::MaxWork(0), LimitDimension::Work),
            (Restriction::MaxRows(3), LimitDimension::Rows),
        ] {
            let short = full.attenuate(restriction).unwrap();
            assert!(matches!(
                db.beacon_search_authorized(
                    &cx, &issuer, &short, "main", &no_staging, text_query(), || 100,
                ),
                Err(ReadError::Interrupted(QueryError::Authorization(
                    fgdb_warden::Error::LimitExceeded(actual)
                ))) if actual == dimension
            ));
        }

        let params = GqlParameters::new();
        let exact = full.attenuate(Restriction::MaxNodes(97)).unwrap();
        let exact_policy = GqlQueryPolicy::new(97, 100, 100_000_000, 100_000);
        assert!(
            db.query_authorized(
                &cx,
                &issuer,
                &exact,
                "main",
                TEXT,
                &params,
                symbols,
                exact_policy,
                || 100,
            )
            .is_ok()
        );
        let short_policy = GqlQueryPolicy::new(96, 100, 100_000_000, 100_000);
        assert!(
            db.query_authorized(
                &cx,
                &issuer,
                &full,
                "main",
                TEXT,
                &params,
                symbols,
                short_policy,
                || 100,
            )
            .is_err()
        );
        let twice = format!("{TEXT} UNION ALL {TEXT}");
        assert!(matches!(
            db.query_authorized(
                &cx,
                &issuer,
                &exact,
                "main",
                &twice,
                &params,
                symbols,
                policy(),
                || 100,
            ),
            Err(QueryError::Authorization(
                fgdb_warden::Error::LimitExceeded(LimitDimension::Nodes)
            ))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_reuse_observes_every_cancellation_and_live_expiry_before_empty_delivery() {
    let ((), report) = run_async_under_lab(0xbeac_4107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), corpus()).await.unwrap();
        db.register_beacon_index(&cx, &options(), ReadPolicy::default())
            .unwrap();
        let issuer = authority(4107);
        let token = issuer.issue_at(&full_grant(), 100).unwrap();
        let empty = "CALL hybrid.search(text => 'graph', text_property => 'body', label => 'Doc', \
            eligible => [], k => 1) YIELD node RETURN node";
        let params = GqlParameters::new();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let expected = db
            .query_authorized(
                &cx.with_checkpoint_probe(probe.clone()),
                &issuer,
                &token,
                "main",
                empty,
                &params,
                symbols,
                policy(),
                || 100,
            )
            .unwrap();
        assert!(matches!(expected, fgdb::QueryResult::Rows { rows, .. } if rows.is_empty()));
        for stop in 1..=probe.calls() {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let result = db.query_authorized(
                &cx.with_checkpoint_probe(probe.clone()),
                &issuer,
                &token,
                "main",
                empty,
                &params,
                symbols,
                policy(),
                || 100,
            );
            assert!(result.is_err(), "cancellation {stop} escaped: {result:?}");
            // native_at intentionally rechecks the live permit after a failed
            // bind/source. That one completion check may follow the injected
            // failure; the retrieval itself must not resume or retry a scan.
            assert!(
                (stop..=stop + 1).contains(&probe.calls()),
                "retrieval continued after cancellation {stop}: {} checkpoints",
                probe.calls()
            );
        }
        let ticks = std::cell::Cell::new(0_u64);
        let expired = db.query_authorized(
            &cx,
            &issuer,
            &token,
            "main",
            empty,
            &params,
            symbols,
            policy(),
            || {
                let at = ticks.get();
                ticks.set(at + 1);
                if at < 12 { 100 } else { 10_000 }
            },
        );
        assert!(matches!(
            expired,
            Err(QueryError::Authorization(fgdb_warden::Error::Expired))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
