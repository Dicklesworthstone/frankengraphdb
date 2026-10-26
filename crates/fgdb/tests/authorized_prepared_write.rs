//! Public host preparation retains syntax, while each execution binds values
//! and authorizes complete effects through the existing Chronicle write path.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::insertion::{GraphInsertLimitDimension, GraphInsertRequest};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteScriptBatchError, GraphWriteScriptExecutionError,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
};
use fgdb_warden::{
    Authority, Error, Grant, LimitDimension, QueryLimits, Restriction, Rights, Scope,
};

const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x71; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x70; 32], NAMESPACE, [0x72; 32])
}

fn authority() -> Authority {
    Authority::new(
        AuthKey::from_seed(8611),
        NAMESPACE,
        "graph",
        SchemaEpoch(1),
        1,
    )
    .unwrap()
}

fn grant() -> Grant {
    Grant {
        branch: "main".into(),
        labels: Scope::only([LabelId(1)]),
        relations: Scope::only([RelationId(1)]),
        properties: Scope::only([PropertyKeyId(1)]),
        rights: Rights::Write,
        limits: QueryLimits {
            max_nodes: 10_000,
            max_work: 1_000_000,
            max_rows: 0,
        },
        expires_at_ms: 10_000,
    }
}

fn policy() -> GraphWriteProgramPolicy {
    GraphWriteProgramPolicy::new(
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 10_000),
        64,
        64,
        64,
    )
}

fn authorization(error: &(dyn core::error::Error + 'static)) -> Option<Error> {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if let Some(WriteTxnError::Authorization(error)) = error.downcast_ref::<WriteTxnError>() {
            return Some(*error);
        }
        cause = error.source();
    }
    None
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Visible") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "secret") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

fn arguments(value: i64) -> GqlParameters {
    GqlParameters::new().with_int64("value", value).unwrap()
}

