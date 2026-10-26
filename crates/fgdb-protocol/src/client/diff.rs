//! Finite historical comparisons retain exact endpoints and signed changes.

use super::*;
use crate::body::{DiffMode, DiffResultChunk, DiffResultEnd, ExecuteDiff, ExecutePreparedDiff};

/// One fully received finite difference. Each entry is a changed tuple with
/// its exact signed multiplicity. A streaming prefix is not a complete answer;
/// this value exists only after its endpoint pair and terminal count agree.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffAnswer {
    pub columns: Vec<String>,
    pub entries: Vec<(i128, Vec<WireValue>)>,
    pub end: DiffResultEnd,
}

impl Client {
    /// Compare a read at two exact revisions, collecting its complete NET
    /// result changes. Reverse and equal endpoints retain their exact meaning.
    /// This is after-minus-before bag comparison, not an event log, branch
    /// comparison, subscription, or resumable result. No retry is performed.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_diff(
        &mut self,
        cx: &Cx,
        mode: DiffMode,
        before: u64,
        after: u64,
        statement: &str,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<DiffAnswer, ClientError> {
        let mut columns = Vec::new();
        let mut entries = Vec::new();
        let end = self
            .execute_diff_streaming(
                cx,
                mode,
                before,
                after,
                statement,
                parameters,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |weight, row| {
                    entries.push((weight, row));
                    Ok(())
                },
            )
            .await?;
        Ok(DiffAnswer {
            columns,
            entries,
            end,
        })
    }

