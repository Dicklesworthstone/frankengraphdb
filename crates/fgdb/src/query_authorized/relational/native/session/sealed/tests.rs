//! Real sealed owners exercise the existing session's source and lifetime laws.
use super::*;
use crate::{DatabaseKeys, MemVfs, NativeReadClass, QueryValue, ReadCacheLimits, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_prism::{
    Directedness, FnxCallSpec, FnxParameters, FnxReadError, FnxReadOptions, SealedGraphView,
    SealedProjectionMask, SealedProjectionSpec, SnapshotBinding,
};
use fgdb_strata::store::RootReadLimits;
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Scope};
use std::cell::Cell;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x69; 32]);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(99);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(99);
const NOW: u64 = 100;
const EXPIRES: u64 = 1000;
const BFS: &str = "CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R') \
     YIELD vertex, distance RETURN vertex, distance ORDER BY vertex";

fn issuer(seed: u64) -> Authority {
    Authority::new(AuthKey::from_seed(seed), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant(restricted: bool) -> Grant {
    let mut grant = Grant::read_only(
        "main",
        EXPIRES,
        QueryLimits {
            max_nodes: 100,
            max_work: 1_000_000,
            max_rows: 100,
        },
    );
    // Use the same explicit predicates as the established open-scope fixtures;
    // read_only alone is not a proof of unrestricted incidence and fields.
    grant.labels = if restricted {
        Scope::only([L])
    } else {
        Scope::All
    };
    grant.relations = if restricted {
        Scope::only([R])
    } else {
        Scope::All
    };
    grant.properties = if restricted {
        Scope::only([P])
    } else {
        Scope::All
    };
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 1_000_000)
}
fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1 << 20,
        max_result_bytes: 1 << 20,
    }
}
fn catalog(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "Other") => Some(GraphSymbol::Relation(RelationId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
async fn database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x68; 32], NS, [0x6a; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(
            VId(id),
            if hidden { vec![L, H] } else { vec![L] },
            vec![(P, CanonicalScalar::Int(id as i64 * 10))],
        );
    }
    for (id, src, dst) in [(1, 1, 2), (2, 2, 3)] {
        let mut props = vec![(P, CanonicalScalar::Int(id as i64 + 1))];
        if hidden {
            props.push((SECRET, CanonicalScalar::Bool(true)));
        }
        batch.add_edge(EId(id), VId(src), VId(dst), props);
    }
    if hidden {
        // A hidden bridge changes reachability, components and raw image size.
        // Its malformed weight must not be resolved by a restricted call.
        for id in 100..120 {
            batch.create_vertex(VId(id), vec![H], vec![]);
            batch.add_edge(
                EId(id),
                VId(1),
                VId(id),
                vec![(P, CanonicalScalar::Bool(true))],
            );
            batch.add_edge(EId(id + 100), VId(id), VId(3), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    db
}
async fn retained(db: &Database<MemVfs>, cx: &QueryCx, cut: Option<CommitSeq>) -> SealedReadView {
    db.prepare_sealed_read(
        cx,
        cut,
        R,
        SealedLimits::default(),
        RootReadLimits::default(),
    )
    .await
    .unwrap()
}
fn compressed_calls() -> usize {
    crate::query::authorized::analytics::sealed::EXECUTIONS.with(Cell::get)
}
fn compressed_builds() -> usize {
    crate::query::authorized::analytics::sealed::BUILDS.with(Cell::get)
}
fn retention(owner: &SealedReadView, cx: &QueryCx, bytes: usize) -> crate::ReadCacheStatus {
    owner
        .authorized_parts()
        .0
        .set_read_cache_limits(
            cx,
            ReadCacheLimits {
                authorized_projection_bytes: bytes,
                ..ReadCacheLimits::default()
            },
        )
        .unwrap()
}
fn refusal<T>(result: Result<T, QueryError>, expected: AuthorizationError) {
    assert!(matches!(result, Err(QueryError::Authorization(actual)) if actual == expected));
}
fn outcome(result: Result<QueryResult, QueryError>) -> Result<QueryResult, String> {
    result.map_err(|error| format!("{error:?}"))
}

#[test]
fn every_native_class_keeps_one_historical_cut_and_exact_prepared_owner() {
    let ((), report) = run_async_under_lab(0xa115_ea01, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit, false).await;
        let mut update = WriteBatch::new(R);
        update.create_vertex(VId(4), vec![L], vec![(P, CanonicalScalar::Int(40))]);
        update.add_edge(EId(3), VId(3), VId(4), vec![(P, CanonicalScalar::Int(1))]);
        db.write(&commit, update).await.unwrap();
        let owner = retained(&db, &cx, Some(CommitSeq(1))).await;
        assert_eq!(owner.frontier(), CommitSeq(2));
        let authority = issuer(1);
        let token = authority.issue_at(&grant(false), NOW).unwrap();
        let mut session = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                || NOW,
            )
            .unwrap();
        let mut other = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                || NOW,
            )
            .unwrap();
        let at = GqlParameters::new().with_uint64("at", 1).unwrap();
        let empty = GqlParameters::new();
        let cases = [
            (NativeReadClass::Pattern, "MATCH (n:L) RETURN n.p"),
            (
                NativeReadClass::Aggregate,
                "MATCH (n:L) RETURN COUNT(*) AS n",
            ),
            (
                NativeReadClass::PipelineAggregate,
                "MATCH (n:L) WITH n.p AS p RETURN COUNT(*) AS n",
            ),
            (
                NativeReadClass::Set,
                "MATCH (n:L) RETURN n AS v UNION ALL MATCH (m:L) RETURN m AS v",
            ),
            (
                NativeReadClass::TemporalPattern,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p",
            ),
            (
                NativeReadClass::TemporalAggregate,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN COUNT(*) AS n",
            ),
            (
                NativeReadClass::TemporalSet,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at \
                 RETURN n AS v UNION ALL MATCH (m:L) RETURN m AS v",
            ),
        ];
        let mut prepared_results = Vec::new();
        for (class, text) in cases {
            let args = if text.contains("FOR SYSTEM_TIME") {
                &at
            } else {
                &empty
            };
            let oracle = if text.contains("FOR SYSTEM_TIME") {
                text.to_owned()
            } else {
                text.replacen(
                    "MATCH (n:L)",
                    "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at",
                    1,
                )
            };
            let expected = db.query(&cx, &oracle, &at, catalog, policy()).unwrap();
            let prepared = session.prepare(&cx, text, args).unwrap();
            assert_eq!(prepared.facade_class(), class);
            assert_eq!(
                session.query_with_snapshot(&cx, text, args).unwrap(),
                (expected.clone(), CommitSeq(1)),
            );
            assert_eq!(session.execute(&cx, &prepared, args).unwrap(), expected);
            refusal(
                other.execute(&cx, &prepared, args),
                AuthorizationError::WrongAuthority,
            );
            prepared_results.push((prepared, args.clone(), expected));
        }
        // The image was sealed from publication 2. Every source of this
        // composed call must still select cut 1, excluding its later edge.
        let temporal_call = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at WITH n \
            CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R') \
            YIELD vertex, distance WHERE vertex = n RETURN vertex, distance ORDER BY vertex";
        let expected = db
            .query(&cx, temporal_call, &at, catalog, policy())
            .unwrap();
        let before = compressed_calls();
        assert_eq!(
            session.query_with_snapshot(&cx, BFS, &empty).unwrap(),
            (expected.clone(), CommitSeq(1)),
        );
        assert!(compressed_calls() > before);
        let prepared = session.prepare(&cx, BFS, &empty).unwrap();
        prepared_results.push((prepared, empty.clone(), expected));
        let shifted = GqlParameters::new().with_uint64("at", 2).unwrap();
        let before = compressed_calls();
        assert!(matches!(
            session.query(
                &cx,
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $at RETURN n.p",
                &shifted,
            ),
            Err(QueryError::Unsupported { .. })
        ));
        assert_eq!(compressed_calls(), before);
        assert!(matches!(
            session.query(&cx, temporal_call, &shifted),
            Err(QueryError::Unsupported { .. })
        ));
        assert_eq!(compressed_calls(), before);
        assert!(!session.is_closed());
        drop(db);
        drop(owner);
        for (prepared, args, expected) in prepared_results {
            assert_eq!(session.execute(&cx, &prepared, &args).unwrap(), expected);
        }
        session.close();
        refusal(
            session.query(&cx, BFS, &empty),
            AuthorizationError::ExecutionStopped,
        );
        assert!(format!("{session:?}").contains("[REDACTED]"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn composed_calls_use_compressed_rows_and_incompatible_sources_keep_the_masked_path() {
    let ((), report) = run_async_under_lab(0xa115_ea02, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), false).await;
        let owner = retained(&db, &cx, None).await;
        let authority = issuer(2);
        let token = authority.issue_at(&grant(false), NOW).unwrap();
        let mut session = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                || NOW,
            )
            .unwrap();
        let args = GqlParameters::new();
        for text in [
            BFS,
            "CALL fnx.weakly_connected_components(relation => 'R') YIELD vertex, component \
             RETURN count(DISTINCT component) AS components",
            "MATCH (n:L) WITH n \
             CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R') \
             YIELD vertex, distance WHERE vertex = n RETURN n.p AS p, distance ORDER BY p",
            "CALL fnx.single_source_dijkstra_path_length(args => [1], relation => 'R', \
             weight => 'p', missing => 'reject', parallel => 'min') \
             YIELD distance, vertex RETURN distance, vertex ORDER BY vertex",
        ] {
            let expected = db.query(&cx, text, &args, catalog, policy()).unwrap();
            let before = compressed_calls();
            assert_eq!(session.query(&cx, text, &args).unwrap(), expected, "{text}");
            assert!(compressed_calls() > before, "{text}");
            let prepared = session.prepare(&cx, text, &args).unwrap();
            assert_eq!(session.execute(&cx, &prepared, &args).unwrap(), expected);
        }
        for text in [
            "CALL fnx.single_source_shortest_path_length(args => [1]) \
             YIELD vertex, distance RETURN vertex, distance ORDER BY vertex",
            "CALL fnx.single_source_shortest_path_length(args => [1], relation => 'Other') \
             YIELD vertex, distance RETURN vertex, distance ORDER BY vertex",
        ] {
            let expected = db.query(&cx, text, &args, catalog, policy()).unwrap();
            let before = compressed_calls();
            assert_eq!(session.query(&cx, text, &args).unwrap(), expected);
            assert_eq!(
                compressed_calls(),
                before,
                "incompatible relation must not inspect the image"
            );
        }
        let mut narrow = grant(false);
        narrow.properties = Scope::only([P]);
        let token = authority.issue_at(&narrow, NOW).unwrap();
        let mut restricted = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                || NOW,
            )
            .unwrap();
        let before = compressed_calls();
        assert_eq!(
            restricted.query(&cx, BFS, &args).unwrap(),
            db.query(&cx, BFS, &args, catalog, policy()).unwrap(),
        );
        assert_eq!(
            compressed_calls(),
            before,
            "field masking alone rules out raw image admission"
        );
        assert_eq!(
            session
                .query(&cx, "MATCH (n:L) RETURN count(*) AS n", &args)
                .unwrap(),
            QueryResult::Rows {
                columns: vec!["n".to_owned()],
                rows: vec![vec![QueryValue::Count(3)]],
            },
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// The public native response exposes no cost metadata. Inspect its existing
// source observer privately to prove both admitted statistics and rows agree.
fn measured<Rs: GraphSymbolResolver, C: FnMut() -> u64>(
    session: &mut AuthorizedSealedReadSession<'_, Rs, C>,
    cx: &QueryCx,
) -> Result<(Vec<Vec<fgdb_prism::FnxValue>>, u64, u64, u64), QueryError> {
    measured_options(session, cx, FnxReadOptions::whole_graph(None))
}

fn measured_options<Rs: GraphSymbolResolver, C: FnMut() -> u64>(
    session: &mut AuthorizedSealedReadSession<'_, Rs, C>,
    cx: &QueryCx,
    mut options: FnxReadOptions,
) -> Result<(Vec<Vec<fgdb_prism::FnxValue>>, u64, u64, u64), QueryError> {
    session
        .inner
        .run(cx, |view, _, scope, _, policy, execution| {
            let call = FnxCallSpec::bind(
                "CALL fnx.single_source_shortest_path_length(1) YIELD vertex,distance",
                &FnxParameters::new(),
            )
            .unwrap();
            let at = selected_cut(view, execution);
            options.as_of = Some(at);
            options.selection.relation = Some(R);
            let mut usage = crate::gql_exec::AdmissionUsage::default();
            let rows = crate::query::authorized::analytics::execute(
                &view.snapshot,
                SnapshotBinding {
                    root: view.partition_root().0,
                    as_of: at,
                },
                &call,
                options,
                scope,
                execution,
                crate::query::authorized::analytics::UsageObserver {
                    usage: &mut usage,
                    policy,
                },
            )
            .map_err(|error| match error {
                FnxReadError::Cancelled(error) => error,
                FnxReadError::Read(error) => QueryError::Read(error),
                other => QueryError::Unsupported {
                    diagnostics: vec![other.to_string()],
                },
            })?;
            let remaining = usage.remaining(policy);
            let stats = (
                usage.snapshot_records(),
                policy.evaluator.max_work_units - remaining.evaluator.max_work_units,
                policy.evaluator.max_scratch_entries - remaining.evaluator.max_scratch_entries,
            );
            let count = rows.len();
            Ok(((rows, stats.0, stats.1, stats.2), count))
        })
}

#[test]
fn restricted_compressed_owners_match_physically_removed_data_in_rows_statistics_and_refusals() {
    let _turn = crate::query::authorized::analytics::reuse_test_turn();
    let ((), report) = run_async_under_lab(0xa115_ea03, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let clean = database(&c.commit(), false).await;
        let hidden = database(&c.commit(), true).await;
        let owners = [
            retained(&clean, &cx, None).await,
            retained(&hidden, &cx, None).await,
        ];
        let authority = issuer(3);
        let token = authority.issue_at(&grant(true), NOW).unwrap();
        let args = GqlParameters::new();
        let before = compressed_calls();
        let mut stats = Vec::new();
        for owner in &owners {
            let mut session = owner
                .authorized_read_session(
                    &cx,
                    &authority,
                    &token,
                    "main",
                    catalog,
                    policy(),
                    memory(),
                    || NOW,
                )
                .unwrap();
            stats.push(measured(&mut session, &cx).unwrap());
        }
        assert_eq!(stats[0], stats[1]);
        let weighted = "CALL fnx.single_source_dijkstra_path_length(args => [1], relation => 'R', \
             weight => 'p', missing => 'reject', parallel => 'min') \
             YIELD vertex, distance RETURN vertex, distance ORDER BY vertex";
        for host_work in [0, 10, 50, 100, 500, 1_000_000] {
            let mut host = policy();
            host.evaluator.max_work_units = host_work;
            let mut results = Vec::new();
            for owner in &owners {
                let mut session = owner
                    .authorized_read_session(
                        &cx,
                        &authority,
                        &token,
                        "main",
                        catalog,
                        host,
                        memory(),
                        || NOW,
                    )
                    .unwrap();
                results.push(outcome(session.query(&cx, weighted, &args)));
            }
            assert_eq!(results[0], results[1], "host work {host_work}");
        }
        for (nodes, rows, work) in [
            (0, 100, 1_000_000),
            (2, 100, 1_000_000),
            (100, 0, 1_000_000),
            (100, 2, 1_000_000),
            (100, 100, 10),
            (100, 100, 50),
        ] {
            let mut limited = grant(true);
            limited.limits = QueryLimits {
                max_nodes: nodes,
                max_rows: rows,
                max_work: work,
            };
            let token = authority.issue_at(&limited, NOW).unwrap();
            let mut results = Vec::new();
            for owner in &owners {
                let result = owner.authorized_read_session(
                    &cx,
                    &authority,
                    &token,
                    "main",
                    catalog,
                    policy(),
                    memory(),
                    || NOW,
                );
                results.push(outcome(
                    result.and_then(|mut session| session.query(&cx, BFS, &args)),
                ));
            }
            assert_eq!(
                results[0], results[1],
                "signed limits {nodes}/{rows}/{work}"
            );
        }
        assert_eq!(
            compressed_calls(),
            before,
            "restricted sources never touch the raw image"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_expiry_boundary_closes_the_owner_and_retirement_is_live_inside_compressed_execution() {
    let ((), report) = run_async_under_lab(0xa115_ea04, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), false).await;
        let owner = retained(&db, &cx, None).await;
        let authority = issuer(4);
        let token = authority.issue_at(&grant(false), NOW).unwrap();
        let args = GqlParameters::new();
        let calls = Cell::new(0usize);
        let cutoff = Cell::new(usize::MAX);
        let clock = || {
            let next = calls.get() + 1;
            calls.set(next);
            if next >= cutoff.get() { EXPIRES } else { NOW }
        };
        let mut session = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                clock,
            )
            .unwrap();
        let prepared = session.prepare(&cx, BFS, &args).unwrap();
        calls.set(0);
        session.execute(&cx, &prepared, &args).unwrap();
        let boundaries = calls.get();
        assert!(boundaries > 20);
        calls.set(0);
        let before = compressed_builds();
        session.execute(&cx, &prepared, &args).unwrap();
        assert_eq!(
            calls.get(),
            boundaries,
            "warm replay preserves every clock sample"
        );
        assert_eq!(compressed_builds(), before);
        session.close();
        let baseline = Arc::strong_count(&owner.authorized_parts().0.snapshot);
        for stop in 1..=boundaries {
            cutoff.set(usize::MAX);
            let mut session = owner
                .authorized_read_session(
                    &cx,
                    &authority,
                    &token,
                    "main",
                    catalog,
                    policy(),
                    memory(),
                    clock,
                )
                .unwrap();
            let prepared = session.prepare(&cx, BFS, &args).unwrap();
            calls.set(0);
            cutoff.set(stop);
            refusal(
                session.execute(&cx, &prepared, &args),
                AuthorizationError::Expired,
            );
            assert!(session.is_closed(), "expiry at {stop}");
            assert_eq!(
                Arc::strong_count(&owner.authorized_parts().0.snapshot),
                baseline
            );
            let sampled = calls.get();
            refusal(
                session.query(&cx, BFS, &args),
                AuthorizationError::ExecutionStopped,
            );
            assert_eq!(calls.get(), sampled);
        }
        let authority = issuer(5);
        let token = authority.issue_at(&grant(false), NOW).unwrap();
        let retire_at = Cell::new(usize::MAX);
        calls.set(0);
        let clock = || {
            let next = calls.get() + 1;
            calls.set(next);
            if next == retire_at.get() {
                authority.retire();
            }
            NOW
        };
        let mut session = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                clock,
            )
            .unwrap();
        let prepared = session.prepare(&cx, BFS, &args).unwrap();
        calls.set(0);
        retire_at.set(boundaries / 2);
        refusal(
            session.execute(&cx, &prepared, &args),
            AuthorizationError::AuthorityRetired,
        );
        assert!(session.is_closed());
        assert_eq!(
            Arc::strong_count(&owner.authorized_parts().0.snapshot),
            baseline
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_at_every_checkpoint_discards_output_and_compressed_memory_limits_are_binding() {
    let ((), report) = run_async_under_lab(0xa115_ea05, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), false).await;
        let owner = retained(&db, &cx, None).await;
        let authority = issuer(6);
        let token = authority.issue_at(&grant(false), NOW).unwrap();
        let args = GqlParameters::new();
        let mut session = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                || NOW,
            )
            .unwrap();
        let prepared = session.prepare(&cx, BFS, &args).unwrap();
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = c.query().with_checkpoint_probe(Arc::clone(&probe));
        let expected = session.execute(&observed, &prepared, &args).unwrap();
        let calls = probe.calls();
        assert!(calls > 20);
        let warm_probe = Arc::new(SimulationCheckpointProbe::new(None));
        let warm = c.query().with_checkpoint_probe(Arc::clone(&warm_probe));
        let before = compressed_builds();
        assert_eq!(session.execute(&warm, &prepared, &args).unwrap(), expected);
        assert_eq!(
            warm_probe.calls(),
            calls,
            "no raw source checkpoint disappears on reuse"
        );
        assert_eq!(compressed_builds(), before);
        for at in 1..=calls {
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(at)));
            let controlled = c.query().with_checkpoint_probe(Arc::clone(&probe));
            assert!(
                session.execute(&controlled, &prepared, &args).is_err(),
                "checkpoint {at}"
            );
            // The session's mandatory final gate may checkpoint again after
            // this injected failure; no completed result may escape it.
            assert!(
                !session.is_closed(),
                "cancellation is not credential retirement"
            );
        }
        assert_eq!(session.execute(&cx, &prepared, &args).unwrap(), expected);
        let mut narrow = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                FnxMemoryLimits {
                    max_kernel_workspace_bytes: 0,
                    ..memory()
                },
                || NOW,
            )
            .unwrap();
        let before = compressed_calls();
        assert!(narrow.query(&cx, BFS, &args).is_err());
        assert!(
            compressed_calls() > before,
            "a memory refusal must not decode and retry"
        );
        assert!(!narrow.is_closed());
        assert_eq!(
            narrow
                .query(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &args)
                .unwrap(),
            QueryResult::Rows {
                columns: vec!["p".to_owned()],
                rows: [10, 20, 30]
                    .into_iter()
                    .map(
                        |p| vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                            p
                        )))]
                    )
                    .collect(),
            },
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_and_composed_calls_reuse_one_bounded_compressed_projection() {
    let _turn = crate::query::authorized::analytics::reuse_test_turn();
    let ((), report) = run_async_under_lab(0xa115_ea06, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit(), false).await;
        let owner = retained(&db, &cx, None).await;
        let authority = issuer(7);
        let token = authority.issue_at(&grant(false), NOW).unwrap();
        let mut session = owner
            .authorized_read_session(
                &cx,
                &authority,
                &token,
                "main",
                catalog,
                policy(),
                memory(),
                || NOW,
            )
            .unwrap();
        let args = GqlParameters::new();
        let before = compressed_builds();
        let expected = session.query(&cx, BFS, &args).unwrap();
        assert_eq!(compressed_builds(), before + 1);
        let prepared = session.prepare(&cx, BFS, &args).unwrap();
        assert_eq!(session.execute(&cx, &prepared, &args).unwrap(), expected);
        assert_eq!(session.query(&cx, BFS, &args).unwrap(), expected);
        assert_eq!(compressed_builds(), before + 1);

        let composed = "CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R') \
            YIELD vertex AS reached, distance AS first_distance WITH reached, first_distance \
            CALL fnx.single_source_shortest_path_length(args => [1], relation => 'R') \
            YIELD vertex, distance WHERE vertex = reached RETURN vertex, distance ORDER BY vertex";
        let calls = compressed_calls();
        assert_eq!(
            session.query(&cx, composed, &args).unwrap(),
            db.query(&cx, composed, &args, catalog, policy()).unwrap(),
        );
        assert!(compressed_calls() >= calls + 2);
        assert_eq!(
            compressed_builds(),
            before + 1,
            "different remaining source counters still replay the same projection",
        );
        let status = db.read_cache_status(&cx).unwrap().authorized_projections;
        assert_eq!(status.entries, 1);
        assert!(status.retained_bytes > 0);
        assert!(status.retained_bytes <= status.max_bytes);

        // Both representations share the existing explicit host quota. A
        // limit below this complete image/directory/tape cost releases it;
        // ordinary execution then succeeds without retaining a partial entry.
        assert_eq!(
            retention(&owner, &cx, status.retained_bytes - 1)
                .authorized_projections
                .entries,
            0,
        );
        let before = compressed_builds();
        for _ in 0..2 {
            assert_eq!(session.query(&cx, BFS, &args).unwrap(), expected);
        }
        assert_eq!(compressed_builds(), before + 2);
        assert_eq!(
            db.read_cache_status(&cx)
                .unwrap()
                .authorized_projections
                .entries,
            0
        );
        retention(&owner, &cx, status.retained_bytes);
        let before = compressed_builds();
        for _ in 0..2 {
            assert_eq!(session.execute(&cx, &prepared, &args).unwrap(), expected);
        }
        assert_eq!(compressed_builds(), before + 1);
        assert_eq!(
            db.read_cache_status(&cx)
                .unwrap()
                .authorized_projections
                .retained_bytes,
            status.retained_bytes,
        );

        // Keeping the cache owner alive must not pin its decoded generation.
        let caches = Arc::clone(&owner.authorized_parts().0.snapshot.read_caches);
        let weak = Arc::downgrade(&owner.authorized_parts().0.snapshot);
        session.close();
        drop(owner);
        drop(db);
        assert!(weak.upgrade().is_none());
        let mut entries = caches.authorized.try_lock().unwrap();
        let retired = crate::query::read_cache::live::LiveCache::prune(&mut entries);
        assert_eq!(caches.authorized.usage(&entries).entries, 0);
        drop(entries);
        drop(retired);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_compressed_construction_query_checkpoint_has_one_live_guard() {
    let ((), report) = run_async_under_lab(0xa115_ea07, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&c.commit(), false).await;
        let owner = retained(&db, &c.query(), None).await;
        let (_, partition, as_of, _) = owner.authorized_parts();
        for direction in [
            Directedness::Directed,
            Directedness::Reversed,
            Directedness::Undirected,
        ] {
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let cx = c.query().with_checkpoint_probe(Arc::clone(&probe));
            let mut options = FnxReadOptions::whole_graph(Some(as_of));
            options.selection.relation = Some(R);
            options.projection.directedness = direction;
            let mut observed = Vec::new();
            let graph = SealedGraphView::build_with_checkpoint(
                &cx,
                partition,
                &[VId(1), VId(2), VId(3)],
                SealedProjectionSpec {
                    as_of,
                    selection: options.selection,
                    projection: options.projection,
                },
                SealedProjectionMask::UNMASKED,
                options.projection_limits,
                || {
                    observed.push(probe.calls());
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(graph.node_count(), 3);
            assert!(probe.calls() > 10);
            assert_eq!(
                observed,
                (1..=probe.calls()).collect::<Vec<_>>(),
                "{direction:?}: raw row opens, incoming construction and pulls must all enter the tape",
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn compressed_footprint(
    owner: &SealedReadView,
    cx: &QueryCx,
    authority: &Authority,
    token: &CapabilityToken,
    host: GqlQueryPolicy,
    options: FnxReadOptions,
) -> (String, usize, usize) {
    let probe = Arc::new(SimulationCheckpointProbe::new(None));
    let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
    let clock_calls = Cell::new(0);
    let result = owner
        .authorized_read_session(
            &observed,
            authority,
            token,
            "main",
            catalog,
            host,
            memory(),
            || {
                clock_calls.set(clock_calls.get() + 1);
                NOW
            },
        )
        .and_then(|mut session| measured_options(&mut session, &observed, options));
    (format!("{result:?}"), probe.calls(), clock_calls.get())
}

#[test]
fn compressed_reuse_matches_cold_rows_statistics_and_each_fresh_allowance() {
    let _turn = crate::query::authorized::analytics::reuse_test_turn();
    let ((), report) = run_async_under_lab(0xa115_ea08, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let cold_db = database(&c.commit(), false).await;
        let warm_db = database(&c.commit(), false).await;
        let cold = retained(&cold_db, &cx, None).await;
        let warm = retained(&warm_db, &cx, None).await;
        retention(&cold, &cx, 0);
        let authority = issuer(8);
        let open = grant(false);
        let token = authority.issue_at(&open, NOW).unwrap();
        let options = FnxReadOptions::whole_graph(None);
        let seeded = compressed_footprint(&warm, &cx, &authority, &token, policy(), options);
        assert!(seeded.0.starts_with("Ok("), "{seeded:?}");
        assert_eq!(
            compressed_footprint(&cold, &cx, &authority, &token, policy(), options),
            seeded,
        );
        let mut cases = vec![(open.clone(), policy(), options)];
        for limit in [0, 3, 20, 100_000] {
            let mut changed = options;
            changed.source_limits.max_work_units = limit;
            cases.push((open.clone(), policy(), changed));
        }
        for limit in [0, 2, 4, 100] {
            let mut changed = options;
            changed.source_limits.max_scratch_entries = limit;
            cases.push((open.clone(), policy(), changed));
        }
        for limit in [0, 15, 100, 1_000_000] {
            let mut signed = open.clone();
            signed.limits.max_work = limit;
            cases.push((signed, policy(), options));
            let mut host = policy();
            host.evaluator.max_work_units = limit;
            cases.push((open.clone(), host, options));
        }
        for limit in [0, 2, 3, 100] {
            let mut signed = open.clone();
            signed.limits.max_nodes = limit;
            cases.push((signed, policy(), options));
            let mut signed = open.clone();
            signed.limits.max_rows = limit;
            cases.push((signed, policy(), options));
        }
        for (index, (signed, host, options)) in cases.into_iter().enumerate() {
            let token = authority.issue_at(&signed, NOW).unwrap();
            let before = compressed_builds();
            let reused = compressed_footprint(&warm, &cx, &authority, &token, host, options);
            assert_eq!(compressed_builds(), before, "case {index} must replay");
            assert_eq!(
                compressed_footprint(&cold, &cx, &authority, &token, host, options),
                reused,
                "case {index}: rows/refusal, admitted statistics, QueryCx and clock ordering",
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
