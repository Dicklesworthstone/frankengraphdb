//! Public host preparation retains syntax, while each execution binds values
//! and authorizes complete effects through the existing Chronicle write path.

use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb::{Database, DatabaseKeys, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, SchemaEpoch};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, GraphWriteProgramPolicy,
};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
};
use fgdb_warden::{Authority, Error, Grant, QueryLimits, Rights, Scope};

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
        let namespace = DatabaseSecurityNamespaceId([0x71; 32]);
        let keys = DatabaseKeys::new([0x70; 32], namespace, [0x72; 32]);
        let mut db = Database::open_memory(&commit, keys).await.unwrap();
        let issuer = Authority::new(
            AuthKey::from_seed(8611),
            namespace,
            "graph",
            SchemaEpoch(1),
            1,
        )
        .unwrap();
        let grant = Grant {
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
        };
        let token = issuer.issue_at(&grant, 100).unwrap();
        let policy = GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 10_000),
            64,
            64,
            64,
        );
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
