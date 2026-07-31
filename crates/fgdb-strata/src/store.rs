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
//! **A WRITE IS FSYNCED BEFORE IT IS CONSIDERED DONE**, through `&CommitCx` as
//! doctrine 3 requires — the capability context is what a lab runtime swaps to
//! inject fsync lies and torn writes at exactly this boundary. Chronicle's root
//! store established that shape and this follows it rather than inventing a second
//! one.
//!
//! **AN EXISTING BLOCK IS NOT REWRITTEN.** Blocks are immutable and
//! content-addressed, so a second write of the same identity is either the same
//! bytes (nothing to do) or a collision (a refusal). Truncating and rewriting
//! would take a durable object that is currently readable and make it briefly
//! absent, to replace it with what it already contained — the hazard
//! `fgdb-capsule-no-overwrite-pysi` names for capsules, avoided here for the same
//! reason.
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

/// Why a block could not be stored or loaded.
#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
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

/// A directory of content-addressed Strata blocks.
#[derive(Debug, Clone)]
pub struct BlockStore {
    dir: PathBuf,
    k_oid: [u8; 32],
    namespace: DatabaseSecurityNamespaceId,
}

impl BlockStore {
    pub fn open(
        database_dir: impl AsRef<Path>,
        k_oid: [u8; 32],
        namespace: DatabaseSecurityNamespaceId,
    ) -> Result<Self, StoreError> {
        let dir = database_dir.as_ref().join(BLOCK_DIR);
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
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
    /// is a no-op; one holding different bytes is a refusal.
    pub fn put(&self, cx: &CommitCx, bytes: &[u8]) -> Result<ObjectId, StoreError> {
        let id = block_id(&self.k_oid, self.namespace, bytes);
        let path = self.path(id);

        if path.exists() {
            let mut existing = Vec::new();
            File::open(&path)?.read_to_end(&mut existing)?;
            if existing == bytes {
                return Ok(id);
            }
            return Err(StoreError::Collision { block_id: id });
        }

        // create_new: two writers racing on one identity both succeed only if one
        // of them loses the create and then finds its own bytes already there —
        // which is the no-op path above, because the bytes are equal by
        // construction.
        let mut file = match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing = Vec::new();
                File::open(&path)?.read_to_end(&mut existing)?;
                if existing == bytes {
                    return Ok(id);
                }
                return Err(StoreError::Collision { block_id: id });
            }
            Err(error) => return Err(error.into()),
        };
        file.write_all(bytes)?;
        // The capability context is where a lab runtime attaches fsync lies and
        // torn writes; doctrine 3 requires it and this is the boundary it exists
        // for.
        cx.with_restriction(|| file.sync_all())?;
        Ok(id)
    }

    /// Load and decode the block named by `id`.
    ///
    /// **THE IDENTITY IS RE-DERIVED FROM THE BYTES**, not assumed from the path. A
    /// store that trusted its own layout would return whatever happened to sit at
    /// the expected path — the exact failure content-addressing exists to prevent,
    /// and the one that is silent.
    pub fn get(&self, id: ObjectId) -> Result<Vec<crate::AdjacencyEntry>, StoreError> {
        let mut bytes = Vec::new();
        File::open(self.path(id))?.read_to_end(&mut bytes)?;

        let actual = block_id(&self.k_oid, self.namespace, &bytes);
        if actual != id {
            return Err(StoreError::IdentityMismatch {
                expected: id,
                actual,
            });
        }
        decode_block(&bytes).map_err(StoreError::Malformed)
    }

    /// Load the raw bytes of a block, verifying identity but not decoding.
    ///
    /// For a caller that needs the bytes themselves — sealing into a capsule,
    /// copying to a replica — and must not pay to decode them.
    pub fn get_bytes(&self, id: ObjectId) -> Result<Vec<u8>, StoreError> {
        let mut bytes = Vec::new();
        File::open(self.path(id))?.read_to_end(&mut bytes)?;
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
