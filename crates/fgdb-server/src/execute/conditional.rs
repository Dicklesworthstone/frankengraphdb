//! Exact-frontier autocommit through the existing authorized write owners.
//! The comparison and native completion share one exclusive writer guard.

use super::*;
use fgdb_protocol::body::{BranchHead, ExecuteConditionalWrite, ExecuteHeadWrite};
use fgdb_types::CommitSeq;

enum Precondition {
    Sequence(u64),
    Head(BranchHead),
}

/// Admission before any conditional request body or parameter inspection.
/// Execution repeats this check after waiting for the exclusive writer.
pub(crate) fn authorize_write(db: &Served, token: &CapabilityToken) -> Result<(), Refusal> {
    let now = unix_millis();
    db.authority
        .verify_at(token, TRUNK, now)
        .and_then(|capability| capability.begin_write_at(TRUNK, now).map(|_| ()))
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))
}

/// The commitment covers the complete branch, including writes outside a
/// restricted graph projection. Discovery therefore requires a whole-branch
/// write grant. Repeat the check after waiting on the database reader.
pub(crate) async fn current_write_head(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
) -> Result<(BranchHead, Generation), Refusal> {
    let authorize = || {
        let now = unix_millis();
        let capability = db
            .authority
            .verify_at(token, TRUNK, now)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        capability
            .begin_write_at(TRUNK, now)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        if !capability.predicates().is_unrestricted_graph() {
            return Err(Refusal::new(
                ErrorCode::PermissionDenied,
                "branch-head discovery requires whole-branch write authority",
            ));
        }
        Ok(())
    };
    authorize()?;
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let guard = db.db.read(cx).await.map_err(Refusal::from)?;
    authorize()?;
    let marker = guard
        .branch_head()
        .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
    let head = match marker {
        Some(marker) => BranchHead::Recorded {
            seq: core::num::NonZeroU64::new(marker.commit_seq.0).ok_or_else(|| {
                Refusal::new(ErrorCode::Execution, "invalid zero marker sequence")
            })?,
            chain_hash: marker.marker_oid.0,
        },
        None => {
            let frontier = guard
                .frontier()
                .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
            if frontier != CommitSeq::ORIGIN {
                return Err(Refusal::new(
                    ErrorCode::Conflict,
                    "current frontier has no recorded branch head",
                ));
            }
            BranchHead::Empty
        }
    };
    Ok((head, guard.generation()))
}

/// Compare the complete marker while the calling operation owns the writer.
pub(super) fn check_write_head(guard: &fgdb::Database, head: BranchHead) -> Result<(), Refusal> {
    let expected = match head {
        BranchHead::Empty => None,
        BranchHead::Recorded { seq, chain_hash } => Some(fgdb_types::MarkerRef {
            marker_oid: fgdb_types::ObjectId(chain_hash),
            commit_seq: CommitSeq(seq.get()),
        }),
    };
    guard.check_branch_head(expected).map_err(|error| {
        let conflict = match &error {
            fgdb::WriteError::BranchHeadUnrecorded { .. } => true,
            fgdb::WriteError::Commit(error) => matches!(
                error.as_ref(),
                fgdb_chronicle::CommitError::Chain(fgdb_chronicle::ChainError::HeadCasMismatch(_))
            ),
            _ => false,
        };
        if conflict {
            Refusal::new(
                ErrorCode::Conflict,
                "the expected branch head is not current",
            )
        } else {
            write_refusal(&error)
        }
    })
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
    Box::pin(write_if_current_inner(
        cx,
        db,
        token,
        Precondition::Sequence(statement.expected_seq),
        &statement.statement,
        &statement.parameters,
        None,
    ))
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
        Precondition::Sequence(statement.expected_seq),
        &statement.statement,
        &statement.parameters,
        Some(frame_budget),
    ))
}

pub(crate) fn write_if_head_framed<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    statement: &'a ExecuteHeadWrite,
    frame_budget: usize,
) -> Execution<'a> {
    Box::pin(write_if_current_inner(
        cx,
        db,
        token,
        Precondition::Head(statement.expected),
        &statement.statement,
        &statement.parameters,
        Some(frame_budget),
    ))
}

async fn write_if_current_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    condition: Precondition,
    text: &str,
    params: &[(String, WireValue)],
    frame_budget: Option<usize>,
) -> Result<Answer, Refusal> {
    authorize_write(db, token)?;
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let guard = db.db.write(cx).await.map_err(Refusal::from)?;
    // Neither the pre-wait credential check nor a cached frontier authorizes
    // this decision. Expiry/retirement wins over a conflict, and frontier()
    // refuses an unhealthy native handle. The guard remains owned below.
    authorize_write(db, token)?;
    match condition {
        Precondition::Sequence(expected) => {
            let frontier = guard
                .frontier()
                .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
            if frontier != CommitSeq(expected) {
                return Err(Refusal::new(
                    ErrorCode::Conflict,
                    "the expected frontier is no longer current; no write was executed",
                ));
            }
        }
        Precondition::Head(head) => check_write_head(&guard, head)?,
    }
    let mut permit = write_permit(db, token)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    permit
        .charge_work_at(unix_millis(), text.len() as u64)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    refuse_element_identity(text)?;
    let parameters = query.with_restriction(|| {
        convert::parameters(params, None)
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
    let symbols = write_symbols(db, token, &guard, None)?;
    let prepared =
        query.with_restriction(|| ReturningTemplate::prepare(db, &symbols, text, &parameters));
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
                &symbols,
                &execution_token,
                prepared,
                guard,
                frame_budget,
                |_, rows, _, _| Ok(rows),
            )
            .await
        }
        None => write_stats_in_guard(cx, db, token, text, &parameters, guard).await,
    }
}

#[cfg(test)]
#[path = "conditional/tests.rs"]
mod tests;
