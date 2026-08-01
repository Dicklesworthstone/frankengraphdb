//! The block store: where a partition stops needing the whole stream.
//!
//! Until now a Strata partition could only be produced by folding the entire
//! commit history — correct, and not a storage tier. Blocks persisted here are
//! read back by identity, so a partition root can be resolved without replaying
//! anything.
//!
//! **CONTENT-ADDRESSED, SO THE PATH IS NOT THE NAME.** A block's filename is
//! derived from its identity, and a read RE-DERIVES that identity from the bytes
//! it found and refuses a mismatch. The path is therefore a hint about where to
//! look, never evidence about what was found: a store that trusted its own layout
//! would return whatever sat at the expected path, which is exactly the failure a
//! content-addressed store exists to prevent (doctrine 5).
//!
//! **A WRITE IS NOT DONE UNTIL BOTH THE INODE AND ITS NAME ARE DURABLE.** The
//! block file is synced first, then the `strata-blocks` directory; opening the
//! store likewise syncs that directory before its entry in the database
//! directory. Every step runs through `&CommitCx` as doctrine 3 requires — the
//! capability context is what a lab runtime swaps to inject fsync lies and torn
//! writes at exactly these boundaries.
//!
//! **AN EXISTING BLOCK IS NOT REWRITTEN.** Blocks are immutable and
//! content-addressed, so a second write of the same identity is either the same
//! bytes (no content rewrite), damage whose bytes derive a different identity, or
//! a true identity collision (both refusals). Truncating and rewriting would take
//! a durable object that is currently readable and make it briefly absent, to
//! replace it with what it already contained — the hazard
//! `fgdb-capsule-no-overwrite-pysi` names for capsules, avoided here for the same
//! reason.
//!
//! **A PARTIAL FILE IS NEVER CANONICAL.** Publication is serialized by a
//! process-death-released filesystem lease represented inside this module by a
//! non-clone permit. The permit owner writes and syncs a staging inode first,
//! then moves that complete inode to the content-addressed path and syncs the
//! directory. An identical loser can therefore observe only absence or the
//! winner's complete bytes — never the winner halfway through `write_all`.
//!
//! **READS USE THE SHARED STORAGE AUTHORITY CONTRACT.** Every synchronous read
//! accepts `&impl StorageReadCx` and performs its filesystem work under that
//! role's restriction. The sealed workspace contract admits query, transaction,
//! commit, maintenance, and replication contexts while leaving deterministic
//! merge evaluation unable to express storage I/O (doctrine 3).
//!
//! **WHAT IS DELIBERATELY ABSENT.** Blocks are stored as their canonical bytes,
//! NOT sealed into capsules. `strata_blocks_are_durable_objects.rs` proves a block
//! survives the whole §5.1 pipeline including erasure recovery, so that composition
//! is established; wiring it in here would duplicate Chronicle's capsule store
//! rather than reuse it, and which store owns Strata's objects is a placement
//! question this slice has no business answering. What is here is the smallest
//! honest thing: bytes on disk, addressed by identity, verified on read.

use crate::{BlockError, DeltaBlockVersion, PartitionRootVersion, block_id, decode_block};
use fgdb_types::context::CommitCx;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
use fgdb_types::{CommitSeq, StorageReadCx};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Directory holding a database's Strata blocks.
pub const BLOCK_DIR: &str = "strata-blocks";

/// One durable, empty inode whose process-scoped lock mints the non-clone block
/// publication permit. It is not transaction authority: it serializes only the
/// instant at which a complete immutable inode gains its canonical name.
const PUBLICATION_LOCK_FILE: &str = ".block-publication.lock";

/// The sole noncanonical inode name used while holding the publication permit.
/// A crash may leave it incomplete; the next permit owner rewrites it before it
/// can become canonical.
const PUBLICATION_STAGING_FILE: &str = ".block-publication.staging";

/// The largest persisted Strata object this store will materialize.
///
/// A block entry occupies 56 bytes, so 64 bytes per admitted entry leaves room
/// for the block header and is also comfortably above the smaller root-format
/// maximum. The bound follows the format's cardinality ceiling instead of being
/// an unrelated process-local allocation policy.
const MAX_STORED_OBJECT_BYTES: u64 = (crate::MAX_BLOCK_ENTRIES as u64) * 64;

/// Creation-only crash instants that distinguish inode durability from
/// namespace durability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStoreCrashPoint {
    /// The `strata-blocks` directory inode is durable, but the database
    /// directory has not yet made that name durable.
    AfterStoreDirectorySyncBeforeDatabaseDirectorySync,
    /// The block inode is durable, but `strata-blocks` has not yet made the
    /// block's content-addressed name durable.
    AfterBlockFileSyncBeforeStoreDirectorySync,
    /// The staging inode is complete and durable, but it has not yet acquired
    /// the content-addressed canonical name.
    AfterStagingFileSyncBeforePublication,
}

