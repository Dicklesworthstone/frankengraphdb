//! Connection-owned native templates over the existing authorized executor.
//! Preparation retains syntax/catalog bindings, never arguments or a snapshot.

use super::*;
use fgdb::PreparedNativeRead;
use fgdb_gql::{GqlParameters, PreparedGraphBranchText, PreparedGraphWriteScript};
use fgdb_protocol::body::{Prepare, PrepareMode, PreparedExecutionMode};
use std::sync::Arc;

pub(crate) struct PreparedRead {
    pub(super) selector: Arc<PreparedGraphBranchText>,
    pub(super) native: Arc<PreparedNativeRead>,
    lowered: Option<fgdb_gql::GqlMapArgumentText>,
    pub(crate) generation: Generation,
    pub(crate) source_bytes: usize,
}

impl PreparedRead {
    /// Rebind the caller's map fields under current authority. The attached
    /// native template and branch selector share the same lowered statement;
    /// neither owns representative argument values from PREPARE.
    pub(crate) fn arguments<'p>(
        &self,
        cx: &fgdb_types::QueryCx,
        db: &Served,
        token: &CapabilityToken,
        parameters: &'p GqlParameters,
    ) -> Result<std::borrow::Cow<'p, GqlParameters>, Refusal> {
        let Some(lowered) = &self.lowered else {
            return Ok(std::borrow::Cow::Borrowed(parameters));
        };
        let now = unix_millis();
        let capability = db
            .authority
            .verify_at(token, TRUNK, now)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        let mut permit = capability
            .begin_read_at(TRUNK, now)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        let _operation = self.generation.enter().map_err(Refusal::from)?;
        cx.checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        let arguments = (|| {
            let selected = self
                .selector
                .bind_parameters(parameters)
                .map_err(branch_error)?;
            authorized_branch(&selected)?;
            lowered
                .arguments(parameters)
                .map(std::borrow::Cow::Owned)
                .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))
        })();
        // Invalidated authority/generation wins over a malformed field too.
        permit
            .checkpoint_at(unix_millis())
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        cx.checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        self.generation.check().map_err(Refusal::from)?;
        arguments
    }
}

pub(crate) struct PreparedWrite {
    script: PreparedGraphWriteScript,
    /// The value-free splice of map arguments standing for property maps
    /// (fgdb-0s2pq); every execution derives its own arguments through it.
    lowered: Option<fgdb_gql::GqlMapArgumentText>,
    generation: Generation,
    source_bytes: usize,
}

/// A script fault of a lowered statement, at the caller's bytes.
fn restored_fault<E, A, C>(
    lowered: Option<&fgdb_gql::GqlMapArgumentText>,
    mut fault: fgdb_gql::GraphWriteScriptExecutionError<E, A, C>,
) -> fgdb_gql::GraphWriteScriptExecutionError<E, A, C> {
    use fgdb_gql::GraphWriteScriptExecutionError as F;
    if let Some(lowered) = lowered {
        match &mut fault {
            F::Binding(error)
            | F::BatchBinding(fgdb_gql::GraphWriteScriptBatchError::Arguments {
                source: error,
                ..
            }) => lowered.restore(error),
            _ => {}
        }
    }
    fault
}

pub(crate) struct PreparedReturning {
    template: ReturningTemplate,
    generation: Generation,
    source_bytes: usize,
}

/// The compiler determines a handle's class once. Client delivery modes can
/// select a compatible executor, but can never promote a read into a write.
pub(crate) enum PreparedDefinition {
    Read(PreparedRead),
    Write(PreparedWrite),
    WriteReturning(PreparedReturning),
}

impl PreparedDefinition {
    pub(crate) fn generation(&self) -> &Generation {
        match self {
            Self::Read(definition) => &definition.generation,
            Self::Write(definition) => &definition.generation,
            Self::WriteReturning(definition) => &definition.generation,
        }
    }

    pub(crate) fn source_bytes(&self) -> usize {
        match self {
            Self::Read(definition) => definition.source_bytes,
            Self::Write(definition) => definition.source_bytes,
            Self::WriteReturning(definition) => definition.source_bytes,
        }
    }

