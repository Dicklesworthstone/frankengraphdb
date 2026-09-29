//! Native text over the existing transaction reader and write executors.
//!
//! Classification is shared with Database::query; execution never calls that
//! method, since doing so would silently drop the caller's staged overlay and
//! transaction observations. No parser, executor or snapshot is duplicated.

use super::WriteTxn;
use crate::{Database, WriteTxnError};
use crate::query::{
    PreparedNativeRead, QueryError, QueryResult, QueryWriteError, aggregates, values,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::RelationId;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphMutationProgramError, GraphSymbol, GraphSymbolKind,
    GraphSymbolResolver, GraphWriteProgramError, GraphWriteProgramPolicy,
    GraphWriteScriptExecutionError,
};
use fgdb_types::{CommitCx, QueryCx, TxnCx};

impl WriteTxn {
    /// Execute a native read over this transaction's pinned basis and canonical
    /// staged effects. Pattern, aggregate, WITH-aggregate and set/pipeline reads
    /// use the same classification and result columns as [`Database::query`].
    ///
    /// Ownership and lifecycle are checked before invoking the symbol resolver.
    /// Binding/execution errors never try another facade or a live reader. Each
    /// read uses the supplied policy and the existing transaction observations,
    /// including observations from filtered/empty answers and late refusals.
    /// Refusal does not finish the transaction or discard its staged effects.
    ///
    /// This does not stage a write, commit, finish, retry or advance the basis.
    /// Historical selectors and EXPLAIN are not part of this bounded facade;
    /// temporal plans return [`QueryError::TemporalTransactionUnsupported`]. No
    /// durable snapshot certificate is issued for an uncommitted overlay.
    pub fn query<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        self.ensure_database(database)
            .map_err(|error| QueryError::Transaction(Box::new(error)))?;
        let prepared = PreparedNativeRead::prepare(text, params, resolver)?;
        prepared.execute_in_transaction(self, database, cx, params, policy)
    }
}

impl PreparedNativeRead {
    /// Bind this reusable native template to one transaction's existing reader.
    /// Parameter values may change between executions; classification and symbol
    /// resolution do not run again. Staged effects and read witnesses remain
    /// owned by `transaction`, including after a budget or source refusal.
    ///
    /// The template itself owns no transaction, snapshot pin or permission. Each
    /// call validates the supplied transaction's lifecycle and database owner
    /// before binding. A temporal template fails explicitly rather than reading
    /// either the live database or an invented historical overlay.
    pub fn execute_in_transaction<V: Vfs + Clone>(
        &self,
        transaction: &WriteTxn,
        database: &Database<V>,
        cx: &QueryCx,
        params: &GqlParameters,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        transaction
            .ensure_database(database)
            .map_err(|error| QueryError::Transaction(Box::new(error)))?;
        match self {
            Self::Pattern(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = transaction
                    .execute_graph_pattern_governed(database, cx, &query, policy)
                    .map_err(|error| QueryError::TransactionPattern(Box::new(error)))?;
                Ok(values(query.columns().to_vec(), result.value))
            }
            Self::Aggregate(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PatternText)?;
                let result = transaction
                    .execute_graph_aggregate_governed(database, cx, &query, policy)
                    .map_err(|error| QueryError::TransactionAggregate(Box::new(error)))?;
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::PipelineAggregate(prepared) => {
                // Keep every graph leaf on this transaction's pinned overlay.
                // Zero-source plans still validate ownership and lifecycle.
                let result = if prepared.requires_relational_input() {
                    let query = prepared
                        .bind_relation_parameters(params)
                        .map_err(QueryError::PipelineText)?;
                    transaction
                        .execute_graph_set_aggregate_governed(database, cx, &query, policy)
                        .map_err(|error| QueryError::TransactionAggregate(Box::new(error)))?
                } else {
                    let query = prepared
                        .bind_parameters(params)
                        .map_err(QueryError::PipelineText)?;
                    transaction
                        .execute_graph_aggregate_governed(database, cx, &query, policy)
                        .map_err(|error| QueryError::TransactionAggregate(Box::new(error)))?
                };
                Ok(aggregates(
                    prepared.columns().to_vec(),
                    prepared.output_slots(),
                    result.value,
                ))
            }
            Self::Set(prepared) => {
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::SetText)?;
                let result = transaction
                    .execute_graph_set_governed(database, cx, &query, policy)
                    .map_err(|error| QueryError::TransactionSet(Box::new(error)))?;
                Ok(values(prepared.columns().to_vec(), result.value))
            }
            Self::TemporalPattern(_) | Self::TemporalSet(_) | Self::TemporalAggregate(_) => {
                Err(QueryError::TemporalTransactionUnsupported {
                    facade: self.facade_class(),
                })
            }
        }
    }
}

