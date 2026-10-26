//! Finite, authorized result differences over two exact historical cuts.
//! Buffered and external-sort execution share the same weighted framing and
//! control path. Only a complete DIFF_RESULT_END accepts a delivered prefix.

use super::source_stream::SourceStatement;
use super::*;
use crate::execute::ordered::{ordered_generation, read_ordered_session};
use crate::execute::{read_difference, read_prepared_difference};
use fgdb_protocol::body::{DiffMode, DiffResultChunk, DiffResultEnd};
use fgdb_types::PurposeContexts;

type Entry = (i128, Vec<WireValue>);

/// The adapter retains at most one measured entry beyond the current frame.
/// A signed multiplicity is data; flow credit counts changed tuples, not its
/// absolute value. Metadata-only credit must never drive another source pull.
struct Chunks {
    columns: Option<Vec<String>>,
    pending: Option<(Entry, usize)>,
    ended: bool,
}

impl Chunks {
    fn new(columns: Vec<String>) -> Self {
        Self {
            columns: Some(columns),
            pending: None,
            ended: false,
        }
    }

    fn done(&self) -> bool {
        self.ended && self.pending.is_none() && self.columns.is_none()
    }

    fn metadata_bytes(&self) -> usize {
        self.columns.as_ref().map_or(0, |names| {
            4 + names.iter().map(|name| 4 + name.len()).sum::<usize>()
        })
    }

    fn needs_entry(&self, byte_room: usize, row_room: usize) -> bool {
        self.pending.is_none()
            && !self.ended
            && row_room > 0
            && self.metadata_bytes().saturating_add(20) <= byte_room
    }

    fn accept(
        &mut self,
        entry: Option<Entry>,
        ended: bool,
        frame_budget: usize,
    ) -> Result<(), Refusal> {
        self.pending = match entry {
            Some((weight, row)) => {
                if weight == 0 {
                    return Err(Refusal::new(
                        ErrorCode::Execution,
                        "zero-weight difference entry",
                    ));
                }
                let length = DiffResultChunk::entry_len(&row).map_err(|_| {
                    Refusal::new(
                        ErrorCode::Execution,
                        "a difference value exceeds the wire bounds",
                    )
                })?;
                if length > frame_budget {
                    return Err(Refusal::new(
                        ErrorCode::Execution,
                        "a difference entry exceeds the negotiated frame limit",
                    ));
                }
                Some(((weight, row), length))
            }
            None => None,
        };
        self.ended = ended || self.pending.is_none();
        Ok(())
    }

    fn next(
        &mut self,
        byte_room: usize,
        row_room: usize,
        frame_budget: usize,
        mut pull: impl FnMut() -> Result<(Option<Entry>, bool), Refusal>,
    ) -> Result<Option<DiffResultChunk>, Refusal> {
        let mut size = self.metadata_bytes();
        if size > frame_budget {
            return Err(Refusal::new(
                ErrorCode::Execution,
                "the difference column list exceeds the negotiated frame limit",
            ));
        }
        if size > byte_room {
            return Ok(None);
        }
        let mut entries = Vec::new();
        while entries.len() < row_room {
            if self.pending.is_none() {
                if self.ended || size.saturating_add(20) > byte_room {
                    break;
                }
                let (entry, ended) = pull()?;
                self.accept(entry, ended, frame_budget)?;
            }
            let Some((_, length)) = &self.pending else {
                break;
            };
            if size.saturating_add(*length) > byte_room {
                break;
            }
            size += *length;
            entries.push(self.pending.take().expect("measured difference entry").0);
        }
        if entries.is_empty() && self.columns.is_none() {
            return Ok(None);
        }
        Ok(Some(DiffResultChunk {
            columns: self.columns.take(),
            entries,
        }))
    }
}