    pub(crate) fn read(&self) -> Option<&PreparedRead> {
        match self {
            Self::Read(definition) => Some(definition),
            Self::Write(_) | Self::WriteReturning(_) => None,
        }
    }
}

impl From<PreparedRead> for PreparedDefinition {
    fn from(value: PreparedRead) -> Self {
        Self::Read(value)
    }
}

pub(crate) async fn prepare_statement(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    request: &Prepare,
) -> Result<PreparedDefinition, Refusal> {
    match request.mode {
        PrepareMode::Read => prepare_read(cx, db, token, request)
            .await
            .map(PreparedDefinition::Read),
        PrepareMode::Write => prepare_write(cx, db, token, request)
            .await
            .map(PreparedDefinition::Write),
        PrepareMode::WriteReturning => {
            prepare_write_returning(cx, db, token, request).map(PreparedDefinition::WriteReturning)
        }
    }
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
    if request.mode != PrepareMode::Read {
        return Err(mode_refusal());
    }
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
        let lowered = fgdb_gql::GqlMapArgumentText::lower_fields(&request.statement, &parameters);
        let (statement, parameters) = lowered.as_ref().map_or(
            (request.statement.as_str(), &parameters),
            |(lowered, derived)| (lowered.text(), derived),
        );
        let selector = PreparedGraphBranchText::prepare(statement).map_err(branch_error)?;
        let selected = selector.bind_parameters(parameters).map_err(branch_error)?;
        authorized_branch(&selected)?;
        refuse_element_identity(selected.statement())?;
        let native = PreparedNativeRead::prepare(
            selected.statement(),
            selected.parameters(),
            db.symbols.clone(),
        )
        .map_err(query_refusal)?;
        Ok::<_, Refusal>((selector, native, lowered.map(|(lowered, _)| lowered)))
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
    let (selector, native, lowered) = definition?;
    Ok(PreparedRead {
        selector: Arc::new(selector),
        native: Arc::new(native),
        lowered,
        generation,
        source_bytes: request.statement.len(),
    })
}

async fn prepare_write(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    request: &Prepare,
) -> Result<PreparedWrite, Refusal> {
    let generation = db.db.generation().map_err(Refusal::from)?;
    let _operation = generation.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    db.authority
        .verify_at(token, TRUNK, now)
        .and_then(|capability| capability.begin_write_at(TRUNK, now).map(|_| ()))
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let parameters = convert::parameters(&request.parameters, None)
        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
    // Map arguments standing for property maps splice with the representative
    // arguments' keys; the template keeps the keys, never the values.
    let lowered = fgdb_gql::GqlMapArgumentText::lower(&request.statement, &parameters);
    let (statement, parameters) = lowered.as_ref().map_or(
        (request.statement.as_str(), &parameters),
        |(lowered, derived)| (lowered.text(), derived),
    );
    let guard = db.db.read(cx).await.map_err(Refusal::from)?;
    generation.check().map_err(Refusal::from)?;
    let script = guard.prepare_graph_write_script_authorized(
        &query,
        &contexts.commit(),
        &db.authority,
        token,
        TRUNK,
        statement,
        parameters,
        |kind, name| db.symbols.resolve(kind, name),
        db.write_relation,
        db.max_statements,
        unix_millis,
    );
    generation.check().map_err(Refusal::from)?;
    let lowered = lowered.map(|(lowered, _)| lowered);
    Ok(PreparedWrite {
        script: script.map_err(|error| write_refusal(&restored_fault(lowered.as_ref(), error)))?,
        lowered,
        generation,
        source_bytes: request.statement.len(),
    })
}

