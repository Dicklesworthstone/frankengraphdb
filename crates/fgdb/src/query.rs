//! Text entrypoints over the existing governed engines. Name resolution remains
//! caller supplied; this module neither invents a catalog nor grants authority.

use crate::{Database, GqlError, WriteTxn, WriteTxnError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, RelationId};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::*;
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};

type Cancel = Box<asupersync::error::Error>;

#[path = "query_aggregate_stream.rs"]
mod aggregate_stream;
#[path = "query_authorized.rs"]
mod authorized;
#[path = "query_beacon.rs"]
mod beacon;
#[path = "query_diff.rs"]
mod diff;
#[path = "query_explain.rs"]
mod explain;
#[path = "query_prism.rs"]
mod prism;
#[path = "query_view.rs"]
mod view;
pub use crate::gql_cert::NativeResultCertificate;
pub use aggregate_stream::NativeAggregateCursor;
pub use authorized::{
    AuthorizedAggregateCursor, AuthorizedBeaconIndex, AuthorizedPreparedFnxCall,
    AuthorizedPreparedRead, AuthorizedReadSession, AuthorizedRowCursor,
};
pub use beacon::procedure::{HYBRID_SEARCH_OUTPUTS, HybridCallError};
pub(crate) use beacon::procedure::{
    HybridSearch, is_hybrid, privileged as hybrid_procedure, refuse as hybrid_refusal,
};
pub use beacon::{PinnedIndex, RefreshReport, ResidentIndex, ResidentIndexError};
pub use explain::{ExplainRow, NativeExplainCertificate, PreparedNativeRead, ReplayRefusal};
use explain::{explain_prefix, explain_result};
pub use prism::ProcedureError;
pub(crate) use prism::{
    bind_procedure, fnx_procedure, procedure_failure, procedure_options, procedure_rows,
    push_staged as push_prism_staged,
};
pub use view::{
    NativeAggregateSpool, NativeAggregateSpoolCursor, NativeAggregateSpoolError,
    NativeAggregateSpoolRow, NativeResultSpool, NativeSpoolCursor, NativeSpoolError,
    PreparedBufferedAggregate, PreparedBufferedOrder,
};

/// Lossless cells: identity/scalar values, counts, wide integer sums and exact
/// averages retain their native domains instead of narrowing to scalar Int.
pub type QueryValue = GraphAggregateValue;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryResult {
    /// Read results or explicit write RETURN rows. Transaction-local write rows
    /// are not a durability acknowledgment; the caller must still finish.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<QueryValue>>,
    },
    /// None means staged in the caller's still-open transaction, not committed.
    Write {
        receipt: GraphWriteProgramReceipt,
        completion: Option<EmbeddedTxnCompletion>,
    },
}

