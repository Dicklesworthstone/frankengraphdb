//! One statement through a capability-authorized session: the execution
//! path every surface (FGP, HTTP and Bolt) shares, so adapters differ only in
//! framing, never in authority, statement classes or error classes.

mod edge_upsert_output;
pub(crate) mod ordered;
mod prepared;
mod subscription;
pub(crate) use prepared::{PreparedRead, prepare_read, read_prepared, read_prepared_session};
pub(crate) use subscription::{poll, subscribe};

use crate::recovery::{Generation, Unavailable};
use crate::{Served, TRUNK, convert, unix_millis};
use asupersync::Cx;
use fgdb::{QueryError, QueryResult};
use fgdb_gql::{
    GqlQueryError, PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText,
    PreparedGraphEdgeUpsertQuery, PreparedGraphEdgeUpsertQueryText, PreparedGraphInsertQuery,
    PreparedGraphInsertQueryText, PreparedGraphMutationQuery, PreparedGraphMutationQueryText,
    PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertQueryText, PreparedGraphWriteChain,
    PreparedGraphWriteChainText,
};
use fgdb_protocol::body::{ErrorCode, Execute, ExecuteBatch, Outcome, WireValue};
use fgdb_types::{EmbeddedTxnCompletion, PurposeContexts};
use fgdb_warden::CapabilityToken;

/// A refusal of one statement, reported on its child stream.
pub(crate) struct Refusal {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
}

impl Refusal {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<Unavailable> for Refusal {
    fn from(error: Unavailable) -> Self {
        Self::new(error.code(), error.message())
    }
}

/// A complete, already-decided statement answer.
pub(crate) struct Answer<R = Vec<Vec<WireValue>>> {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: R,
    pub(crate) outcome: Outcome,
    pub(crate) generation: Generation,
}

/// A statement's type-erased future: one `Send` proof here keeps every
/// connection future shallow enough for the compiler's recursion limit.
pub(crate) type Execution<'a> =
    core::pin::Pin<Box<dyn core::future::Future<Output = Result<Answer, Refusal>> + Send + 'a>>;

/// Run a read statement on a read-only authorized session.
pub(crate) fn read<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a Execute,
) -> Execution<'a> {
    Box::pin(read_inner(cx, db, token, statement))
}

/// Run a write statement as one autocommit program on an authorized session.
pub(crate) fn write<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a Execute,
) -> Execution<'a> {
    Box::pin(write_inner(cx, db, token, statement, None))
}

/// Consume one deferred no-RETURN request in its original recovery generation.
/// Its local bookmark is a lower bound, not a historical write snapshot.
/// This does not turn the queued request into a durable prepared transaction.
pub(crate) fn write_no_return_at<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a Execute,
    generation: &'a Generation,
    minimum_seq: fgdb_types::CommitSeq,
) -> Execution<'a> {
    Box::pin(write_inner(
        cx,
        db,
        token,
        statement,
        Some((generation, minimum_seq)),
    ))
}

/// One finite parameter batch through the same native authorized program as
/// embedded ingestion. The request can select no identity or raw write batch.
pub(crate) fn write_batch<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a ExecuteBatch,
) -> Execution<'a> {
    Box::pin(write_batch_inner(cx, db, token, statement))
}

/// A read-only authorized session over one served database: it cannot
/// express a write, pins one generation, and outlives the read lock.
pub(crate) struct ReadSession<'a> {
    native: fgdb::AuthorizedReadSession<'a, crate::Symbols, fn() -> u64>,
    generation: Generation,
}

/// A native source position, never a collected result. Each nonblocking poll
/// enters the retained recovery generation; no generation guard or writer
/// lock remains held while an aggregate yields or the client withholds credit.
pub(crate) struct ReadCursor<'q> {
    native: ReadSource<'q>,
    generation: Generation,
}