    /// Deliver finite historical changes under ordinary byte and changed-row
    /// credit. Callbacks may receive a provisional prefix; only a successful
    /// return confirms complete delivery at the requested endpoints. A callback
    /// error cancels and drains the child before it is returned, allowing the
    /// next request to use the connection. No fallback or query retry occurs.
    ///
    /// Buffered mode completes comparison in memory before delivery. Ordered
    /// mode uses configured server scratch and refuses unsupported profiles.
    /// Both retain one execution's authority and compare complete native query
    /// results, including each endpoint's ordering, DISTINCT and pagination.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_diff_streaming(
        &mut self,
        cx: &Cx,
        mode: DiffMode,
        before: u64,
        after: u64,
        statement: &str,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_entry: impl FnMut(i128, Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<DiffResultEnd, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecuteDiff,
                StreamId::CONTROL,
                &ExecuteDiff {
                    mode,
                    before,
                    after,
                    statement: statement.to_owned(),
                    parameters,
                },
            )
            .await?;
        self.receive_diff(cx, request, before, after, on_columns, on_entry)
            .await
    }

    /// Rebind a connection-owned read template for both exact cuts and collect
    /// one finite difference. The server rechecks live authority; preparation
    /// does not retain a snapshot or execution permission.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_diff(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        mode: DiffMode,
        before: u64,
        after: u64,
        parameters: Vec<(String, WireValue)>,
    ) -> Result<DiffAnswer, ClientError> {
        let mut columns = Vec::new();
        let mut entries = Vec::new();
        let end = self
            .execute_prepared_diff_streaming(
                cx,
                handle,
                mode,
                before,
                after,
                parameters,
                |names| {
                    columns = names.to_vec();
                    Ok(())
                },
                |weight, row| {
                    entries.push((weight, row));
                    Ok(())
                },
            )
            .await?;
        Ok(DiffAnswer {
            columns,
            entries,
            end,
        })
    }

    /// Stream a prepared read's finite difference with the same provisional
    /// prefix, cancellation, credit and completion checks as execute_diff_streaming.
    /// A write handle, released handle, or foreign-connection handle refuses.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_prepared_diff_streaming(
        &mut self,
        cx: &Cx,
        handle: PreparedHandle,
        mode: DiffMode,
        before: u64,
        after: u64,
        mut parameters: Vec<(String, WireValue)>,
        on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        on_entry: impl FnMut(i128, Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<DiffResultEnd, ClientError> {
        if self.selected.is_none() {
            return Err(ClientError::Protocol("select a database first"));
        }
        parameters.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let request = self
            .send(
                cx,
                FrameKind::ExecutePreparedDiff,
                StreamId::CONTROL,
                &ExecutePreparedDiff {
                    mode,
                    before,
                    after,
                    handle,
                    parameters,
                },
            )
            .await?;
        self.receive_diff(cx, request, before, after, on_columns, on_entry)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn receive_diff(
        &mut self,
        cx: &Cx,
        request: u64,
        before: u64,
        after: u64,
        mut on_columns: impl FnMut(&[String]) -> Result<(), ClientError>,
        mut on_entry: impl FnMut(i128, Vec<WireValue>) -> Result<(), ClientError>,
    ) -> Result<DiffResultEnd, ClientError> {
        let mut stream = None;
        let mut columns = None;
        let mut rows = 0_u64;
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
                return Err(ClientError::Protocol("diff frame for a different request"));
            }
            match stream {
                None if !header.stream_id().is_control() => stream = Some(header.stream_id()),
                Some(id) if id != header.stream_id() => {
                    return Err(ClientError::Protocol("diff frame on a foreign stream"));
                }
                _ => {}
            }
            match header.kind() {
                FrameKind::DiffResultChunk => {
                    let Some(id) = stream else {
                        return Err(ClientError::Protocol("diff chunk on the control stream"));
                    };
                    let chunk = DiffResultChunk::decode(frame.payload())?;
                    let count = chunk.entries.len() as u64;
                    let cost = header.frame_len() as u64;
                    if cost > available.0 || count > available.1 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    available = (available.0 - cost, available.1 - count);
                    match (&chunk.columns, columns) {
                        (Some(names), None) => columns = Some(names.len()),
                        (None, Some(_)) => {}
                        _ => return Err(ClientError::Protocol("diff columns out of order")),
                    }
                    let width = columns.expect("first diff chunk carries its columns");
                    if chunk.entries.iter().any(|(_, row)| row.len() != width) {
                        return Err(ClientError::Protocol("diff row width differs from columns"));
                    }
                    rows = rows
                        .checked_add(count)
                        .ok_or(ClientError::Protocol("diff row count overflow"))?;
                    if let Some(names) = chunk.columns
                        && let Err(error) = on_columns(&names)
                    {
                        self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                        callback_error = Some(error);
                    }
                    for (weight, row) in chunk.entries {
                        if callback_error.is_none()
                            && let Err(error) = on_entry(weight, row)
                        {
                            self.send(cx, FrameKind::QueryCancel, id, &Empty).await?;
                            callback_error = Some(error);
                        }
                    }
                    let frame_limit = self.send_limits.max_frame_len() as u64;
                    if callback_error.is_none() && (available.0 < frame_limit || available.1 == 0) {
                        grants = grants
                            .checked_add(1)
                            .ok_or(ClientError::Protocol("diff flow sequence overflow"))?;
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
                FrameKind::DiffResultEnd => {
                    if header.frame_len() as u64 > available.0 {
                        return Err(ClientError::Protocol("server exceeded its flow credit"));
                    }
                    if stream.is_none() || columns.is_none() {
                        return Err(ClientError::Protocol("diff END before columns"));
                    }
                    let end = DiffResultEnd::decode(frame.payload())?;
                    if end.before != before || end.after != after {
                        return Err(ClientError::Protocol(
                            "diff END changed the requested endpoints",
                        ));
                    }
                    if end.rows != rows {
                        return Err(ClientError::Protocol(
                            "diff END row count differs from delivery",
                        ));
                    }
                    return callback_error.map_or(Ok(end), Err);
                }
                FrameKind::Error => {
                    let refusal = ErrorBody::decode(frame.payload())?;
                    return Err(callback_error.unwrap_or(ClientError::Server {
                        code: refusal.code,
                        message: refusal.message,
                    }));
                }
                _ => return Err(ClientError::Protocol("unexpected frame in a finite diff")),
            }
        }
    }
}
