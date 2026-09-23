//! Public read-only sessions over actual committed generations and Warden.
#[path = "authorized_sessions/streams.rs"]
mod streams;

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use fgdb_warden::{Authority, Error, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x52; 32]);
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0x5ec0_5001), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only("main", 1000, QueryLimits {
        max_nodes: 1000, max_work: 1_000_000, max_rows: 1000,
    });
    grant.labels = Scope::only([LabelId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant
}
fn policy() -> GqlQueryPolicy { GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000) }
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "H") => Some(GraphSymbol::Label(LabelId(99))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([1; 32], NS, [2; 32])).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, labels, p) in [(1, vec![LabelId(1), LabelId(99)], 7), (2, vec![LabelId(99)], 13), (3, vec![LabelId(1)], 19)] {
        batch.create_vertex(VId(id), labels, vec![
            (PropertyKeyId(1), CanonicalScalar::Int(p)),
            (PropertyKeyId(2), CanonicalScalar::Int(55)),
        ]);
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn value(value: i64) -> QueryValue { QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(value))) }
fn vertex(id: u128) -> QueryValue { QueryValue::Value(GraphValue::Vertex(VId(id))) }
fn result(columns: &[&str], rows: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows { columns: columns.iter().map(|name| (*name).to_owned()).collect(), rows }
}
fn ids() -> QueryResult { result(&["id"], vec![vec![vertex(1)], vec![vertex(3)]]) }

