//! One CLI invocation, one native transaction, ordered reads and writes.
//!
//! This is an embedded driver, not a second transaction engine or a GQL
//! interpreter. Native preparation owns classification; WriteTxn owns the
//! overlay, identities, conflicts and completion. No intermediate result or
//! success event reaches stdout before successful completion. Rollback and
//! precommit errors discard both effects and buffered output. Issued identities
//! are never reclaimed, and ambiguous completion is never reported as abort.
//!
//! A --write step may contain one native CREATE/INSERT ... RETURN query,
//! including MATCH-selected or UNWIND-driven creation. Its rows are frozen at
//! that step, carry kind=write and statements=1, and share the transaction-wide
//! row/output allowances with --query results. All RETURN expressions and
//! output admission precede the sole completion boundary; LIMIT affects rows,
//! never creations. Other write steps retain their multi-statement programs.

use super::{
    Failure, Options, cell, execution_failure, human_value, parameter, policy, quoted, value_cell,
};
use asupersync::fs::Vfs;
use fgdb::{Database, NativeReadClass, PreparedNativeRead, QueryResult, QueryValue};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    BoundNativeGraphWrite, GqlParameters, GqlQueryPolicy, GraphWriteProgramPolicy,
    PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
};
use fgdb_types::{EmbeddedTxnCompletion, EmbeddedTxnState, PurposeContexts, QueryCx};
use std::io::Write;

pub(super) const MAX_STATEMENTS: usize = 64;
const MAX_INPUT_BYTES: usize = 1_048_576;

pub(super) struct Step {
    write: bool,
    text: String,
    pub(super) raw_params: Vec<(String, String)>,
}
impl Step {
    pub(super) fn new(write: bool, text: String) -> Self {
        Self {
            write,
            text,
            raw_params: Vec::new(),
        }
    }
}

pub(super) fn validate_input(steps: &[Step]) -> Result<(), Failure> {
    if steps.is_empty() || steps.len() > MAX_STATEMENTS {
        return Err(Failure::usage(
            "transaction requires 1..=64 --write/--query steps",
        ));
    }
    let mut bytes = 0usize;
    for step in steps {
        if step.text.trim().is_empty() {
            return Err(Failure::usage("transaction step must not be empty"));
        }
        for size in std::iter::once(step.text.len()).chain(
            step.raw_params
                .iter()
                .flat_map(|(name, value)| [name.len(), value.len()]),
        ) {
            bytes = bytes
                .checked_add(size)
                .ok_or_else(|| Failure::usage("transaction input limit exceeded"))?;
            if bytes > MAX_INPUT_BYTES {
                return Err(Failure::usage("transaction input exceeds 1 MiB"));
            }
        }
    }
    Ok(())
}

struct Limits {
    rows: u64,
    output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            rows: 100_000,
            output_bytes: 16 * 1024 * 1024,
        }
    }
}

