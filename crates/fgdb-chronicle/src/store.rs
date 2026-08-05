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
//! Doctrine 3 requires an explicit capability context for every I/O path.
//! Persisted reads accept the sealed `StorageReadCx` capability, while root
//! creation and publication stay on `CommitCx`. That split lets read-only roles
//! inspect durable state without gaining any way to publish it.
//!
//! ALL I/O GOES THROUGH A [`Vfs`]. The store is generic over asupersync's
//! filesystem trait and defaults to [`UnixVfs`], so production behaviour is
//! the real filesystem while the lab hands in a faulting implementation that
//! can lie about fsync, tear a write, flip a bit, or refuse space — the §15
//! fault classes `std::fs` structurally could not model. The async boundary
//! exists for the same reason: `VfsFile`'s operations are futures, so the lab
//! runtime owns every scheduling point of the durable path.
//!
//! NOT HERE, deliberately: the filesystem profile (sector size, atomicity
//! class) that decides how large a write may be before it can tear, which is
//! bead `w2-filesystem-profiles`; and the publication sequencer/permit
//! machinery of `w2-root-publication`. This module implements the alternation
//! and durability barrier those two will configure and sequence.

use crate::root::{
    ROOT_FILE_LEN, RootSelection, RootSlot, SLOT_A_OFFSET, SLOT_B_OFFSET, SLOT_LEN, select_root,
};
use asupersync::fs::{OpenOptions, UnixVfs, Vfs, VfsFile};
use asupersync::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use fgdb_types::StorageReadCx;
use fgdb_types::context::CommitCx;
use std::future::Future;
use std::path::{Path, PathBuf};

/// Run the ordered durability work for a new or previously uncertain
/// directory entry.
///
/// A file sync makes the inode contents durable; it does not make the name by
/// which recovery finds that inode durable. Creation therefore owes both
/// operations, in this order, under the same `CommitCx` boundary. The hook is
/// test-facing: crash matrices stop after the inode sync and before the
/// directory sync without maintaining a second, weaker implementation path.
pub(crate) async fn sync_created_entry<V: Vfs>(
    cx: &CommitCx,
    vfs: &V,
    file: &V::File,
    parent_directory: &Path,
    after_file_sync: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    run_created_entry_barrier(
        || sync_file(cx, file),
        after_file_sync,
        || sync_directory(cx, vfs, parent_directory),
    )
    .await
}

/// Sync one already-open file through the commit capability boundary.
pub(crate) async fn sync_file<F: VfsFile>(cx: &CommitCx, file: &F) -> std::io::Result<()> {
    cx.with_restriction_async(file.sync_all()).await
}

/// Make the directory entries in `directory` durable.
pub(crate) async fn sync_directory<V: Vfs>(
    cx: &CommitCx,
    vfs: &V,
    directory: &Path,
) -> std::io::Result<()> {
    cx.with_restriction_async(async {
        let directory = vfs.open(directory, &OpenOptions::new().read(true)).await?;
        directory.sync_all().await
    })
    .await
}

async fn run_created_entry_barrier<FileFut, DirFut>(
    sync_file: impl FnOnce() -> FileFut,
    after_file_sync: impl FnOnce() -> std::io::Result<()>,
    sync_parent_directory: impl FnOnce() -> DirFut,
) -> std::io::Result<()>
where
    FileFut: Future<Output = std::io::Result<()>>,
    DirFut: Future<Output = std::io::Result<()>>,
{
    sync_file().await?;
    after_file_sync()?;
    sync_parent_directory().await
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

/// Evidence that one exact publication was observed durable — the plan's
/// `RootPublicationEvidence` in its W1 subset: in-memory and unsigned, never
/// persisted (the durable, signed form arrives with the registered
/// certificate kinds and their signer machinery).
///
/// Minted ONLY from the post-barrier reread: the store re-reads the file it
/// just synced and authenticates that recovery would now select exactly the
/// published slot. Evidence therefore states what a recovering process WOULD
/// see, never what the writer intended — and that difference is precisely
/// the certificate boundary of dual-root publication (§15 LDFI): a lying
/// sync or a torn slot makes the reread refuse, and no evidence exists to
/// hand upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootPublicationEvidence {
    /// Which physical slot the publication landed in: 0 = A, 1 = B.
    pub written_index: u8,
    /// The generation the post-barrier reread selected.
    pub slot_generation: u64,
    /// The root manifest object the selected slot points at.
    pub root_manifest_oid: [u8; 32],
}

/// The continuity head an external CAS authority currently publishes for
/// this database's incarnation lineage (§5.1's `ExternalCas` profile, W1
/// subset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinuityHead {
    /// The authority's current compare-and-swap version.
    pub cas_version: u64,
    /// The incarnation-continuity digest the authority holds at that version.
    pub cluster_incarnation_continuity_digest: [u8; 32],
}

