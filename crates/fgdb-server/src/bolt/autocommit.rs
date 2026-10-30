//! Deferred autocommit writes over Bolt's existing capability boundary.
//!
//! RUN retains one bounded request, never a transaction or database lock.
//! PULL and DISCARD consume it before awaiting the native atomic writer;
//! RESET, GOODBYE and disconnect before consumption abandon it without effects.
//! A lost terminal response is not evidence of rollback and is not replayed.

use super::*;
use crate::execute::{Returning, ReturningTemplate};
use fgdb_gql::{BoundNativeGraphWrite, GraphSymbol, PreparedGraphWriteChainText};
use fgdb_protocol::body::{Body, Execute, ExecuteMode, Outcome};
use fgdb_warden::{PlannerPredicates, Rights};

#[cfg(test)]
mod tests;

/// The request owns values, not interpolated source or client-chosen identities.
/// There is deliberately no Clone or payload-bearing Debug implementation.
pub(super) struct DeferredWrite {
    request: WriteRequest,
    minimum_seq: CommitSeq,
}

enum WriteRequest {
    Program(Execute),
    Returning {
        definition: Returning,
        // RUN's definition/binding debit narrows the execution allowance.
        // The native owner still checks live authority at consumption.
        token: CapabilityToken,
    },
}

pub(super) enum PendingWrite {
    Deferred(DeferredWrite),
    // Rows were admitted before the native commit. Partial delivery retains
    // this exact summary and can never re-enter the deferred execution path.
    Completed(Map),
}

// Bound the additional owned Bolt representation, including container
// capacities. This is a retained-result limit, not a process RSS claim.
const MAX_RETURN_BYTES: usize = 64 * 1024 * 1024;

fn output_budget() -> Refusal {
    Refusal::new(
        ErrorCode::Budget,
        "Bolt write RETURN exceeds the output limit",
    )
}

fn output_work(
    work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
    units: u64,
) -> Result<(), Refusal> {
    work(units).map_err(|error| crate::execute::write_refusal(&error))
}

fn output_bytes(total: &mut usize, bytes: usize) -> Result<(), Refusal> {
    *total = total
        .checked_add(bytes)
        .filter(|total| *total <= MAX_RETURN_BYTES)
        .ok_or_else(output_budget)?;
    Ok(())
}

fn output_slots<T>(total: &mut usize, count: usize) -> Result<(), Refusal> {
    output_bytes(
        total,
        count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(output_budget)?,
    )
}

/// Reserve conversion work before visiting or copying a nested cell. Graph
/// identities need a transaction-scoped hydration owner; a later read session
/// must never turn a committed write into a conversion failure.
fn admit_return_value(
    value: &WireValue,
    depth: usize,
    total: &mut usize,
    work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
) -> Result<(), Refusal> {
    output_work(work, 1)?;
    if depth > fgdb_bolt::packstream::MAX_DEPTH {
        return Err(output_budget());
    }
    let bytes = match value {
        WireValue::Vertex(_)
        | WireValue::Edge(_)
        | WireValue::Vertices(_)
        | WireValue::Edges(_)
        | WireValue::Path { .. } => {
            return Err(Refusal::new(
                ErrorCode::Statement,
                "Bolt write RETURN supports values; return element properties instead of graph elements",
            ));
        }
        WireValue::List(values) => {
            output_slots::<Value>(total, values.len())?;
            for value in values {
                admit_return_value(value, depth + 1, total, work)?;
            }
            0
        }
        WireValue::Map(entries) => {
            output_slots::<(String, Value)>(total, entries.len())?;
            for (key, value) in entries {
                output_work(work, key.len().div_ceil(64) as u64 + 1)?;
                output_bytes(total, key.capacity())?;
                admit_return_value(value, depth + 1, total, work)?;
            }
            0
        }
        WireValue::Text(text) | WireValue::Decimal(text) => {
            output_bytes(total, text.capacity())?;
            text.len()
        }
        WireValue::Date(text) | WireValue::LocalTime(text) => {
            output_slots::<Value>(total, 1)?;
            text.len()
        }
        WireValue::Time(text) | WireValue::LocalDateTime(text) => {
            output_slots::<Value>(total, 2)?;
            text.len()
        }
        WireValue::Duration(text) => {
            output_slots::<Value>(total, 4)?;
            text.len()
        }
        WireValue::Bytes(bytes) => {
            output_bytes(total, bytes.capacity())?;
            bytes.len()
        }
        WireValue::Timestamp(timestamp) => {
            output_slots::<Value>(total, 3)?;
            if let Some(zone) = &timestamp.zone {
                output_bytes(total, zone.identifier.capacity())?;
                zone.identifier.len()
            } else {
                0
            }
        }
        WireValue::Count(value) => {
            if i64::try_from(*value).is_err() {
                output_bytes(total, 20)?;
            }
            0
        }
        WireValue::WideInt(value) => {
            if i64::try_from(*value).is_err() {
                output_bytes(total, 40)?;
            }
            0
        }
        WireValue::Null
        | WireValue::Bool(_)
        | WireValue::Int(_)
        | WireValue::Float(_)
        | WireValue::Average { .. } => 0,
    };
    output_work(work, bytes.div_ceil(64) as u64)
}

