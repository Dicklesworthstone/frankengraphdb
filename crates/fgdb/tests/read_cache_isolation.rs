//! Exercise cache ownership through real Chronicle writes, public read APIs,
//! live authorization, compaction and authoritative recovery.

#![recursion_limit = "256"]

use asupersync::security::key::AuthKey;
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{
    Database, DatabaseKeys, DerivedPublicationStage, EmbeddedReadView, MemVfs, QueryResult,
    ReadCacheLimits, WriteBatch, WriteError,
};
use fgdb_delta_types::{LabelId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_prism::{FnxReadOptions, SnapshotGraphView};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId};
use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Restriction, Scope};
use std::cell::Cell;
use std::sync::Arc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x6d; 32]);
const READS: [&str; 3] = [
    "MATCH (a)-[:R]->(b) RETURN count(*) AS n",
    "MATCH (a)-[:R]->(b) RETURN a",
    "CALL fnx.pagerank(0.85,50,1e-9,true) YIELD vertex,score",
];
const ZERO: ReadCacheLimits = ReadCacheLimits {
    projection_bytes: 0,
    admission_bytes: 0,
    authorized_projection_bytes: 0,
};

async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut database = Database::open_memory(cx, DatabaseKeys::new([0x6c; 32], NS, [0x6e; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        batch.create_vertex(VId(id), vec![LabelId(if id == 4 { 2 } else { 1 })], vec![]);
    }
    for (eid, source, target) in [(1, 1, 2), (2, 2, 3), (3, 3, 1), (4, 1, 4)] {
        batch.add_edge(EId(eid), VId(source), VId(target), vec![]);
    }
    database.write(cx, batch).await.unwrap();
    database
}

fn project(view: &EmbeddedReadView, cx: &QueryCx, variation: u64) -> SnapshotGraphView {
    let mut options = FnxReadOptions::whole_graph(None);
    options.source_limits.max_work_units -= variation;
    view.prism_projection_at(
        cx,
        view.frontier(),
        options.selection,
        options.projection,
        options.projection_limits,
        options.source_limits,
    )
    .unwrap()
}

fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        10_000,
        QueryLimits {
            max_nodes: 10_000,
            max_work: 10_000_000,
            max_rows: 10_000,
        },
    );
    grant.labels = Scope::All;
    grant.relations = Scope::All;
    grant.properties = Scope::All;
    grant
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// Native equality compares actual canonical cells; their Debug output
    /// deliberately redacts payloads and cannot serve as a result oracle.
    Returned(QueryResult),
    Refused(String),
}

impl Answer {
    fn succeeded(&self) -> bool {
        matches!(self, Self::Returned(_))
    }
}

fn footprint(
    database: &Database<MemVfs>,
    cx: &QueryCx,
    authority: &Authority,
    token: &CapabilityToken,
    text: &str,
    stop: Option<usize>,
) -> (Answer, usize, usize) {
    let probe = Arc::new(SimulationCheckpointProbe::new(stop));
    let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
    let clocks = Cell::new(0);
    let result = database.query_authorized(
        &observed,
        authority,
        token,
        "main",
        text,
        &GqlParameters::new(),
        symbols,
        GqlQueryPolicy::new(100_000, 10_000, 10_000_000, 1_000_000),
        || {
            clocks.set(clocks.get() + 1);
            100
        },
    );
    let answer = match result {
        Ok(result) => Answer::Returned(result),
        Err(error) => Answer::Refused(format!("{error:?}")),
    };
    (answer, probe.calls(), clocks.get())
}

#[test]
fn simultaneous_databases_keep_independent_slots_and_byte_allowances() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let mut owners = Vec::new();
        // More live databases than the old process-global eight-slot cache.
        for _ in 0..12 {
            let database = database(&contexts.commit()).await;
            let view = database.read_session().unwrap();
            let first = project(&view, &cx, 0);
            let bytes = first.charged_resident_bytes();
            database
                .set_read_cache_limits(
                    &cx,
                    ReadCacheLimits {
                        projection_bytes: bytes,
                        ..ReadCacheLimits::default()
                    },
                )
                .unwrap();
            // This distinct request fits by itself but cannot displace the
            // first request under the configured one-entry byte allowance.
            let declined = project(&view, &cx, 1);
            assert!(!first.shares_cache_with(&declined));
            let status = database.read_cache_status(&cx).unwrap();
            assert_eq!(status.projections.entries, 1);
            assert_eq!(status.projections.retained_bytes, bytes);
            assert_eq!(status.projections.max_bytes, bytes);
            owners.push((database, view, first));
        }
        for (database, view, first) in &owners {
            let warm = project(view, &cx, 0);
            assert!(first.shares_cache_with(&warm));
            assert_eq!(first.digest(), warm.digest());
            assert_eq!(
                database.read_cache_status(&cx).unwrap().projections.entries,
                1
            );
        }
        owners[0].0.set_read_cache_limits(&cx, ZERO).unwrap();
        assert_eq!(
            owners[0]
                .0
                .read_cache_status(&cx)
                .unwrap()
                .projections
                .entries,
            0
        );
        for (_, view, first) in owners.iter().skip(1) {
            assert!(first.shares_cache_with(&project(view, &cx, 0)));
        }
    });
}

