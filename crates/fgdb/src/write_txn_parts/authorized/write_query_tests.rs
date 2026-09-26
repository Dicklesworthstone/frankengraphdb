//! Public write -> masked staged query -> one native completion regressions.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GraphMutationProgramError,
    GraphSymbol, GraphSymbolKind, PreparedGraphText, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const H: LabelId = LabelId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const SECRET: PropertyKeyId = PropertyKeyId(2);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x91; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x90; 32], NS, [0x92; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9991), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R]),
        properties: Scope::only([P]),
        rights: Rights::ReadWrite,
        limits: QueryLimits { max_nodes: 100_000, max_work: 1_000_000, max_rows: 100 },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 100_000), 100, 100, 100)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn program(text: &str) -> PreparedGraphWriteProgram {
    PreparedGraphWriteScript::prepare(text, R, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap()
}
fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols).unwrap()
        .bind_parameters(&GqlParameters::new()).unwrap().with_duplicates()
}
fn values() -> PreparedGraphPattern<GraphValueRow> {
    query("MATCH (n:Visible) RETURN n.p AS p, n.secret AS hidden ORDER BY p")
}
fn update() -> PreparedGraphWriteProgram {
    program("MATCH (n:Visible) WHERE n.p=10 SET n.p=11")
}
fn row(items: &[Option<i64>]) -> GraphValueRow {
    GraphValueRow::from_owned_values(items.iter().map(|value| {
        GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
    }).collect())
}
fn authorization(error: &Fault) -> Option<Error> {
    let mut cause: Option<&(dyn core::error::Error + 'static)> = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}
fn lab<T, F>(seed: u64, body: impl FnOnce(PurposeContexts) -> F + Send + 'static) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    let (result, report) = run_async_under_lab(seed, |root| async move {
        body(PurposeContexts::narrow_runtime_root(&root)).await
    });
    assert!(report.lab_test_passed(), "{report:?}");
    result
}
async fn seed(db: &mut Database<MemVfs>, cx: &CommitCx, hidden: bool) {
    let mut batch = WriteBatch::new(R);
    for (id, p) in [(1, 10), (2, 20)] {
        let mut fields = vec![(P, CanonicalScalar::Int(p))];
        if hidden { fields.push((SECRET, CanonicalScalar::Int(70 + p))); }
        batch.create_vertex(VId(id), vec![L], fields);
    }
    let mut fields = vec![(P, CanonicalScalar::Int(1))];
    if hidden { fields.push((SECRET, CanonicalScalar::Int(99))); }
    batch.add_edge(EId(1), VId(1), VId(2), fields);
    if hidden {
        batch.create_vertex(VId(3), vec![H], vec![(P, CanonicalScalar::Int(999))]);
        batch.add_edge(EId(3), VId(1), VId(3), vec![]);
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(2), VId(1), VId(2), vec![]);
        db.write(cx, other).await.unwrap();
    }
}

