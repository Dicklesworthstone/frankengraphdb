//! The two-fsync commit protocol.
//!
//! This is where everything else in the crate composes into a commit, and the
//! whole design is about one question: **what does a crash at each instant
//! leave behind?**
//!
//! ```text
//!   build capsule  ──▶  D1: capsule durable  ──▶  append marker  ──▶  D2: marker durable
//!        │                      │                       │                     │
//!   nothing written      orphan capsule,          orphan capsule,        COMMITTED
//!                        NOT committed            NOT committed
//! ```
//!
//! **The marker is the commit.** A capsule on disk with no marker naming it is
//! not a commit that half-happened — it is bytes nobody referenced, and
//! recovery discards it. That is what makes the protocol safe with only two
//! barriers and no double-write journal: there is exactly one durable fact
//! that means "committed", and it is written last.
//!
//! D1 must precede the marker for the mirror reason: a marker naming a capsule
//! that is not durable would be a commit pointing at bytes that may not exist.
//! The order is therefore not an optimisation, it is the correctness argument.
//!
//! THE TORN-TAIL RULE. A crash during D2 can leave a partial marker entry at
//! the end of the log. Recovery **truncates a torn tail rather than erroring**:
//! an incomplete final entry means the commit never completed, which is a
//! normal outcome of crashing, not corruption. A torn entry anywhere EARLIER
//! is corruption, because entries before it were durable.

use crate::capsule::{CapsuleError, CapsuleKeys, decode_container, encode_container};
use crate::marker::{ChainError, CommitMarker, EffectSource, MarkerChain};
use crate::store::{sync_created_entry, sync_directory, sync_file};
use asupersync::fs::{OpenOptions, UnixVfs, Vfs, VfsFile};
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use fgdb_crypto::Digest;
use fgdb_types::StorageReadCx;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::ObjectId;
use fgdb_types::{CommitSeq, MarkerRef};
use std::fs::TryLockError;
use std::path::{Path, PathBuf};

/// Sub-directory holding capsule bytes.
pub const CAPSULE_DIR: &str = "capsules";

/// The append-only marker log.
pub const COMMIT_LOG_NAME: &str = "commits.log";

/// Stable inode whose whole-file lock is the sole live-writer authority for a
/// database's commit stream. The file is durable layout scaffolding; the lock
/// itself is process-death-released and never interpreted as durable state.
const COORDINATOR_LOCK_NAME: &str = ".commit-coordinator.lock";

/// Stack buffer used to compare an existing immutable capsule with the
/// deterministic candidate without allocating a second capsule-sized `Vec`.
const CAPSULE_COMPARE_BUFFER_BYTES: usize = 8 * 1024;

/// Version-3 per-entry framing magic.
///
/// Version 3 widens marker graph/branch coordinates to their canonical 128-bit
/// representation. Recovery deliberately rejects the narrower version-2 shape:
/// accepting two shapes without a migration contract would make field boundaries
/// ambiguous and could alias distinct high-bit identities.
pub const ENTRY_MAGIC: [u8; 4] = *b"FGC3";

/// End sentinel for a complete version-3 entry.
const ENTRY_TRAILER_MAGIC: [u8; 4] = *b"FGE3";

const ENTRY_HEADER_BYTES: usize = 8;
const CHAIN_HASH_BYTES: usize = 32;
const ENTRY_TRAILER_BYTES: usize = 8;

/// The largest entry body the writer will ever emit.
///
/// This bound is load-bearing for the torn-tail rule rather than a defensive
/// habit: it is what lets recovery tell "the file ends early" from "the length
/// field is damaged". Without it, a length corrupted to a huge value reads
/// exactly like a truncated tail, and recovery would silently discard every
/// entry after it. A marker is a bounded record — `head_updates` is the only
/// variable part — so a body beyond this is not a marker this code wrote.
pub const MAX_ENTRY_BODY: usize = 64 * 1024;

/// Why a commit or a recovery failed.
#[derive(Debug)]
pub enum CommitError {
    Io(std::io::Error),
    /// The marker violated a structural law of the chain.
    Chain(ChainError),
    /// The caller's capsule identity disagrees with the identity authenticated
    /// by the marker. Writing either object would create an unopenable commit.
    CapsuleRefMismatch {
        capsule_oid: ObjectId,
        marker_capsule_ref: ObjectId,
    },
    /// The marker body is outside the framing profile recovery accepts.
    MarkerTooLarge {
        body_len: usize,
        max_body_len: usize,
    },
    /// The content-addressed capsule path already exists with bytes other than
    /// the deterministic container this commit would publish. Existing
    /// capsule objects are immutable, so the coordinator refuses rather than
    /// overwrite either the prior object or evidence of corruption.
    CapsulePathConflict {
        capsule_oid: ObjectId,
    },
    /// A log entry before the final one is malformed. Unlike a torn tail this
    /// is corruption: entries preceding it were durable, so the damage is not
    /// explained by a crash.
    CorruptLogEntry {
        commit_seq: u64,
    },
    /// The recovered chain does not verify at this sequence.
    ChainDiverged {
        commit_seq: u64,
    },
    /// Sealing or recovering a capsule failed. A capsule that cannot be sealed
    /// must not be committed, and one that cannot be recovered is not a commit
    /// this database can honour.
    Capsule(CapsuleError),
    /// Another live coordinator already owns this database's commit stream.
    /// Recovery and sequence allocation must never run concurrently against
    /// one log, even inside one process.
    WriterAlreadyOpen,
    /// A previous commit failed at or after the marker write, so this
    /// coordinator can no longer know whether the durable log contains that
    /// entry. Reopen the directory to find out.
    Poisoned,
}

