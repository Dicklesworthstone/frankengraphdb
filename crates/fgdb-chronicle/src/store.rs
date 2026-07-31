//! The durable `manifest.root` store: where Chronicle finally touches a disk.
//!
//! Everything else in this crate is pure computation over bytes. This module
//! is the one place that opens a file, and it exists to make the dual-slot
//! discipline real rather than notional:
//!
//! **A publish writes the slot that is NOT currently holding the newest
//! credible root, and fsyncs before anything else happens.** That single rule
//! is what makes a crash survivable at every instant: while slot X is being
//! written it is not the slot recovery would choose, so a torn or partial
//! write destroys a slot nobody was depending on. The previous generation
//! remains complete, authenticated, and selectable throughout.
//!
//! `&CommitCx` is required by doctrine 3 — every function that performs I/O
//! takes a capability context. It is what makes the lab runtime able to run
//! this path under injected latency, torn writes, and fsync lies without the
//! code knowing the difference.
//!
//! NOT HERE, deliberately: the filesystem profile (sector size, atomicity
//! class) that decides how large a write may be before it can tear, which is
//! bead `w2-filesystem-profiles`; and the publication sequencer/permit
//! machinery of `w2-root-publication`. This module implements the alternation
//! and durability barrier those two will configure and sequence.

use crate::root::{
    ROOT_FILE_LEN, RootSelection, RootSlot, SLOT_A_OFFSET, SLOT_B_OFFSET, SLOT_LEN, select_root,
};
use fgdb_types::context::CommitCx;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Run the ordered durability work for a new or previously uncertain
/// directory entry.
///
/// A file sync makes the inode contents durable; it does not make the name by
/// which recovery finds that inode durable. Creation therefore owes both
/// operations, in this order, under the same `CommitCx` boundary. The hook is
/// test-facing: crash matrices stop after the inode sync and before the
/// directory sync without maintaining a second, weaker implementation path.
pub(crate) fn sync_created_entry(
    cx: &CommitCx,
    file: &File,
    parent_directory: &Path,
    after_file_sync: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    run_created_entry_barrier(
        || cx.with_restriction(|| file.sync_all()),
        after_file_sync,
        || sync_directory(cx, parent_directory),
    )
}

/// Sync one already-open file through the commit capability boundary.
pub(crate) fn sync_file(cx: &CommitCx, file: &File) -> std::io::Result<()> {
    cx.with_restriction(|| file.sync_all())
}

/// Make the directory entries in `directory` durable.
pub(crate) fn sync_directory(cx: &CommitCx, directory: &Path) -> std::io::Result<()> {
    let directory = File::open(directory)?;
    cx.with_restriction(|| directory.sync_all())
}

fn run_created_entry_barrier(
    sync_file: impl FnOnce() -> std::io::Result<()>,
    after_file_sync: impl FnOnce() -> std::io::Result<()>,
    sync_parent_directory: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    sync_file()?;
    after_file_sync()?;
    sync_parent_directory()
}

/// The published root file's name inside a database directory.
pub const ROOT_FILE_NAME: &str = "manifest.root";

/// The creation-only crash instant that distinguishes inode durability from
/// namespace durability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootCreateCrashPoint {
    /// Both root slots and the root inode are durable, but the parent
    /// directory has not yet made the new `manifest.root` name durable.
    AfterFileSyncBeforeDirectorySync,
}

/// Why a durable root operation failed.
#[derive(Debug)]
pub enum StoreError {
    /// The underlying file operation failed. Carried whole because an
    /// operator debugging a durability problem needs the real errno, not a
    /// summary of it.
    Io(std::io::Error),
    /// The file exists but is not two slots long. A short root file is not
    /// "empty" — it is a truncated one, and treating it as fresh would
    /// discard a database.
    MalformedFile { len: u64 },
    /// No slot in the file is structurally credible, so there is nothing to
    /// publish over safely and nothing to recover.
    NoCredibleRoot,
    /// Two credible slots at one generation disagree. Publishing over either
    /// could destroy the one an acknowledged commit depended on, so a writer
    /// must resolve this through the takeover/convergence path first.
    DivergentPair { generation: u64 },
    /// The new generation is not strictly greater than the current one.
    /// Publishing it would make recovery's "highest generation wins" rule
    /// select the older state.
    NonMonotonicGeneration { current: u64, proposed: u64 },
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "root file I/O failed: {error}"),
            Self::MalformedFile { len } => {
                write!(f, "root file is {len} bytes, not {ROOT_FILE_LEN}")
            }
            Self::NoCredibleRoot => f.write_str("no structurally credible root slot"),
            Self::DivergentPair { generation } => write!(
                f,
                "two credible slots at generation {generation} disagree; resolve before writing"
            ),
            Self::NonMonotonicGeneration { current, proposed } => write!(
                f,
                "proposed generation {proposed} does not exceed current {current}"
            ),
        }
    }
}

