//! Complete native admission precedes every byte of a consistent read batch.
//! Its ordered results share one child, one flow window and one terminal.

use super::*;
use crate::execute::ordered::ordered_generation;
use crate::execute::{ReadBatchAnswer, read_batch as execute_read_batch};
use fgdb_protocol::body::{
    BodyError, MAX_COLUMNS, MAX_NAME_BYTES, MAX_ROWS_PER_CHUNK, MAX_VALUE_NODES,
    ReadBatchResultChunk, ReadBatchResultEnd,
};
use fgdb_types::PurposeContexts;

const BATCH_PREFIX_BYTES: usize = 4 + 8 + 1;

fn charge_value_nodes(value: &WireValue, remaining: &mut usize) -> Result<(), BodyError> {
    *remaining = remaining.checked_sub(1).ok_or(BodyError::TooLarge)?;
    match value {
        WireValue::List(values) => {
            for value in values {
                charge_value_nodes(value, remaining)?;
            }
        }
        WireValue::Map(entries) => {
            for (_, value) in entries {
                charge_value_nodes(value, remaining)?;
            }
        }
        WireValue::Path { steps, .. } => {
            *remaining = remaining
                .checked_sub(steps.len())
                .ok_or(BodyError::TooLarge)?;
        }
        WireValue::Vertices(ids) | WireValue::Edges(ids) => {
            *remaining = remaining
                .checked_sub(ids.len())
                .ok_or(BodyError::TooLarge)?;
        }
        _ => {}
    }
    Ok(())
}

/// Encoding checks each tree's depth and width, while decoding also bounds
/// the sum of nodes across every cell in the body. Enforce both before send,
/// including for operators who negotiate frames larger than that node bound.
fn row_cost(row: &[WireValue]) -> Result<(usize, usize), BodyError> {
    let bytes = ResultChunk::row_len(row)?;
    let mut remaining = MAX_VALUE_NODES;
    for value in row {
        charge_value_nodes(value, &mut remaining)?;
    }
    Ok((bytes, MAX_VALUE_NODES - remaining))
}

/// Refuse an unrepresentable later result before a successful prefix escapes.
/// Only the already admitted output is visited; no graph is rescanned and no
/// second execution permit is minted. Byte and shape limits are independent
/// of whichever flow credit happens to be available at delivery time.
fn preflight(
    answer: &ReadBatchAnswer,
    expected: usize,
    frame_budget: usize,
    mut checkpoint: impl FnMut() -> Result<(), Refusal>,
) -> Result<(), Refusal> {
    let invalid = |message| Refusal::new(ErrorCode::Execution, message);
    if answer.results.len() != expected {
        return Err(invalid(
            "native read batch result count differs from its request",
        ));
    }
    for result in &answer.results {
        checkpoint()?;
        if result.columns.len() > MAX_COLUMNS
            || result
                .columns
                .iter()
                .any(|name| name.len() > MAX_NAME_BYTES)
        {
            return Err(invalid("a read batch column list exceeds the wire bounds"));
        }
        let metadata = result.columns.iter().try_fold(4_usize, |size, name| {
            size.checked_add(4)?.checked_add(name.len())
        });
        if metadata.is_none_or(|size| size > frame_budget) {
            return Err(invalid(
                "a read batch column list exceeds the negotiated frame limit",
            ));
        }
        for row in &result.rows {
            checkpoint()?;
            if row.len() != result.columns.len() {
                return Err(invalid("a read batch row width differs from its columns"));
            }
            let (length, _) =
                row_cost(row).map_err(|_| invalid("a read batch value exceeds the wire bounds"))?;
            if length > frame_budget {
                return Err(invalid(
                    "a read batch row exceeds the negotiated frame limit",
                ));
            }
        }
    }
    Ok(())
}