/// Original engine errors remain inspectable, including budget and source
/// refusal. Unsupported includes structural parser diagnostics, never values.
#[derive(Debug)]
pub enum QueryError {
    /// Signature, scope, expiry, retirement, or signed-budget refusal.
    Authorization(fgdb_warden::Error),
    /// A native read could not admit its database generation or revision.
    Read(crate::ReadError),
    Unsupported {
        diagnostics: Vec<String>,
    },
    /// The furthest-progressing native parser, retaining its original error.
    Refused {
        facade: crate::NativeReadClass,
        source: Box<QueryError>,
    },
    PatternText(GraphPatternTextError),
    SetText(GraphSetTextError),
    PipelineText(GraphPipelineAggregateTextError),
    TemporalText(GraphTemporalTextError),
    TemporalSetText(GraphTemporalSetTextError),
    Pattern(GqlQueryError<GqlError, Cancel>),
    Aggregate(GqlQueryError<GraphAggregateError<GqlError>, Cancel>),
    Set(GqlQueryError<GraphSetExecutionError<GqlError>, Cancel>),
    /// Opening a pull query failed in the existing governed scan compiler or
    /// source. Later pull errors remain the cursor's native typed errors.
    Stream(GqlQueryError<fgdb_gql::stream::VertexScanError<crate::ReadError>, Cancel>),
    /// Identified-edge stream preparation/source refusal retains its own typed
    /// error. A failed edge plan is never retried as a vertex or eager query.
    EdgeStream(GqlQueryError<fgdb_gql::edge_stream::EdgeScanError<crate::ReadError>, Cancel>),
    /// Native aggregate preparation cannot discard unsupported operators.
    AggregateStreamPlan(fgdb_gql::stream::aggregate::VertexAggregateBuildError),
    /// Source/frontier/context refusal while opening a vertex aggregate.
    /// Late failures preserve the cause inside the cursor's ScanError sum.
    AggregateStream(fgdb_gql::stream::aggregate::VertexAggregateError<crate::ReadError, Cancel>),
    /// Fixed-edge aggregate compilation failed before source access.
    EdgeAggregateStreamPlan(fgdb_gql::edge_stream::aggregate::EdgeAggregateBuildError),
    /// The selected edge aggregate source/frontier/context refused opening.
    EdgeAggregateStream(
        fgdb_gql::edge_stream::aggregate::EdgeAggregateError<crate::ReadError, Cancel>,
    ),
    /// This native class has no pull specialization. Never collect an eager
    /// result and misrepresent its iterator as a streaming execution.
    StreamingUnsupported {
        facade: crate::NativeReadClass,
    },
    /// Ownership or lifecycle refused before transaction-read preparation.
    Transaction(Box<WriteTxnError>),
    /// Historical selectors have no defined staged-overlay semantics. A
    /// transaction read must not silently fall back to a database read.
    TemporalTransactionUnsupported {
        facade: crate::NativeReadClass,
    },
    TransactionPattern(Box<GqlQueryError<WriteTxnError, Cancel>>),
    TransactionAggregate(Box<GqlQueryError<GraphAggregateError<WriteTxnError>, Cancel>>),
    TransactionSet(Box<GqlQueryError<GraphSetExecutionError<WriteTxnError>, Cancel>>),
}
impl core::fmt::Display for QueryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Authorization(error) => error.fmt(f),
            Self::Read(error) => error.fmt(f),
            Self::Unsupported { diagnostics } => {
                write!(f, "unsupported query construct: {}", diagnostics.join("; "))
            }
            Self::Refused { source, .. } => write!(f, "unsupported query construct: {source}"),
            Self::PatternText(e) => e.fmt(f),
            Self::SetText(e) => e.fmt(f),
            Self::PipelineText(e) => e.fmt(f),
            Self::TemporalText(e) => e.fmt(f),
            Self::TemporalSetText(e) => e.fmt(f),
            Self::Pattern(e) => e.fmt(f),
            Self::Aggregate(e) => e.fmt(f),
            Self::Set(e) => e.fmt(f),
            Self::Stream(e) => e.fmt(f),
            Self::EdgeStream(e) => e.fmt(f),
            Self::AggregateStreamPlan(e) => e.fmt(f),
            Self::AggregateStream(e) => e.fmt(f),
            Self::EdgeAggregateStreamPlan(e) => e.fmt(f),
            Self::EdgeAggregateStream(e) => e.fmt(f),
            Self::StreamingUnsupported { facade } => {
                write!(f, "native {facade:?} read has no supported pull execution")
            }
            Self::Transaction(e) => e.fmt(f),
            Self::TemporalTransactionUnsupported { facade } => write!(
                f,
                "native {facade:?} read cannot select history inside a staged transaction"
            ),
            Self::TransactionPattern(e) => e.fmt(f),
            Self::TransactionAggregate(e) => e.fmt(f),
            Self::TransactionSet(e) => e.fmt(f),
        }
    }
}
impl core::error::Error for QueryError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Authorization(error) => Some(error),
            Self::Read(error) => Some(error),
            Self::Refused { source, .. } => Some(source.as_ref()),
            Self::Stream(error) => Some(error),
            Self::EdgeStream(error) => Some(error),
            Self::AggregateStreamPlan(error) => Some(error),
            Self::AggregateStream(error) => Some(error),
            Self::EdgeAggregateStreamPlan(error) => Some(error),
            Self::EdgeAggregateStream(error) => Some(error),
            Self::Transaction(error) => Some(error.as_ref()),
            Self::TransactionPattern(error) => Some(error.as_ref()),
            Self::TransactionAggregate(error) => Some(error.as_ref()),
            Self::TransactionSet(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum QueryWriteError<A> {
    Prepare(GraphWriteScriptError),
    /// A recognized UNWIND mutation failed whole-input admission/binding.
    UnwindBinding(fgdb_gql::unwind_write::GraphUnwindWriteError),
    Execute(GraphWriteScriptExecutionError<WriteTxnError, A, Cancel>),
    /// CREATE/INSERT RETURN is a single native query, not a no-result script.
    InsertText(GraphInsertTextError),
    Insert(GqlQueryError<GraphInsertQueryError<WriteTxnError, A>, Cancel>),
    /// MATCH-selected SET/REMOVE/DETACH DELETE RETURN retains the mutation
    /// query's preparation and execution errors, never a script fallback.
    MutationText(GraphMutationTextError),
    Mutation(GqlQueryError<GraphMutationQueryError<WriteTxnError>, Cancel>),
    /// Vertex MERGE RETURN includes the selected branch, trailing actions and
    /// post-clause result. Completion errors retain their original meaning.
    VertexUpsertText(GraphVertexUpsertTextError),
    VertexUpsert(GqlQueryError<GraphVertexUpsertError<WriteTxnError, A>, Cancel>),
}
impl<A: core::fmt::Display> core::fmt::Display for QueryWriteError<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Prepare(e) => e.fmt(f),
            Self::UnwindBinding(e) => e.fmt(f),
            Self::Execute(e) => e.fmt(f),
            Self::InsertText(e) => e.fmt(f),
            Self::Insert(e) => e.fmt(f),
            Self::MutationText(e) => e.fmt(f),
            Self::Mutation(e) => e.fmt(f),
            Self::VertexUpsertText(e) => e.fmt(f),
            Self::VertexUpsert(e) => e.fmt(f),
        }
    }
}
impl<A: core::error::Error + 'static> core::error::Error for QueryWriteError<A> {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Prepare(error) => Some(error),
            Self::UnwindBinding(error) => Some(error),
            Self::Execute(error) => Some(error),
            Self::InsertText(error) => Some(error),
            Self::Insert(error) => Some(error),
            Self::MutationText(error) => Some(error),
            Self::Mutation(error) => Some(error),
            Self::VertexUpsertText(error) => Some(error),
            Self::VertexUpsert(error) => Some(error),
        }
    }
}
impl<A> From<NativeGraphWriteBindError> for QueryWriteError<A> {
    fn from(error: NativeGraphWriteBindError) -> Self {
        match error {
            NativeGraphWriteBindError::ScriptPreparation(error) => Self::Prepare(error),
            NativeGraphWriteBindError::ScriptBinding(error) => {
                Self::Execute(GraphWriteScriptExecutionError::Binding(error))
            }
            NativeGraphWriteBindError::Unwind(error) => Self::UnwindBinding(error),
        }
    }
}