#[test]
fn query_returns_canonical_new_values_with_masked_fields_and_one_reopenable_commit() {
    lab(0xa9d1, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let frontier = db.frontier().unwrap();
        let authority = authority();
        // Only the two projected rows are delivered. The four internal created/
        // mutated identity occurrences are NOT a second, hidden receipt bill.
        let token = authority.issue_at(&grant(), NOW).unwrap().attenuate(Restriction::MaxRows(2)).unwrap();
        let program = program("MATCH (a:Visible)-[e:R]->(b:Visible) WHERE a.p=10 SET a.p=11, e.p=2; \
            MATCH (a:Visible) WHERE a.p=11 CREATE (b:Visible {p:30}), (a)-[:R {p:3}]->(b)");
        let query = query("MATCH (a:Visible)-[e:R]->(b:Visible) \
            RETURN a.p AS a, e.p AS edge, b.p AS b, e.secret AS hidden ORDER BY b");
        let (stats, result, completion) = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &token, "main", &program, &query, policy(), || NOW,
        ).await.unwrap();
        assert_eq!((stats.created_vertices, stats.created_edges, stats.mutation_effects), (1, 1, 2));
        assert_eq!(result.value, vec![row(&[Some(11), Some(2), Some(20), None]), row(&[Some(11), Some(3), Some(30), None])]);
        let seq = db.frontier().unwrap();
        assert_eq!(seq.0, frontier.0 + 1);
        assert_eq!(completion, EmbeddedTxnCompletion::WriteCommitted { commit_seq: seq });
        assert_eq!(db.edge(EId(1)).unwrap().unwrap().props,
            vec![(P, CanonicalScalar::Int(2)), (SECRET, CanonicalScalar::Int(99))]);
        let state = (db.vertices().unwrap(), db.edges().unwrap());
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys()).await.unwrap();
        assert_eq!((db.vertices().unwrap(), db.edges().unwrap()), state);
        let reopened = db.execute_graph_pattern_authorized(&query_cx, &authority, &token,
            "main", &query, policy().mutations.query, || NOW).unwrap();
        assert_eq!(reopened, result.value);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn final_query_error_or_result_quota_rolls_back_every_staged_write() {
    lab(0xa9d2, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for mode in 0..3 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, true).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let result_query = if mode == 0 {
                query("MATCH (n:Visible) RETURN n.p / (n.p - n.p) AS invalid")
            } else { values() };
            let token = token.attenuate(Restriction::MaxRows(if mode == 0 { 100 } else { mode - 1 })).unwrap();
            let error = db.execute_graph_write_program_then_query_authorized(
                &txn, &query_cx, &commit, &authority, &token, "main", &update(), &result_query, policy(), || NOW,
            ).await.unwrap_err();
            assert!(matches!(&error, Fault::Query(_)), "{error:?}");
            if mode != 0 { assert_eq!(authorization(&error), Some(Error::LimitExceeded(LimitDimension::Rows))); }
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn original_native_record_row_work_and_scratch_ceilings_cover_both_phases() {
    lab(0xa9d3, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let (write, read, _) = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &token, "main", &update(), &values(), policy(), || NOW,
        ).await.unwrap();
        let records = write.selection.snapshot_records + read.rows.snapshot_records;
        let rows = write.selection.result_rows + read.rows.result_rows;
        assert_eq!((records, rows), (4, 3)); // independent two-source/one-update witness
        let work = write.evaluator.work_units + read.evaluator.work_units;
        let scratch = write.evaluator.scratch_entries + read.evaluator.scratch_entries;
        for dimension in 0..4 {
            for below in [false, true] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                seed(&mut db, &commit, false).await;
                let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                let mut bounded = policy();
                let mut caps = [records, rows, work, scratch];
                caps[dimension] -= u64::from(below);
                bounded.mutations.query = GqlQueryPolicy::new(caps[0], caps[1], caps[2], caps[3]);
                let result = db.execute_graph_write_program_then_query_authorized(
                    &txn, &query_cx, &commit, &authority, &token, "main", &update(), &values(), bounded, || NOW,
                ).await;
                if below {
                    match result.unwrap_err() {
                        Fault::Query(GqlQueryError::Rows(error)) if dimension < 2 => {
                            assert_eq!(error.limit, caps[dimension]);
                            assert_eq!(error.observed, caps[dimension] + 1);
                        }
                        Fault::Query(GqlQueryError::Evaluator(error)) if dimension >= 2 => {
                            assert_eq!(error.limit, caps[dimension]);
                            assert_eq!(error.observed, u128::from(caps[dimension]) + 1);
                        }
                        other => panic!("unexpected residual quota error: {other:?}"),
                    }
                    assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                } else {
                    assert_eq!(result.unwrap().1.value, vec![row(&[Some(11), None]), row(&[Some(20), None])]);
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
        }
    });
}