#[test]
fn scoped_reuse_and_tiny_budgets_preserve_rows_charges_and_refusal_boundaries() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let warm = database(&contexts.commit()).await;
        let cold = database(&contexts.commit()).await;
        cold.set_read_cache_limits(&cx, ZERO).unwrap();
        let authority =
            Authority::new(AuthKey::from_seed(0x6d), NS, "graph", SchemaEpoch(0), 1).unwrap();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut expected = Vec::new();
        let mut admission_before_index = 0;
        for (at, text) in READS.into_iter().enumerate() {
            let fresh = footprint(&cold, &cx, &authority, &token, text, None);
            assert!(fresh.0.succeeded(), "{fresh:?}");
            assert_eq!(footprint(&warm, &cx, &authority, &token, text, None), fresh);
            assert_eq!(footprint(&warm, &cx, &authority, &token, text, None), fresh);
            if at == 0 {
                // COUNT retains a core without refilling its vertex table.
                admission_before_index = warm
                    .read_cache_status(&cx)
                    .unwrap()
                    .admissions
                    .retained_bytes;
            } else if at == 1 {
                // This table query initializes the cached core's lazy index;
                // its prospective capacity was charged before admission.
                assert_eq!(
                    warm.read_cache_status(&cx)
                        .unwrap()
                        .admissions
                        .retained_bytes,
                    admission_before_index
                );
            }
            expected.push(fresh);
        }
        let retained = warm.read_cache_status(&cx).unwrap();
        assert_eq!(retained.admissions.entries, 1);
        assert_eq!(retained.authorized_projections.entries, 1);
        assert!(retained.admissions.retained_bytes > 0);
        assert!(retained.authorized_projections.retained_bytes > 0);
        let exact = ReadCacheLimits {
            projection_bytes: 0,
            admission_bytes: retained.admissions.retained_bytes,
            authorized_projection_bytes: retained.authorized_projections.retained_bytes,
        };
        warm.set_read_cache_limits(&cx, ZERO).unwrap();
        warm.set_read_cache_limits(&cx, exact).unwrap();
        for (at, text) in READS.into_iter().enumerate() {
            assert_eq!(
                footprint(&warm, &cx, &authority, &token, text, None),
                expected[at]
            );
        }
        let at_limit = warm.read_cache_status(&cx).unwrap();
        assert_eq!(at_limit.admissions.retained_bytes, exact.admission_bytes);
        assert_eq!(
            at_limit.authorized_projections.retained_bytes,
            exact.authorized_projection_bytes
        );
        warm.set_read_cache_limits(
            &cx,
            ReadCacheLimits {
                projection_bytes: 0,
                admission_bytes: exact.admission_bytes - 1,
                authorized_projection_bytes: exact.authorized_projection_bytes - 1,
            },
        )
        .unwrap();
        for (at, text) in READS.into_iter().enumerate() {
            assert_eq!(
                footprint(&warm, &cx, &authority, &token, text, None),
                expected[at]
            );
        }
        let below_limit = warm.read_cache_status(&cx).unwrap();
        assert_eq!(below_limit.admissions.entries, 0);
        assert_eq!(below_limit.authorized_projections.entries, 0);

        warm.set_read_cache_limits(&cx, ReadCacheLimits::default())
            .unwrap();
        let mut restricted = grant();
        restricted.labels = Scope::only([LabelId(1)]);
        let scoped = authority.issue_at(&restricted, 100).unwrap();
        for (at, text) in READS.into_iter().enumerate() {
            // The full graph is present before the restricted request. Exact
            // predicate keys must prevent it from answering the smaller scope.
            assert_eq!(
                footprint(&warm, &cx, &authority, &token, text, None),
                expected[at]
            );
            let fresh = footprint(&cold, &cx, &authority, &scoped, text, None);
            assert_ne!(
                fresh.0, expected[at].0,
                "the fixture must expose hidden rows"
            );
            assert_eq!(
                footprint(&warm, &cx, &authority, &scoped, text, None),
                fresh
            );
            assert_eq!(
                footprint(&warm, &cx, &authority, &scoped, text, None),
                fresh
            );
            assert_eq!(
                footprint(&warm, &cx, &authority, &token, text, None),
                expected[at]
            );

            let insufficient = token.attenuate(Restriction::MaxWork(1)).unwrap();
            let refused = footprint(&cold, &cx, &authority, &insufficient, text, None);
            assert!(!refused.0.succeeded());
            assert_eq!(
                footprint(&warm, &cx, &authority, &insufficient, text, None),
                refused
            );
            for stop in [1, fresh.1 / 2, fresh.1] {
                let a = footprint(&cold, &cx, &authority, &scoped, text, Some(stop));
                let b = footprint(&warm, &cx, &authority, &scoped, text, Some(stop));
                assert!(!a.0.succeeded(), "cold {text}, stop {stop}: {a:?}");
                assert!(!b.0.succeeded(), "warm {text}, stop {stop}: {b:?}");
                assert_eq!(
                    (a.1, a.2),
                    (b.1, b.2),
                    "{text}, stop {stop}: cold/warm refusal work differs"
                );
                // native_at checks live authority once after constructing its
                // result, including an engine error. The simulation probe
                // interrupts only at its exact ordinal, so this completion
                // checkpoint can follow the interrupted engine checkpoint.
                assert!(
                    a.1 == stop || a.1 == stop + 1,
                    "{text}, stop {stop}: unexpected work after refusal: {a:?}"
                );
            }
        }
        let scoped_status = warm.read_cache_status(&cx).unwrap();
        assert_eq!(
            scoped_status.admissions.entries, 1,
            "scoped scans never enter the unrestricted core cache"
        );
        assert_eq!(
            scoped_status.authorized_projections.entries, 2,
            "each exact predicate set keeps its own projection"
        );
        assert_eq!(cold.read_cache_status(&cx).unwrap().admissions.entries, 0);
        assert_eq!(
            cold.read_cache_status(&cx)
                .unwrap()
                .authorized_projections
                .entries,
            0
        );
    });
}

