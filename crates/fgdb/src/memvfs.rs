//! The in-memory filesystem behind the embedded spine's `:memory:` surface.
//!
//! asupersync owns the [`Vfs`]/[`VfsFile`] contract and ships exactly one
//! implementation, [`asupersync::fs::UnixVfs`]. This module adds the second
//! one, scoped to this crate: a filesystem whose file CONTENT lives only in
//! RAM, so [`Database`](crate::Database) can honor the README's
//! `Database::open(":memory:")` promise — a database that survives every
//! reopen-within-the-process, answers every read from the real two-fsync
//! commit path, and leaves nothing behind when the last handle drops.
//!
//! # Design: RAM content over a sparse real shadow namespace
//!
//! Two facts about the spine and the VFS contract shape this implementation:
//!
//! 1. Chronicle's writer lease (`CommitCoordinator::acquire_writer_lease`)
//!    and Strata's publication permit (`BlockStore::acquire_publication_permit`)
//!    deliberately stay on `std::fs` — process-liveness authority, and
//!    [`Vfs`] has no lock surface. Both address the database directory by
//!    its path through the REAL filesystem, so a memory database needs a
//!    real directory at the same absolute path.
//! 2. asupersync's `Metadata` and `ReadDir` wrap `std::fs` types behind
//!    `pub(crate)` constructors, so a foreign [`Vfs`] can only OBSERVE a
//!    real directory tree — it cannot fabricate those values. And the spine
//!    READS `VfsFile::metadata().len()` on product paths (torn-tail
//!    recovery, capsule comparison, root-slot framing, bounded block
//!    reads), so the observed length must equal the in-memory length
//!    exactly.
//!
//! [`MemVfs`] therefore keeps every file's bytes in RAM and maintains, at
//! the same absolute paths under a private `std::env::temp_dir` root, a
//! SHADOW namespace: real directories, and real sparse files whose logical
//! length is kept equal to the in-memory length (ftruncate on every length
//! change) but whose data blocks are never written — holes only. The two
//! locks and the observation types bind to the shadow; no graph byte is
//! ever written to it. The shadow root is removed when the last handle to
//! the [`MemVfs`] drops.
//!
//! Stated plainly, the honest cost of the two facts above: a memory
//! database still needs a writable temp directory for its content-free
//! shadow namespace. What it never does is write durable bytes to disk —
//! crash the process and the shadow holds zero-filled holes, which is
//! exactly the "lost on drop" contract.
//!
//! # Open semantics come from the real filesystem, by construction
//!
//! asupersync's `OpenOptions` exposes no getters — its flags are private
//! and its builder methods consume `self` — so a foreign [`Vfs::open`]
//! cannot branch on them. [`MemVfs::open`] therefore hands the caller's
//! options to the REAL filesystem, applied against the shadow path
//! (`create_new` conflicts, missing parents, `EISDIR` on write-opened
//! directories, `truncate` — exact `std::fs` behavior for free), and then
//! reconciles memory: a truncate-open is detected as a shadow length that
//! collapsed to zero against a non-empty buffer, a fresh create arrives
//! empty, and a plain open finds shadow and buffer already agreeing. One
//! consequence is documented rather than hidden: read/write mode
//! enforcement cannot be re-implemented in memory (std exposes no mode
//! query on an open file), so the in-memory handle serves reads and
//! accepts writes regardless of mode. The spine never relies on mode
//! refusals — its write handles only write and its read handles only read.
//!
//! # Durability semantics are the contract, not a skipped duty
//!
//! [`VfsFile::sync_all`] and [`VfsFile::sync_data`] return `Ok(())`
//! unconditionally. For a memory filesystem that is the CORRECT answer,
//! not a lie: every reader — including the two-fsync commit protocol's own
//! read-backs and the torn-tail recovery at reopen — observes the same
//! RAM, so there is no non-volatile medium those barriers could lag. The
//! protocol still runs in full (capsule write, D1, marker, D2, directory
//! barriers); its effects are trivially satisfied instead of patiently
//! awaited. What a process death loses is lost BY CONTRACT: a memory
//! database lives exactly as long as its last [`MemVfs`] handle.
//!
//! # What is deliberately NOT here
//!
//! Symlinks do not exist (`symlink_metadata` is the same observation as
//! `metadata`; `read_link` and `hard_link` fail closed with
//! [`io::ErrorKind::Unsupported`], the convention asupersync's own
//! `path_ops` uses for structurally absent operations). `read_dir` entry
//! NAMES are exactly the namespace's (the shadow invariant), but iteration
//! ORDER follows the host directory, as [`asupersync::fs::UnixVfs`]'s does;
//! every spine consumer of `read_dir` is order-insensitive (the create-time
//! emptiness probe and Chronicle's orphan scan feeding a reference set).

