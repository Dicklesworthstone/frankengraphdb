//! Connection-owned native transactions. Statement rows are provisional until
//! explicit completion; only a completely delivered provisional END retains
//! the owner for another request. No idle database lock or implicit commit is
//! introduced, and a lost commit reply is never replayed.

use super::*;
use crate::execute::NativeWriteTransaction;
use fgdb_protocol::body::{
    ExecuteTransaction, TransactionBegun, TransactionCompletion, TransactionHandle,
    TransactionSelector,
};

pub(super) struct ActiveTransaction<'server> {
    handle: TransactionHandle,
    native: NativeWriteTransaction<'server>,
    statement: u64,
    pub(super) last_stream: Option<StreamId>,
}

impl ActiveTransaction<'_> {
    pub(super) fn generation(&self) -> Generation {
        self.native.generation()
    }
}

fn unavailable_transaction() -> Refusal {
    Refusal::new(
        ErrorCode::Statement,
        "transaction not available on this connection; begin a new transaction",
    )
}

/// Every attempted transaction operation consumes the retained owner first.
/// A foreign/stale handle has the same refusal as a missing owner; it cannot
/// leave a staged prefix available to commit after an operation failed.
fn take_transaction<'server>(
    active: &mut Option<ActiveTransaction<'server>>,
    handle: TransactionHandle,
) -> Result<ActiveTransaction<'server>, Refusal> {
    let owned = active
        .take()
        .filter(|owned| owned.handle == handle)
        .ok_or_else(unavailable_transaction)?;
    owned.native.generation().check().map_err(Refusal::from)?;
    Ok(owned)
}

impl Lane {
    /// Recovery wakes an idle owner without waiting for another client frame.
    /// Closing this connection drops the native transaction and its pins.
    pub(super) async fn receive_transaction(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        generation: Generation,
    ) -> Inbound {
        let mut watcher = generation.watcher();
        let Self {
            reader,
            conn,
            finished,
            ..
        } = self;
        poll_fn(|task| {
            while watcher.poll_changed(task) {}
            if generation.check().is_err() {
                return Poll::Ready(Inbound::Closed);
            }
            if waiter.poll_triggered(task) {
                return Poll::Ready(Inbound::Shutdown);
            }
            reader
                .poll_receive(cx, task, |header| validate(conn, finished, header))
                .map(|received| match received {
                    Ok(Some(frame)) => Inbound::Frame(Box::new(frame)),
                    Ok(None) | Err(_) => Inbound::Closed,
                })
        })
        .await
    }