enum ReadSource<'q> {
    Rows(fgdb::AuthorizedSendRowCursor<'q>),
    Aggregate(fgdb::AuthorizedAsyncAggregateCursor<'q>),
}

impl ReadCursor<'_> {
    pub(crate) fn is_aggregate(&self) -> bool {
        matches!(self.native, ReadSource::Aggregate(_))
    }

    pub(crate) fn columns(&self) -> &[String] {
        match &self.native {
            ReadSource::Rows(cursor) => cursor.columns(),
            ReadSource::Aggregate(cursor) => cursor.columns(),
        }
    }
    pub(crate) fn snapshot_seq(&self) -> u64 {
        match &self.native {
            ReadSource::Rows(cursor) => cursor.snapshot_seq().0,
            ReadSource::Aggregate(cursor) => cursor.snapshot_seq().0,
        }
    }
    pub(crate) fn exhausted(&self) -> bool {
        let state = match &self.native {
            ReadSource::Rows(cursor) => cursor.state(),
            ReadSource::Aggregate(cursor) => cursor.state(),
        };
        state == fgdb_gql::stream::VertexScanState::Exhausted
    }

    /// Ordinary row streams keep their existing frame-sized batch pulls.
    /// Aggregate input must use the cooperative future, never this method.
    pub(crate) fn next_immediate(&mut self) -> Result<Option<Vec<WireValue>>, Refusal> {
        let _operation = self.generation.enter().map_err(Refusal::from)?;
        let ReadSource::Rows(cursor) = &mut self.native else {
            return Err(Refusal::new(
                ErrorCode::Execution,
                "aggregate input requires cooperative execution",
            ));
        };
        let result = cursor
            .next()
            .transpose()
            .map(|row| row.map(|row| row.values().iter().map(convert::graph).collect()));
        self.generation.check().map_err(Refusal::from)?;
        result.map_err(query_refusal)
    }

    pub(crate) async fn next(&mut self) -> Result<Option<Vec<WireValue>>, Refusal> {
        let generation = self.generation.clone();
        ordered::in_generation(&generation, async {
            let result = match &mut self.native {
                ReadSource::Rows(cursor) => cursor
                    .next()
                    .transpose()
                    .map(|row| row.map(|row| row.values().iter().map(convert::graph).collect())),
                ReadSource::Aggregate(cursor) => cursor.next_row().await.map(|row| {
                    row.map(|row| {
                        // The native compiler validates these RETURN ordinals before
                        // opening a source. Preserve exact aggregate domains and
                        // repeated keys without a lossy row-value intermediate.
                        cursor
                            .output_slots()
                            .iter()
                            .map(|slot| match *slot {
                                fgdb_gql::GraphAggregateTextSlot::GroupKey(at) => {
                                    convert::graph(&row.keys()[at])
                                }
                                fgdb_gql::GraphAggregateTextSlot::Aggregate(at) => {
                                    convert::cell(&row.values()[at])
                                }
                            })
                            .collect()
                    })
                }),
            };
            result.map_err(query_refusal)
        })
        .await
    }
}

pub(crate) enum SessionError {
    Query(QueryError),
    Unavailable(Unavailable),
}

impl From<QueryError> for SessionError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}

