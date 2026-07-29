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
use crate::marker::{ChainError, CommitMarker, EffectSource, MarkerChain, MarkerRef};
use fgdb_crypto::Digest;
use fgdb_types::context::CommitCx;
use fgdb_types::ids::ObjectId;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Sub-directory holding capsule bytes.
pub const CAPSULE_DIR: &str = "capsules";

/// The append-only marker log.
pub const COMMIT_LOG_NAME: &str = "commits.log";

/// Stack buffer used to compare an existing immutable capsule with the
/// deterministic candidate without allocating a second capsule-sized `Vec`.
const CAPSULE_COMPARE_BUFFER_BYTES: usize = 8 * 1024;

/// Version-2 per-entry framing magic.
///
/// Version 2 adds an end trailer carrying the body length again. Recovery
/// deliberately rejects the earlier `FGCM` shape: accepting two shapes without
/// a migration contract would make damaged framing ambiguous.
pub const ENTRY_MAGIC: [u8; 4] = *b"FGC2";

/// End sentinel for a complete version-2 entry.
const ENTRY_TRAILER_MAGIC: [u8; 4] = *b"FGE2";

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
    /// D1 complete: the capsule is durable, but no marker exists yet.
    AfterD1,
    /// Marker bytes appended, D2 barrier NOT reached.
    AfterMarkerBeforeD2,
}

/// The single-actor commit coordinator.
///
/// One actor allocates every `CommitSeq`, which is what makes the sequence
/// gap-free without coordination: there is no second allocator to race. The
/// type owns the chain, so a caller cannot hold two coordinators over one log.
#[derive(Debug)]
pub struct CommitCoordinator {
    dir: PathBuf,
    keys: CapsuleKeys,
    chain: MarkerChain,
    discarded_tail_bytes: usize,
    poisoned: bool,
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

impl CommitCoordinator {
    /// Open a database directory's commit stream, recovering whatever is
    /// durable.
    pub fn open(database_dir: impl AsRef<Path>, keys: CapsuleKeys) -> Result<Self, CommitError> {
        let dir = database_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(dir.join(CAPSULE_DIR))?;
        let (chain, discarded_tail_bytes) = Self::recover_chain(&dir)?;
        Ok(Self {
            dir,
            keys,
            chain,
            discarded_tail_bytes,
            poisoned: false,
        })
    }