fn prepare_write_returning(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    request: &Prepare,
) -> Result<PreparedReturning, Refusal> {
    let generation = db.db.generation().map_err(Refusal::from)?;
    let _operation = generation.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    let capability = db
        .authority
        .verify_at(token, TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let mut permit = capability
        .begin_write_at(TRUNK, now)
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    // Admit bounded definition work before parameter conversion, parsing or
    // catalog resolution. Preparation retains no workspace or live permit.
    permit
        .charge_work_at(
            now,
            request.statement.len() as u64 + request.parameters.len() as u64 + 1,
        )
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    let definition = query.with_restriction(|| {
        let parameters = convert::parameters(&request.parameters, None)
            .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
        let template = ReturningTemplate::prepare(db, &request.statement, &parameters)?
            .filter(ReturningTemplate::returns_rows)
            .ok_or_else(mode_refusal)?;
        // Validate representative types/constant expressions, then discard
        // their values. They can never become an execution's implicit defaults.
        drop(template.bind(&parameters)?);
        Ok::<_, Refusal>(template)
    });
    permit
        .checkpoint_at(unix_millis())
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    query
        .checkpoint()
        .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
    generation.check().map_err(Refusal::from)?;
    Ok(PreparedReturning {
        template: definition?,
        generation,
        source_bytes: request.statement.len(),
    })
}

fn mode_refusal() -> Refusal {
    Refusal::new(
        ErrorCode::Statement,
        "prepared statement class does not permit the requested execution mode",
    )
}

pub(crate) fn run_prepared<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    definition: &'a PreparedDefinition,
    mode: PreparedExecutionMode,
    parameters: &'a [(String, WireValue)],
) -> Execution<'a> {
    match (definition, mode) {
        (PreparedDefinition::Read(prepared), PreparedExecutionMode::Buffered) => {
            read_prepared(cx, db, token, prepared, parameters)
        }
        (PreparedDefinition::Write(prepared), PreparedExecutionMode::Write) => {
            Box::pin(write_prepared_inner(
                cx,
                db,
                token,
                prepared,
                PreparedParameters::Single(parameters),
            ))
        }
        (PreparedDefinition::WriteReturning(prepared), PreparedExecutionMode::WriteReturning) => {
            Box::pin(write_returning_prepared(
                cx, db, token, prepared, parameters,
            ))
        }
        _ => Box::pin(async { Err(mode_refusal()) }),
    }
}

/// Execute every binding against one frozen write template and one permit.
/// The native executor binds the entire batch before staging any mutations.
pub(crate) fn run_prepared_batch<'a>(
    cx: &'a Cx,
    db: &'a Served,
    token: &'a CapabilityToken,
    definition: &'a PreparedDefinition,
    argument_sets: &'a [Vec<(String, WireValue)>],
) -> Execution<'a> {
    match definition {
        PreparedDefinition::Write(prepared) => Box::pin(write_prepared_inner(
            cx,
            db,
            token,
            prepared,
            PreparedParameters::Batch(argument_sets),
        )),
        PreparedDefinition::Read(_) | PreparedDefinition::WriteReturning(_) => {
            Box::pin(async { Err(mode_refusal()) })
        }
    }
}

async fn write_returning_prepared(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    prepared: &PreparedReturning,
    parameters: &[(String, WireValue)],
) -> Result<Answer, Refusal> {
    let _operation = prepared.generation.enter().map_err(Refusal::from)?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    let (bound, execution_token) = {
        let now = unix_millis();
        let capability = db
            .authority
            .verify_at(token, TRUNK, now)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        let mut permit = capability
            .begin_write_at(TRUNK, now)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        permit
            .charge_work_at(now, parameters.len() as u64 + 1)
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        let bound = query.with_restriction(|| {
            let parameters = convert::parameters(parameters, None)
                .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))?;
            prepared.template.bind(&parameters)
        });
        permit
            .checkpoint_at(unix_millis())
            .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
        query
            .checkpoint()
            .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
        prepared.generation.check().map_err(Refusal::from)?;
        let bound = bound?;
        // Carry the definition debit into the native owner's signed ceiling.
        // Appending a restriction can only narrow this verified capability.
        let remaining = permit.predicates().limits().max_work - permit.usage().work;
        let execution_token = token
            .attenuate(fgdb_warden::Restriction::MaxWork(remaining))
            .map_err(|error| {
                let code = if matches!(error, fgdb_warden::Error::TooLarge) {
                    ErrorCode::Budget
                } else {
                    warden_code(error)
                };
                Refusal::new(code, error.to_string())
            })?;
        (bound, execution_token)
    };
    // Reuse the existing checked native owner: effect selection, RETURN,
    // final wire admission and publication share its cumulative allowance.
    // The original generation is checked again after waiting for the writer.
    // No conversion, new authority check or retry follows native completion.
    write_returning_checked(
        cx,
        db,
        &execution_token,
        bound,
        Some((&prepared.generation, fgdb_types::CommitSeq(0))),
        |_, rows, _| Ok(rows),
    )
    .await
}

