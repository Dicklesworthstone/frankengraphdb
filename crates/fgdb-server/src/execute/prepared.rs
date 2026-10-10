//! Connection-owned native templates over the existing authorized executor.
//! Preparation retains syntax/catalog bindings, never arguments or a snapshot.

use super::*;
use fgdb::PreparedNativeRead;
use fgdb_gql::{GqlParameters, PreparedGraphBranchText};
use fgdb_protocol::body::Prepare;

pub(crate) struct PreparedRead {
    selector: PreparedGraphBranchText,
    native: PreparedNativeRead,
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
    let query = contexts.query();
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    let generation = db.db.read(cx).await.map_err(Refusal::from)?.generation();
    let definition = query.with_restriction(|| {
        let parameters = convert::parameters(&request.parameters, None)
            .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
        let selector =
            PreparedGraphBranchText::prepare(&request.statement).map_err(branch_error)?;
        let selected = selector
            .bind_parameters(&parameters)
            .map_err(branch_error)?;
        authorized_branch(&selected)?;
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
        selector,
        native,
        generation,
        source_bytes: request.statement.len(),
    })
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
    let selected = prepared
        .selector
        .bind_parameters(&parameters)
        .map_err(branch_error)?;
    authorized_branch(&selected)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let guard = db.db.read(cx).await.map_err(Refusal::from)?;
    prepared.generation.check().map_err(Refusal::from)?;
    let frontier = guard
        .frontier()
        .map_err(|error| Refusal::new(ErrorCode::Execution, error.to_string()))?;
    let result = prepared.native.execute_authorized(
        &guard,
        &contexts.query(),
        &db.authority,
        token,
        TRUNK,
        selected.parameters(),
        db.query_policy,
        unix_millis,
    );
    prepared.generation.check().map_err(Refusal::from)?;
    match result.map_err(query_refusal)? {
        QueryResult::Rows { columns, rows } => Ok(Answer {
            columns,
            rows: rows
                .iter()
                .map(|row| row.iter().map(convert::cell).collect())
                .collect(),
            outcome: Outcome::Rows { seq: frontier.0 },
            generation: prepared.generation.clone(),
        }),
        QueryResult::Write { .. } => Err(Refusal::new(
            ErrorCode::Statement,
            "a prepared read cannot execute a write",
        )),
    }
}