// Both public write entrypoints must bind the same native query. The parser,
// not a text rewrite, owns statement framing, scopes and parameter occurrence
// tables. Scalar and collection arguments may appear only in RETURN. Keep
// their declared kinds, while numeric and pagination roles retain inference.
fn write_return_declarations(params: &GqlParameters) -> Vec<(&str, GqlParameterType)> {
    params
        .parameter_types()
        .filter(|(_, kind)| {
            matches!(
                kind,
                GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
            )
        })
        .collect()
}

fn prepare_insert_return(
    text: &str,
    params: &GqlParameters,
    resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    relation: RelationId,
) -> Result<PreparedGraphInsertQuery, GraphInsertTextError> {
    let declarations = write_return_declarations(params);
    PreparedGraphInsertQueryText::prepare_with_parameter_types(
        text,
        relation,
        &declarations,
        resolver,
    )?
    .bind_parameters(params)
}

fn prepare_mutation_return(
    text: &str,
    params: &GqlParameters,
    resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    relation: RelationId,
) -> Result<PreparedGraphMutationQuery, GraphMutationTextError> {
    let declarations = write_return_declarations(params);
    PreparedGraphMutationQueryText::prepare_with_parameter_types(
        text,
        relation,
        &declarations,
        resolver,
    )?
    .bind_parameters(params)
}

fn prepare_vertex_upsert_return(
    text: &str,
    params: &GqlParameters,
    resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    relation: RelationId,
) -> Result<PreparedGraphVertexUpsertQuery, GraphVertexUpsertTextError> {
    let declarations = write_return_declarations(params);
    PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(
        text,
        relation,
        &declarations,
        resolver,
    )?
    .bind_parameters(params)
}

