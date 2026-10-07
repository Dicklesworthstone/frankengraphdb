//! One statement through a capability-authorized session: the execution
//! path every surface (FGP and HTTP) shares, so adapters differ only in
//! framing, never in authority, statement classes or error classes.

use crate::{Served, TRUNK, convert, unix_millis};
use asupersync::Cx;
use fgdb::{
    NativeSubscription, QueryError, QueryResult, StandingQueryError, SubscribeError,
    SubscriptionBatch, SubscriptionError,
};
use fgdb_gql::GqlQueryError;
use fgdb_protocol::body::{ErrorCode, Execute, Outcome, WireValue};
use fgdb_types::{EmbeddedTxnCompletion, PurposeContexts};
use fgdb_warden::CapabilityToken;
use std::sync::Arc;
use std::sync::atomic::Ordering;

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

/// A read-only authorized session over one served database: it cannot
/// express a write, pins one generation, and outlives the read lock.
pub(crate) type ReadSession<'a> = fgdb::AuthorizedReadSession<'a, crate::Symbols, fn() -> u64>;

/// Open a read session for `token` and report the generation it pinned.
pub(crate) async fn read_session<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
) -> Result<(ReadSession<'a>, fgdb_types::CommitSeq), Refusal> {
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = contexts.query();
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
    Ok((session, frontier))
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

pub(crate) fn query_refusal(error: QueryError) -> Refusal {
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

/// A registered subscription and its output columns.
pub(crate) struct Subscription {
    pub(crate) consumer: NativeSubscription,
    pub(crate) columns: Vec<String>,
}

/// Register `SUBSCRIBE TO <read>` for a capability that may observe it.
///
/// The engine's maintained queries are privileged: they have no capability
/// masking yet. Until authorized standing queries exist, a subscription
/// therefore requires a read capability whose scope hides nothing (every
/// label, relation and property), for which the unmasked result is exactly
/// what it may already read. Registrations live as long as the database, so
/// each served database admits a bounded number over the server's lifetime.
/// Per-subscription delta backlog: retained commits, changed rows, and
/// logical payload units. Eviction drops the oldest whole ticks first.
const REPLAY_TICKS: usize = 1024;
const REPLAY_ROWS: usize = 100_000;
const REPLAY_PAYLOAD_UNITS: usize = 1 << 22;

pub(crate) async fn subscribe(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &Execute,
) -> Result<Subscription, Refusal> {
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let scope = capability.predicates();
    if !(scope.sees_all_incidence() && scope.sees_all_fields()) {
        return Err(Refusal::new(
            ErrorCode::PermissionDenied,
            "a subscription requires a read capability with unrestricted label, relation and property scope",
        ));
    }
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    if db
        .subscriptions
        .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < db.max_subscriptions).then_some(count + 1)
        })
        .is_err()
    {
        return Err(Refusal::new(
            ErrorCode::Budget,
            "this database's subscription registrations are exhausted until restart",
        ));
    }
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = contexts.query();
    let mut guard = db
        .db
        .write(cx)
        .await
        .map_err(|_| Refusal::new(ErrorCode::Execution, "database unavailable"))?;
    let mut consumer = guard
        .subscribe_native(
            &query,
            &statement.statement,
            &parameters,
            db.symbols.clone(),
            db.query_policy,
        )
        .map_err(|error| {
            let code = match &error {
                SubscribeError::Subscription(SubscriptionError::Query(
                    StandingQueryError::Interrupted(_),
                )) => ErrorCode::Execution,
                _ => ErrorCode::Statement,
            };
            Refusal::new(code, error.to_string())
        })?;
    // Retain a bounded backlog of deltas, so commits that land between two
    // polls arrive as one exact combined change instead of forcing a fresh
    // baseline. A consumer that falls further behind than the backlog gets
    // DeltaUnavailable and restarts from a new baseline (see `poll`).
    consumer
        .enable_replay(
            &mut guard,
            &query,
            REPLAY_TICKS,
            REPLAY_ROWS,
            REPLAY_PAYLOAD_UNITS,
            db.query_policy,
        )
        .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
    let columns = guard
        .standing_native_columns(&query, consumer.handle())
        .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?
        .to_vec();
    Ok(Subscription { consumer, columns })
}

/// The next batch the subscriber has not acknowledged, or `None` when it is
/// caught up. The capability is rechecked first, so expiry or a retired
/// issuer ends a subscription at its next batch. A delta the engine no longer
/// retains is replaced by a fresh baseline, never by an empty or partial one.
pub(crate) async fn poll(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    subscription: &mut Subscription,
) -> Result<Option<Arc<SubscriptionBatch>>, Refusal> {
    db.authority
        .verify_at(token, TRUNK, unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = contexts.query();
    let guard = db
        .db
        .read(cx)
        .await
        .map_err(|_| Refusal::new(ErrorCode::Execution, "database unavailable"))?;
    match subscription.consumer.poll(&guard, &query, db.query_policy) {
        Err(SubscriptionError::Query(StandingQueryError::DeltaUnavailable { .. })) => {
            subscription
                .consumer
                .restart_from_current()
                .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
            subscription
                .consumer
                .poll(&guard, &query, db.query_policy)
                .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))
        }
        other => other.map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string())),
    }
}
