//! Credit-driven native source reads, with one retained session and permit.
//! A frame and at most one row awaiting byte credit are the only row buffers.

use super::*;
use crate::execute::read_session;
use fgdb_types::PurposeContexts;

struct Chunks {
    columns: Option<Vec<String>>,
    pending: Option<(Vec<WireValue>, usize)>,
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

    /// Pull only when there is credit for a row and its smallest possible
    /// encoding. Exact byte length is known after native projection, so one
    /// row may wait for byte credit. It is never pulled again or requeries.
    fn next(
        &mut self,
        byte_room: usize,
        row_room: usize,
        frame_budget: usize,
        mut pull: impl FnMut() -> Result<(Option<Vec<WireValue>>, bool), Refusal>,
    ) -> Result<Option<ResultChunk>, Refusal> {
        let mut size = self.columns.as_ref().map_or(0, |names| {
            4 + names.iter().map(|name| 4 + name.len()).sum::<usize>()
        });
        if size > frame_budget {
            return Err(Refusal::new(
                ErrorCode::Execution,
                "the column list exceeds the negotiated frame limit",
            ));
        }
        if size > byte_room {
            return Ok(None);
        }
        let mut rows = Vec::new();
        while rows.len() < row_room {
            if self.pending.is_none() {
                // The row's column count alone requires four bytes. A header
                // or column-only grant must not drive graph candidates.
                if self.ended || size.saturating_add(4) > byte_room {
                    break;
                }
                let (row, ended) = pull()?;
                let empty = row.is_none();
                self.pending = match row {
                    Some(row) => {
                        // row_len validates/encodes recursively. Measure once,
                        // never again for a ping or insufficient credit grant.
                        let length = ResultChunk::row_len(&row).map_err(|_| {
                            Refusal::new(
                                ErrorCode::Execution,
                                "a result value exceeds the wire bounds",
                            )
                        })?;
                        if length > frame_budget {
                            return Err(Refusal::new(
                                ErrorCode::Execution,
                                "a result row exceeds the negotiated frame limit",
                            ));
                        }
                        Some((row, length))
                    }
                    None => None,
                };
                self.ended = ended || empty;
            }
            let Some((_, length)) = &self.pending else {
                break;
            };
            if size.saturating_add(*length) > byte_room {
                break;
            }
            size += *length;
            rows.push(self.pending.take().expect("measured pending row").0);
        }
        if rows.is_empty() && self.columns.is_none() {
            return Ok(None);
        }
        Ok(Some(ResultChunk {
            columns: self.columns.take(),
            rows,
        }))
    }
}

