//! One pinned, authorized native read batch and its ordered result streams.

use super::*;
use crate::body::{
    ExecutePreparedReadBatch, ExecuteReadStatements, ReadBatchResultChunk, ReadBatchResultEnd,
};
use crate::read_batch::ReadBatchProgress;

enum ReadBatchRequest {
    Parameterized(ExecuteBatch),
    Statements(ExecuteReadStatements),
    Prepared(ExecutePreparedReadBatch),
}

impl ReadBatchRequest {
    fn len(&self) -> usize {
        match self {
            Self::Parameterized(batch) => batch.argument_sets.len(),
            Self::Statements(batch) => batch.statements.len(),
            Self::Prepared(batch) => batch.argument_sets.len(),
        }
    }

    fn canonicalize(&mut self) {
        let sort = |parameters: &mut Vec<(String, WireValue)>| {
            parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        };
        match self {
            Self::Parameterized(batch) => batch.argument_sets.iter_mut().for_each(sort),
            Self::Prepared(batch) => batch.argument_sets.iter_mut().for_each(sort),
            Self::Statements(batch) => {
                for statement in &mut batch.statements {
                    sort(&mut statement.parameters);
                }
            }
        }
    }
}

/// One result in request order, retaining its exact selected historical cut.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadBatchResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<WireValue>>,
    pub seq: u64,
}

/// A fully received read batch. This value is constructed only after the
/// terminal agrees with the requested result count and every delivered row.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadBatchAnswer {
    pub results: Vec<ReadBatchResult>,
    pub end: ReadBatchResultEnd,
}

impl Client {
    /// Execute every argument set in one authorized read session, sharing the
    /// session's current snapshot and signed allowance. Explicit temporal
    /// operands still select each result's exact cut. The server admits the
    /// complete batch before emitting any result; writes always refuse.
    /// No request or individual argument set is retried.
    pub async fn execute_read_batch(
        &mut self,
        cx: &Cx,
        statement: &str,
        argument_sets: Vec<Vec<(String, WireValue)>>,
    ) -> Result<ReadBatchAnswer, ClientError> {
        self.collect_read_batch(
            cx,
            ReadBatchRequest::Parameterized(ExecuteBatch {
                statement: statement.to_owned(),
                argument_sets,
            }),
        )
        .await
    }

    /// Execute 1–32 distinct reads with independent parameters and schemas in
    /// one pinned native session. All statements share one signed allowance.
    /// Each mode must be Read; write text still refuses at native compilation.
    /// A late refusal returns no successful prefix and performs no mutation.
    /// Older servers reject this request's frame kind; the client never retries
    /// it as independent reads that could observe different snapshots.
    pub async fn execute_read_statements(
        &mut self,
        cx: &Cx,
        statements: Vec<Execute>,
    ) -> Result<ReadBatchAnswer, ClientError> {
        self.collect_read_batch(
            cx,
            ReadBatchRequest::Statements(ExecuteReadStatements { statements }),
        )
        .await
    }

    /// Reuse one prepared read for 1–32 fresh argument sets in one consistent
    /// session. The complete batch shares signed limits and is admitted before
    /// any output. Current reads share a pin; temporal arguments retain each
    /// result's selected cut. Representative preparation values are never used
    /// as defaults. A non-read or unavailable handle refuses without effects.
    /// Older servers reject the distinct request; no fallback or retry occurs.
    pub async fn execute_prepared_read_batch(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        argument_sets: Vec<Vec<(String, WireValue)>>,
    ) -> Result<ReadBatchAnswer, ClientError> {
        self.collect_read_batch(
            cx,
            ReadBatchRequest::Prepared(ExecutePreparedReadBatch {
                handle,
                argument_sets,
            }),
        )
        .await
    }

