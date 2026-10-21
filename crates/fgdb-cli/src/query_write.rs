//! `write --from-query`: one frozen native read feeds one atomic script batch.
//!
//! CLI parameters belong to the read; projection names supply script arguments.
//! Neither source values nor identities are interpolated into statement text.
//! The native query-write adapter owns admission, cumulative quotas, allocation,
//! rollback and completion. This module owns flags, preparation and reporting.

use super::{Failure, Options, decimal_usize, emit, execution_failure};
use asupersync::fs::Vfs;
use fgdb::{Database, WriteTxn};
use fgdb_gql::query_write::{GraphQueryWriteExecution, PreparedGraphQueryWrite};
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GraphWriteProgramPolicy,
    PreparedGraphSetText, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalarKind, EmbeddedTxnCompletion, PurposeContexts, QueryCx};
use std::io::Write;

pub(super) struct QueryWriteOptions {
    source: Option<String>,
    declarations: Vec<(String, GqlParameterType)>,
    // Reuse native name, duplicate, count and transcript admission; these dummy
    // values validate declarations only and never become source/write arguments.
    names: GqlParameters,
    max_statements: Option<usize>,
    input_bytes: usize,
}

impl Default for QueryWriteOptions {
    fn default() -> Self {
        Self {
            source: None,
            declarations: Vec::new(),
            names: GqlParameters::new(),
            max_statements: None,
            input_bytes: 0,
        }
    }
}

impl QueryWriteOptions {
    pub(super) const FLAGS: &[&str] = &["--from-query", "--feed-type", "--max-batch-statements"];

    pub(super) fn enabled(&self) -> bool {
        self.source.is_some()
    }