#[test]
fn readwrite_preflight_precedes_creation_even_when_the_result_would_be_empty() {
    lab(0xa9d4, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let insert = program("CREATE (n:Visible {p:10})");
        let empty = query("MATCH (n:Visible) WHERE n.p=999 RETURN n");
        for rights in [Rights::Read, Rights::Write] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut grant = grant();
            grant.rights = rights;
            grant.limits.max_work = 0;
            let token = authority.issue_at(&grant, NOW).unwrap();
            let error = db.execute_graph_write_program_then_query_authorized(
                &txn, &query_cx, &commit, &authority, &token, "main", &insert, &empty, policy(), || NOW,
            ).await.unwrap_err();
            assert_eq!(authorization(&error), Some(Error::PermissionDenied));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(db.allocate_identity(&query_cx, GraphInsertRequest::Vertex { row: 0, vertex: 0 }).unwrap(),
                ElementId::Vertex(VId(1)));
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn program_failure_is_not_retried_or_replaced_by_a_result_query_failure() {
    lab(0xa9d5, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, true).await;
        let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
        let program = program("MATCH (n:Visible) WHERE n.p=10 SET n.p=11, n.secret=n.secret");
        let invalid = query("MATCH (n:Visible) RETURN n.p / (n.p - n.p) AS invalid");
        let error = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &token, "main", &program, &invalid, policy(), || NOW,
        ).await.unwrap_err();
        assert!(matches!(&error, Fault::Program(_)));
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn empty_output_still_commits_writes_but_an_empty_write_program_read_closes() {
    lab(0xa9d6, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let before = db.frontier().unwrap();
        let empty_write = program("MATCH (n:Visible) WHERE n.p=999 SET n.p=1");
        let (_, result, completion) = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &token, "main", &empty_write, &values(), policy(), || NOW,
        ).await.unwrap();
        assert_eq!(result.value.len(), 2);
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), before);
        let zero = token.attenuate(Restriction::MaxRows(0)).unwrap();
        let empty = query("MATCH (n:Visible) WHERE n.p=999 RETURN n");
        let (_, result, completion) = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &zero, "main", &update(), &empty, policy(), || NOW,
        ).await.unwrap();
        assert!(result.value.is_empty());
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn result_selection_shares_signed_nodes_and_hidden_graph_does_not_move_thresholds() {
    lab(0xa9d7, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for dimension in [LimitDimension::Nodes, LimitDimension::Work] {
            let mut floors = Vec::new();
            for mode in 0..3 {
                let (mut low, mut high) = (0_u64, 8192_u64);
                while low < high {
                    let middle = low + (high - low) / 2;
                    let restriction = match dimension {
                        LimitDimension::Nodes => Restriction::MaxNodes(middle),
                        LimitDimension::Work => Restriction::MaxWork(middle),
                        _ => unreachable!(),
                    };
                    let limited = token.attenuate(restriction).unwrap();
                    let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                    seed(&mut db, &commit, mode == 2).await;
                    let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
                    let result = if mode == 0 {
                        db.execute_graph_write_program_authorized(&txn, &query_cx, &commit,
                            &authority, &limited, "main", &update(), policy(), || NOW)
                            .await.map(|_| ()).map_err(Fault::Program)
                    } else {
                        db.execute_graph_write_program_then_query_authorized(&txn, &query_cx, &commit,
                            &authority, &limited, "main", &update(), &values(), policy(), || NOW)
                            .await.map(|(_, result, _)| {
                                assert_eq!(result.value, vec![row(&[Some(11), None]), row(&[Some(20), None])]);
                            })
                    };
                    match result {
                        Ok(()) => high = middle,
                        Err(error) => {
                            assert_eq!(authorization(&error), Some(Error::LimitExceeded(dimension)));
                            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
                            low = middle + 1;
                        }
                    }
                    assert_eq!(txn.outstanding_obligations(), 0);
                }
                assert!(low > 0 && low < 8192);
                floors.push(low);
            }
            assert_eq!(floors[1], floors[2], "hidden data changed {dimension:?}");
            if dimension == LimitDimension::Nodes {
                assert_eq!(floors[1], floors[0] + 2, "final source got a fresh allowance");
            } else {
                assert!(floors[1] > floors[0], "final query was not charged");
            }
        }
    });
}