/// Container lengths were admitted before this conversion. Allocate those
/// exact lengths explicitly; a fallible collect must not grow an unreserved
/// destination or retain the larger wire cell allocation.
fn convert_return_value(cell: WireValue, entities: &Entities) -> Result<Value, Failure> {
    match cell {
        WireValue::List(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(convert_return_value(value, entities)?);
            }
            Ok(Value::List(output))
        }
        WireValue::Map(entries) => {
            let mut output = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                output.push((key, convert_return_value(value, entities)?));
            }
            Ok(Value::Map(output))
        }
        scalar => value(scalar, entities),
    }
}

/// Inline value slots belong to their enclosing container. Count all owned
/// payload capacities, including the structures temporal conversion produces.
fn retained_value(
    value: &Value,
    depth: usize,
    total: &mut usize,
    work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
) -> Result<(), Refusal> {
    output_work(work, 1)?;
    if depth > fgdb_bolt::packstream::MAX_DEPTH {
        return Err(output_budget());
    }
    match value {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Float(_) => Ok(()),
        Value::Bytes(bytes) => output_bytes(total, bytes.capacity()),
        Value::String(text) => output_bytes(total, text.capacity()),
        Value::List(values) | Value::Struct { fields: values, .. } => {
            output_slots::<Value>(total, values.capacity())?;
            for value in values {
                retained_value(value, depth + 1, total, work)?;
            }
            Ok(())
        }
        Value::Map(entries) => {
            output_slots::<(String, Value)>(total, entries.capacity())?;
            for (key, value) in entries {
                output_bytes(total, key.capacity())?;
                retained_value(value, depth + 1, total, work)?;
            }
            Ok(())
        }
    }
}