impl ReadSession<'_> {
    pub(crate) fn generation(&self) -> Generation {
        self.generation.clone()
    }

    pub(crate) fn query(
        &mut self,
        cx: &fgdb_types::QueryCx,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<QueryResult, SessionError> {
        self.query_with_snapshot(cx, text, parameters)
            .map(|(result, _)| result)
    }

    pub(crate) fn query_with_snapshot(
        &mut self,
        cx: &fgdb_types::QueryCx,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<(QueryResult, fgdb_types::CommitSeq), SessionError> {
        let _operation = self.generation.enter().map_err(SessionError::Unavailable)?;
        let result = self.native.query_with_snapshot(cx, text, parameters);
        // A concurrently fenced write invalidates this source result even
        // when its immutable snapshot remains readable in the engine.
        self.generation.check().map_err(SessionError::Unavailable)?;
        result.map_err(SessionError::Query)
    }

    pub(crate) fn prepare(
        &mut self,
        cx: &fgdb_types::QueryCx,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<fgdb::AuthorizedPreparedRead, Refusal> {
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        let prepared = self.native.prepare(cx, text, parameters);
        generation.check().map_err(Refusal::from)?;
        prepared.map_err(query_refusal)
    }

    #[cfg(test)]
    pub(crate) fn stream<'q>(
        &'q mut self,
        cx: &'q fgdb_types::QueryCx,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<ReadCursor<'q>, Refusal> {
        let prepared = self.prepare(cx, text, parameters)?;
        self.stream_prepared(cx, &prepared, parameters)
    }

    pub(crate) fn stream_prepared<'q>(
        &'q mut self,
        cx: &'q fgdb_types::QueryCx,
        prepared: &fgdb::AuthorizedPreparedRead,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<ReadCursor<'q>, Refusal> {
        let generation = self.generation.clone();
        let _operation = generation.enter().map_err(Refusal::from)?;
        // Classification selects exactly one existing physical compiler. A
        // refused aggregate profile never falls back to eager materialization.
        let cursor = match prepared.facade_class() {
            fgdb::NativeReadClass::Aggregate
            | fgdb::NativeReadClass::TemporalAggregate
            | fgdb::NativeReadClass::PipelineAggregate => self
                .native
                .stream_aggregate_async(cx, prepared, parameters)
                .map(ReadSource::Aggregate),
            _ => self
                .native
                .stream_send(cx, prepared, parameters)
                .map(ReadSource::Rows),
        };
        generation.check().map_err(Refusal::from)?;
        Ok(ReadCursor {
            native: cursor.map_err(query_refusal)?,
            generation,
        })
    }
}

/// Open a read session for `token` and report the generation it pinned.
pub(crate) async fn read_session<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
) -> Result<(ReadSession<'a>, fgdb_types::CommitSeq), Refusal> {
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let (view, generation) = db.db.read_view(cx).await.map_err(Refusal::from)?;
    // The view owns both graph and frontier; publication can advance without
    // changing either half of this session's answer.
    let frontier = view.frontier();
    let session = view
        .authorized_read_session(
            &query,
            &db.authority,
            token,
            TRUNK,
            db.symbols.clone(),
            db.query_policy,
            unix_millis as fn() -> u64,
        )
        .map_err(query_refusal)?;
    generation.check().map_err(Refusal::from)?;
    Ok((
        ReadSession {
            native: session,
            generation,
        },
        frontier,
    ))
}

async fn read_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
) -> Result<Answer, Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let (mut session, _) = read_session(cx, db, token).await?;
    match session.query_with_snapshot(&query, &statement.statement, &parameters) {
        Ok((QueryResult::Rows { columns, rows }, snapshot)) => Ok(Answer {
            columns,
            rows: rows
                .iter()
                .map(|row| row.iter().map(convert::cell).collect())
                .collect(),
            outcome: Outcome::Rows { seq: snapshot.0 },
            generation: session.generation(),
        }),
        Ok((QueryResult::Write { .. }, _)) => Err(Refusal::new(
            ErrorCode::Statement,
            "a write statement cannot execute as a read",
        )),
        Err(error) => Err(query_refusal(error)),
    }
}

/// Engine identities are not masked, so no capability may observe them through
/// id()/elementId() (fgdb-j687q, owner ruling 2026-09-30). The authorized
/// sessions refuse too; this covers the server's own RETURNING and
/// subscription parsers, which run before any session sees the text.
fn refuse_element_identity(text: &str) -> Result<(), Refusal> {
    if fgdb_gql::reads_element_identity(text) {
        return Err(Refusal::new(
            ErrorCode::PermissionDenied,
            "id() and elementId() are not available under a capability",
        ));
    }
    Ok(())
}

