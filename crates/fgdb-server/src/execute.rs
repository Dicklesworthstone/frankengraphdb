//! One statement through a capability-authorized session: the execution
//! path every surface (FGP and HTTP) shares, so adapters differ only in
//! framing, never in authority, statement classes or error classes.

use crate::{Served, TRUNK, convert, unix_millis};
use asupersync::Cx;
use fgdb::{QueryError, QueryResult};
use fgdb_gql::GqlQueryError;
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

/// A complete, already-decided statement answer.
pub(crate) struct Answer {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<Vec<WireValue>>,
    pub(crate) outcome: Outcome,
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
    let (session, frontier) = {
        let guard = db
            .db
            .read(cx)
            .await
            .map_err(|_| Refusal::new(ErrorCode::Execution, "database unavailable"))?;
        // Under the read lock no write can land between these two reads, so
        // the session's pinned generation is exactly this frontier.
        let frontier = guard
            .frontier()
            .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
        let session = guard.authorized_read_session(
            &query,
            &db.authority,
            token,
            TRUNK,
            db.symbols.clone(),
            db.query_policy,
            unix_millis,
        );
        (session, frontier)
    };
    let mut session = session.map_err(query_refusal)?;
    match session.query(&query, &statement.statement, &parameters) {
        Ok(QueryResult::Rows { columns, rows }) => Ok(Answer {
            columns,
            rows: rows
                .iter()
                .map(|row| row.iter().map(convert::cell).collect())
                .collect(),
            outcome: Outcome::Rows { seq: frontier.0 },
        }),
        Ok(QueryResult::Write { .. }) => Err(Refusal::new(
            ErrorCode::Statement,
            "a write statement cannot execute as a read",
        )),
        Err(error) => Err(query_refusal(error)),
    }
}

async fn write_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
) -> Result<Answer, Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (txn, commit, query) = (contexts.txn(), contexts.commit(), contexts.query());
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let mut guard = db
        .db
        .write(cx)
        .await
        .map_err(|_| Refusal::new(ErrorCode::Execution, "database unavailable"))?;
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
    let outcome = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Outcome::WriteCommitted {
            seq: commit_seq.0,
            statements,
        },
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => Outcome::ReadClosed {
            seq: snapshot_seq.0,
            statements,
        },
    };
    Ok(Answer {
        columns: Vec::new(),
        rows: Vec::new(),
        outcome,
    })
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

fn query_refusal(error: QueryError) -> Refusal {
    let code = match &error {
        QueryError::Authorization(error) => warden_code(*error),
        QueryError::Read(_) => ErrorCode::Execution,
        QueryError::Pattern(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Aggregate(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Set(e) if gql_budget(e) => ErrorCode::Budget,
        QueryError::Pattern(_) | QueryError::Aggregate(_) | QueryError::Set(_) => {
            ErrorCode::Execution
        }
        _ => ErrorCode::Statement,
    };
    Refusal::new(code, error.to_string())
}

/// Classify a write failure by walking its typed cause chain. Nothing here
/// turns an unknown commit outcome into a refusal: that class is preserved.
fn write_refusal(error: &(dyn core::error::Error + 'static)) -> Refusal {
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
                fgdb::WriteTxnError::AuthorizedMutationRefused => {
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
