//! Real tokens, parameter binding, canonical program state and Chronicle.
use super::*;
use crate::{DatabaseKeys, MemVfs};
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlQueryPolicy, GraphEdgeMergeOutcome, GraphMutationProgramDimension,
    GraphMutationProgramError, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteScriptBatchError,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const L: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const SECRET: PropertyKeyId = PropertyKeyId(3);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x85; 32]);
const NOW: u64 = 100;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x84; 32], NS, [0x86; 32])
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(9985), NS, "graph", SchemaEpoch(1), 1).unwrap()
}
fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([L]),
        relations: Scope::only([R, S]),
        properties: Scope::only([P, Q]),
        rights: Rights::ReadWrite,
        limits: QueryLimits {
            max_nodes: 1_000_000,
            max_work: 10_000_000,
            max_rows: 100_000,
        },
        expires_at_ms: 10_000,
    }
}
fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 1_000_000),
        1_000,
        1_000,
        1_000,
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(SECRET)),
        _ => None,
    }
}
fn prepare(text: &str) -> PreparedGraphWriteScript {
    PreparedGraphWriteScript::prepare(text, R, symbols).unwrap()
}
fn script() -> PreparedGraphWriteScript {
    prepare("CREATE (n:Visible {p:$key});\nMATCH (n:Visible) WHERE n.p=$key SET n.q=$value")
}
fn values(key: i64, value: i64) -> GqlParameters {
    GqlParameters::new()
        .with_int64("key", key)
        .unwrap()
        .with_int64("value", value)
        .unwrap()
}
fn authorization(error: &Fault) -> Option<Error> {
    // Inspect the typed native error chain, not Debug strings that could hide
    // a wrong carrier or mistake a diagnostic's contents for the actual cause.
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

#[test]
fn single_script_executes_dependent_statements_with_and_without_identity_receipts() {
    lab(0xa9c1, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        for returning in [false, true] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            // The script's one CREATE takes engine vertex counter 1 of this handle.
            let created = crate::write_txn::engine_vertex(&keys(), 1);
            let token = token
                .attenuate(Restriction::MaxRows(if returning { 2 } else { 0 }))
                .unwrap();
            let stats = if returning {
                let (receipt, completion) = db
                    .execute_graph_write_script_returning_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &token,
                        "main",
                        &script(),
                        &values(7, 70),
                        policy(),
                        || NOW,
                    )
                    .await
                    .unwrap();
                assert_eq!(receipt.steps()[0].created_vertices(), Some(&[created][..]));
                assert_eq!(receipt.steps()[1].mutation_targets(), Some(&[created][..]));
                assert!(matches!(
                    completion,
                    EmbeddedTxnCompletion::WriteCommitted { .. }
                ));
                receipt.stats()
            } else {
                db.execute_graph_write_script_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &script(),
                    &values(7, 70),
                    policy(),
                    || NOW,
                )
                .await
                .unwrap()
                .0
            };
            assert_eq!(
                (
                    stats.completed_statements,
                    stats.created_vertices,
                    stats.mutation_effects
                ),
                (2, 1, 1)
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(
                db.vertex(created).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(7)), (Q, CanonicalScalar::Int(70))]
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn relationship_ingestion_reuses_shared_vertices_and_edges_and_reopens_one_commit() {
    lab(0xa9c2, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.frontier().unwrap();
        let authority = authority();
        let token = authority
            .issue_at(&grant(), NOW)
            .unwrap()
            .attenuate(Restriction::MaxRows(9))
            .unwrap();
        let script = prepare(
            "MERGE (a:Visible {p:$source}); MERGE (b:Visible {p:$target}); \
             MATCH (a:Visible), (b:Visible) WHERE a.p=$source AND b.p=$target \
             MERGE (a)-[e:R]->(b) ON CREATE SET e.q=$value ON MATCH SET e.q=$value",
        );
        let arguments = [(1, 11), (2, 22), (1, 33)]
            .into_iter()
            .map(|(target, value)| {
                GqlParameters::new()
                    .with_int64("source", 0)
                    .unwrap()
                    .with_int64("target", target)
                    .unwrap()
                    .with_int64("value", value)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let bound = script.bind_parameter_sets(&arguments).unwrap();
        let (receipt, completion) = db
            .execute_graph_write_script_batch_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &script,
                &arguments,
                9,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        let stats = receipt.stats();
        assert_eq!(
            (
                stats.completed_statements,
                stats.created_vertices,
                stats.created_edges,
                stats.mutation_effects
            ),
            (9, 3, 2, 3)
        );
        // Record 0 creates a {p:0} and b {p:1} (engine vertex counters 1 and 2)
        // and their relationship (engine edge counter 1); record 1 creates the
        // second relationship (engine edge counter 2); record 2 matches the first.
        let (a, b) = (
            crate::write_txn::engine_vertex(&keys(), 1),
            crate::write_txn::engine_vertex(&keys(), 2),
        );
        let (first, second) = (
            crate::write_txn::engine_edge(&keys(), 1),
            crate::write_txn::engine_edge(&keys(), 2),
        );
        for (record, outcome) in [
            GraphEdgeMergeOutcome::Created(first),
            GraphEdgeMergeOutcome::Created(second),
            GraphEdgeMergeOutcome::Matched(first),
        ]
        .into_iter()
        .enumerate()
        {
            let steps = bound.record_receipts(&receipt, record).unwrap();
            assert_eq!(steps.len(), 3);
            assert_eq!(steps[2].merged_edge(), Some(outcome));
        }
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.vertices().unwrap().len(), 3);
        let one = db.edge(first).unwrap().unwrap();
        assert_eq!((one.entry.src, one.entry.dst), (a, b));
        assert_eq!(one.props, vec![(Q, CanonicalScalar::Int(33))]);
        assert_eq!(
            db.edge(second).unwrap().unwrap().props,
            vec![(Q, CanonicalScalar::Int(22))]
        );
        let state = (
            db.frontier().unwrap(),
            db.vertices().unwrap(),
            db.edges().unwrap(),
        );
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            (
                db.frontier().unwrap(),
                db.vertices().unwrap(),
                db.edges().unwrap()
            ),
            state
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn a_late_binding_error_opens_no_transaction_and_reserves_no_identity() {
    lab(0xa9c3, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let arguments = [values(1, 10), values(2, 20), GqlParameters::new()];
        let error = db
            .execute_graph_write_script_batch_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &script(),
                &arguments,
                6,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::BatchBinding(GraphWriteScriptBatchError::Arguments {
                argument_set: 2,
                ..
            })
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(
            db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                .unwrap(),
            ElementId::Vertex(crate::write_txn::engine_vertex(&keys(), 1))
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn authority_rights_and_merge_relation_preflight_precede_argument_values() {
    lab(0xa9c4, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut scope = grant();
            if mode == 0 {
                scope.rights = Rights::Read;
            }
            if mode == 1 {
                scope.rights = Rights::Write;
                scope.limits.max_work = 0;
            }
            if mode == 2 {
                scope.relations = Scope::only([S]);
                scope.limits.max_work = 0;
            }
            let token = authority.issue_at(&scope, NOW).unwrap();
            let script = prepare(
                "CREATE (n:Visible {p:$key}); \
                 MATCH (a:Visible), (b:Visible) MERGE (a)-[:R]->(b)",
            );
            let error = db
                .execute_graph_write_script_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &script,
                    &GqlParameters::new(),
                    policy(),
                    || if mode == 3 { 10_000 } else { NOW },
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(match mode {
                    0 | 1 => Error::PermissionDenied,
                    2 => Error::ScopeDenied,
                    _ => Error::Expired,
                })
            );
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(
                db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                    .unwrap(),
                ElementId::Vertex(crate::write_txn::engine_vertex(&keys(), 1))
            );
            assert_eq!(txn.outstanding_obligations(), 0);
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let other = Authority::new(
            AuthKey::from_seed(9986),
            DatabaseSecurityNamespaceId([0x99; 32]),
            "graph",
            SchemaEpoch(1),
            1,
        )
        .unwrap();
        let token = other.issue_at(&grant(), NOW).unwrap();
        let error = db
            .execute_graph_write_script_authorized(
                &txn,
                &query,
                &commit,
                &other,
                &token,
                "main",
                &script(),
                &GqlParameters::new(),
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::WrongAuthority));
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn late_runtime_expression_failure_retains_record_coordinates_and_rolls_back_all_records() {
    lab(0xa9c5, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let script = prepare(
            "CREATE (n:Visible {p:$key});\nMATCH (n:Visible) WHERE n.p=$key SET n.q=n.p/$value",
        );
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_script_batch_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &script,
                &[values(6, 2), values(7, 0)],
                4,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        let Fault::BatchProgram {
            location: Some(location),
            source,
        } = error
        else {
            panic!("runtime refusal must retain its native batch-program carrier")
        };
        assert_eq!((location.argument_set, location.statement), (1, 1));
        assert_eq!(location.span, script.statement_span(1).unwrap());
        assert!(matches!(
            source,
            GraphWriteProgramError::Program(GraphMutationProgramError::Statement {
                statement: 3,
                ..
            })
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        // Records 0 and 1 each issued one vertex (engine vertex counters 1 and
        // 2) before the rollback, so the next engine vertex is counter 3.
        assert_eq!(
            db.allocate_identity(&query, GraphInsertRequest::Vertex { row: 0, vertex: 0 })
                .unwrap(),
            ElementId::Vertex(crate::write_txn::engine_vertex(&keys(), 3)),
            "issued prefix identities are not reclaimed"
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn forbidden_noop_on_a_later_record_refuses_without_publishing_a_prefix() {
    lab(0xa9c6, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let script = prepare(
            "CREATE (n:Visible {p:$key}); \
             MATCH (n:Visible) WHERE n.p=$key AND n.p=2 SET n.secret=n.secret",
        );
        let args = [1, 2]
            .into_iter()
            .map(|key| GqlParameters::new().with_int64("key", key).unwrap())
            .collect::<Vec<_>>();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &script,
                &args,
                4,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert!(
            matches!(error, Fault::BatchProgram { location: Some(ref location), .. }
            if location.argument_set == 1 && location.statement == 1)
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn receipt_creation_and_effect_limits_are_whole_batch_not_per_record() {
    lab(0xa9c7, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let full = authority.issue_at(&grant(), NOW).unwrap();
        let script = script();
        let args = [values(1, 10), values(2, 20), values(3, 30)];
        for mode in 0..4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut policy = policy();
            let token = full
                .attenuate(Restriction::MaxRows(if mode == 0 { 5 } else { 6 }))
                .unwrap();
            if mode == 1 {
                policy.max_created_vertices = 2;
            }
            if mode == 2 {
                policy.mutations.max_effects = 2;
            }
            let result = db
                .execute_graph_write_script_batch_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &script,
                    &args,
                    6,
                    policy,
                    || NOW,
                )
                .await;
            if mode == 3 {
                let (receipt, _) = result.unwrap();
                assert_eq!(receipt.steps().len(), 6);
                assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            } else {
                let error = result.unwrap_err();
                match mode {
                    0 => assert_eq!(
                        authorization(&error),
                        Some(Error::LimitExceeded(LimitDimension::Rows))
                    ),
                    1 => assert!(matches!(error, Fault::BatchProgram {
                        location: Some(ref location), source: GraphWriteProgramError::CreationBudget {
                            statement: 4, dimension: GraphInsertLimitDimension::Vertices, limit: 2, observed: 3,
                        },
                    } if location.argument_set == 2 && location.statement == 0)),
                    _ => assert!(matches!(error, Fault::BatchProgram {
                        location: Some(ref location), source: GraphWriteProgramError::Program(
                            GraphMutationProgramError::Budget {
                                statement: 5, dimension: GraphMutationProgramDimension::Effects,
                                limit: 2, observed: 3,
                            }),
                    } if location.argument_set == 2 && location.statement == 1)),
                }
                assert_eq!(db.frontier().unwrap(), before);
                assert!(db.vertices().unwrap().is_empty());
            }
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn bound_batch_reuse_reauthorizes_and_preserves_read_closed_and_zero_row_modes() {
    lab(0xa9c8, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let full = authority.issue_at(&grant(), NOW).unwrap();
        let script = prepare("MERGE (n:Visible {p:$key})");
        let args = [1, 2]
            .into_iter()
            .map(|key| GqlParameters::new().with_int64("key", key).unwrap())
            .collect::<Vec<_>>();
        let batch = script.bind_parameter_sets(&args).unwrap();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let (first, _) = db
            .execute_bound_graph_write_script_batch_returning_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &full,
                "main",
                &batch,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(first.stats().created_vertices, 2);
        let before = db.frontier().unwrap();
        let zero = full.attenuate(Restriction::MaxRows(0)).unwrap();
        let mut no_creation = policy();
        no_creation.max_created_vertices = 0;
        let (stats, completion) = db
            .execute_bound_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &zero,
                "main",
                &batch,
                no_creation,
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(stats.created_vertices, 0);
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::ReadClosed { .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        let mut read = grant();
        read.rights = Rights::Read;
        let restricted = authority.issue_at(&read, NOW).unwrap();
        let error = db
            .execute_bound_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &restricted,
                "main",
                &batch,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::PermissionDenied));
        assert_eq!(db.frontier().unwrap(), before);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}

#[test]
fn binding_work_is_not_forgotten_when_the_shared_program_starts() {
    lab(0xa9c9, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let script = script();
        let args = [values(1, 10), values(2, 20)];
        let batch = script.bind_parameter_sets(&args).unwrap();
        let mut floors = Vec::new();
        for binding in [false, true] {
            let (mut low, mut high) = (0_u64, 8192_u64);
            while low < high {
                let limit = low + (high - low) / 2;
                let limited = token.attenuate(Restriction::MaxWork(limit)).unwrap();
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let before = db.frontier().unwrap();
                let result = if binding {
                    db.execute_graph_write_script_batch_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &limited,
                        "main",
                        &script,
                        &args,
                        4,
                        policy(),
                        || NOW,
                    )
                    .await
                } else {
                    db.execute_bound_graph_write_script_batch_authorized(
                        &txn,
                        &query,
                        &commit,
                        &authority,
                        &limited,
                        "main",
                        &batch,
                        policy(),
                        || NOW,
                    )
                    .await
                };
                match result {
                    Ok(_) => high = limit,
                    Err(error) => {
                        assert_eq!(
                            authorization(&error),
                            Some(Error::LimitExceeded(LimitDimension::Work))
                        );
                        assert_eq!(db.frontier().unwrap(), before);
                        assert!(db.vertices().unwrap().is_empty());
                        low = limit + 1;
                    }
                }
                assert_eq!(txn.outstanding_obligations(), 0);
            }
            assert!(low > 0 && low < 8192);
            floors.push(low);
        }
        // Independent input-derived bill: four statement slots admitted before
        // expansion, two record checkpoints, one final binding checkpoint.
        assert_eq!(
            floors[1],
            floors[0] + 4 + 2 + 1,
            "binding obtained a second permit"
        );
    });
}

#[test]
fn expiry_before_binding_during_execution_and_at_final_admission_publishes_nothing() {
    lab(0xa9ca, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let token = authority.issue_at(&grant(), NOW).unwrap();
        let script = script();
        let args = [values(1, 10), values(2, 20)];
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let calls = AtomicU64::new(0);
        db.execute_graph_write_script_batch_returning_authorized(
            &txn,
            &query,
            &commit,
            &authority,
            &token,
            "main",
            &script,
            &args,
            4,
            policy(),
            || {
                calls.fetch_add(1, Ordering::Relaxed);
                NOW
            },
        )
        .await
        .unwrap();
        let count = calls.load(Ordering::Relaxed);
        assert!(count > 10);
        // Every trusted-clock boundary, including the binder's preallocation,
        // each input record, the shared executor and final native admission.
        for cutoff in 0..count {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let calls = AtomicU64::new(0);
            let error = db
                .execute_graph_write_script_batch_returning_authorized(
                    &txn,
                    &query,
                    &commit,
                    &authority,
                    &token,
                    "main",
                    &script,
                    &args,
                    4,
                    policy(),
                    || {
                        if calls.fetch_add(1, Ordering::Relaxed) < cutoff {
                            NOW
                        } else {
                            10_000
                        }
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(
                authorization(&error),
                Some(Error::Expired),
                "cutoff={cutoff}: {error:?}"
            );
            if cutoff == count - 1 {
                assert!(matches!(error, Fault::BatchProgram { location: None, .. }));
            }
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(txn.outstanding_obligations(), 0);
        }
    });
}

#[test]
fn explicit_larger_batch_uses_one_write_only_transaction_not_per_record_commits() {
    lab(0xa9cb, |contexts| async move {
        let commit = contexts.commit();
        let query = contexts.query();
        let txn = contexts.txn();
        let authority = authority();
        let mut scope = grant();
        scope.rights = Rights::Write;
        scope.limits.max_rows = 0;
        let token = authority.issue_at(&scope, NOW).unwrap();
        let script = prepare("CREATE (n:Visible {p:$key})");
        let args = (0..65)
            .map(|key| GqlParameters::new().with_int64("key", key).unwrap())
            .collect::<Vec<_>>();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &script,
                &args,
                64,
                policy(),
                || NOW,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            Fault::BatchBinding(GraphWriteScriptBatchError::TooManyStatements {
                limit: 64,
                observed: 65,
            })
        ));
        assert_eq!(db.frontier().unwrap(), before);
        let (stats, completion) = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &authority,
                &token,
                "main",
                &script,
                &args,
                65,
                policy(),
                || NOW,
            )
            .await
            .unwrap();
        assert_eq!(
            (stats.completed_statements, stats.created_vertices),
            (65, 65)
        );
        assert!(matches!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted { .. }
        ));
        assert_eq!(db.frontier().unwrap().0, before.0 + 1);
        assert_eq!(db.vertices().unwrap().len(), 65);
        // The refused admission issued nothing, so the committed batch began at
        // engine vertex counter 1.
        let first = crate::write_txn::engine_vertex(&keys(), 1);
        assert!(
            db.vertex(first).unwrap().is_some(),
            "refused count admission must reserve no ID"
        );
        assert_eq!(txn.outstanding_obligations(), 0);
    });
}