impl core::fmt::Display for CommitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "commit I/O failed: {error}"),
            Self::Chain(error) => write!(f, "marker rejected: {error}"),
            Self::CapsuleRefMismatch {
                capsule_oid,
                marker_capsule_ref,
            } => write!(
                f,
                "capsule identity {capsule_oid:?} disagrees with marker reference \
                 {marker_capsule_ref:?}"
            ),
            Self::MarkerTooLarge {
                body_len,
                max_body_len,
            } => write!(
                f,
                "marker body is {body_len} bytes, above the {max_body_len}-byte framing limit"
            ),
            Self::CapsulePathConflict { capsule_oid } => write!(
                f,
                "immutable capsule path for {capsule_oid:?} contains different bytes"
            ),
            Self::CorruptLogEntry { commit_seq } => {
                write!(f, "commit log entry at seq {commit_seq} is corrupt")
            }
            Self::ChainDiverged { commit_seq } => {
                write!(f, "commit chain diverges at seq {commit_seq}")
            }
            Self::Capsule(error) => write!(f, "capsule: {error}"),
            Self::WriterAlreadyOpen => {
                f.write_str("another commit coordinator already owns this database")
            }
            Self::Poisoned => write!(
                f,
                "coordinator poisoned by an interrupted commit; reopen the database directory"
            ),
        }
    }
}

impl core::error::Error for CommitError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Chain(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CommitError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<CapsuleError> for CommitError {
    fn from(error: CapsuleError) -> Self {
        Self::Capsule(error)
    }
}

impl From<ChainError> for CommitError {
    fn from(error: ChainError) -> Self {
        Self::Chain(error)
    }
}

/// Where a commit may be interrupted. Test-facing: the crash-point matrix
/// (§15) needs to place a failure at an exact instant, and naming the instants
/// in the protocol itself keeps the test honest about which step it stopped
/// after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashPoint {
    /// Before anything is written. Nothing durable changes.
    BeforeCapsule,
    /// Capsule bytes written, D1 barrier NOT reached.
    AfterCapsuleBeforeD1,
    /// The capsule inode is durable, but its entry in `capsules/` is not yet
    /// directory-durable.
    AfterCapsuleFileSyncBeforeDirectorySync,
    /// The capsule entry is durable inside `capsules/`, but the database
    /// directory has not yet made a newly opened capsule directory durable.
    AfterCapsuleDirectorySyncBeforeParentDirectorySync,
    /// D1 complete: the capsule is durable, but no marker exists yet.
    AfterD1,
    /// Marker bytes appended, D2 barrier NOT reached.
    AfterMarkerBeforeD2,
    /// The marker-log inode is durable, but a newly created `commits.log`
    /// entry has not yet been made durable in the database directory.
    AfterMarkerFileSyncBeforeDirectorySync,
}

/// The single-actor commit coordinator.
///
/// One actor allocates every `CommitSeq`, which is what makes the sequence
/// gap-free without coordination: there is no second allocator to race. The
/// type retains a process-death-released whole-file lease for its lifetime, so
/// a caller cannot hold two coordinators over one log.
///
/// Every durable read and write goes through the [`Vfs`] the coordinator was
/// opened with; production is [`UnixVfs`], the lab hands in a faulting one.
/// The one deliberate exception is the writer lease below.
#[derive(Debug)]
pub struct CommitCoordinator<V: Vfs = UnixVfs> {
    vfs: V,
    dir: PathBuf,
    /// The whole-file lock is process-liveness authority, not durable state:
    /// it is released by process death and never read back. `Vfs` has no lock
    /// surface — a faulting filesystem that could "lie" about lock ownership
    /// would model nothing real — so the lease stays on `std::fs` on purpose.
    _writer_lease: std::fs::File,
    keys: CapsuleKeys,
    chain: MarkerChain,
    discarded_tail_bytes: usize,
    poisoned: bool,
    capsule_directory_parent_sync_pending: bool,
    commit_log_parent_sync_pending: bool,
}

/// How an entry failed to decode. The two arms ARE the torn-tail rule: bytes
/// missing at the end of the file is a crash, bytes present but wrong is
/// damage. See [`CommitCoordinator::recover_chain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryDefect {
    /// Fewer bytes remain than a complete entry needs.
    Truncated,
    /// Enough bytes are present, but they are not an entry this writer emitted.
    Corrupt,
}

/// One decoded commit-log frame and one bounded reader step. Naming both keeps
/// the streaming reader's result vocabulary legible without boxing a marker on
/// every successful entry.
type DecodedEntry = (CommitMarker, Digest, usize);
type EntryRead = Option<Result<DecodedEntry, EntryDefect>>;

