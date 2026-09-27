//! Bounded-buffer I/O for the existing query-private scratch authority.
//! Windows are withheld until the WHOLE run passes its original checksum.

use super::*;

impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> SpillFile<F> {
    /// Restore a byte window without materializing the complete scratch run.
    ///
    /// start/len are relative to the opaque run, never to the file. Invalid or
    /// overflowing windows refuse before I/O. The entire run is read and hashed
    /// ONCE, including bytes outside the window; no unverified prefix escapes.
    /// An empty window still checks all bytes. This is O(run length) I/O per
    /// call, not a sublinear authenticated seek or a durable storage reader.
    ///
    /// The output and a reusable buffer of at most 64 KiB are admitted through
    /// the same hierarchical MemoryPool. Buffer size shrinks to its available
    /// headroom. A proper window needs its output plus at least one byte of
    /// scratch; a whole-run request reuses restore's single allocation. Neither
    /// a returned window nor its memory charge can be detached from its bytes.
    ///
    /// Source failures, cancellation and unwinding refund all local buffers.
    /// An unfinished seek/read poisons the file as in restore; completed I/O
    /// followed by checksum/final-checkpoint refusal does not. Only accepted
    /// windows increment restored_runs. No new run handle or authority is made.
    pub async fn restore_window(
        &mut self,
        cx: &QueryCx,
        run: &SpillRun,
        start: usize,
        len: usize,
    ) -> Result<TrackedBytes, SpillError> {
        cx.with_restriction_async(self.restore_window_inner(run, start, len, || {
            cx.checkpoint().map_err(SpillError::Interrupted)
        }))
        .await
    }

    pub(super) async fn restore_window_inner(
        &mut self,
        run: &SpillRun,
        start: usize,
        len: usize,
        mut checkpoint: impl FnMut() -> Result<(), SpillError>,
    ) -> Result<TrackedBytes, SpillError> {
        checkpoint()?;
        if self.io_pending {
            return Err(SpillError::PoisonedFile);
        }
        if !Arc::ptr_eq(&self.owner, &run.owner) {
            return Err(SpillError::ForeignRun);
        }
        let run_len = u64::try_from(run.len).map_err(|_| SpillError::SizeOverflow)?;
        let extent_end = run.offset.checked_add(run_len).ok_or(SpillError::InvalidRun)?;
        let window_end = start.checked_add(len).ok_or(SpillError::InvalidRun)?;
        if run.id == 0
            || run.id > self.stats.reserved_runs
            || run.len > self.limits.max_run_bytes
            || extent_end > self.stats.reserved_bytes
            || start > run.len
            || window_end > run.len
        {
            return Err(SpillError::InvalidRun);
        }
        if start == 0 && len == run.len {
            // No second allocation for the full-window or empty-run cases.
            return self.restore_inner(run, checkpoint).await;
        }
        let mut output = self.pool.allocate_inner(len, 0)?;
        let mut buffer = self.stream_buffer(run.len)?;
        self.io_pending = true;
        let actual = self.file.seek(SeekFrom::Start(run.offset)).await?;
        if actual != run.offset {
            return Err(SpillError::UnexpectedPosition {
                expected: run.offset,
                actual,
            });
        }
        let mut hash = run_hasher(run.id, run.offset, run_len);
        let mut position = 0;
        while position < run.len {
            checkpoint()?;
            let count = buffer.len().min(run.len - position);
            let chunk = &mut buffer.as_mut()[..count];
            self.file.read_exact(chunk).await?;
            hash.update(chunk);
            let next = position + count; // count <= run.len - position
            let left = position.max(start);
            let right = next.min(window_end);
            if left < right {
                output.as_mut()[left - start..right - start]
                    .copy_from_slice(&chunk[left - position..right - position]);
            }
            position = next;
        }
        self.io_pending = false;
        if hash.finalize().0 != run.checksum {
            return Err(SpillError::ChecksumMismatch);
        }
        checkpoint()?;
        self.stats.restored_runs = self.stats.restored_runs.saturating_add(1);
        Ok(output)
    }

    // Advisory headroom selects a bounded request, never bypasses admission.
    // Racing allocations can still make allocate_inner refuse. Zero input
    // needs no payload buffer; nonempty input must never enter a zero-step loop.
    fn stream_buffer(&self, len: usize) -> Result<TrackedBytes, SpillError> {
        let bytes = len.min(IO_CHUNK_BYTES).min(self.pool.available());
        if len != 0 && bytes == 0 {
            return Err(MemoryError::ResourceExhausted {
                requested: 1,
                available: 0,
                limit: self.pool.effective_limit(),
            }
            .into());
        }
        self.pool.allocate_inner(bytes, 0).map_err(SpillError::Memory)
    }
}