impl Lane {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn source_read(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &Served,
        token: &CapabilityToken,
        request: u64,
        stream: StreamId,
        binding: Binding,
        statement: &Execute,
    ) -> Result<(), Stop> {
        let parameters = match crate::convert::parameters(&statement.parameters, None) {
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
        let (mut session, _) = match read_session(cx, db, token).await {
            Ok(opened) => opened,
            Err(error) => {
                return self
                    .refuse(cx, request, stream, binding, error.code, &error.message)
                    .await;
            }
        };
        self.send_generation = Some(session.generation());
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = contexts.query();
        let mut source = match session.stream(&query, &statement.statement, &parameters) {
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
        let mut chunks = Chunks::new(source.columns().to_vec());
        let mut window = FlowWindow::new(self.initial_window, self.maximum_window)
            .map_err(|_| Stop::Transport)?;
        let framing = binding.header_len() + CHUNK_OVERHEAD;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        let mut delivered = 0u64;
        while !chunks.done() {
            let available = window.available();
            let byte_room = usize::try_from(available.bytes)
                .unwrap_or(usize::MAX)
                .saturating_sub(framing)
                .min(frame_budget);
            let row_room = usize::try_from(available.rows).unwrap_or(usize::MAX);
            let next = chunks.next(byte_room, row_room, frame_budget, || {
                let row = source.next()?;
                Ok((row, source.exhausted()))
            });
            let chunk = match next {
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
            let count = chunk.rows.len() as u64;
            let payload = match chunk.encode() {
                Ok(payload) => payload,
                Err(_) => {
                    return self
                        .refuse(
                            cx,
                            request,
                            stream,
                            binding,
                            ErrorCode::Execution,
                            "a result value exceeds the wire bounds",
                        )
                        .await;
                }
            };
            let frame = Frame::new(
                FrameKind::SnapshotResultChunk,
                request,
                stream,
                binding,
                payload,
                self.send_limits,
            )
            .map_err(|_| Stop::Transport)?;
            // Existing output guards recheck live capability and recovery at
            // each physical write/flush, including after suspended byte credit.
            self.credited(cx, waiter, &mut window, stream, &frame, count)
                .await?;
            delivered += count;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted(result: Result<Option<ResultChunk>, Refusal>) -> Option<ResultChunk> {
        result.unwrap_or_else(|error| panic!("unexpected refusal: {}", error.message))
    }

    #[test]
    fn zero_row_or_payload_credit_never_pulls_and_byte_wait_retains_one_row() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let metadata = accepted(chunks.next(9, 0, 128, || panic!("no row credit"))).unwrap();
        assert_eq!(metadata.columns, Some(vec!["n".into()]));
        assert!(metadata.rows.is_empty());
        assert!(accepted(chunks.next(0, 4, 128, || panic!("no payload credit"))).is_none());
        assert!(accepted(chunks.next(128, 0, 128, || panic!("no row credit"))).is_none());

        let mut pulls = 0;
        let row = vec![WireValue::Text("waiting for byte credit".into())];
        let size = ResultChunk::row_len(&row).unwrap();
        assert!(
            accepted(chunks.next(4, 1, 128, || {
                pulls += 1;
                Ok((Some(row.clone()), false))
            }))
            .is_none()
        );
        assert_eq!(pulls, 1);
        for bytes in [0, 4, size - 1] {
            assert!(
                accepted(chunks.next(bytes, 10, 128, || panic!("pending row was repulled")))
                    .is_none()
            );
        }
        let ready =
            accepted(chunks.next(size, 1, 128, || panic!("pending row was repulled"))).unwrap();
        assert_eq!(ready.rows, [row]);
        assert!(!chunks.done());
        assert!(
            accepted(chunks.next(128, 1, 128, || {
                pulls += 1;
                Ok((None, true))
            }))
            .is_none()
        );
        assert!(chunks.done());
        assert_eq!(pulls, 2);
    }

    #[test]
    fn exact_row_credit_never_reads_a_suffix_and_native_limit_needs_no_eof_pull() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let mut pulls = 0;
        let first = accepted(chunks.next(128, 1, 128, || {
            pulls += 1;
            Ok((Some(vec![WireValue::Int(pulls)]), false))
        }))
        .unwrap();
        assert_eq!(first.rows, [vec![WireValue::Int(1)]]);
        assert_eq!(pulls, 1);
        assert!(accepted(chunks.next(128, 0, 128, || panic!("exhausted row credit"))).is_none());
        let last = accepted(chunks.next(128, 1, 128, || {
            pulls += 1;
            Ok((Some(vec![WireValue::Int(pulls)]), true))
        }))
        .unwrap();
        assert_eq!(last.rows, [vec![WireValue::Int(2)]]);
        assert!(chunks.done());
        assert_eq!(pulls, 2);
        assert!(accepted(chunks.next(128, 1, 128, || panic!("LIMIT drained source"))).is_none());
        // Dropping a cancelled connection's framing state has no source work.
        drop(chunks);
        assert_eq!(pulls, 2);
    }

    #[test]
    fn a_late_native_refusal_is_not_eof_or_an_oversized_private_chunk() {
        let mut chunks = Chunks::new(vec!["n".into()]);
        let first =
            accepted(chunks.next(128, 1, 128, || Ok((Some(vec![WireValue::Int(1)]), false))))
                .unwrap();
        assert_eq!(first.rows.len(), 1);
        let error = chunks
            .next(128, 1, 128, || {
                Err(Refusal::new(
                    ErrorCode::Budget,
                    "retained native allowance exhausted",
                ))
            })
            .expect_err("native refusal");
        assert_eq!(error.code, ErrorCode::Budget);
        assert!(!chunks.done());

        let error = chunks
            .next(128, 1, 128, || {
                Ok((Some(vec![WireValue::Text("x".repeat(256))]), true))
            })
            .expect_err("oversized row refusal");
        assert_eq!(error.code, ErrorCode::Execution);
        assert!(!chunks.done());
    }
}
