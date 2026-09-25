//! Completion checks must not erase the cause needed for session retirement.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x63; 32]);
pub(super) const BRANCH: &str = "batch-owner";
pub(super) fn issuer(seed: u64) -> Authority {
    Authority::new(AuthKey::from_seed(seed), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
pub(super) fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        1000,
        QueryLimits {
            max_nodes: 1000,
            max_work: 1_000_000,
            max_rows: 1000,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}
pub(super) fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
pub(super) fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
pub(super) async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x37; 32], NS, [0x95; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, label, p) in [(1, 1, 10), (2, 99, 20), (3, 1, 30)] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(label)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(p))],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}
pub(super) fn refusal<T>(result: Result<T, QueryError>, expected: AuthorizationError) {
    // ubs:ignore -- test assertion on an authorization error value, not secret material.
    assert!(matches!(result, Err(QueryError::Authorization(actual)) if actual == expected));
}

#[test]
fn first_authorization_cause_is_stable_and_no_failed_permit_resumes() {
    let ((), report) = run_async_under_lab(0x5ec0_6001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        for (case, expected) in [
            (0, AuthorizationError::Expired),
            (1, AuthorizationError::ClockWentBackwards),
            (2, AuthorizationError::AuthorityRetired),
            (3, AuthorizationError::LimitExceeded(LimitDimension::Work)),
            (4, AuthorizationError::LimitExceeded(LimitDimension::Nodes)),
            (5, AuthorizationError::LimitExceeded(LimitDimension::Rows)),
        ] {
            let issuer = issuer(600 + case);
            let mut grant = grant();
            match case {
                3 => grant.limits.max_work = 0,
                4 => grant.limits.max_nodes = 0,
                5 => grant.limits.max_rows = 0,
                _ => {}
            }
            let token = issuer.issue_at(&grant, 100).unwrap();
            let verified = issuer.verify_at(&token, BRANCH, 100).unwrap();
            let permit = verified.begin_read_at(BRANCH, 100).unwrap();
            let now = Cell::new(100);
            let calls = Cell::new(0);
            let mut execution = Execution::new(&cx, permit, || {
                calls.set(calls.get() + 1);
                now.get()
            });
            match case {
                0 => now.set(1000),
                1 => now.set(99),
                2 => assert!(issuer.retire(), "first retirement of a fresh issuer"),
                _ => {}
            }
            let result = match case {
                4 => execution.node(),
                5 => execution.deliver(1),
                _ => execution.checkpoint(),
            };
            refusal(result, expected);
            let used = execution.permit.usage();
            let sampled = calls.get();
            for _ in 0..4 {
                refusal(execution.checkpoint(), expected);
                refusal(execution.node(), expected);
                refusal(execution.deliver(0), expected);
                assert_eq!(execution.permit.usage(), used);
                assert_eq!(
                    calls.get(),
                    sampled,
                    "failed execution must not call host code again"
                );
            }
            assert_eq!(
                execution.permit.checkpoint_at(100),
                Err(AuthorizationError::ExecutionStopped),
                "the original Warden permit remains terminal; retaining a cause grants no retry"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_text_and_prepared_expiry_boundary_closes_and_releases_the_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_6002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = issuer(610);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0);
        let cutoff = Cell::new(usize::MAX);
        let clock = || {
            let next = calls.get() + 1;
            calls.set(next);
            if next >= cutoff.get() { 1000 } else { 100 }
        };
        let params = GqlParameters::new();
        let text = "MATCH (n:L) RETURN n.p AS p UNION ALL MATCH (n:L) RETURN n.p AS p";
        let baseline_refs = Arc::strong_count(&db.snapshot);
        for prepared_mode in [false, true] {
            cutoff.set(usize::MAX);
            calls.set(0);
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), clock)
                .unwrap();
            let prepared = session.prepare(&cx, text, &params).unwrap();
            calls.set(0);
            if prepared_mode {
                session.execute(&cx, &prepared, &params).unwrap();
            } else {
                session.query(&cx, text, &params).unwrap();
            }
            let checkpoints = calls.get();
            session.close();
            for stop in 1..=checkpoints {
                cutoff.set(usize::MAX);
                calls.set(0);
                let mut session = db
                    .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), clock)
                    .unwrap();
                let prepared = session.prepare(&cx, text, &params).unwrap();
                calls.set(0);
                cutoff.set(stop);
                let result = if prepared_mode {
                    session.execute(&cx, &prepared, &params)
                } else {
                    session.query(&cx, text, &params)
                };
                refusal(result, AuthorizationError::Expired);
                assert!(
                    session.is_closed(),
                    "expiry at checkpoint {stop} must close immediately"
                );
                assert_eq!(
                    Arc::strong_count(&db.snapshot),
                    baseline_refs,
                    "a prepared handle must not retain the retired generation"
                );
                let sampled = calls.get();
                refusal(
                    session.query(&cx, text, &params),
                    AuthorizationError::ExecutionStopped,
                );
                assert_eq!(calls.get(), sampled);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_quota_refusals_keep_the_actual_dimension_and_allow_a_new_statement() {
    let ((), report) = run_async_under_lab(0x5ec0_6003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = issuer(611);
        for dimension in [LimitDimension::Nodes, LimitDimension::Rows] {
            let mut grant = grant();
            match dimension {
                LimitDimension::Nodes => grant.limits.max_nodes = 1,
                LimitDimension::Rows => grant.limits.max_rows = 1,
                _ => unreachable!(),
            }
            let token = issuer.issue_at(&grant, 100).unwrap();
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
                .unwrap();
            let params = GqlParameters::new();
            refusal(
                session.query(&cx, "MATCH (n) RETURN n.p AS p", &params),
                AuthorizationError::LimitExceeded(dimension),
            );
            assert!(!session.is_closed());
            assert!(
                matches!(session.query(&cx, "RETURN 1 AS one", &params).unwrap(),
                QueryResult::Rows { rows, .. } if rows.len() == 1)
            );
            refusal(
                session.query(&cx, "MATCH (n) RETURN n.p AS p", &params),
                AuthorizationError::LimitExceeded(dimension),
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retirement_in_successful_or_failing_catalog_callback_closes_in_the_same_call() {
    let ((), report) = run_async_under_lab(0x5ec0_6004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let refs = Arc::strong_count(&db.snapshot);
        for valid_symbol in [false, true] {
            let issuer = issuer(612);
            let token = issuer.issue_at(&grant(), 100).unwrap();
            let called = Cell::new(false);
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    |kind, name: &str| {
                        called.set(true);
                        issuer.retire();
                        if valid_symbol {
                            symbols(kind, name)
                        } else {
                            None
                        }
                    },
                    policy(),
                    || 100,
                )
                .unwrap();
            refusal(
                session.query(&cx, "MATCH (n:L) RETURN n", &GqlParameters::new()),
                AuthorizationError::AuthorityRetired,
            );
            assert!(called.get());
            assert!(session.is_closed());
            assert_eq!(Arc::strong_count(&db.snapshot), refs);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