/// Convert the complete result while the native rollback workspace and signed
/// permit are live. Even PULL 1 or DISCARD must admit every later row first.
fn collect_returning(
    rows: Vec<Vec<WireValue>>,
    max_frame_len: usize,
    work: &mut dyn FnMut(u64) -> Result<(), fgdb::WriteTxnError>,
) -> Result<VecDeque<Vec<Value>>, Refusal> {
    output_work(work, 1)?;
    let mut reserved = 0;
    output_slots::<Vec<Value>>(&mut reserved, rows.len())?;
    let mut output = VecDeque::with_capacity(rows.len());
    let mut retained = 0;
    output_slots::<Vec<Value>>(&mut retained, output.capacity())?;
    let entities = Entities::default();
    for row in rows {
        output_work(work, 1)?;
        let mut reserved = retained;
        output_slots::<Value>(&mut reserved, row.len())?;
        for cell in &row {
            // The record structure and its field list occupy depths 0 and 1.
            admit_return_value(cell, 2, &mut reserved, work)?;
        }
        let mut cells = Vec::with_capacity(row.len());
        for cell in row {
            cells.push(
                convert_return_value(cell, &entities)
                    .map_err(|error| Refusal::new(ErrorCode::Statement, error.message))?,
            );
        }
        let before = retained;
        output_slots::<Value>(&mut retained, cells.capacity())?;
        for cell in &cells {
            retained_value(cell, 2, &mut retained, work)?;
        }
        // Owned slots/payloads bound this row's encoding size, including the
        // record and list headers. Reserve codec work before its allocation.
        let bound = retained - before + 7;
        output_work(work, bound.div_ceil(64) as u64)?;
        let row = Value::List(cells);
        let mut encoded = Vec::new();
        fgdb_bolt::packstream::encode(&row, &mut encoded);
        if encoded
            .len()
            .checked_add(2)
            .is_none_or(|bytes| bytes > max_frame_len)
        {
            return Err(output_budget());
        }
        let Value::List(cells) = row else {
            unreachable!("constructed list")
        };
        output.push_back(cells);
    }
    Ok(output)
}

/// Validate metadata whose meaning matters to a write before retaining it.
/// A local bookmark is a lower bound, checked under the native write lock.
/// Foreign bookmarks, impersonation and unsupported timeouts never silently
/// turn into a write with weaker semantics.
fn write_options(extra: &Map, database: &str) -> Result<CommitSeq, Failure> {
    let options = causal::options(extra, database)?;
    if options.mode == causal::AccessMode::Read {
        return Err(read_only());
    }
    Ok(options.minimum_seq)
}

/// The old keyword detector is only a hint, never execution authority. A
/// valid native read wins even when an alias, label or comment spells CREATE.
/// Branch selectors use the same native owner as normal read preparation.
fn is_native_read(db: &Served, text: &str, parameters: &GqlParameters) -> bool {
    let Ok(selector) = PreparedGraphBranchText::prepare(text) else {
        return false;
    };
    let Ok(selected) = selector.bind_parameters(parameters) else {
        return false;
    };
    PreparedNativeRead::prepare(
        selected.statement(),
        selected.parameters(),
        db.symbols.clone(),
    )
    .is_ok()
}