impl Lane {
    /// Native and conversion failures can contain data-dependent diagnostics.
    /// Unlike generic recovery notices, these retain their source generation
    /// through every physical write and flush, just like result fragments.
    async fn refuse_read_batch(
        &mut self,
        cx: &Cx,
        request: u64,
        stream: StreamId,
        binding: Binding,
        error: Refusal,
    ) -> Result<(), Stop> {
        let frame = Frame::new(
            FrameKind::Error,
            request,
            stream,
            binding,
            ErrorBody {
                code: error.code,
                message: error.message,
            }
            .encode()
            .map_err(|_| Stop::Transport)?,
            self.send_limits,
        )
        .map_err(|_| Stop::Transport)?;
        self.send_generation_frame(cx, &frame).await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn read_batch(
        &mut self,
        cx: &Cx,
        waiter: &Waiter,
        db: &Served,
        token: &CapabilityToken,
        request: u64,
        stream: StreamId,
        binding: Binding,
        statement: &ExecuteBatch,
    ) -> Result<(), Stop> {
        // Read rights precede recovery/configuration inspection. This same
        // generation fences admission, data-dependent errors and every send.
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
        let answer = self
            .read_progress(
                cx,
                waiter,
                &mut window,
                stream,
                request,
                binding,
                execute_read_batch(cx, db, token, statement),
            )
            .await?;
        generation.check().map_err(Stop::Recovery)?;
        let answer = match answer {
            Ok(answer) => answer,
            Err(error) => {
                return self
                    .refuse_read_batch(cx, request, stream, binding, error)
                    .await;
            }
        };
        let framing = binding.header_len() + BATCH_PREFIX_BYTES + CHUNK_OVERHEAD;
        let frame_budget = self.send_limits.max_frame_len() - framing;
        let contexts = PurposeContexts::narrow_runtime_root(cx);
        let query = crate::query_cx(&contexts);
        if let Err(error) = preflight(&answer, statement.argument_sets.len(), frame_budget, || {
            query
                .checkpoint()
                .map_err(|error| Refusal::new(ErrorCode::Cancelled, error.to_string()))?;
            generation.check().map_err(Refusal::from)
        }) {
            // Never release data-dependent preflight diagnostics after the
            // generation that produced them has been replaced.
            generation.check().map_err(Stop::Recovery)?;
            return self
                .refuse_read_batch(cx, request, stream, binding, error)
                .await;
        }
        self.send_generation = Some(answer.generation);
        let results = u32::try_from(answer.results.len()).map_err(|_| Stop::Transport)?;
        let mut delivered = 0_u64;
        for (index, result) in answer.results.into_iter().enumerate() {
            let index = u32::try_from(index).map_err(|_| Stop::Transport)?;
            let mut columns = Some(result.columns);
            let mut rows = result.rows.into_iter().peekable();
            while columns.is_some() || rows.peek().is_some() {
                let available = window.available();
                let byte_room = usize::try_from(available.bytes)
                    .unwrap_or(usize::MAX)
                    .saturating_sub(framing)
                    .min(frame_budget);
                let row_room = usize::try_from(available.rows)
                    .unwrap_or(usize::MAX)
                    .min(MAX_ROWS_PER_CHUNK);
                let mut size = columns.as_ref().map_or(0, |names| {
                    4 + names.iter().map(|name| 4 + name.len()).sum::<usize>()
                });
                let mut chunk_rows = Vec::new();
                let mut chunk_nodes = 0_usize;
                if size <= byte_room {
                    while chunk_rows.len() < row_room {
                        let Some(row) = rows.peek() else { break };
                        let (length, nodes) = row_cost(row).map_err(|_| Stop::Transport)?;
                        if size.saturating_add(length) > byte_room
                            || chunk_nodes.saturating_add(nodes) > MAX_VALUE_NODES
                        {
                            break;
                        }
                        size += length;
                        chunk_nodes += nodes;
                        chunk_rows.extend(rows.next());
                    }
                }
                if size > byte_room || (chunk_rows.is_empty() && columns.is_none()) {
                    self.await_credit(cx, waiter, &mut window, stream, request, binding)
                        .await?;
                    continue;
                }
                let count = chunk_rows.len() as u64;
                let part = ReadBatchResultChunk {
                    index,
                    seq: result.seq,
                    last: rows.peek().is_none(),
                    chunk: ResultChunk {
                        columns: columns.take(),
                        rows: chunk_rows,
                    },
                };
                let frame = Frame::new(
                    FrameKind::ReadBatchResultChunk,
                    request,
                    stream,
                    binding,
                    part.encode().map_err(|_| Stop::Transport)?,
                    self.send_limits,
                )
                .map_err(|_| Stop::Transport)?;
                // credited uses send_generation_frame, including a fresh
                // Warden guard on every physical write and resumed flush.
                self.credited(cx, waiter, &mut window, stream, &frame, count)
                    .await?;
                delivered = delivered.checked_add(count).ok_or(Stop::Transport)?;
            }
        }
        let frame = Frame::new(
            FrameKind::ReadBatchResultEnd,
            request,
            stream,
            binding,
            ReadBatchResultEnd {
                results,
                rows: delivered,
            }
            .encode()
            .map_err(|_| Stop::Transport)?,
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

    #[test]
    fn nested_cells_share_the_body_node_allowance() {
        let value = WireValue::Map(vec![(
            "items".into(),
            WireValue::List(vec![
                WireValue::Null,
                WireValue::List(vec![WireValue::Null]),
            ]),
        )]);
        // The map, its list, both nulls and the nested list each cost a node.
        assert_eq!(row_cost(std::slice::from_ref(&value)).unwrap().1, 5);
        let mut allowance = 5;
        charge_value_nodes(&value, &mut allowance).unwrap();
        assert_eq!(allowance, 0);
        assert_eq!(
            charge_value_nodes(&WireValue::Null, &mut allowance),
            Err(BodyError::TooLarge)
        );
        let mut allowance = 4;
        assert_eq!(
            charge_value_nodes(&value, &mut allowance),
            Err(BodyError::TooLarge)
        );
        for value in [
            WireValue::Path {
                start: 1,
                steps: vec![(2, 3)],
            },
            WireValue::Vertices(vec![1]),
            WireValue::Edges(vec![1]),
        ] {
            assert_eq!(row_cost(std::slice::from_ref(&value)).unwrap().1, 2);
            let mut allowance = 1;
            assert_eq!(
                charge_value_nodes(&value, &mut allowance),
                Err(BodyError::TooLarge)
            );
        }
    }
}