/// Make every directory entry currently visible in `directory` durable.
fn sync_directory(cx: &CommitCx, directory: &Path) -> std::io::Result<()> {
    let directory = cx.with_restriction(|| File::open(directory))?;
    cx.with_restriction(|| directory.sync_all())
}

/// Whether an existing noncanonical staging name is the inode's only link.
/// Rewriting a multiply linked staging inode could silently rewrite a canonical
/// block reached through another name, violating immutability. Platforms that
/// cannot report the link count fail closed instead of guessing.
fn staging_inode_is_exclusive(metadata: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 1
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.number_of_links() == Some(1)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        false
    }
}

/// Run the ordered durability work for a new or previously uncertain file
/// entry. The hook lets crash tests stop between inode and namespace durability
/// without maintaining a second, weaker write path.
fn sync_file_and_directory(
    cx: &CommitCx,
    file: &File,
    parent_directory: &Path,
    after_file_sync: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    run_ordered_creation_barrier(
        || cx.with_restriction(|| file.sync_all()),
        after_file_sync,
        || sync_directory(cx, parent_directory),
    )
}

fn run_ordered_creation_barrier(
    sync_created_inode: impl FnOnce() -> std::io::Result<()>,
    after_inode_sync: impl FnOnce() -> std::io::Result<()>,
    sync_parent_directory: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    sync_created_inode()?;
    after_inode_sync()?;
    sync_parent_directory()
}

/// Exclusive authority to publish one complete staging inode.
///
/// The file descriptor owns a process-death-released whole-inode lock. This
/// type is deliberately neither public nor cloneable, so the code that moves a
/// staging name to a canonical name cannot run without visibly acquiring the
/// publication authority first. Dropping the descriptor releases the lock on
/// every return path, including an injected crash error.
#[derive(Debug)]
struct ObjectPublicationPermit {
    _locked_file: File,
}

/// Selects the authoritative identity transcript for one immutable store object.
///
/// Block and root derivations intentionally produce the same bits today. Keeping
/// the choice explicit on both write and read prevents that present equivalence
/// from becoming a hidden compatibility dependency if either transcript changes.
#[derive(Clone, Copy, Debug)]
enum StoredObjectKind {
    Block,
    Root,
}

impl StoredObjectKind {
    fn identity(
        self,
        k_oid: &[u8; 32],
        namespace: DatabaseSecurityNamespaceId,
        bytes: &[u8],
    ) -> ObjectId {
        match self {
            Self::Block => block_id(k_oid, namespace, bytes),
            Self::Root => crate::root::root_id(k_oid, namespace, bytes),
        }
    }
}

/// Why an immutable Strata object could not be stored or loaded.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    /// A persisted object exceeds the finite byte ceiling this store can read.
    ObjectTooLarge {
        limit: u64,
        observed: u64,
    },
    /// The bytes at the block's path are not the block that path names.
    ///
    /// Damage, or a store that was written by something that did not derive the
    /// path from the content. Either way the bytes are not what was asked for and
    /// returning them would be worse than failing.
    IdentityMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// An existing canonical path contains bytes whose own identity names a
    /// different object.
    ///
    /// This is damage or a nonconforming writer, not a hash collision: a true
    /// collision requires two different byte strings that both derive the same
    /// identity. The existing inode is preserved for diagnosis.
    DamagedExisting {
        expected: ObjectId,
        actual: ObjectId,
    },
    /// Two different byte strings derived the same identity.
    ///
    /// Under a keyed 256-bit identity this is not expected to occur. Bytes at the
    /// path that derive a *different* identity are [`StoreError::DamagedExisting`]
    /// instead, so ordinary corruption is never mislabeled as cryptographic
    /// evidence.
    Collision {
        object_id: ObjectId,
    },
    /// The stored bytes are not a lawful block.
    Malformed(BlockError),
    /// The stored bytes are not a lawful partition root, or a block it names
    /// disagreed with what the root claimed about it.
    ///
    /// Separate from `Malformed`: a caller reopening a partition needs to know
    /// whether the ROOT is wrong or one of the BLOCKS is, because those are
    /// different objects to go and look at.
    MalformedRoot(crate::root::RootError),
    /// Loading one of a root's named blocks failed at the storage boundary.
    ///
    /// Kept distinct from `MalformedRoot`: an absent inode, an oversized object,
    /// and a false block identity are storage diagnoses, not evidence that the
    /// authenticated root's own byte layout is malformed.
    RootBlockLoad {
        at: usize,
        error: Box<StoreError>,
    },
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "block store io: {error}"),
            Self::ObjectTooLarge { limit, observed } => write!(
                f,
                "stored object has at least {observed} bytes, above the {limit}-byte limit"
            ),
            Self::IdentityMismatch { expected, actual } => write!(
                f,
                "the bytes stored for {expected:?} are actually {actual:?}"
            ),
            Self::DamagedExisting { expected, actual } => write!(
                f,
                "the existing path for {expected:?} contains object {actual:?}"
            ),
            Self::Collision { object_id } => {
                write!(f, "{object_id:?} already exists with different bytes")
            }
            Self::Malformed(error) => write!(f, "stored block is malformed: {error}"),
            Self::MalformedRoot(error) => write!(f, "stored root is malformed: {error}"),
            Self::RootBlockLoad { at, error } => {
                write!(f, "loading root block {at}: {error}")
            }
        }
    }
}