use std::collections::BTreeMap;
use std::io::{self, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use asupersync::fs::{Metadata, OpenOptions, Permissions, ReadDir, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncWrite, AsyncWriteExt, ReadBuf};
use asupersync::runtime::spawn_blocking_io;

/// The directory name a memory database occupies inside its private
/// [`MemVfs`] namespace. Deliberately shaped like the on-disk convention so
/// an operator inspecting the shadow root sees a familiar layout.
const DATABASE_DIR_NAME: &str = "database.fgdbdir";

/// Why an internal lock could not be entered: a previous holder panicked
/// mid-mutation. Surfaced as an I/O error instead of unwrapped, so a
/// poisoned namespace fails every caller closed rather than panicking here.
fn poisoned_lock() -> io::Error {
    io::Error::other("MemVfs internal lock poisoned")
}

/// One file's authoritative content: the bytes every read serves from, plus
/// a write handle to the sparse shadow inode used to keep the shadow's
/// logical length equal to the buffer's (one ftruncate per length change,
/// never a data write).
struct FileShared {
    bytes: Mutex<Vec<u8>>,
    shadow: Mutex<std::fs::File>,
}

impl FileShared {
    fn lock_bytes(&self) -> io::Result<MutexGuard<'_, Vec<u8>>> {
        self.bytes.lock().map_err(|_| poisoned_lock())
    }

    fn lock_shadow(&self) -> io::Result<MutexGuard<'_, std::fs::File>> {
        self.shadow.lock().map_err(|_| poisoned_lock())
    }
}

/// One namespace entry. Directories carry no payload: their existence is
/// the whole fact, exactly as parent-directory tracking requires.
#[derive(Clone)]
enum MemNode {
    Dir,
    File(Arc<FileShared>),
}

/// The shared namespace behind every [`MemVfs`] clone.
struct MemVfsInner {
    /// The real shadow root: a private, freshly created directory under
    /// `std::env::temp_dir`, removed when this value drops.
    root: PathBuf,
    /// The namespace, keyed by canonical absolute path. A `BTreeMap` so
    /// prefix walks (directory listings, subtree removal, rename re-keying)
    /// are ordered ranges rather than full scans.
    nodes: Mutex<BTreeMap<PathBuf, MemNode>>,
}

impl Drop for MemVfsInner {
    fn drop(&mut self) {
        // Best-effort: the shadow carries no durable bytes, so a failure to
        // remove it (or a process death skipping this drop) leaks only an
        // empty-but-for-holes directory tree in temp, never data.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// An in-memory [`Vfs`]: RAM file content over a sparse real shadow
/// namespace (module docs explain why the shadow exists and why it carries
/// no durable bytes).
///
/// Cloning shares one namespace — that is the retention story for reopen:
/// hold a clone across a [`Database`](crate::Database) drop, and
/// [`Database::open_with_vfs`](crate::Database::open_with_vfs) on the clone
/// reopens the same memory database. Dropping the last clone drops the
/// namespace and removes the shadow root.
#[derive(Clone)]
pub struct MemVfs {
    inner: Arc<MemVfsInner>,
}

impl MemVfs {
    /// Creates a fresh, empty memory filesystem with its own private shadow
    /// root. Two `MemVfs` values never see each other's entries — that
    /// isolation is what makes each `:memory:`-style database private.
    ///
    /// # Errors
    ///
    /// Fails when a fresh temp directory cannot be claimed (unwritable or
    /// exhausted temp), or when the root cannot be mirrored into the
    /// namespace table.
    ///
    pub fn new() -> io::Result<Self> {
        let root = fresh_root()?;
        let mut nodes = BTreeMap::new();
        nodes.insert(root.clone(), MemNode::Dir);
        Ok(Self {
            inner: Arc::new(MemVfsInner {
                root,
                nodes: Mutex::new(nodes),
            }),
        })
    }

    /// The path, inside this namespace and on the real filesystem alike,
    /// where a memory database created over this [`MemVfs`] lives. Pass it
    /// to [`Database::create_with_vfs`](crate::Database::create_with_vfs) /
    /// [`Database::open_with_vfs`](crate::Database::open_with_vfs); retain a
    /// clone of `self` to reopen after the handle drops.
    #[must_use]
    pub fn database_dir(&self) -> PathBuf {
        self.inner.root.join(DATABASE_DIR_NAME)
    }

    /// Resolves `path` to a canonical absolute entry of this namespace:
    /// relative paths anchor at the root, `.` components vanish, and `..`
    /// may climb within the namespace but never out of it. Purely lexical —
    /// there are no symlinks to follow (module docs).
    fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
        let root = &self.inner.root;
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if !absolute.starts_with(root) {
            return Err(escape_error());
        }
        let relative = match absolute.strip_prefix(root) {
            Ok(relative) => relative,
            Err(_) => return Err(escape_error()),
        };
        let mut normalized = root.clone();
        for component in relative.components() {
            match component {
                Component::Normal(segment) => normalized.push(segment),
                Component::CurDir => {}
                Component::ParentDir => {
                    // Popping past the root would leave the namespace.
                    if !normalized.pop() || !normalized.starts_with(root) {
                        return Err(escape_error());
                    }
                }
                // Prefix and RootDir cannot occur below the absolute root.
                _ => return Err(escape_error()),
            }
        }
        Ok(normalized)
    }

    fn lock_nodes(&self) -> io::Result<MutexGuard<'_, BTreeMap<PathBuf, MemNode>>> {
        self.inner.nodes.lock().map_err(|_| poisoned_lock())
    }

    /// Blocks briefly to apply one length change to the sparse shadow
    /// inode. Called from synchronous poll methods, where a bounded
    /// metadata syscall is the honest price of the shadow invariant; every
    /// async entry point that touches the real filesystem routes through
    /// `spawn_blocking_io` instead.
    fn sync_shadow_len(shared: &FileShared, len: u64) -> io::Result<()> {
        let shadow = shared.lock_shadow()?;
        shadow.set_len(len)
    }
}

fn escape_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "path escapes the memory filesystem namespace",
    )
}

fn divergence_error(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "MemVfs shadow namespace diverged from memory at {}",
            path.display()
        ),
    )
}