enum PreparedParameters<'a> {
    Single(&'a [(String, WireValue)]),
    Batch(&'a [Vec<(String, WireValue)>]),
}

enum NativeParameters {
    Single(GqlParameters),
    Batch(Vec<GqlParameters>),
}

async fn write_prepared_inner(
    cx: &Cx,
    db: &Served,
    token: &CapabilityToken,
    prepared: &PreparedWrite,
    parameters: PreparedParameters<'_>,
) -> Result<Answer, Refusal> {
    let _operation = prepared.generation.enter().map_err(Refusal::from)?;
    let now = unix_millis();
    db.authority
        .verify_at(token, TRUNK, now)
        .and_then(|capability| capability.begin_write_at(TRUNK, now).map(|_| ()))
        .map_err(|error| Refusal::new(warden_code(error), error.to_string()))?;
    let contexts = PurposeContexts::narrow_runtime_root(cx);
    let query = crate::query_cx(&contexts);
    // Each argument set's map arguments must carry the keys the template was
    // lowered with; it binds through the fresh per-key arguments.
    let lowered = prepared.lowered.as_ref();
    let derive = |parameters: fgdb_gql::GqlParameters| match lowered {
        Some(lowered) => lowered.arguments(&parameters),
        None => Ok(parameters),
    };
    let parameters = match parameters {
        PreparedParameters::Single(parameters) => NativeParameters::Single(
            convert::parameters(parameters, None)
                .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))
                .and_then(|parameters| {
                    derive(parameters)
                        .map_err(|error| Refusal::new(ErrorCode::Statement, error.to_string()))
                })?,
        ),
        PreparedParameters::Batch(argument_sets) => {
            let mut converted = Vec::with_capacity(argument_sets.len());
            for (index, parameters) in argument_sets.iter().enumerate() {
                query
                    .checkpoint()
                    .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
                let refusal = |error: &dyn core::fmt::Display| {
                    Refusal::new(
                        ErrorCode::Statement,
                        format!("argument set {index}: {error}"),
                    )
                };
                let parameters =
                    convert::parameters(parameters, None).map_err(|error| refusal(&error))?;
                converted.push(derive(parameters).map_err(|error| refusal(&error))?);
            }
            NativeParameters::Batch(converted)
        }
    };
    let mut guard = db.db.write(cx).await.map_err(Refusal::from)?;
    // Waiting for another writer cannot revive metadata from an old recovery
    // generation. No binding or identity allocation precedes this fence.
    prepared.generation.check().map_err(Refusal::from)?;
    let generation = guard.generation();
    let (stats, completion) = match parameters {
        NativeParameters::Single(parameters) => {
            guard
                .execute_graph_write_script_authorized(
                    &contexts.txn(),
                    &query,
                    &contexts.commit(),
                    &db.authority,
                    token,
                    TRUNK,
                    &prepared.script,
                    &parameters,
                    db.write_policy,
                    unix_millis,
                )
                .await
        }
        NativeParameters::Batch(argument_sets) => {
            guard
                .execute_graph_write_script_batch_authorized(
                    &contexts.txn(),
                    &query,
                    &contexts.commit(),
                    &db.authority,
                    token,
                    TRUNK,
                    &prepared.script,
                    &argument_sets,
                    db.max_statements,
                    db.write_policy,
                    unix_millis,
                )
                .await
        }
    }
    .map_err(|error| write_refusal(&restored_fault(lowered, error)))?;
    drop(guard);
    let statements = stats.completed_statements as u64;
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
        created: super::Created {
            vertices: stats.created_vertices,
            edges: stats.created_edges,
        },
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
    let query = crate::query_cx(&contexts);
    let parameters = prepared.arguments(&query, db, token, &parameters)?;
    let result = session
        .native
        .execute_with_snapshot(&query, &template, &parameters);
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
            created: super::Created::default(),
        }),
        QueryResult::Write { .. } => Err(Refusal::new(
            ErrorCode::Statement,
            "a prepared read cannot execute a write",
        )),
    }
}