pub(crate) fn values(columns: Vec<String>, rows: Vec<GraphValueRow>) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(GraphAggregateValue::Value)
                    .collect()
            })
            .collect(),
    }
}
pub(crate) fn aggregates(
    columns: Vec<String>,
    slots: &[GraphAggregateTextSlot],
    rows: Vec<GraphAggregateRow>,
) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                slots
                    .iter()
                    .map(|slot| match *slot {
                        GraphAggregateTextSlot::GroupKey(i) => {
                            GraphAggregateValue::Value(row.keys()[i].clone())
                        }
                        GraphAggregateTextSlot::Aggregate(i) => row.values()[i].clone(),
                    })
                    .collect()
            })
            .collect(),
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a read using the native parsers as the classification authority.
    /// More specific grammars precede their general forms. A successful prepare
    /// commits the classification: binding/execution failures never try another
    /// engine. Resolution (including misses) is frozen across grammar probes.
    pub fn query(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        budget: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        if let Some(explain) = explain_prefix(text) {
            let (statement, certificate) = explain?;
            let (listing, certificate) = self.explain(statement, params, resolver, certificate)?;
            return explain_result(listing, certificate);
        }
        PreparedNativeRead::prepare(text, params, resolver)?.execute(self, cx, params, budget)
    }

    /// Prepare a supported native aggregate without collecting its input rows.
    /// Grouped vertex reads expose keys and aggregates through output_slots().
    /// Reuse PreparedNativeRead::stream_aggregate for repeated parameter binding.
    /// Unsupported physical shapes refuse; execution never retries eagerly.
    pub fn query_aggregate_stream<'q>(
        &self,
        cx: &'q QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        policy: GqlQueryPolicy,
    ) -> Result<NativeAggregateCursor<'q>, QueryError> {
        PreparedNativeRead::prepare(text, params, resolver)?
            .stream_aggregate(self, cx, params, policy)
    }

    /// Autocommit counterpart. Purpose contexts, relation coordinate and identity
    /// allocator remain explicit, exactly as in the existing native script API.
    /// Ordinary scripts and expanded UNWIND batches share one work budget.
    /// CREATE/INSERT, MATCH-selected mutation and vertex MERGE RETURN produce
    /// Rows only after native transaction finish succeeds. Writes without RETURN
    /// retain their Write receipt. Statement-specific native classifiers own
    /// dispatch; comments, literals and names are never substring-scanned here.
    /// A RETURN failure is never retried as a script or as a read. Multi-statement RETURN
    /// scripts are refused rather than executing a prefix and dropping rows.
    /// One bounded UNWIND MERGE/MATCH mutation binds every map row before the
    /// ordinary atomic executor starts. Its rows share the whole program budget
    /// and its errors retain input-record coordinates; no per-row commit occurs.
    #[allow(clippy::too_many_arguments)]
    // Returns once per statement/script and wraps the script execution error,
    // whose record location plus program error is deliberate (write_scripts).
    #[allow(clippy::result_large_err)]
    pub async fn query_write<A>(
        &mut self,
        txcx: &TxnCx,
        cx: &QueryCx,
        commit_cx: &CommitCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        budget: GraphWriteProgramPolicy,
        mut allocate: impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<QueryResult, QueryWriteError<A>> {
        if PreparedGraphInsertQueryText::has_return_clause(text)
            .map_err(QueryWriteError::InsertText)?
        {
            let query = prepare_insert_return(text, params, resolver, relation)
                .map_err(QueryWriteError::InsertText)?;
            let columns = query.columns().to_vec();
            let (_, rows, _) = self
                .execute_graph_insert_query_autocommit_governed(
                    txcx,
                    cx,
                    commit_cx,
                    &query,
                    budget.insertion_policy(),
                    |request| {
                        allocate(GraphWriteIdentityRequest {
                            statement: 0,
                            request,
                        })
                    },
                )
                .await
                .map_err(QueryWriteError::Insert)?;
            return Ok(values(columns, rows.value));
        }
        if PreparedGraphMutationQueryText::has_return_clause(text)
            .map_err(QueryWriteError::MutationText)?
        {
            let query = prepare_mutation_return(text, params, resolver, relation)
                .map_err(QueryWriteError::MutationText)?;
            let columns = query.columns().to_vec();
            let (_, rows, _) = self
                .execute_graph_mutation_query_autocommit_governed(
                    txcx,
                    cx,
                    commit_cx,
                    &query,
                    budget.mutations,
                )
                .await
                .map_err(QueryWriteError::Mutation)?;
            return Ok(values(columns, rows.value));
        }
        if PreparedGraphVertexUpsertQueryText::has_return_clause(text)
            .map_err(QueryWriteError::VertexUpsertText)?
        {
            let query = prepare_vertex_upsert_return(text, params, resolver, relation)
                .map_err(QueryWriteError::VertexUpsertText)?;
            let columns = query.columns().to_vec();
            let (_, _, rows, _) = self
                .execute_graph_vertex_upsert_query_autocommit_governed(
                    txcx,
                    cx,
                    commit_cx,
                    &query,
                    budget.vertex_upsert_policy(),
                    |request| {
                        allocate(GraphWriteIdentityRequest {
                            statement: 0,
                            request,
                        })
                    },
                )
                .await
                .map_err(QueryWriteError::VertexUpsert)?;
            return Ok(values(columns, rows.value));
        }
        let bound = BoundNativeGraphWrite::bind(text, params, relation, resolver)
            .map_err(QueryWriteError::from)?;
        let (receipt, completion) = self
            .execute_graph_write_program_returning_autocommit_governed(
                txcx,
                cx,
                commit_cx,
                bound.program(),
                budget,
                allocate,
            )
            .await
            .map_err(|error| QueryWriteError::Execute(bound.execution_error(error)))?;
        Ok(QueryResult::Write {
            receipt,
            completion: Some(completion),
        })
    }
}