enum PreparedStep {
    Read(Box<PreparedNativeRead>, GqlParameters),
    Write(Box<BoundNativeGraphWrite>),
    Returning(Box<PreparedGraphInsertQuery>),
}
fn prepare(
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    cx: &QueryCx,
) -> Result<(Vec<PreparedStep>, usize), Failure> {
    validate_input(&options.steps)?;
    let mut prepared = Vec::new();
    let mut statements = 0usize;
    for (index, step) in options.steps.iter().enumerate() {
        cx.checkpoint().map_err(Failure::query)?;
        let prepared_step = (|| {
            let mut params = GqlParameters::new();
            for (name, raw) in &step.raw_params {
                params
                    .insert(name, parameter(raw, resolver)?)
                    .map_err(Failure::query)?;
            }
            if step.write {
                // Native token framing, never substring matching or a failed
                // read retried as a write. Each step keeps its own argument map.
                if PreparedGraphInsertQueryText::has_return_clause(&step.text)
                    .map_err(Failure::query)?
                {
                    let declarations: Vec<_> = params.parameter_types().collect();
                    let query = PreparedGraphInsertQueryText::prepare_with_parameter_types(
                        &step.text,
                        options.coordinate,
                        &declarations,
                        |kind, name| options.resolve(kind, name),
                    )
                    .map_err(Failure::query)?
                    .bind_parameters(&params)
                    .map_err(Failure::query)?;
                    statements = statements
                        .checked_add(1)
                        .ok_or_else(|| Failure::usage("transaction statement count overflow"))?;
                    return Ok(PreparedStep::Returning(Box::new(query)));
                }
                let bound = BoundNativeGraphWrite::bind(
                    &step.text,
                    &params,
                    options.coordinate,
                    |kind, name| options.resolve(kind, name),
                )
                .map_err(Failure::query)?;
                // Count expanded UNWIND statements, not merely the one textual
                // clause, against this invocation's existing 64-statement cap.
                statements = statements
                    .checked_add(bound.program().statements().len())
                    .ok_or_else(|| Failure::usage("transaction statement count overflow"))?;
                Ok(PreparedStep::Write(Box::new(bound)))
            } else {
                let query = PreparedNativeRead::prepare(&step.text, &params, options)
                    .map_err(execution_failure)?;
                if matches!(
                    query.facade_class(),
                    NativeReadClass::TemporalPattern
                        | NativeReadClass::TemporalAggregate
                        | NativeReadClass::TemporalSet
                ) {
                    return Err(Failure::query(
                        "historical selectors have no staged transaction semantics",
                    ));
                }
                statements = statements
                    .checked_add(1)
                    .ok_or_else(|| Failure::usage("transaction statement count overflow"))?;
                Ok(PreparedStep::Read(Box::new(query), params))
            }
        })()
        .map_err(|error| at_step(index, error))?;
        if statements > MAX_STATEMENTS {
            return Err(Failure::usage("transaction exceeds 64 native statements"));
        }
        prepared.push(prepared_step);
    }
    Ok((prepared, statements))
}
fn at_step(index: usize, error: Failure) -> Failure {
    Failure::new(
        error.code,
        error.class,
        format!("transaction step {}: {}", index + 1, error.message),
    )
}

struct BufferedOutput {
    bytes: Vec<u8>,
    limit: usize,
}
impl BufferedOutput {
    fn line(&mut self, line: &str) -> Result<(), Failure> {
        let end = self
            .bytes
            .len()
            .checked_add(line.len())
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| Failure::query("transaction output counter overflow"))?;
        if end > self.limit {
            return Err(Failure::query("transaction encoded output limit exceeded"));
        }
        self.bytes.extend_from_slice(line.as_bytes());
        self.bytes.push(b'\n');
        Ok(())
    }
}

fn buffer_rows(
    output: &mut BufferedOutput,
    result: QueryResult,
    index: usize,
    basis: u64,
    robot: bool,
    cx: &QueryCx,
) -> Result<u64, Failure> {
    let QueryResult::Rows { columns, rows } = result else {
        return Err(Failure::query("transaction read returned a write receipt"));
    };
    let count =
        u64::try_from(rows.len()).map_err(|_| Failure::query("transaction row count overflow"))?;
    buffer_row_header(output, &columns, index, basis, robot, count, false)?;
    for row in rows {
        cx.checkpoint().map_err(Failure::query)?;
        let mut cells = Vec::with_capacity(row.len());
        for value in &row {
            cx.checkpoint().map_err(Failure::query)?;
            cells.push(if robot {
                cell(value)?
            } else {
                match value {
                    QueryValue::Value(value) => human_value(value)?,
                    QueryValue::Count(value) => value.to_string(),
                    QueryValue::Integer(value) => value.to_string(),
                    QueryValue::Average(value) => value.to_string(),
                }
            });
        }
        buffer_row(output, &cells, index, robot)?;
    }
    Ok(count)
}