// Allocator admission is a native program preflight, not a parse failure or a
// successful rollback. Keep its original lifecycle/health/control error typed.
fn write_preflight(error: WriteTxnError) -> QueryWriteError<WriteTxnError> {
    QueryWriteError::Execute(GraphWriteScriptExecutionError::Program(
        GraphWriteProgramError::Program(GraphMutationProgramError::Preflight(error)),
    ))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a native text write using database-owned identity reservations.
    ///
    /// This is [`Database::query_write`] without an external identity allocator:
    /// parsing, parameter binding, statement selection, cumulative quotas and
    /// atomic publication all use that same path exactly once. Scripts retain
    /// their write receipts; CREATE/INSERT RETURN retains its result rows and
    /// returns only after native transaction completion succeeds.
    ///
    /// The allocator is the same one used by the typed engine-governed APIs.
    /// Issued identities are not reclaimed after a refusal or rollback. This
    /// method neither guesses IDs from graph contents nor restarts a row-local
    /// counter for each statement. Name resolution and purpose contexts remain
    /// caller supplied; this convenience method grants no additional authority.
    /// Allocator admission failures retain the native program Preflight arm.
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    pub async fn query_write_engine(
        &mut self,
        txcx: &TxnCx,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        policy: GraphWriteProgramPolicy,
    ) -> Result<QueryResult, QueryWriteError<WriteTxnError>> {
        let mut allocate = self.engine_allocator(cx).map_err(write_preflight)?;
        self.query_write(
            txcx,
            cx,
            commit_cx,
            text,
            params,
            resolver,
            relation,
            policy,
            |request| allocate(request.request),
        )
        .await
    }
}

impl WriteTxn {
    /// Stage a native text write with database-owned identity reservations.
    ///
    /// Ownership and lifecycle admission precede allocator acquisition and
    /// symbol resolution. Execution delegates once to [`WriteTxn::query_write`]
    /// over this transaction's existing overlay; it never autocommits or opens
    /// another transaction. RETURN rows remain transaction-local, and receipts
    /// retain completion=None until the caller finishes the outer transaction.
    /// A failed execution preserves the ordinary write API's rollback contract;
    /// IDs already issued by the engine are never reclaimed by that rollback.
    #[allow(clippy::too_many_arguments, clippy::result_large_err)]
    pub fn query_write_engine<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        policy: GraphWriteProgramPolicy,
    ) -> Result<QueryResult, QueryWriteError<WriteTxnError>> {
        self.ensure_database(database).map_err(write_preflight)?;
        let mut allocate = database.engine_allocator(cx).map_err(write_preflight)?;
        self.query_write(
            database,
            cx,
            text,
            params,
            resolver,
            relation,
            policy,
            |request| allocate(request.request),
        )
    }
}