#[test]
fn session_and_prepared_reads_keep_one_generation_after_writes_and_writer_drop() {
    let ((), report) = run_async_under_lab(0x5ec0_5001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = database(&commit).await;
        let at = db.frontier().unwrap();
        let issuer = authority(); let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
        let params = GqlParameters::new().with_int64("min", 0).unwrap();
        let prepared = session.prepare(&cx, "MATCH (n) WHERE n.p >= $min RETURN n AS id, n.p AS p", &params).unwrap();
        assert_eq!(prepared.parameter_schema().len(), 1);
        let old = result(&["id", "p"], vec![vec![vertex(1), value(7)], vec![vertex(3), value(19)]]);
        assert_eq!(session.execute(&cx, &prepared, &params).unwrap(), old);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        db.write(&commit, change).await.unwrap();
        let future = db.frontier().unwrap();
        let mut fresh = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
        assert_eq!(fresh.query(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &GqlParameters::new()).unwrap(),
            result(&["id", "p"], vec![vec![vertex(1), value(70)], vec![vertex(3), value(19)]]));
        assert!(matches!(session.query(&cx,
            &format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN n AS id", future.0), &GqlParameters::new()),
            Err(QueryError::Read(_))));
        assert_eq!(fresh.query(&cx,
            &format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN n AS id, n.p AS p", at.0), &GqlParameters::new()).unwrap(), old);
        drop(db); drop(token); // Neither the writer nor bearer bytes are retained borrows.
        assert_eq!(session.execute(&cx, &prepared, &params).unwrap(), old);
        let params = GqlParameters::new().with_int64("min", 10).unwrap();
        assert_eq!(session.execute(&cx, &prepared, &params).unwrap(),
            result(&["id", "p"], vec![vec![vertex(3), value(19)]]));
        session.close(); session.close();
        assert!(session.is_closed());
        assert!(matches!(session.execute(&cx, &prepared, &params),
            Err(QueryError::Authorization(Error::ExecutionStopped))));
        assert_eq!(fresh.query(&cx, "MATCH (n) RETURN n AS id", &GqlParameters::new()).unwrap(), ids());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_session_owns_prepared_catalogs_and_scope_cannot_cross_handles() {
    let ((), report) = run_async_under_lab(0x5ec0_5002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0);
        let resolver = |kind, name: &str| { calls.set(calls.get() + 1); symbols(kind, name) };
        let mut first = db.authorized_read_session(&cx, &issuer, &token, "main", resolver, policy(), || 100).unwrap();
        let mut second = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
        let none = token.attenuate(fgdb_warden::Restriction::Labels(Scope::only([]))).unwrap();
        let mut denied = db.authorized_read_session(&cx, &issuer, &none, "main", symbols, policy(), || 100).unwrap();
        let args = GqlParameters::new();
        let prepared = first.prepare(&cx, "MATCH (n:L) RETURN n AS id", &args).unwrap();
        let after_prepare = calls.get(); assert!(after_prepare > 0);
        for _ in 0..3 { assert_eq!(first.execute(&cx, &prepared, &args).unwrap(), ids()); }
        assert_eq!(calls.get(), after_prepare, "execution must not re-resolve a prepared catalog");
        assert!(matches!(second.execute(&cx, &prepared, &args), Err(QueryError::Authorization(Error::WrongAuthority))));
        assert!(matches!(denied.execute(&cx, &prepared, &args), Err(QueryError::Authorization(Error::WrongAuthority))));
        assert!(!second.is_closed()); assert!(!denied.is_closed());
        assert_eq!(denied.query(&cx, "MATCH (n) RETURN n AS id", &args).unwrap(), result(&["id"], vec![]));
        let masked = first.query(&cx, "MATCH (n) WHERE n.hidden IS NULL RETURN n AS id", &args).unwrap();
        assert_eq!(masked, ids());
        assert_eq!(first.query(&cx, "MATCH (n:H) RETURN n AS id", &args).unwrap(), result(&["id"], vec![]));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn branch_selectors_and_argument_rebinding_do_not_redirect_session_authority() {
    let ((), report) = run_async_under_lab(0x5ec0_5003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
        let params = GqlParameters::new().with_text("route", "main").unwrap().with_int64("min", 0).unwrap();
        let prepared = session.prepare(&cx, "AT BRANCH $route MATCH (n) WHERE n.p >= $min RETURN n AS id", &params).unwrap();
        assert_eq!(prepared.parameter_schema().len(), 1);
        assert_eq!(session.execute(&cx, &prepared, &params).unwrap(), ids());
        let other = GqlParameters::new().with_text("route", "other").unwrap().with_int64("min", 0).unwrap();
        assert!(matches!(session.execute(&cx, &prepared, &other), Err(QueryError::Authorization(Error::ScopeDenied))));
        assert!(session.execute(&cx, &prepared, &GqlParameters::new().with_int64("min", 0).unwrap()).is_err());
        let extra = params.clone().with_int64("unexpected", 1).unwrap();
        assert!(session.execute(&cx, &prepared, &extra).is_err());
        assert_eq!(session.execute(&cx, &prepared, &params).unwrap(), ids());
        // Shared selector/query arguments must survive the selector's stripping.
        let only_route = GqlParameters::new().with_text("route", "main").unwrap();
        let shared = session.prepare(&cx, "AT BRANCH $route RETURN $route AS route", &only_route).unwrap();
        assert_eq!(session.execute(&cx, &shared, &only_route).unwrap(), result(&["route"], vec![vec![
            QueryValue::Value(GraphValue::Scalar(CanonicalScalar::ucs_basic_text("main").unwrap())),
        ]]));
        assert!(session.query(&cx, "CREATE (n)", &GqlParameters::new()).is_err());
        assert!(session.query(&cx, "EXPLAIN MATCH (n) RETURN n", &GqlParameters::new()).is_err());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn fixed_native_and_signed_limits_span_each_complete_statement_not_each_input() {
    let ((), report) = run_async_under_lab(0x5ec0_5004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = authority();
        let mut limits = grant(); limits.limits.max_rows = 1;
        let token = issuer.issue_at(&limits, 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
        let args = GqlParameters::new();
        let count = result(&["n"], vec![vec![QueryValue::Count(2)]]);
        assert_eq!(session.query(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args).unwrap(), count);
        assert!(matches!(session.query(&cx, "MATCH (n) RETURN n AS id", &args),
            Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Rows)))));
        assert!(!session.is_closed());
        assert_eq!(session.query(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args).unwrap(), count);
        let mut limits = grant(); limits.limits.max_nodes = 3;
        let token = issuer.issue_at(&limits, 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100).unwrap();
        assert!(matches!(session.query(&cx, "MATCH (n) RETURN n AS id UNION ALL MATCH (n) RETURN n AS id", &args),
            Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Nodes)))));
        assert_eq!(session.query(&cx, "MATCH (n) RETURN n AS id", &args).unwrap(), ids());
        let mut small = db.authorized_read_session(&cx, &issuer, &token, "main", symbols,
            GqlQueryPolicy::new(1000, 1, 1_000_000, 1_000_000), || 100).unwrap();
        assert!(matches!(small.query(&cx, "MATCH (n) RETURN n AS id", &args), Err(QueryError::Pattern(GqlQueryError::Rows(_)))));
        assert_eq!(small.query(&cx, "MATCH (n) RETURN COUNT(*) AS n", &args).unwrap(), count);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authentication_precedes_callbacks_and_time_cannot_restart_a_closed_session() {
    let ((), report) = run_async_under_lab(0x5ec0_5005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0);
        assert!(matches!(db.authorized_read_session(&cx, &issuer, &token, "other",
            |kind, name: &str| { calls.set(calls.get() + 1); symbols(kind, name) }, policy(), || 100),
            Err(QueryError::Authorization(Error::ScopeDenied))));
        assert_eq!(calls.get(), 0);
        let now = Cell::new(100);
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || now.get()).unwrap();
        now.set(300);
        session.query(&cx, "RETURN 1 AS n", &GqlParameters::new()).unwrap();
        now.set(200); // Across statements, not only inside one permit.
        assert!(matches!(session.query(&cx, "RETURN 1 AS n", &GqlParameters::new()),
            Err(QueryError::Authorization(Error::ClockWentBackwards))));
        assert!(session.is_closed());
        now.set(300);
        assert!(matches!(session.query(&cx, "RETURN 1 AS n", &GqlParameters::new()),
            Err(QueryError::Authorization(Error::ExecutionStopped))));
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || now.get()).unwrap();
        now.set(1000);
        assert!(matches!(session.query(&cx, "not valid GQL", &GqlParameters::new()), Err(QueryError::Authorization(Error::Expired))));
        assert!(session.is_closed());
        now.set(500);
        assert!(matches!(session.query(&cx, "RETURN 1 AS n", &GqlParameters::new()), Err(QueryError::Authorization(Error::ExecutionStopped))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn callback_retirement_and_unwinding_release_the_session_instead_of_retaining_access() {
    let ((), report) = run_async_under_lab(0x5ec0_5006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main",
            |kind, name: &str| { issuer.retire(); symbols(kind, name) }, policy(), || 100).unwrap();
        assert!(matches!(session.query(&cx, "MATCH (n:L) RETURN n AS id", &GqlParameters::new()),
            Err(QueryError::Authorization(Error::AuthorityRetired))));
        assert!(session.is_closed());
        let issuer = authority(); let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, "main",
            |_, _: &str| -> Option<GraphSymbol> { panic!("injected host resolver unwind") }, policy(), || 100).unwrap();
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.query(&cx, "MATCH (n:L) RETURN n AS id", &GqlParameters::new())
        }));
        assert!(stopped.is_err());
        assert!(session.is_closed());
        session.close();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
