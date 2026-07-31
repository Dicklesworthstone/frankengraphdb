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
//! bytes (no content rewrite) or a collision (a refusal). Truncating and rewriting
//! would take a durable object that is currently readable and make it briefly
//! absent, to replace it with what it already contained — the hazard
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
//! **WHAT IS DELIBERATELY ABSENT.** Blocks are stored as their canonical bytes,
//! NOT sealed into capsules. `strata_blocks_are_durable_objects.rs` proves a block
//! survives the whole §5.1 pipeline including erasure recovery, so that composition
//! is established; wiring it in here would duplicate Chronicle's capsule store
//! rather than reuse it, and which store owns Strata's objects is a placement
//! question this slice has no business answering. What is here is the smallest
//! honest thing: bytes on disk, addressed by identity, verified on read.

use crate::{BlockError, block_id, decode_block};
use fgdb_types::context::CommitCx;
use fgdb_types::ids::{DatabaseSecurityNamespaceId, ObjectId};
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
    let directory = File::open(directory)?;
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
struct BlockPublicationPermit {
    _locked_file: File,
}

/// Why a block could not be stored or loaded.
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
    /// A block with this identity already exists holding DIFFERENT bytes.
    ///
    /// Under a keyed 256-bit identity this is not a hash collision anyone will
    /// meet; it is a key/namespace mix-up or a corrupted store, and both are worse
    /// to overwrite than to refuse.
    Collision {
        block_id: ObjectId,
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
            Self::Collision { block_id } => {
                write!(f, "{block_id:?} already exists with different bytes")
            }
            Self::Malformed(error) => write!(f, "stored block is malformed: {error}"),
            Self::MalformedRoot(error) => write!(f, "stored root is malformed: {error}"),
        }
    }
}

impl core::error::Error for StoreError {}

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

fn read_bounded(file: &mut File, limit: u64) -> Result<Vec<u8>, StoreError> {
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
}