impl core::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// A database directory's published root.
#[derive(Debug)]
pub struct RootStore {
    path: PathBuf,
}

impl RootStore {
    /// Bind to the `manifest.root` inside a database directory. Opening is
    /// separate from reading so a caller can bind before the file exists.
    pub fn new(database_dir: impl AsRef<Path>) -> Self {
        Self {
            path: database_dir.as_ref().join(ROOT_FILE_NAME),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create the root file with its first published generation.
    ///
    /// Both slots are written with the same bytes. That is not redundancy for
    /// its own sake: it means the very first recovery finds an identical pair
    /// rather than one credible slot beside 4096 zero bytes, so the identical-
    /// pair rule covers genesis exactly as it covers any later convergence.
    /// Creation returns only after both the root inode and its parent-directory
    /// entry are durable.
    pub fn create(&self, cx: &CommitCx, slot: &RootSlot) -> Result<(), StoreError> {
        self.create_with_crash(cx, slot, None)
    }

    /// Create the first root, optionally stopping between inode and directory
    /// durability. The normal path delegates here so the crash matrix cannot
    /// accidentally test a different sequence of operations.
    #[doc(hidden)]
    pub fn create_with_crash(
        &self,
        cx: &CommitCx,
        slot: &RootSlot,
        crash_at: Option<RootCreateCrashPoint>,
    ) -> Result<(), StoreError> {
        let bytes = slot.serialize();
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        file.write_all(&bytes)?;
        file.write_all(&bytes)?;
        let parent = self.path.parent().ok_or_else(|| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "manifest.root has no parent directory",
            ))
        })?;
        sync_created_entry(cx, &file, parent, || {
            if crash_at == Some(RootCreateCrashPoint::AfterFileSyncBeforeDirectorySync) {
                return Err(std::io::Error::other(
                    "crash: root inode durable before directory entry",
                ));
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Read the file and apply the recovery rule.
    pub fn recover(&self) -> Result<RootSelection, StoreError> {
        let bytes = self.read_file()?;
        Ok(select_root(&bytes))
    }

    /// The currently published root, or an error naming why there is none.
    pub fn current(&self) -> Result<RootSlot, StoreError> {
        match self.recover()? {
            RootSelection::Selected { slot, .. } | RootSelection::IdenticalPair { slot } => {
                Ok(*slot)
            }
            RootSelection::MalformedFile { len } => {
                Err(StoreError::MalformedFile { len: len as u64 })
            }
            RootSelection::NoCredibleSlot { .. } => Err(StoreError::NoCredibleRoot),
            RootSelection::DivergentPair { generation } => {
                Err(StoreError::DivergentPair { generation })
            }
        }
    }

    /// PUBLISH. Write `next` into the slot that is not currently selected,
    /// then fsync.
    ///
    /// The ordering is the durability argument, so it is worth stating
    /// plainly: at no instant between the first byte of the write and the
    /// completion of the barrier is the slot being written the one recovery
    /// would choose. A crash anywhere in that window therefore leaves the
    /// previous generation whole. Only once the barrier returns does the new
    /// generation become the highest credible one, and it becomes so
    /// atomically from any reader's perspective, because selection is by
    /// generation and the new slot is either fully durable or not credible.
    pub fn publish(&self, cx: &CommitCx, next: &RootSlot) -> Result<(), StoreError> {
        let file_bytes = self.read_file()?;
        let (current_generation, occupied_index) = match select_root(&file_bytes) {
            RootSelection::Selected { slot, index, .. } => (slot.slot_generation, index),
            // An identical pair occupies both slots equally; writing either is
            // safe because the other still holds the same authenticated state.
            RootSelection::IdenticalPair { slot } => (slot.slot_generation, 0),
            RootSelection::MalformedFile { len } => {
                return Err(StoreError::MalformedFile { len: len as u64 });
            }
            RootSelection::NoCredibleSlot { .. } => return Err(StoreError::NoCredibleRoot),
            RootSelection::DivergentPair { generation } => {
                return Err(StoreError::DivergentPair { generation });
            }
        };

        // Recovery only ever moves forward, so a publish that does not
        // increase the generation could never be selected — and worse, would
        // leave the writer believing it had published.
        if next.slot_generation <= current_generation {
            return Err(StoreError::NonMonotonicGeneration {
                current: current_generation,
                proposed: next.slot_generation,
            });
        }

        let target_offset = if occupied_index == 0 {
            SLOT_B_OFFSET
        } else {
            SLOT_A_OFFSET
        };
        let mut file = OpenOptions::new().write(true).open(&self.path)?;
        file.seek(SeekFrom::Start(target_offset as u64))?;
        file.write_all(&next.serialize())?;
        Self::barrier(cx, &file)?;
        Ok(())
    }

    /// The durability barrier. Separated and named because it is the step a
    /// benchmark is most tempted to skip, and a durability claim measured
    /// without it is not a durability claim (doctrine 7: no non-durable
    /// benchmark mode reported as a result).
    fn barrier(cx: &CommitCx, file: &File) -> Result<(), StoreError> {
        // The capability context is what a lab runtime swaps to inject fsync
        // lies, latency, and crashes at this exact boundary; the restriction
        // scope is where that interception attaches.
        sync_file(cx, file)?;
        Ok(())
    }

    fn read_file(&self) -> Result<Vec<u8>, StoreError> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        if len != ROOT_FILE_LEN as u64 {
            return Err(StoreError::MalformedFile { len });
        }
        let mut bytes = Vec::with_capacity(ROOT_FILE_LEN);
        file.read_to_end(&mut bytes)?;
        if bytes.len() != ROOT_FILE_LEN {
            return Err(StoreError::MalformedFile {
                len: bytes.len() as u64,
            });
        }
        Ok(bytes)
    }

    /// Which physical slot currently holds the selected root: 0 = A, 1 = B.
    /// Exposed so a test — or an operator — can observe that publishing
    /// actually alternates rather than rewriting one slot forever, which is
    /// the failure mode that silently removes all crash safety.
    pub fn selected_slot_index(&self) -> Result<u8, StoreError> {
        match self.recover()? {
            RootSelection::Selected { index, .. } => Ok(index),
            RootSelection::IdenticalPair { .. } => Ok(0),
            RootSelection::MalformedFile { len } => {
                Err(StoreError::MalformedFile { len: len as u64 })
            }
            RootSelection::NoCredibleSlot { .. } => Err(StoreError::NoCredibleRoot),
            RootSelection::DivergentPair { generation } => {
                Err(StoreError::DivergentPair { generation })
            }
        }
    }

    /// Simulate a crash mid-publish by damaging the slot a publish WOULD have
    /// targeted. Test-facing on purpose: the crash-point matrix (§15) needs to
    /// place a failure at an exact instant, and doing that through the real
    /// write path is how the test stays honest about which bytes moved.
    #[doc(hidden)]
    pub fn damage_slot_for_test(&self, index: u8, byte: usize) -> Result<(), StoreError> {
        let offset = if index == 0 {
            SLOT_A_OFFSET
        } else {
            SLOT_B_OFFSET
        };
        let mut file = OpenOptions::new().write(true).open(&self.path)?;
        file.seek(SeekFrom::Start((offset + byte % SLOT_LEN) as u64))?;
        file.write_all(&[0xff])?;
        file.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::run_created_entry_barrier;
    use std::cell::RefCell;

    #[test]
    fn creation_barrier_orders_inode_hook_then_parent_directory() {
        let events = RefCell::new(Vec::new());
        run_created_entry_barrier(
            || {
                events.borrow_mut().push("file");
                Ok(())
            },
            || {
                events.borrow_mut().push("crash-hook");
                Ok(())
            },
            || {
                events.borrow_mut().push("parent-directory");
                Ok(())
            },
        )
        .expect("barrier");

        assert_eq!(
            events.into_inner(),
            ["file", "crash-hook", "parent-directory"]
        );
    }

    #[test]
    fn a_crash_after_inode_sync_prevents_directory_publication() {
        let events = RefCell::new(Vec::new());
        let result = run_created_entry_barrier(
            || {
                events.borrow_mut().push("file");
                Ok(())
            },
            || {
                events.borrow_mut().push("crash-hook");
                Err(std::io::Error::other("injected crash"))
            },
            || {
                events.borrow_mut().push("parent-directory");
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(events.into_inner(), ["file", "crash-hook"]);
    }
}
