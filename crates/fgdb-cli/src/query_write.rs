//! `write --from-query`: one frozen native read feeds one atomic script batch.
//!
//! CLI parameters belong to the read; projection names supply script arguments.
//! Neither source values nor identities are interpolated into statement text.
//! The native query-write adapter owns admission, cumulative quotas, allocation,
//! rollback and completion. This module owns flags, preparation and reporting.

use super::{Failure, Options, decimal_usize, emit, execution_failure};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_gql::query_write::PreparedGraphQueryWrite;
use fgdb_gql::{
    GqlParameterType, GqlParameterValue, GqlParameters, GraphWriteProgramPolicy,
    PreparedGraphSetText, PreparedGraphWriteScript,
};
use fgdb_types::{CanonicalScalarKind, EmbeddedTxnCompletion, PurposeContexts};
use std::io::Write;

pub(super) struct QueryWriteOptions {
    source: Option<String>,
    declarations: Vec<(String, GqlParameterType)>,
    // Reuse native name, duplicate, count and transcript admission; these dummy
    // values validate declarations only and never become source/write arguments.
    names: GqlParameters,
    max_statements: Option<usize>,
}

impl Default for QueryWriteOptions {
    fn default() -> Self {
        Self {
            source: None,
            declarations: Vec::new(),
            names: GqlParameters::new(),
            max_statements: None,
        }
    }
}

impl QueryWriteOptions {
    pub(super) const FLAGS: &[&str] =
        &["--from-query", "--feed-type", "--max-batch-statements"];

    pub(super) fn enabled(&self) -> bool {
        self.source.is_some()
    }

    pub(super) fn set(&mut self, flag: &str, raw: &str) -> Result<(), Failure> {
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
                    _ => return Err(Failure::usage("unknown --feed-type scalar type")),
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
        Ok(())
    }

    pub(super) fn validate(&self, has_rows: bool) -> Result<(), Failure> {
        if !self.enabled() && (!self.declarations.is_empty() || self.max_statements.is_some()) {
            return Err(Failure::usage(
                "--feed-type and --max-batch-statements require --from-query",
            ));
        }
        if self.enabled() && has_rows {
            return Err(Failure::usage("--from-query cannot be combined with --rows"));
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

/// Prepare both native definitions before opening writable graph storage.
/// Actual source rows necessarily bind later, inside the same transaction as
/// their writes. No failed definition is retried as an ordinary write script.
pub(super) fn prepare(options: &Options) -> Result<Option<PreparedQueryWrite>, Failure> {
    let config = &options.query_write;
    config.validate(options.rows.is_some())?;
    let Some(source) = config.source.as_deref() else {
        return Ok(None);
    };
    // Preserve full resolver metadata (reverse names and pinned time zones),
    // not a closure that supplies only forward symbol resolution.
    let mut resolver = options;
    let source_types: Vec<_> = options.params.parameter_types().collect();
    let source = PreparedGraphSetText::prepare_with_parameter_types_and_resolver(
        source,
        &source_types,
        &mut resolver,
    )
    .map_err(Failure::query)?
    .bind_parameters(&options.params)
    .map_err(Failure::query)?;
    let declarations: Vec<_> = config
        .declarations
        .iter()
        .map(|(name, kind)| (name.as_str(), *kind))
        .collect();
    let script = PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
        &options.text,
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