#[test]
fn host_prepared_write_discards_representative_values_and_rechecks_effect_authority() {
    let ((), report) = run_async_under_lab(0xaa61, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issuer = authority();
        let grant = grant();
        let token = issuer.issue_at(&grant, 100).unwrap();
        let policy = policy();
        let representative = arguments(99);
        let prepared = db
            .prepare_graph_write_script_authorized(
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                "CREATE (:Visible {p:$value})",
                &representative,
                symbols,
                RelationId(1),
                64,
                || 100,
            )
            .unwrap();
        drop(representative);
        let before = db.frontier().unwrap();
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(
            txn.outstanding_obligations(),
            0,
            "preparation owns no transaction pin"
        );
        assert!(
            db.execute_graph_write_script_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &GqlParameters::new(),
                policy,
                || 100,
            )
            .await
            .is_err(),
            "missing arguments cannot use representative values"
        );
        assert_eq!(db.frontier().unwrap(), before);
        for (offset, value) in [(1, 11), (2, 33)] {
            let (stats, completion) = db
                .execute_graph_write_script_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &prepared,
                    &arguments(value),
                    policy,
                    || 100,
                )
                .await
                .unwrap();
            assert_eq!(stats.completed_statements, 1);
            assert_eq!(stats.created_vertices, 1);
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: fgdb_types::CommitSeq(before.0 + offset),
                }
            );
        }
        let rows = db.vertices().unwrap();
        assert_eq!(rows.len(), 2);
        for value in [11, 33] {
            assert!(rows.iter().any(|row| {
                row.props
                    .contains(&(PropertyKeyId(1), CanonicalScalar::Int(value).into()))
            }));
        }
        let frontier = db.frontier().unwrap();
        // The trusted host supplies catalog bindings. Template admission does
        // not assert that every attempted field has passed effect admission.
        let forbidden = db
            .prepare_graph_write_script_authorized(
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                "CREATE (:Visible {secret:$value})",
                &arguments(88),
                symbols,
                RelationId(1),
                64,
                || 100,
            )
            .unwrap();
        let error = db
            .execute_graph_write_script_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &forbidden,
                &arguments(44),
                policy,
                || 100,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::ScopeDenied));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), rows);
        let read_token = issuer
            .issue_at(
                &Grant {
                    rights: Rights::Read,
                    ..grant
                },
                100,
            )
            .unwrap();
        let error = db
            .execute_graph_write_script_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &read_token,
                "main",
                &prepared,
                &arguments(55),
                policy,
                || 100,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::PermissionDenied));
        assert!(issuer.retire());
        assert!(
            db.execute_graph_write_script_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &arguments(66),
                policy,
                || 100,
            )
            .await
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), rows);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_prepared_batch_admits_all_arguments_and_cumulative_limits_before_publication() {
    let ((), report) = run_async_under_lab(0xaa62, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let prepared = db
            .prepare_graph_write_script_authorized(
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                "CREATE (:Visible {p:$value}); CREATE (:Visible {p:$value + 1})",
                &arguments(99),
                symbols,
                RelationId(1),
                2,
                || 100,
            )
            .unwrap();
        let before = db.frontier().unwrap();
        let invalid_tail = [arguments(11), GqlParameters::new()];

        // A cached template grants no authority, even to inspect malformed
        // arguments or an over-limit expansion on a subsequent request.
        let read_token = issuer
            .issue_at(
                &Grant {
                    rights: Rights::Read,
                    ..grant()
                },
                100,
            )
            .unwrap();
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &read_token,
                "main",
                &prepared,
                &invalid_tail,
                0,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::PermissionDenied));

        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &[],
                4,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteScriptExecutionError::BatchBinding(GraphWriteScriptBatchError::Empty)
        ));

        // Two input records each contain two statements. Admission counts all
        // four before attempting to bind the deliberately malformed last map.
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &invalid_tail,
                3,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteScriptExecutionError::BatchBinding(
                GraphWriteScriptBatchError::TooManyStatements {
                    limit: 3,
                    observed: 4
                }
            )
        ));
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &invalid_tail,
                4,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteScriptExecutionError::BatchBinding(GraphWriteScriptBatchError::Arguments {
                argument_set: 1,
                ..
            })
        ));

        // Binding alone reserves four statement slots, two record checkpoints
        // and final acceptance on ONE signed permit. The last checkpoint must
        // refuse a six-unit token before a transaction or identity can exist.
        let limited = token.attenuate(Restriction::MaxWork(6)).unwrap();
        let values = [arguments(11), arguments(21)];
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &limited,
                "main",
                &prepared,
                &values,
                4,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert_eq!(
            authorization(&error),
            Some(Error::LimitExceeded(LimitDimension::Work))
        );
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);
        let mut untouched = Database::open_memory(&commit, keys()).await.unwrap();
        let request = GraphInsertRequest::Vertex { row: 0, vertex: 0 };
        assert_eq!(
            db.allocate_identity(&query, request).unwrap(),
            untouched.allocate_identity(&query, request).unwrap(),
            "no refused batch may reserve a prefix identity while binding"
        );

        // Execution also has one creation allowance for the entire batch. Its
        // final refusal rolls back the three successfully staged creations.
        let mut limited_policy = policy();
        limited_policy.max_created_vertices = 3;
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &values,
                4,
                limited_policy,
                || 100,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GraphWriteScriptExecutionError::BatchProgram {
                location: Some(ref location),
                source: GraphWriteProgramError::CreationBudget {
                    statement: 3,
                    dimension: GraphInsertLimitDimension::Vertices,
                    limit: 3,
                    observed: 4,
                },
            } if location.argument_set == 1 && location.statement == 1
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);

        // The same host-owned syntax can execute again at the new frontier.
        // Write-only and max_rows=0 remain sufficient for every stats receipt.
        for (offset, start) in [(1, 11), (2, 31)] {
            let (stats, completion) = db
                .execute_graph_write_script_batch_authorized(
                    &txn,
                    &query,
                    &commit,
                    &issuer,
                    &token,
                    "main",
                    &prepared,
                    &[arguments(start), arguments(start + 10)],
                    4,
                    policy(),
                    || 100,
                )
                .await
                .unwrap();
            assert_eq!((stats.completed_statements, stats.created_vertices), (4, 4));
            assert_eq!(
                completion,
                EmbeddedTxnCompletion::WriteCommitted {
                    commit_seq: fgdb_types::CommitSeq(before.0 + offset),
                }
            );
        }
        let rows = db.vertices().unwrap();
        assert_eq!(rows.len(), 8);
        for value in [11, 12, 21, 22, 31, 32, 41, 42] {
            assert!(rows.iter().any(|row| {
                row.props
                    .contains(&(PropertyKeyId(1), CanonicalScalar::Int(value).into()))
            }));
        }
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn host_prepared_batch_rolls_back_a_runtime_tail_and_reauthorizes_reuse() {
    let ((), report) = run_async_under_lab(0xaa63, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, query, txn) = (contexts.commit(), contexts.query(), contexts.txn());
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let values = |value, divisor| arguments(value).with_int64("divisor", divisor).unwrap();
        let prepared = db
            .prepare_graph_write_script_authorized(
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                "CREATE (:Visible {p:$value}); CREATE (:Visible {p:100/$divisor})",
                &values(99, 1),
                symbols,
                RelationId(1),
                2,
                || 100,
            )
            .unwrap();
        let before = db.frontier().unwrap();
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &[values(11, 2), values(33, 0)],
                4,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        let GraphWriteScriptExecutionError::BatchProgram {
            location: Some(location),
            source,
        } = error
        else {
            panic!("runtime failure must preserve the original record and statement")
        };
        assert_eq!((location.argument_set, location.statement), (1, 1));
        assert_eq!(location.span, prepared.statement_span(1).unwrap());
        assert!(matches!(
            source,
            GraphWriteProgramError::Insert { statement: 3, .. }
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        assert_eq!(txn.outstanding_obligations(), 0);

        let (stats, completion) = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &[values(11, 2), values(33, 4)],
                4,
                policy(),
                || 100,
            )
            .await
            .unwrap();
        assert_eq!((stats.completed_statements, stats.created_vertices), (4, 4));
        assert_eq!(
            completion,
            EmbeddedTxnCompletion::WriteCommitted {
                commit_seq: fgdb_types::CommitSeq(before.0 + 1),
            }
        );
        let rows = db.vertices().unwrap();
        assert_eq!(rows.len(), 4);
        for value in [11, 50, 33, 25] {
            assert!(rows.iter().any(|row| {
                row.props
                    .contains(&(PropertyKeyId(1), CanonicalScalar::Int(value).into()))
            }));
        }
        let frontier = db.frontier().unwrap();
        assert!(issuer.retire());
        let error = db
            .execute_graph_write_script_batch_authorized(
                &txn,
                &query,
                &commit,
                &issuer,
                &token,
                "main",
                &prepared,
                &[GqlParameters::new()],
                4,
                policy(),
                || 100,
            )
            .await
            .unwrap_err();
        assert_eq!(authorization(&error), Some(Error::AuthorityRetired));
        assert_eq!(db.frontier().unwrap(), frontier);
        assert_eq!(db.vertices().unwrap(), rows);
        assert_eq!(txn.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
