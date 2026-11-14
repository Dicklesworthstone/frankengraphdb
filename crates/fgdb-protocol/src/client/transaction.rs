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

impl Client {
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
