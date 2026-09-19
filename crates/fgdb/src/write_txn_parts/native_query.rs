//! Native text and reusable native plans over the existing transaction reader.
//!
//! Classification is shared with Database::query; execution never calls that
//! method, since doing so would silently drop the caller's staged overlay and
//! transaction observations. No parser, executor or snapshot is duplicated.

use super::WriteTxn;
use crate::Database;
use crate::query::{PreparedNativeRead, QueryError, QueryResult, aggregates, values};
use asupersync::fs::Vfs;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolResolver};
use fgdb_types::QueryCx;

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
                let query = prepared
                    .bind_parameters(params)
                    .map_err(QueryError::PipelineText)?;
                let result = transaction
                    .execute_graph_aggregate_governed(database, cx, &query, policy)
                    .map_err(|error| QueryError::TransactionAggregate(Box::new(error)))?;
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
