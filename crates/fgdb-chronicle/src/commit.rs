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
//! the end of the log. Recovery **discards a torn tail rather than erroring**:
//! an incomplete final entry means the commit never completed, which is a
//! normal outcome of crashing, not corruption. A torn entry anywhere EARLIER
//! is corruption, because entries before it were durable.

use crate::marker::{ChainError, CommitMarker, MarkerChain, MarkerRef};
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

/// Per-entry framing magic, so a torn tail is distinguishable from a valid
/// entry that happens to start with a small length.
pub const ENTRY_MAGIC: [u8; 4] = *b"FGCM";

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
            Self::CorruptLogEntry { commit_seq } => {
                write!(f, "commit log entry at seq {commit_seq} is corrupt")
            }
            Self::ChainDiverged { commit_seq } => {
                write!(f, "commit chain diverges at seq {commit_seq}")
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
    pub fn open(database_dir: impl AsRef<Path>) -> Result<Self, CommitError> {
        let dir = database_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(dir.join(CAPSULE_DIR))?;
        let (chain, discarded_tail_bytes) = Self::recover_chain(&dir)?;
        Ok(Self {
            dir,
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
        capsule_oid: ObjectId,
        capsule_bytes: &[u8],
        marker_for: impl FnOnce(u64) -> CommitMarker,
    ) -> Result<MarkerRef, CommitError> {
        self.commit_with_crash(cx, capsule_oid, capsule_bytes, marker_for, None)
    }

    /// Commit, optionally stopping at a crash point. The crash path is the
    /// same code as the durable path up to the stopping instant, which is the
    /// only way a crash test says anything about the real protocol.
    pub fn commit_with_crash(
        &mut self,
        cx: &CommitCx,
        capsule_oid: ObjectId,
        capsule_bytes: &[u8],
        marker_for: impl FnOnce(u64) -> CommitMarker,
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

        // ---- Build + D1: the capsule becomes durable BEFORE any marker can
        // name it. A marker pointing at non-durable bytes would be a commit
        // pointing at bytes that may not exist.
        let capsule_path = Self::capsule_path(&self.dir, capsule_oid);
        let mut capsule_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&capsule_path)?;
        capsule_file.write_all(capsule_bytes)?;
        if crash_at == Some(CrashPoint::AfterCapsuleBeforeD1) {
            return Err(CommitError::Io(std::io::Error::other("crash: before D1")));
        }
        Self::barrier(cx, &capsule_file)?; // D1
        if crash_at == Some(CrashPoint::AfterD1) {
            return Err(CommitError::Io(std::io::Error::other("crash: after D1")));
        }

        // ---- The marker. Validated against every chain law BEFORE it is
        // written, so a log entry that exists is an entry that was legal.
        let marker = marker_for(commit_seq);
        let chained = self.chain.validate(&marker)?;
        let chain_hash = chained.chain_hash;
        let marker_ref = MarkerRef {
            marker_oid: chained.marker_oid,
            commit_seq: chained.marker.commit_seq,
        };

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
        log.write_all(&Self::encode_entry(&marker, chain_hash))?;
        if crash_at == Some(CrashPoint::AfterMarkerBeforeD2) {
            return Err(CommitError::Io(std::io::Error::other("crash: before D2")));
        }
        Self::barrier(cx, &log)?; // D2 — the commit point.

        // Only now is the commit real, so only now does in-memory state move.
        self.chain.adopt(chained)?;
        self.poisoned = false;
        Ok(marker_ref)
    }

    /// One log entry: magic, length, canonical marker bytes, chain hash.
    ///
    /// The chain hash is stored rather than recomputed on the fly so recovery
    /// can detect a torn entry by *disagreement* as well as by short read.
    fn encode_entry(marker: &CommitMarker, chain_hash: Digest) -> Vec<u8> {
        let body = marker.canonical_bytes();
        let mut entry = Vec::with_capacity(4 + 4 + body.len() + 32);
        entry.extend_from_slice(&ENTRY_MAGIC);
        entry.extend_from_slice(&(body.len() as u32).to_be_bytes());
        entry.extend_from_slice(&body);
        entry.extend_from_slice(&chain_hash.0);
        entry
    }

    /// Recover the chain from the durable log.
    ///
    /// THE TORN-TAIL RULE lives here, and it turns on one distinction:
    /// **missing bytes versus wrong bytes.** The file ending mid-entry is a
    /// crash during D2 — a commit that never completed — so the partial entry
    /// is discarded and recovery succeeds. Bytes that are *present but not a
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
        Ok((chain, bytes.len() - cursor))
    }

    /// Decode one entry, or say which way it failed.
    fn decode_entry(bytes: &[u8]) -> Result<(CommitMarker, Digest, usize), EntryDefect> {
        if bytes.len() >= ENTRY_MAGIC.len() && bytes[..ENTRY_MAGIC.len()] != ENTRY_MAGIC {
            return Err(EntryDefect::Corrupt);
        }
        if bytes.len() < 8 {
            return Err(EntryDefect::Truncated);
        }
        let body_len = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if body_len > MAX_ENTRY_BODY {
            return Err(EntryDefect::Corrupt);
        }
        let total = 8 + body_len + 32;
        if bytes.len() < total {
            return Err(EntryDefect::Truncated);
        }
        let marker =
            crate::marker::decode_canonical(&bytes[8..8 + body_len]).ok_or(EntryDefect::Corrupt)?;
        let mut chain_hash = [0u8; 32];
        chain_hash.copy_from_slice(&bytes[8 + body_len..total]);
        Ok((marker, Digest(chain_hash), total))
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
