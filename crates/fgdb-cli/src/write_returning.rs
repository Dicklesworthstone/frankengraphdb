//! Shared preparation and staged execution for native CREATE, mutation and
//! vertex MERGE RETURN writes, used by ordinary writes and ordered transactions.
//! A RETURN-less write, including a bounded native UNWIND, uses the native
//! write program path.

use super::{
    Failure, Options, emit, execution_failure, human_value, policy, render_row_body, value_cell,
};
use asupersync::fs::Vfs;
use fgdb::{Database, WriteTxn};
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::GraphInsertPolicy;
use fgdb_gql::{
    GqlParameters, GqlQueryExecution, GqlQueryPolicy, GraphMutationPolicy, GraphVertexMergePolicy,
    GraphVertexUpsertPolicy, PreparedGraphInsertQuery, PreparedGraphInsertQueryText,
    PreparedGraphMutationQuery, PreparedGraphMutationQueryText, PreparedGraphVertexUpsertQuery,
    PreparedGraphVertexUpsertQueryText,
};
use fgdb_types::{EmbeddedTxnCompletion, EmbeddedTxnState, PurposeContexts, QueryCx};
use std::io::{self, Write};

const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
// The sequence and row count are the only values added after completion. Their
// bounded integer encodings plus the fixed terminal frame fit this reservation.
const TERMINAL_RESERVATION: usize = 256;

/// One write statement whose RETURN rows are produced with its effects.
pub(super) enum Returning {
    Insert(Box<PreparedGraphInsertQuery>),
    Mutation(Box<PreparedGraphMutationQuery>),
    Merge(Box<PreparedGraphVertexUpsertQuery>),
}
impl Returning {
    pub(super) fn columns(&self) -> &[String] {
        match self {
            Self::Insert(query) => query.columns(),
            Self::Mutation(query) => query.columns(),
            Self::Merge(query) => query.columns(),
        }
    }

    /// Freeze the native statement's rows while staging its complete effects.
    /// The caller owns output admission and the sole completion boundary.
    pub(super) fn execute_in_transaction<V: Vfs + Clone>(
        &self,
        transaction: &mut WriteTxn,
        database: &mut Database<V>,
        cx: &QueryCx,
        allowance: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, Failure> {
        Ok(match self {
            Self::Insert(query) => {
                transaction
                    .execute_graph_insert_query_engine_governed(
                        database,
                        cx,
                        query,
                        GraphInsertPolicy::new(allowance, 100_000, 100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
            Self::Mutation(query) => {
                transaction
                    .execute_graph_mutation_query_governed(
                        database,
                        cx,
                        query,
                        GraphMutationPolicy::new(allowance, 100_000),
                    )
                    .map_err(execution_failure)?
                    .1
            }
            Self::Merge(query) => {
                transaction
                    .execute_graph_vertex_upsert_query_engine_governed(
                        database,
                        cx,
                        query,
                        GraphVertexUpsertPolicy::new(GraphVertexMergePolicy::new(allowance), 1_000),
                    )
                    .map_err(execution_failure)?
                    .2
            }
        })
    }
}

pub(super) fn prepare(options: &Options) -> Result<Option<Returning>, Failure> {
    prepare_statement(&options.text, &options.params, options)
}

/// Native token classifiers choose the write family before binding. Ordered
/// transaction steps supply their own text and parameters, with one catalog.
pub(super) fn prepare_statement(
    statement: &str,
    params: &GqlParameters,
    options: &Options,
) -> Result<Option<Returning>, Failure> {
    let declarations: Vec<_> = params.parameter_types().collect();
    if PreparedGraphInsertQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphInsertQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            |kind, name| options.resolve(kind, name),
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::Insert(Box::new(query))))
            .map_err(Failure::query);
    }
    if PreparedGraphMutationQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphMutationQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            |kind, name| options.resolve(kind, name),
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::Mutation(Box::new(query))))
            .map_err(Failure::query);
    }
    if PreparedGraphVertexUpsertQueryText::has_return_clause(statement).map_err(Failure::query)? {
        let template = PreparedGraphVertexUpsertQueryText::prepare_with_parameter_types(
            statement,
            options.coordinate,
            &declarations,
            |kind, name| options.resolve(kind, name),
        )
        .map_err(Failure::query)?;
        return template
            .bind_parameters(params)
            .map(|query| Some(Returning::Merge(Box::new(query))))
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
        let result = query.execute_in_transaction(&mut transaction, database, &cx, policy())?;
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
