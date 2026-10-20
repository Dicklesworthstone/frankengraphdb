// Query-fed writes use the same engine identity source and begin/finish
// protocol as ordinary native writes. No row owns a separate transaction.

type QueryWriteAutocommitResult<A> = Result<
    (
        fgdb_gql::query_write::GraphQueryWriteExecution,
        EmbeddedTxnCompletion,
    ),
    fgdb_gql::query_write::GraphQueryWriteExecutionError<
        WriteTxnError,
        A,
        Box<asupersync::error::Error>,
    >,
>;

impl WriteTxn {
    /// Execute a query-fed script with identities issued by the owning
    /// database. Allocation uses the ordinary engine counter/permutation and
    /// stays spent after failure, just as for every other engine-owned write.
    #[allow(clippy::result_large_err)]
    pub fn execute_graph_query_write_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::query_write::PreparedGraphQueryWrite,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        max_statements: usize,
    ) -> Result<
        fgdb_gql::query_write::GraphQueryWriteExecution,
        fgdb_gql::query_write::GraphQueryWriteExecutionError<
            WriteTxnError,
            WriteTxnError,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::query_write::GraphQueryWriteExecutionError as Error;
        // A foreign transaction must not obtain another database's allocator.
        self.ensure_database(database).map_err(Error::Preflight)?;
        let mut allocate = database.engine_allocator(cx).map_err(Error::Preflight)?;
        self.execute_graph_query_write_governed(
            database,
            cx,
            query,
            policy,
            max_statements,
            move |request| allocate(request.request),
        )
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Read the source, bind all rows and stage all script instances in one
    /// private transaction, then finish exactly once. Empty source input closes
    /// read-only and creates no marker commit. A staging failure explicitly
    /// aborts; a finish failure preserves its native durability classification.
    /// No successful prefix or transaction-local receipt escapes an error.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_query_write_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::query_write::PreparedGraphQueryWrite,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        max_statements: usize,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> QueryWriteAutocommitResult<A> {
        use fgdb_gql::query_write::GraphQueryWriteExecutionError as Error;
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| Error::Preflight(WriteTxnError::Write(error)))?;
        let execution = match transaction.execute_graph_query_write_governed(
            self,
            query_cx,
            query,
            policy,
            max_statements,
            allocate,
        ) {
            Ok(execution) => execution,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(Error::Completion)?;
        Ok((execution, completion))
    }

    /// The autocommit path with database-owned identities. This is the same
    /// transaction lifecycle and cumulative execution policy as the explicit
    /// allocator path, not a separate bulk writer or a per-row retry loop.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_query_write_autocommit_engine_governed(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        query: &fgdb_gql::query_write::PreparedGraphQueryWrite,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        max_statements: usize,
    ) -> QueryWriteAutocommitResult<WriteTxnError> {
        use fgdb_gql::query_write::GraphQueryWriteExecutionError as Error;
        let mut allocate = self.engine_allocator(query_cx).map_err(Error::Preflight)?;
        self.execute_graph_query_write_autocommit_governed(
            txcx,
            query_cx,
            commit_cx,
            query,
            policy,
            max_statements,
            move |request| allocate(request.request),
        )
        .await
    }
}

