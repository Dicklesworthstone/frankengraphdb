//! Host-owned scratch admission for external ordered reads. No path, file
//! handle or physical limit is taken from a request or returned to a client.

use crate::ServerError;
use crate::execute::Refusal;
use asupersync::Cx;
use asupersync::fs::File;
use asupersync::runtime::spawn_blocking_io;
use fgdb::AuthorizedSortLimits;
use fgdb_protocol::body::ErrorCode;
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::QueryCx;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Operator-owned physical policy for FGP `ReadOrdered` / `OrderedStream`.
/// The directory must already exist. Each execution gets two distinct,
/// exclusive files unlinked before any query bytes are written; descriptor
/// ownership retires them on completion, error, cancellation or disconnect.
/// This is ephemeral scratch, never a result store or recovery directory.
#[derive(Clone, Debug)]
pub struct OrderedReadConfig {
    pub directory: PathBuf,
    /// One shared tracked spill-buffer ceiling across this database's orders.
    /// Source graph residency, wire values and foundation I/O buffers are not
    /// represented by this accounting counter.
    pub memory_bytes: usize,
    /// Concurrent admitted ordered sessions, including those awaiting credit.
    pub max_concurrent: usize,
    /// Append-only quotas applied separately to each of the two private files.
    pub spill: SpillLimits,
    pub sort: AuthorizedSortLimits,
}

impl OrderedReadConfig {
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            memory_bytes: 64 * 1024 * 1024,
            max_concurrent: 4,
            spill: SpillLimits {
                max_file_bytes: 512 * 1024 * 1024,
                max_runs: 8192,
                max_run_bytes: 128 * 1024 * 1024,
            },
            sort: AuthorizedSortLimits {
                run_rows: 256,
                max_runs: 1024,
                page_bytes: 32 * 1024,
                max_row_bytes: 1024 * 1024,
                max_input_rows: 100_000,
                // The CLI's default grant has 100m work units. This entire
                // 10m allowance is prepaid; source and final-read work add to it.
                max_sort_work: 10_000_000,
            },
        }
    }

    fn validate(&self) -> Result<(), ServerError> {
        if self.directory.as_os_str().is_empty()
            || self.memory_bytes == 0
            || self.max_concurrent == 0
            || self.spill.max_file_bytes == 0
            || self.spill.max_runs == 0
            || self.spill.max_run_bytes == 0
            || self.sort.run_rows == 0
            || self.sort.max_runs == 0
            || self.sort.page_bytes == 0
            || self.sort.page_bytes > 64 * 1024
            || self.sort.max_row_bytes == 0
            || self.sort.max_sort_work == 0
        {
            return Err(ServerError::InvalidLimits);
        }
        Ok(())
    }
}

pub(crate) struct OrderedReads {
    config: OrderedReadConfig,
    pool: MemoryPool,
    active: Arc<AtomicUsize>,
}

/// Retain admission while a session owns its scratch, even if output waits
/// indefinitely for credit. This does not count a cancelled foundation syscall
/// that is finishing against an already unlinked, never-reused descriptor.
pub(crate) struct OrderLease(Arc<AtomicUsize>);
impl Drop for OrderLease {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl OrderedReads {
    pub(crate) async fn open(cx: &Cx, config: OrderedReadConfig) -> Result<Self, ServerError> {
        config.validate()?;
        cx.checkpoint().map_err(|_| ServerError::Spawn)?;
        let directory = config.directory.clone();
        spawn_blocking_io(move || {
            if std::fs::metadata(directory)?.is_dir() {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    "scratch is not a directory",
                ))
            }
        })
        .await
        .map_err(ServerError::OrderedScratch)?;
        let pool =
            MemoryPool::new(config.memory_bytes, 0).map_err(|_| ServerError::InvalidLimits)?;
        Ok(Self {
            config,
            pool,
            active: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub(crate) fn limits(&self) -> AuthorizedSortLimits {
        self.config.sort
    }

    /// The caller authenticates and generation-guards this future before its
    /// first poll and every continuation. File creation and unlink occur in
    /// one blocking closure: cancellation cannot strand a named partial file.
    pub(crate) async fn files(
        &self,
        cx: &Cx,
        query: &QueryCx,
    ) -> Result<(SpillFile<File>, SpillFile<File>, OrderLease), Refusal> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.config.max_concurrent).then(|| count + 1)
            })
            .map_err(|_| {
                Refusal::new(ErrorCode::Budget, "ordered read concurrency limit reached")
            })?;
        let lease = OrderLease(Arc::clone(&self.active));
        let mut nonce = [0; 16];
        cx.random_bytes(&mut nonce);
        let directory = self.config.directory.clone();
        let (a, b, lease) = spawn_blocking_io(move || {
            let a = anonymous_file(&directory, nonce, 'a')?;
            let b = anonymous_file(&directory, nonce, 'b')?;
            Ok((a, b, lease))
        })
        .await
        .map_err(|_| {
            Refusal::new(
                ErrorCode::Execution,
                "ordered scratch file admission failed",
            )
        })?;
        let a = SpillFile::new(
            query,
            File::from_std(a),
            self.pool.clone(),
            self.config.spill,
        )
        .await
        .map_err(super::execute::ordered::spool_refusal)?;
        let b = SpillFile::new(
            query,
            File::from_std(b),
            self.pool.clone(),
            self.config.spill,
        )
        .await
        .map_err(super::execute::ordered::spool_refusal)?;
        Ok((a, b, lease))
    }
}

// A failed or abandoned open has no published pathname containing query data.
// create_new refuses links/collisions; unlink happens before the file is handed
// to an async owner and before the first spill write. The host directory is
// fixed at server setup, and the sole name component comes from host entropy.
#[cfg(unix)]
fn anonymous_file(directory: &Path, nonce: [u8; 16], suffix: char) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::from("fgdb-order-");
    for byte in nonce {
        name.push(char::from(HEX[usize::from(byte >> 4)]));
        name.push(char::from(HEX[usize::from(byte & 15)]));
    }
    let path = directory.join(format!("{name}-{suffix}"));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    std::fs::remove_file(path)?;
    Ok(file)
}

#[cfg(not(unix))]
fn anonymous_file(_: &Path, _: [u8; 16], _: char) -> io::Result<std::fs::File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "ordered scratch requires Unix unlink semantics",
    ))
}

#[cfg(all(test, unix))]
#[path = "ordered/tests.rs"]
mod tests;
