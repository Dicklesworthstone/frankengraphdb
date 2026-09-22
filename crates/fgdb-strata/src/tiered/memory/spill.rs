//! Bounded, non-durable query scratch over an exclusively owned async file.
//!
//! The composition root supplies an EMPTY, query-private seekable file (normally
//! opened through asupersync's Vfs) and owns namespace cleanup. This module never
//! opens a path, uses ambient entropy, fsyncs, or publishes graph authority. It
//! drops the file with the query; `into_file` transfers it back for explicit
//! region cleanup. Scratch is not recoverable database state.
//!
//! Reservations are append-only for the lifetime of the file. A partial or
//! cancelled append burns its reserved extent rather than reusing bytes a
//! cancelled backend operation may still touch. Both attempted run count and
//! file high-water mark are bounded. There is no resident run catalog and no
//! unbounded payload staging; only the caller's input and a fixed-size hasher
//! are retained while writing. Opaque run handles contain all read coordinates
//! and integrity evidence, never a path or a self-asserted on-disk length.

use core::fmt;
use std::io::{self, SeekFrom};
use std::sync::Arc;

use asupersync::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use fgdb_types::QueryCx;

use super::{MemoryError, MemoryPool, TrackedBytes};

const IO_CHUNK_BYTES: usize = 64 * 1024;
const CHECKSUM_DOMAIN: &[u8] = b"fgdb.strata.query-spill.v1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpillLimits {
    /// Cumulative extent reservations, including incomplete writes. Not RSS.
    pub max_file_bytes: u64,
    /// Cumulative append attempts, including incomplete writes and empty runs.
    pub max_runs: u64,
    /// Maximum bytes restored in one admitted resident allocation.
    pub max_run_bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SpillStats {
    pub reserved_bytes: u64,
    pub reserved_runs: u64,
    pub published_runs: u64,
    pub restored_runs: u64,
}

#[derive(Debug)]
pub enum SpillError {
    InvalidLimits,
    NonEmptyFile,
    SizeOverflow,
    FileLimit { requested: u64, available: u64 },
    RunLimit { limit: u64 },
    RunTooLarge { bytes: usize, limit: usize },
    ForeignRun,
    InvalidRun,
    ChecksumMismatch,
    Memory(MemoryError),
    Io(io::Error),
    Interrupted(Box<asupersync::error::Error>),
}

impl fmt::Display for SpillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileLimit { requested, available } => write!(f,
                "ResourceExhausted: spill needs {requested} file bytes, {available} available"),
            Self::RunLimit { limit } => write!(f, "ResourceExhausted: spill run limit {limit}"),
            Self::RunTooLarge { bytes, limit } => write!(f,
                "ResourceExhausted: spill run has {bytes} bytes, limit {limit}"),
            Self::Memory(error) => error.fmt(f),
            Self::Io(error) => write!(f, "query spill I/O: {error}"),
            Self::Interrupted(_) => f.write_str("query spill interrupted"),
            other => write!(f, "query spill: {other:?}"),
        }
    }
}