/// Claims a fresh private directory under the system temp dir. The
/// pid + counter + nanosecond name is a claim, not an authority: the
/// atomic act is the `create_dir` itself, and a lost race retries.
fn fresh_root() -> io::Result<PathBuf> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let base = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    for _ in 0..64 {
        let candidate = base.join(format!(
            "fgdb-mem-{}-{}-{nanos}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not claim a fresh MemVfs shadow root under the temp directory",
    ))
}

impl core::fmt::Debug for MemVfs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Structural only: entry COUNT is diagnostic shape; names and bytes
        // are graph data and stay redacted, matching Database's Debug law.
        let count = self
            .inner
            .nodes
            .lock()
            .map(|nodes| nodes.len())
            .unwrap_or_default();
        f.debug_struct("MemVfs")
            .field("root", &self.inner.root)
            .field("entries", &count)
            .finish()
    }
}

/// One end of the namespace: a directory handle (openable read-only, like a
/// dirfd — the spine uses one for directory-barrier syncs) or a file handle
/// over shared RAM content.
pub struct MemVfsFile {
    handle: Handle,
    /// The read/write cursor. Owned by the handle, not the shared bytes:
    /// two handles over one file seek independently, exactly as two
    /// descriptors over one inode do.
    cursor: u64,
}

enum Handle {
    Dir {
        path: PathBuf,
    },
    File {
        shared: Arc<FileShared>,
        path: PathBuf,
    },
}

impl core::fmt::Debug for MemVfsFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The path is storage layout, safe to show; bytes stay redacted.
        match &self.handle {
            Handle::Dir { path } => f
                .debug_struct("MemVfsFile")
                .field("kind", &"directory")
                .field("path", path)
                .finish(),
            Handle::File { path, .. } => f
                .debug_struct("MemVfsFile")
                .field("kind", &"file")
                .field("path", path)
                .field("cursor", &self.cursor)
                .finish(),
        }
    }
}

impl AsyncRead for MemVfsFile {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Handle::File { shared, .. } = &this.handle else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "is a directory",
            )));
        };
        let bytes = match shared.lock_bytes() {
            Ok(bytes) => bytes,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let len = bytes.len() as u64;
        if this.cursor >= len {
            // Zero bytes advanced: the EOF signal every ext reader needs.
            return Poll::Ready(Ok(()));
        }
        let available = (len - this.cursor) as usize;
        let take = available.min(buf.remaining());
        let start = this.cursor as usize;
        buf.put_slice(&bytes[start..start + take]);
        this.cursor += take as u64;
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for MemVfsFile {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Handle::File { shared, .. } = &this.handle else {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "is a directory",
            )));
        };
        // The write lands at the cursor and OVERWRITES in place; only a
        // write past EOF extends the file (zero-filling the hole), never a
        // mid-file write — POSIX writes do not truncate. The shadow length
        // is therefore only ever GROWN here, to the post-write length; an
        // end offset inside the file leaves the length alone.
        let start = this.cursor;
        let end = match start.checked_add(buf.len() as u64) {
            Some(end) => end,
            None => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write end overflows",
                )));
            }
        };
        let (start_usize, end_usize) = match (usize::try_from(start), usize::try_from(end)) {
            (Ok(start), Ok(end)) => (start, end),
            _ => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write position exceeds the addressable range",
                )));
            }
        };
        let current_len = match shared.lock_bytes() {
            Ok(bytes) => bytes.len() as u64,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let new_len = current_len.max(end);
        if new_len > current_len {
            // Shadow first: if the length cannot land on the sparse inode,
            // the write fails BEFORE memory mutates, so no divergence is
            // possible.
            if let Err(error) = MemVfs::sync_shadow_len(shared, new_len) {
                return Poll::Ready(Err(error));
            }
        }
        {
            let mut bytes = match shared.lock_bytes() {
                Ok(bytes) => bytes,
                Err(error) => return Poll::Ready(Err(error)),
            };
            if bytes.len() < start_usize {
                bytes.resize(start_usize, 0);
            }
            if bytes.len() < end_usize {
                bytes.resize(end_usize, 0);
            }
            bytes[start_usize..end_usize].copy_from_slice(buf);
        }
        this.cursor = end;
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Nothing is buffered between the write and its readers: RAM writes
        // are immediately visible to every handle, so flush has no duty.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Same duty-free flush, plus no close-time state to push anywhere.
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for MemVfsFile {
    fn poll_seek(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        let this = self.get_mut();
        let len = match &this.handle {
            Handle::File { shared, .. } => {
                let bytes = match shared.lock_bytes() {
                    Ok(bytes) => bytes,
                    Err(error) => return Poll::Ready(Err(error)),
                };
                bytes.len() as u64
            }
            Handle::Dir { .. } => 0,
        };
        let target = match pos {
            SeekFrom::Start(offset) => Some(offset as i128),
            SeekFrom::Current(delta) => Some(this.cursor as i128 + delta as i128),
            SeekFrom::End(delta) => Some(len as i128 + delta as i128),
        };
        match target {
            Some(target) if target >= 0 && target <= u64::MAX as i128 => {
                this.cursor = target as u64;
                Poll::Ready(Ok(this.cursor))
            }
            _ => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            ))),
        }
    }
}

impl VfsFile for MemVfsFile {
    async fn metadata(&self) -> io::Result<Metadata> {
        let path = match &self.handle {
            Handle::Dir { path } | Handle::File { path, .. } => path,
        };
        // The shadow's length is kept equal to the buffer at every length
        // change, so the real observation IS the memory observation.
        asupersync::fs::metadata(path).await
    }