#[cfg(test)]
mod query_write_autocommit_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::query_write::{GraphQueryWriteExecutionError, PreparedGraphQueryWrite};
    use fgdb_gql::{
        GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
        GraphWriteProgramPolicy, PreparedGraphSet, PreparedGraphSetText, PreparedGraphWriteScript,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "City") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "seen") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    fn read(text: &str) -> PreparedGraphSet {
        PreparedGraphSetText::prepare(text, resolve)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }

    fn query(source: &str, script: &str) -> PreparedGraphQueryWrite {
        PreparedGraphQueryWrite::prepare(
            read(source),
            PreparedGraphWriteScript::prepare(script, RelationId(1), resolve).unwrap(),
        )
        .unwrap()
    }

    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000),
            10_000,
            10_000,
            10_000,
        )
    }

    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new(
            [0x81; 32],
            DatabaseSecurityNamespaceId([0x82; 32]),
            [0x83; 32],
        )
    }

    #[test]
    fn engine_autocommit_merges_duplicate_keys_and_finishes_only_once() {
        let ((), report) = run_async_under_lab(0x81c1, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let prepared = query(
                "UNWIND [1,2,1] AS key RETURN key",
                "MERGE (c:City {name:$key}) ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1",
            );
            let (execution, _) = db
                .execute_graph_query_write_autocommit_engine_governed(
                    &txcx, &cx, &commit, &prepared, policy(), 3,
                )
                .await
                .unwrap();
            assert_eq!(execution.input_rows, 3);
            assert_eq!(execution.stats.created_vertices, 2);
            assert_eq!(execution.receipt.as_ref().unwrap().steps().len(), 3);
            assert_eq!(db.frontier().unwrap(), CommitSeq(before.0 + 1));
            let rows = db
                .execute_graph_set_governed(
                    &cx,
                    &read("MATCH (c:City) RETURN c.name AS name, c.seen AS seen ORDER BY name"),
                    policy().mutations.query,
                )
                .unwrap()
                .value;
            assert_eq!(rows.len(), 2);
            for (row, expected) in rows.iter().zip([(1, 2), (2, 1)]) {
                assert_eq!(row.values(), &[
                    GraphValue::Scalar(CanonicalScalar::Int(expected.0)),
                    GraphValue::Scalar(CanonicalScalar::Int(expected.1)),
                ]);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn late_action_failure_aborts_every_new_vertex_without_advancing_frontier() {
        let ((), report) = run_async_under_lab(0x81c2, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            // The first occurrence creates a vertex. The second hits that
            // same key, then fails only during its ON MATCH action.
            let prepared = query(
                "UNWIND [2,0] AS denominator RETURN 1 AS key, denominator ORDER BY denominator DESC",
                "MERGE (c:City {name:$key}) ON CREATE SET c.seen=10 ON MATCH SET c.seen=c.seen / $denominator",
            );
            let error = db
                .execute_graph_query_write_autocommit_engine_governed(
                    &txcx, &cx, &commit, &prepared, policy(), 2,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, GraphQueryWriteExecutionError::Program(_)));
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.execute_graph_set_governed(
                &cx, &read("MATCH (c:City) RETURN c"), policy().mutations.query,
            ).unwrap().value.is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
            // The aborted operation does not poison later engine allocation
            // or strand a transaction obligation.
            let retry = query("RETURN 1 AS key", "MERGE (c:City {name:$key})");
            db.execute_graph_query_write_autocommit_engine_governed(
                &txcx, &cx, &commit, &retry, policy(), 1,
            ).await.unwrap();
            assert_eq!(db.frontier().unwrap(), CommitSeq(before.0 + 1));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn empty_source_and_statement_refusal_never_allocate_or_commit() {
        let ((), report) = run_async_under_lab(0x81c3, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let empty = query("UNWIND [] AS key RETURN key", "CREATE (c:City {name:$key})");
            let (execution, completion) = db
                .execute_graph_query_write_autocommit_governed(
                    &txcx, &cx, &commit, &empty, policy(), 0,
                    |_| Err::<ElementId, _>("must not allocate"),
                )
                .await
                .unwrap();
            assert_eq!(execution.input_rows, 0);
            assert!(execution.receipt.is_none());
            assert!(matches!(completion, EmbeddedTxnCompletion::ReadClosed { .. }));
            let too_many = query("UNWIND [1,2] AS key RETURN key", "CREATE (c:City {name:$key})");
            let error = db
                .execute_graph_query_write_autocommit_governed(
                    &txcx, &cx, &commit, &too_many, policy(), 1,
                    |_| Err::<ElementId, _>("must not allocate"),
                )
                .await
                .unwrap_err();
            assert!(matches!(error, GraphQueryWriteExecutionError::Source(
                fgdb_gql::GqlQueryError::Rows(_),
            )));
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn engine_transaction_adapter_rejects_a_foreign_database_before_zero_quota() {
        let ((), report) = run_async_under_lab(0x81c4, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut other = Database::open_memory(&commit, keys()).await.unwrap();
            let prepared = query("RETURN 1 AS key", "CREATE (c:City {name:$key})");
            let mut transaction = db.begin(&txcx).unwrap();
            let error = transaction.execute_graph_query_write_engine_governed(
                &mut other,
                &cx,
                &prepared,
                GraphWriteProgramPolicy::new(GqlQueryPolicy::new(0, 0, 0, 0), 0, 0, 0),
                0,
            ).unwrap_err();
            assert!(matches!(error, GraphQueryWriteExecutionError::Preflight(
                WriteTxnError::WrongDatabase,
            )));
            transaction.abort();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn a_later_allocator_refusal_keeps_no_successful_prefix() {
        let ((), report) = run_async_under_lab(0x81c5, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let prepared = query(
                "UNWIND [1,2] AS key RETURN key",
                "CREATE (c:City {name:$key})",
            );
            let mut calls = 0;
            let error = db.execute_graph_query_write_autocommit_governed(
                &txcx, &cx, &commit, &prepared, policy(), 2, |_| {
                    calls += 1;
                    match calls {
                        1 => Ok(ElementId::Vertex(VId(100))),
                        _ => Err("identity source exhausted"),
                    }
                },
            ).await.unwrap_err();
            assert_eq!(calls, 2);
            assert!(matches!(error, GraphQueryWriteExecutionError::Program(_)));
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.execute_graph_set_governed(
                &cx, &read("MATCH (c:City) RETURN c"), policy().mutations.query,
            ).unwrap().value.is_empty());
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

}
