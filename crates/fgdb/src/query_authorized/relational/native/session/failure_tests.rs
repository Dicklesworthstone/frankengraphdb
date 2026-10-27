//! Completion checks must not erase the cause needed for session retirement.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts,
};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

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

#[test]
fn result_snapshot_metadata_tracks_temporal_binding_and_the_session_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_6005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = issuer(640);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut pinned = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let value = |n| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)));
        // RETURN without ORDER BY is canonical row order (owner ruling 2026-10-09).
        let expected = |first: i64| {
            let mut values = [first, 30];
            values.sort_unstable();
            QueryResult::Rows {
                columns: vec!["p".to_owned()],
                rows: values.into_iter().map(|n| vec![value(n)]).collect(),
            }
        };
        let plain = "MATCH (n) RETURN n.p AS p";
        let params = GqlParameters::new();
        assert_eq!(
            pinned.query_with_snapshot(&cx, plain, &params).unwrap(),
            (expected(10), CommitSeq(1))
        );

        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            pinned.query_with_snapshot(&cx, plain, &params).unwrap(),
            (expected(10), CommitSeq(1)),
            "ordinary result metadata belongs to the pin, not the live writer"
        );
        let mut current = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        assert_eq!(
            current.query_with_snapshot(&cx, plain, &params).unwrap(),
            (expected(99), CommitSeq(2))
        );

        let temporal = "AT BRANCH $route MATCH (n) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p AS p";
        let arguments = |at| {
            GqlParameters::new()
                .with_text("route", BRANCH)
                .unwrap()
                .with_uint64("at", at)
                .unwrap()
        };
        let prepared = current.prepare(&cx, temporal, &arguments(2)).unwrap();
        for (at, first) in [(1, 10), (2, 99), (1, 10)] {
            let arguments = arguments(at);
            let answer = (expected(first), CommitSeq(at));
            assert_eq!(
                current
                    .query_with_snapshot(&cx, temporal, &arguments)
                    .unwrap(),
                answer
            );
            assert_eq!(
                current
                    .execute_with_snapshot(&cx, &prepared, &arguments)
                    .unwrap(),
                answer
            );
        }
        assert!(matches!(
            pinned.query_with_snapshot(&cx, temporal, &arguments(2)),
            Err(QueryError::Read(ReadError::BeyondFrontier {
                asked: CommitSeq(2),
                frontier: CommitSeq(1)
            }))
        ));
        assert!(matches!(
            current.execute_with_snapshot(&cx, &prepared, &arguments(3)),
            Err(QueryError::Read(ReadError::BeyondFrontier {
                asked: CommitSeq(3),
                frontier: CommitSeq(2)
            }))
        ));
        refusal(
            pinned.execute_with_snapshot(&cx, &prepared, &arguments(1)),
            AuthorizationError::WrongAuthority,
        );

        let (_, explained_at) = current
            .query_with_snapshot(&cx, "EXPLAIN MATCH (n) RETURN n.p AS p", &params)
            .unwrap();
        assert_eq!(explained_at, CommitSeq(2));
        current.close();
        refusal(
            current.query_with_snapshot(&cx, temporal, &arguments(1)),
            AuthorizationError::ExecutionStopped,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn retained_read(
    text: &str,
    params: &GqlParameters,
) -> (Arc<PreparedGraphBranchText>, Arc<PreparedNativeRead>) {
    let selector = Arc::new(PreparedGraphBranchText::prepare(text).unwrap());
    let native = {
        let selected = selector.bind_parameters(params).unwrap();
        PreparedNativeRead::prepare(selected.statement(), selected.parameters(), symbols).unwrap()
    };
    (selector, Arc::new(native))
}

fn read_values(
    cursor: impl Iterator<Item = Result<GraphValueRow, QueryError>>,
) -> Vec<Vec<GraphValue>> {
    cursor.map(|row| row.unwrap().values().to_vec()).collect()
}

#[test]
fn prepared_map_fields_rebind_inside_the_original_authority_and_snapshot() {
    let ((), report) = run_async_under_lab(0x5ec0_6021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = database(&contexts.commit()).await;
        let issuer = issuer(641);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let resolutions = AtomicUsize::new(0);
        let now = AtomicUsize::new(100);
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                BRANCH,
                |kind, name: &str| {
                    resolutions.fetch_add(1, Ordering::Relaxed);
                    symbols(kind, name)
                },
                policy(),
                || now.load(Ordering::Relaxed) as u64,
            )
            .unwrap();
        let arguments = |value: Option<i64>, route: &str| {
            GqlParameters::new()
                .with_map(
                    "m",
                    value
                        .map(|value| {
                            vec![(
                                "key".into(),
                                GraphValue::Scalar(CanonicalScalar::Int(value)),
                            )]
                        })
                        .unwrap_or_default(),
                )
                .unwrap()
                .with_text("route", route)
                .unwrap()
        };
        let text = "AT BRANCH $route MATCH (n:L {p: $m.key}) RETURN n.p AS p";
        let initial = arguments(Some(10), BRANCH);
        let prepared = session.prepare(&cx, text, &initial).unwrap();
        let aggregate = session
            .prepare(
                &cx,
                "AT BRANCH $route MATCH (n:L {p: $m.key}) RETURN count(*) AS count",
                &initial,
            )
            .unwrap();
        assert_eq!(
            prepared.parameter_schema(),
            &[fgdb_gql::GqlParameterSpec {
                name: "m".into(),
                parameter_type: fgdb_gql::GqlParameterType::Map,
                requires_positive: false,
                occurrences: 1,
            }]
        );
        let frozen = resolutions.load(Ordering::Relaxed);
        let expected = |value: Option<i64>| QueryResult::Rows {
            columns: vec!["p".into()],
            rows: value
                .map(|value| {
                    vec![vec![crate::QueryValue::Value(GraphValue::Scalar(
                        CanonicalScalar::Int(value),
                    ))]]
                })
                .unwrap_or_default(),
        };
        for (value, visible) in [(Some(30), Some(30)), (Some(20), None), (None, None)] {
            let args = arguments(value, BRANCH);
            assert_eq!(
                session.execute(&cx, &prepared, &args).unwrap(),
                expected(visible)
            );
            let values = visible
                .map(|value| vec![vec![GraphValue::Scalar(CanonicalScalar::Int(value))]])
                .unwrap_or_default();
            assert_eq!(
                read_values(session.stream(&cx, &prepared, &args).unwrap()),
                values
            );
            assert_eq!(
                read_values(session.stream_send(&cx, &prepared, &args).unwrap()),
                values
            );
            let mut cursor = session.stream_aggregate(&cx, &aggregate, &args).unwrap();
            let row = cursor.next().unwrap().unwrap();
            assert_eq!(
                row.values()[0].as_count(),
                Some(u64::from(visible.is_some()))
            );
            assert!(cursor.next().is_none());
        }
        let other = arguments(Some(30), BRANCH);
        assert_eq!(
            session
                .execute_batch(&cx, &[(&prepared, &initial), (&prepared, &other)])
                .unwrap(),
            vec![expected(Some(10)), expected(Some(30))]
        );
        let changes = session
            .execute_diff(&cx, &prepared, &other, CommitSeq(0), CommitSeq(1))
            .unwrap();
        assert_eq!(changes.changes().len(), 1);
        let (row, weight) = changes.changes().iter().next().unwrap();
        assert_eq!(
            row,
            &[crate::QueryValue::Value(GraphValue::Scalar(
                CanonicalScalar::Int(30)
            ))]
        );
        assert_eq!(weight.to_i128(), Some(1));
        let wrong = GqlParameters::new().with_int64("m", 30).unwrap();
        assert!(session.execute(&cx, &prepared, &wrong).is_err());
        assert!(!session.is_closed());
        refusal(
            session.execute(&cx, &prepared, &arguments(Some(30), "other")),
            AuthorizationError::ScopeDenied,
        );
        refusal(
            session.execute(
                &cx,
                &prepared,
                &wrong.clone().with_text("route", "other").unwrap(),
            ),
            AuthorizationError::ScopeDenied,
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(300)));
        db.write(&contexts.commit(), change).await.unwrap();
        assert_eq!(
            session
                .execute_with_snapshot(&cx, &prepared, &other)
                .unwrap(),
            (expected(Some(30)), CommitSeq(1))
        );
        assert_eq!(resolutions.load(Ordering::Relaxed), frozen);
        let mut foreign = db
            .authorized_read_session(&cx, &issuer, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        refusal(
            foreign.execute(&cx, &prepared, &GqlParameters::new()),
            AuthorizationError::WrongAuthority,
        );
        now.store(1000, Ordering::Relaxed);
        refusal(
            session.execute(&cx, &prepared, &wrong),
            AuthorizationError::Expired,
        );
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn all_native_host_facades_keep_branch_framing_and_one_trailing_terminator() {
    let ((), report) = run_async_under_lab(0x5ec0_6009, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = issuer(619);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for (class, body) in [
            (crate::NativeReadClass::Pattern, "MATCH (n:L) RETURN n"),
            (
                crate::NativeReadClass::Aggregate,
                "MATCH (n:L) RETURN count(*) AS total",
            ),
            (
                crate::NativeReadClass::PipelineAggregate,
                "MATCH (n:L) WITH n.p AS p RETURN count(*) AS total",
            ),
            (
                crate::NativeReadClass::Set,
                "MATCH (n:L) RETURN n UNION ALL MATCH (n:L) RETURN n",
            ),
            (
                crate::NativeReadClass::TemporalPattern,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN n",
            ),
            (
                crate::NativeReadClass::TemporalSet,
                concat!(
                    "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN n ",
                    "UNION ALL MATCH (n:L) RETURN n"
                ),
            ),
            (
                crate::NativeReadClass::TemporalAggregate,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN count(*) AS total",
            ),
        ] {
            let mut params = GqlParameters::new().with_text("route", BRANCH).unwrap();
            if matches!(
                class,
                crate::NativeReadClass::TemporalPattern
                    | crate::NativeReadClass::TemporalSet
                    | crate::NativeReadClass::TemporalAggregate
            ) {
                params = params.with_uint64("at", 1).unwrap();
            }
            let text = format!("AT BRANCH $route {body};\n");
            let (selector, native) = retained_read(&text, &params);
            assert_eq!(native.facade_class(), class);
            let (mut session, prepared) = db
                .authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    policy(),
                    || 100,
                    selector,
                    native,
                )
                .unwrap();
            assert_eq!(prepared.facade_class(), class);
            let (result, snapshot) = session
                .execute_with_snapshot(&cx, &prepared, &params)
                .unwrap();
            assert_eq!(snapshot, CommitSeq(1));
            assert_eq!(result, session.query(&cx, &text, &params).unwrap());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retained_host_templates_share_metadata_but_earn_fresh_pins_and_exact_session_owners() {
    let ((), report) = run_async_under_lab(0x5ec0_6010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = issuer(620);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new().with_text("route", BRANCH).unwrap();
        let text = "AT BRANCH $route MATCH (n:L) RETURN n AS node,n.p AS p;";
        let (selector, native) = retained_read(text, &params);
        let calls = AtomicUsize::new(0);
        let resolver = |kind, name: &str| {
            calls.fetch_add(1, Ordering::Relaxed);
            symbols(kind, name)
        };
        let (mut first, first_handle) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                BRANCH,
                resolver,
                policy(),
                || 100,
                Arc::clone(&selector),
                Arc::clone(&native),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&first_handle.selector, &selector));
        assert!(Arc::ptr_eq(&first_handle.native, &native));
        let expected = vec![
            vec![
                GraphValue::Vertex(VId(1)),
                GraphValue::Scalar(CanonicalScalar::Int(10)),
            ],
            vec![
                GraphValue::Vertex(VId(3)),
                GraphValue::Scalar(CanonicalScalar::Int(30)),
            ],
        ];
        let cursor = first.stream_send(&cx, &first_handle, &params).unwrap();
        assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
        assert_eq!(read_values(cursor), expected);

        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(4),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(40))],
        );
        db.write(&commit, batch).await.unwrap();
        let latest_refs = Arc::strong_count(&db.snapshot);
        let (mut second, second_handle) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                BRANCH,
                resolver,
                policy(),
                || 100,
                Arc::clone(&selector),
                Arc::clone(&native),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&second_handle.selector, &selector));
        assert!(Arc::ptr_eq(&second_handle.native, &native));
        refusal(
            second.stream_send(&cx, &first_handle, &params),
            AuthorizationError::WrongAuthority,
        );
        refusal(
            first.execute(&cx, &second_handle, &params),
            AuthorizationError::WrongAuthority,
        );
        assert_eq!(
            read_values(first.stream_send(&cx, &first_handle, &params).unwrap()),
            expected,
            "an existing session keeps its original pin after the writer advances"
        );
        let wrong_branch = GqlParameters::new()
            .with_text("route", "elsewhere")
            .unwrap();
        refusal(
            second.stream_send(&cx, &second_handle, &wrong_branch),
            AuthorizationError::ScopeDenied,
        );
        let cursor = second.stream_send(&cx, &second_handle, &params).unwrap();
        assert_eq!(cursor.snapshot_seq(), CommitSeq(2));
        let mut current = expected.clone();
        current.push(vec![
            GraphValue::Vertex(VId(4)),
            GraphValue::Scalar(CanonicalScalar::Int(40)),
        ]);
        assert_eq!(read_values(cursor), current);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "import and rebinding never reopen the catalog"
        );
        first.close();
        second.close();
        assert_eq!(
            Arc::strong_count(&db.snapshot),
            latest_refs,
            "retained session handles share templates but keep no snapshot pin"
        );
        assert!(Arc::ptr_eq(&first_handle.native, &second_handle.native));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_template_import_checks_source_coherence_and_identity_after_authentication() {
    let ((), report) = run_async_under_lab(0x5ec0_6011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = issuer(621);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let (selector, native) = retained_read("MATCH (n:L) RETURN n; \n", &params);
        let refs = Arc::strong_count(&db.snapshot);
        let (mut session, prepared) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                BRANCH,
                symbols,
                policy(),
                || 100,
                Arc::clone(&selector),
                Arc::clone(&native),
            )
            .unwrap();
        assert_eq!(
            read_values(session.stream_send(&cx, &prepared, &params).unwrap()).len(),
            2
        );
        session.close();
        assert_eq!(Arc::strong_count(&db.snapshot), refs);

        let mismatch =
            Arc::new(PreparedGraphBranchText::prepare("MATCH (n:L) RETURN n.p AS p").unwrap());
        for (now, expected) in [
            (100, AuthorizationError::WrongAuthority),
            (1000, AuthorizationError::Expired),
        ] {
            refusal(
                db.authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    policy(),
                    || now,
                    Arc::clone(&mismatch),
                    Arc::clone(&native),
                ),
                expected,
            );
            assert_eq!(Arc::strong_count(&db.snapshot), refs);
        }
        for function in ["id", "elementId"] {
            let text = format!("MATCH (n:L) RETURN {function}(n) AS identity");
            let (identity_selector, identity_native) = retained_read(&text, &params);
            refusal(
                db.authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    policy(),
                    || 100,
                    Arc::clone(&selector),
                    Arc::clone(&identity_native),
                ),
                AuthorizationError::WrongAuthority,
            );
            refusal(
                db.authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    policy(),
                    || 100,
                    identity_selector,
                    identity_native,
                ),
                AuthorizationError::ScopeDenied,
            );
            assert_eq!(Arc::strong_count(&db.snapshot), refs);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_host_template_constructor_expiry_cut_releases_its_new_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_6012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = issuer(622);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let (selector, native) = retained_read("MATCH (n:L) RETURN n", &GqlParameters::new());
        let refs = Arc::strong_count(&db.snapshot);
        let calls = Cell::new(0);
        let cutoff = Cell::new(usize::MAX);
        let clock = || {
            let next = calls.get() + 1;
            calls.set(next);
            if next >= cutoff.get() { 1000 } else { 100 }
        };
        let admitted = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                BRANCH,
                symbols,
                policy(),
                clock,
                Arc::clone(&selector),
                Arc::clone(&native),
            )
            .unwrap();
        let checkpoints = calls.get();
        assert!(checkpoints > 1);
        drop(admitted);
        assert_eq!(Arc::strong_count(&db.snapshot), refs);
        for stop in 1..=checkpoints {
            calls.set(0);
            cutoff.set(stop);
            refusal(
                db.authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    policy(),
                    clock,
                    Arc::clone(&selector),
                    Arc::clone(&native),
                ),
                AuthorizationError::Expired,
            );
            assert_eq!(
                Arc::strong_count(&db.snapshot),
                refs,
                "constructor expiry at checkpoint {stop} must release its new pin"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn imported_templates_use_current_scope_and_keep_stream_credentials_live() {
    let ((), report) = run_async_under_lab(0x5ec0_6013, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = issuer(623);
        let params = GqlParameters::new();
        let (selector, native) = retained_read(
            "MATCH (n) WHERE n.p >= 0 RETURN n AS node,n.p AS p",
            &params,
        );
        let refs = Arc::strong_count(&db.snapshot);
        for (case, rows) in [(0, 3), (1, 2), (2, 0)] {
            let mut scope = grant();
            if case == 0 {
                scope.labels = Scope::All;
            } else if case == 2 {
                scope.properties = Scope::only([]);
            }
            let token = issuer.issue_at(&scope, 100).unwrap();
            let (mut session, prepared) = db
                .authorized_prepared_read_session(
                    &cx,
                    &issuer,
                    &token,
                    BRANCH,
                    symbols,
                    policy(),
                    || 100,
                    Arc::clone(&selector),
                    Arc::clone(&native),
                )
                .unwrap();
            assert_eq!(
                read_values(session.stream_send(&cx, &prepared, &params).unwrap()).len(),
                rows
            );
            session.close();
            assert_eq!(Arc::strong_count(&db.snapshot), refs);
        }

        let token = issuer.issue_at(&grant(), 100).unwrap();
        let now = Cell::new(100);
        let (mut session, prepared) = db
            .authorized_prepared_read_session(
                &cx,
                &issuer,
                &token,
                BRANCH,
                symbols,
                policy(),
                || now.get(),
                selector,
                native,
            )
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &params).unwrap();
        assert!(cursor.next().unwrap().is_ok());
        now.set(1000);
        refusal(cursor.next().unwrap(), AuthorizationError::Expired);
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(session.is_closed());
        assert_eq!(Arc::strong_count(&db.snapshot), refs);
        refusal(
            session.stream(&cx, &prepared, &params),
            AuthorizationError::ExecutionStopped,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