impl core::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Malformed(error) => Some(error),
            Self::MalformedRoot(error) => Some(error),
            Self::RootBlockLoad { error, .. } => Some(error.as_ref()),
            Self::ObjectTooLarge { .. }
            | Self::IdentityMismatch { .. }
            | Self::DamagedExisting { .. }
            | Self::Collision { .. } => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn ensure_size_within_limit(observed: u64, limit: u64) -> Result<(), StoreError> {
    if observed > limit {
        return Err(StoreError::ObjectTooLarge { limit, observed });
    }
    Ok(())
}

fn read_bounded(
    cx: &impl StorageReadCx,
    file: &mut File,
    limit: u64,
) -> Result<Vec<u8>, StoreError> {
    cx.with_restriction(|| {
        ensure_size_within_limit(file.metadata()?.len(), limit)?;

        let mut bytes = Vec::new();
        {
            // Metadata is only an early refusal, not authority: the inode could grow
            // after it was read. One extra byte distinguishes the exact ceiling from
            // an over-limit stream without ever materializing the unbounded tail.
            let mut bounded = file.take(limit.saturating_add(1));
            bounded.read_to_end(&mut bytes)?;
        }
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        ensure_size_within_limit(observed, limit)?;
        Ok(bytes)
    })
}

/// A directory of content-addressed Strata blocks.
#[derive(Debug, Clone)]
pub struct BlockStore {
    dir: PathBuf,
    publication_lock_path: PathBuf,
    k_oid: [u8; 32],
    namespace: DatabaseSecurityNamespaceId,
}

/// Proof that one authenticated root was checked against every block it names.
///
/// The token borrows the exact store that performed admission. Its fields are
/// private and it is not cloneable, so range-based skipping cannot be requested
/// with a caller-constructed [`crate::root::PartitionRoot`]. A fresh process must
/// pay the full admission read once; later snapshots may act on the proven range
/// summaries without materializing blocks that begin after the snapshot.
#[derive(Debug)]
pub struct AdmittedPartitionRoot<'store> {
    store: &'store BlockStore,
    root_id: PartitionRootVersion,
    root: crate::root::PartitionRoot,
}

impl AdmittedPartitionRoot<'_> {
    /// The content identity of the admitted root.
    pub const fn root_id(&self) -> PartitionRootVersion {
        self.root_id
    }

    /// The authenticated root whose block ranges were admitted.
    pub const fn root(&self) -> &crate::root::PartitionRoot {
        &self.root
    }

    /// Load the blocks that can contribute to `as_of`.
    pub fn resolve_blocks_at(
        &self,
        cx: &impl StorageReadCx,
        as_of: CommitSeq,
    ) -> Result<Vec<Vec<crate::AdjacencyEntry>>, StoreError> {
        self.store
            .resolve_admitted_root_blocks_at(cx, &self.root, as_of)
    }
}

impl BlockStore {
    pub fn open(
        cx: &CommitCx,
        database_dir: impl AsRef<Path>,
        k_oid: [u8; 32],
        namespace: DatabaseSecurityNamespaceId,
    ) -> Result<Self, StoreError> {
        Self::open_with_crash(cx, database_dir, k_oid, namespace, None)
    }