/// A directory of content-addressed Strata blocks.
#[derive(Debug, Clone)]
pub struct BlockStore {
    dir: PathBuf,
    publication_lock_path: PathBuf,
    k_oid: [u8; 32],
    namespace: DatabaseSecurityNamespaceId,
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
                if !std::fs::symlink_metadata(&dir)?.file_type().is_dir() {
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
        let publication_lock = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&publication_lock_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !std::fs::symlink_metadata(&publication_lock_path)?
                    .file_type()
                    .is_file()
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "block publication lock path exists but is not a regular file",
                    )
                    .into());
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&publication_lock_path)?
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
    pub fn put(&self, cx: &CommitCx, bytes: &[u8]) -> Result<ObjectId, StoreError> {
        self.put_with_crash(cx, bytes, None)
    }

    /// Acquire the sole block-publication authority through the capability
    /// context. Opening a fresh descriptor per acquisition matters: whole-file
    /// locks on duplicated descriptors may be re-entrant within one process,
    /// whereas two independent opens contend just like two processes do.
    fn acquire_publication_permit(
        &self,
        cx: &CommitCx,
    ) -> Result<BlockPublicationPermit, StoreError> {
        let locked_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.publication_lock_path)?;
        cx.with_restriction(|| File::lock(&locked_file))?;
        Ok(BlockPublicationPermit {
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
    ) -> Result<ObjectId, StoreError> {
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
    ) -> Result<ObjectId, StoreError> {
        let offered_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        ensure_size_within_limit(offered_len, MAX_STORED_OBJECT_BYTES)?;

        let id = block_id(&self.k_oid, self.namespace, bytes);
        let path = self.path(id);

        before_lock();
        let _permit = self.acquire_publication_permit(cx)?;

        // The canonical path is inspected only while publication authority is
        // held, so a conforming writer can see either no winner or one complete
        // winner. Equal bytes are never rewritten, but they are re-synced after
        // reopen because visibility alone is not a durability receipt.
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "canonical block path exists but is not a regular file",
                    )
                    .into());
                }
                let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
                if file.metadata()?.len() != offered_len {
                    return Err(StoreError::Collision { block_id: id });
                }
                let existing = read_bounded(&mut file, offered_len)?;
                if existing != bytes {
                    return Err(StoreError::Collision { block_id: id });
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
        let mut staging = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&staging_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&staging_path)?;
                if !metadata.file_type().is_file() || !staging_inode_is_exclusive(&metadata) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "block staging path is not an exclusive regular-file inode",
                    )
                    .into());
                }
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .truncate(true)
                    .open(&staging_path)?
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

        let published = OpenOptions::new().read(true).write(true).open(&path)?;
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
    pub fn get(&self, id: ObjectId) -> Result<Vec<crate::AdjacencyEntry>, StoreError> {
        let bytes = self.get_bytes(id)?;
        decode_block(&bytes).map_err(StoreError::Malformed)
    }

    /// Load the raw bytes of a block, verifying identity but not decoding.
    ///
    /// For a caller that needs the bytes themselves — sealing into a capsule,
    /// copying to a replica — and must not pay to decode them.
    pub fn get_bytes(&self, id: ObjectId) -> Result<Vec<u8>, StoreError> {
        let mut file = File::open(self.path(id))?;
        let bytes = read_bounded(&mut file, MAX_STORED_OBJECT_BYTES)?;
        let actual = block_id(&self.k_oid, self.namespace, &bytes);
        if actual != id {
            return Err(StoreError::IdentityMismatch {
                expected: id,
                actual,
            });
        }
        Ok(bytes)
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
    /// Deliberately the same `put`, so a root gets the identity derivation, the
    /// no-overwrite rule and the collision refusal without a second implementation
    /// that could drift from them. Only the reader differs, because only the reader
    /// knows which decoder applies.
    pub fn put_root(
        &self,
        cx: &CommitCx,
        root: &crate::root::PartitionRoot,
    ) -> Result<ObjectId, StoreError> {
        let bytes = crate::root::encode_root(root).map_err(StoreError::MalformedRoot)?;
        self.put(cx, &bytes)
    }

    /// Load the partition root named by `id`, verifying identity then lawfulness.
    pub fn get_root(&self, id: ObjectId) -> Result<crate::root::PartitionRoot, StoreError> {
        let bytes = self.get_bytes(id)?;
        crate::root::decode_root(&bytes).map_err(StoreError::MalformedRoot)
    }

    /// Reopen a whole partition: the root, and every block it names.
    ///
    /// **THIS IS THE PAYOFF OF EVERYTHING ABOVE.** No commit stream is replayed and
    /// no writer runs: a root identity, a directory, and the two checks that make a
    /// content-addressed store trustworthy — the bytes are the object asked for, and
    /// each block spans what the root claimed about it.
    pub fn reopen(
        &self,
        id: ObjectId,
    ) -> Result<(crate::root::PartitionRoot, Vec<Vec<crate::AdjacencyEntry>>), StoreError> {
        let root = self.get_root(id)?;
        let blocks = crate::root::resolve_blocks(&self.k_oid, self.namespace, &root, |wanted| {
            self.get_bytes(wanted).ok()
        })
        .map_err(StoreError::MalformedRoot)?;
        Ok((root, blocks))
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
    pub fn contains(&self, id: ObjectId) -> bool {
        self.get_bytes(id).is_ok()
    }
}

#[cfg(test)]
mod durability_tests {
    use super::{
        BlockStore, MAX_STORED_OBJECT_BYTES, PUBLICATION_STAGING_FILE, StoreError, read_bounded,
        run_ordered_creation_barrier,
    };
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
            read_bounded(&mut file, 2),
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
                store.get_bytes(id),
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
        type PutHandle = JoinHandle<Result<ObjectId, StoreError>>;

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
                store.path(original_id),
                store.dir.join(PUBLICATION_STAGING_FILE),
            )
            .expect("construct multiply linked staging control");

            assert!(matches!(
                store.put(cx, b"a different block"),
                Err(StoreError::Io(error))
                    if error.kind() == std::io::ErrorKind::InvalidData
            ));
            assert_eq!(
                store.get_bytes(original_id).expect("original survives"),
                original
            );
        });
    }
}
