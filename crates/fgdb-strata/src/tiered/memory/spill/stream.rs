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
        let extent_end = run
            .offset
            .checked_add(run_len)
            .ok_or(SpillError::InvalidRun)?;
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
        self.pool
            .allocate_inner(bytes, 0)
            .map_err(SpillError::Memory)
    }
}

impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> SpillFile<F> {
    /// Spill exactly len bytes from an async producer without collecting them.
    ///
    /// Unlike whole-buffer append, this explicitly admits runs larger than the
    /// pool's effective resident ceiling. max_run_bytes, max_file_bytes and
    /// max_runs still apply to the complete logical extent before source demand.
    /// One reusable pool-charged buffer (at most 64 KiB, at least one byte for
    /// nonempty input) covers both producer reads and scratch writes. The
    /// producer's own memory, side effects and cancellation law remain its
    /// responsibility; this method does not provide a whole-query RSS bound.
    ///
    /// The producer is consumed only through the declared prefix, with no EOF
    /// probe into a subsequent record. A shorter source is an I/O error. The
    /// reserved extent and attempt are never reused after source/write/flush
    /// failure or cancellation. Pending source reads also conservatively poison
    /// the scratch owner. The producer is NOT rewound on failure.
    ///
    /// Publication requires complete transfer, the ordinary flush and final
    /// checkpoint. The run uses the SAME checksum transcript as append, not a
    /// new scratch format. Read large runs through restore_window; restore
    /// continues to enforce its full resident allocation, without a fallback.
    pub async fn append_from<R: AsyncRead + Unpin>(
        &mut self,
        cx: &QueryCx,
        source: &mut R,
        len: usize,
    ) -> Result<SpillRun, SpillError> {
        cx.with_restriction_async(self.append_from_inner(source, len, || {
            cx.checkpoint().map_err(SpillError::Interrupted)
        }))
        .await
    }

    pub(super) async fn append_from_inner<R: AsyncRead + Unpin>(
        &mut self,
        source: &mut R,
        len: usize,
        mut checkpoint: impl FnMut() -> Result<(), SpillError>,
    ) -> Result<SpillRun, SpillError> {
        checkpoint()?;
        if self.io_pending {
            return Err(SpillError::PoisonedFile);
        }
        if len > self.limits.max_run_bytes {
            return Err(SpillError::RunTooLarge {
                bytes: len,
                limit: self.limits.max_run_bytes,
            });
        }
        if self.stats.reserved_runs >= self.limits.max_runs {
            return Err(SpillError::RunLimit {
                limit: self.limits.max_runs,
            });
        }
        let extent = u64::try_from(len).map_err(|_| SpillError::SizeOverflow)?;
        let available = self.limits.max_file_bytes - self.stats.reserved_bytes;
        if extent > available {
            return Err(SpillError::FileLimit {
                requested: extent,
                available,
            });
        }
        // Admission failure cannot consume the producer or burn file capacity.
        // All allocations precede the first possibly-pending source/file I/O.
        let mut buffer = self.stream_buffer(len)?;
        let offset = self.stats.reserved_bytes;
        let id = self.stats.reserved_runs + 1;
        self.stats.reserved_bytes += extent;
        self.stats.reserved_runs = id;
        self.io_pending = true;
        let actual = self.file.seek(SeekFrom::Start(offset)).await?;
        if actual != offset {
            return Err(SpillError::UnexpectedPosition {
                expected: offset,
                actual,
            });
        }
        let mut hash = run_hasher(id, offset, extent);
        let mut remaining = len;
        while remaining != 0 {
            checkpoint()?;
            let count = remaining.min(buffer.len());
            let chunk = &mut buffer.as_mut()[..count];
            source.read_exact(chunk).await?;
            checkpoint()?;
            self.file.write_all(chunk).await?;
            hash.update(chunk);
            remaining -= count;
        }
        checkpoint()?;
        self.file.flush().await?;
        self.io_pending = false;
        checkpoint()?;
        self.stats.published_runs += 1;
        Ok(SpillRun {
            owner: Arc::clone(&self.owner),
            id,
            offset,
            len,
            checksum: hash.finalize().0,
        })
    }
}