impl std::error::Error for SpillError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Memory(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MemoryError> for SpillError {
    fn from(error: MemoryError) -> Self { Self::Memory(error) }
}

impl From<io::Error> for SpillError {
    fn from(error: io::Error) -> Self { Self::Io(error) }
}

/// Process-local, immutable authority for exactly one completed scratch run.
/// Cloning shares only an identity token, not the file or resident payload.
/// There is deliberately no constructor or deserializer for these coordinates.
#[derive(Clone, Debug)]
pub struct SpillRun {
    owner: Arc<()>,
    id: u64,
    offset: u64,
    len: usize,
    checksum: [u8; 32],
}

impl SpillRun {
    /// Replay-stable ordinal within this query's scratch file.
    pub const fn id(&self) -> u64 { self.id }
    pub const fn offset(&self) -> u64 { self.offset }
    pub const fn len(&self) -> usize { self.len }
    pub const fn is_empty(&self) -> bool { self.len == 0 }
}

/// Owns its file exclusively. No clone or file-reference accessor permits a
/// competing seek or mutation while a run is being written/read. The backend
/// must provide normal ordered seek/read/write semantics on this handle.
pub struct SpillFile<F> {
    file: F,
    owner: Arc<()>,
    pool: MemoryPool,
    limits: SpillLimits,
    stats: SpillStats,
}

fn run_hasher(id: u64, offset: u64, len: u64) -> fgdb_crypto::Hasher {
    let mut hash = fgdb_crypto::Hasher::new();
    hash.update(CHECKSUM_DOMAIN);
    hash.update(&id.to_le_bytes());
    hash.update(&offset.to_le_bytes());
    hash.update(&len.to_le_bytes());
    hash
}

impl<F> SpillFile<F> {
    pub const fn stats(&self) -> SpillStats { self.stats }
    pub fn memory_pool(&self) -> &MemoryPool { &self.pool }

    /// Consume the scratch owner for region-owned close/truncate/unlink work.
    /// Previously issued run handles cannot authorize a different SpillFile.
    pub fn into_file(self) -> F { self.file }
}

impl<F: AsyncRead + AsyncWrite + AsyncSeek + Unpin> SpillFile<F> {
    /// Refuse nonempty input instead of truncating an existing object. Supplying
    /// an exclusive query-private handle and cleaning its name remain the
    /// composition root's responsibility; this is not a path-based file API.
    pub async fn new(
        cx: &QueryCx,
        file: F,
        pool: MemoryPool,
        limits: SpillLimits,
    ) -> Result<Self, SpillError> {
        cx.with_restriction_async(Self::new_inner(file, pool, limits,
            || cx.checkpoint().map_err(SpillError::Interrupted))).await
    }

    async fn new_inner(
        mut file: F,
        pool: MemoryPool,
        limits: SpillLimits,
        mut checkpoint: impl FnMut() -> Result<(), SpillError>,
    ) -> Result<Self, SpillError> {
        checkpoint()?;
        if limits.max_file_bytes == 0 || limits.max_runs == 0 || limits.max_run_bytes == 0 {
            return Err(SpillError::InvalidLimits);
        }
        if file.seek(SeekFrom::End(0)).await? != 0 {
            return Err(SpillError::NonEmptyFile);
        }
        checkpoint()?;
        Ok(Self { file, owner: Arc::new(()), pool, limits, stats: SpillStats::default() })
    }

    /// Persist a borrowed buffer without allocating a second payload. On error
    /// or cancellation the caller still owns the original input. A handle is
    /// published only after the entire write, flush, and final checkpoint.
    pub async fn append(&mut self, cx: &QueryCx, bytes: &[u8]) -> Result<SpillRun, SpillError> {
        cx.with_restriction_async(self.append_inner(bytes,
            || cx.checkpoint().map_err(SpillError::Interrupted))).await
    }

    async fn append_inner(
        &mut self,
        bytes: &[u8],
        mut checkpoint: impl FnMut() -> Result<(), SpillError>,
    ) -> Result<SpillRun, SpillError> {
        checkpoint()?;
        if bytes.len() > self.limits.max_run_bytes {
            return Err(SpillError::RunTooLarge { bytes: bytes.len(), limit: self.limits.max_run_bytes });
        }
        if self.stats.reserved_runs >= self.limits.max_runs {
            return Err(SpillError::RunLimit { limit: self.limits.max_runs });
        }
        let len = u64::try_from(bytes.len()).map_err(|_| SpillError::SizeOverflow)?;
        let available = self.limits.max_file_bytes - self.stats.reserved_bytes;
        if len > available {
            return Err(SpillError::FileLimit { requested: len, available });
        }
        let offset = self.stats.reserved_bytes;
        let id = self.stats.reserved_runs + 1;
        // Reserve BEFORE the first potentially pending I/O. Never refund or
        // reuse this range: cancellation may leave any prefix on the backend.
        self.stats.reserved_bytes += len;
        self.stats.reserved_runs = id;
        self.file.seek(SeekFrom::Start(offset)).await?;
        let mut hash = run_hasher(id, offset, len);
        for chunk in bytes.chunks(IO_CHUNK_BYTES) {
            checkpoint()?;
            self.file.write_all(chunk).await?;
            hash.update(chunk);
        }
        // Visibility to subsequent reads, NOT a database durability barrier.
        self.file.flush().await?;
        checkpoint()?;
        self.stats.published_runs += 1;
        Ok(SpillRun { owner: Arc::clone(&self.owner), id, offset, len: bytes.len(), checksum: hash.finalize().0 })
    }

    /// Admit resident capacity BEFORE any read/allocation can escape. A short
    /// read, cancellation, or checksum mismatch drops and refunds the entire
    /// restored allocation. Input run coordinates never come from disk bytes.
    pub async fn restore(&mut self, cx: &QueryCx, run: &SpillRun) -> Result<TrackedBytes, SpillError> {
        cx.with_restriction_async(self.restore_inner(run,
            || cx.checkpoint().map_err(SpillError::Interrupted))).await
    }

    async fn restore_inner(
        &mut self,
        run: &SpillRun,
        mut checkpoint: impl FnMut() -> Result<(), SpillError>,
    ) -> Result<TrackedBytes, SpillError> {
        checkpoint()?;
        if !Arc::ptr_eq(&self.owner, &run.owner) {
            return Err(SpillError::ForeignRun);
        }
        let len = u64::try_from(run.len).map_err(|_| SpillError::SizeOverflow)?;
        let end = run.offset.checked_add(len).ok_or(SpillError::InvalidRun)?;
        if run.id == 0 || run.id > self.stats.reserved_runs
            || run.len > self.limits.max_run_bytes || end > self.stats.reserved_bytes
        {
            return Err(SpillError::InvalidRun);
        }
        let mut bytes = self.pool.allocate_inner(run.len, 0)?;
        self.file.seek(SeekFrom::Start(run.offset)).await?;
        let mut hash = run_hasher(run.id, run.offset, len);
        for chunk in bytes.as_mut().chunks_mut(IO_CHUNK_BYTES) {
            checkpoint()?;
            self.file.read_exact(chunk).await?;
            hash.update(chunk);
        }
        if hash.finalize().0 != run.checksum {
            return Err(SpillError::ChecksumMismatch);
        }
        checkpoint()?;
        self.stats.restored_runs = self.stats.restored_runs.saturating_add(1);
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::ReadBuf;
    use std::future::Future;
    use std::io::{Cursor, Seek, Write};
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    #[derive(Default)]
    struct TestFile {
        data: Cursor<Vec<u8>>,
        write_limit: Option<usize>,
        pending_write: bool,
        fail_flush: bool,
        read_calls: usize,
    }

    impl AsyncSeek for TestFile {
        fn poll_seek(mut self: Pin<&mut Self>, _: &mut Context<'_>, pos: SeekFrom) -> Poll<io::Result<u64>> {
            Poll::Ready(self.data.seek(pos))
        }
    }

    impl AsyncWrite for TestFile {
        fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
            if self.pending_write { return Poll::Pending; }
            let count = self.write_limit.unwrap_or(bytes.len()).min(bytes.len());
            if count == 0 && !bytes.is_empty() {
                return Poll::Ready(Err(io::Error::other("injected partial write failure")));
            }
            let written = self.data.write(&bytes[..count]);
            if let (Some(remaining), Ok(count)) = (self.write_limit.as_mut(), &written) {
                *remaining -= *count;
            }
            Poll::Ready(written)
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(if self.fail_flush { Err(io::Error::other("injected flush failure")) } else { Ok(()) })
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
    }

    impl AsyncRead for TestFile {
        fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
            self.read_calls += 1;
            let position = usize::try_from(self.data.position()).unwrap();
            let available = self.data.get_ref().len().saturating_sub(position);
            let count = available.min(out.remaining());
            if count > 0 {
                out.put_slice(&self.data.get_ref()[position..position + count]);
                self.data.set_position((position + count) as u64);
            }
            Poll::Ready(Ok(()))
        }
    }

    fn complete<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the test transport unexpectedly suspended"),
        }
    }

    fn limits() -> SpillLimits {
        SpillLimits { max_file_bytes: 1024 * 1024, max_runs: 100, max_run_bytes: 256 * 1024 }
    }

    fn scratch(pool: MemoryPool) -> SpillFile<TestFile> {
        complete(SpillFile::new_inner(TestFile::default(), pool, limits(), || Ok(()))).unwrap()
    }

    #[test]
    fn spill_round_trip_releases_and_readmits_hierarchical_memory() {
        let root = MemoryPool::new(1024, 0).unwrap();
        let pool = root.child(512, 0).unwrap();
        let mut file = scratch(pool.clone());
        let mut input = pool.allocate_inner(400, 0).unwrap();
        input.as_mut().fill(42);
        let run = complete(file.append_inner(input.as_ref(), || Ok(()))).unwrap();
        assert!(complete(file.restore_inner(&run, || Ok(()))).is_err());
        assert_eq!(file.file.read_calls, 0);
        drop(input);
        assert_eq!(root.used(), 0);
        let restored = complete(file.restore_inner(&run, || Ok(()))).unwrap();
        assert_eq!(restored.as_ref(), &[42; 400]);
        assert_eq!(root.used(), restored.charged_bytes());
        drop(restored);
        assert_eq!((root.used(), pool.used()), (0, 0));
    }

    #[test]
    fn every_payload_byte_is_verified_and_corrupt_reads_refund() {
        let pool = MemoryPool::new(1024, 0).unwrap();
        let mut file = scratch(pool.clone());
        let run = complete(file.append_inner(b"a checked query batch", || Ok(()))).unwrap();
        for index in 0..run.len() {
            file.file.data.get_mut()[index] ^= 1;
            assert!(matches!(complete(file.restore_inner(&run, || Ok(()))), Err(SpillError::ChecksumMismatch)));
            assert_eq!(pool.used(), 0);
            file.file.data.get_mut()[index] ^= 1;
        }
        file.file.data.get_mut().truncate(run.len() - 1);
        assert!(matches!(complete(file.restore_inner(&run, || Ok(()))), Err(SpillError::Io(_))));
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn foreign_tokens_and_oversized_runs_do_no_io() {
        let pool = MemoryPool::new(1024, 0).unwrap();
        let mut a = scratch(pool.clone());
        let mut b = scratch(pool.clone());
        let run = complete(a.append_inner(b"abc", || Ok(()))).unwrap();
        assert!(matches!(complete(b.restore_inner(&run, || Ok(()))), Err(SpillError::ForeignRun)));
        assert_eq!(b.file.read_calls, 0);
        b.limits.max_run_bytes = 2;
        assert!(matches!(complete(b.append_inner(b"abc", || Ok(()))), Err(SpillError::RunTooLarge { .. })));
        assert_eq!(b.stats(), SpillStats::default());
        assert!(b.file.data.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
    }

    #[test]
    fn failed_writes_burn_their_extent_without_publishing_a_run() {
        let mut file = scratch(MemoryPool::new(1024, 0).unwrap());
        file.file.write_limit = Some(2);
        assert!(matches!(complete(file.append_inner(b"abcde", || Ok(()))), Err(SpillError::Io(_))));
        assert_eq!(file.stats().published_runs, 0);
        assert_eq!(file.stats().reserved_bytes, 5);
        file.file.write_limit = None;
        let run = complete(file.append_inner(b"next", || Ok(()))).unwrap();
        assert_eq!((run.id(), run.offset()), (2, 5));
        assert_eq!(complete(file.restore_inner(&run, || Ok(()))).unwrap().as_ref(), b"next");
    }

    #[test]
    fn dropping_a_pending_write_cannot_reuse_its_reserved_coordinates() {
        let mut file = scratch(MemoryPool::new(1024, 0).unwrap());
        file.file.pending_write = true;
        {
            let future = file.append_inner(b"pending", || Ok(()));
            let mut future = std::pin::pin!(future);
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        }
        assert_eq!((file.stats().reserved_bytes, file.stats().published_runs), (7, 0));
        file.file.pending_write = false;
        let run = complete(file.append_inner(b"ok", || Ok(()))).unwrap();
        assert_eq!((run.id(), run.offset()), (2, 7));
    }

    #[test]
    fn flush_failure_never_publishes_read_authority() {
        let mut file = scratch(MemoryPool::new(1024, 0).unwrap());
        file.file.fail_flush = true;
        assert!(complete(file.append_inner(b"written not flushed", || Ok(()))).is_err());
        assert_eq!(file.stats().published_runs, 0);
        assert_eq!(file.stats().reserved_runs, 1);
    }

    #[test]
    fn quotas_include_failed_and_empty_attempts() {
        let mut file = scratch(MemoryPool::new(1024, 0).unwrap());
        file.limits.max_file_bytes = 3;
        file.limits.max_runs = 2;
        complete(file.append_inner(b"abc", || Ok(()))).unwrap();
        assert!(matches!(complete(file.append_inner(b"d", || Ok(()))), Err(SpillError::FileLimit { .. })));
        let empty = complete(file.append_inner(b"", || Ok(()))).unwrap();
        assert!(empty.is_empty());
        assert!(complete(file.restore_inner(&empty, || Ok(()))).unwrap().is_empty());
        assert!(matches!(complete(file.append_inner(b"", || Ok(()))), Err(SpillError::RunLimit { .. })));
        assert_eq!(file.file.data.get_ref().len(), 3);
    }

    #[test]
    fn chunk_boundaries_and_repeated_reads_preserve_exact_bytes() {
        let bytes: Vec<_> = (0..IO_CHUNK_BYTES * 2 + 7).map(|i| (i % 251) as u8).collect();
        let pool = MemoryPool::new(bytes.len(), 0).unwrap();
        let mut file = scratch(pool.clone());
        let mut checkpoints = 0;
        let run = complete(file.append_inner(&bytes, || { checkpoints += 1; Ok(()) })).unwrap();
        assert_eq!(checkpoints, 5);
        for _ in 0..3 {
            let restored = complete(file.restore_inner(&run, || Ok(()))).unwrap();
            assert_eq!(restored.as_ref(), bytes.as_slice());
            drop(restored);
            assert_eq!(pool.used(), 0);
        }
        assert_eq!(file.stats().restored_runs, 3);
    }

    #[test]
    fn checkpoint_failures_do_not_publish_or_leak_resident_memory() {
        let pool = MemoryPool::new(1024, 0).unwrap();
        let mut file = scratch(pool.clone());
        let mut calls = 0;
        assert!(complete(file.append_inner(b"abc", || {
            calls += 1;
            if calls == 3 { Err(SpillError::Io(io::Error::other("stop"))) } else { Ok(()) }
        })).is_err());
        assert_eq!(file.stats().published_runs, 0);
        let run = complete(file.append_inner(b"checked", || Ok(()))).unwrap();
        let mut calls = 0;
        assert!(complete(file.restore_inner(&run, || {
            calls += 1;
            if calls == 3 { Err(SpillError::Io(io::Error::other("stop"))) } else { Ok(()) }
        })).is_err());
        assert_eq!(pool.used(), 0);
        assert_eq!(file.stats().restored_runs, 0);
    }

    #[test]
    fn construction_never_truncates_a_nonempty_file() {
        let file = TestFile { data: Cursor::new(b"not scratch".to_vec()), ..Default::default() };
        assert!(matches!(complete(SpillFile::new_inner(file, MemoryPool::new(100, 0).unwrap(), limits(), || Ok(()))),
            Err(SpillError::NonEmptyFile)));
    }
}
