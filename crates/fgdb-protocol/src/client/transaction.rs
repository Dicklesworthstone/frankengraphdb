//! Sequential, connection-scoped explicit transactions. Every row is
//! provisional until a separately validated commit response arrives.

use super::*;
use crate::body::{
    ExecuteTransaction, TransactionBegun, TransactionCompletion, TransactionHandle,
    TransactionResultEnd, TransactionSelector,
};

#[derive(Clone, Copy)]
pub(super) struct TransactionState {
    begun: TransactionBegun,
    statements: u64,
}

/// A completely delivered statement result inside a private transaction.
/// This value does not claim that any write has committed.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionAnswer {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<WireValue>>,
    pub end: TransactionResultEnd,
}

/// Maximum number of statements in a managed client transaction batch.
/// The server's retained native statement and capability limits may be lower.
pub const MAX_TRANSACTION_BATCH_STATEMENTS: usize = 32;

/// One statement's delivered rows in a completely committed or read-closed
/// managed batch. The original transaction basis is carried by the batch end.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionBatchResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<WireValue>>,
}

/// Completion of one managed transaction, constructed only after the existing
/// explicit-transaction client validates the native completion receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransactionBatchEnd {
    pub basis: u64,
    pub statements: u32,
    pub rows: u64,
    pub outcome: Outcome,
}

/// A complete managed transaction. No successful prefix is returned if any
/// statement or the final commit fails. A lost commit response remains unknown.
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionBatchAnswer {
    pub results: Vec<TransactionBatchResult>,
    pub end: TransactionBatchEnd,
}

/// Dropping a managed operation must not leave its private owner available to
/// a later call, including a panic in the between-statement callback. An idle
/// owner needs the same reconnect fence as an incomplete wire request. This
/// guard never sends from Drop and never claims that a write was rolled back.
struct TransactionBatchGuard<'a> {
    client: &'a mut Client,
}

impl TransactionBatchGuard<'_> {
    async fn abort(
        &mut self,
        cx: &Cx,
        handle: TransactionHandle,
        error: ClientError,
    ) -> ClientError {
        // A decoded statement refusal or drained callback cancellation already
        // discarded the owner. An incomplete request cannot accept cleanup.
        if self.client.transaction.is_some()
            && self.client.outstanding_request.is_none()
            && self.client.rollback_transaction(cx, handle).await.is_err()
        {
            self.fence();
        }
        error
    }

    fn fence(&mut self) {
        self.client.transaction = None;
        // No further request may be sent on a connection whose cleanup is
        // unconfirmed. This marker is local; it is not a fabricated wire reply.
        self.client
            .outstanding_request
            .get_or_insert(self.client.next_request);
    }
}

impl Drop for TransactionBatchGuard<'_> {
    fn drop(&mut self) {
        if self.client.transaction.is_some() {
            self.fence();
        }
    }
}

impl Client {

