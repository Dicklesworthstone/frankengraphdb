//! Independent golden outputs and public session-batch lifecycle tests.
use super::*;
use super::super::failure_tests::{BRANCH, database, grant, issuer, policy, refusal, symbols};
use asupersync::lab::run_async_under_lab;
use crate::WriteBatch;
use fgdb_delta_types::PropertyKeyId;
use fgdb_types::{CanonicalScalar, PurposeContexts};
use fgdb_warden::LimitDimension;
use std::cell::Cell;

fn value(n: i64) -> QueryValue { QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n))) }
fn rows(columns: &[&str], values: Vec<Vec<QueryValue>>) -> QueryResult {
    QueryResult::Rows { columns: columns.iter().map(|name| (*name).to_owned()).collect(), rows: values }
}
fn visible() -> QueryResult { rows(&["p"], vec![vec![value(10)], vec![value(30)]]) }

#[test]
fn all_seven_native_classes_keep_independent_schemas_and_one_pinned_generation() {
    let ((), report) = run_async_under_lab(0x5ec0_6101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query(); let commit = c.commit();
        let mut db = database(&commit).await; let issuer = issuer(701);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100).unwrap();
        let params = GqlParameters::new();
        let statements = [
            ("MATCH (n) RETURN n.p AS p", &params),
            ("MATCH (n) RETURN sum(n.p) AS total, count(*) AS rows", &params),
            ("MATCH (n) WITH n.p AS p RETURN sum(p) AS total, count(*) AS rows", &params),
            ("MATCH (n) RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p", &params),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p", &params),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN sum(n.p) AS total, count(*) AS rows", &params),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p UNION ALL MATCH (n) RETURN n.p AS p", &params),
        ];
        let summary = rows(&["total", "rows"], vec![vec![QueryValue::Integer(40), QueryValue::Count(2)]]);
        let union = rows(&["p"], vec![vec![value(10)], vec![value(10)], vec![value(30)], vec![value(30)]]);
        let expected = vec![visible(), summary.clone(), summary.clone(), union.clone(), visible(), summary, union];
        assert_eq!(session.query_batch(&cx, &statements).unwrap(), expected);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        db.write(&commit, change).await.unwrap();
        db.compact(&commit).await.unwrap();
        assert_eq!(session.query_batch(&cx, &statements).unwrap(), expected);
        let future = [("RETURN 1 AS one", &params),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p", &params)];
        assert!(matches!(session.query_batch(&cx, &future),
            Err(QueryError::Read(ReadError::BeyondFrontier { asked: CommitSeq(2), frontier: CommitSeq(1) }))));
        drop(db);
        assert_eq!(session.query_batch(&cx, &statements).unwrap(), expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_combined_delivery_and_source_limits_do_not_reset_between_statements() {
    let ((), report) = run_async_under_lab(0x5ec0_6102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = issuer(702);
        let mut permitted = grant(); permitted.limits.max_rows = 3; permitted.limits.max_nodes = 4;
        let token = issuer.issue_at(&permitted, 100).unwrap();
        let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100).unwrap();
        let params = GqlParameters::new();
        let pair = [("MATCH (n) RETURN n.p AS p", &params), ("MATCH (n) RETURN count(*) AS count", &params)];
        let expected = vec![visible(), rows(&["count"], vec![vec![QueryValue::Count(2)]])];
        assert_eq!(session.query_batch(&cx, &pair).unwrap(), expected);
        // Each query fits alone. The third result does not fit their shared
        // delivery allowance; no successful prefix of the batch is returned.
        refusal(session.query_batch(&cx, &[pair[0], pair[1], ("RETURN 1 AS one", &params)]),
            AuthorizationError::LimitExceeded(LimitDimension::Rows));
        assert!(!session.is_closed());
        assert_eq!(session.query_batch(&cx, &pair).unwrap(), expected);
        refusal(session.query_batch(&cx, &[pair[1], pair[1], pair[1]]),
            AuthorizationError::LimitExceeded(LimitDimension::Nodes));
        assert!(!session.is_closed());
        assert_eq!(session.query_batch(&cx, &pair).unwrap(), expected);
        // Native limits remain per statement even though the signed ceiling
        // is cumulative. A zero native result cap is never replaced by the
        // larger signed limit or by another statement's unused row capacity.
        let mut zero = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols,
            GqlQueryPolicy::new(1000, 0, 1_000_000, 1_000_000), || 100).unwrap();
        assert!(zero.query_batch(&cx, &pair).is_err());
        let empty = [("RETURN 1 AS one LIMIT 0", &params); 2];
        assert!(zero.query_batch(&cx, &empty).unwrap().iter().all(|result|
            matches!(result, QueryResult::Rows { rows, .. } if rows.is_empty())));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_batches_rebind_repeated_handles_without_catalog_callbacks_or_double_row_charges() {
    let ((), report) = run_async_under_lab(0x5ec0_6103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = issuer(703);
        let mut grant = grant(); grant.limits.max_rows = 3;
        let token = issuer.issue_at(&grant, 100).unwrap();
        let resolutions = Cell::new(0);
        let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH,
            |kind, name: &str| { resolutions.set(resolutions.get() + 1); symbols(kind, name) }, policy(), || 100).unwrap();
        let low = GqlParameters::new().with_int64("minimum", 0).unwrap();
        let mid = GqlParameters::new().with_int64("minimum", 20).unwrap();
        let high = GqlParameters::new().with_int64("minimum", 40).unwrap();
        let prepared = session.prepare(&cx, "MATCH (n) WHERE n.p >= $minimum RETURN n.p AS p", &low).unwrap();
        let frozen = resolutions.get();
        let batch = [(&prepared, &low), (&prepared, &mid), (&prepared, &high)];
        let expected = vec![visible(), rows(&["p"], vec![vec![value(30)]]), rows(&["p"], vec![])];
        assert_eq!(session.execute_batch(&cx, &batch).unwrap(), expected);
        assert_eq!(resolutions.get(), frozen);
        let unknown = high.clone().with_int64("unknown", 7).unwrap();
        assert!(session.execute_batch(&cx, &[(&prepared, &low), (&prepared, &mid), (&prepared, &unknown)]).is_err());
        assert!(!session.is_closed());
        assert_eq!(session.execute_batch(&cx, &batch).unwrap(), expected);
        assert_eq!(resolutions.get(), frozen);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_prepared_owner_and_branch_are_checked_before_the_first_source() {
    let ((), report) = run_async_under_lab(0x5ec0_6104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = issuer(704);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let denied = GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000);
        let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, denied, || 100).unwrap();
        let mut other = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, denied, || 100).unwrap();
        let params = GqlParameters::new();
        let own = session.prepare(&cx, "MATCH (n) RETURN n.p AS p", &params).unwrap();
        let foreign = other.prepare(&cx, "RETURN 1 AS one LIMIT 0", &params).unwrap();
        // Executing the first source would refuse its zero native allowance.
        // Exact-owner admission must instead find the later foreign handle.
        refusal(session.execute_batch(&cx, &[(&own, &params), (&foreign, &params)]), AuthorizationError::WrongAuthority);
        let branch = GqlParameters::new().with_text("route", BRANCH).unwrap();
        let wrong = GqlParameters::new().with_text("route", "elsewhere").unwrap();
        let selected = session.prepare(&cx, "AT BRANCH $route RETURN 1 AS one LIMIT 0", &branch).unwrap();
        refusal(session.execute_batch(&cx, &[(&own, &params), (&selected, &wrong)]), AuthorizationError::ScopeDenied);
        // With preflight satisfied, the original native source refusal is kept.
        assert!(matches!(session.execute_batch(&cx, &[(&own, &params), (&selected, &branch)]),
            Err(QueryError::Pattern(GqlQueryError::Rows(_)))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_and_maximum_batches_are_bounded_and_still_authenticate() {
    let ((), report) = run_async_under_lab(0x5ec0_6105, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = issuer(705);
        let mut grant = grant(); grant.limits.max_nodes = 0; grant.limits.max_rows = MAX_STATEMENTS as u64;
        let token = issuer.issue_at(&grant, 100).unwrap();
        let now = Cell::new(100);
        let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || now.get()).unwrap();
        let params = GqlParameters::new();
        assert!(session.query_batch(&cx, &[]).unwrap().is_empty());
        assert!(session.execute_batch(&cx, &[]).unwrap().is_empty());
        let max = vec![("RETURN 1 AS one", &params); MAX_STATEMENTS];
        assert_eq!(session.query_batch(&cx, &max).unwrap(), vec![rows(&["one"], vec![vec![value(1)]]); MAX_STATEMENTS]);
        let oversized = vec![("RETURN 1 AS one", &params); MAX_STATEMENTS + 1];
        assert!(matches!(session.query_batch(&cx, &oversized), Err(QueryError::Unsupported { .. })));
        assert!(!session.is_closed());
        let prepared = session.prepare(&cx, "RETURN 1 AS one", &params).unwrap();
        assert!(matches!(session.execute_batch(&cx, &vec![(&prepared, &params); MAX_STATEMENTS + 1]),
            Err(QueryError::Unsupported { .. })));
        now.set(1000);
        refusal(session.query_batch(&cx, &[]), AuthorizationError::Expired);
        assert!(session.is_closed());
        refusal(session.execute_batch(&cx, &[]), AuthorizationError::ExecutionStopped);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_errors_discard_completed_results_and_do_not_run_later_statements_or_writes() {
    let ((), report) = run_async_under_lab(0x5ec0_6106, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = issuer(706);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let reached_last = Cell::new(false);
        let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH,
            |kind, name: &str| { if name == "late_marker" { reached_last.set(true); } symbols(kind, name) }, policy(), || 100).unwrap();
        let params = GqlParameters::new(); let before = db.frontier().unwrap();
        for failing in ["UNWIND [1, 0] AS x RETURN 10 / x AS quotient LIMIT 0", "INSERT (n:L)"] {
            let batch = [("RETURN 1 AS one", &params), (failing, &params), ("MATCH (n:late_marker) RETURN n", &params)];
            assert!(session.query_batch(&cx, &batch).is_err());
            assert!(!reached_last.get()); assert!(!session.is_closed());
            assert_eq!(db.frontier().unwrap(), before);
        }
        assert_eq!(session.query_batch(&cx, &[("RETURN 1 AS one", &params)]).unwrap(),
            vec![rows(&["one"], vec![vec![value(1)]])]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_batch_expiry_cut_keeps_all_results_private_and_closes_the_session() {
    let ((), report) = run_async_under_lab(0x5ec0_6107, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root); let cx = c.query();
        let db = database(&c.commit()).await; let issuer = issuer(707);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0); let cutoff = Cell::new(usize::MAX);
        let clock = || { let at = calls.get() + 1; calls.set(at); if at >= cutoff.get() { 1000 } else { 100 } };
        let params = GqlParameters::new();
        let batch = [("RETURN 1 AS one", &params), ("MATCH (n:L) RETURN count(*) AS rows", &params)];
        let refs = Arc::strong_count(&db.snapshot);
        for prepared_mode in [false, true] {
            cutoff.set(usize::MAX); calls.set(0);
            let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), clock).unwrap();
            let a = session.prepare(&cx, batch[0].0, &params).unwrap();
            let b = session.prepare(&cx, batch[1].0, &params).unwrap();
            calls.set(0);
            if prepared_mode { session.execute_batch(&cx, &[(&a, &params), (&b, &params)]).unwrap(); }
            else { session.query_batch(&cx, &batch).unwrap(); }
            let cuts = calls.get(); session.close();
            for stop in 1..=cuts {
                cutoff.set(usize::MAX); calls.set(0);
                let mut session = db.authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), clock).unwrap();
                let a = session.prepare(&cx, batch[0].0, &params).unwrap();
                let b = session.prepare(&cx, batch[1].0, &params).unwrap();
                calls.set(0); cutoff.set(stop);
                let result = if prepared_mode { session.execute_batch(&cx, &[(&a, &params), (&b, &params)]) }
                    else { session.query_batch(&cx, &batch) };
                refusal(result, AuthorizationError::Expired);
                assert!(session.is_closed());
                assert_eq!(Arc::strong_count(&db.snapshot), refs);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