async fn write_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
    deferred: Option<(&Generation, fgdb_types::CommitSeq)>,
) -> Result<Answer, Refusal> {
    refuse_element_identity(&statement.statement)?;
    let _operation = match deferred {
        Some((generation, _)) => generation.enter(),
        None => db.db.enter(),
    }
    .map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (
        contexts.txn(),
        contexts.commit(),
        crate::query_cx(&contexts),
    );
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    if let Some(returning) = Returning::prepare(db, &statement.statement, &parameters)? {
        if deferred.is_none() {
            return write_returning(cx, db, token, returning).await;
        }
        // Bolt retains a no-RETURN definition at RUN and executes it only at
        // PULL/DISCARD. Chains use the same checked owner as projected writes,
        // even when their only rows are private WITH input. Keep the original
        // recovery generation/bookmark fence and refuse actual RETURN here.
        if !matches!(&returning, Returning::Chain(chain) if !chain.returns_rows()) {
            return Err(Refusal::new(
                ErrorCode::Statement,
                "a deferred write cannot return rows",
            ));
        }
        return write_returning_checked(cx, db, token, returning, deferred, |_, rows, _| Ok(rows))
            .await;
    }
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
    if let Some((generation, minimum_seq)) = deferred {
        // Another writer may have fenced the slot while this request waited
        // for the lock. Check again before binding, allocating or mutating.
        generation.check().map_err(Refusal::from)?;
        let frontier = guard
            .frontier()
            .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
        if frontier < minimum_seq {
            return Err(Refusal::new(
                ErrorCode::Busy,
                "the local bookmark has not been reached; no write was executed",
            ));
        }
    }
    let generation = guard.generation();
    let symbols = &db.symbols;
    let mut session = guard
        .authorized_write_session(
            &txn,
            &commit,
            &db.authority,
            token,
            TRUNK,
            |kind, name| symbols.resolve(kind, name),
            db.write_relation,
            db.write_policy,
            db.max_statements,
            unix_millis,
        )
        .map_err(|error| write_refusal(&error))?;
    // This path delivers only completion metadata, never identity receipts.
    // Requesting query() would charge a signed row for each affected identity
    // before discarding those receipts below, refusing useful max_rows=0
    // ingestion grants. Keep RETURN on its separately admitted result path.
    let (stats, completion) = session
        .query_stats(&query, &statement.statement, &parameters)
        .await
        .map_err(|error| write_refusal(&error))?;
    let statements = stats.completed_statements as u64;
    drop(session);
    drop(guard);
    let outcome = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => {
            // Wake subscriptions only after the write lock is released, so
            // their polls observe the published generation.
            db.commits.committed();
            Outcome::WriteCommitted {
                seq: commit_seq.0,
                statements,
            }
        }
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => Outcome::ReadClosed {
            seq: snapshot_seq.0,
            statements,
        },
    };
    Ok(Answer {
        columns: Vec::new(),
        rows: Vec::new(),
        outcome,
        generation,
    })
}

async fn write_batch_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &ExecuteBatch,
) -> Result<Answer, Refusal> {
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (
        contexts.txn(),
        contexts.commit(),
        crate::query_cx(&contexts),
    );
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
    let generation = guard.generation();
    let mut session = guard
        .authorized_write_session(
            &txn,
            &commit,
            &db.authority,
            token,
            TRUNK,
            |kind, name| db.symbols.resolve(kind, name),
            db.write_relation,
            db.write_policy,
            db.max_statements,
            unix_millis,
        )
        .map_err(|error| write_refusal(&error))?;
    // Authenticate Write rights before inspecting argument values. The wire
    // codec bounds this finite request; native preparation, all bindings and
    // graph execution then share ONE signed permit inside query_batch_stats.
    let parameters = statement
        .argument_sets
        .iter()
        .enumerate()
        .map(|(record, arguments)| {
            query
                .checkpoint()
                .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
            convert::parameters(arguments, None).map_err(|error| {
                Refusal::new(
                    ErrorCode::Statement,
                    format!("argument set {record}: {error}"),
                )
            })
        })
        .collect::<Result<Vec<_>, Refusal>>()?;
    let (stats, completion) = session
        .query_batch_stats(&query, &statement.statement, &parameters)
        .await
        .map_err(|error| write_refusal(&error))?;
    let statements = stats.completed_statements as u64;
    drop(session);
    drop(guard);
    let outcome = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => {
            db.commits.committed();
            Outcome::WriteCommitted {
                seq: commit_seq.0,
                statements,
            }
        }
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => Outcome::ReadClosed {
            seq: snapshot_seq.0,
            statements,
        },
    };
    Ok(Answer {
        columns: Vec::new(),
        rows: Vec::new(),
        outcome,
        generation,
    })
}
/// Prepared write projections share the same capability and native completion
/// boundary as their effects. Their collectors own RETURN semantics.
pub(crate) enum Returning {
    Chain(Box<PreparedGraphWriteChain>),
    Insert(Box<PreparedGraphInsertQuery>),
    Mutation(Box<PreparedGraphMutationQuery>),
    Merge(Box<PreparedGraphVertexUpsertQuery>),
    EdgeMerge(Box<PreparedGraphEdgeMergeQuery>),
    EdgeUpsert(Box<PreparedGraphEdgeUpsertQuery>),
}

