use super::*;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::{
    DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x45; 32]);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(99);
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x45), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only("main", EXPIRES, QueryLimits {
        max_nodes: 3,
        max_work: 1_000_000,
        max_rows: 3,
    });
    grant.labels = Scope::only([L]);
    grant.properties = Scope::only([T, X]);
    grant
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(3, 3, 1_000_000, 1000)
}
fn catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    None
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn queries() -> [Search<'static>; 5] {
    [
        Search::Text { query: "red", k: 3, mode: TextMatch::Any },
        Search::Text { query: "red red", k: 3, mode: TextMatch::Phrase },
        Search::Vector { query: &[0.0], k: 3, mode: VectorSearch::Exact },
        Search::Vector {
            query: &[0.0], k: 3, mode: VectorSearch::Approximate { ef_search: 16 },
        },
        Search::Hybrid(ExactHybridQuery {
            vector: &[0.0], text: "red", k: 3,
            vector_candidates: 3, text_candidates: 3,
            vector_mode: VectorSearch::Approximate { ef_search: 16 },
            text_mode: TextMatch::Any, profile: ExactRrfProfile::default(),
        }),
    ]
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x46; 32], NS, [0x47; 32]))
        .await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, words, x) in [(1, "red", 0), (2, "red red", 2), (3, "blue", 3)] {
        let mut labels = vec![L];
        let mut props = vec![(T, text(words)), (X, CanonicalScalar::Int(x))];
        if hidden {
            labels.push(H);
            props.push((SECRET, text("forbidden")));
        }
        batch.create_vertex(VId(id), labels, props);
    }
    if hidden {
        for id in 100..132 {
            batch.create_vertex(VId(id), vec![H], vec![
                (T, CanonicalScalar::Int(1)), (X, text("not a coordinate")),
            ]);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn denied(result: Result<Rows, Error>, expected: WardenError) {
    assert!(matches!(&result,
        Err(super::super::SearchError::Interrupted(QueryError::Authorization(error)))
            if *error == expected
    ), "expected {expected:?}, got {result:?}");
}

#[test]
fn reusable_search_freezes_definition_and_matches_one_shot_after_writer_drop() {
    let ((), report) = run_async_under_lab(0xbeac_4101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let mut definition = options();
        let prepared = session.prepare_beacon_index(&c.query(), &definition).unwrap();
        let copy = prepared.clone();
        assert_eq!(prepared.source_sequence(), db.frontier().unwrap());
        assert_eq!(format!("{prepared:?}"), "AuthorizedBeaconIndex([REDACTED])");
        let expected = queries().map(|query| clean.beacon_search(&c.query(), &options(), query).unwrap());
        for (query, expected) in queries().into_iter().zip(&expected) {
            assert_eq!(session.search_beacon_index(&c.query(), &prepared, query, ReadPolicy::default()).unwrap(), *expected);
            assert_eq!(session.beacon_search(&c.query(), &options(), query).unwrap(), *expected);
        }
        // Neither caller-owned definition edits nor later native mutations
        // can alter the retained corpus, numeric profile, or lane selection.
        definition.projection.text = Some(SECRET);
        definition.projection.vector.clear();
        definition.index.vector = None;
        definition.policy.max_result_rows = 0;
        let mut update = WriteBatch::new(RelationId(1));
        update.delete_vertex(VId(1));
        update.set_vertex_property(VId(2), T, Some(text("different")));
        db.write(&c.commit(), update).await.unwrap();
        assert_ne!(db.beacon_search_authorized(
            &c.query(), &issuer, &token, "main", &options(), queries()[0], || NOW,
        ).unwrap(), expected[0]);
        drop(db);
        drop(prepared);
        for (query, expected) in queries().into_iter().zip(expected) {
            assert_eq!(session.search_beacon_index(&c.query(), &copy, query, ReadPolicy::default()).unwrap(), expected);
        }
        session.close();
        denied(session.search_beacon_index(&c.query(), &copy, queries()[0], ReadPolicy::default()),
            WardenError::ExecutionStopped);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn identical_or_narrower_sessions_cannot_reuse_a_foreign_corpus() {
    let ((), report) = run_async_under_lab(0xbeac_4102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut first = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let prepared = first.prepare_beacon_index(&c.query(), &options()).unwrap();
        let mut second = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        denied(second.search_beacon_index(&c.query(), &prepared, queries()[4], ReadPolicy::default()),
            WardenError::ScopeDenied);
        assert!(!second.is_closed());
        let own = second.prepare_beacon_index(&c.query(), &options()).unwrap();
        assert!(second.search_beacon_index(&c.query(), &own, queries()[4], ReadPolicy::default()).is_ok());
        let mut narrower = grant();
        narrower.properties = Scope::only([T]);
        let narrow_token = issuer.issue_at(&narrower, NOW).unwrap();
        let mut third = db.authorized_read_session(
            &c.query(), &issuer, &narrow_token, "main", catalog, host(), || NOW,
        ).unwrap();
        denied(third.search_beacon_index(&c.query(), &prepared, queries()[2], ReadPolicy::default()),
            WardenError::ScopeDenied);
        let masked = third.prepare_beacon_index(&c.query(), &options()).unwrap();
        assert!(third.search_beacon_index(&c.query(), &masked, queries()[2], ReadPolicy::default()).unwrap().is_empty());
        issuer.retire();
        // Retirement outranks both owner and query diagnostics on reuse.
        denied(second.search_beacon_index(&c.query(), &prepared, queries()[0], ReadPolicy::default()),
            WardenError::AuthorityRetired);
        assert!(second.is_closed());
        first.close();
        third.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_cuts_and_all_enabled_lanes_are_validated_at_preparation() {
    let ((), report) = run_async_under_lab(0xbeac_4103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), true).await;
        let at = db.frontier().unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_label(VId(1), L, false);
        update.set_vertex_property(VId(1), X, Some(text("hidden successor")));
        db.write(&c.commit(), update).await.unwrap();
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let mut historical = options();
        historical.as_of = Some(at);
        let old = session.prepare_beacon_index(&c.query(), &historical).unwrap();
        let current = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        assert_eq!(old.source_sequence(), at);
        assert_eq!(current.source_sequence(), db.frontier().unwrap());
        let rows = session.search_beacon_index(&c.query(), &old, queries()[2], ReadPolicy::default()).unwrap();
        // ubs:ignore -- test assertion on a vertex id, not secret material.
        assert!(matches!(rows, Rows::Vector(ref hits) if hits.iter().any(|hit| hit.id == VId(1))));
        let rows = session.search_beacon_index(&c.query(), &current, queries()[2], ReadPolicy::default()).unwrap();
        // ubs:ignore -- test assertion on a vertex id, not secret material.
        assert!(matches!(rows, Rows::Vector(ref hits) if hits.iter().all(|hit| hit.id != VId(1))));
        let mut invalid = options();
        invalid.projection.vector.push(SECRET);
        assert!(matches!(session.prepare_beacon_index(&c.query(), &invalid),
            Err(super::super::SearchError::Index(BeaconError::InvalidConfig(_)))));
        historical.as_of = Some(CommitSeq(db.frontier().unwrap().0 + 1));
        assert!(matches!(session.prepare_beacon_index(&c.query(), &historical),
            Err(super::super::SearchError::Read(_))));
        let mut text_only = options();
        text_only.index.vector = None;
        let text_only = session.prepare_beacon_index(&c.query(), &text_only).unwrap();
        assert!(matches!(session.search_beacon_index(&c.query(), &text_only, queries()[2], ReadPolicy::default()),
            Err(super::super::SearchError::Index(BeaconError::Disabled("vector")))));
        assert!(!session.is_closed());
        assert!(session.search_beacon_index(&c.query(), &old, queries()[0], ReadPolicy::default()).is_ok());
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reuse_does_not_rescan_and_cannot_widen_frozen_or_host_limits() {
    let ((), report) = run_async_under_lab(0xbeac_4104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let prepared = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        let mut request = ReadPolicy::default();
        request.max_source_scratch = 0;
        request.max_staging_rows = 0;
        assert_eq!(session.search_beacon_index(&c.query(), &prepared, queries()[2], request).unwrap().len(), 3);
        request.max_work_units = 0;
        assert!(matches!(session.search_beacon_index(&c.query(), &prepared, queries()[2], request),
            Err(super::super::SearchError::Index(BeaconError::WorkBudgetExceeded))));
        request = ReadPolicy::default();
        request.max_result_rows = 2;
        assert!(matches!(session.search_beacon_index(&c.query(), &prepared, queries()[2], request),
            Err(super::super::SearchError::Index(BeaconError::ResourceLimit { resource: "result rows", limit: 2 }))));
        let mut narrow = options();
        narrow.policy.max_result_rows = 2;
        let narrow = session.prepare_beacon_index(&c.query(), &narrow).unwrap();
        assert!(matches!(session.search_beacon_index(&c.query(), &narrow, queries()[2], ReadPolicy::default()),
            Err(super::super::SearchError::Index(BeaconError::ResourceLimit { resource: "result rows", limit: 2 }))));
        // Signed rows still apply, even with a much larger request ceiling.
        denied(session.search_beacon_index(&c.query(), &prepared,
            Search::Text { query: "red", k: 4, mode: TextMatch::Any }, ReadPolicy::default()),
            WardenError::LimitExceeded(fgdb_warden::LimitDimension::Rows));
        let mut zero = options();
        zero.policy.max_source_scratch = 0;
        assert!(matches!(session.prepare_beacon_index(&c.query(), &zero),
            Err(super::super::SearchError::Index(BeaconError::ResourceLimit { .. }))));
        assert!(!session.is_closed());
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_reuse_checkpoint_is_cancellable_and_preserves_the_same_handle() {
    let ((), report) = run_async_under_lab(0xbeac_4105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &c.query(), &issuer, &token, "main", catalog, host(), || NOW,
        ).unwrap();
        let prepared = session.prepare_beacon_index(&c.query(), &options()).unwrap();
        for query in queries().into_iter().chain([
            Search::Text { query: "absent", k: 3, mode: TextMatch::Any },
            Search::Text { query: "red", k: 0, mode: TextMatch::Any },
        ]) {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let expected = session.search_beacon_index(
                &c.query().with_checkpoint_probe(probe.clone()), &prepared, query, ReadPolicy::default(),
            ).unwrap();
            let calls = probe.calls();
            assert!(calls > 5);
            for cut in 1..=calls {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
                let result = session.search_beacon_index(
                    &c.query().with_checkpoint_probe(probe.clone()), &prepared, query, ReadPolicy::default(),
                );
                assert!(matches!(result, Err(super::super::SearchError::Interrupted(_))), "cut={cut}: {result:?}");
                // run checks once more after a failed action; an injected
                // cancellation is not a whole-task cancel, so do not assert
                // that the context can never be sampled for cleanup again.
                assert!(probe.calls() >= cut);
                assert!(!session.is_closed());
                assert_eq!(session.search_beacon_index(&c.query(), &prepared, query, ReadPolicy::default()).unwrap(), expected);
            }
        }
        session.close();
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_live_clock_boundary_can_expire_even_a_cached_empty_result() {
    let ((), report) = run_async_under_lab(0xbeac_4106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for query in [queries()[4],
            Search::Text { query: "absent", k: 3, mode: TextMatch::Any },
            Search::Text { query: "red", k: 0, mode: TextMatch::Any }]
        {
            let seen = Cell::new(0);
            let mut session = db.authorized_read_session(
                &c.query(), &issuer, &token, "main", catalog, host(), || {
                    seen.set(seen.get() + 1);
                    NOW
                },
            ).unwrap();
            let prepared = session.prepare_beacon_index(&c.query(), &options()).unwrap();
            seen.set(0);
            session.search_beacon_index(&c.query(), &prepared, query, ReadPolicy::default()).unwrap();
            let calls = seen.get();
            session.close();
            assert!(calls > 5);
            for cut in 0..calls {
                let seen = Cell::new(0);
                let stop = Cell::new(None);
                let mut session = db.authorized_read_session(
                    &c.query(), &issuer, &token, "main", catalog, host(), || {
                        let at = seen.get();
                        seen.set(at + 1);
                        if stop.get().is_some_and(|cut| at >= cut) { EXPIRES } else { NOW }
                    },
                ).unwrap();
                let prepared = session.prepare_beacon_index(&c.query(), &options()).unwrap();
                seen.set(0);
                stop.set(Some(cut));
                denied(session.search_beacon_index(&c.query(), &prepared, query, ReadPolicy::default()), WardenError::Expired);
                assert!(session.is_closed(), "expiry at {cut} did not close");
                assert_eq!(seen.get(), cut + 1, "continued after live refusal");
            }
        }
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