    /// Open the block store, optionally stopping between durability of the
    /// store directory and durability of its name in the database directory.
    /// The normal path delegates here so the crash matrix exercises the exact
    /// production ordering.
    #[doc(hidden)]
    pub fn open_with_crash(
        cx: &CommitCx,
        database_dir: impl AsRef<Path>,
        k_oid: [u8; 32],
        namespace: DatabaseSecurityNamespaceId,
        crash_at: Option<BlockStoreCrashPoint>,
    ) -> Result<Self, StoreError> {
        let database_dir = database_dir.as_ref().to_path_buf();
        let dir = database_dir.join(BLOCK_DIR);
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !cx
                    .with_restriction(|| std::fs::symlink_metadata(&dir))?
                    .file_type()
                    .is_dir()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "strata block path exists but is not a directory",
                    )
                    .into());
                }
            }
            Err(error) => return Err(error.into()),
        }

        let publication_lock_path = dir.join(PUBLICATION_LOCK_FILE);
        let publication_lock = match cx.with_restriction(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&publication_lock_path)
        }) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !cx
                    .with_restriction(|| std::fs::symlink_metadata(&publication_lock_path))?
                    .file_type()
                    .is_file()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "object publication lock path exists but is not a regular file",
                    )
                    .into());
                }
                cx.with_restriction(|| {
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&publication_lock_path)
                })?
            }
            Err(error) => return Err(error.into()),
        };

        // The lock inode belongs to the store-directory creation closure. Sync
        // it before the child directory, then the child before its name in the
        // database parent. A successful open therefore never relies on a
        // volatile lock-file dirent for writer exclusion after restart.
        cx.with_restriction(|| publication_lock.sync_all())?;

        // Re-sync on every open. Besides closing a newly created directory,
        // this repairs the uncertainty left by an earlier process that failed
        // between the child-directory and database-directory barriers.
        run_ordered_creation_barrier(
            || sync_directory(cx, &dir),
            || {
                if crash_at
                    == Some(
                        BlockStoreCrashPoint::AfterStoreDirectorySyncBeforeDatabaseDirectorySync,
                    )
                {
                    return Err(std::io::Error::other(
                        "crash: strata-blocks durable before database directory entry",
                    ));
                }
                Ok(())
            },
            || sync_directory(cx, &database_dir),
        )?;

        Ok(Self {
            dir,
            publication_lock_path,
            k_oid,
            namespace,
        })
    }

    /// Where a block of this identity lives.
    ///
    /// Hex rather than any shorter encoding, because a filename is read by humans
    /// during recovery and a base-N alphabet that varies by platform or locale is
    /// a bad thing to have in a durable layout.
    pub fn path(&self, block_id: ObjectId) -> PathBuf {
        let mut name = String::with_capacity(64);
        for byte in block_id.0 {
            name.push(char::from_digit(u32::from(byte >> 4), 16).expect("nibble"));
            name.push(char::from_digit(u32::from(byte & 0xf), 16).expect("nibble"));
        }
        self.dir.join(format!("{name}.block"))
    }

    /// Store `bytes`, returning the identity they were stored under.
    ///
    /// The identity is DERIVED from the bytes, never accepted, so a caller cannot
    /// name one block and store another. An existing file holding the same bytes
    /// is not rewritten, but its durability is re-established before success;
    /// one holding different bytes is a refusal.
    pub fn put(&self, cx: &CommitCx, bytes: &[u8]) -> Result<DeltaBlockVersion, StoreError> {
        self.put_with_crash(cx, bytes, None)
    }

    /// Acquire the sole block-publication authority through the capability
    /// context. Opening a fresh descriptor per acquisition matters: whole-file
    /// locks on duplicated descriptors may be re-entrant within one process,
    /// whereas two independent opens contend just like two processes do.
    fn acquire_publication_permit(
        &self,
        cx: &CommitCx,
    ) -> Result<ObjectPublicationPermit, StoreError> {
        let locked_file = cx.with_restriction(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.publication_lock_path)
        })?;
        cx.with_restriction(|| File::lock(&locked_file))?;
        Ok(ObjectPublicationPermit {
            _locked_file: locked_file,
        })
    }

    /// Store bytes while optionally stopping between block-inode and block-name
    /// durability. The normal path delegates here so crash tests cannot drift
    /// from production ordering.
    #[doc(hidden)]
    pub fn put_with_crash(
        &self,
        cx: &CommitCx,
        bytes: &[u8],
        crash_at: Option<BlockStoreCrashPoint>,
    ) -> Result<DeltaBlockVersion, StoreError> {
        self.put_with_steps(cx, bytes, crash_at, || {}, || {})
    }

    /// The exact production path with two deterministic observation points.
    /// Unit tests use them to order a loser before the winner publishes without
    /// sleeps; ordinary and crash-test callers both supply no-op hooks.
    fn put_with_steps(
        &self,
        cx: &CommitCx,
        bytes: &[u8],
        crash_at: Option<BlockStoreCrashPoint>,
        before_lock: impl FnOnce(),
        after_staging_sync: impl FnOnce(),
    ) -> Result<DeltaBlockVersion, StoreError> {
        self.put_object_with_steps(
            StoredObjectKind::Block,
            cx,
            bytes,
            crash_at,
            before_lock,
            after_staging_sync,
        )
        .map(DeltaBlockVersion)
    }

    fn put_object_with_steps(
        &self,
        kind: StoredObjectKind,
        cx: &CommitCx,
        bytes: &[u8],
        crash_at: Option<BlockStoreCrashPoint>,
        before_lock: impl FnOnce(),
        after_staging_sync: impl FnOnce(),
    ) -> Result<ObjectId, StoreError> {
        let offered_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        ensure_size_within_limit(offered_len, MAX_STORED_OBJECT_BYTES)?;

        let id = kind.identity(&self.k_oid, self.namespace, bytes);
        let path = self.path(id);

        before_lock();
        let _permit = self.acquire_publication_permit(cx)?;

        // The canonical path is inspected only while publication authority is
        // held, so a conforming writer can see either no winner or one complete
        // winner. Equal bytes are never rewritten, but they are re-synced after
        // reopen because visibility alone is not a durability receipt.
        match cx.with_restriction(|| std::fs::symlink_metadata(&path)) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "canonical object path exists but is not a regular file",
                    )
                    .into());
                }
                let mut file =
                    cx.with_restriction(|| OpenOptions::new().read(true).write(true).open(&path))?;
                let existing = read_bounded(cx, &mut file, MAX_STORED_OBJECT_BYTES)?;
                let actual = kind.identity(&self.k_oid, self.namespace, &existing);
                if actual != id {
                    return Err(StoreError::DamagedExisting {
                        expected: id,
                        actual,
                    });
                }
                if existing != bytes {
                    return Err(StoreError::Collision { object_id: id });
                }
                sync_file_and_directory(cx, &file, &self.dir, || {
                    if crash_at
                        == Some(BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync)
                    {
                        return Err(std::io::Error::other(
                            "crash: strata block inode durable before directory entry",
                        ));
                    }
                    Ok(())
                })?;
                return Ok(id);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        // Only a permit owner may touch the one staging name. It is explicitly
        // noncanonical, so an interrupted prior owner may leave partial bytes
        // here and the next owner may safely rewrite them. The canonical name
        // remains absent until this inode is complete and synced.
        let staging_path = self.dir.join(PUBLICATION_STAGING_FILE);
        let mut staging = match cx.with_restriction(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&staging_path)
        }) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = cx.with_restriction(|| std::fs::symlink_metadata(&staging_path))?;
                if !metadata.file_type().is_file() || !staging_inode_is_exclusive(&metadata) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "object staging path is not an exclusive regular-file inode",
                    )
                    .into());
                }
                cx.with_restriction(|| {
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .truncate(true)
                        .open(&staging_path)
                })?
            }
            Err(error) => return Err(error.into()),
        };

        staging.write_all(bytes)?;
        cx.with_restriction(|| staging.sync_all())?;
        after_staging_sync();
        if crash_at == Some(BlockStoreCrashPoint::AfterStagingFileSyncBeforePublication) {
            return Err(StoreError::Io(std::io::Error::other(
                "crash: complete staging inode before canonical publication",
            )));
        }

        // Close before rename for platforms that refuse to move an open file.
        // Publication is atomic for conforming writers because the non-clone
        // permit spans the absence check, staging write, and move.
        drop(staging);
        std::fs::rename(&staging_path, &path)?;

        let published =
            cx.with_restriction(|| OpenOptions::new().read(true).write(true).open(&path))?;
        sync_file_and_directory(cx, &published, &self.dir, || {
            if crash_at == Some(BlockStoreCrashPoint::AfterBlockFileSyncBeforeStoreDirectorySync) {
                return Err(std::io::Error::other(
                    "crash: strata block inode durable before directory entry",
                ));
            }
            Ok(())
        })?;
        Ok(id)
    }

    /// Load and decode the block named by `id`.
    ///
    /// **THE IDENTITY IS RE-DERIVED FROM THE BYTES**, not assumed from the path. A
    /// store that trusted its own layout would return whatever happened to sit at
    /// the expected path — the exact failure content-addressing exists to prevent,
    /// and the one that is silent.
    pub fn get(
        &self,
        cx: &impl StorageReadCx,
        id: DeltaBlockVersion,
    ) -> Result<Vec<crate::AdjacencyEntry>, StoreError> {
        let bytes = self.get_bytes(cx, id)?;
        decode_block(&bytes).map_err(StoreError::Malformed)
    }

    fn read_object_bytes(
        &self,
        cx: &impl StorageReadCx,
        id: ObjectId,
        limit: u64,
    ) -> Result<Vec<u8>, StoreError> {
        let mut file = cx.with_restriction(|| File::open(self.path(id)))?;
        read_bounded(cx, &mut file, limit)
    }

    /// Load the raw bytes of a block, verifying identity but not decoding.
    ///
    /// For a caller that needs the bytes themselves — sealing into a capsule,
    /// copying to a replica — and must not pay to decode them.
    pub fn get_bytes(
        &self,
        cx: &impl StorageReadCx,
        id: DeltaBlockVersion,
    ) -> Result<Vec<u8>, StoreError> {
        let bytes = self.read_object_bytes(cx, id.0, MAX_STORED_OBJECT_BYTES)?;
        let actual = block_id(&self.k_oid, self.namespace, &bytes);
        if actual != id.0 {
            return Err(StoreError::IdentityMismatch {
                expected: id.0,
                actual,
            });
        }
        Ok(bytes)
    }

    fn resolve_root_block(
        &self,
        cx: &impl StorageReadCx,
        at: usize,
        reference: &crate::root::BlockRef,
    ) -> Result<Vec<crate::AdjacencyEntry>, StoreError> {
        let bytes = self
            .get_bytes(cx, DeltaBlockVersion(reference.block_id))
            .map_err(|error| StoreError::RootBlockLoad {
                at,
                error: Box::new(error),
            })?;
        crate::root::resolve_block_ref(&self.k_oid, self.namespace, at, reference, &bytes)
            .map_err(StoreError::MalformedRoot)
    }

    /// Prove every block named by an already-structural root while retaining only
    /// the caller-selected decoded blocks.
    ///
    /// Every encoded block is dropped before loading the next. One canonical
    /// statement per EId remains in the incremental history validator so a future
    /// block cannot reuse an identity and still mint a selective-read token. This
    /// is the fresh admission path: even a block outside the requested snapshot is
    /// checked, because neither an unproved range nor an unproved identity is
    /// permission to skip that block.
    fn inspect_root_blocks(
        &self,
        cx: &impl StorageReadCx,
        root: &crate::root::PartitionRoot,
        mut retain: impl FnMut(usize, &crate::root::BlockRef) -> bool,
    ) -> Result<Vec<Vec<crate::AdjacencyEntry>>, StoreError> {
        crate::root::validate_root(root).map_err(StoreError::MalformedRoot)?;

        let mut blocks = Vec::new();
        let mut history = crate::root::EdgeHistoryValidator::default();
        for (at, reference) in root.blocks.iter().enumerate() {
            let entries = self.resolve_root_block(cx, at, reference)?;
            history
                .observe_block(at, &entries)
                .map_err(StoreError::MalformedRoot)?;
            if retain(at, reference) {
                blocks.push(entries);
            }
        }
        Ok(blocks)
    }

    /// Load and validate every block named by an already-structural root.
    fn resolve_root_blocks(
        &self,
        cx: &impl StorageReadCx,
        root: &crate::root::PartitionRoot,
    ) -> Result<Vec<Vec<crate::AdjacencyEntry>>, StoreError> {
        self.inspect_root_blocks(cx, root, |_, _| true)
    }

    /// Resolve only blocks selected by an already-minted admission token.
    ///
    /// `selected` comes from the shared root skip rule and is walked in lockstep
    /// with publication order. Unlike fresh admission, skipped blocks are not
    /// opened: their range summaries were proved when the token was minted.
    fn resolve_admitted_root_blocks_at(
        &self,
        cx: &impl StorageReadCx,
        root: &crate::root::PartitionRoot,
        as_of: CommitSeq,
    ) -> Result<Vec<Vec<crate::AdjacencyEntry>>, StoreError> {
        crate::root::validate_root(root).map_err(StoreError::MalformedRoot)?;
        let selected = crate::root::blocks_visible_at(root, as_of);
        let mut selected = selected.into_iter().peekable();
        let mut blocks = Vec::with_capacity(selected.len());
        for (at, reference) in root.blocks.iter().enumerate() {
            if selected.peek() != Some(&at) {
                continue;
            }
            selected.next();
            blocks.push(self.resolve_root_block(cx, at, reference)?);
        }
        debug_assert!(selected.next().is_none());
        Ok(blocks)
    }

    /// Store a partition root, returning the identity it was stored under.
    ///
    /// **A ROOT IS AN OBJECT LIKE ANY OTHER**, which is what makes reopening a
    /// partition possible at all: the root is content-addressed and immutable, so
    /// publishing a new one never mutates the old, and a reader that holds a root
    /// identity can prove the bytes it found are that root. `manifest.root` remains
    /// the only mutable object in the database (doctrine 5) — what would live there
    /// is a POINTER to the current root's identity, and choosing where that pointer
    /// lives is Chronicle's question rather than this store's.
    ///
    /// The root is admitted against every named block before publication. That
    /// binds each authenticated range to the actual block bytes, so a future
    /// range-skipping reader cannot inherit a lie minted by a privileged caller.
    /// Publication then delegates to the same immutable-object path as blocks.
    pub fn put_root(
        &self,
        cx: &CommitCx,
        root: &crate::root::PartitionRoot,
    ) -> Result<PartitionRootVersion, StoreError> {
        let bytes = crate::root::encode_root(root).map_err(StoreError::MalformedRoot)?;
        self.inspect_root_blocks(cx, root, |_, _| false)?;
        self.put_object_with_steps(StoredObjectKind::Root, cx, &bytes, None, || {}, || {})
            .map(PartitionRootVersion)
    }

    /// Load the partition root named by `id`, using the root format's exact byte
    /// ceiling and root-specific identity verifier before structural decoding.
    pub fn get_root(
        &self,
        cx: &impl StorageReadCx,
        id: PartitionRootVersion,
    ) -> Result<crate::root::PartitionRoot, StoreError> {
        let bytes = self.read_object_bytes(cx, id.0, crate::root::MAX_ENCODED_ROOT_BYTES as u64)?;
        crate::root::read_root(&self.k_oid, self.namespace, &bytes, id.0)
            .map_err(StoreError::MalformedRoot)
    }

    /// Reopen a whole partition: the root, and every block it names.
    ///
    /// **THIS IS THE PAYOFF OF EVERYTHING ABOVE.** No commit stream is replayed and
    /// no writer runs: a root identity, a directory, and the two checks that make a
    /// content-addressed store trustworthy — the bytes are the object asked for, and
    /// each block spans what the root claimed about it.
    pub fn reopen(
        &self,
        cx: &impl StorageReadCx,
        id: PartitionRootVersion,
    ) -> Result<(crate::root::PartitionRoot, Vec<Vec<crate::AdjacencyEntry>>), StoreError> {
        let root = self.get_root(cx, id)?;
        let blocks = self.resolve_root_blocks(cx, &root)?;
        Ok((root, blocks))
    }

    /// Admit a stored root against every named block without retaining the
    /// decoded partition.
    pub fn admit_root(
        &self,
        cx: &impl StorageReadCx,
        id: PartitionRootVersion,
    ) -> Result<AdmittedPartitionRoot<'_>, StoreError> {
        let root = self.get_root(cx, id)?;
        self.inspect_root_blocks(cx, &root, |_, _| false)?;
        Ok(AdmittedPartitionRoot {
            store: self,
            root_id: id,
            root,
        })
    }

    /// Reopen a partition for one snapshot while minting a reusable admission
    /// token for later selective reads.
    ///
    /// A fresh root cannot skip I/O on the strength of its own unproved ranges,
    /// so this first call reads and verifies every named block. It retains only
    /// blocks that can affect `as_of`; the returned token makes later calls to
    /// [`AdmittedPartitionRoot::resolve_blocks_at`] genuinely selective.
    pub fn reopen_at(
        &self,
        cx: &impl StorageReadCx,
        id: PartitionRootVersion,
        as_of: CommitSeq,
    ) -> Result<(AdmittedPartitionRoot<'_>, Vec<Vec<crate::AdjacencyEntry>>), StoreError> {
        let root = self.get_root(cx, id)?;
        let visible = crate::root::blocks_visible_at(&root, as_of);
        let mut visible = visible.into_iter().peekable();
        let blocks = self.inspect_root_blocks(cx, &root, |at, _| {
            if visible.peek() != Some(&at) {
                return false;
            }
            visible.next();
            true
        })?;
        debug_assert!(visible.next().is_none());
        let admitted = AdmittedPartitionRoot {
            store: self,
            root_id: id,
            root,
        };
        Ok((admitted, blocks))
    }

    /// Does this store hold the block named by `id`?
    ///
    /// **IT VERIFIES, IT DOES NOT JUST STAT.** A path check would answer "yes" for
    /// a file this store cannot actually serve — most visibly when two stores under
    /// different keys share a directory, where one key's block sits at a path the
    /// other would never resolve. A `contains` that can disagree with `get` is a
    /// trap: every caller that guards a `get` with it would be told the block is
    /// there and then handed a refusal.
    ///
    /// Found by a law about two keys sharing a directory, which failed against the
    /// stat-only version. Reading the file to answer is the cost of the answer
    /// being true, and this crate is never optimized (§15).
    pub fn contains(&self, cx: &impl StorageReadCx, id: DeltaBlockVersion) -> bool {
        self.get_bytes(cx, id).is_ok()
    }
}