/// The external continuity authority consulted immediately before an
/// irreversible slot write under the `ExternalCas` incarnation-continuity
/// profile ("every … `RootSlot` write … revalidates the exact external
/// continuity head/CAS version").
///
/// A trait rather than a concrete client so the deterministic lab can host
/// the registered model — a linearizable CAS register under the
/// predecessor-digest law (§15) — and fault it: a stale head, a forked head,
/// and an outage are injectable data for the sim, which is what makes the
/// external-CAS boundary of dual-root publication reachable for LDFI.
pub trait ContinuityAuthority {
    /// Fetch the exact current head. An error means UNAVAILABLE, and
    /// publication under `ExternalCas` fails closed on it: "a stale/later/
    /// unavailable external continuity head makes the database read-only" —
    /// never a retry loop inside the store, never a write on faith.
    fn current_head(&self, cx: &CommitCx) -> impl Future<Output = std::io::Result<ContinuityHead>>;
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
    /// The write and barrier reported success, but the post-barrier reread
    /// did not select the published slot. The bytes the writer believes
    /// durable are not the bytes recovery would choose, so no publication
    /// evidence may exist — this is the fail-closed arm of the certificate
    /// boundary.
    PublicationNotObservable { expected_generation: u64 },
    /// The external continuity authority could not be reached. Publication
    /// under `ExternalCas` fails closed rather than writing on faith.
    ContinuityUnavailable(std::io::Error),
    /// The authority's CAS version disagrees with the slot's. Behind means
    /// the writer is publishing against a superseded observation; ahead
    /// means another incarnation has advanced the lineage. Either way this
    /// writer has lost the right to publish until it re-observes the head.
    ContinuityVersionSkew {
        head_cas_version: u64,
        slot_cas_version: u64,
    },
    /// The versions agree but the digests do not: two histories claim the
    /// same CAS position. This is fork evidence, not staleness, and nothing
    /// local can resolve it.
    ContinuityForked {
        cas_version: u64,
        head_digest: [u8; 32],
        slot_digest: [u8; 32],
    },
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
            Self::PublicationNotObservable {
                expected_generation,
            } => write!(
                f,
                "post-barrier reread did not select the published slot at generation \
                 {expected_generation}; the publication is not observable and no evidence exists"
            ),
            Self::ContinuityUnavailable(error) => {
                write!(f, "external continuity authority unavailable: {error}")
            }
            Self::ContinuityVersionSkew {
                head_cas_version,
                slot_cas_version,
            } => write!(
                f,
                "continuity head is at CAS version {head_cas_version} but the slot expects \
                 {slot_cas_version}; re-observe the head before publishing"
            ),
            Self::ContinuityForked { cas_version, .. } => write!(
                f,
                "continuity digests disagree at CAS version {cas_version}: two histories claim \
                 one position; nothing local can resolve this"
            ),
        }
    }
}

impl core::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) | Self::ContinuityUnavailable(error) => Some(error),
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
pub struct RootStore<V: Vfs = UnixVfs> {
    vfs: V,
    path: PathBuf,
}

impl RootStore<UnixVfs> {
    /// Bind to the `manifest.root` inside a database directory on the real
    /// filesystem. Opening is separate from reading so a caller can bind
    /// before the file exists.
    pub fn new(database_dir: impl AsRef<Path>) -> Self {
        Self::with_vfs(UnixVfs::new(), database_dir)
    }
}

impl<V: Vfs> RootStore<V> {
    /// Bind to the `manifest.root` inside a database directory reached through
    /// an explicit [`Vfs`]. This is the constructor the lab uses to interpose
    /// a faulting filesystem; [`RootStore::new`] is the production shape.
    pub fn with_vfs(vfs: V, database_dir: impl AsRef<Path>) -> Self {
        Self {
            vfs,
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
    pub async fn create(&self, cx: &CommitCx, slot: &RootSlot) -> Result<(), StoreError> {
        self.create_with_crash(cx, slot, None).await
    }

    /// Create the first root, optionally stopping between inode and directory
    /// durability. The normal path delegates here so the crash matrix cannot
    /// accidentally test a different sequence of operations.
    #[doc(hidden)]
    pub async fn create_with_crash(
        &self,
        cx: &CommitCx,
        slot: &RootSlot,
        crash_at: Option<RootCreateCrashPoint>,
    ) -> Result<(), StoreError> {
        let bytes = slot.serialize();
        let mut file = self
            .vfs
            .open(&self.path, &OpenOptions::new().write(true).create_new(true))
            .await?;
        file.write_all(&bytes).await?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        let parent = self.path.parent().ok_or_else(|| {
            StoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "manifest.root has no parent directory",
            ))
        })?;
        sync_created_entry(cx, &self.vfs, &file, parent, || {
            if crash_at == Some(RootCreateCrashPoint::AfterFileSyncBeforeDirectorySync) {
                return Err(std::io::Error::other(
                    "crash: root inode durable before directory entry",
                ));
            }
            Ok(())
        })
        .await?;
        Ok(())
    }