impl Returning {
    pub(crate) fn prepare(
        db: &Served,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<Option<Self>, Refusal> {
        // Other adapters may retain this definition until a later PULL.
        // Preparing it directly must not bypass the shared identity fence.
        refuse_element_identity(text)?;
        let declarations: Vec<_> = parameters.parameter_types().collect();
        let refusal = |error: &dyn core::fmt::Display| {
            Refusal::new(ErrorCode::Statement, format!("statement: {error}"))
        };
        // Classify the whole chain before any single-write parser can claim
        // its prefix. Both private WITH rows and public RETURN rows belong to
        // the same authorized workspace and sole completion boundary.
        if PreparedGraphWriteChainText::is_chain(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                db.symbols.clone(),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|chain| Some(Self::Chain(Box::new(chain))))
                .map_err(|e| refusal(&e));
        }
        if PreparedGraphInsertQueryText::has_return_clause(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                db.symbols.clone(),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|query| Some(Self::Insert(Box::new(query))))
                .map_err(|e| refusal(&e));
        }
        if PreparedGraphMutationQueryText::has_return_clause(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                db.symbols.clone(),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|query| Some(Self::Mutation(Box::new(query))))
                .map_err(|e| refusal(&e));
        }
        if PreparedGraphVertexUpsertQueryText::has_return_clause(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                db.symbols.clone(),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|query| Some(Self::Merge(Box::new(query))))
                .map_err(|e| refusal(&e));
        }
        // ON/SET relationship upserts have their own native classifier. A
        // binding failure stays terminal, never retried as plain MERGE.
        if PreparedGraphEdgeUpsertQueryText::has_return_clause(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphEdgeUpsertQueryText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                db.symbols.clone(),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|query| Some(Self::EdgeUpsert(Box::new(query))))
                .map_err(|e| refusal(&e));
        }
        if PreparedGraphEdgeMergeQueryText::has_return_clause(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphEdgeMergeQueryText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                db.symbols.clone(),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|query| Some(Self::EdgeMerge(Box::new(query))))
                .map_err(|e| refusal(&e));
        }
        Ok(None)
    }

    pub(crate) fn columns(&self) -> &[String] {
        match self {
            Self::Chain(chain) => chain.columns(),
            Self::Insert(query) => query.columns(),
            Self::Mutation(query) => query.columns(),
            Self::Merge(query) => query.columns(),
            Self::EdgeMerge(query) => query.columns(),
            Self::EdgeUpsert(query) => query.columns(),
        }
    }
}

/// Compute projected rows before the sole authorized commit. No RETURN source
/// lookup, query evaluation or new authorization happens after publication.
async fn write_returning(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    prepared: Returning,
) -> Result<Answer, Refusal> {
    write_returning_checked(cx, db, token, prepared, None, |_, rows, _| Ok(rows)).await
}