    async fn sync_all(&self) -> io::Result<()> {
        // Correct for memory, not a skipped duty (module docs): every
        // reader observes the same RAM, so no barrier can lag anything.
        Ok(())
    }

    async fn sync_data(&self) -> io::Result<()> {
        // Same contract as sync_all: there is no non-volatile medium here
        // for data-but-not-metadata to fall behind on.
        Ok(())
    }

    async fn set_len(&self, size: u64) -> io::Result<()> {
        let Handle::File { shared, .. } = &self.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot set the length of a directory",
            ));
        };
        let size_usize = usize::try_from(size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "length out of range"))?;
        // Shadow first, memory second — the same no-divergence ordering as
        // poll_write: a failed ftruncate must not leave a longer buffer
        // behind a shorter shadow.
        MemVfs::sync_shadow_len(shared, size)?;
        let mut bytes = shared.lock_bytes()?;
        bytes.resize(size_usize, 0);
        Ok(())
    }

    async fn set_permissions(&self, perm: Permissions) -> io::Result<()> {
        let path = match &self.handle {
            Handle::Dir { path } | Handle::File { path, .. } => path,
        };
        asupersync::fs::set_permissions(path, perm).await
    }
}

impl Vfs for MemVfs {
    type File = MemVfsFile;

    async fn open(&self, path: &Path, opts: &OpenOptions) -> io::Result<Self::File> {
        let target = self.resolve(path)?;
        let existing = self.lock_nodes()?.get(&target).cloned();

        // The caller's flags are invisible outside asupersync (module
        // docs), so the REAL filesystem applies them against the shadow:
        // create/create_new conflicts, missing parents, EISDIR on
        // write-opened directories, and truncate all get exact std
        // behavior by construction.
        let opened = opts.open(&target).await?;

        match existing {
            Some(MemNode::Dir) => {
                // A successful read-only open of a directory yields a
                // dirfd with no duty here; the handle carries the path.
                drop(opened);
                Ok(MemVfsFile {
                    handle: Handle::Dir { path: target },
                    cursor: 0,
                })
            }
            Some(MemNode::File(shared)) => {
                let std_file = opened.into_std()?;
                let shadow_len = std_file.metadata()?.len();
                {
                    let mut bytes = shared.lock_bytes()?;
                    let byte_len = bytes.len() as u64;
                    if shadow_len == 0 && byte_len != 0 {
                        // A truncate-open went through the shadow; memory
                        // follows the namespace, exactly as a real truncate
                        // would have cut this descriptor's content.
                        bytes.clear();
                    } else if shadow_len != byte_len {
                        // The 1:1 shadow invariant has no other legal
                        // shape; fail closed rather than guess.
                        return Err(divergence_error(&target));
                    }
                }
                // The caller's descriptor is dropped, never stored: it may
                // be read-only, and the stored shadow fd must stay writable
                // for the length syncs (the stored fd came from this file's
                // creating open, which required write access, and follows
                // the inode across renames and truncate-opens).
                drop(std_file);
                Ok(MemVfsFile {
                    handle: Handle::File {
                        shared,
                        path: target,
                    },
                    cursor: 0,
                })
            }
            None => {
                let std_file = opened.into_std()?;
                let shared = Arc::new(FileShared {
                    bytes: Mutex::new(Vec::new()),
                    shadow: Mutex::new(std_file),
                });
                self.lock_nodes()?
                    .insert(target.clone(), MemNode::File(shared.clone()));
                Ok(MemVfsFile {
                    handle: Handle::File {
                        shared,
                        path: target,
                    },
                    cursor: 0,
                })
            }
        }
    }