    /// Read the file and apply the recovery rule.
    pub async fn recover(&self, cx: &impl StorageReadCx) -> Result<RootSelection, StoreError> {
        let bytes = self.read_file(cx).await?;
        Ok(select_root(&bytes))
    }

    /// The currently published root, or an error naming why there is none.
    pub async fn current(&self, cx: &impl StorageReadCx) -> Result<RootSlot, StoreError> {
        match self.recover(cx).await? {
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
    pub async fn publish(&self, cx: &CommitCx, next: &RootSlot) -> Result<(), StoreError> {
        self.publish_evidenced(cx, next).await.map(|_| ())
    }

    /// Publish, then prove it: the same alternation and barrier as
    /// [`RootStore::publish`], followed by a post-barrier reread that
    /// authenticates recovery would now select exactly `next`. Returns the
    /// [`RootPublicationEvidence`] minted from that reread — the certificate
    /// boundary of dual-root publication in its W1 subset. A publication the
    /// reread cannot observe returns
    /// [`StoreError::PublicationNotObservable`] and mints nothing; the
    /// caller must treat the publication as failed even though bytes may
    /// have moved, exactly as it would for a crash at the same instant.
    pub async fn publish_evidenced(
        &self,
        cx: &CommitCx,
        next: &RootSlot,
    ) -> Result<RootPublicationEvidence, StoreError> {
        self.publish_evidenced_with_steps(cx, next, || Ok(())).await
    }

    /// The exact production path with one deterministic observation point
    /// between the durability barrier and the evidence reread. Witnesses use
    /// it to model interference arriving at that instant — a torn slot the
    /// barrier claimed durable — without maintaining a second, weaker
    /// implementation path; the ordinary caller supplies a no-op.
    #[doc(hidden)]
    pub async fn publish_evidenced_with_steps(
        &self,
        cx: &CommitCx,
        next: &RootSlot,
        after_barrier: impl FnOnce() -> std::io::Result<()>,
    ) -> Result<RootPublicationEvidence, StoreError> {
        let file_bytes = self.read_file(cx).await?;
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
        let mut file = self
            .vfs
            .open(&self.path, &OpenOptions::new().write(true))
            .await?;
        file.seek(SeekFrom::Start(target_offset as u64)).await?;
        file.write_all(&next.serialize()).await?;
        file.flush().await?;
        Self::barrier(cx, &file).await?;
        after_barrier()?;

        // The reread that mints the evidence. It goes through the same Vfs
        // as the write and the barrier, so a lab filesystem that lied about
        // the sync or tore the slot is caught HERE, by the store, before any
        // caller can treat the publication as real.
        let written_index: u8 = if occupied_index == 0 { 1 } else { 0 };
        let reread = self.read_file(cx).await?;
        match select_root(&reread) {
            RootSelection::Selected { slot, index, .. }
                if index == written_index && *slot == *next =>
            {
                Ok(RootPublicationEvidence {
                    written_index,
                    slot_generation: next.slot_generation,
                    root_manifest_oid: next.root_manifest_oid,
                })
            }
            _ => Err(StoreError::PublicationNotObservable {
                expected_generation: next.slot_generation,
            }),
        }
    }

    /// [`RootStore::publish_evidenced`] under the `ExternalCas`
    /// incarnation-continuity profile: revalidate the exact external
    /// continuity head immediately before the irreversible slot write.
    ///
    /// The head must sit at exactly the CAS version the slot carries and
    /// hold exactly the slot's continuity digest; skew, fork, or an
    /// unreachable authority refuses BEFORE any byte moves, which the
    /// witnesses pin by proving the root file is untouched after a refusal.
    /// This is the external-CAS boundary of dual-root publication; the
    /// always-on wiring of profile to path belongs to the publication
    /// sequencer (`w2-root-publication`), which will make the profile choose
    /// the entry point rather than the caller.
    pub async fn publish_with_continuity<A: ContinuityAuthority>(
        &self,
        cx: &CommitCx,
        next: &RootSlot,
        authority: &A,
    ) -> Result<RootPublicationEvidence, StoreError> {
        let head = authority
            .current_head(cx)
            .await
            .map_err(StoreError::ContinuityUnavailable)?;
        if head.cas_version != next.continuity_cas_version {
            return Err(StoreError::ContinuityVersionSkew {
                head_cas_version: head.cas_version,
                slot_cas_version: next.continuity_cas_version,
            });
        }
        if head.cluster_incarnation_continuity_digest != next.cluster_incarnation_continuity_digest
        {
            return Err(StoreError::ContinuityForked {
                cas_version: head.cas_version,
                head_digest: head.cluster_incarnation_continuity_digest,
                slot_digest: next.cluster_incarnation_continuity_digest,
            });
        }
        self.publish_evidenced(cx, next).await
    }

    /// The durability barrier. Separated and named because it is the step a
    /// benchmark is most tempted to skip, and a durability claim measured
    /// without it is not a durability claim (doctrine 7: no non-durable
    /// benchmark mode reported as a result).
    async fn barrier(cx: &CommitCx, file: &V::File) -> Result<(), StoreError> {
        // The capability context and the Vfs are what a lab runtime swaps to
        // inject fsync lies, latency, and crashes at this exact boundary.
        sync_file(cx, file).await?;
        Ok(())
    }

    async fn read_file(&self, cx: &impl StorageReadCx) -> Result<Vec<u8>, StoreError> {
        cx.with_restriction_async(async {
            let mut file = self.vfs.open_read(&self.path).await?;
            let len = file.metadata().await?.len();
            if len != ROOT_FILE_LEN as u64 {
                return Err(StoreError::MalformedFile { len });
            }
            let mut bytes = Vec::with_capacity(ROOT_FILE_LEN);
            file.read_to_end(&mut bytes).await?;
            if bytes.len() != ROOT_FILE_LEN {
                return Err(StoreError::MalformedFile {
                    len: bytes.len() as u64,
                });
            }
            Ok(bytes)
        })
        .await
    }

    /// Which physical slot currently holds the selected root: 0 = A, 1 = B.
    /// Exposed so a test — or an operator — can observe that publishing
    /// actually alternates rather than rewriting one slot forever, which is
    /// the failure mode that silently removes all crash safety.
    pub async fn selected_slot_index(&self, cx: &impl StorageReadCx) -> Result<u8, StoreError> {
        match self.recover(cx).await? {
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
    ///
    /// Deliberately takes no capability context: it models damage arriving
    /// from OUTSIDE the process — a torn write the store never observed — so
    /// routing it through a `CommitCx` would claim an authority the scenario
    /// does not have.
    #[doc(hidden)]
    pub async fn damage_slot_for_test(&self, index: u8, byte: usize) -> Result<(), StoreError> {
        let offset = if index == 0 {
            SLOT_A_OFFSET
        } else {
            SLOT_B_OFFSET
        };
        let mut file = self
            .vfs
            .open(&self.path, &OpenOptions::new().write(true))
            .await?;
        file.seek(SeekFrom::Start((offset + byte % SLOT_LEN) as u64))
            .await?;
        file.write_all(&[0xff]).await?;
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::run_created_entry_barrier;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    /// Drive a future that never actually suspends. The barrier composes
    /// ready futures in these tests, so a single poll must complete it; a
    /// `Pending` here would mean the test harness, not the barrier, blocked.
    fn poll_ready<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut task = Context::from_waker(waker);
        match pin!(future).poll(&mut task) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("barrier future suspended in a ready-only test"),
        }
    }

    #[test]
    fn creation_barrier_orders_inode_hook_then_parent_directory() {
        let events = RefCell::new(Vec::new());
        poll_ready(run_created_entry_barrier(
            || {
                events.borrow_mut().push("file");
                std::future::ready(Ok(()))
            },
            || {
                events.borrow_mut().push("crash-hook");
                Ok(())
            },
            || {
                events.borrow_mut().push("parent-directory");
                std::future::ready(Ok(()))
            },
        ))
        .expect("barrier");

        assert_eq!(
            events.into_inner(),
            ["file", "crash-hook", "parent-directory"]
        );
    }

    #[test]
    fn a_crash_after_inode_sync_prevents_directory_publication() {
        let events = RefCell::new(Vec::new());
        let result = poll_ready(run_created_entry_barrier(
            || {
                events.borrow_mut().push("file");
                std::future::ready(Ok(()))
            },
            || {
                events.borrow_mut().push("crash-hook");
                Err(std::io::Error::other("injected crash"))
            },
            || {
                events.borrow_mut().push("parent-directory");
                std::future::ready(Ok(()))
            },
        ));

        assert!(result.is_err());
        assert_eq!(events.into_inner(), ["file", "crash-hook"]);
    }
}