    async fn collect_read_batch(
        &mut self,
        cx: &Cx,
        request: ReadBatchRequest,
    ) -> Result<ReadBatchAnswer, ClientError> {
        let mut metadata = Vec::new();
        let mut rows: Vec<Vec<Vec<WireValue>>> = Vec::new();
        let end = self
            .read_batch_request(
                cx,
                request,
                |_, seq, columns| {
                    metadata.push((seq, columns.to_vec()));
                    Ok(())
                },
                |index, row| {
                    let index = index as usize;
                    if rows.len() <= index {
                        rows.resize_with(index + 1, Vec::new);
                    }
                    rows[index].push(row);
                    Ok(())
                },
                |_, _, _| Ok(()),
            )
            .await?;
        let mut rows = rows.into_iter();
        let results = metadata
            .into_iter()
            .map(|(seq, columns)| ReadBatchResult {
                columns,
                rows: rows.next().unwrap_or_default(),
                seq,
            })
            .collect();
        Ok(ReadBatchAnswer { results, end })
    }

    /// Deliver a consistent batch under one stream's byte and row credit.
    /// Callback indexes are zero-based. The schema callback receives the
    /// result's exact selected sequence; its end callback receives that same
    /// sequence and the result's complete delivered row count.
    ///
    /// Every callback is provisional until this method returns its single
    /// successful terminal. Native execution is buffered before delivery;
    /// this is not a source-stream or ordered-spill execution mode. A callback
    /// error sends one cancellation and drains the original terminal without
    /// more callbacks or credit grants. A dropped future or invalid response
    /// requires reconnecting, like every other outstanding client request.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_read_batch_streaming(
        &mut self,
        cx: &Cx,
        statement: &str,
        argument_sets: Vec<Vec<(String, WireValue)>>,
        on_columns: impl FnMut(u32, u64, &[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(u32, Vec<WireValue>) -> Result<(), ClientError>,
        on_result_end: impl FnMut(u32, u64, u64) -> Result<(), ClientError>,
    ) -> Result<ReadBatchResultEnd, ClientError> {
        self.read_batch_request(
            cx,
            ReadBatchRequest::Parameterized(ExecuteBatch {
                statement: statement.to_owned(),
                argument_sets,
            }),
            on_columns,
            on_row,
            on_result_end,
        )
        .await
    }

    /// Stream a heterogeneous read batch using the same result indexes, exact
    /// snapshot sequences, shared flow credit and cancellation contract as
    /// [`Self::execute_read_batch_streaming`]. Execution remains buffered before
    /// delivery. All callbacks are provisional until the single terminal.
    /// Invalid count/mode/value encodings send nothing and leave the connection
    /// reusable; a dropped outstanding request requires reconnecting.
    pub async fn execute_read_statements_streaming(
        &mut self,
        cx: &Cx,
        statements: Vec<Execute>,
        on_columns: impl FnMut(u32, u64, &[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(u32, Vec<WireValue>) -> Result<(), ClientError>,
        on_result_end: impl FnMut(u32, u64, u64) -> Result<(), ClientError>,
    ) -> Result<ReadBatchResultEnd, ClientError> {
        self.read_batch_request(
            cx,
            ReadBatchRequest::Statements(ExecuteReadStatements { statements }),
            on_columns,
            on_row,
            on_result_end,
        )
        .await
    }

    /// Deliver a consistent prepared-read batch under the same flow and
    /// cancel-and-drain contract as [`Self::execute_read_batch_streaming`].
    /// Native execution finishes before delivery; callbacks are provisional
    /// until the single successful terminal. Local shape errors send nothing.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_read_batch_streaming(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        argument_sets: Vec<Vec<(String, WireValue)>>,
        on_columns: impl FnMut(u32, u64, &[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(u32, Vec<WireValue>) -> Result<(), ClientError>,
        on_result_end: impl FnMut(u32, u64, u64) -> Result<(), ClientError>,
    ) -> Result<ReadBatchResultEnd, ClientError> {
        self.read_batch_request(
            cx,
            ReadBatchRequest::Prepared(ExecutePreparedReadBatch {
                handle,
                argument_sets,
            }),
            on_columns,
            on_row,
            on_result_end,
        )
        .await
    }

    async fn read_batch_request(
        &mut self,
        cx: &Cx,
        mut request: ReadBatchRequest,
        mut on_columns: impl FnMut(u32, u64, &[String]) -> Result<(), ClientError>,
        mut on_row: impl FnMut(u32, Vec<WireValue>) -> Result<(), ClientError>,
        mut on_result_end: impl FnMut(u32, u64, u64) -> Result<(), ClientError>,
    ) -> Result<ReadBatchResultEnd, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        let expected =
            u32::try_from(request.len()).map_err(|_| ClientError::Body(BodyError::TooLarge))?;
        request.canonicalize();
        let request = match request {
            ReadBatchRequest::Parameterized(body) => {
                self.send(cx, FrameKind::ExecuteReadBatch, StreamId::CONTROL, &body)
                    .await?
            }
            ReadBatchRequest::Statements(body) => {
                self.send(
                    cx,
                    FrameKind::ExecuteReadStatements,
                    StreamId::CONTROL,
                    &body,
                )
                .await?
            }
            ReadBatchRequest::Prepared(body) => {
                self.send(
                    cx,
                    FrameKind::ExecutePreparedReadBatch,
                    StreamId::CONTROL,
                    &body,
                )
                .await?
            }
        };
        let mut progress = ReadBatchProgress::new(expected);
        let mut stream = None;
        let mut callback_error = None;
        let (initial_bytes, initial_rows) = self.initial_window;
        let mut available = (initial_bytes, initial_rows);
        let mut grants = 0_u64;
        loop {
            let frame = self.receive(cx).await?.ok_or(ClientError::Closed)?;
            let header = *frame.header();
            if header.kind() == FrameKind::Goodbye {
                return Err(ClientError::Closed);
            }
            if header.request_id() != request {
                return Err(ClientError::Protocol(
                    "read batch frame for a different request",
                ));
            }
            match stream {
                None if !header.stream_id().is_control() => stream = Some(header.stream_id()),
                Some(id) if id != header.stream_id() => {
                    return Err(ClientError::Protocol(
                        "read batch frame on a foreign stream",
                    ));
                }
                _ => {}
            }
            match header.kind() {
                FrameKind::ReadBatchResultChunk => {
                    let Some(id) = stream else {
                        return Err(ClientError::Protocol(
                            "read batch chunk on the control stream",
                        ));
                    };
                    let part = ReadBatchResultChunk::decode(frame.payload())?;
                    let count = part.chunk.rows.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    let completed = progress.observe(&part).map_err(ClientError::Protocol)?;
                    available = (available.0 - cost, available.1 - count);
                    if callback_error.is_none()
                        && let Some(names) = part.chunk.columns
                        && let Err(error) = on_columns(part.index, part.seq, &names)
                    {
                        self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                        callback_error = Some(error);
                    }
                    for row in part.chunk.rows {
                        if callback_error.is_none()
                            && let Err(error) = on_row(part.index, row)
                        {
                            self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            callback_error = Some(error);
                        }
                    }
                    if callback_error.is_none()
                        && let Some(rows) = completed
                        && let Err(error) = on_result_end(part.index, part.seq, rows)
                    {
                        self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                        callback_error = Some(error);
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if callback_error.is_none() && (available.0 < frame_limit || available.1 == 0) {
                        grants = grants
                            .checked_add(1)
                            .ok_or(ClientError::Protocol("read batch flow sequence overflow"))?;
                        self.send(
                            cx,
                            FrameKind::WindowUpdate,
                            id,
                            &WindowUpdate {
                                sequence: grants,
                                bytes: initial_bytes - available.0,
                                rows: initial_rows - available.1,
                            },
                        )
                        .await?;
                        available = (initial_bytes, initial_rows);
                    }
                }
                FrameKind::ReadBatchResultEnd => {
                    if stream.is_none() {
                        return Err(ClientError::Protocol(
                            "read batch END on the control stream",
                        ));
                    }
                    if header.frame_len() as u64 > available.0 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    let end = ReadBatchResultEnd::decode(frame.payload())?;
                    progress.finish(&end).map_err(ClientError::Protocol)?;
                    self.finish_request(request)?;
                    return callback_error.map_or(Ok(end), Err);
                }
                FrameKind::Error => {
                    let refusal = ErrorBody::decode(frame.payload())?;
                    self.finish_request(request)?;
                    return Err(callback_error.unwrap_or(ClientError::Server {
                        code: refusal.code,
                        message: refusal.message,
                    }));
                }
                _ => return Err(ClientError::Protocol("unexpected frame in a read batch")),
            }
        }
    }
}