impl CommitCoordinator<UnixVfs> {
    /// Open a database directory's commit stream on the real filesystem,
    /// recovering whatever is durable.
    pub async fn open(
        cx: &CommitCx,
        database_dir: impl AsRef<Path>,
        keys: CapsuleKeys,
    ) -> Result<Self, CommitError> {
        Self::open_with_vfs(cx, UnixVfs::new(), database_dir, keys).await
    }
}

impl<V: Vfs> CommitCoordinator<V> {
    /// Open a database directory's commit stream through an explicit [`Vfs`],
    /// recovering whatever is durable. This is the constructor the lab uses to
    /// interpose a faulting filesystem; [`CommitCoordinator::open`] is the
    /// production shape.
    pub async fn open_with_vfs(
        cx: &CommitCx,
        vfs: V,
        database_dir: impl AsRef<Path>,
        keys: CapsuleKeys,
    ) -> Result<Self, CommitError> {
        let dir = database_dir.as_ref().to_path_buf();
        let writer_lease = Self::acquire_writer_lease(cx, &dir)?;
        let capsule_dir = dir.join(CAPSULE_DIR);
        cx.with_restriction_async(async {
            match vfs.create_dir(&capsule_dir).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !vfs
                        .symlink_metadata(&capsule_dir)
                        .await?
                        .file_type()
                        .is_dir()
                    {
                        return Err(CommitError::from(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "capsule path exists but is not a directory",
                        )));
                    }
                    Ok(())
                }
                Err(error) => Err(error.into()),
            }
        })
        .await?;
        let (chain, discarded_tail_bytes) = Self::recover_chain(cx, &vfs, &dir).await?;
        Ok(Self {
            vfs,
            dir,
            _writer_lease: writer_lease,
            keys,
            chain,
            discarded_tail_bytes,
            poisoned: false,
            // Re-sync once after every open. This both closes a newly created
            // directory/log entry and safely repairs the uncertainty left by
            // an earlier process that failed between inode and parent sync.
            capsule_directory_parent_sync_pending: true,
            commit_log_parent_sync_pending: true,
        })
    }

    /// Mint the sole live-writer authority before recovery can inspect or
    /// truncate the log. Opening an independent descriptor is load-bearing:
    /// duplicated handles can share lock ownership on some platforms, whereas
    /// independent opens contend both within one process and across processes.
    fn acquire_writer_lease(cx: &CommitCx, dir: &Path) -> Result<std::fs::File, CommitError> {
        cx.with_restriction(|| {
            let path = dir.join(COORDINATOR_LOCK_NAME);
            let lease = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !std::fs::symlink_metadata(&path)?.file_type().is_file() {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "commit coordinator lock path exists but is not a regular file",
                        )
                        .into());
                    }
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&path)?
                }
                Err(error) => return Err(error.into()),
            };
            match lease.try_lock() {
                Ok(()) => Ok(lease),
                Err(TryLockError::WouldBlock) => Err(CommitError::WriterAlreadyOpen),
                Err(TryLockError::Error(error)) => Err(error.into()),
            }
        })
    }

    pub fn chain(&self) -> &MarkerChain {
        &self.chain
    }

    /// The directory whose durable stream this coordinator owns.
    ///
    /// Recovery/verification layers use its canonical identity to bind
    /// independently materialized reference views to one database. They must not
    /// reconstruct this path from capsule or log filenames, which are private
    /// storage details.
    pub fn database_dir(&self) -> &Path {
        &self.dir
    }

    /// Bytes of partial entry this open discarded as a torn tail.
    ///
    /// Reported rather than swallowed: discarding a tail is correct, but it is
    /// also the signature of a crash mid-D2, and an operator investigating a
    /// missing write is entitled to see that recovery dropped something.
    pub fn discarded_tail_bytes(&self) -> usize {
        self.discarded_tail_bytes
    }

    /// Has an interrupted commit left this coordinator unable to speak for the
    /// durable log?
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    pub fn next_commit_seq(&self) -> Result<CommitSeq, ChainError> {
        self.chain.next_commit_seq()
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(COMMIT_LOG_NAME)
    }

    fn capsule_path(dir: &Path, capsule_oid: ObjectId) -> PathBuf {
        let mut name = String::with_capacity(64);
        for byte in capsule_oid.0 {
            name.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
            name.push(char::from_digit(u32::from(byte & 0xf), 16).expect("nibble"));
        }
        dir.join(CAPSULE_DIR).join(format!("{name}.capsule"))
    }

    /// Compare a freshly opened existing capsule with the bytes deterministic
    /// sealing produced for the same object identity.
    ///
    /// This is deliberately streaming: a capsule is already materialized once
    /// for publication, and deduplication must not require another
    /// capsule-sized allocation. A length or byte mismatch is a conflict;
    /// genuine read failures remain I/O errors.
    async fn existing_capsule_matches(
        cx: &impl StorageReadCx,
        file: &mut V::File,
        expected: &[u8],
    ) -> Result<bool, CommitError> {
        cx.with_restriction_async(async {
            let expected_len = u64::try_from(expected.len()).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "capsule container length does not fit u64",
                )
            })?;
            if file.metadata().await?.len() != expected_len {
                return Ok(false);
            }

            let mut actual = [0u8; CAPSULE_COMPARE_BUFFER_BYTES];
            for expected_chunk in expected.chunks(actual.len()) {
                match file.read_exact(&mut actual[..expected_chunk.len()]).await {
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                        return Ok(false);
                    }
                    Err(error) => return Err(error.into()),
                }
                // ubs:ignore -- durable encrypted container bytes, not secret material.
                if actual[..expected_chunk.len()] != expected_chunk[..] {
                    return Ok(false);
                }
            }

            // Defend the exact comparison against a file that grew after the
            // metadata read. Same-directory writer exclusion is a separate
            // protocol concern, but accepting bytes we did not compare would still
            // be wrong here.
            let mut trailing = [0u8; 1];
            Ok(file.read(&mut trailing).await? == 0)
        })
        .await
    }

    /// Read a durable capsule's bytes back.
    ///
    /// Recovery needs this and nothing else could provide it: the capsule file
    /// name is derived from the object id by a private rule, so a caller that
    /// had to rebuild the path would be duplicating that rule and would drift
    /// from it the first time it changed.
    ///
    /// The bytes are returned UNINTERPRETED. This layer knows a capsule is
    /// content addressed and durable; what the bytes *mean* belongs to whoever
    /// wrote them, and the marker already carries the digests that let a reader
    /// prove it got the object it asked for.
    pub async fn read_capsule(
        &self,
        cx: &impl StorageReadCx,
        capsule_oid: ObjectId,
    ) -> Result<Vec<u8>, CommitError> {
        cx.with_restriction_async(async {
            let container = self
                .vfs
                .read(&Self::capsule_path(&self.dir, capsule_oid))
                .await?;
            let (descriptor, symbols) = decode_container(&container)?;
            Ok(self.keys.recover(&descriptor, &symbols, capsule_oid)?)
        })
        .await
    }

    /// The identity `plaintext` will have as a capsule under this database's
    /// keys, without sealing it.
    pub fn capsule_id(&self, plaintext: &[u8]) -> ObjectId {
        self.keys.identify(plaintext)
    }

    pub fn keys(&self) -> &CapsuleKeys {
        &self.keys
    }

    /// Is this capsule durable? Used by recovery to identify orphans — bytes
    /// written by a commit that never reached D2.
    pub async fn capsule_exists(&self, cx: &impl StorageReadCx, capsule_oid: ObjectId) -> bool {
        cx.with_restriction_async(async {
            self.vfs
                .metadata(&Self::capsule_path(&self.dir, capsule_oid))
                .await
                .is_ok()
        })
        .await
    }

    /// The file component of a durability barrier, named for the same reason
    /// `RootStore`'s is: it is the step a benchmark is most tempted to drop,
    /// and doctrine 7 forbids reporting a non-durable mode as a result. D1 and
    /// D2 compose this inode sync with every directory sync their creation
    /// operations owe.
    ///
    /// It goes through the capability context because that boundary is where a
    /// lab runtime attaches fsync lies, latency, and crash injection — the two
    /// barriers are exactly the instants a durability test needs to control.
    async fn barrier(cx: &CommitCx, file: &V::File) -> Result<(), CommitError> {
        sync_file(cx, file).await?;
        Ok(())
    }

    /// Tear `bytes` off the end of the commit log, modelling a crash that left
    /// an un-fsynced entry only partly written.
    ///
    /// Test-facing, and it exists because the honest crash model for
    /// `AfterMarkerBeforeD2` is that those bytes MAY be lost: a suite that only
    /// exercised "the tail happened to survive" would never reach the torn-tail
    /// rule it claims to test. Takes a path rather than `&mut self` so a test
    /// can tear the log with no coordinator alive — which is what a crash
    /// actually looks like from the file's side.
    pub fn tear_log_tail_for_test(
        database_dir: impl AsRef<Path>,
        bytes: u64,
    ) -> Result<(), CommitError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(database_dir.as_ref().join(COMMIT_LOG_NAME))?;
        let len = file.metadata()?.len();
        file.set_len(len.saturating_sub(bytes))?;
        Ok(())
    }

    /// Commit: build → D1 → marker → D2.
    ///
    /// `marker_for` receives the allocated `commit_seq` and returns the marker
    /// to append. It is a callback because the sequence is allocated HERE, by
    /// the single actor — a caller that chose its own sequence could not be
    /// gap-free.
    pub async fn commit(
        &mut self,
        cx: &CommitCx,
        plaintext: &[u8],
        marker_for: impl FnOnce(u64, ObjectId) -> CommitMarker,
    ) -> Result<MarkerRef, CommitError> {
        self.commit_with_crash(cx, plaintext, marker_for, None)
            .await
    }

    /// Commit, optionally stopping at a crash point. The crash path is the
    /// same code as the durable path up to the stopping instant, which is the
    /// only way a crash test says anything about the real protocol.
    pub async fn commit_with_crash(
        &mut self,
        cx: &CommitCx,
        plaintext: &[u8],
        marker_for: impl FnOnce(u64, ObjectId) -> CommitMarker,
        crash_at: Option<CrashPoint>,
    ) -> Result<MarkerRef, CommitError> {
        if self.poisoned {
            return Err(CommitError::Poisoned);
        }
        let commit_seq = self.next_commit_seq()?.0;
        if crash_at == Some(CrashPoint::BeforeCapsule) {
            return Err(CommitError::Io(std::io::Error::other(
                "crash: before capsule",
            )));
        }

        // ---- Validate the marker and its framing BEFORE writing the capsule.
        // Writer/reader symmetry is a durability law: acknowledging a marker
        // that recovery must reject is data loss on the next restart. The
        // capsule reference is part of the marker's authenticated transcript,
        // so it must name the exact object this call is about.
        // ---- Seal the capsule. The identity is DERIVED from the bytes, never
        // accepted from the caller, so it is impossible to name one object and
        // store another. `marker_for` is handed the derived id precisely so it
        // can name the right one.
        let sealed = self.keys.seal(plaintext)?;
        let capsule_oid = sealed.object_id;

        let marker = marker_for(commit_seq, capsule_oid);
        let marker_capsule_ref = match &marker.effect_source {
            EffectSource::Local { capsule_ref, .. } => *capsule_ref,
        };
        // ubs:ignore -- authenticated public object identity, not secret material.
        if marker_capsule_ref != capsule_oid {
            return Err(CommitError::CapsuleRefMismatch {
                capsule_oid,
                marker_capsule_ref,
            });
        }
        let chained = self.chain.validate(&marker)?;
        let chain_hash = chained.chain_hash;
        let entry = Self::encode_entry(&marker, chain_hash)?;
        let marker_ref = MarkerRef {
            marker_oid: chained.marker_oid,
            commit_seq: CommitSeq(chained.marker.commit_seq),
        };

        // ---- Build + D1: the capsule becomes durable BEFORE any marker can
        // name it. A marker pointing at non-durable bytes would be a commit
        // pointing at bytes that may not exist.
        let capsule_path = Self::capsule_path(&self.dir, capsule_oid);
        let encoded_capsule = encode_container(&sealed);
        let capsule_file = match self
            .vfs
            .open(
                &capsule_path,
                &OpenOptions::new().read(true).write(true).create_new(true),
            )
            .await
        {
            Ok(mut file) => {
                file.write_all(&encoded_capsule).await?;
                file.flush().await?;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = cx
                    .with_restriction_async(self.vfs.symlink_metadata(&capsule_path))
                    .await?;
                if !metadata.file_type().is_file() {
                    return Err(CommitError::CapsulePathConflict { capsule_oid });
                }
                let mut file = cx
                    .with_restriction_async(self.vfs.open_read(&capsule_path))
                    .await?;
                if !Self::existing_capsule_matches(cx, &mut file, &encoded_capsule).await? {
                    return Err(CommitError::CapsulePathConflict { capsule_oid });
                }
                file
            }
            Err(error) => return Err(error.into()),
        };
        if crash_at == Some(CrashPoint::AfterCapsuleBeforeD1) {
            return Err(CommitError::Io(std::io::Error::other("crash: before D1")));
        }
        let capsule_dir = self.dir.join(CAPSULE_DIR);
        sync_created_entry(cx, &self.vfs, &capsule_file, &capsule_dir, || {
            if crash_at == Some(CrashPoint::AfterCapsuleFileSyncBeforeDirectorySync) {
                return Err(std::io::Error::other(
                    "crash: capsule inode durable before directory entry",
                ));
            }
            Ok(())
        })
        .await?;
        if self.capsule_directory_parent_sync_pending {
            if crash_at == Some(CrashPoint::AfterCapsuleDirectorySyncBeforeParentDirectorySync) {
                return Err(CommitError::Io(std::io::Error::other(
                    "crash: capsule directory durable before database directory entry",
                )));
            }
            sync_directory(cx, &self.vfs, &self.dir).await?;
            self.capsule_directory_parent_sync_pending = false;
        }
        if crash_at == Some(CrashPoint::AfterD1) {
            return Err(CommitError::Io(std::io::Error::other("crash: after D1")));
        }

        // ---- The marker. Every structural and framing law was checked before
        // D1, so a log entry that exists is an entry recovery can decode.
        let log_path = self.log_path();
        let (mut log, log_created) = match self
            .vfs
            .open(&log_path, &OpenOptions::new().append(true).create_new(true))
            .await
        {
            Ok(file) => (file, true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
                self.vfs
                    .open(&log_path, &OpenOptions::new().append(true))
                    .await?,
                false,
            ),
            Err(error) => return Err(error.into()),
        };

        // Past this point the durable log MAY contain this entry, so this
        // coordinator can no longer know the truth by looking at itself. It is
        // poisoned until someone reopens the directory and reads what is
        // actually there. Without this, a caller that ignored the error and
        // committed again would re-issue `commit_seq` and append a duplicate —
        // turning a survivable crash into a log that fails recovery outright.
        self.poisoned = true;
        log.write_all(&entry).await?;
        log.flush().await?;
        if crash_at == Some(CrashPoint::AfterMarkerBeforeD2) {
            return Err(CommitError::Io(std::io::Error::other("crash: before D2")));
        }
        if log_created || self.commit_log_parent_sync_pending {
            sync_created_entry(cx, &self.vfs, &log, &self.dir, || {
                if crash_at == Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync) {
                    return Err(std::io::Error::other(
                        "crash: marker-log inode durable before directory entry",
                    ));
                }
                Ok(())
            })
            .await?;
            self.commit_log_parent_sync_pending = false;
        } else {
            Self::barrier(cx, &log).await?;
            if crash_at == Some(CrashPoint::AfterMarkerFileSyncBeforeDirectorySync) {
                return Err(CommitError::Io(std::io::Error::other(
                    "crash: marker-log inode durable",
                )));
            }
        }

        // Only now is the commit real, so only now does in-memory state move.
        self.chain.adopt(chained)?;
        self.poisoned = false;
        Ok(marker_ref)
    }

    /// One log entry: version magic, length, canonical marker bytes, chain
    /// hash, duplicated length, trailer magic.
    ///
    /// The trailer makes a complete frame self-delimiting without trusting the
    /// leading length. The chain hash is stored rather than recomputed on the
    /// fly so recovery can detect content tampering as well as framing damage.
    fn encode_entry(marker: &CommitMarker, chain_hash: Digest) -> Result<Vec<u8>, CommitError> {
        let body = marker.canonical_bytes()?;
        if body.len() > MAX_ENTRY_BODY {
            return Err(CommitError::MarkerTooLarge {
                body_len: body.len(),
                max_body_len: MAX_ENTRY_BODY,
            });
        }
        let body_len = u32::try_from(body.len()).map_err(|_| CommitError::MarkerTooLarge {
            body_len: body.len(),
            max_body_len: MAX_ENTRY_BODY,
        })?;
        let mut entry = Vec::with_capacity(
            ENTRY_HEADER_BYTES + body.len() + CHAIN_HASH_BYTES + ENTRY_TRAILER_BYTES,
        );
        entry.extend_from_slice(&ENTRY_MAGIC);
        entry.extend_from_slice(&body_len.to_be_bytes());
        entry.extend_from_slice(&body);
        entry.extend_from_slice(&chain_hash.0);
        entry.extend_from_slice(&body_len.to_be_bytes());
        entry.extend_from_slice(&ENTRY_TRAILER_MAGIC);
        Ok(entry)
    }

    /// Recover the chain from the durable log.
    ///
    /// THE TORN-TAIL RULE lives here, and it turns on one distinction:
    /// **missing bytes versus wrong bytes.** The file ending mid-entry is a
    /// crash during D2 — a commit that never completed — so the partial entry
    /// is truncated and recovery succeeds. Bytes that are *present but not a
    /// valid entry* are damage to something that was already durable, and that
    /// fails closed.
    ///
    /// The distinction is what keeps the rule safe for entries that are NOT
    /// last. A damaged middle entry is followed by more bytes, so it can never
    /// present as "missing bytes" — it fails the magic, the body decode, or the
    /// length bound, all of which are corruption. Treating any malformed entry
    /// as a tail would let one bad entry silently delete every commit after it
    /// while recovery reported success, which is the worst outcome available to
    /// a commit log: durable data lost with a green light.
    async fn recover_chain(
        cx: &CommitCx,
        vfs: &V,
        dir: &Path,
    ) -> Result<(MarkerChain, usize), CommitError> {
        cx.with_restriction_async(async {
            let path = dir.join(COMMIT_LOG_NAME);
            let mut file = match vfs.open_read(&path).await {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok((MarkerChain::new(), 0));
                }
                Err(error) => return Err(error.into()),
            };

            let mut chain = MarkerChain::new();
            let mut cursor = 0usize;
            // One bounded entry buffer replaces whole-log materialization. A valid
            // writer entry is at most MAX_ENTRY_BODY plus its fixed framing, so a
            // hostile sparse or multi-gigabyte log can never demand a matching
            // allocation merely because it exists on disk.
            let mut entry = Vec::with_capacity(
                ENTRY_HEADER_BYTES + MAX_ENTRY_BODY + CHAIN_HASH_BYTES + ENTRY_TRAILER_BYTES,
            );
            while let Some(decoded) = Self::read_next_entry(&mut file, &mut entry).await? {
                let (marker, stored_chain_hash, consumed) = match decoded {
                    Ok(decoded) => decoded,
                    // Reachable only when the remaining bytes run out, which by
                    // construction means end of file.
                    Err(EntryDefect::Truncated) => break,
                    Err(EntryDefect::Corrupt) => {
                        return Err(CommitError::CorruptLogEntry {
                            commit_seq: chain.next_commit_seq()?.0,
                        });
                    }
                };
                let expected = marker.chain_hash(chain.chain_value()).map_err(|_| {
                    CommitError::CorruptLogEntry {
                        commit_seq: marker.commit_seq,
                    }
                })?;
                if expected != stored_chain_hash {
                    return Err(CommitError::ChainDiverged {
                        commit_seq: marker.commit_seq,
                    });
                }
                let seq = marker.commit_seq;
                chain
                    .append(marker)
                    .map_err(|_| CommitError::CorruptLogEntry { commit_seq: seq })?;
                cursor = cursor.checked_add(consumed).ok_or_else(|| {
                    CommitError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "commit log length exceeds this platform's address space",
                    ))
                })?;
            }
            let valid_len = u64::try_from(cursor).map_err(|_| {
                CommitError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "commit log length does not fit u64",
                ))
            })?;
            let file_len = file.metadata().await?.len();
            let discarded_tail_bytes =
                usize::try_from(file_len.checked_sub(valid_len).ok_or_else(|| {
                    CommitError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "commit log shrank during recovery",
                    ))
                })?)
                .map_err(|_| {
                    CommitError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "discarded commit-log tail does not fit usize",
                    ))
                })?;
            if discarded_tail_bytes != 0 {
                // O_APPEND uses the file's physical end, not the recovered
                // chain's logical end. Leaving the torn bytes in place would put
                // the next acknowledged entry after an undecodable prefix; the
                // following restart would stop at that old prefix and silently
                // lose the acknowledged commit. The next successful D2 sync makes
                // this truncation durable together with the appended entry.
                vfs.open(&path, &OpenOptions::new().write(true))
                    .await?
                    .set_len(valid_len)
                    .await?;
            }
            Ok((chain, discarded_tail_bytes))
        })
        .await
    }

    /// Read and decode exactly one bounded entry.
    ///
    /// `None` is a clean entry-boundary EOF. A partial header/body is handed to
    /// `decode_entry`, which preserves the same missing-versus-wrong-byte rule
    /// as the in-memory decoder without ever reading beyond one maximum entry.
    async fn read_next_entry<R: AsyncRead + Unpin>(
        reader: &mut R,
        entry: &mut Vec<u8>,
    ) -> std::io::Result<EntryRead> {
        entry.clear();
        if !Self::read_until_len(reader, entry, ENTRY_HEADER_BYTES).await? {
            return if entry.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Self::decode_entry(entry)))
            };
        }

        // Refuse impossible framing from the fixed header before asking the
        // reader for any claimed body bytes. This is what makes an invalid
        // prefix in front of an enormous file cheap and fail-closed.
        if entry[..ENTRY_MAGIC.len()] != ENTRY_MAGIC {
            return Ok(Some(Err(EntryDefect::Corrupt)));
        }
        let body_len = u32::from_be_bytes([entry[4], entry[5], entry[6], entry[7]]) as usize;
        if body_len > MAX_ENTRY_BODY {
            return Ok(Some(Err(EntryDefect::Corrupt)));
        }
        let total = ENTRY_HEADER_BYTES + body_len + CHAIN_HASH_BYTES + ENTRY_TRAILER_BYTES;
        let _complete = Self::read_until_len(reader, entry, total).await?;
        Ok(Some(Self::decode_entry(entry)))
    }

    /// Extend `bytes` to `target_len`, returning false only when EOF arrives
    /// first. Reads use a fixed stack chunk so even a corrupt length field never
    /// controls a single allocation or read request.
    async fn read_until_len<R: AsyncRead + Unpin>(
        reader: &mut R,
        bytes: &mut Vec<u8>,
        target_len: usize,
    ) -> std::io::Result<bool> {
        let mut chunk = [0u8; 8 * 1024];
        while bytes.len() < target_len {
            let wanted = (target_len - bytes.len()).min(chunk.len());
            match reader.read(&mut chunk[..wanted]).await {
                Ok(0) => return Ok(false),
                Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(true)
    }

    /// Decode one entry, or say which way it failed.
    fn decode_entry(bytes: &[u8]) -> Result<(CommitMarker, Digest, usize), EntryDefect> {
        if bytes.len() >= ENTRY_MAGIC.len() && bytes[..ENTRY_MAGIC.len()] != ENTRY_MAGIC {
            return Err(EntryDefect::Corrupt);
        }
        if bytes.len() < ENTRY_HEADER_BYTES {
            return Err(EntryDefect::Truncated);
        }
        let body_len = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if body_len > MAX_ENTRY_BODY {
            return Err(EntryDefect::Corrupt);
        }
        let total = ENTRY_HEADER_BYTES + body_len + CHAIN_HASH_BYTES + ENTRY_TRAILER_BYTES;
        if bytes.len() < total {
            // A complete marker prefix followed by the full fixed suffix means
            // the physical frame is present under its intrinsic body extent.
            // The leading length is therefore damaged; silently calling this a
            // torn tail would discard an acknowledged final commit.
            if Self::has_complete_intrinsic_entry_extent(bytes) {
                return Err(EntryDefect::Corrupt);
            }
            return Err(EntryDefect::Truncated);
        }
        let body_end = ENTRY_HEADER_BYTES + body_len;
        let marker = crate::marker::decode_canonical(&bytes[ENTRY_HEADER_BYTES..body_end])
            .ok_or(EntryDefect::Corrupt)?;
        let chain_end = body_end + CHAIN_HASH_BYTES;
        let mut chain_hash = [0u8; 32];
        chain_hash.copy_from_slice(&bytes[body_end..chain_end]);
        let duplicated_body_len = u32::from_be_bytes([
            bytes[chain_end],
            bytes[chain_end + 1],
            bytes[chain_end + 2],
            bytes[chain_end + 3],
        ]) as usize;
        if duplicated_body_len != body_len || bytes[chain_end + 4..total] != ENTRY_TRAILER_MAGIC {
            return Err(EntryDefect::Corrupt);
        }
        Ok((marker, Digest(chain_hash), total))
    }

    /// Does `bytes` contain a whole version-3 entry under the marker's
    /// self-described canonical extent, independently of the leading length?
    fn has_complete_intrinsic_entry_extent(bytes: &[u8]) -> bool {
        let Some(marker_and_suffix) = bytes.get(ENTRY_HEADER_BYTES..) else {
            return false;
        };
        let Some((_, intrinsic_body_len)) =
            crate::marker::decode_canonical_prefix(marker_and_suffix)
        else {
            return false;
        };
        if intrinsic_body_len > MAX_ENTRY_BODY {
            return false;
        }
        let intrinsic_total =
            ENTRY_HEADER_BYTES + intrinsic_body_len + CHAIN_HASH_BYTES + ENTRY_TRAILER_BYTES;
        bytes.len() >= intrinsic_total
    }

    /// Capsules on disk that no committed marker names.
    ///
    /// These are the residue of commits that crashed between D1 and D2. They
    /// are NOT partial commits — they are bytes nobody referenced — and
    /// reclaiming them is ordinary maintenance rather than recovery.
    pub async fn orphan_capsules(
        &self,
        cx: &impl StorageReadCx,
    ) -> Result<Vec<ObjectId>, CommitError> {
        let referenced: Vec<ObjectId> = self
            .chain
            .entries()
            .iter()
            // Exhaustive by design: when the `Global` arm lands it carries no
            // local capsule, and this match stops compiling until someone
            // decides what that means for orphan reclamation. Silently
            // skipping an unknown arm would report live capsules as orphans.
            .map(|entry| match &entry.marker.effect_source {
                crate::marker::EffectSource::Local { capsule_ref, .. } => *capsule_ref,
            })
            .collect();

        cx.with_restriction_async(async {
            let mut orphans = Vec::new();
            let capsule_dir = self.dir.join(CAPSULE_DIR);
            let mut entries = self.vfs.read_dir(&capsule_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let name = entry.file_name();
                let Some(stem) = name
                    .to_string_lossy()
                    .strip_suffix(".capsule")
                    .map(str::to_owned)
                else {
                    continue;
                };
                let Some(oid) = decode_hex_oid(&stem) else {
                    continue;
                };
                if !referenced.contains(&oid) {
                    orphans.push(oid);
                }
            }
            orphans.sort_by_key(|oid| oid.0);
            Ok(orphans)
        })
        .await
    }
}

fn decode_hex_oid(hex: &str) -> Option<ObjectId> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(ObjectId(out))
}