    pub fn chain(&self) -> &MarkerChain {
        &self.chain
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

    pub fn next_commit_seq(&self) -> u64 {
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
    fn existing_capsule_matches(file: &mut File, expected: &[u8]) -> Result<bool, CommitError> {
        let expected_len = u64::try_from(expected.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "capsule container length does not fit u64",
            )
        })?;
        if file.metadata()?.len() != expected_len {
            return Ok(false);
        }

        let mut actual = [0u8; CAPSULE_COMPARE_BUFFER_BYTES];
        for expected_chunk in expected.chunks(actual.len()) {
            match file.read_exact(&mut actual[..expected_chunk.len()]) {
                Ok(()) => {}
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
        Ok(file.read(&mut trailing)? == 0)
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
    pub fn read_capsule(&self, capsule_oid: ObjectId) -> Result<Vec<u8>, CommitError> {
        let mut container = Vec::new();
        File::open(Self::capsule_path(&self.dir, capsule_oid))?.read_to_end(&mut container)?;
        let (descriptor, symbols) = decode_container(&container)?;
        Ok(self.keys.recover(&descriptor, &symbols, capsule_oid)?)
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
    pub fn capsule_exists(&self, capsule_oid: ObjectId) -> bool {
        Self::capsule_path(&self.dir, capsule_oid).exists()
    }

    /// The durability barrier, named for the same reason `RootStore`'s is: it
    /// is the step a benchmark is most tempted to drop, and doctrine 7 forbids
    /// reporting a non-durable mode as a result. Both D1 and D2 are this call.
    ///
    /// It goes through the capability context because that boundary is where a
    /// lab runtime attaches fsync lies, latency, and crash injection — the two
    /// barriers are exactly the instants a durability test needs to control.
    fn barrier(cx: &CommitCx, file: &File) -> Result<(), CommitError> {
        cx.with_restriction(|| file.sync_all())?;
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
        let file = OpenOptions::new()
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
    pub fn commit(
        &mut self,
        cx: &CommitCx,
        plaintext: &[u8],
        marker_for: impl FnOnce(u64, ObjectId) -> CommitMarker,
    ) -> Result<MarkerRef, CommitError> {
        self.commit_with_crash(cx, plaintext, marker_for, None)
    }

    /// Commit, optionally stopping at a crash point. The crash path is the
    /// same code as the durable path up to the stopping instant, which is the
    /// only way a crash test says anything about the real protocol.
    pub fn commit_with_crash(
        &mut self,
        cx: &CommitCx,
        plaintext: &[u8],
        marker_for: impl FnOnce(u64, ObjectId) -> CommitMarker,
        crash_at: Option<CrashPoint>,
    ) -> Result<MarkerRef, CommitError> {
        if self.poisoned {
            return Err(CommitError::Poisoned);
        }
        let commit_seq = self.next_commit_seq();
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
            commit_seq: chained.marker.commit_seq,
        };

        // ---- Build + D1: the capsule becomes durable BEFORE any marker can
        // name it. A marker pointing at non-durable bytes would be a commit
        // pointing at bytes that may not exist.
        let capsule_path = Self::capsule_path(&self.dir, capsule_oid);
        let encoded_capsule = encode_container(&sealed);
        let capsule_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&capsule_path)
        {
            Ok(mut file) => {
                file.write_all(&encoded_capsule)?;
                file
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&capsule_path)?;
                if !metadata.file_type().is_file() {
                    return Err(CommitError::CapsulePathConflict { capsule_oid });
                }
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&capsule_path)?;
                if !Self::existing_capsule_matches(&mut file, &encoded_capsule)? {
                    return Err(CommitError::CapsulePathConflict { capsule_oid });
                }
                file
            }
            Err(error) => return Err(error.into()),
        };
        if crash_at == Some(CrashPoint::AfterCapsuleBeforeD1) {
            return Err(CommitError::Io(std::io::Error::other("crash: before D1")));
        }
        Self::barrier(cx, &capsule_file)?; // D1
        if crash_at == Some(CrashPoint::AfterD1) {
            return Err(CommitError::Io(std::io::Error::other("crash: after D1")));
        }

        // ---- The marker. Every structural and framing law was checked before
        // D1, so a log entry that exists is an entry recovery can decode.
        let mut log = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.log_path())?;

        // Past this point the durable log MAY contain this entry, so this
        // coordinator can no longer know the truth by looking at itself. It is
        // poisoned until someone reopens the directory and reads what is
        // actually there. Without this, a caller that ignored the error and
        // committed again would re-issue `commit_seq` and append a duplicate —
        // turning a survivable crash into a log that fails recovery outright.
        self.poisoned = true;
        log.write_all(&entry)?;
        if crash_at == Some(CrashPoint::AfterMarkerBeforeD2) {
            return Err(CommitError::Io(std::io::Error::other("crash: before D2")));
        }
        Self::barrier(cx, &log)?; // D2 — the commit point.

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
        let body = marker.canonical_bytes();
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
    fn recover_chain(dir: &Path) -> Result<(MarkerChain, usize), CommitError> {
        let path = dir.join(COMMIT_LOG_NAME);
        let mut bytes = Vec::new();
        match File::open(&path) {
            Ok(mut file) => {
                file.read_to_end(&mut bytes)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((MarkerChain::new(), 0));
            }
            Err(error) => return Err(error.into()),
        }

        let mut chain = MarkerChain::new();
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            let (marker, stored_chain_hash, consumed) = match Self::decode_entry(&bytes[cursor..]) {
                Ok(decoded) => decoded,
                // Reachable only when the remaining bytes run out, which by
                // construction means end of file.
                Err(EntryDefect::Truncated) => break,
                Err(EntryDefect::Corrupt) => {
                    return Err(CommitError::CorruptLogEntry {
                        commit_seq: chain.next_commit_seq(),
                    });
                }
            };
            let expected = marker.chain_hash(chain.chain_value());
            if expected != stored_chain_hash {
                return Err(CommitError::ChainDiverged {
                    commit_seq: marker.commit_seq,
                });
            }
            let seq = marker.commit_seq;
            chain
                .append(marker)
                .map_err(|_| CommitError::CorruptLogEntry { commit_seq: seq })?;
            cursor += consumed;
        }
        let discarded_tail_bytes = bytes.len() - cursor;
        if discarded_tail_bytes != 0 {
            // O_APPEND uses the file's physical end, not the recovered
            // chain's logical end. Leaving the torn bytes in place would put
            // the next acknowledged entry after an undecodable prefix; the
            // following restart would stop at that old prefix and silently
            // lose the acknowledged commit. The next successful D2 sync makes
            // this truncation durable together with the appended entry.
            let valid_len = u64::try_from(cursor).map_err(|_| {
                CommitError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "commit log length does not fit u64",
                ))
            })?;
            OpenOptions::new()
                .write(true)
                .open(&path)?
                .set_len(valid_len)?;
        }
        Ok((chain, discarded_tail_bytes))
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

    /// Does `bytes` contain a whole version-2 entry under the marker's
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
    pub fn orphan_capsules(&self) -> Result<Vec<ObjectId>, CommitError> {
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

        let mut orphans = Vec::new();
        let capsule_dir = self.dir.join(CAPSULE_DIR);
        for entry in std::fs::read_dir(&capsule_dir)? {
            let entry = entry?;
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