// The two row-producing step types share framing and scalar encoding, not
// storage execution. Insertion rows are borrowed during encoding: they are
// never cloned into another complete QueryResult just to drive transport.
fn buffer_row_header(
    output: &mut BufferedOutput,
    columns: &[String],
    index: usize,
    basis: u64,
    robot: bool,
    count: u64,
    write: bool,
) -> Result<(), Failure> {
    let kind = if write { "write" } else { "query" };
    if robot {
        let statements = if write { ",\"statements\":1" } else { "" };
        output.line(&format!(
            r#"{{"v":1,"event":"statement","index":{index},"kind":"{kind}","view":"transaction_local","basis":{basis},"count":{count}{statements}}}"#,
        ))?;
        output.line(&format!(
            r#"{{"v":1,"event":"columns","statement":{index},"columns":[{}]}}"#,
            columns
                .iter()
                .map(|name| quoted(name))
                .collect::<Vec<_>>()
                .join(",")
        ))?;
    } else {
        output.line(&format!(
            "statement {index}: {kind} (transaction-local basis {basis})"
        ))?;
        output.line(
            &columns
                .iter()
                .map(|s| s.chars().flat_map(char::escape_default).collect::<String>())
                .collect::<Vec<_>>()
                .join("\t"),
        )?;
    }
    Ok(())
}

fn buffer_row(
    output: &mut BufferedOutput,
    cells: &[String],
    index: usize,
    robot: bool,
) -> Result<(), Failure> {
    if robot {
        output.line(&format!(
            r#"{{"v":1,"event":"row","statement":{index},"cells":[{}]}}"#,
            cells.join(",")
        ))
    } else {
        output.line(&cells.join("\t"))
    }
}

fn buffer_insert_rows(
    output: &mut BufferedOutput,
    columns: &[String],
    rows: Vec<GraphValueRow>,
    index: usize,
    basis: u64,
    robot: bool,
    cx: &QueryCx,
) -> Result<u64, Failure> {
    let count =
        u64::try_from(rows.len()).map_err(|_| Failure::query("transaction row count overflow"))?;
    buffer_row_header(output, columns, index, basis, robot, count, true)?;
    for row in rows {
        cx.checkpoint().map_err(Failure::query)?;
        let mut cells = Vec::with_capacity(row.values().len());
        for value in row.values() {
            cx.checkpoint().map_err(Failure::query)?;
            cells.push(if robot {
                value_cell(value)?
            } else {
                human_value(value)?
            });
        }
        buffer_row(output, &cells, index, robot)?;
    }
    Ok(count)
}

pub(super) async fn run<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    run_with_limits(
        db,
        contexts,
        options,
        resolver,
        robot,
        out,
        Limits::default(),
        None,
    )
    .await
}

