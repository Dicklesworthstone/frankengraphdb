use super::*;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_beacon::{DistanceMetric, ExactHybridQuery, ExactRrfProfile, HnswConfig, TextMatch, VectorSearch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Authority, Grant, QueryLimits, Scope};
use std::cell::Cell;
use std::rc::Rc;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x71; 32]);
const BRANCH: &str = "session-beacon";
const LABEL: LabelId = LabelId(1);
const HIDDEN: LabelId = LabelId(99);
const TEXT: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x71), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(BRANCH, EXPIRES, QueryLimits {
        max_nodes: 100, max_work: 1_000_000, max_rows: 10,
    });
    grant.labels = Scope::only([LABEL]);
    grant.properties = Scope::only([TEXT, X]);
    grant
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 10, 1_000_000, 1000)
}
fn no_catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    panic!("Beacon must not access the native query catalog")
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn options() -> Options {
    let mut options = Options::text(TEXT);
    options.vertex_label = Some(LABEL);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn searches() -> [Search<'static>; 4] {
    [
        Search::Text { query: "red", k: 3, mode: TextMatch::Any },
        Search::Vector { query: &[0.0], k: 3, mode: VectorSearch::Exact },
        Search::Vector { query: &[0.0], k: 3, mode: VectorSearch::Approximate { ef_search: 16 } },
        Search::Hybrid(ExactHybridQuery {
            vector: &[0.0], text: "red", k: 3, vector_candidates: 3, text_candidates: 3,
            vector_mode: VectorSearch::Approximate { ef_search: 16 },
            text_mode: TextMatch::Any, profile: ExactRrfProfile::default(),
        }),
    ]
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x72; 32], NS, [0x73; 32]))
        .await.unwrap();
    let mut rows = WriteBatch::new(RelationId(1));
    for (id, words, x) in [(1, "red", 0), (2, "red red", 2), (3, "blue", 3)] {
        let mut labels = vec![LABEL];
        let mut props = vec![(TEXT, text(words)), (X, CanonicalScalar::Int(x))];
        if hidden {
            labels.push(HIDDEN);
            props.push((SECRET, text("forbidden coordinate")));
        }
        rows.create_vertex(VId(id), labels, props);
    }
    if hidden {
        for id in 100..120 {
            rows.create_vertex(VId(id), vec![HIDDEN], vec![
                (TEXT, text("red red red red red")), (X, CanonicalScalar::Int(0)),
            ]);
        }
        rows.create_vertex(VId(120), vec![HIDDEN], vec![
            (TEXT, CanonicalScalar::Int(9)), (X, text("not a vector")),
        ]);
    }
    db.write(cx, rows).await.unwrap();
    db
}
fn ids(rows: &Rows) -> Vec<VId> {
    match rows {
        Rows::Text(rows) => rows.iter().map(|row| row.id).collect(),
        Rows::Vector(rows) => rows.iter().map(|row| row.id).collect(),
        Rows::Hybrid(rows) => rows.iter().map(|row| row.id).collect(),
    }
}