/// Admission probes native definitions, not expanded rows. The resolver
/// exposes no catalog name outside the signed scopes. The complete native
/// binder runs again at consumption, under its cooperative execution budget.
fn is_native_write(
    db: &Served,
    text: &str,
    parameters: &GqlParameters,
    scope: &PlannerPredicates,
) -> bool {
    let resolve = |kind, name: &str| {
        db.symbols
            .resolve(kind, name)
            .filter(|symbol| match symbol {
                GraphSymbol::Label(label) => scope.allows_label(*label),
                GraphSymbol::Relation(relation) => scope.allows_relation(*relation),
                GraphSymbol::Property(property) => scope.allows_property(*property),
            })
    };
    if PreparedGraphWriteChainText::is_chain(text).unwrap_or(false) {
        // Preparing the definition does not expand parameter rows or allocate
        // identities. PULL/DISCARD later binds the retained definition.
        // Execution uses one signed permit and the queued generation fence.
        // Map arguments splice as the executor's ReturningTemplate splices
        // them (fgdb-0s2pq), so both classify the same statement.
        let lowered = fgdb_gql::GqlMapArgumentText::lower(text, parameters);
        let (text, parameters) = lowered
            .as_ref()
            .map_or((text, parameters), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let declarations: Vec<_> = parameters.parameter_types().collect();
        return PreparedGraphWriteChainText::prepare_with_parameter_types(
            text,
            db.write_relation,
            &declarations,
            resolve,
        )
        .is_ok_and(|chain| !chain.returns_rows());
    }
    BoundNativeGraphWrite::probe_definition(text, parameters, db.write_relation, resolve).is_ok()
}

impl<'s> Connection<'s> {
    /// No graph access, identity allocation or transaction opening occurs here.
    /// RETURN binds its immutable native definition to obtain the output schema;
    /// its preparation debit follows it into the checked execution owner. Only
    /// PULL/DISCARD evaluates the statement or publishes its admitted effects.
    pub(super) fn queue_autocommit_write(
        &mut self,
        cx: &Cx,
        query: &str,
        arguments: &Map,
        parameters: &GqlParameters,
        extra: &Map,
    ) -> Result<Option<Map>, Failure> {
        if self.transaction.is_some() || !looks_like_write(query) {
            return Ok(None);
        }
        let (name, db) = self.database(extra)?;
        self.output_database = Some(db);
        let token = self.token.as_ref().expect("authenticated");
        let capability = db
            .authority
            .verify_at(token, crate::TRUNK, crate::unix_millis())
            .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
        if is_native_read(db, query, parameters) {
            return Ok(None);
        }
        let program = is_native_write(db, query, parameters, capability.predicates());
        let minimum_seq = write_options(extra, &name)?;
        if !matches!(
            capability.predicates().rights(),
            Rights::Write | Rights::ReadWrite
        ) {
            return Err(read_only());
        }
        // The ordinary body encoder enforces its complete size/name/value
        // contract even though Bolt does not put this request on an FGP wire.
        let statement = Execute {
            mode: ExecuteMode::Write,
            statement: query.to_owned(),
            parameters: wire_parameters(arguments)?,
        };
        statement
            .encode()
            .map_err(|error| Failure::invalid(error.to_string()))?;
        let generation = db
            .db
            .generation()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        let (request, columns) = if program {
            (WriteRequest::Program(statement), Vec::new())
        } else {
            let contexts = PurposeContexts::narrow_runtime_root(cx);
            let query_cx = crate::query_cx(&contexts);
            let now = crate::unix_millis();
            let mut permit = capability
                .begin_write_at(crate::TRUNK, now)
                .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
            permit
                .charge_work_at(now, query.len() as u64 + parameters.len() as u64 + 1)
                .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
            query_cx
                .checkpoint()
                .map_err(|_| Failure::invalid("Bolt write preparation cancelled"))?;
            let definition = query_cx.with_restriction(|| {
                ReturningTemplate::prepare(db, query, parameters)?
                    .filter(ReturningTemplate::returns_rows)
                    .map(|template| template.bind(parameters))
                    .transpose()
            });
            permit
                .checkpoint_at(crate::unix_millis())
                .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
            query_cx
                .checkpoint()
                .map_err(|_| Failure::invalid("Bolt write preparation cancelled"))?;
            let Some(definition) = definition.map_err(Failure::from_refusal)? else {
                return Ok(None);
            };
            if db.max_statements == 0 {
                return Err(Failure::new(
                    "Neo.ClientError.Statement.ExecutionFailed",
                    "the host write statement limit is zero",
                ));
            }
            let remaining = permit.predicates().limits().max_work - permit.usage().work;
            let token = token
                .attenuate(fgdb_warden::Restriction::MaxWork(remaining))
                .map_err(|error| Failure::from_refusal(crate::execute::write_refusal(&error)))?;
            let columns = definition.columns().to_vec();
            (WriteRequest::Returning { definition, token }, columns)
        };
        generation
            .check()
            .map_err(|error| Failure::from_refusal(error.into()))?;
        self.output_generation = Some(generation.clone());
        self.pending = Some(Pending {
            rows: VecDeque::new(),
            database: name,
            db,
            seq: None,
            generation,
            write: Some(PendingWrite::Deferred(DeferredWrite {
                request,
                minimum_seq,
            })),
        });
        Ok(Some(vec![
            (
                "fields".to_owned(),
                Value::List(columns.into_iter().map(Value::String).collect()),
            ),
            ("t_first".to_owned(), Value::Int(0)),
        ]))
    }

    /// The owner is removed before the first await: an error, cancellation or
    /// lost response cannot leave a request that a second PULL can execute.
    pub(super) async fn finish_autocommit_write(&mut self, cx: &Cx) -> Result<(), Failure> {
        let mut pending = self.pending.take().expect("pending write checked by PULL");
        let Some(PendingWrite::Deferred(deferred)) = pending.write.take() else {
            return Err(Failure::invalid("write was already consumed"));
        };
        self.output_database = Some(pending.db);
        self.output_generation = Some(pending.generation.clone());
        let max_frame_len = self.server.limits.max_frame_len as usize;
        let answer = match deferred.request {
            WriteRequest::Program(statement) => {
                let answer = crate::execute::write_no_return_at(
                    cx,
                    pending.db,
                    self.token.as_ref().expect("authenticated"),
                    &statement,
                    &pending.generation,
                    deferred.minimum_seq,
                )
                .await
                .map_err(Failure::from_refusal)?;
                crate::execute::Answer {
                    columns: answer.columns,
                    rows: VecDeque::new(),
                    outcome: answer.outcome,
                    generation: answer.generation,
                    created: answer.created,
                }
            }
            WriteRequest::Returning { definition, token } => {
                let execution: crate::execute::Execution<'_, VecDeque<Vec<Value>>> =
                    Box::pin(crate::execute::write_returning_checked(
                        cx,
                        pending.db,
                        &token,
                        definition,
                        Some((&pending.generation, deferred.minimum_seq)),
                        move |_, rows, work| collect_returning(rows, max_frame_len, work),
                    ));
                execution.await.map_err(Failure::from_refusal)?
            }
        };
        self.output_generation = Some(answer.generation.clone());
        let committed = matches!(answer.outcome, Outcome::WriteCommitted { .. });
        let (seq, statements) = match answer.outcome {
            Outcome::WriteCommitted { seq, statements }
            | Outcome::ReadClosed { seq, statements } => (CommitSeq(seq), statements),
            _ => {
                // Never translate an unexpected post-execution answer into a
                // retryable refusal or suggest that a commit rolled back.
                return Err(Failure::new(
                    "Neo.DatabaseError.Statement.ExecutionFailed",
                    "unexpected native write completion; do not replay automatically",
                ));
            }
        };
        let unrepresentable = || {
            Failure::new(
                "Neo.DatabaseError.Statement.ExecutionFailed",
                "write completion cannot be represented; do not replay automatically",
            )
        };
        let statements = i64::try_from(statements).map_err(|_| unrepresentable())?;
        // Native counters and the completion outcome own this summary.
        let mut stats = vec![("contains-updates".to_owned(), Value::Bool(committed))];
        for (key, count) in [
            ("nodes-created", answer.created.vertices),
            ("relationships-created", answer.created.edges),
        ] {
            if count != 0 {
                let count = i64::try_from(count).map_err(|_| unrepresentable())?;
                stats.push((key.to_owned(), Value::Int(count)));
            }
        }
        let summary = vec![
            ("type".to_owned(), Value::string("w")),
            ("t_last".to_owned(), Value::Int(0)),
            ("db".to_owned(), Value::string(pending.database.clone())),
            (
                "bookmark".to_owned(),
                Value::string(bookmark(&pending.database, seq)),
            ),
            ("stats".to_owned(), Value::Map(stats)),
            ("fgdb_statements".to_owned(), Value::Int(statements)),
        ];
        pending.rows = answer.rows;
        pending.seq = Some(seq);
        pending.generation = answer.generation;
        pending.write = Some(PendingWrite::Completed(summary));
        self.pending = Some(pending);
        Ok(())
    }
}
