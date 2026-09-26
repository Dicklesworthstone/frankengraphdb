use super::*;
use crate::{Database, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GraphSymbol, GraphSymbolKind,
};
use fgdb_prism::{
    Directedness, FnxArgument, FnxExecutionError, FnxExecutionLimits, FnxSelection,
    FnxSourceLimits, FnxWeightError, FnxWeightSpec, MissingWeightPolicy, ParallelEdgePolicy,
    ProjectionLimits, ProjectionSpec, SelfLoopPolicy,
};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use fgdb_warden::{Authority, Grant, LimitDimension, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x59; 32]);
const NOW: u64 = 100;
const EXPIRES: u64 = 10_000;
const BFS: &str =
    "CALL fnx.single_source_shortest_path_length($source) YIELD distance AS hops,vertex AS id";
const CC: &str = "CALL fnx.connected_components() YIELD vertex,component";
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(99);
const W: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(99);

fn issuer() -> Authority {
    Authority::new(AuthKey::from_seed(0x59), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        EXPIRES,
        QueryLimits {
            max_nodes: 100,
            max_work: 1_000_000,
            max_rows: 100,
        },
    );
    grant.labels = Scope::only([L]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([W]);
    grant
}
fn host() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1000)
}
fn catalog(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    panic!("registered Prism calls must not consult the native GQL catalog")
}
fn params() -> FnxParameters {
    FnxParameters::from([("source".to_owned(), FnxArgument::Vertex(VId(1)))])
}
fn options(direction: Directedness) -> FnxReadOptions {
    FnxReadOptions {
        as_of: None,
        selection: FnxSelection {
            vertex_label: None,
            relation: None,
            weight: FnxWeightSpec::Property {
                key: W,
                missing: MissingWeightPolicy::Reject,
            },
        },
        projection: ProjectionSpec {
            directedness: direction,
            parallel_edges: ParallelEdgePolicy::Minimum,
            self_loops: SelfLoopPolicy::Keep,
        },
        source_limits: FnxSourceLimits {
            max_work_units: 100_000,
            max_scratch_entries: 10_000,
            max_staging_bytes: 1 << 20,
        },
        projection_limits: ProjectionLimits {
            max_vertices: 100,
            max_input_edges: 100,
            max_adjacency_entries: 200,
            max_workspace_bytes: 1 << 20,
        },
        execution_limits: FnxExecutionLimits {
            max_iterations: 1000,
            max_result_rows: 100,
            max_estimated_work: 1 << 26,
        },
    }
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x5a; 32], NS, [0x5b; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [1, 2, 3, u128::MAX] {
        batch.create_vertex(VId(id), if hidden { vec![L, H] } else { vec![L] }, vec![]);
    }
    for (eid, src, dst, weight) in [
        (1, 1, 2, 2),
        (2, 2, 3, 3),
        (3, 3, 1, 4),
        (4, 1, 2, 5),
        (5, 2, 2, 1),
    ] {
        let mut props = vec![(W, CanonicalScalar::Int(weight))];
        if hidden {
            props.push((SECRET, CanonicalScalar::Bool(true)));
        }
        batch.add_edge(EId(eid), VId(src), VId(dst), props);
    }
    if hidden {
        for id in 100..120 {
            batch.create_vertex(VId(id), vec![H], vec![]);
            batch.add_edge(
                EId(id),
                VId(1),
                VId(id),
                vec![(W, CanonicalScalar::Bool(true))],
            );
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut batch = WriteBatch::new(RelationId(99));
        batch.add_edge(
            EId(200),
            VId(1),
            VId(3),
            vec![(W, CanonicalScalar::Bool(true))],
        );
        db.write(cx, batch).await.unwrap();
    }
    db
}
fn authorization<T: core::fmt::Debug>(result: Result<T, Error>, expected: WardenError) {
    match result {
        Err(Error::Cancelled(QueryError::Authorization(actual))) => {
            assert_eq!(actual, expected, "authorization refusal differs");
        }
        other => panic!("expected authorization refusal {expected:?}, got {other:?}"),
    }
}

#[test]
fn every_registered_kernel_uses_the_same_scoped_projection_and_explicit_laws() {
    let ((), report) = run_async_under_lab(0xa11a_1001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = full
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let calls = [
            (
                "CALL fnx.pagerank(0.85,1000,1e-12,true) YIELD score AS rank,vertex",
                Directedness::Directed,
            ),
            (BFS, Directedness::Reversed),
            (
                "CALL fnx.single_source_dijkstra_path_length(1,NULL,true) YIELD vertex,distance",
                Directedness::Directed,
            ),
            (CC, Directedness::Undirected),
            (
                "CALL fnx.weakly_connected_components() YIELD component,vertex",
                Directedness::Directed,
            ),
            (
                "CALL fnx.strongly_connected_components() YIELD vertex,component",
                Directedness::Directed,
            ),
            (
                "CALL fnx.triangles() YIELD vertex,triangles",
                Directedness::Undirected,
            ),
            (
                "CALL fnx.clustering_coefficient() YIELD score,vertex",
                Directedness::Undirected,
            ),
        ];
        for (text, direction) in calls {
            let opt = options(direction);
            let expected = clean
                .call_fnx(&c.query(), text, &params(), opt)
                .unwrap()
                .analytics
                .rows;
            assert_eq!(
                session.call_fnx(&c.query(), text, &params(), opt).unwrap(),
                expected
            );
            let prepared = session
                .prepare_fnx(&c.query(), text, &params(), opt)
                .unwrap();
            assert_eq!(
                session.execute_fnx(&c.query(), &prepared).unwrap(),
                expected
            );
            assert_eq!(
                full.call_fnx_authorized(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    text,
                    &params(),
                    opt,
                    || NOW
                )
                .unwrap(),
                expected
            );
        }
        for law in [
            ParallelEdgePolicy::Reject,
            ParallelEdgePolicy::CollapseUnit,
            ParallelEdgePolicy::Minimum,
            ParallelEdgePolicy::Maximum,
            ParallelEdgePolicy::Sum,
        ] {
            let mut opt = options(Directedness::Directed);
            opt.projection.parallel_edges = law;
            let expected = clean.call_fnx(&c.query(), BFS, &params(), opt);
            let actual = session.call_fnx(&c.query(), BFS, &params(), opt);
            match (expected, actual) {
                (Ok(expected), Ok(actual)) => assert_eq!(actual, expected.analytics.rows),
                (Err(FnxReadError::Projection(expected)), Err(Error::Projection(actual))) => {
                    assert_eq!(actual, expected)
                }
                other => panic!("projection law drift: {other:?}"),
            }
            assert!(!session.is_closed());
        }
        for missing in [
            MissingWeightPolicy::Reject,
            MissingWeightPolicy::Unit,
            MissingWeightPolicy::Zero,
        ] {
            let mut opt = options(Directedness::Directed);
            opt.selection.weight = FnxWeightSpec::Property {
                key: SECRET,
                missing,
            };
            let actual = session.call_fnx(&c.query(), BFS, &params(), opt);
            match missing {
                MissingWeightPolicy::Reject => assert!(matches!(
                    actual,
                    Err(Error::Weight {
                        reason: FnxWeightError::Missing,
                        ..
                    })
                )),
                _ => assert_eq!(
                    actual.unwrap(),
                    clean
                        .call_fnx(&c.query(), BFS, &params(), opt)
                        .unwrap()
                        .analytics
                        .rows
                ),
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_calls_freeze_arguments_and_history_without_borrowing_the_writer() {
    let ((), report) = run_async_under_lab(0xa11a_1002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let sequence = db.frontier().unwrap();
        let expected = db
            .call_fnx(&c.query(), BFS, &params(), options(Directedness::Directed))
            .unwrap()
            .analytics
            .rows;
        let mut arguments = params();
        let mut opt = options(Directedness::Directed);
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_fnx(&c.query(), BFS, &arguments, opt)
            .unwrap();
        let copy = prepared.clone();
        assert_eq!(prepared.source_sequence(), sequence);
        assert_eq!(
            prepared
                .outputs()
                .iter()
                .map(|column| column.name.as_str())
                .collect::<Vec<_>>(),
            ["hops", "id"]
        );
        assert_eq!(
            format!("{prepared:?}"),
            "AuthorizedPreparedFnxCall([REDACTED])"
        );
        arguments.insert("source".to_owned(), FnxArgument::Vertex(VId(u128::MAX)));
        opt.projection.directedness = Directedness::Reversed;
        opt.execution_limits.max_result_rows = 0;
        assert!(session.call_fnx(&c.query(), BFS, &arguments, opt).is_err());
        assert!(!session.is_closed());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(3));
        db.write(&c.commit(), change).await.unwrap();
        assert_ne!(
            db.call_fnx(&c.query(), BFS, &params(), options(Directedness::Directed))
                .unwrap()
                .analytics
                .rows,
            expected
        );
        let mut latest = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut historical = options(Directedness::Directed);
        historical.as_of = Some(sequence);
        assert_eq!(
            latest
                .call_fnx(&c.query(), BFS, &params(), historical)
                .unwrap(),
            expected
        );
        let historical_call = latest
            .prepare_fnx(&c.query(), BFS, &params(), historical)
            .unwrap();
        assert_eq!(
            latest.execute_fnx(&c.query(), &historical_call).unwrap(),
            expected
        );
        historical.as_of = Some(db.frontier().unwrap());
        assert!(matches!(
            session.call_fnx(&c.query(), BFS, &params(), historical),
            Err(Error::Read(_))
        ));
        assert!(!session.is_closed());
        drop(db);
        assert_eq!(session.execute_fnx(&c.query(), &copy).unwrap(), expected);
        assert_eq!(
            session
                .call_fnx(&c.query(), BFS, &params(), options(Directedness::Directed))
                .unwrap(),
            expected
        );
        session.close();
        authorization(
            session.execute_fnx(&c.query(), &prepared),
            WardenError::ExecutionStopped,
        );
        assert!(session.state.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_prepared_calls_refuse_even_with_equal_authority_and_empty_projection() {
    let ((), report) = run_async_under_lab(0xa11a_1003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut first = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut second = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut opt = options(Directedness::Undirected);
        opt.selection.vertex_label = Some(H);
        let prepared = first.prepare_fnx(&c.query(), CC, &params(), opt).unwrap();
        assert!(first.execute_fnx(&c.query(), &prepared).unwrap().is_empty());
        authorization(
            second.execute_fnx(&c.query(), &prepared),
            WardenError::WrongAuthority,
        );
        assert!(!second.is_closed());
        assert!(
            second
                .call_fnx(&c.query(), CC, &params(), opt)
                .unwrap()
                .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_and_host_limits_bound_one_execution_and_leave_ordinary_refusals_retryable() {
    let ((), report) = run_async_under_lab(0xa11a_1004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let opt = options(Directedness::Undirected);
        for dimension in [
            LimitDimension::Nodes,
            LimitDimension::Rows,
            LimitDimension::Work,
        ] {
            let mut grant = grant();
            match dimension {
                LimitDimension::Nodes => grant.limits.max_nodes = 3,
                LimitDimension::Rows => grant.limits.max_rows = 3,
                LimitDimension::Work => grant.limits.max_work = 10,
            }
            let token = issuer.issue_at(&grant, NOW).unwrap();
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || NOW,
                )
                .unwrap();
            let prepared = session.prepare_fnx(&c.query(), CC, &params(), opt).unwrap();
            authorization(
                session.execute_fnx(&c.query(), &prepared),
                WardenError::LimitExceeded(dimension),
            );
            assert!(!session.is_closed());
        }
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for (records, rows, work, scratch, expected) in [
            (8, 100, 1_000_000, 1000, "records"),
            (100, 3, 1_000_000, 1000, "rows"),
            (100, 100, 5, 1000, "work"),
            (100, 100, 1_000_000, 8, "scratch"),
        ] {
            let policy = GqlQueryPolicy::new(records, rows, work, scratch);
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    policy,
                    || NOW,
                )
                .unwrap();
            let result = session.call_fnx(&c.query(), CC, &params(), opt);
            let matches = match &result {
                Err(Error::Cancelled(QueryError::Pattern(GqlQueryError::Rows(error)))) => {
                    expected == "records" && error.dimension == GqlBudgetDimension::SnapshotRecords
                }
                Err(Error::Cancelled(QueryError::Pattern(GqlQueryError::Evaluator(error)))) => {
                    match error.dimension {
                        GlaLimitDimension::WorkUnits => expected == "work",
                        GlaLimitDimension::ScratchEntries => expected == "scratch",
                    }
                }
                Err(Error::Execution(FnxExecutionError::LimitExceeded {
                    resource: "result rows",
                    ..
                })) => expected == "rows",
                _ => false,
            };
            assert!(matches, "{expected}: {result:?}");
            assert!(!session.is_closed());
        }
        // Request limits can narrow a generous host. They must not become
        // dead fields merely because the raw historical scan is poll-only.
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let mut narrow = opt;
        narrow.source_limits.max_scratch_entries = 0;
        assert!(
            matches!(session.call_fnx(&c.query(), CC, &params(), narrow),
            Err(Error::Cancelled(QueryError::Pattern(GqlQueryError::Evaluator(error)))) if error.dimension == GlaLimitDimension::ScratchEntries && error.limit == 0)
        );
        assert!(!session.is_closed());
        let prepared = session
            .prepare_fnx(&c.query(), CC, &params(), narrow)
            .unwrap();
        assert!(matches!(session.execute_fnx(&c.query(), &prepared),
            Err(Error::Cancelled(QueryError::Pattern(GqlQueryError::Evaluator(error)))) if error.dimension == GlaLimitDimension::ScratchEntries && error.limit == 0));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn whole_pipeline_work_threshold_is_independent_of_hidden_history() {
    let ((), report) = run_async_under_lab(0xa11a_1005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let full = database(&c.commit(), true).await;
        let clean = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut thresholds = Vec::new();
        for db in [&full, &clean] {
            let mut low = 0;
            let mut high = host().evaluator.max_work_units;
            while low < high {
                let middle = low + (high - low) / 2;
                let mut policy = host();
                policy.evaluator.max_work_units = middle;
                let mut session = db
                    .authorized_read_session(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        catalog,
                        policy,
                        || NOW,
                    )
                    .unwrap();
                if session
                    .call_fnx(&c.query(), BFS, &params(), options(Directedness::Directed))
                    .is_ok()
                {
                    high = middle;
                } else {
                    low = middle + 1;
                }
            }
            assert!(
                low > 30,
                "the entire source/build/kernel must spend one work allowance"
            );
            let mut policy = host();
            policy.evaluator.max_work_units = low - 1;
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    policy,
                    || NOW,
                )
                .unwrap();
            assert!(
                matches!(session.call_fnx(&c.query(), BFS, &params(), options(Directedness::Directed)),
                Err(Error::Cancelled(QueryError::Pattern(GqlQueryError::Evaluator(error)))) if error.dimension == GlaLimitDimension::WorkUnits)
            );
            thresholds.push(low);
        }
        assert_eq!(thresholds[0], thresholds[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_clock_boundary_fences_one_shot_and_prepared_output_and_releases_the_pin() {
    let ((), report) = run_async_under_lab(0xa11a_1006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        for prepared_path in [false, true] {
            let issuer = issuer();
            let token = issuer.issue_at(&grant(), NOW).unwrap();
            let seen = Cell::new(0);
            let stop = Cell::new(u64::MAX);
            let clock = || {
                let next = seen.get() + 1;
                seen.set(next);
                if next >= stop.get() { EXPIRES } else { NOW }
            };
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    clock,
                )
                .unwrap();
            let prepared = session
                .prepare_fnx(&c.query(), CC, &params(), options(Directedness::Undirected))
                .unwrap();
            seen.set(0);
            if prepared_path {
                session.execute_fnx(&c.query(), &prepared).unwrap();
            } else {
                session
                    .call_fnx(&c.query(), CC, &params(), options(Directedness::Undirected))
                    .unwrap();
            }
            let boundaries = seen.get();
            assert!(boundaries > 20);
            for boundary in 1..=boundaries {
                stop.set(u64::MAX);
                seen.set(0);
                let mut session = db
                    .authorized_read_session(
                        &c.query(),
                        &issuer,
                        &token,
                        "main",
                        catalog,
                        host(),
                        clock,
                    )
                    .unwrap();
                let prepared = session
                    .prepare_fnx(&c.query(), CC, &params(), options(Directedness::Undirected))
                    .unwrap();
                seen.set(0);
                stop.set(boundary);
                let result = if prepared_path {
                    session.execute_fnx(&c.query(), &prepared)
                } else {
                    session.call_fnx(&c.query(), CC, &params(), options(Directedness::Undirected))
                };
                authorization(result, WardenError::Expired);
                assert!(session.is_closed(), "expiry at {boundary}");
                assert!(session.state.is_none(), "generation retained at {boundary}");
            }
            let retiring = self::issuer();
            let token = retiring.issue_at(&grant(), NOW).unwrap();
            let seen = Cell::new(0);
            let stop = Cell::new(u64::MAX);
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &retiring,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || {
                        seen.set(seen.get() + 1);
                        if seen.get() == stop.get() {
                            retiring.retire();
                        }
                        NOW
                    },
                )
                .unwrap();
            let prepared = session
                .prepare_fnx(&c.query(), CC, &params(), options(Directedness::Undirected))
                .unwrap();
            seen.set(0);
            stop.set(boundaries);
            let result = if prepared_path {
                session.execute_fnx(&c.query(), &prepared)
            } else {
                session.call_fnx(&c.query(), CC, &params(), options(Directedness::Undirected))
            };
            authorization(result, WardenError::AuthorityRetired);
            assert!(session.state.is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_live_check_overrides_native_errors_and_backwards_time_closes_session() {
    let ((), report) = run_async_under_lab(0xa11a_1007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        for prepare in [false, true] {
            let calls = Cell::new(0);
            let armed = Cell::new(false);
            let mut session = db
                .authorized_read_session(
                    &c.query(),
                    &issuer,
                    &token,
                    "main",
                    catalog,
                    host(),
                    || {
                        calls.set(calls.get() + 1);
                        if armed.get() && calls.get() >= 3 {
                            EXPIRES
                        } else {
                            NOW
                        }
                    },
                )
                .unwrap();
            assert!(matches!(
                session.call_fnx(
                    &c.query(),
                    "invalid",
                    &params(),
                    options(Directedness::Directed)
                ),
                Err(Error::Bind(_))
            ));
            assert!(!session.is_closed());
            calls.set(0);
            armed.set(true);
            if prepare {
                authorization(
                    session.prepare_fnx(
                        &c.query(),
                        "invalid",
                        &params(),
                        options(Directedness::Directed),
                    ),
                    WardenError::Expired,
                );
            } else {
                authorization(
                    session.call_fnx(
                        &c.query(),
                        "invalid",
                        &params(),
                        options(Directedness::Directed),
                    ),
                    WardenError::Expired,
                );
            }
            assert!(session.state.is_none());
        }
        let now = Cell::new(NOW);
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || {
                now.get()
            })
            .unwrap();
        let prepared = session
            .prepare_fnx(&c.query(), BFS, &params(), options(Directedness::Directed))
            .unwrap();
        now.set(NOW - 1);
        authorization(
            session.execute_fnx(&c.query(), &prepared),
            WardenError::ClockWentBackwards,
        );
        assert!(session.state.is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_at_every_checkpoint_discards_output_and_keeps_session_retryable() {
    let ((), report) = run_async_under_lab(0xa11a_1008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), true).await;
        let issuer = issuer();
        let token = issuer.issue_at(&grant(), NOW).unwrap();
        let mut session = db
            .authorized_read_session(&c.query(), &issuer, &token, "main", catalog, host(), || NOW)
            .unwrap();
        let prepared = session
            .prepare_fnx(&c.query(), BFS, &params(), options(Directedness::Reversed))
            .unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = c.query().with_checkpoint_probe(Arc::clone(&probe));
        let expected = session.execute_fnx(&observed, &prepared).unwrap();
        let calls = probe.calls();
        assert!(calls > 20);
        for at in 1..=calls {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(at)));
            let controlled = c.query().with_checkpoint_probe(probe);
            assert!(
                matches!(
                    session.execute_fnx(&controlled, &prepared),
                    Err(Error::Cancelled(_))
                ),
                "checkpoint {at}"
            );
            assert!(!session.is_closed());
        }
        assert_eq!(
            session.execute_fnx(&c.query(), &prepared).unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