// Private limits/fault inputs let the same production driver prove refusal and
// completion behavior without an alternate transaction or mocked commit path.
#[allow(clippy::too_many_arguments)]
async fn run_with_limits<V: Vfs + Clone>(
    db: &mut Database<V>,
    contexts: &PurposeContexts,
    options: &Options,
    resolver: Option<&fgdb::PinnedTzdb>,
    robot: bool,
    out: &mut impl Write,
    limits: Limits,
    crash: Option<fgdb::CrashPoint>,
) -> Result<(), Failure> {
    let cx = contexts.query();
    let (steps, statements) = prepare(options, resolver, &cx)?;
    let mut txn = db.begin(&contexts.txn()).map_err(execution_failure)?;
    let basis = txn.basis().0;
    let mut output = BufferedOutput {
        bytes: Vec::new(),
        limit: limits.output_bytes,
    };
    let mut count = 0u64;
    let staged = (|| {
        for (index, step) in steps.iter().enumerate() {
            let result = (|| {
                cx.checkpoint().map_err(Failure::query)?;
                match step {
                    PreparedStep::Read(query, params) => {
                        let remaining = limits
                            .rows
                            .checked_sub(count)
                            .ok_or_else(|| Failure::query("transaction row limit exceeded"))?;
                        // Source/work/scratch bounds are per native operation;
                        // the delivered row allowance spans the entire transaction.
                        let allowance = GqlQueryPolicy {
                            rows: fgdb_gql::GqlExecutionBudget::new(100_000, remaining),
                            ..policy()
                        };
                        let rows = query
                            .execute_in_transaction(&txn, db, &cx, params, allowance)
                            .map_err(execution_failure)?;
                        let added = buffer_rows(&mut output, rows, index + 1, basis, robot, &cx)?;
                        count = count
                            .checked_add(added)
                            .filter(|n| *n <= limits.rows)
                            .ok_or_else(|| Failure::query("transaction row limit exceeded"))?;
                    }
                    PreparedStep::Returning(query) => {
                        let remaining = limits
                            .rows
                            .checked_sub(count)
                            .ok_or_else(|| Failure::query("transaction row limit exceeded"))?;
                        // Only final RETURN rows consume this allowance; source
                        // occurrences/creations retain native insertion limits.
                        let allowance = GqlQueryPolicy {
                            rows: fgdb_gql::GqlExecutionBudget::new(100_000, remaining),
                            ..policy()
                        };
                        let (_, rows) = txn
                            .execute_graph_insert_query_engine_governed(
                                db,
                                &cx,
                                query,
                                GraphInsertPolicy::new(allowance, 100_000, 100_000),
                            )
                            .map_err(execution_failure)?;
                        let added = buffer_insert_rows(
                            &mut output,
                            query.columns(),
                            rows.value,
                            index + 1,
                            basis,
                            robot,
                            &cx,
                        )?;
                        count = count
                            .checked_add(added)
                            .filter(|n| *n <= limits.rows)
                            .ok_or_else(|| Failure::query("transaction row limit exceeded"))?;
                    }
                    PreparedStep::Write(bound) => {
                        let stats = txn
                            .execute_graph_write_program_engine_governed(
                                db,
                                &cx,
                                bound.program(),
                                GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
                            )
                            .map_err(|error| execution_failure(bound.execution_error(error)))?;
                        if robot {
                            output.line(&format!(
                                r#"{{"v":1,"event":"statement","index":{},"kind":"write","view":"transaction_local","basis":{basis},"statements":{}}}"#,
                                index + 1, stats.completed_statements,
                            ))?;
                        } else {
                            output.line(&format!(
                                "statement {}: {} staged write statement(s)",
                                index + 1,
                                stats.completed_statements
                            ))?;
                        }
                    }
                }
                Ok(())
            })();
            result.map_err(|error| at_step(index, error))?;
        }
        cx.checkpoint().map_err(Failure::query)
    })();
    if let Err(error) = staged {
        txn.abort();
        return Err(error);
    }
    let (kind, seq) = if options.rollback {
        txn.abort();
        // Preview rows are not ordinary successful results of a rolled-back
        // transaction. Do not release them or any provisional write receipts.
        output.bytes.clear();
        count = 0;
        ("rolled_back", basis)
    } else {
        match txn.finish_with_crash(db, &contexts.commit(), crash).await {
            Ok(EmbeddedTxnCompletion::WriteCommitted { commit_seq }) => ("committed", commit_seq.0),
            Ok(EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. }) => {
                ("read_closed", snapshot_seq.0)
            }
            Err(error) => {
                // The native completion guard owns cleanup. In particular,
                // do NOT reinterpret an ambiguous or durable outcome as abort.
                return Err(match txn.state() {
                    EmbeddedTxnState::CommitOutcomeUnknown { .. } => Failure::io(format!(
                        "transaction outcome unknown; reopen and resolve before retrying: {error}"
                    )),
                    EmbeddedTxnState::CommittedNeedsRecovery { commit_seq } => {
                        Failure::io(format!(
                            "transaction committed at seq {}; recovery required: {error}",
                            commit_seq.0
                        ))
                    }
                    _ => execution_failure(error),
                });
            }
        }
    };
    // There is no cancellation point between accepted completion and reporting.
    // Output can still fail: it does not undo a durable commit or justify retry.
    let summary = if robot {
        format!(
            r#"{{"v":1,"event":"result","kind":"{kind}","basis":{basis},"seq":{seq},"count":{count},"statements":{statements}}}"#
        )
    } else {
        format!("transaction {kind} (basis {basis}, seq {seq}, {statements} statements)")
    };
    out.write_all(&output.bytes)
        .and_then(|()| writeln!(out, "{summary}"))
        .and_then(|()| out.flush())
        .map_err(|error| {
            Failure::io(format!(
                "transaction {kind} at seq {seq}, but output failed; do not blindly retry: {error}"
            ))
        })
}

#[cfg(test)]
mod returning_tests;
#[cfg(test)]
mod tests;