/// A surface owns its final representation BEFORE the native commit, not a
/// fallible conversion after publication. Shared wire admission still checks
/// every native cell first; the surface's additional work uses the SAME live
/// signed permit. A rejected representation aborts all staged effects.
///
/// A deferred definition retains no transaction, read pin or operation permit.
/// Its original generation and bookmark floor are checked again under the
/// write lock before allocating identities or executing the native program.
/// Successful output is returned only after the native completion; an unknown
/// outcome is propagated unchanged and this owner never retries.
pub(crate) async fn write_returning_checked<R: Send>(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    prepared: Returning,
    deferred: Option<(&Generation, fgdb_types::CommitSeq)>,
    mut collect: impl FnMut(
        &[String],
        Vec<Vec<WireValue>>,
        &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
    ) -> Result<R, Refusal>
    + Send,
) -> Result<Answer<R>, Refusal> {
    let _operation = match deferred {
        Some((generation, _)) => generation.enter(),
        None => db.db.enter(),
    }
    .map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (
        contexts.txn(),
        contexts.commit(),
        crate::query_cx(&contexts),
    );
    let columns = prepared.columns().to_vec();
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
    if let Some((generation, minimum_seq)) = deferred {
        generation.check().map_err(Refusal::from)?;
        let frontier = guard
            .frontier()
            .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
        if frontier < minimum_seq {
            return Err(Refusal::new(
                ErrorCode::Busy,
                "the local bookmark has not been reached; no write was executed",
            ));
        }
    }
    let generation = guard.generation();
    // RETURN changes delivered rows, never the host's write authority. Derive
    // each native policy from the SAME configured program policy as rowless
    // writes. Reconstructing defaults here bypasses custom creation/effect
    // ceilings (including zero), and a read policy can be looser than a write
    // policy. Final signed row admission stays with the authorized collectors.
    let policy = db.write_policy;
    let mut admitted_rows = None;
    let mut output_refusal = None;
    let mut output =
        |rows: &[fgdb_gql::algebra::GraphValueRow],
         work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>| {
            let rows = edge_upsert_output::collect(&columns, rows, work)?;
            collect(&columns, rows, work)
        };
    let result = match &prepared {
        Returning::Chain(prepared) => guard
            .execute_graph_write_chain_authorized_checked(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                policy.insertion_policy(),
                unix_millis,
                |rows, work| match output(rows, work) {
                    Ok(rows) => {
                        admitted_rows = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::Insert(prepared) => guard
            .execute_graph_insert_query_authorized_checked(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                policy.insertion_policy(),
                unix_millis,
                |rows, work| match output(rows, work) {
                    Ok(rows) => {
                        admitted_rows = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        // Retain the exact output/meter error. Returning
                        // Err here prevents native completion altogether;
                        // it never substitutes a refusal for Unknown.
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::Mutation(prepared) => guard
            .execute_graph_mutation_query_authorized_checked(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                policy.mutations,
                unix_millis,
                |rows, work| match output(rows, work) {
                    Ok(rows) => {
                        admitted_rows = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        // Retain the exact output/meter error. Returning
                        // Err here prevents native completion altogether;
                        // it never substitutes a refusal for Unknown.
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::Merge(prepared) => guard
            .execute_graph_vertex_upserts_query_authorized_checked(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                policy.vertex_upsert_policy(),
                unix_millis,
                |rows, work| match output(rows, work) {
                    Ok(rows) => {
                        admitted_rows = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        // Retain the exact output/meter error. Returning
                        // Err here prevents native completion altogether;
                        // it never substitutes a refusal for Unknown.
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::EdgeUpsert(prepared) => guard
            .execute_graph_edge_upserts_query_authorized_checked(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                policy.edge_upsert_policy(),
                unix_millis,
                |rows, work| match output(rows, work) {
                    Ok(rows) => {
                        admitted_rows = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        // Retain the exact output/meter error. Returning
                        // Err here prevents native completion altogether;
                        // it never substitutes a refusal for Unknown.
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::EdgeMerge(prepared) => guard
            .execute_graph_edge_merge_query_authorized_checked(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                policy.edge_merge_policy(),
                unix_millis,
                |rows, work| match output(rows, work) {
                    Ok(rows) => {
                        admitted_rows = Some(rows);
                        Ok(())
                    }
                    Err(error) => {
                        output_refusal = Some(error);
                        Err(fgdb::WriteTxnError::AuthorizedMutationRefused)
                    }
                },
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
    };
    drop(guard);
    let (_execution, completion) = result.map_err(|error| output_refusal.unwrap_or(error))?;
    // Every write-RETURN family is converted and body-admitted while its
    // rollback workspace is still private. A successful checked call must
    // therefore have installed the exact rows before native completion.
    let rows = admitted_rows.expect("checked write output admission ran before completion");
    let outcome = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => {
            db.commits.committed();
            Outcome::WriteCommitted {
                seq: commit_seq.0,
                statements: 1,
            }
        }
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => Outcome::ReadClosed {
            seq: snapshot_seq.0,
            statements: 1,
        },
    };
    Ok(Answer {
        columns,
        rows,
        outcome,
        generation,
    })
}

fn returning_refusal<E, C>(error: GqlQueryError<E, C>) -> Refusal
where
    E: core::error::Error + 'static,
    C: core::error::Error + 'static,
{
    if gql_budget(&error) {
        Refusal::new(ErrorCode::Budget, error.to_string())
    } else {
        write_refusal(&error)
    }
}

fn warden_code(error: fgdb_warden::Error) -> ErrorCode {
    match error {
        fgdb_warden::Error::LimitExceeded(_) => ErrorCode::Budget,
        _ => ErrorCode::PermissionDenied,
    }
}

fn gql_budget<E, C>(error: &GqlQueryError<E, C>) -> bool {
    matches!(error, GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
}

/// A CALL whose name, arguments or YIELD list its procedure refused is a
/// statement error; a refusal while it ran (projection, index, kernel) is not.
fn procedure_statement<C>(
    error: &GqlQueryError<fgdb_gql::GraphSetExecutionError<fgdb::GqlError>, C>,
) -> bool {
    let GqlQueryError::Source(fgdb_gql::GraphSetExecutionError::Source(fgdb::GqlError::Procedure(
        procedure,
    ))) = error
    else {
        return false;
    };
    match procedure {
        fgdb::ProcedureError::Search(search) => !matches!(search, fgdb::HybridCallError::Index(_)),
        other => other.is_statement_refusal(),
    }
}

pub(crate) fn query_refusal(error: impl Into<SessionError>) -> Refusal {
    let error = match error.into() {
        SessionError::Query(error) => error,
        SessionError::Unavailable(error) => return error.into(),
    };
    let code = match &error {
        QueryError::Authorization(error) => warden_code(*error),
        QueryError::Read(_) => ErrorCode::Execution,
        QueryError::Pattern(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Aggregate(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Set(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Stream(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::EdgeStream(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::AggregateStream(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::EdgeAggregateStream(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Pattern(GqlQueryError::Interrupted(_))
        | QueryError::Aggregate(GqlQueryError::Interrupted(_))
        | QueryError::Set(GqlQueryError::Interrupted(_))
        | QueryError::Stream(GqlQueryError::Interrupted(_))
        | QueryError::EdgeStream(GqlQueryError::Interrupted(_))
        | QueryError::AggregateStream(GqlQueryError::Interrupted(_))
        | QueryError::EdgeAggregateStream(GqlQueryError::Interrupted(_)) => ErrorCode::Cancelled,
        QueryError::Stream(GqlQueryError::Source(fgdb_gql::stream::VertexScanError::Plan(_)))
        | QueryError::EdgeStream(GqlQueryError::Source(
            fgdb_gql::edge_stream::EdgeScanError::Plan(_),
        )) => ErrorCode::Statement,
        QueryError::Stream(_)
        | QueryError::EdgeStream(_)
        | QueryError::AggregateStream(_)
        | QueryError::EdgeAggregateStream(_) => ErrorCode::Execution,
        QueryError::Set(e) if procedure_statement(e) => ErrorCode::Statement,
        QueryError::Pattern(_) | QueryError::Aggregate(_) | QueryError::Set(_) => {
            ErrorCode::Execution
        }
        _ => ErrorCode::Statement,
    };
    Refusal::new(code, error.to_string())
}

/// Classify a write failure by walking its typed cause chain. Nothing here
/// turns an unknown commit outcome into a refusal: that class is preserved.
pub(crate) fn write_refusal(error: &(dyn core::error::Error + 'static)) -> Refusal {
    let mut code = ErrorCode::Statement;
    let mut source = Some(error);
    while let Some(current) = source {
        if matches!(
            current.downcast_ref::<fgdb_gql::GraphWriteScriptBatchError>(),
            Some(fgdb_gql::GraphWriteScriptBatchError::TooManyStatements { .. })
        ) {
            code = ErrorCode::Budget;
            break;
        }
        if let Some(warden) = current.downcast_ref::<fgdb_warden::Error>() {
            code = warden_code(*warden);
            break;
        }
        if let Some(write) = current.downcast_ref::<fgdb::WriteError>() {
            code = match write {
                fgdb::WriteError::FirstCommitterWins { .. } => ErrorCode::Conflict,
                fgdb::WriteError::CommitOutcomeUnknown { .. }
                | fgdb::WriteError::HandleCommitOutcomeUnknown { .. }
                | fgdb::WriteError::CommittedNeedsRecovery { .. }
                | fgdb::WriteError::RecoveryRequired(_) => ErrorCode::OutcomeUnknown,
                _ => ErrorCode::Execution,
            };
            break;
        }
        if let Some(txn) = current.downcast_ref::<fgdb::WriteTxnError>() {
            match txn {
                fgdb::WriteTxnError::Authorization(warden) => {
                    code = warden_code(*warden);
                    break;
                }
                fgdb::WriteTxnError::AuthorizedChainStepUnsupported { .. } => {
                    code = ErrorCode::Statement;
                    break;
                }
                fgdb::WriteTxnError::AuthorizedMutationRefused
                | fgdb::WriteTxnError::AuthorizedClientIdentity => {
                    code = ErrorCode::PermissionDenied;
                    break;
                }
                _ => code = ErrorCode::Execution,
            }
        }
        source = current.source();
    }
    Refusal::new(code, error.to_string())
}

/// The schema names a capability may see: the operator's label, relation
/// and property bindings (there is no durable catalog), each filtered by the
/// token's scope so a hidden name is never disclosed (FG-INV-20). Read
/// rights are required, as for any statement. Names come back sorted.
pub(crate) struct Schema {
    pub(crate) labels: Vec<String>,
    pub(crate) relations: Vec<String>,
    pub(crate) properties: Vec<String>,
}

pub(crate) fn schema(db: &Served, token: &CapabilityToken) -> Result<Schema, Refusal> {
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let scope = capability.predicates();
    let visible = |names: &mut dyn Iterator<Item = (&str, u32)>, allows: &dyn Fn(u64) -> bool| {
        names
            .filter(|&(_, id)| allows(u64::from(id)))
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>()
    };
    Ok(Schema {
        labels: visible(&mut db.symbols.labels(), &|id| {
            scope.allows_label(fgdb_delta_types::LabelId(id))
        }),
        relations: visible(&mut db.symbols.relations(), &|id| {
            scope.allows_relation(fgdb_delta_types::RelationId(id))
        }),
        properties: visible(&mut db.symbols.properties(), &|id| {
            scope.allows_property(fgdb_delta_types::PropertyKeyId(id))
        }),
    })
}

#[cfg(test)]
mod all_target_merge_tests;

#[cfg(test)]
mod occurrence_upsert_tests;

#[cfg(test)]
mod returning_admission_tests;
