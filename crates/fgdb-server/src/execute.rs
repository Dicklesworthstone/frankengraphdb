//! One statement through a capability-authorized session: the execution
//! path every surface (FGP and HTTP) shares, so adapters differ only in
//! framing, never in authority, statement classes or error classes.

mod prepared;
mod subscription;
pub(crate) use prepared::{PreparedRead, prepare_read, read_prepared};
pub(crate) use subscription::{poll, subscribe};

use crate::recovery::{Generation, Unavailable};
use crate::{Served, TRUNK, convert, unix_millis};
use asupersync::Cx;
use fgdb::{QueryError, QueryResult};
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlQueryError, GraphMutationPolicy, GraphVertexMergePolicy, GraphVertexUpsertPolicy,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText, PreparedGraphMutationQuery,
    PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_protocol::body::{ErrorCode, Execute, Outcome, WireValue};
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
pub(crate) struct Answer {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<WireValue>>,
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
    Box::pin(write_inner(cx, db, token, statement))
}

/// A read-only authorized session over one served database: it cannot
/// express a write, pins one generation, and outlives the read lock.
pub(crate) struct ReadSession<'a> {
    native: fgdb::AuthorizedReadSession<'a, crate::Symbols, fn() -> u64>,
    generation: Generation,
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
        let _operation = self.generation.enter().map_err(SessionError::Unavailable)?;
        let result = self.native.query(cx, text, parameters);
        // A concurrently fenced write invalidates this source result even
        // when its immutable snapshot remains readable in the engine.
        self.generation.check().map_err(SessionError::Unavailable)?;
        result.map_err(SessionError::Query)
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
    let query = contexts.query();
    let guard = db.db.read(cx).await.map_err(Refusal::from)?;
    let generation = guard.generation();
    // Under the read lock no write can land between these two reads, so
    // the session's pinned generation is exactly this frontier.
    let frontier = guard
        .frontier()
        .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
    let session = guard
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
    let query = contexts.query();
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let (mut session, frontier) = read_session(cx, db, token).await?;
    match session.query(&query, &statement.statement, &parameters) {
        Ok(QueryResult::Rows { columns, rows }) => Ok(Answer {
            columns,
            rows: rows
                .iter()
                .map(|row| row.iter().map(convert::cell).collect())
                .collect(),
            outcome: Outcome::Rows { seq: frontier.0 },
            generation: session.generation(),
        }),
        Ok(QueryResult::Write { .. }) => Err(Refusal::new(
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
) -> Result<Answer, Refusal> {
    refuse_element_identity(&statement.statement)?;
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (contexts.txn(), contexts.commit(), contexts.query());
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    if let Some(returning) = Returning::prepare(db, &statement.statement, &parameters)? {
        return write_returning(cx, db, token, returning).await;
    }
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
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
    let (receipt, completion) = session
        .query(&query, &statement.statement, &parameters)
        .await
        .map_err(|error| write_refusal(&error))?;
    let statements = receipt.stats().completed_statements as u64;
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

/// Created elements one CREATE/INSERT ... RETURN statement may make.
const MAX_CREATED: u64 = 100_000;

/// Prepared write projections share the same capability and native completion
/// boundary as their effects. Their collectors own RETURN semantics.
enum Returning {
    Insert(Box<PreparedGraphInsertQuery>),
    Mutation(Box<PreparedGraphMutationQuery>),
    Merge(Box<PreparedGraphVertexUpsertQuery>),
}

impl Returning {
    fn prepare(
        db: &Served,
        text: &str,
        parameters: &fgdb_gql::GqlParameters,
    ) -> Result<Option<Self>, Refusal> {
        let declarations: Vec<_> = parameters.parameter_types().collect();
        let refusal = |error: &dyn core::fmt::Display| {
            Refusal::new(ErrorCode::Statement, format!("statement: {error}"))
        };
        if PreparedGraphInsertQueryText::has_return_clause(text).map_err(|e| refusal(&e))? {
            let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
                text,
                db.write_relation,
                &declarations,
                |kind, name| db.symbols.resolve(kind, name),
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
                |kind, name| db.symbols.resolve(kind, name),
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
                |kind, name| db.symbols.resolve(kind, name),
            )
            .map_err(|e| refusal(&e))?;
            return template
                .bind_parameters(parameters)
                .map(|query| Some(Self::Merge(Box::new(query))))
                .map_err(|e| refusal(&e));
        }
        Ok(None)
    }

    fn columns(&self) -> &[String] {
        match self {
            Self::Insert(query) => query.columns(),
            Self::Mutation(query) => query.columns(),
            Self::Merge(query) => query.columns(),
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
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (contexts.txn(), contexts.commit(), contexts.query());
    let columns = prepared.columns().to_vec();
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
    let generation = guard.generation();
    let result = match &prepared {
        Returning::Insert(prepared) => guard
            .execute_graph_insert_query_authorized(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                GraphInsertPolicy::new(db.query_policy, MAX_CREATED, MAX_CREATED),
                unix_millis,
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::Mutation(prepared) => guard
            .execute_graph_mutation_query_authorized(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                GraphMutationPolicy::new(db.query_policy, MAX_CREATED),
                unix_millis,
            )
            .await
            .map(|(_, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
        Returning::Merge(prepared) => guard
            .execute_graph_vertex_upsert_query_authorized(
                &txn,
                &query,
                &commit,
                &db.authority,
                token,
                TRUNK,
                prepared,
                GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(db.query_policy), 1_000),
                unix_millis,
            )
            .await
            .map(|(_, _, rows, completion)| (rows, completion))
            .map_err(returning_refusal),
    };
    drop(guard);
    let (execution, completion) = result?;
    let rows = execution
        .value
        .iter()
        .map(|row| row.values().iter().map(convert::graph).collect())
        .collect();
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
        fgdb::ProcedureError::Bind(_) => true,
        fgdb::ProcedureError::Search(search) => !matches!(search, fgdb::HybridCallError::Index(_)),
        _ => false,
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
