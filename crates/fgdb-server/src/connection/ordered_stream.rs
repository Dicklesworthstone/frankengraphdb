//! Private complete ordering followed by credit-driven final frame reads.
//! A metadata-only first chunk announces the child before the sort is polled,
//! so clients can cancel expensive ordering by its real stream identity.

use super::source_stream::{Chunks, SourceStatement};
use super::*;
use crate::execute::ordered::{ordered_generation, read_ordered_session};
use core::future::Future;
use fgdb_types::PurposeContexts;

enum Progress<T> {
    Complete(Result<T, Refusal>),
    Inbound(Box<Frame>),
}

#[cfg(test)]
#[path = "ordered_stream/tests.rs"]
mod tests;

impl Lane {
    /// Own and pin one future outside the control loop. A PING or credit
    /// update cannot drop/recreate a partially advanced spill operation.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn read_progress<T>(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        stream: StreamId,
        request: u64,
        binding: Binding,
        future: impl Future<Output = Result<T, Refusal>>,
    ) -> Result<Result<T, Refusal>, Stop> {
        let generation = self.send_generation.clone().ok_or(Stop::Transport)?;
        let mut watcher = generation.watcher();
        let mut future = std::pin::pin!(future);
        loop {
            let progress = {
                let Self {
                    reader,
                    conn,
                    finished,
                    send_authority,
                    ..
                } = self;
                poll_fn(|task| {
                    while watcher.poll_changed(task) {}
                    if let Err(error) = generation.check() {
                        return Poll::Ready(Err(Stop::Recovery(error)));
                    }
                    if waiter.poll_triggered(task) {
                        return Poll::Ready(Err(Stop::Drain));
                    }
                    if send_authority
                        .as_ref()
                        .is_none_or(|(db, token)| !db.admits(token))
                    {
                        return Poll::Ready(Err(Stop::Transport));
                    }
                    // Controls already received win over another source poll.
                    // The FrameReader retains partial input across polls.
                    match reader.poll_receive(cx, task, |header| validate(conn, finished, header)) {
                        Poll::Ready(Ok(Some(frame))) => {
                            return Poll::Ready(Ok(Progress::Inbound(Box::new(frame))));
                        }
                        Poll::Ready(Ok(None) | Err(_)) => return Poll::Ready(Err(Stop::Transport)),
                        Poll::Pending => {}
                    }
                    future
                        .as_mut()
                        .poll(task)
                        .map(|result| Ok(Progress::Complete(result)))
                })
                .await?
            };
            match progress {
                Progress::Complete(result) => return Ok(result),
                Progress::Inbound(frame) => {
                    self.stream_control(cx, window, stream, request, binding, &frame)
                        .await?
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn ordered_chunk(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        stream: StreamId,
        request: u64,
        binding: Binding,
        chunk: ResultChunk,
    ) -> Result<u64, Stop> {
        let count = chunk.rows.len() as u64;
        let payload = chunk.encode().map_err(|_| Stop::Transport)?;
        let frame = Frame::new(
            FrameKind::SnapshotResultChunk,
            request,
            stream,
            binding,
            payload,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.credited(cx, waiter, window, stream, &frame, count)
            .await?;
        Ok(count)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn ordered_read(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &Served,
        token: &CapabilityToken,
        request: u64,
        stream: StreamId,
        binding: Binding,
        statement: SourceStatement<'_>,
        parameters: &[(String, WireValue)],
    ) -> Result<(), Stop> {
        let parameters = match crate::convert::parameters(parameters, None) {
            Ok(parameters) => parameters,
            Err(error) => {
                return self
                    .refuse(
                        cx,
                        request,
                        stream,
                        binding,
                        ErrorCode::Statement,
                        &error.to_string(),
                    )
                    .await;
            }
        };
        let definition = match statement {
            SourceStatement::Prepared(prepared) => Some(prepared),
            SourceStatement::Text(_) => None,
        };
        let generation = match ordered_generation(db, token) {
            Ok(generation) => generation,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        self.send_generation = Some(generation.clone());
        let mut window = FlowWindow::new(self.initial_window, self.maximum_window)
            .map_err(|_| Stop::Transport)?;
        // Admission owns its one filesystem future across PING/credit races
        // too. No schema exists yet, but disconnect, drain and recovery can
        // release this connection without waiting for an outstanding syscall.
        let (mut session, imported) = match self
            .read_progress(
                cx,
                waiter,
                &mut window,
                stream,
                request,
                binding,
                read_ordered_session(cx, db, token, definition, generation),
            )
            .await?
        {
            Ok(session) => session,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        self.send_generation = Some(session.generation());
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        let prepared = match statement {
            SourceStatement::Text(text) => session.prepare(&query, text, &parameters),
            SourceStatement::Prepared(_) => {
                Ok(imported.expect("prepared ordered admission returns its session handle"))
            }
        };
        let execution =
            match prepared.and_then(|prepared| session.start(&query, &prepared, &parameters)) {
                Ok(execution) => execution,
                Err(error) => {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            };
        let framing = binding.header_len() + CHUNK_OVERHEAD;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        let mut chunks = Chunks::new(execution.columns().to_vec());
        let byte_room = usize::try_from(window.available().bytes)
            .unwrap_or(usize::MAX)
            .saturating_sub(framing)
            .min(frame_budget);
        // This first chunk is charged to the same flow window. No source row
        // or sort I/O is demanded before the client can name its child stream.
        let columns = match chunks.next(byte_room, 0, frame_budget, || {
            unreachable!("metadata never pulls")
        }) {
            Ok(Some(columns)) => columns,
            Ok(None) => return Err(Stop::Transport),
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        self.ordered_chunk(cx, waiter, &mut window, stream, request, binding, columns)
            .await?;
        let mut source = match self
            .read_progress(
                cx,
                waiter,
                &mut window,
                stream,
                request,
                binding,
                execution.finish(),
            )
            .await?
        {
            Ok(source) => source,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        let outcome = Outcome::Rows {
            seq: source.snapshot_seq(),
        };
        let mut delivered = 0u64;
        while !chunks.done() {
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            if chunks.needs_row(byte_room, row_room) {
                let row = match self
                    .read_progress(
                        cx,
                        waiter,
                        &mut window,
                        stream,
                        request,
                        binding,
                        source.next(),
                    )
                    .await?
                {
                    Ok(row) => row,
                    Err(error) => {
                        return self
                            .refuse(cx, request, stream, binding, error.code, &error.message)
                            .await;
                    }
                };
                if let Err(error) = chunks.accept(row, source.exhausted(), frame_budget) {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            }
            // Grants handled while the same row future was Pending already
            // belong to this window. Do not wait for a second grant using the
            // byte room sampled before that await.
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            // One authenticated row per chunk keeps the async admission and
            // exact byte-credit boundary simple. The pending measured row is
            // retained unchanged if only its minimum encoding fit the credit.
            let chunk = match chunks.next(byte_room, row_room.min(1), frame_budget, || {
                Err(Refusal::new(
                    ErrorCode::Execution,
                    "ordered row was not admitted",
                ))
            }) {
                Ok(Some(chunk)) => chunk,
                Ok(None) if chunks.done() => break,
                Ok(None) => {
                    self.await_credit(cx, waiter, &mut window, stream, request, binding)
                        .await?;
                    continue;
                }
                Err(error) => {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            };
            delivered += self
                .ordered_chunk(cx, waiter, &mut window, stream, request, binding, chunk)
                .await?;
        }
        let end = ResultEnd {
            outcome,
            rows: delivered,
        };
        let frame = Frame::new(
            FrameKind::SnapshotResultEnd,
            request,
            stream,
            binding,
            end.encode().map_err(|_| Stop::Transport)?,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.credited(cx, waiter, &mut window, stream, &frame, 0)
            .await
    }
}
