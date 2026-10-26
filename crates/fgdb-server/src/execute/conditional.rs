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
    Box::pin(write_if_current_inner(cx, db, token, statement))
}

async fn write_if_current_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    statement: &ExecuteConditionalWrite,
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
    refuse_element_identity(&statement.statement)?;
    let parameters = convert::parameters(&statement.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    match Returning::prepare(db, &statement.statement, &parameters)? {
        Some(prepared) => {
            write_returning_in_guard(cx, db, token, prepared, guard, |_, rows, _| Ok(rows)).await
        }
        None => write_stats_in_guard(cx, db, token, &statement.statement, &parameters, guard).await,
    }
}

#[cfg(test)]
#[path = "conditional/tests.rs"]
mod tests;