    /// Original flag values, including type spellings and decimal limits.
    /// Ordered transactions include these bytes in their whole-input cap.
    pub(super) const fn input_bytes(&self) -> usize {
        self.input_bytes
    }

    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
        let input_bytes = self
            .input_bytes
            .checked_add(raw.len())
            .ok_or_else(|| Failure::usage("query-fed input size overflow"))?;
        match flag {
            "--from-query" => {
                if self.source.is_some() {
                    return Err(Failure::usage("--from-query must be supplied only once"));
                }
                if raw.len() > fgdb_gql::MAX_GRAPH_TEXT_BYTES {
                    return Err(Failure::usage("--from-query exceeds the native text limit"));
                }
                self.source = Some(raw.to_owned());
            }
            "--feed-type" => {
                let (name, kind) = raw
                    .split_once('=')
                    .ok_or_else(|| Failure::usage("expected --feed-type name=type"))?;
                let kind = match kind {
                    "int64" => GqlParameterType::Int64,
                    "uint64" => GqlParameterType::UInt64,
                    "int" => GqlParameterType::Scalar(CanonicalScalarKind::Int),
                    "text" => GqlParameterType::Scalar(CanonicalScalarKind::Text),
                    "bool" => GqlParameterType::Scalar(CanonicalScalarKind::Bool),
                    "null" => GqlParameterType::Scalar(CanonicalScalarKind::Null),
                    "float" => GqlParameterType::Scalar(CanonicalScalarKind::Float),
                    "decimal" => GqlParameterType::Scalar(CanonicalScalarKind::Decimal),
                    "bytes" => GqlParameterType::Scalar(CanonicalScalarKind::Bytes),
                    "list" => GqlParameterType::List,
                    "map" => GqlParameterType::Map,
                    _ => return Err(Failure::usage("unknown --feed-type parameter type")),
                };
                self.names
                    .insert(name, GqlParameterValue::Int64(0))
                    .map_err(Failure::usage)?;
                self.declarations.push((name.to_owned(), kind));
            }
            "--max-batch-statements" => {
                if self.max_statements.is_some() {
                    return Err(Failure::usage(
                        "--max-batch-statements must be supplied only once",
                    ));
                }
                let value = decimal_usize(flag, raw)?;
                if value > PreparedGraphWriteScript::MAX_BATCH_STATEMENTS {
                    return Err(Failure::usage(format!(
                        "--max-batch-statements exceeds native ceiling {}",
                        PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
                    )));
                }
                // Zero is meaningful: an empty source can still close read-only.
                self.max_statements = Some(value);
            }
            _ => return Err(Failure::usage("unknown query-fed write flag")),
        }
        self.input_bytes = input_bytes;
        Ok(())
    }

    pub(super) fn validate(&self, has_rows: bool) -> Result<(), Failure> {
        if !self.enabled() && (!self.declarations.is_empty() || self.max_statements.is_some()) {
            return Err(Failure::usage(
                "--feed-type and --max-batch-statements require --from-query",
            ));
        }
        if self.enabled() && has_rows {
            return Err(Failure::usage(
                "--from-query cannot be combined with --rows",
            ));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct PreparedQueryWrite {
    query: PreparedGraphQueryWrite,
    policy: GraphWriteProgramPolicy,
    max_statements: usize,
}

impl PreparedQueryWrite {
    /// Stage in the caller's transaction, never an inner autocommit. The
    /// remaining invocation capacity narrows, but never widens, the caller's
    /// expanded-statement cap. Source and writes keep one native allowance.
    pub(super) fn execute_in_transaction<V: Vfs + Clone>(
        &self,
        transaction: &mut WriteTxn,
        database: &mut Database<V>,
        cx: &QueryCx,
        remaining_statements: usize,
    ) -> Result<GraphQueryWriteExecution, Failure> {
        transaction
            .execute_graph_query_write_engine_governed(
                database,
                cx,
                &self.query,
                self.policy,
                self.max_statements.min(remaining_statements),
            )
            .map_err(execution_failure)
    }
}

/// Prepare both native definitions before opening writable graph storage.
/// Actual source rows necessarily bind later, inside the same transaction as
/// their writes. No failed definition is retried as an ordinary write script.
pub(super) fn prepare(options: &Options) -> Result<Option<PreparedQueryWrite>, Failure> {
    options.query_write.validate(options.rows.is_some())?;
    prepare_statement(&options.text, &options.params, &options.query_write, options)
}

/// Each ordered write step owns both definitions, its source parameters and
/// its script declarations. Only the catalog and execution limits are shared.
pub(super) fn prepare_statement(
    statement: &str,
    params: &GqlParameters,
    config: &QueryWriteOptions,
    options: &Options,
) -> Result<Option<PreparedQueryWrite>, Failure> {
    config.validate(false)?;
    let Some(source) = config.source.as_deref() else {
        return Ok(None);
    };
    // Preserve full resolver metadata (reverse names and pinned time zones),
    // not a closure that supplies only forward symbol resolution.
    let mut resolver = options;
    let source_types: Vec<_> = params.parameter_types().collect();
    let source = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        source,
        &source_types,
        &mut resolver,
    )
    .map_err(Failure::query)?
    .bind_parameters(params)
    .map_err(Failure::query)?;
    let declarations: Vec<_> = config
        .declarations
        .iter()
        .map(|(name, kind)| (name.as_str(), *kind))
        .collect();
    let script = PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
        statement,
        options.coordinate,
        &declarations,
        &mut resolver,
    )
    .map_err(Failure::query)?;
    let query = PreparedGraphQueryWrite::prepare(source, script).map_err(Failure::query)?;
    Ok(Some(PreparedQueryWrite {
        query,
        // The same effect and creation caps as ordinary `write`; all native
        // source/binding/write costs spend one query allowance, never per row.
        policy: GraphWriteProgramPolicy::new(options.budget.policy(), 100_000, 100_000, 100_000),
        max_statements: config
            .max_statements
            .unwrap_or(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS),
    }))
}

pub(super) async fn run<V: Vfs + Clone>(
    database: &mut Database<V>,
    contexts: &PurposeContexts,
    prepared: PreparedQueryWrite,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let (execution, completion) = database
        .execute_graph_query_write_autocommit_engine_governed(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            &prepared.query,
            prepared.policy,
            prepared.max_statements,
        )
        .await
        .map_err(execution_failure)?;
    let seq = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0,
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0,
    };
    let records = execution.input_rows;
    let statements = execution.stats.completed_statements;
    // Completion is final. From here only transport may fail; no conversion,
    // cancellation or quota check may relabel the accepted write as rollback.
    // These are existing result fields/kinds, not a new robot event protocol.
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"records":{records},"statements":{statements}}}"#,
            ),
        )
    } else {
        writeln!(
            out,
            "wrote {records} source row(s), {statements} statement(s) at seq {seq}",
        )
        .map_err(Failure::io)
    }
}

#[cfg(test)]
mod tests;