#[cfg(test)]
mod engine_write_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::GraphAggregateValue;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_types::{
        CanonicalScalar, DatabaseSecurityNamespaceId, EmbeddedTxnCompletion, PurposeContexts,
        VId,
    };
    use std::collections::BTreeSet;

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x74; 32],
            DatabaseSecurityNamespaceId([0x75; 32]),
            [0x76; 32],
        )
    }

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(1_000, 100, 5_000_000, 2_000_000),
            1_000,
            100,
            100,
        )
    }

    fn vertex_rows(result: QueryResult) -> Vec<VId> {
        let QueryResult::Rows { rows, .. } = result else {
            panic!("explicit RETURN must produce rows")
        };
        rows.into_iter()
            .flatten()
            .map(|value| match value {
                GraphAggregateValue::Value(GraphValue::Vertex(vertex)) => vertex,
                _ => panic!("expected returned vertex identity"),
            })
            .collect()
    }

    #[test]
    fn engine_text_autocommit_allocates_once_and_survives_checkpoint_reopen() {
        let ((), report) = run_async_under_lab(0xe119_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let before = db.frontier().unwrap();
            let params = GqlParameters::new().with_int64("p", 7).unwrap();
            let mut issued = BTreeSet::new();
            for step in 1..=3 {
                let ids = vertex_rows(
                    db.query_write_engine(
                        &txcx,
                        &cx,
                        &commit,
                        "CREATE (a {p:$p})-[r:R]->(b) RETURN a,b",
                        &params,
                        symbols,
                        R,
                        policy(),
                    )
                    .await
                    .unwrap(),
                );
                assert_eq!(ids.len(), 2);
                for id in ids {
                    assert!(issued.insert(id), "a later call reused an identity");
                }
                assert_eq!(db.frontier().unwrap().0, before.0 + step);
                assert_eq!(db.vertices().unwrap().len(), issued.len());
                assert_eq!(db.edges().unwrap().len() as u64, step);
            }
            let result = db
                .query_write_engine(
                    &txcx,
                    &cx,
                    &commit,
                    "CREATE (n {p:$p}); CREATE (m {p:$p})",
                    &params,
                    symbols,
                    R,
                    policy(),
                )
                .await
                .unwrap();
            let QueryResult::Write { receipt, completion } = result else {
                panic!("a no-RETURN script must preserve its write receipt")
            };
            assert_eq!(receipt.stats().completed_statements, 2);
            assert!(matches!(
                completion,
                Some(EmbeddedTxnCompletion::WriteCommitted { .. })
            ));
            assert_eq!(db.frontier().unwrap().0, before.0 + 4);
            assert_eq!(db.vertices().unwrap().len(), 8);
            db.compact(&commit).await.unwrap();
            drop(db);
            let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            for id in &issued {
                assert!(db.vertex(*id).unwrap().is_some());
            }
            let ids = vertex_rows(
                db.query_write_engine(
                    &txcx,
                    &cx,
                    &commit,
                    "CREATE (a)-[r:R]->(b) RETURN a,b",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(),
                )
                .await
                .unwrap(),
            );
            assert_eq!(ids.len(), 2);
            for id in ids {
                assert!(issued.insert(id), "reopen reused a live identity");
            }
            assert_eq!(db.frontier().unwrap().0, before.0 + 5);
            assert_eq!(db.vertices().unwrap().len(), 10);
            assert_eq!(db.edges().unwrap().len(), 4);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn engine_text_transaction_keeps_overlay_and_defers_publication() {
        let ((), report) = run_async_under_lab(0xe119_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let result = transaction
                .query_write_engine(
                    &mut db,
                    &cx,
                    "CREATE (n {p:$p})",
                    &GqlParameters::new().with_int64("p", 7).unwrap(),
                    symbols,
                    R,
                    policy(),
                )
                .unwrap();
            assert!(matches!(
                result,
                QueryResult::Write {
                    completion: None,
                    ..
                }
            ));
            let copies = vertex_rows(
                transaction
                    .query_write_engine(
                        &mut db,
                        &cx,
                        "MATCH (n) CREATE (copy {p:n.p+1}) RETURN copy",
                        &GqlParameters::new(),
                        symbols,
                        R,
                        policy(),
                    )
                    .unwrap(),
            );
            assert_eq!(
                copies.len(),
                1,
                "the second statement must see the staged source once"
            );
            assert_eq!(transaction.vertices(&db).unwrap().len(), 2);
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            transaction.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert_eq!(
                db.vertex(copies[0]).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(8))]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