#[test]
fn generation_changes_and_recovery_preserve_policy_without_reusing_stale_sources() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut database = database(&commit).await;
        let limits = ReadCacheLimits {
            projection_bytes: 1 << 20,
            admission_bytes: 1 << 18,
            authorized_projection_bytes: 1 << 19,
        };
        database.set_read_cache_limits(&cx, limits).unwrap();
        let old = database.read_session().unwrap();
        let weak = old.downgrade();
        let original = project(&old, &cx, 0);
        let mut next = WriteBatch::new(RelationId(1));
        next.create_vertex(VId(5), vec![LabelId(1)], vec![]);
        next.add_edge(EId(5), VId(4), VId(5), vec![]);
        database.write(&commit, next).await.unwrap();
        let current = database.read_session().unwrap();
        let changed = project(&current, &cx, 0);
        assert!(!original.shares_cache_with(&changed));
        assert_eq!(original.vertex_ids().len(), 4);
        assert_eq!(changed.vertex_ids().len(), 5);
        assert!(original.shares_cache_with(&project(&weak.upgrade().unwrap(), &cx, 0)));
        assert_eq!(
            old.read_cache_status(&cx).unwrap(),
            database.read_cache_status(&cx).unwrap()
        );
        database.compact(&commit).await.unwrap();
        let compacted = database.read_session().unwrap();
        let after_compaction = project(&compacted, &cx, 0);
        assert_eq!(after_compaction.vertex_ids(), changed.vertex_ids());
        assert!(!after_compaction.shares_cache_with(&changed));
        assert_eq!(
            database.read_cache_status(&cx).unwrap().projections.entries,
            3
        );

        let mut durable = WriteBatch::new(RelationId(1));
        durable.create_vertex(VId(6), vec![LabelId(1)], vec![]);
        assert!(matches!(
            database
                .write_with_publication_failure(
                    &commit,
                    durable,
                    DerivedPublicationStage::PublishPartitionRoot,
                )
                .await,
            Err(WriteError::CommittedNeedsRecovery { .. })
        ));
        let recovered = database.recover_authoritatively(&commit).await.unwrap();
        let recovered_view = recovered.read_session().unwrap();
        let latest = project(&recovered_view, &cx, 0);
        assert_eq!(latest.vertex_ids().len(), 6);
        assert!(!latest.shares_cache_with(&after_compaction));
        let status = recovered.read_cache_status(&cx).unwrap();
        assert_eq!(status.projections.max_bytes, limits.projection_bytes);
        assert_eq!(status.admissions.max_bytes, limits.admission_bytes);
        assert_eq!(
            status.authorized_projections.max_bytes,
            limits.authorized_projection_bytes
        );
        assert_eq!(status.projections.entries, 4);
        assert_eq!(old.read_cache_status(&cx).unwrap(), status);
        drop(old);
        assert!(
            weak.upgrade().is_none(),
            "cache entries must not pin historical sources"
        );
        assert_eq!(
            recovered
                .read_cache_status(&cx)
                .unwrap()
                .projections
                .entries,
            3
        );
        current.set_read_cache_limits(&cx, ZERO).unwrap();
        assert_eq!(
            recovered
                .read_cache_status(&cx)
                .unwrap()
                .projections
                .entries,
            0
        );
        assert_eq!(
            latest.vertex_ids().len(),
            6,
            "active caller-held results survive eviction"
        );
    });
}
