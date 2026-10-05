//! CLI binding for the native map-row UNWIND adapter. Preparation runs before
//! opening the database; execution is the same governed atomic program path as
//! plain writes and --rows. No success frame is emitted before completion.

use super::super::{Failure, Options, emit, execution_failure, policy};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_gql::unwind_write::GraphUnwindWriteText;
use fgdb_gql::{BoundGraphWriteScriptBatch, GraphWriteProgramPolicy, PreparedGraphWriteScript};
use fgdb_types::{EmbeddedTxnCompletion, PurposeContexts};
use std::io::Write;

pub(super) fn prepare(options: &Options) -> Result<Option<BoundGraphWriteScriptBatch>, Failure> {
    let Some(text) =
        GraphUnwindWriteText::parse_if_supported(&options.text).map_err(Failure::query)?
    else {
        return Ok(None);
    };
    text.bind_with_limit(
        &options.params,
        options.coordinate,
        PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
        |kind, name| options.resolve(kind, name),
    )
    .map(Some)
    .map_err(Failure::query)
}

pub(super) async fn run<V: Vfs + Clone>(
    database: &mut Database<V>,
    contexts: &PurposeContexts,
    batch: BoundGraphWriteScriptBatch,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let records = batch.argument_sets();
    let (receipt, completion) = database
        .execute_graph_write_program_returning_autocommit_engine_governed(
            &contexts.txn(),
            &contexts.query(),
            &contexts.commit(),
            batch.program(),
            GraphWriteProgramPolicy::new(policy(), 100_000, 100_000, 100_000),
        )
        .await
        // A failing executed step names its input record (argument set),
        // statement and span, as the native write path's errors do.
        .map_err(|error| execution_failure(batch.execution_error(error)))?;
    let seq = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0,
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0,
    };
    let statements = receipt.stats().completed_statements;
    if robot {
        // Retain the existing written-result schema. These are input records,
        // not returned query rows; do not invent a count of output rows.
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"statements":{statements}}}"#
            ),
        )
    } else {
        emit(
            out,
            &format!("{records} input row(s), {statements} statement(s), completed at seq {seq}"),
        )
    }
}