#[test]
fn every_expiry_boundary_before_result_acceptance_and_native_commit_is_atomic() {
    lab(0xa9d8, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let calls = AtomicU64::new(0);
        db.execute_graph_write_program_then_query_authorized(&txn, &query_cx, &commit,
            &authority, &token, "main", &update(), &values(), policy(), || {
                calls.fetch_add(1, Ordering::Relaxed); NOW
            }).await.unwrap();
        let total = calls.load(Ordering::Relaxed);
        assert!(total > 10);
        for cutoff in 0..total {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            seed(&mut db, &commit, false).await;
            let before = (db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap());
            let calls = AtomicU64::new(0);
            let error = db.execute_graph_write_program_then_query_authorized(&txn, &query_cx, &commit,
                &authority, &token, "main", &update(), &values(), policy(), || {
                    if calls.fetch_add(1, Ordering::Relaxed) < cutoff { NOW } else { 10_000 }
                }).await.unwrap_err();
            assert_eq!(authorization(&error), Some(Error::Expired), "cutoff={cutoff}: {error:?}");
            if cutoff == total - 1 {
                assert!(matches!(error, Fault::Program(super::super::Fault::Program(
                    GraphMutationProgramError::Interrupted { completed_statements: 1, .. }
                ))));
            }
            assert_eq!((db.frontier().unwrap(), db.vertices().unwrap(), db.edges().unwrap()), before);
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn residual_policy_arithmetic_preserves_unlimited_axes_and_rejects_inconsistent_usage() {
    let used = GraphWriteProgramStats {
        completed_statements: 1,
        selection: GqlExecutionStats { snapshot_records: 3, result_rows: 2 },
        evaluator: GlaExecutionStats { work_units: 7, scratch_entries: 5 },
        target_vertex_visits: 0, target_edge_visits: 0, mutation_effects: 0,
        created_vertices: 1, created_edges: 0,
    };
    for rows in [GqlExecutionBudget::UNLIMITED, GqlExecutionBudget::snapshot_records(3),
        GqlExecutionBudget::result_rows(2), GqlExecutionBudget::new(3, 2)]
    {
        let policy = GqlQueryPolicy { rows, evaluator: GlaExecutionLimits::new(7, 5) };
        let remaining = remaining(policy, used).unwrap();
        assert_eq!(remaining.rows.max_snapshot_records(), rows.max_snapshot_records().map(|_| 0));
        assert_eq!(remaining.rows.max_result_rows(), rows.max_result_rows().map(|_| 0));
        assert_eq!((remaining.evaluator.max_work_units, remaining.evaluator.max_scratch_entries), (0, 0));
    }
    assert!(remaining(GqlQueryPolicy::new(2, 2, 7, 5), used).is_err());
    assert!(remaining(GqlQueryPolicy::new(3, 1, 7, 5), used).is_err());
    assert!(remaining(GqlQueryPolicy::new(3, 2, 6, 5), used).is_err());
    assert!(remaining(GqlQueryPolicy::new(3, 2, 7, 4), used).is_err());
}

#[test]
fn result_query_preserves_parallel_bags_then_observes_complete_overlay_deletion() {
    lab(0xa9d9, |contexts| async move {
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut grant = grant();
        grant.labels = Scope::All;
        grant.relations = Scope::All;
        grant.properties = Scope::All;
        let token = authority.issue_at(&grant, NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit, false).await;
        let mut parallel = WriteBatch::new(R);
        parallel.add_edge(EId(4), VId(1), VId(2), vec![]);
        db.write(&commit, parallel).await.unwrap();
        let before = db.frontier().unwrap();
        let no_write = program("MATCH (n:Visible) WHERE n.p=999 SET n.p=1");
        let bag = query("MATCH (a:Visible)-[:R]->(b:Visible) RETURN a.p AS a, b.p AS b");
        let (_, result, completion) = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &token, "main", &no_write, &bag, policy(), || NOW,
        ).await.unwrap();
        assert_eq!(result.value, vec![row(&[Some(10), Some(20)]), row(&[Some(10), Some(20)])]);
        assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
        assert_eq!(db.frontier().unwrap(), before);
        let deletion = program("MATCH (a:Visible)-[e:R]->(b:Visible) DELETE e; \
            MATCH (n:Visible) WHERE n.p=10 DELETE n");
        let (stats, result, completion) = db.execute_graph_write_program_then_query_authorized(
            &txn, &query_cx, &commit, &authority, &token, "main", &deletion, &values(), policy(), || NOW,
        ).await.unwrap();
        assert_eq!(stats.mutation_effects, 3);
        assert_eq!(result.value, vec![row(&[Some(20), None])]);
        assert!(db.vertex(VId(1)).unwrap().is_none() && db.edges().unwrap().is_empty());
        assert!(matches!(completion, EmbeddedTxnCompletion::WriteCommitted { .. }));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn cumulative_error_does_not_wrap_a_maximal_row_observation() {
    let used = GraphWriteProgramStats {
        completed_statements: 1,
        selection: GqlExecutionStats { snapshot_records: 0, result_rows: u64::MAX },
        evaluator: GlaExecutionStats::default(),
        target_vertex_visits: 0, target_edge_visits: 0, mutation_effects: 0,
        created_vertices: 0, created_edges: 0,
    };
    let error = cumulative_error(GqlQueryError::Rows(fgdb_gql::GqlBudgetExceeded {
        dimension: GqlBudgetDimension::ResultRows, limit: 0, observed: 1,
    }), GqlQueryPolicy::new(0, u64::MAX, 100, 100), used);
    assert_eq!(authorization(&error), Some(Error::TooLarge));
}