impl WriteTxn {
    /// Stage a native statement/script atomically inside this transaction.
    /// The caller alone decides when to finish the outer transaction. Explicit
    /// CREATE/INSERT, MATCH-selected mutation and vertex MERGE RETURN produce
    /// transaction-local Rows, not a durability acknowledgment. Native query
    /// engines admit the complete result before accepting the statement;
    /// errors preserve earlier staged effects and their read dependencies.
    /// Result pagination never limits effects or hides a failing expression.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-statement report, as above
    pub fn query_write<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
        relation: RelationId,
        budget: GraphWriteProgramPolicy,
        mut allocate: impl FnMut(GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<QueryResult, QueryWriteError<A>> {
        if PreparedGraphInsertQueryText::has_return_clause(text)
            .map_err(QueryWriteError::InsertText)?
        {
            let query = prepare_insert_return(text, params, resolver, relation)
                .map_err(QueryWriteError::InsertText)?;
            let columns = query.columns().to_vec();
            let (_, rows) = self
                .execute_graph_insert_query_governed(
                    database,
                    cx,
                    &query,
                    budget.insertion_policy(),
                    |request| {
                        allocate(GraphWriteIdentityRequest {
                            statement: 0,
                            request,
                        })
                    },
                )
                .map_err(QueryWriteError::Insert)?;
            return Ok(values(columns, rows.value));
        }
        if PreparedGraphMutationQueryText::has_return_clause(text)
            .map_err(QueryWriteError::MutationText)?
        {
            let query = prepare_mutation_return(text, params, resolver, relation)
                .map_err(QueryWriteError::MutationText)?;
            let columns = query.columns().to_vec();
            let (_, rows) = self
                .execute_graph_mutation_query_governed(database, cx, &query, budget.mutations)
                .map_err(QueryWriteError::Mutation)?;
            return Ok(values(columns, rows.value));
        }
        if PreparedGraphVertexUpsertQueryText::has_return_clause(text)
            .map_err(QueryWriteError::VertexUpsertText)?
        {
            let query = prepare_vertex_upsert_return(text, params, resolver, relation)
                .map_err(QueryWriteError::VertexUpsertText)?;
            let columns = query.columns().to_vec();
            let (_, _, rows) = self
                .execute_graph_vertex_upsert_query_governed(
                    database,
                    cx,
                    &query,
                    budget.vertex_upsert_policy(),
                    |request| {
                        allocate(GraphWriteIdentityRequest {
                            statement: 0,
                            request,
                        })
                    },
                )
                .map_err(QueryWriteError::VertexUpsert)?;
            return Ok(values(columns, rows.value));
        }
        let bound = BoundNativeGraphWrite::bind(text, params, relation, resolver)
            .map_err(QueryWriteError::from)?;
        let receipt = self
            .execute_graph_write_program_returning_governed(
                database,
                cx,
                bound.program(),
                budget,
                allocate,
            )
            .map_err(|error| QueryWriteError::Execute(bound.execution_error(error)))?;
        Ok(QueryResult::Write {
            receipt,
            completion: None,
        })
    }
}

#[cfg(test)]
mod write_return_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs, WriteBatch};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::insertion::GraphInsertRequest;
    use fgdb_types::context::SimulationCheckpointProbe;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
    use std::sync::Arc;
    // Atomics, not Cell: the counters live inside lab futures, which must be Send.
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

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
            (GraphSymbolKind::Label, "Copy") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    }

    fn policy(rows: u64, vertices: u64, edges: u64) -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(
            GqlQueryPolicy::new(1_000, rows, 5_000_000, 2_000_000),
            1_000,
            vertices,
            edges,
        )
    }

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn rows(result: QueryResult, expected_columns: &[&str]) -> Vec<Vec<GraphValue>> {
        let QueryResult::Rows { columns, rows } = result else {
            panic!("an explicit RETURN must not be replaced by a write receipt")
        };
        assert_eq!(
            columns,
            expected_columns
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<Vec<_>>()
        );
        rows.into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| match value {
                        GraphAggregateValue::Value(value) => value,
                        _ => panic!("CREATE RETURN preserves native graph values"),
                    })
                    .collect()
            })
            .collect()
    }

    fn allocate(request: GraphWriteIdentityRequest) -> Result<ElementId, ()> {
        assert_eq!(request.statement, 0);
        Ok(match request.request {
            GraphInsertRequest::Vertex { row, vertex } => {
                ElementId::Vertex(VId(100 + row as u128 * 10 + vertex as u128))
            }
            GraphInsertRequest::Edge { row, edge } => {
                ElementId::Edge(EId(1_000 + row as u128 * 10 + edge as u128))
            }
        })
    }

    #[test]
    fn public_write_return_preserves_identified_match_edges_and_durable_results() {
        let ((), report) = run_async_under_lab(0xc8e7_0011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(3))]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![(P, CanonicalScalar::Int(7))]);
            seed.add_edge(EId(11), VId(1), VId(2), vec![(P, CanonicalScalar::Int(9))]);
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    "MATCH (a)-[r:R]->(b)
                     CREATE (copy:Copy {p:r.p+a.p}),(a)-[e:R {p:r.p}]->(copy)
                     RETURN a,r,copy,e,r.p AS original,copy.p AS copied ORDER BY original",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(2, 2, 2),
                    allocate,
                )
                .await
                .unwrap();
            assert_eq!(
                rows(result, &["a", "r", "copy", "e", "original", "copied"]),
                vec![
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Edge(EId(10)),
                        GraphValue::Vertex(VId(100)),
                        GraphValue::Edge(EId(1_000)),
                        int(7),
                        int(10),
                    ],
                    vec![
                        GraphValue::Vertex(VId(1)),
                        GraphValue::Edge(EId(11)),
                        GraphValue::Vertex(VId(110)),
                        GraphValue::Edge(EId(1_010)),
                        int(9),
                        int(12),
                    ],
                ]
            );
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            db.compact(&commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.vertices().unwrap().len(), 4);
            assert_eq!(db.edges().unwrap().len(), 4);
            for (vertex, edge, value) in [(100, 1_000, 10), (110, 1_010, 12)] {
                assert_eq!(
                    db.vertex(VId(vertex)).unwrap().unwrap().props,
                    vec![(P, CanonicalScalar::Int(value))]
                );
                let edge = db.edge(EId(edge)).unwrap().unwrap();
                assert_eq!((edge.entry.src, edge.entry.dst), (VId(1), VId(vertex)));
                assert_eq!(edge.props, vec![(P, CanonicalScalar::Int(value - 3))]);
            }
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn public_transaction_return_reads_staged_sources_and_keeps_prefix_after_failure() {
        let ((), report) = run_async_under_lab(0xc8e7_0012, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let mut transaction = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(R);
            prefix.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(2))]);
            prefix.create_vertex(VId(2), vec![], vec![(P, CanonicalScalar::Int(0))]);
            transaction.write(&mut db, prefix).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let next = AtomicU64::new(100);
            let mut allocate = |request: GraphWriteIdentityRequest| -> Result<ElementId, ()> {
                assert_eq!(request.statement, 0);
                assert!(matches!(request.request, GraphInsertRequest::Vertex { .. }));
                let vertex = u128::from(next.fetch_add(1, Ordering::Relaxed));
                Ok(ElementId::Vertex(VId(vertex)))
            };
            let result = transaction.query_write(
                &mut db,
                &cx,
                "MATCH (n) CREATE (copy {p:n.p}) RETURN 10/copy.p AS quotient",
                &GqlParameters::new(),
                symbols,
                R,
                policy(2, 2, 0),
                &mut allocate,
            );
            assert!(matches!(
                result,
                Err(QueryWriteError::Insert(GqlQueryError::Source(
                    GraphInsertQueryError::Returning(_)
                )))
            ));
            assert_eq!(
                next.load(Ordering::Relaxed),
                102,
                "issued identities are not rolled back"
            );
            assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
            assert_eq!(transaction.vertices(&db).unwrap().len(), 2);
            assert!(transaction.vertex(&db, VId(100)).unwrap().is_none());
            assert!(transaction.vertex(&db, VId(101)).unwrap().is_none());
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            let arguments = GqlParameters::new()
                .with_int64("step", 3)
                .unwrap()
                .with_list("tail", vec![int(7)])
                .unwrap();
            let result = transaction
                .query_write(
                    &mut db,
                    &cx,
                    "MATCH (n) WHERE n.p > 0 CREATE (copy {p:n.p+$step})
                     RETURN n,copy,copy.p AS p,$tail AS tail ORDER BY n",
                    &arguments,
                    symbols,
                    R,
                    policy(1, 1, 0),
                    &mut allocate,
                )
                .unwrap();
            assert_eq!(
                rows(result, &["n", "copy", "p", "tail"]),
                vec![vec![
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(102)),
                    int(5),
                    GraphValue::List(vec![int(7)].into_boxed_slice()),
                ]]
            );
            assert!(db.vertex(VId(102)).unwrap().is_none());
            transaction.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(db.vertices().unwrap().len(), 3);
            assert!(db.vertex(VId(100)).unwrap().is_none());
            assert!(db.vertex(VId(101)).unwrap().is_none());
            assert_eq!(
                db.vertex(VId(102)).unwrap().unwrap().props,
                vec![(P, CanonicalScalar::Int(5))]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn public_return_refusals_budgets_and_cancellation_never_publish_a_prefix() {
        let ((), report) = run_async_under_lab(0xc8e7_0013, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            for text in [
                "CREATE (n:Copy) RETURN n; CREATE (m)",
                "CREATE (n:Copy); CREATE (m) RETURN m",
                "MATCH (n) CREATE (copy:Copy) SET copy.p=1 RETURN copy",
            ] {
                let calls = AtomicI32::new(0);
                let result = db
                    .query_write(
                        &txcx,
                        &cx,
                        &commit,
                        text,
                        &GqlParameters::new(),
                        |kind, name| {
                            calls.fetch_add(1, Ordering::Relaxed);
                            symbols(kind, name)
                        },
                        R,
                        policy(2, 2, 0),
                        |_| -> Result<ElementId, ()> { panic!("refused text cannot allocate") },
                    )
                    .await;
                assert!(matches!(result, Err(QueryWriteError::InsertText(_))));
                assert_eq!(
                    calls.load(Ordering::Relaxed),
                    0,
                    "refusal precedes catalog calls: {text}"
                );
                assert_eq!(db.frontier().unwrap(), before);
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
            let result = db
                .query_write(
                    &txcx,
                    &cx,
                    &commit,
                    "UNWIND [2,1] AS x CREATE (n:Copy {p:x}) RETURN n",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(1, 2, 0),
                    allocate,
                )
                .await;
            assert!(matches!(
                result,
                Err(QueryWriteError::Insert(GqlQueryError::Rows(_)))
            ));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
            let cancelled =
                cx.with_checkpoint_probe(Arc::new(SimulationCheckpointProbe::new(Some(1))));
            let result = db
                .query_write(
                    &txcx,
                    &cancelled,
                    &commit,
                    "CREATE (n:Copy) RETURN n",
                    &GqlParameters::new(),
                    symbols,
                    R,
                    policy(1, 1, 0),
                    |_| -> Result<ElementId, ()> { panic!("cancelled query cannot allocate") },
                )
                .await;
            assert!(matches!(
                result,
                Err(QueryWriteError::Insert(GqlQueryError::Interrupted(_)))
            ));
            assert!(db.vertices().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