impl Lane {
    #[allow(clippy::too_many_arguments)]
    async fn difference_chunk(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        stream: StreamId,
        request: u64,
        binding: Binding,
        chunk: DiffResultChunk,
    ) -> Result<u64, Stop> {
        let rows = chunk.entries.len() as u64;
        let frame = Frame::new(
            FrameKind::DiffResultChunk,
            request,
            stream,
            binding,
            chunk.encode().map_err(|_| Stop::Transport)?,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.credited(cx, waiter, window, stream, &frame, rows)
            .await?;
        Ok(rows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn difference_end(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        window: &mut FlowWindow,
        stream: StreamId,
        request: u64,
        binding: Binding,
        end: DiffResultEnd,
    ) -> Result<(), Stop> {
        let frame = Frame::new(
            FrameKind::DiffResultEnd,
            request,
            stream,
            binding,
            end.encode().map_err(|_| Stop::Transport)?,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.credited(cx, waiter, window, stream, &frame, 0).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn difference(
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
        mode: DiffMode,
        before: u64,
        after: u64,
    ) -> Result<(), Stop> {
        // Read authority precedes argument inspection, scratch configuration,
        // and recovery status. Retain this generation through setup as well.
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
        // Column presence and entry count. Each entry includes i128 weight.
        let framing = binding.header_len() + 1 + 4;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        if mode == DiffMode::Buffered {
            let future = match statement {
                SourceStatement::Text(text) => {
                    read_difference(cx, db, token, text, parameters, before, after)
                }
                SourceStatement::Prepared(prepared) => {
                    read_prepared_difference(cx, db, token, prepared, parameters, before, after)
                }
            };
            let answer = match self
                .read_progress(cx, waiter, &mut window, stream, request, binding, future)
                .await?
            {
                Ok(answer) => answer,
                Err(error) => {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            };
            generation.check().map_err(Stop::Recovery)?;
            self.send_generation = Some(answer.generation);
            let mut chunks = Chunks::new(answer.columns);
            let mut entries = answer.entries.into_iter();
            let mut delivered = 0u64;
            while !chunks.done() {
                let available = window.available();
                let byte_room = usize::try_from(available.bytes)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(framing)
                    .min(frame_budget);
                let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
                let chunk = match chunks.next(byte_room, row_room, frame_budget, || {
                    let entry = entries.next();
                    Ok((entry, entries.len() == 0))
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
                delivered = delivered
                    .checked_add(
                        self.difference_chunk(
                            cx,
                            waiter,
                            &mut window,
                            stream,
                            request,
                            binding,
                            chunk,
                        )
                        .await?,
                    )
                    .ok_or(Stop::Transport)?;
            }
            return self
                .difference_end(
                    cx,
                    waiter,
                    &mut window,
                    stream,
                    request,
                    binding,
                    DiffResultEnd {
                        before: answer.before,
                        after: answer.after,
                        rows: delivered,
                    },
                )
                .await;
        }

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
                Ok(imported.expect("prepared difference admission returns its session handle"))
            }
        };
        let execution = match prepared.and_then(|prepared| {
            session.start_difference(&query, &prepared, &parameters, before, after)
        }) {
            Ok(execution) => execution,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        let mut chunks = Chunks::new(execution.columns().to_vec());
        // Publish schema before polling the external sorts. Cancellation now
        // addresses the real child; every source row still remains private.
        loop {
            let byte_room = usize::try_from(window.available().bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            match chunks.next(byte_room, 0, frame_budget, || unreachable!("metadata only")) {
                Ok(Some(chunk)) => {
                    self.difference_chunk(cx, waiter, &mut window, stream, request, binding, chunk)
                        .await?;
                    break;
                }
                Ok(None) => {
                    self.await_credit(cx, waiter, &mut window, stream, request, binding)
                        .await?
                }
                Err(error) => {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            }
        }
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
        let mut delivered = 0u64;
        while !chunks.done() {
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            if chunks.needs_entry(byte_room, row_room) {
                let entry = match self
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
                    Ok(entry) => entry,
                    Err(error) => {
                        return self
                            .refuse(cx, request, stream, binding, error.code, &error.message)
                            .await;
                    }
                };
                if entry.is_none() && !source.exhausted() {
                    return self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            ErrorCode::Execution,
                            "difference cursor stopped before complete exhaustion",
                        )
                        .await;
                }
                if let Err(error) = chunks.accept(entry, source.exhausted(), frame_budget) {
                    return self
                        .refuse(cx, request, stream, binding, error.code, &error.message)
                        .await;
                }
            }
            // Controls handled during a Pending row may have replenished credit.
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            let chunk = match chunks.next(byte_room, row_room.min(1), frame_budget, || {
                Err(Refusal::new(
                    ErrorCode::Execution,
                    "difference entry was not admitted",
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
            delivered = delivered
                .checked_add(
                    self.difference_chunk(cx, waiter, &mut window, stream, request, binding, chunk)
                        .await?,
                )
                .ok_or(Stop::Transport)?;
        }
        self.difference_end(
            cx,
            waiter,
            &mut window,
            stream,
            request,
            binding,
            DiffResultEnd {
                before: source.before(),
                after: source.after(),
                rows: delivered,
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difference_credit_counts_tuples_and_retains_a_measured_entry() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let columns = chunks
            .next(9, 0, 200, || panic!("metadata cannot pull"))
            .ok()
            .flatten()
            .unwrap();
        assert_eq!(columns.columns.unwrap(), ["n"]);
        assert!(columns.entries.is_empty());
        assert!(!chunks.needs_entry(19, 1));
        assert!(!chunks.needs_entry(200, 0));
        let row = vec![WireValue::Text("a large row".into())];
        let size = DiffResultChunk::entry_len(&row).unwrap();
        chunks
            .accept(Some((-37, row.clone())), true, size)
            .unwrap_or_else(|_| panic!("fits"));
        assert!(
            chunks
                .next(size - 1, 1, size, || panic!("no second pull"))
                .ok()
                .flatten()
                .is_none()
        );
        let chunk = chunks
            .next(size, 1, size, || panic!("no second pull"))
            .ok()
            .flatten()
            .unwrap();
        assert_eq!(chunk.entries, [(-37, row)]);
        assert!(chunks.done());
    }
}
