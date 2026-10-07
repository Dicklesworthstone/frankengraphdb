//! Pre-open preparation for CREATE RETURN and SET/REMOVE/DETACH DELETE RETURN
//! writes. A RETURN-less write, including a bounded native UNWIND, is bound by
//! `main`'s native write path.

use super::{
    Failure, Options, emit, execution_failure, human_value, policy, render_row_body, value_cell,
};
use asupersync::fs::Vfs;
use fgdb::Database;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GraphMutationPolicy, PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
    PreparedGraphMutationQuery, PreparedGraphMutationQueryText,
};
use fgdb_types::{EmbeddedTxnCompletion, EmbeddedTxnState, PurposeContexts};
use std::io::{self, Write};

const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
// The sequence and row count are the only values added after completion. Their
// bounded integer encodings plus the fixed terminal frame fit this reservation.
const TERMINAL_RESERVATION: usize = 256;

/// One write statement whose RETURN rows are produced with its effects.
pub(super) enum Returning {
    Insert(PreparedGraphInsertQuery),
    Mutation(PreparedGraphMutationQuery),
}
impl Returning {
    fn columns(&self) -> &[String] {
        match self {
            Self::Insert(query) => query.columns(),
            Self::Mutation(query) => query.columns(),
        }
    }
}

pub(super) fn prepare(options: &Options) -> Result<Option<Returning>, Failure> {
    let declarations: Vec<_> = options.params.parameter_types().collect();
    if PreparedGraphInsertQueryText::has_return_clause(&options.text).map_err(Failure::query)? {
        let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
            &options.text,
            options.coordinate,
            &declarations,
            |kind, name| options.resolve(kind, name),
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(&options.params)
            .map(|query| Some(Returning::Insert(query)))
            .map_err(Failure::query);
    }
    if PreparedGraphMutationQueryText::has_return_clause(&options.text).map_err(Failure::query)? {
        let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
            &options.text,
            options.coordinate,
            &declarations,
            |kind, name| options.resolve(kind, name),
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(&options.params)
            .map(|query| Some(Returning::Mutation(query)))
            .map_err(Failure::query);
    }
    Ok(None)
}

struct BufferedRows {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BufferedRows {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|end| *end <= self.limit)
            .ok_or_else(|| io::Error::other("write RETURN encoded output exceeds 16 MiB"))?;
        self.bytes.extend_from_slice(bytes);
        debug_assert_eq!(self.bytes.len(), end);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) async fn run<V: Vfs + Clone>(
    database: &mut Database<V>,
    contexts: &PurposeContexts,
    query: Returning,
    robot: bool,
    out: &mut impl Write,
) -> Result<(), Failure> {
    let cx = contexts.query();
    let mut transaction = database.begin(&contexts.txn()).map_err(execution_failure)?;
    let prepared = (|| {
        let result = match &query {
            Returning::Insert(query) => {
                transaction
                    .execute_graph_insert_query_engine_governed(
                        database,
                        &cx,
                        query,
                        GraphInsertPolicy::new(policy(), 100_000, 100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
            Returning::Mutation(query) => {
                transaction
                    .execute_graph_mutation_query_governed(
                        database,
                        &cx,
                        query,
                        GraphMutationPolicy::new(policy(), 100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
        };
        let mut rendered = Vec::with_capacity(result.value.len());
        let mut encoded_cells = 0usize;
        for row in result.value {
            cx.checkpoint().map_err(Failure::query)?;
            let mut cells = Vec::with_capacity(row.values().len());
            for value in row.values() {
                let cell = if robot {
                    value_cell(value)?
                } else {
                    human_value(value)?
                };
                encoded_cells = encoded_cells
                    .checked_add(cell.len())
                    .filter(|bytes| *bytes <= MAX_OUTPUT_BYTES - TERMINAL_RESERVATION)
                    .ok_or_else(|| Failure::query("write RETURN encoded output exceeds 16 MiB"))?;
                cells.push(cell);
            }
            rendered.push(cells);
        }
        let count = rendered.len();
        let mut buffer = BufferedRows {
            bytes: Vec::new(),
            limit: MAX_OUTPUT_BYTES - TERMINAL_RESERVATION,
        };
        render_row_body(query.columns(), &rendered, robot, &mut buffer)
            .map_err(|error| Failure::query(error.message))?;
        cx.checkpoint().map_err(Failure::query)?;
        Ok((buffer.bytes, count))
    })();
    let (bytes, count) = match prepared {
        Ok(output) => output,
        Err(error) => {
            transaction.abort();
            return Err(error);
        }
    };
    let completion = match transaction.finish(database, &contexts.commit()).await {
        Ok(completion) => completion,
        Err(error) => {
            return Err(match transaction.state() {
                EmbeddedTxnState::CommitOutcomeUnknown { .. } => Failure::io(format!(
                    "transaction outcome unknown; reopen and resolve before retrying: {error}"
                )),
                EmbeddedTxnState::CommittedNeedsRecovery { commit_seq } => Failure::io(format!(
                    "transaction committed at seq {}; recovery required: {error}",
                    commit_seq.0
                )),
                _ => execution_failure(error),
            });
        }
    };
    let seq = match completion {
        EmbeddedTxnCompletion::WriteCommitted { commit_seq } => commit_seq.0,
        EmbeddedTxnCompletion::ReadClosed { snapshot_seq, .. } => snapshot_seq.0,
    };
    // Only terminal transport can fail from here. No query, conversion, quota,
    // authorization, or cancellation check can discard a committed result.
    out.write_all(&bytes).map_err(Failure::io)?;
    if robot {
        emit(
            out,
            &format!(
                r#"{{"v":1,"event":"result","kind":"written","seq":{seq},"count":{count},"statements":1}}"#
            ),
        )
    } else {
        emit(out, &format!("{count} row(s), completed at seq {seq}"))
    }
}