    pub(super) async fn transaction_request<'server>(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &'server Served,
        token: &CapabilityToken,
        active: &mut Option<ActiveTransaction<'server>>,
        frame: &Frame,
    ) -> Result<(), Stop> {
        let request = frame.header().request_id();
        let binding = self.conn.binding();
        self.send_generation = None;
        match frame.header().kind() {
            FrameKind::BeginTransaction => {
                if Empty::decode(frame.payload()).is_err() {
                    self.fatal(cx, request, ErrorCode::Protocol, "malformed BEGIN_TRANSACTION")
                        .await;
                    return Err(Stop::Transport);
                }
                let native = match NativeWriteTransaction::begin(cx, db, token).await {
                    Ok(native) => native,
                    Err(refusal) => {
                        return self
                            .refuse(
                                cx,
                                request,
                                StreamId::CONTROL,
                                binding,
                                refusal.code,
                                &refusal.message,
                            )
                            .await;
                    }
                };
                let handle = loop {
                    let mut bytes = [0; 16];
                    cx.random_bytes(&mut bytes);
                    if bytes != [0; 16] {
                        break TransactionHandle(bytes);
                    }
                };
                let body = TransactionBegun {
                    handle,
                    basis: native.basis().0,
                };
                let response = Frame::new(
                    FrameKind::TransactionBegun,
                    request,
                    StreamId::CONTROL,
                    binding,
                    body.encode().map_err(|_| Stop::Transport)?,
                    self.send_limits,
                )
                .map_err(|_| Stop::Transport)?;
                match self
                    .write_frame(cx, &response, Some(native.generation()))
                    .await
                {
                    Ok(()) => {
                        *active = Some(ActiveTransaction {
                            handle,
                            native,
                            statement: 0,
                            last_stream: None,
                        });
                        Ok(())
                    }
                    Err(Stop::Recovery(error)) => {
                        self.refuse(
                            cx,
                            request,
                            StreamId::CONTROL,
                            binding,
                            error.code(),
                            error.message(),
                        )
                        .await
                    }
                    Err(stop) => Err(stop),
                }
            }
            FrameKind::ExecuteTransaction => {
                let Ok(statement) = ExecuteTransaction::decode(frame.payload()) else {
                    self.fatal(cx, request, ErrorCode::Protocol, "malformed EXECUTE_TRANSACTION")
                        .await;
                    return Err(Stop::Transport);
                };
                let mut owned = match take_transaction(active, statement.handle) {
                    Ok(owned) => owned,
                    Err(refusal) => {
                        return self
                            .refuse(
                                cx,
                                request,
                                StreamId::CONTROL,
                                binding,
                                refusal.code,
                                &refusal.message,
                            )
                            .await;
                    }
                };
                let Some(ordinal) = owned.statement.checked_add(1) else {
                    return self
                        .refuse(
                            cx,
                            request,
                            StreamId::CONTROL,
                            binding,
                            ErrorCode::Budget,
                            "transaction statement ordinal exhausted",
                        )
                        .await;
                };
                let stream = loop {
                    let mut bytes = [0; 16];
                    cx.random_bytes(&mut bytes);
                    let stream = StreamId(bytes);
                    if !stream.is_control() && !self.finished.contains(&stream) {
                        break stream;
                    }
                };
                let child = self
                    .conn
                    .admit_child(stream, ChildKind::Query)
                    .map_err(|_| Stop::Transport)?;
                let frame_budget = self.send_limits.max_frame_len()
                    - binding.header_len()
                    - CHUNK_OVERHEAD;
                let answer = owned
                    .native
                    .execute_framed(cx, &statement.statement, frame_budget)
                    .await;
                let delivered = match answer {
                    Ok(answer) => {
                        self.stream_with_transaction(
                            cx,
                            waiter,
                            request,
                            stream,
                            binding,
                            answer,
                            Some((owned.handle, ordinal)),
                        )
                        .await
                    }
                    Err(refusal) => self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            refusal.code,
                            &refusal.message,
                        )
                        .await
                        .map(|()| false),
                };
                let delivered = match delivered {
                    Err(Stop::Recovery(error)) => self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            error.code(),
                            error.message(),
                        )
                        .await
                        .map(|()| false),
                    result => result,
                };
                self.send_generation = None;
                let terminal =
                    self.conn
                        .child_terminal(stream, child, ChildTerminus::EphemeralCompleted);
                self.finish(stream);
                terminal.map_err(|_| Stop::Transport)?;
                if delivered? {
                    owned.statement = ordinal;
                    owned.last_stream = Some(stream);
                    *active = Some(owned);
                }
                Ok(())
            }
            FrameKind::CommitTransaction | FrameKind::RollbackTransaction => {
                let Ok(selector) = TransactionSelector::decode(frame.payload()) else {
                    self.fatal(
                        cx,
                        request,
                        ErrorCode::Protocol,
                        "malformed transaction selector",
                    )
                    .await;
                    return Err(Stop::Transport);
                };
                let owned = match take_transaction(active, selector.handle) {
                    Ok(owned) => owned,
                    Err(refusal) => {
                        return self
                            .refuse(
                                cx,
                                request,
                                StreamId::CONTROL,
                                binding,
                                refusal.code,
                                &refusal.message,
                            )
                            .await;
                    }
                };
                if frame.header().kind() == FrameKind::RollbackTransaction {
                    let generation = owned.native.generation();
                    owned.native.rollback();
                    let response = Frame::new(
                        FrameKind::TransactionRolledBack,
                        request,
                        StreamId::CONTROL,
                        binding,
                        selector.encode().map_err(|_| Stop::Transport)?,
                        self.send_limits,
                    )
                    .map_err(|_| Stop::Transport)?;
                    return match self.write_frame(cx, &response, Some(generation)).await {
                        Err(Stop::Recovery(error)) => {
                            self.refuse(
                                cx,
                                request,
                                StreamId::CONTROL,
                                binding,
                                error.code(),
                                error.message(),
                            )
                            .await
                        }
                        result => result,
                    };
                }
                // The connection no longer owns this transaction before the
                // completion await. Failure cannot make COMMIT replayable.
                let answer = match owned.native.commit(cx).await {
                    Ok(answer) => answer,
                    Err(refusal) => {
                        return self
                            .refuse(
                                cx,
                                request,
                                StreamId::CONTROL,
                                binding,
                                refusal.code,
                                &refusal.message,
                            )
                            .await;
                    }
                };
                let committed = matches!(&answer.outcome, Outcome::WriteCommitted { .. });
                let body = TransactionCompletion {
                    handle: selector.handle,
                    outcome: answer.outcome,
                };
                let response = Frame::new(
                    FrameKind::TransactionCommitted,
                    request,
                    StreamId::CONTROL,
                    binding,
                    body.encode().map_err(|_| Stop::Transport)?,
                    self.send_limits,
                )
                .map_err(|_| Stop::Transport)?;
                match self
                    .write_frame(cx, &response, Some(answer.generation))
                    .await
                {
                    Err(Stop::Recovery(error)) => {
                        let (code, message) = if committed {
                            (
                                ErrorCode::OutcomeUnknown,
                                "transaction committed before recovery, but completion was not delivered; do not replay the transaction",
                            )
                        } else {
                            (error.code(), error.message())
                        };
                        self.refuse(
                            cx,
                            request,
                            StreamId::CONTROL,
                            binding,
                            code,
                            message,
                        )
                        .await
                    }
                    result => result,
                }
            }
            _ => Err(Stop::Transport),
        }
    }
}