#[cfg(test)]
mod tests {
    use super::{CommitCoordinator, ENTRY_HEADER_BYTES, EntryDefect, UnixVfs};
    use asupersync::io::{AsyncRead, ReadBuf};
    use std::future::Future;
    use std::io;
    use std::pin::{Pin, pin};
    use std::task::{Context, Poll, Waker};

    /// A hostile source with an invalid fixed header and an arbitrarily large
    /// suffix. Touching the suffix panics, so this is a direct negative control
    /// for the old whole-log `read_to_end` behavior.
    struct InvalidHeaderWithForbiddenSuffix {
        header_delivered: bool,
    }

    impl AsyncRead for InvalidHeaderWithForbiddenSuffix {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _task: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            assert!(
                !self.header_delivered,
                "recovery read past a corrupt header"
            );
            assert_eq!(buf.remaining(), ENTRY_HEADER_BYTES);
            buf.put_slice(b"BAD!\0\0\0\0");
            self.header_delivered = true;
            Poll::Ready(Ok(()))
        }
    }

    /// Drive a future that never actually suspends: the hostile source always
    /// returns `Ready`, so a `Pending` would mean the reader, not the source,
    /// blocked.
    fn poll_ready<F: Future>(future: F) -> F::Output {
        let waker = Waker::noop();
        let mut task = Context::from_waker(waker);
        match pin!(future).poll(&mut task) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("read future suspended over a ready-only source"),
        }
    }

    #[test]
    fn corrupt_header_is_rejected_before_its_unbounded_suffix_is_read() {
        let mut source = InvalidHeaderWithForbiddenSuffix {
            header_delivered: false,
        };
        let mut entry = Vec::new();
        let decoded = poll_ready(CommitCoordinator::<UnixVfs>::read_next_entry(
            &mut source,
            &mut entry,
        ))
        .expect("the source itself is readable")
        .expect("a header was present");
        assert!(matches!(decoded, Err(EntryDefect::Corrupt)));
        assert_eq!(entry.len(), ENTRY_HEADER_BYTES);
    }
}
