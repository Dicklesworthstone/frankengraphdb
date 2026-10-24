use super::*;
use crate::{DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;
use std::collections::BTreeMap;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x59; 32]);
const BRANCH: &str = "main";
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const ROWS: &str = "MATCH (n) RETURN n.p AS p";
const MULTI: &str = "MATCH (a) WITH a.p AS x MATCH (b) WITH x, b.p AS y \
    RETURN COUNT(*) AS n, SUM(y) AS total, AVG(y) AS average";
type Bag = BTreeMap<Vec<QueryValue>, i128>;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(5901), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        BRANCH,
        1000,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 100_000_000,
            max_rows: 100_000,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([R]);
    grant.properties = Scope::only([P]);
    grant
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
async fn history(cx: &CommitCx, hidden: bool) -> (Database<MemVfs>, [CommitSeq; 3]) {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x19; 32], NS, [0x29; 32]))
        .await
        .unwrap();
    let mut seed = WriteBatch::new(R);
    for (id, value) in [(1, 3), (2, 3), (3, 7)] {
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        if hidden {
            properties.push((H, CanonicalScalar::Int(9000 + value)));
        }
        seed.create_vertex(VId(id), vec![LabelId(1)], properties);
    }
    seed.add_edge(EId(1), VId(1), VId(2), vec![]);
    seed.add_edge(EId(2), VId(2), VId(3), vec![]);
    if hidden {
        seed.create_vertex(
            VId(99),
            vec![LabelId(99)],
            vec![(P, CanonicalScalar::Int(9999))],
        );
        seed.add_edge(EId(99), VId(2), VId(99), vec![]);
    }
    let first = db.write(cx, seed).await.unwrap();
    let mut edit = WriteBatch::new(R);
    edit.delete_vertex(VId(1));
    if hidden {
        edit.set_vertex_property(VId(99), P, Some(CanonicalScalar::Int(8888)));
    }
    let second = db.write(cx, edit).await.unwrap();
    let mut edit = WriteBatch::new(R);
    edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(2)));
    edit.create_vertex(VId(4), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(5))]);
    edit.add_edge(EId(3), VId(2), VId(4), vec![]);
    if hidden {
        edit.set_vertex_property(VId(2), H, Some(CanonicalScalar::Int(-7777)));
    }
    let third = db.write(cx, edit).await.unwrap();
    (db, [first, second, third])
}
fn bag(result: QueryResult) -> Bag {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("expected read rows");
    };
    let mut bag = Bag::new();
    for row in rows {
        *bag.entry(row).or_default() += 1;
    }
    bag
}
fn difference(before: &Bag, after: &Bag) -> Bag {
    let mut result = after.clone();
    for (row, count) in before {
        *result.entry(row.clone()).or_default() -= count;
    }
    result.retain(|_, count| *count != 0);
    result
}
fn delivered(result: &GraphResultChanges) -> Bag {
    result
        .changes()
        .iter()
        .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
        .collect()
}
fn authorization<T>(result: Result<T, Error>, expected: AuthorizationError) {
    assert!(matches!(
        result,
        Err(GqlQueryError::Interrupted(QueryError::Authorization(actual))) if actual == expected
    ));
}