    /// Execute 1–32 independently parameterized Read/Write statements through
    /// one explicit native transaction and return all results after completion.
    /// Later statements observe earlier staged effects. The existing native
    /// owner retains the original capability and cumulative execution limits.
    ///
    /// Every request body is admitted before BEGIN. No statement is retried,
    /// reclassified or sent as an autocommit. This collected form retains all
    /// returned rows; use the streaming form to avoid that client-side bag.
    pub async fn execute_transaction_batch(
        &mut self,
        cx: &Cx,
        statements: Vec<Execute>,
    ) -> Result<TransactionBatchAnswer, ClientError> {
        let mut columns = Vec::new();
        let mut rows: Vec<Vec<Vec<WireValue>>> = Vec::new();
        let end = self
            .execute_transaction_batch_streaming(
                cx,
                statements,
                |_, _, names| {
                    columns.push(names.to_vec());
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
                |_, _| Ok(()),
            )
            .await?;
        let mut rows = rows.into_iter();
        let results = columns
            .into_iter()
            .map(|columns| TransactionBatchResult {
                columns,
                rows: rows.next().unwrap_or_default(),
            })
            .collect();
        Ok(TransactionBatchAnswer { results, end })
    }

    /// Deliver a finite mixed read/write script through one native workspace.
    /// Callback indexes are zero-based; the schema callback carries the
    /// original pinned basis and the result-end callback carries its row count.
    /// Every callback is provisional until this method returns successfully.
    /// Earlier statements' effects become visible to the following statements,
    /// and only the final validated COMMIT response establishes completion.
    ///
    /// Complete mode, value, count and negotiated-frame admission precedes
    /// BEGIN, including the transaction selector's wire overhead. The server
    /// still owns syntax, binding, authorization and cumulative budgets.
    /// Callback rejection stops the script and discards its unpublished owner:
    /// in-result rejection uses cancellation/draining; rejection after a
    /// statement END uses explicit rollback. Failed cleanup, an incomplete
    /// response, a dropped future or an unwinding callback requires reconnecting.
    ///
    /// No operation is retried. Once COMMIT is attempted, this method never
    /// sends rollback or another commit; an incomplete response can mean the
    /// write committed. Disconnect or a callback result is not a commit receipt.
    pub async fn execute_transaction_batch_streaming(
        &mut self,
        cx: &Cx,
        mut statements: Vec<Execute>,
        mut on_columns: impl FnMut(u32, u64, &[String]) -> Result<(), ClientError>,
        mut on_row: impl FnMut(u32, Vec<WireValue>) -> Result<(), ClientError>,
        mut on_result_end: impl FnMut(u32, u64) -> Result<(), ClientError>,
    ) -> Result<TransactionBatchEnd, ClientError> {
        if statements.is_empty() || statements.len() > MAX_TRANSACTION_BATCH_STATEMENTS {
            return Err(ClientError::Protocol(
                "a transaction batch needs 1..=32 statements",
            ));
        }
        for statement in &mut statements {
            if statement.statement.trim().is_empty() {
                return Err(ClientError::Protocol(
                    "transaction batch statements must not be empty",
                ));
            }
            statement
                .parameters
                .sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            // The selector is fixed-width. This nonzero value is used only to
            // run the real codec and frame admission; it is never transmitted.
            let body = ExecuteTransaction {
                handle: TransactionHandle([1; 16]),
                statement: statement.clone(),
            };
            Frame::new(
                FrameKind::ExecuteTransaction,
                self.next_request,
                StreamId::CONTROL,
                self.binding,
                body.encode()?,
                self.send_limits,
            )
            .map_err(|_| ClientError::Protocol("request exceeds the negotiated frame limit"))?;
        }
        let count = statements.len() as u32;
        let begun = self.begin_transaction(cx).await?;
        let mut owner = TransactionBatchGuard { client: self };
        let mut rows = 0_u64;
        for (index, statement) in statements.iter().enumerate() {
            let index = index as u32;
            let result = owner
                .client
                .execute_transaction_streaming(
                    cx,
                    begun.handle,
                    statement,
                    |names| on_columns(index, begun.basis, names),
                    |row| on_row(index, row),
                )
                .await;
            let end = match result {
                Ok(end) => end,
                Err(error) => return Err(owner.abort(cx, begun.handle, error).await),
            };
            let Some(total) = rows.checked_add(end.rows) else {
                return Err(owner
                    .abort(
                        cx,
                        begun.handle,
                        ClientError::Protocol("transaction batch row counter overflow"),
                    )
                    .await);
            };
            if let Err(error) = on_result_end(index, end.rows) {
                return Err(owner.abort(cx, begun.handle, error).await);
            }
            rows = total;
        }
        let outcome = owner.client.commit_transaction(cx, begun.handle).await?;
        Ok(TransactionBatchEnd {
            basis: begun.basis,
            statements: count,
            rows,
            outcome,
        })
    }

    /// Open one explicit native write workspace on the selected database.
    /// It retains the original authority and cumulative budgets through
    /// commit or rollback. Disconnect abandons unpublished effects. This is
    /// not a durable transaction lease and cannot be reattached or resumed.
    pub async fn begin_transaction(
        &mut self,
        cx: &Cx,
    ) -> Result<TransactionBegun, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        let request = self
            .send(cx, FrameKind::BeginTransaction, StreamId::CONTROL, &Empty)
            .await?;
        let frame = self.reply(cx, request).await?;
        let begun: TransactionBegun = expect(&frame, FrameKind::TransactionBegun)?;
        self.finish_request(request)?;
        self.transaction = Some(TransactionState {
            begun,
            statements: 0,
        });
        Ok(begun)
    }

