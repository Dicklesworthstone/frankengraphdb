//! Exact-frontier autocommit through the existing authorized write owners.
//! The comparison and native completion share one exclusive writer guard.

use super::*;
use fgdb_protocol::body::ExecuteConditionalWrite;
use fgdb_types::CommitSeq;

/// Admission before any conditional request body or parameter inspection.
/// Execution repeats this check after waiting for the exclusive writer.
pub(crate) fn authorize_write(db: &Served, token: &CapabilityToken) -> Result<(), Refusal> {
    let now = unix_millis();
    db.authority
        .verify_at(token, TRUNK, now)
        .and_then(|capability| capability.begin_write_at(TRUNK, now).map(|_| ()))
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))
}

/// Execute once only if the current healthy frontier still equals the caller's
/// observation. This is a precondition on ordinary current-state autocommit,
/// not a historical transaction, saved snapshot, or automatic retry.
pub(crate) fn write_if_current<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a ExecuteConditionalWrite,
) -> Execution<'a> {
    Box::pin(write_if_current_inner(cx, db, token, statement, None))
}

/// The same exact-frontier owner with negotiated FGP output admission before
/// completion. The frontier comparison still precedes binding and execution.
pub(crate) fn write_if_current_framed<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a ExecuteConditionalWrite,
    frame_budget: usize,
) -> Execution<'a> {
    Box::pin(write_if_current_inner(
        cx,
        db,
        token,
        statement,
        Some(frame_budget),
    ))
}

async fn write_if_current_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &ExecuteConditionalWrite,
    frame_budget: Option<usize>,
) -> Result<Answer, Refusal> {
    authorize_write(db, token)?;
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let guard = db.db.write(cx).await.map_err(Refusal::from)?;
    // Neither the pre-wait credential check nor a cached frontier authorizes
    // this decision. Expiry/retirement wins over a conflict, and frontier()
    // refuses an unhealthy native handle. The guard remains owned below.
    authorize_write(db, token)?;
    let frontier = guard
        .frontier()
        .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
    if frontier != CommitSeq(statement.expected_seq) {
        return Err(Refusal::new(
            ErrorCode::Conflict,
            "the expected frontier is no longer current; no write was executed",
        ));
    }
    let mut permit = write_permit(db, token)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    permit
        .charge_work_at(unix_millis(), statement.statement.len() as u64)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    refuse_element_identity(&statement.statement)?;
    let parameters = query.with_restriction(|| {
        convert::parameters(&statement.parameters, None)
            .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))
    });
    permit
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    let parameters = parameters?;
    admit_write_parameters(&mut permit, &parameters)?;
    let prepared = query
        .with_restriction(|| ReturningTemplate::prepare(db, &statement.statement, &parameters));
    let prepared = prepared.and_then(|template| {
        template
            .map(|template| template.bind_current(&query, db, &parameters))
            .transpose()
    });
    permit
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    match prepared? {
        Some(prepared) => {
            let execution_token = write_execution_token(&permit, token)?;
            write_returning_in_guard(
                cx,
                db,
                &execution_token,
                prepared,
                guard,
                frame_budget,
                |_, rows, _, _| Ok(rows),
            )
            .await
        }
        None => write_stats_in_guard(cx, db, token, &statement.statement, &parameters, guard).await,
    }
}

#[cfg(test)]
#[path = "conditional/tests.rs"]
mod tests;