    async fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        let target = self.resolve(path)?;
        asupersync::fs::metadata(target).await
    }

    async fn symlink_metadata(&self, path: &Path) -> io::Result<Metadata> {
        // No symlinks exist in this namespace, so the no-follow observation
        // is the same fact as the following one (module docs).
        let target = self.resolve(path)?;
        asupersync::fs::symlink_metadata(target).await
    }

    async fn set_permissions(&self, path: &Path, perm: Permissions) -> io::Result<()> {
        let target = self.resolve(path)?;
        asupersync::fs::set_permissions(target, perm).await
    }

    async fn create_dir(&self, path: &Path) -> io::Result<()> {
        let target = self.resolve(path)?;
        // Real-first: AlreadyExists, missing-parent, and parent-is-a-file
        // errors arrive with exact std kinds, and only a successful real
        // creation is mirrored into the namespace.
        let target_for_io = target.clone();
        spawn_blocking_io(move || std::fs::create_dir(target_for_io)).await?;
        self.lock_nodes()?.insert(target, MemNode::Dir);
        Ok(())
    }

    async fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let target = self.resolve(path)?;
        let target_for_io = target.clone();
        // std's create_dir_all already succeeds when the target exists as
        // a directory and refuses when a path component is a file.
        spawn_blocking_io(move || std::fs::create_dir_all(target_for_io)).await?;
        // Mirror every component the real call may have created.
        let mut nodes = self.lock_nodes()?;
        let mut cursor = self.inner.root.clone();
        let relative = target
            .strip_prefix(&self.inner.root)
            .map_err(|_| escape_error())?;
        for component in relative.components() {
            let Component::Normal(segment) = component else {
                continue;
            };
            cursor.push(segment);
            nodes.entry(cursor.clone()).or_insert(MemNode::Dir);
        }
        Ok(())
    }

    async fn remove_dir(&self, path: &Path) -> io::Result<()> {
        let target = self.resolve(path)?;
        // Real-first: NotFound and DirectoryNotEmpty (ENOTEMPTY) arrive
        // with exact std kinds; an empty directory is required before the
        // namespace may forget it.
        let target_for_io = target.clone();
        spawn_blocking_io(move || std::fs::remove_dir(target_for_io)).await?;
        match self.lock_nodes()?.remove(&target) {
            Some(MemNode::Dir) => Ok(()),
            Some(MemNode::File(_)) | None => Err(divergence_error(&target)),
        }
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        let target = self.resolve(path)?;
        // Real-first: removing a directory path fails with IsADirectory and
        // a missing one with NotFound, both before memory is touched.
        let target_for_io = target.clone();
        spawn_blocking_io(move || std::fs::remove_file(target_for_io)).await?;
        match self.lock_nodes()?.remove(&target) {
            Some(MemNode::File(_)) => Ok(()),
            Some(MemNode::Dir) | None => Err(divergence_error(&target)),
        }
    }

    async fn read_dir(&self, path: &Path) -> io::Result<ReadDir> {
        let target = self.resolve(path)?;
        // The shadow directory holds exactly the namespace's names (the
        // invariant), so the real listing IS the memory listing; its ORDER
        // is the host's, as UnixVfs's is (module docs).
        asupersync::fs::read_dir(target).await
    }

    async fn remove_dir_all(&self, path: &Path) -> io::Result<()> {
        let target = self.resolve(path)?;
        let target_for_io = target.clone();
        spawn_blocking_io(move || std::fs::remove_dir_all(target_for_io)).await?;
        let mut nodes = self.lock_nodes()?;
        let doomed: Vec<PathBuf> = nodes
            .range(target.clone()..)
            .take_while(|(key, _)| key.starts_with(&target))
            .map(|(key, _)| key.clone())
            .collect();
        for key in doomed {
            nodes.remove(&key);
        }
        Ok(())
    }

    async fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let from = self.resolve(from)?;
        let to = self.resolve(to)?;
        // Real-first gives every POSIX rename law its exact std kind:
        // dir-onto-nonempty-dir is ENOTEMPTY, file-onto-dir is EISDIR, and
        // file-onto-file silently replaces.
        let from_for_io = from.clone();
        let to_for_io = to.clone();
        spawn_blocking_io(move || std::fs::rename(from_for_io, to_for_io)).await?;
        let mut nodes = self.lock_nodes()?;
        // Re-key the moved entry — and, for a directory, its whole ordered
        // subtree — in two phases: collect the ranged keys, remove each,
        // then insert under its new prefix. A replaced destination (file
        // over file, or the emptied target directory) is overwritten by the
        // moved entry exactly as the real rename replaced it.
        let subtree_keys: Vec<PathBuf> = nodes
            .range(from.clone()..)
            .take_while(|(key, _)| key.starts_with(&from))
            .map(|(key, _)| key.clone())
            .collect();
        let mut moved: Vec<(PathBuf, MemNode)> = Vec::with_capacity(subtree_keys.len());
        for key in subtree_keys {
            match nodes.remove(&key) {
                Some(node) => moved.push((key, node)),
                None => return Err(divergence_error(&key)),
            }
        }
        for (old_key, node) in moved {
            let new_key = if old_key == from {
                to.clone()
            } else {
                match old_key.strip_prefix(&from) {
                    Ok(relative) if !relative.as_os_str().is_empty() => to.join(relative),
                    Ok(_) => to.clone(),
                    Err(_) => return Err(divergence_error(&old_key)),
                }
            };
            nodes.insert(new_key, node);
        }
        Ok(())
    }

    async fn copy(&self, src: &Path, dst: &Path) -> io::Result<u64> {
        let src = self.resolve(src)?;
        let dst = self.resolve(dst)?;
        // Content is served from RAM; the real copy exists only to give
        // the shadow destination its length (std copies the sparse source
        // as zero blocks, which is exactly a correctly sized shadow). The
        // bytes below come from the namespace, never from the disk.
        let src_bytes = match self.lock_nodes()?.get(&src).cloned() {
            Some(MemNode::File(shared)) => shared.lock_bytes()?.clone(),
            Some(MemNode::Dir) => {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    "is a directory",
                ));
            }
            None => return Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        };
        let copied = {
            let src_for_io = src.clone();
            let dst_for_io = dst.clone();
            let dst_for_shadow = dst.clone();
            spawn_blocking_io(move || {
                let copied = std::fs::copy(src_for_io, dst_for_io)?;
                // A write handle to the new shadow inode for future length
                // syncs, mirroring what open() would have left behind.
                let shadow = std::fs::OpenOptions::new()
                    .write(true)
                    .open(dst_for_shadow)?;
                Ok((copied, shadow))
            })
            .await?
        };
        let shared = Arc::new(FileShared {
            bytes: Mutex::new(src_bytes),
            shadow: Mutex::new(copied.1),
        });
        self.lock_nodes()?.insert(dst, MemNode::File(shared));
        Ok(copied.0)
    }

    async fn hard_link(&self, _original: &Path, _link: &Path) -> io::Result<()> {
        // Structurally absent: multiple namespace names for one content
        // would break the shadow invariant (one real file per entry), so
        // this fails closed with the asupersync path_ops convention.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "hard links are unsupported on the MemVfs memory filesystem",
        ))
    }

    async fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        let target = self.resolve(path)?;
        // Existence is checked against the namespace (not the shadow), so
        // the answer is authoritative even under a shadow race; with no
        // symlinks, canonical == lexical resolution.
        match self.lock_nodes()?.contains_key(&target) {
            true => Ok(target),
            false => Err(io::Error::new(io::ErrorKind::NotFound, "no such file")),
        }
    }

    async fn read_link(&self, _path: &Path) -> io::Result<PathBuf> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "symbolic links are unsupported on the MemVfs memory filesystem",
        ))
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        // Through the handle, so the bytes come from RAM; reading the
        // shadow would return the holes' zeros.
        let mut file = self.open(path, &OpenOptions::new().read(true)).await?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).await?;
        Ok(bytes)
    }

    async fn read_to_string(&self, path: &Path) -> io::Result<String> {
        let bytes = self.read(path).await?;
        String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        // Creates or truncates, exactly like std::fs::write, then lands the
        // bytes in RAM through the reconciled handle.
        let mut file = self
            .open(
                path,
                &OpenOptions::new().write(true).create(true).truncate(true),
            )
            .await?;
        file.write_all(contents).await?;
        file.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::context::{CommitCx, PurposeContexts};

    /// Drives one async test under the lab runtime, exactly as the spine's
    /// own tests do — MemVfs is product code and must behave under the same
    /// executor its only consumer uses.
    fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
    where
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let (output, report) = run_async_under_lab(seed, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            test(contexts.commit()).await
        });
        assert!(
            report.lab_test_passed(),
            "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
        );
        output
    }

    async fn names_of(vfs: &MemVfs, dir: &Path) -> io::Result<Vec<String>> {
        let mut entries = vfs.read_dir(dir).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        Ok(names)
    }

    /// The std open matrix MemVfs inherits from the real filesystem via the
    /// shadow: existence is required without create, creation needs write,
    /// truncate needs write, append and truncate refuse each other, and
    /// create_new refuses an existing entry. Refused opens change nothing.
    #[test]
    fn open_options_follow_the_real_std_matrix() {
        under_lab(0x4D_01, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            let file = Path::new("letters");
            vfs.write(file, b"keep").await.expect("seed");

            let read_absent = vfs
                .open(Path::new("absent"), &OpenOptions::new().read(true))
                .await
                .expect_err("absent file without create must not open");
            assert_eq!(read_absent.kind(), io::ErrorKind::NotFound);

            let create_readonly = vfs
                .open(
                    Path::new("absent"),
                    &OpenOptions::new().read(true).create(true),
                )
                .await
                .expect_err("create without write must be refused");
            assert_eq!(create_readonly.kind(), io::ErrorKind::InvalidInput);

            let truncate_readonly = vfs
                .open(file, &OpenOptions::new().read(true).truncate(true))
                .await
                .expect_err("truncate without write must be refused");
            assert_eq!(truncate_readonly.kind(), io::ErrorKind::InvalidInput);

            let append_truncate = vfs
                .open(
                    file,
                    &OpenOptions::new().write(true).append(true).truncate(true),
                )
                .await
                .expect_err("append and truncate must refuse each other");
            assert_eq!(append_truncate.kind(), io::ErrorKind::InvalidInput);

            let conflict = vfs
                .open(file, &OpenOptions::new().write(true).create_new(true))
                .await
                .expect_err("create_new over an existing file must fail");
            assert_eq!(conflict.kind(), io::ErrorKind::AlreadyExists);

            assert_eq!(vfs.read(file).await.expect("seed intact"), b"keep");
        });
    }

    /// Reads serve RAM, writes land in RAM, the cursor obeys seek, a seek
    /// past EOF zero-fills on the next write, set_len truncates and extends,
    /// and — the load-bearing shadow property — `VfsFile::metadata().len()`
    /// tracks the in-memory length exactly, because the spine reads it on
    /// product paths (torn-tail recovery, capsule comparison, root framing).
    #[test]
    fn content_seek_truncate_and_shadow_length_agree() {
        under_lab(0x4D_02, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            let file = Path::new("log");

            let mut handle = vfs
                .open(
                    file,
                    &OpenOptions::new().read(true).write(true).create(true),
                )
                .await
                .expect("opens");
            handle.write_all(b"hello world").await.expect("writes");
            let metadata = handle.metadata().await.expect("metadata");
            assert_eq!(metadata.len(), 11, "shadow length tracks the write");

            handle
                .seek(SeekFrom::Start(6))
                .await
                .expect("seeks to Start");
            let mut tail = Vec::new();
            handle.read_to_end(&mut tail).await.expect("reads to EOF");
            assert_eq!(tail, b"world");

            handle
                .seek(SeekFrom::Current(-5))
                .await
                .expect("seeks back");
            handle
                .write_all(b"WORLD")
                .await
                .expect("overwrites in place");
            assert_eq!(
                vfs.read(file).await.expect("reads back"),
                b"hello WORLD",
                "seeked write overwrites, not appends"
            );

            handle.seek(SeekFrom::End(0)).await.expect("seeks to end");
            let mut eof = Vec::new();
            handle.read_to_end(&mut eof).await.expect("EOF reads empty");
            assert!(eof.is_empty());

            // Seek four past EOF, write one byte: the hole zero-fills.
            handle.seek(SeekFrom::End(4)).await.expect("seeks past EOF");
            handle.write_all(b"!").await.expect("writes into the hole");
            assert_eq!(
                vfs.read(file).await.expect("reads hole"),
                b"hello WORLD\0\0\0\0!",
                "hole fill is zeros"
            );

            let negative = handle
                .seek(SeekFrom::Current(-1000))
                .await
                .expect_err("seek before start must fail");
            assert_eq!(negative.kind(), io::ErrorKind::InvalidInput);

            handle.set_len(5).await.expect("truncates");
            assert_eq!(vfs.read(file).await.expect("reads truncated"), b"hello");
            handle.set_len(7).await.expect("extends");
            assert_eq!(
                vfs.read(file).await.expect("reads extended"),
                b"hello\0\0",
                "extension zero-fills"
            );
        });
    }

    /// Handles over one file share its content and keep independent cursors;
    /// a handle keeps serving an unlinked file's bytes until it is dropped,
    /// exactly as an open descriptor survives unlink on a real filesystem,
    /// while the namespace itself stops naming the file.
    #[test]
    fn handles_share_content_and_survive_unlink() {
        under_lab(0x4D_03, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            let file = Path::new("shared");
            vfs.write(file, b"alpha").await.expect("seeds");

            let mut writer = vfs
                .open(file, &OpenOptions::new().write(true))
                .await
                .expect("opens writer");
            writer.seek(SeekFrom::End(0)).await.expect("seeks to end");
            writer.write_all(b"-beta").await.expect("appends");

            let mut reader = vfs
                .open(file, &OpenOptions::new().read(true))
                .await
                .expect("opens reader");
            let mut seen = Vec::new();
            reader.read_to_end(&mut seen).await.expect("reads");
            assert_eq!(seen, b"alpha-beta", "reader sees the writer's bytes");

            vfs.remove_file(file).await.expect("unlinks");
            reader.seek(SeekFrom::Start(0)).await.expect("rewinds");
            let mut after_unlink = Vec::new();
            reader
                .read_to_end(&mut after_unlink)
                .await
                .expect("still reads");
            assert_eq!(after_unlink, b"alpha-beta", "unlinked inode stays alive");

            let gone = vfs
                .metadata(file)
                .await
                .expect_err("namespace no longer names it");
            assert_eq!(gone.kind(), io::ErrorKind::NotFound);
        });
    }

    /// Parent-directory tracking gives the honest POSIX refusals: ENOENT for
    /// missing parents, ENOTDIR for file-as-parent, EEXIST for duplicates,
    /// ENOTEMPTY for non-empty removal, EISDIR for file operations on
    /// directories — and read_dir lists exactly the namespace's names.
    #[test]
    fn directory_semantics_fail_closed_like_posix() {
        under_lab(0x4D_04, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            vfs.create_dir(Path::new("people")).await.expect("mkdir");

            let duplicate = vfs
                .create_dir(Path::new("people"))
                .await
                .expect_err("duplicate mkdir");
            assert_eq!(duplicate.kind(), io::ErrorKind::AlreadyExists);

            let orphan = vfs
                .write(Path::new("people/edges/x"), b"no")
                .await
                .expect_err("missing parent directory");
            assert_eq!(orphan.kind(), io::ErrorKind::NotFound);

            vfs.write(Path::new("plain"), b"file").await.expect("file");
            let not_a_dir = vfs
                .create_dir(Path::new("plain/sub"))
                .await
                .expect_err("file as parent");
            assert_eq!(not_a_dir.kind(), io::ErrorKind::NotADirectory);

            let dir_as_file = vfs
                .remove_file(Path::new("people"))
                .await
                .expect_err("unlink on a directory");
            assert_eq!(dir_as_file.kind(), io::ErrorKind::IsADirectory);

            vfs.write(Path::new("people/a"), b"1").await.expect("entry");
            let non_empty = vfs
                .remove_dir(Path::new("people"))
                .await
                .expect_err("rmdir non-empty");
            assert_eq!(non_empty.kind(), io::ErrorKind::DirectoryNotEmpty);

            vfs.remove_file(Path::new("people/a")).await.expect("clean");
            vfs.remove_dir(Path::new("people"))
                .await
                .expect("rmdir empty");
            let gone = vfs
                .metadata(Path::new("people"))
                .await
                .expect_err("removed dir is gone");
            assert_eq!(gone.kind(), io::ErrorKind::NotFound);

            vfs.create_dir(Path::new("people")).await.expect("re-mkdir");
            vfs.write(Path::new("people/a"), b"1").await.expect("a");
            vfs.write(Path::new("people/b"), b"2").await.expect("b");
            let mut names = names_of(&vfs, Path::new("people")).await.expect("lists");
            names.sort();
            assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        });
    }

    /// Rename obeys the POSIX matrix through the real shadow: files move
    /// with content, files replace files, directories move subtrees,
    /// dir-onto-nonempty-dir is ENOTEMPTY, file-onto-dir is EISDIR, and the
    /// absent source is NotFound.
    #[test]
    fn rename_moves_files_and_directory_subtrees() {
        under_lab(0x4D_05, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");

            vfs.write(Path::new("src"), b"payload").await.expect("seed");
            vfs.rename(Path::new("src"), Path::new("dst"))
                .await
                .expect("renames file");
            assert!(vfs.read(Path::new("src")).await.is_err(), "old name gone");
            assert_eq!(
                vfs.read(Path::new("dst")).await.expect("content moved"),
                b"payload"
            );

            vfs.write(Path::new("victim"), b"old")
                .await
                .expect("victim");
            vfs.rename(Path::new("dst"), Path::new("victim"))
                .await
                .expect("replace");
            assert_eq!(
                vfs.read(Path::new("victim")).await.expect("winner"),
                b"payload"
            );

            vfs.create_dir(Path::new("tree")).await.expect("tree");
            vfs.create_dir(Path::new("tree/inner"))
                .await
                .expect("inner");
            vfs.write(Path::new("tree/inner/leaf"), b"l")
                .await
                .expect("leaf");
            vfs.rename(Path::new("tree"), Path::new("grove"))
                .await
                .expect("renames subtree");
            assert_eq!(
                vfs.read(Path::new("grove/inner/leaf"))
                    .await
                    .expect("deep content moved"),
                b"l"
            );

            vfs.create_dir(Path::new("full")).await.expect("full");
            vfs.write(Path::new("full/thing"), b"t")
                .await
                .expect("thing");
            let not_empty = vfs
                .rename(Path::new("grove"), Path::new("full"))
                .await
                .expect_err("dir onto non-empty dir");
            assert_eq!(not_empty.kind(), io::ErrorKind::DirectoryNotEmpty);

            let file_onto_dir = vfs
                .rename(Path::new("grove/inner/leaf"), Path::new("full"))
                .await
                .expect_err("file onto dir");
            assert_eq!(file_onto_dir.kind(), io::ErrorKind::IsADirectory);

            let absent = vfs
                .rename(Path::new("nothing"), Path::new("elsewhere"))
                .await
                .expect_err("renaming nothing");
            assert_eq!(absent.kind(), io::ErrorKind::NotFound);
        });
    }

    /// copy duplicates content and returns the length; canonicalize resolves
    /// within the namespace, refuses escapes and the absent; hard_link and
    /// read_link fail closed as Unsupported, the asupersync path_ops
    /// convention for structurally absent operations.
    #[test]
    fn copy_canonicalize_and_unsupported_ops() {
        under_lab(0x4D_06, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            vfs.write(Path::new("book"), b"chapter one")
                .await
                .expect("seed");

            let copied = vfs
                .copy(Path::new("book"), Path::new("book.copy"))
                .await
                .expect("copies");
            assert_eq!(copied, 11);
            assert_eq!(
                vfs.read(Path::new("book.copy"))
                    .await
                    .expect("copy content"),
                b"chapter one"
            );

            let absent_copy = vfs
                .copy(Path::new("absent"), Path::new("wherever"))
                .await
                .expect_err("copying an absent file");
            assert_eq!(absent_copy.kind(), io::ErrorKind::NotFound);

            let canonical = vfs
                .canonicalize(Path::new("book.copy"))
                .await
                .expect("canonicalizes");
            assert!(canonical.ends_with("book.copy"), "absolute: {canonical:?}");

            let dot = vfs
                .canonicalize(Path::new("."))
                .await
                .expect("dot is the root");
            assert_eq!(dot, vfs.inner.root);

            let escape = vfs
                .canonicalize(Path::new("../../etc"))
                .await
                .expect_err("climbing out is refused");
            assert_eq!(escape.kind(), io::ErrorKind::InvalidInput);

            let absent = vfs
                .canonicalize(Path::new("absent"))
                .await
                .expect_err("canonicalize verifies existence");
            assert_eq!(absent.kind(), io::ErrorKind::NotFound);

            let hard_link = vfs
                .hard_link(Path::new("book"), Path::new("book.link"))
                .await
                .expect_err("hard links are structurally absent");
            assert_eq!(hard_link.kind(), io::ErrorKind::Unsupported);
            let read_link = vfs
                .read_link(Path::new("book"))
                .await
                .expect_err("symlinks are structurally absent");
            assert_eq!(read_link.kind(), io::ErrorKind::Unsupported);
        });
    }

    /// create_dir_all builds chains, tolerates an existing target, and
    /// refuses a file masquerading as a path component.
    #[test]
    fn create_dir_all_builds_and_verifies_chains() {
        under_lab(0x4D_07, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            vfs.create_dir_all(Path::new("deep/nested/tree"))
                .await
                .expect("builds chain");
            let metadata = vfs
                .metadata(Path::new("deep/nested/tree"))
                .await
                .expect("exists");
            assert!(metadata.is_dir());
            vfs.create_dir_all(Path::new("deep/nested/tree"))
                .await
                .expect("existing target is fine");

            vfs.write(Path::new("deep/nested/tree/blocker"), b"f")
                .await
                .expect("file");
            let blocked = vfs
                .create_dir_all(Path::new("deep/nested/tree/blocker/more"))
                .await
                .expect_err("file in the chain");
            assert_eq!(blocked.kind(), io::ErrorKind::NotADirectory);
        });
    }

    /// The shadow root lives while any clone lives and is removed with the
    /// last one; clones share one namespace, which is what makes a retained
    /// MemVfs a reopen handle rather than a copy.
    #[test]
    fn shadow_root_tracks_clones_and_namespaces_stay_shared() {
        under_lab(0x4D_08, |_cx| async move {
            let vfs = MemVfs::new().expect("memory filesystem");
            let root = vfs.inner.root.clone();
            assert!(root.is_dir(), "shadow root exists while held");

            let retained = vfs.clone();
            drop(vfs);
            assert!(root.is_dir(), "a retained clone keeps the shadow");

            retained
                .write(Path::new("clone-written"), b"x")
                .await
                .expect("write via clone");
            let seen = retained
                .read(Path::new("clone-written"))
                .await
                .expect("same namespace");
            assert_eq!(seen, b"x");

            drop(retained);
            assert!(!root.exists(), "last drop removes the shadow root");
        });
    }
}