#[test]
fn scoped_session_search_matches_visible_corpus_and_keeps_its_pin() {
    let ((), report) = run_async_under_lab(0xbeac_3001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut full = database(&commit, true).await;
        let clean = database(&commit, false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = full.authorized_read_session(
            &query, &issuer, &token, BRANCH, no_catalog, host(), || NOW,
        ).unwrap();
        let expected = searches().map(|search| clean.beacon_search(&query, &options(), search).unwrap());
        for (search, rows) in searches().into_iter().zip(&expected) {
            assert_eq!(session.beacon_search(&query, &options(), search).unwrap(), *rows);
            assert_eq!(full.beacon_search_authorized(
                &query, &issuer, &token, BRANCH, &options(), search, || NOW,
            ).unwrap(), *rows);
        }
        // The hidden malformed text is a real negative control, not dead data.
        assert!(full.beacon_search(&query, &options(), searches()[0]).is_err());
        let mut update = WriteBatch::new(RelationId(2));
        update.delete_vertex(VId(1));
        update.create_vertex(VId(4), vec![LABEL], vec![(TEXT, text("red")), (X, CanonicalScalar::Int(1))]);
        full.write(&commit, update).await.unwrap();
        assert_ne!(full.beacon_search_authorized(
            &query, &issuer, &token, BRANCH, &options(), searches()[0], || NOW,
        ).unwrap(), expected[0]);
        drop(full);
        for (search, rows) in searches().into_iter().zip(&expected) {
            assert_eq!(session.beacon_search(&query, &options(), search).unwrap(), *rows);
        }
        session.close();
        assert!(matches!(session.beacon_search(&query, &options(), searches()[0]),
            Err(SearchError::Interrupted(QueryError::Authorization(WardenError::ExecutionStopped)))));
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn historical_winners_and_property_masks_precede_search_validation() {
    let ((), report) = run_async_under_lab(0xbeac_3002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let mut db = database(&commit, true).await;
        let old = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LABEL, false);
        change.set_vertex_property(VId(1), TEXT, Some(CanonicalScalar::Int(99)));
        db.write(&commit, change).await.unwrap();
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db.authorized_read_session(
            &query, &issuer, &token, BRANCH, no_catalog, host(), || NOW,
        ).unwrap();
        let current = session.beacon_search(&query, &options(), searches()[0]).unwrap();
        assert!(!ids(&current).contains(&VId(1)), "a hidden successor cannot revive its predecessor");
        let mut historical = options();
        historical.as_of = Some(old);
        assert!(ids(&session.beacon_search(&query, &historical, searches()[0]).unwrap()).contains(&VId(1)));
        historical.as_of = Some(CommitSeq(db.frontier().unwrap().0 + 1));
        assert!(matches!(session.beacon_search(&query, &historical, searches()[0]),
            Err(SearchError::Read(ReadError::BeyondFrontier { .. }))));
        assert!(!session.is_closed());

        let mut masked = options();
        masked.projection.text = Some(SECRET);
        assert!(session.beacon_search(&query, &masked, searches()[0]).unwrap().is_empty());
        masked = options();
        masked.index.vector.as_mut().unwrap().dimensions = 2;
        masked.projection.vector = vec![X, SECRET];
        assert!(session.beacon_search(&query, &masked, Search::Vector {
            query: &[0.0, 0.0], k: 3, mode: VectorSearch::Exact,
        }).unwrap().is_empty(), "masked coordinates omit the lane before type validation");
        // A disabled lane's inconsistent definition must not poison text reads.
        masked.index.vector.as_mut().unwrap().dimensions = 0;
        assert_eq!(session.beacon_search(&query, &masked, searches()[0]).unwrap(), current);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn request_limits_cannot_widen_host_or_signed_session_limits() {
    let ((), report) = run_async_under_lab(0xbeac_3003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let db = database(&contexts.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut request = options();
        request.policy = ReadPolicy {
            max_work_units: usize::MAX, max_source_scratch: usize::MAX,
            max_staging_rows: usize::MAX, max_result_rows: usize::MAX,
        };
        for (case, limits) in [
            (0, GqlQueryPolicy::new(2, 10, 1_000_000, 1000)),
            (1, GqlQueryPolicy::new(100, 2, 1_000_000, 1000)),
            (2, GqlQueryPolicy::new(100, 10, 0, 1000)),
            (3, GqlQueryPolicy::new(100, 10, 1_000_000, 2)),
        ] {
            let mut session = db.authorized_read_session(
                &query, &issuer, &token, BRANCH, no_catalog, limits, || NOW,
            ).unwrap();
            let result = session.beacon_search(&query, &request, searches()[0]);
            match case {
                0 | 1 => assert!(matches!(result,
                    Err(SearchError::Interrupted(QueryError::Pattern(GqlQueryError::Rows(_)))))),
                2 => assert!(matches!(result, Err(SearchError::Index(BeaconError::WorkBudgetExceeded)))),
                _ => assert!(matches!(result, Err(SearchError::Index(BeaconError::ResourceLimit { .. })))),
            }
            assert!(!session.is_closed(), "ordinary native refusal retains the pin");
        }
        let mut limited = grant();
        limited.limits.max_rows = 2;
        let token = issuer.issue_at(&limited, NOW).unwrap();
        let mut session = db.authorized_read_session(
            &query, &issuer, &token, BRANCH, no_catalog, host(), || NOW,
        ).unwrap();
        assert!(matches!(session.beacon_search(&query, &request, searches()[0]),
            Err(SearchError::Interrupted(QueryError::Authorization(WardenError::LimitExceeded(LimitDimension::Rows))))));
        assert!(!session.is_closed());
        let small = Search::Text { query: "red", k: 2, mode: TextMatch::Any };
        assert_eq!(session.beacon_search(&query, &request, small).unwrap().len(), 2);
        assert_eq!(session.beacon_search(&query, &request, small).unwrap().len(), 2,
            "one live allowance per execution; no duplicate hit charge");
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expiry_and_host_unwind_close_sessions_even_for_empty_searches() {
    let ((), report) = run_async_under_lab(0xbeac_3004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let db = database(&contexts.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let now = Rc::new(Cell::new(NOW));
        let clock = Rc::clone(&now);
        let mut session = db.authorized_read_session(
            &query, &issuer, &token, BRANCH, no_catalog, host(), move || clock.get(),
        ).unwrap();
        now.set(EXPIRES);
        let empty = Search::Text { query: "", k: 0, mode: TextMatch::Any };
        assert!(matches!(session.beacon_search(&query, &options(), empty),
            Err(SearchError::Interrupted(QueryError::Authorization(WardenError::Expired)))));
        assert!(session.is_closed());
        now.set(NOW);
        assert!(matches!(session.beacon_search(&query, &options(), empty),
            Err(SearchError::Interrupted(QueryError::Authorization(WardenError::ExecutionStopped)))));

        let unwind = Rc::new(Cell::new(false));
        let clock = Rc::clone(&unwind);
        let mut session = db.authorized_read_session(
            &query, &issuer, &token, BRANCH, no_catalog, host(), move || {
                assert!(!clock.get(), "injected host clock unwind");
                NOW
            },
        ).unwrap();
        unwind.set(true);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.beacon_search(&query, &options(), empty)
        })).is_err());
        assert!(session.is_closed());
        assert_eq!(contexts.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