#[test]
fn every_native_input_compares_both_masked_cuts_against_physically_removed_state() {
    let ((), report) = run_async_under_lab(0x5ec0_d101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let (visible, visible_cuts) = history(&contexts.commit(), false).await;
        assert_eq!(cuts, visible_cuts);
        let view = visible.read_session().unwrap();
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let mut scoped = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let mut removed = visible
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        for text in [
            ROWS,
            "MATCH (n) RETURN n.hidden AS secret",
            "MATCH (a)-[:R]->(b) RETURN a.p AS a, b.p AS b",
            "MATCH (n) RETURN COUNT(*) AS n, SUM(n.p) AS total, AVG(n.p) AS mean, \
                COLLECT(n.p) AS items, COLLECT(DISTINCT n.p) AS unique_items",
            "MATCH (n) WITH n.p AS x RETURN COUNT(*) AS n, SUM(x) AS total",
            MULTI,
            // A carried vertex's property is projected before OPTIONAL MATCH,
            // the spelling the pipeline grammar admits.
            "MATCH (a) WITH a, a.p AS ap OPTIONAL MATCH (a)-[:R]->(b) WITH ap AS x, b.p AS y \
                RETURN COUNT(*) AS n, COUNT(y) AS found, COLLECT(y) AS items",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
            "UNWIND [1,3,5] AS x RETURN COUNT(*) AS n, SUM(x) AS total",
        ] {
            let native = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            if text == MULTI {
                assert!(matches!(
                    &native,
                    PreparedNativeRead::PipelineAggregate(query) if query.requires_relational_input()
                ));
            }
            let expected: Vec<_> = cuts
                .iter()
                .map(|at| {
                    bag(native
                        .execute_in_view_at_seq(&view, &cx, &params, policy(), *at)
                        .unwrap())
                })
                .collect();
            let prepared = scoped.prepare(&cx, text, &params).unwrap();
            for (i, before) in cuts.iter().enumerate() {
                for (j, after) in cuts.iter().enumerate() {
                    let result = scoped
                        .execute_diff(&cx, &prepared, &params, *before, *after)
                        .unwrap();
                    assert_eq!(result.before(), *before);
                    assert_eq!(result.after(), *after);
                    assert_eq!(
                        delivered(&result),
                        difference(&expected[i], &expected[j]),
                        "{text}"
                    );
                    assert_eq!(
                        result,
                        scoped
                            .query_diff(&cx, text, &params, *before, *after)
                            .unwrap()
                    );
                    assert_eq!(
                        result,
                        removed
                            .query_diff(&cx, text, &params, *before, *after)
                            .unwrap()
                    );
                }
            }
        }
        // Independent arithmetic, not merely a differential with the same
        // query engine: 3x3 rows with column sum 3*(3+3+7), then 2x2 and 2*(3+7).
        let delta = scoped
            .query_diff(&cx, MULTI, &params, cuts[0], cuts[1])
            .unwrap();
        assert_eq!(delta.changes().len(), 2);
        for (row, weight) in delta.changes().iter() {
            match weight.to_i128().unwrap() {
                -1 => {
                    assert_eq!(row[0], QueryValue::Count(9));
                    assert_eq!(row[1], QueryValue::Integer(39));
                }
                1 => {
                    assert_eq!(row[0], QueryValue::Count(4));
                    assert_eq!(row[1], QueryValue::Integer(20));
                }
                other => panic!("unexpected aggregate weight {other}"),
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_diffs_rebind_without_catalog_calls_and_keep_the_original_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_d102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, cuts) = history(&contexts.commit(), true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        // Atomic, not a Cell: the session holds this resolver across an await
        // and the lab requires a Send future.
        let resolver_calls = std::sync::atomic::AtomicUsize::new(0);
        let resolver = |kind, name: &str| {
            resolver_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            symbols(kind, name)
        };
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, resolver, policy(), || 100)
            .unwrap();
        let initial = GqlParameters::new().with_int64("floor", 3).unwrap();
        let text = "MATCH (n) WHERE n.p >= $floor RETURN n.p AS p";
        let prepared = session.prepare(&cx, text, &initial).unwrap();
        let calls = resolver_calls.load(std::sync::atomic::Ordering::Relaxed);
        assert!(calls > 0);
        let rebound = GqlParameters::new().with_int64("floor", 6).unwrap();
        let result = session
            .execute_diff(&cx, &prepared, &rebound, cuts[0], cuts[2])
            .unwrap();
        assert_eq!(
            resolver_calls.load(std::sync::atomic::Ordering::Relaxed),
            calls
        );
        assert_eq!(
            delivered(&result),
            Bag::from([(
                vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                    7
                )))],
                -1,
            )])
        );
        let mut edit = WriteBatch::new(R);
        edit.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(700)));
        let future = db.write(&contexts.commit(), edit).await.unwrap();
        assert!(matches!(
            session.execute_diff(&cx, &prepared, &rebound, cuts[0], future),
            Err(GqlQueryError::Source(GraphDiffError::Endpoint {
                endpoint: DiffEndpoint::After,
                ..
            }))
        ));
        drop(db);
        assert_eq!(
            session
                .execute_diff(&cx, &prepared, &rebound, cuts[0], cuts[2])
                .unwrap(),
            result
        );
        assert_eq!(
            resolver_calls.load(std::sync::atomic::Ordering::Relaxed),
            calls
        );
        assert!(matches!(
            session.execute_diff(&cx, &prepared, &GqlParameters::new(), cuts[0], cuts[2]),
            Err(GqlQueryError::Source(GraphDiffError::Definition(
                QueryError::PatternText(_)
            )))
        ));
        assert!(!session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authority_owner_branch_identity_and_temporal_fences_precede_source_work() {
    let ((), report) = run_async_under_lab(0x5ec0_d103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let params = GqlParameters::new();
        let mut first = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let prepared = first.prepare(&cx, ROWS, &params).unwrap();
        let mut second = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        authorization(
            second.execute_diff(
                &cx,
                &prepared,
                &params,
                CommitSeq(u64::MAX),
                CommitSeq(u64::MAX),
            ),
            AuthorizationError::WrongAuthority,
        );
        let calls = Cell::new(0);
        let resolver = |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        };
        let mut zero = db
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                BRANCH,
                resolver,
                GqlQueryPolicy::new(0, 0, 0, 0),
                || 100,
            )
            .unwrap();
        for text in [
            "MATCH (n) RETURN id(n) AS identity LIMIT 0",
            "MATCH (n) RETURN elementId(n) AS identity LIMIT 0",
        ] {
            authorization(
                zero.query_diff(&cx, text, &params, cuts[0], cuts[0]),
                AuthorizationError::ScopeDenied,
            );
        }
        assert_eq!(calls.get(), 0);
        for text in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS n",
        ] {
            assert!(matches!(
                first.query_diff(&cx, text, &params, cuts[0], cuts[2]),
                Err(GqlQueryError::Source(GraphDiffError::TemporalSelector))
            ));
        }
        assert!(!first.is_closed());
        authorization(
            first.query_diff(
                &cx,
                "MATCH (n) AT BRANCH other RETURN n.p AS p",
                &params,
                cuts[0],
                cuts[2],
            ),
            AuthorizationError::ScopeDenied,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_nodes_and_native_usage_accumulate_but_only_changed_tuples_spend_rows() {
    let ((), report) = run_async_under_lab(0x5ec0_d104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), false).await;
        let authority = issuer();
        let params = GqlParameters::new();
        let mut bounded = grant();
        bounded.limits.max_nodes = 3;
        let token = authority.issue_at(&bounded, 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        assert_eq!(
            bag(session.query(&cx, ROWS, &params).unwrap())
                .values()
                .sum::<i128>(),
            3
        );
        authorization(
            session.query_diff(&cx, ROWS, &params, cuts[0], cuts[0]),
            AuthorizationError::LimitExceeded(LimitDimension::Nodes),
        );
        assert!(!session.is_closed());
        session.query(&cx, ROWS, &params).unwrap();
        bounded.limits.max_nodes = 100_000;
        bounded.limits.max_rows = 0;
        let token = authority.issue_at(&bounded, 100).unwrap();
        let mut zero_rows = db
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                BRANCH,
                symbols,
                GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
                || 100,
            )
            .unwrap();
        assert!(
            zero_rows
                .query_diff(&cx, ROWS, &params, cuts[0], cuts[0])
                .unwrap()
                .changes()
                .is_empty()
        );
        assert!(
            zero_rows
                .query_diff(
                    &cx,
                    "UNWIND [1,2,3] AS x RETURN COUNT(*) AS n",
                    &params,
                    cuts[0],
                    cuts[2]
                )
                .unwrap()
                .changes()
                .is_empty()
        );
        assert!(
            zero_rows
                .query_diff(&cx, ROWS, &params, cuts[0], cuts[2])
                .is_err()
        );
        assert!(!zero_rows.is_closed());
        // Discover the exact native snapshot-record requirement of one endpoint
        // through its real source, then show that two identical endpoints do
        // not each get a fresh allowance. This is internal test observation only.
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut measured = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        let bound = PreparedNativeRead::prepare(ROWS, &params, symbols).unwrap();
        let records = measured
            .run(&cx, |view, _, scope, _, policy, execution| {
                let PreparedNativeRead::Pattern(query) = &bound else {
                    panic!("pattern required");
                };
                let query = query.bind_parameters(&params).unwrap();
                let output = pattern_at(&view.snapshot, cuts[0], &query, scope, execution, policy)
                    .map_err(query_error)?;
                Ok((output.rows.snapshot_records, 0))
            })
            .unwrap();
        assert!(records > 0);
        let mut limited = db
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                BRANCH,
                symbols,
                GqlQueryPolicy::new(records, 1000, 10_000_000, 10_000_000),
                || 100,
            )
            .unwrap();
        assert!(matches!(
            limited.query_diff(&cx, ROWS, &params, cuts[0], cuts[0]),
            Err(GqlQueryError::Rows(_))
        ));
        assert!(!limited.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_live_clock_boundary_including_final_delivery_fences_and_releases_the_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_d105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0_usize);
        let cutoff = Cell::new(usize::MAX);
        let clock = || {
            let next = calls.get() + 1;
            calls.set(next);
            if next >= cutoff.get() { 1000 } else { 100 }
        };
        let params = GqlParameters::new();
        let baseline_refs = Arc::strong_count(&db.snapshot);
        for prepared_mode in [false, true] {
            cutoff.set(usize::MAX);
            let mut session = db
                .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), clock)
                .unwrap();
            let prepared = session.prepare(&cx, ROWS, &params).unwrap();
            calls.set(0);
            if prepared_mode {
                session
                    .execute_diff(&cx, &prepared, &params, cuts[0], cuts[2])
                    .unwrap();
            } else {
                session
                    .query_diff(&cx, ROWS, &params, cuts[0], cuts[2])
                    .unwrap();
            }
            let total = calls.get();
            session.close();
            assert!(total > 5);
            for stop in 1..=total {
                cutoff.set(usize::MAX);
                calls.set(0);
                let mut session = db
                    .authorized_read_session(
                        &cx,
                        &authority,
                        &token,
                        BRANCH,
                        symbols,
                        policy(),
                        clock,
                    )
                    .unwrap();
                let prepared = session.prepare(&cx, ROWS, &params).unwrap();
                calls.set(0);
                cutoff.set(stop);
                let result = if prepared_mode {
                    session.execute_diff(&cx, &prepared, &params, cuts[0], cuts[2])
                } else {
                    session.query_diff(&cx, ROWS, &params, cuts[0], cuts[2])
                };
                authorization(result, AuthorizationError::Expired);
                assert!(session.is_closed(), "checkpoint {stop}");
                assert_eq!(Arc::strong_count(&db.snapshot), baseline_refs);
                let sampled = calls.get();
                authorization(
                    session.execute_diff(&cx, &prepared, &params, cuts[0], cuts[0]),
                    AuthorizationError::ExecutionStopped,
                );
                assert_eq!(calls.get(), sampled);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn callback_retirement_unwind_and_backwards_time_cannot_be_hidden_by_definition_errors() {
    let ((), report) = run_async_under_lab(0x5ec0_d106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), false).await;
        let params = GqlParameters::new();
        let baseline_refs = Arc::strong_count(&db.snapshot);
        for panic in [false, true] {
            let authority = issuer();
            let token = authority.issue_at(&grant(), 100).unwrap();
            let resolver = |_: GraphSymbolKind, _: &str| -> Option<GraphSymbol> {
                if panic {
                    panic!("injected catalog unwind");
                }
                let _ = authority.retire();
                None
            };
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &authority,
                    &token,
                    BRANCH,
                    resolver,
                    policy(),
                    || 100,
                )
                .unwrap();
            if panic {
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _ = session.query_diff(&cx, ROWS, &params, cuts[0], cuts[2]);
                    }))
                    .is_err()
                );
            } else {
                authorization(
                    session.query_diff(&cx, ROWS, &params, cuts[0], cuts[2]),
                    AuthorizationError::AuthorityRetired,
                );
            }
            assert!(session.is_closed());
            assert_eq!(Arc::strong_count(&db.snapshot), baseline_refs);
        }
        let authority = issuer();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let now = Cell::new(100_u64);
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || {
                now.get()
            })
            .unwrap();
        now.set(99);
        authorization(
            session.query_diff(&cx, "RETURN 1 AS one", &params, cuts[0], cuts[0]),
            AuthorizationError::ClockWentBackwards,
        );
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_endpoint_errors_and_signed_delivery_refusal_never_release_partial_differences() {
    let ((), report) = run_async_under_lab(0x5ec0_d107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = issuer();
        let params = GqlParameters::new();
        let token = authority.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        for suffix in ["", " LIMIT 0"] {
            let text = format!("MATCH (n) RETURN 100 / (n.p - 2) AS value{suffix}");
            assert!(matches!(
                session.query_diff(&cx, &text, &params, cuts[0], cuts[2]),
                Err(GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint: DiffEndpoint::After,
                    ..
                }))
            ));
            assert!(matches!(
                session.query_diff(&cx, &text, &params, cuts[2], cuts[2]),
                Err(GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint: DiffEndpoint::Before,
                    ..
                }))
            ));
            assert!(!session.is_closed());
            session.query(&cx, ROWS, &params).unwrap();
        }
        let mut bounded = grant();
        bounded.limits.max_rows = 1;
        let token = authority.issue_at(&bounded, 100).unwrap();
        let mut limited = db
            .authorized_read_session(&cx, &authority, &token, BRANCH, symbols, policy(), || 100)
            .unwrap();
        authorization(
            limited.query_diff(&cx, ROWS, &params, cuts[0], cuts[2]),
            AuthorizationError::LimitExceeded(LimitDimension::Rows),
        );
        assert!(!limited.is_closed());
        assert!(
            limited
                .query_diff(&cx, ROWS, &params, cuts[0], cuts[0])
                .unwrap()
                .changes()
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
