//! Connection-owned native templates over the existing authorized executor.
//! Preparation retains syntax/catalog bindings, never arguments or a snapshot.

use super::*;
use fgdb::PreparedNativeRead;
use fgdb_gql::{GqlParameters, PreparedGraphBranchText};
use fgdb_protocol::body::Prepare;
use std::sync::Arc;

pub(crate) struct PreparedRead {
    pub(super) selector: Arc<PreparedGraphBranchText>,
    pub(super) native: Arc<PreparedNativeRead>,
    pub(crate) generation: Generation,
    pub(crate) source_bytes: usize,
}

fn branch_error(error: fgdb_gql::GraphBranchTextError) -> Refusal {
    Refusal::new(ErrorCode::Statement, error.to_string())
}

fn authorized_branch(selected: &fgdb_gql::BoundGraphBranchText<'_>) -> Result<(), Refusal> {
    if selected.branch().is_some_and(|branch| branch != TRUNK) {
        Err(Refusal::new(
            ErrorCode::PermissionDenied,
            "the prepared statement names a different branch",
        ))
    } else {
        Ok(())
    }
}

pub(crate) async fn prepare_read(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    request: &Prepare,
) -> Result<PreparedRead, Refusal> {
    let _operation = db.db.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut permit = capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    // Syntax is bounded by both the wire profile and the signed work ceiling.
    // No resolver, selector parser or parameter binding precedes this permit.
    permit
        .charge_work_at(now, request.statement.len() as u64)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    let generation = db.db.generation().map_err(Refusal::from)?;
    let definition = query.with_restriction(|| {
        let parameters = convert::parameters(&request.parameters, None)
            .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
        let selector =
            PreparedGraphBranchText::prepare(&request.statement).map_err(branch_error)?;
        let selected = selector
            .bind_parameters(&parameters)
            .map_err(branch_error)?;
        authorized_branch(&selected)?;
        refuse_element_identity(selected.statement())?;
        let native = PreparedNativeRead::prepare(
            selected.statement(),
            selected.parameters(),
            db.symbols.clone(),
        )
        .map_err(query_refusal)?;
        Ok::<_, Refusal>((selector, native))
    });
    // A late invalidation wins even over a syntax error. No template is
    // installed after expiry, retirement, cancellation or generation fencing.
    permit
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    let (selector, native) = definition?;
    Ok(PreparedRead {
        selector: Arc::new(selector),
        native: Arc::new(native),
        generation,
        source_bytes: request.statement.len(),
    })
}

/// Reuse trusted catalog bindings in a fresh, capability-governed session.
/// Only immutable templates are retained by the connection; every execution
/// gets its own snapshot, owner identity and cumulative native read allowance.
pub(crate) async fn read_prepared_session<'a>(
    cx: &Cx,
    db: &'a Served,
    token: &CapabilityToken,
    prepared: &PreparedRead,
) -> Result<(ReadSession<'a>, fgdb::AuthorizedPreparedRead), Refusal> {
    let _operation = prepared.generation.enter().map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let (view, generation) = db.db.read_view(cx).await.map_err(Refusal::from)?;
    prepared.generation.check().map_err(Refusal::from)?;
    let admitted = view.authorized_prepared_read_session(
        &query,
        &db.authority,
        token,
        TRUNK,
        db.symbols.clone(),
        db.query_policy,
        unix_millis as fn() -> u64,
        Arc::clone(&prepared.selector),
        Arc::clone(&prepared.native),
    );
    prepared.generation.check().map_err(Refusal::from)?;
    let (native, template) = admitted.map_err(query_refusal)?;
    Ok((ReadSession { native, generation }, template))
}

pub(crate) fn read_prepared<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    prepared: &'a PreparedRead,
    parameters: &'a [(String, WireValue)],
) -> Execution<'a> {
    Box::pin(read_prepared_inner(cx, db, token, prepared, parameters))
}

async fn read_prepared_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    prepared: &PreparedRead,
    parameters: &[(String, WireValue)],
) -> Result<Answer, Refusal> {
    let _operation = prepared.generation.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    capability
        .begin_read_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let parameters: GqlParameters = convert::parameters(parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let (mut session, template) = read_prepared_session(cx, db, token, prepared).await?;
    let result =
        session
            .native
            .execute_with_snapshot(&crate::query_cx(&contexts), &template, &parameters);
    prepared.generation.check().map_err(Refusal::from)?;
    let (result, snapshot) = result.map_err(query_refusal)?;
    match result {
        QueryResult::Rows { columns, rows } => Ok(Answer {
            columns,
            rows: rows
                .iter()
                .map(|row| row.iter().map(convert::cell).collect())
                .collect(),
            outcome: Outcome::Rows { seq: snapshot.0 },
            generation: prepared.generation.clone(),
        }),
        QueryResult::Write { .. } => Err(Refusal::new(
            ErrorCode::Statement,
            "a prepared read cannot execute a write",
        )),
    }
}