#[cfg(test)]
mod durability_tests {
    use super::{
        BlockStore, MAX_STORED_OBJECT_BYTES, PUBLICATION_STAGING_FILE, StoreError, read_bounded,
        run_ordered_creation_barrier,
    };
    use crate::DeltaBlockVersion;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::context::{CommitCx, PurposeContexts};
    use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
    use std::cell::RefCell;
    use std::fs::File;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, mpsc::sync_channel};
    use std::thread::JoinHandle;

    const K_OID: [u8; 32] = [0xa5; 32];
    const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x17; 32]);

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fgdb-block-publication-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn under_lab<T: Send + 'static>(
        seed: u64,
        test: impl FnOnce(&CommitCx) -> T + Send + 'static,
    ) -> T {
        let (output, report) = run_async_under_lab(seed, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            test(&contexts.commit())
        });
        assert!(
            report.invariant_violations.is_empty(),
            "lab invariant violation: {report:?}"
        );
        output
    }

    #[test]
    fn persisted_object_reads_refuse_the_limit_plus_one_byte() {
        let dir = scratch_dir("bounded-read");
        let path = dir.join("object");
        std::fs::write(&path, [1, 2, 3]).expect("fixture");
        let mut file = File::open(path).expect("open fixture");

        assert!(matches!(
            under_lab(49, move |cx| read_bounded(cx, &mut file, 2)),
            Err(StoreError::ObjectTooLarge {
                limit: 2,
                observed: 3
            })
        ));
    }

    #[test]
    fn oversized_canonical_inode_is_refused_before_identity_materialization() {
        let dir = scratch_dir("oversized-canonical-inode");
        under_lab(48, move |cx| {
            let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
            let id = ObjectId([0x48; 32]);
            let file = File::create(store.path(id)).expect("create sparse fixture");
            file.set_len(MAX_STORED_OBJECT_BYTES + 1)
                .expect("extend sparse fixture");

            assert!(matches!(
                store.get_bytes(cx, DeltaBlockVersion(id)),
                Err(StoreError::ObjectTooLarge {
                    limit: MAX_STORED_OBJECT_BYTES,
                    observed
                }) if observed == MAX_STORED_OBJECT_BYTES + 1
            ));
        });
    }

    #[test]
    fn creation_barrier_runs_inode_hook_then_parent() {
        let order = RefCell::new(Vec::new());
        run_ordered_creation_barrier(
            || {
                order.borrow_mut().push("inode");
                Ok(())
            },
            || {
                order.borrow_mut().push("hook");
                Ok(())
            },
            || {
                order.borrow_mut().push("parent");
                Ok(())
            },
        )
        .expect("barrier");
        assert_eq!(*order.borrow(), ["inode", "hook", "parent"]);
    }

    #[test]
    fn crash_hook_prevents_parent_directory_publication() {
        let order = RefCell::new(Vec::new());
        let outcome = run_ordered_creation_barrier(
            || {
                order.borrow_mut().push("inode");
                Ok(())
            },
            || {
                order.borrow_mut().push("hook");
                Err(std::io::Error::other("crash"))
            },
            || {
                order.borrow_mut().push("parent");
                Ok(())
            },
        );
        assert!(outcome.is_err());
        assert_eq!(*order.borrow(), ["inode", "hook"]);
    }

    /// The interleaving is channel-driven, not timing-driven: the winner holds
    /// the non-clone permit with a fully synced staging inode, then the loser
    /// announces that it is immediately about to acquire the same permit. Only
    /// after that handshake may the winner publish. The loser therefore cannot
    /// read an empty or partial canonical file and both calls must return the
    /// same identity.
    #[test]
    fn an_identical_loser_observes_only_the_complete_winner() {
        type PutHandle = JoinHandle<Result<DeltaBlockVersion, StoreError>>;

        let dir = scratch_dir("identical-loser");
        let bytes = b"one complete immutable block".to_vec();
        let loser_bytes = bytes.clone();
        let loser_dir = dir.clone();
        let (loser_attempting_tx, loser_attempting_rx) = sync_channel(0);
        let loser_handle: Arc<Mutex<Option<PutHandle>>> = Arc::new(Mutex::new(None));
        let hook_handle = Arc::clone(&loser_handle);

        let winner = under_lab(45, move |cx| {
            let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("winner opens");
            store.put_with_steps(
                cx,
                &bytes,
                None,
                || {},
                move || {
                    let handle = std::thread::spawn(move || {
                        under_lab(46, move |loser_cx| {
                            let store = BlockStore::open(loser_cx, &loser_dir, K_OID, NAMESPACE)
                                .expect("loser opens");
                            store.put_with_steps(
                                loser_cx,
                                &loser_bytes,
                                None,
                                move || loser_attempting_tx.send(()).expect("attempt handshake"),
                                || {},
                            )
                        })
                    });
                    *hook_handle.lock().expect("handle slot") = Some(handle);
                    loser_attempting_rx
                        .recv()
                        .expect("loser reached the permit boundary");
                },
            )
        })
        .expect("winner publishes");

        let loser = loser_handle
            .lock()
            .expect("handle slot")
            .take()
            .expect("loser was spawned")
            .join()
            .expect("loser thread")
            .expect("identical loser succeeds");
        assert_eq!(loser, winner);
    }

    /// A staging pathname is noncanonical, but its inode still might have been
    /// hard-linked to a canonical block by a corrupt or nonconforming actor.
    /// Reusing that inode must fail closed before truncation.
    #[test]
    fn a_multiply_linked_staging_inode_cannot_rewrite_a_canonical_block() {
        let dir = scratch_dir("multiply-linked-staging");
        under_lab(47, move |cx| {
            let store = BlockStore::open(cx, &dir, K_OID, NAMESPACE).expect("opens");
            let original = b"already canonical";
            let original_id = store.put(cx, original).expect("initial publication");
            std::fs::hard_link(
                store.path(original_id.0),
                store.dir.join(PUBLICATION_STAGING_FILE),
            )
            .expect("construct multiply linked staging control");

            assert!(matches!(
                store.put(cx, b"a different block"),
                Err(StoreError::Io(error))
                    if error.kind() == std::io::ErrorKind::InvalidData
            ));
            assert_eq!(
                store.get_bytes(cx, original_id).expect("original survives"),
                original
            );
        });
    }
}