    fn transaction_state(
        &self,
        handle: TransactionHandle,
    ) -> Result<TransactionState, ClientError> {
        let state = self.transaction.ok_or(ClientError::Protocol(
            "no explicit transaction is active on this connection",
        ))?;
        if state.begun.handle != handle {
            return Err(ClientError::Protocol(
                "transaction handle belongs to a different owner",
            ));
        }
        Ok(state)
    }

    /// Execute a statement against the retained private workspace and collect
    /// its provisional rows. Only Read and Write modes are supported. A Read
    /// statement cannot stage writes. The client never falls back to autocommit.
    pub async fn execute_transaction(
        &mut self,
        cx: &Cx,
        handle: TransactionHandle,
        statement: &Execute,
    ) -> Result<TransactionAnswer, ClientError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let end = self
            .execute_transaction_streaming(
                cx,
                handle,
                statement,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |row| {
                    rows.push(row);
                    Ok(())
                },
            )
            .await?;
        Ok(TransactionAnswer { columns, rows, end })
    }

    /// Deliver provisional columns and rows under the ordinary flow window.
    /// Rejecting a callback sends cancellation and drains its terminal reply.
    /// The server also aborts on a late cancellation of this transaction's
    /// last delivered stream, so an END already in flight cannot retain the
    /// rejected prefix. Definite statement failure discards the entire owner.
    /// A dropped future or invalid/incomplete response requires reconnecting.
    pub async fn execute_transaction_streaming(
        &mut self,
        cx: &Cx,
        handle: TransactionHandle,
        statement: &Execute,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<TransactionResultEnd, ClientError> {
        let state = self.transaction_state(handle)?;
        let ordinal = state
            .statements
            .checked_add(1)
            .ok_or(ClientError::Protocol("transaction statement sequence exhausted"))?;
        let mut statement = statement.clone();
        statement.parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecuteTransaction,
                StreamId::CONTROL,
                &ExecuteTransaction { handle, statement },
            )
            .await?;
        self.receive_transaction_result(
            cx, request, handle, ordinal, on_columns, on_row,
        )
        .await
    }

    async fn receive_transaction_result(
        &mut self,
        cx: &Cx,
        request: u64,
        handle: TransactionHandle,
        ordinal: u64,
        mut on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        mut on_row: impl FnMut(Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<TransactionResultEnd, ClientError> {
        let mut stream: Option<StreamId> = None;
        let mut callback_error = None;
        let (initial_bytes, initial_rows) = self.initial_window;
        let mut available = (initial_bytes, initial_rows);
        let mut grants = 0_u64;
        let mut progress = ResultProgress::default();
        loop {
            let frame = self.receive(cx).await?.ok_or(ClientError::Closed)?;
            let header = *frame.header();
            if header.kind() == FrameKind::Goodbye {
                return Err(ClientError::Closed);
            }
            if header.request_id() != request {
                return Err(ClientError::Protocol("transaction frame for a different request"));
            }
            match stream {
                None if !header.stream_id().is_control() => stream = Some(header.stream_id()),
                Some(id) if id != header.stream_id() => {
                    return Err(ClientError::Protocol("transaction frame on a foreign stream"));
                }
                _ => {}
            }
            match header.kind() {
                FrameKind::TransactionResultChunk => {
                    let Some(id) = stream else {
                        return Err(ClientError::Protocol("transaction chunk on the control stream"));
                    };
                    let chunk = ResultChunk::decode(frame.payload())?;
                    let count = chunk.rows.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    available = (available.0 - cost, available.1 - count);
                    progress.observe_chunk(&chunk).map_err(ClientError::Protocol)?;
                    if callback_error.is_none()
                        && let Some(names) = chunk.columns
                        && let Err(error) = on_columns(&names)
                    {
                        self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                        callback_error = Some(error);
                    }
                    for row in chunk.rows {
                        if callback_error.is_none()
                            && let Err(error) = on_row(row)
                        {
                            self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            callback_error = Some(error);
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if callback_error.is_none() && (available.0 < frame_limit || available.1 == 0) {
                        grants = grants
                            .checked_add(1)
                            .ok_or(ClientError::Protocol("result flow sequence overflow"))?;
                        let update = WindowUpdate {
                            sequence: grants,
                            bytes: initial_bytes - available.0,
                            rows: initial_rows - available.1,
                        };
                        self.send(cx, FrameKind::WindowUpdate, id, &update).await?;
                        available = (initial_bytes, initial_rows);
                    }
                }
                FrameKind::TransactionResultEnd => {
                    if stream.is_none() {
                        return Err(ClientError::Protocol("transaction END on the control stream"));
                    }
                    let end = TransactionResultEnd::decode(frame.payload())?;
                    if end.handle != handle || end.statement != ordinal {
                        return Err(ClientError::Protocol("transaction END changed owner or ordinal"));
                    }
                    progress
                        .finish_rows(end.rows, header.frame_len() as u64, available.0)
                        .map_err(ClientError::Protocol)?;
                    self.finish_request(request)?;
                    if let Some(error) = callback_error {
                        self.transaction = None;
                        return Err(error);
                    }
                    if let Some(state) = &mut self.transaction {
                        state.statements = ordinal;
                    }
                    return Ok(end);
                }
                FrameKind::Error => {
                    let refusal = ErrorBody::decode(frame.payload())?;
                    self.finish_request(request)?;
                    self.transaction = None;
                    return Err(callback_error.unwrap_or(ClientError::Server {
                        code: refusal.code,
                        message: refusal.message,
                    }));
                }
                _ => return Err(ClientError::Protocol("unexpected transaction result frame")),
            }
        }
    }

    /// Publish the retained workspace exactly once. No automatic retry is
    /// attempted. Losing the response can leave the commit outcome unknown;
    /// reconnecting does not establish that the transaction rolled back.
    pub async fn commit_transaction(
        &mut self,
        cx: &Cx,
        handle: TransactionHandle,
    ) -> Result<Outcome, ClientError> {
        let state = self.transaction_state(handle)?;
        let request = self
            .send(
                cx,
                FrameKind::CommitTransaction,
                StreamId::CONTROL,
                &TransactionSelector { handle },
            )
            .await?;
        let frame = match self.reply(cx, request).await {
            Ok(frame) => frame,
            Err(error) => {
                if matches!(&error, ClientError::Server { .. }) {
                    self.transaction = None;
                }
                return Err(error);
            }
        };
        let completion: TransactionCompletion = expect(&frame, FrameKind::TransactionCommitted)?;
        if completion.handle != handle {
            return Err(ClientError::Protocol("commit completed a different transaction"));
        }
        let valid = match completion.outcome {
            Outcome::WriteCommitted { seq, statements } => {
                statements != 0 && seq > state.begun.basis && statements == state.statements
            }
            Outcome::ReadClosed { seq, statements } => {
                seq == state.begun.basis && statements == state.statements
            }
            Outcome::Rows { .. } => false,
        };
        if !valid {
            return Err(ClientError::Protocol(
                "commit completion changed the transaction basis or statement count",
            ));
        }
        self.finish_request(request)?;
        self.transaction = None;
        Ok(completion.outcome)
    }

    /// Discard every unpublished effect and release this connection's owner.
    /// Local encoding failure leaves it active; an incomplete wire response
    /// requires reconnecting, which also abandons an uncommitted workspace.
    pub async fn rollback_transaction(
        &mut self,
        cx: &Cx,
        handle: TransactionHandle,
    ) -> Result<(), ClientError> {
        self.transaction_state(handle)?;
        let request = self
            .send(
                cx,
                FrameKind::RollbackTransaction,
                StreamId::CONTROL,
                &TransactionSelector { handle },
            )
            .await?;
        let frame = match self.reply(cx, request).await {
            Ok(frame) => frame,
            Err(error) => {
                if matches!(&error, ClientError::Server { .. }) {
                    self.transaction = None;
                }
                return Err(error);
            }
        };
        let rolled_back: TransactionSelector = expect(&frame, FrameKind::TransactionRolledBack)?;
        if rolled_back.handle != handle {
            return Err(ClientError::Protocol("rollback selected a different transaction"));
        }
        self.finish_request(request)?;
        self.transaction = None;
        Ok(())
    }
}
