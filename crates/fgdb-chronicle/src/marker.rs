//! `CommitMarker`: the ~100-byte record that *is* the commit stream.
//!
//! Everything B1 claims rests here. A marker is what a commit durably becomes,
//! and the chain of markers is simultaneously the MVCC version order, the
//! time-travel history, the replication stream, and the branch-head record —
//! not four mechanisms that agree, one mechanism read four ways.
//!
//! TWO PROPERTIES MAKE THAT WORK, and both are enforced here rather than
//! assumed:
//!
//! 1. **The chain hash.** `chain_hash` covers the prior chain value plus the
//!    marker's own bytes excluding `chain_hash` itself. So the chain value at
//!    sequence N commits to the entire history up to N: tampering with any
//!    earlier marker invalidates every marker after it, and detection names
//!    the exact sequence where the history diverges.
//!
//! 2. **No forward references.** The marker deliberately names no terminal
//!    outcome and no future record, fragment, or batch. Apply constructs the
//!    marker FIRST and later objects may name it — which is what keeps the
//!    outcome and distributed-batch graphs acyclic. Verification therefore
//!    needs nothing that does not already exist, and a reader can validate a
//!    stream prefix without the suffix.
//!
//! Branch heads ride the same structure: each `head_update` carries the
//! `expected_previous` marker for its branch, so advancing a head is a
//! compare-and-swap against the history rather than a write that hopes.
//!
//! THIS INCREMENT lands the Local effect source. The `Global` arm carries W12
//! meta types whose union arms are still in the G0 decision batch, so it is
//! absent rather than guessed — a subset of the final abstraction, not a
//! substitute for it (doctrine 7).

use fgdb_crypto::Digest;
use fgdb_types::{BranchId, CommitSeq, GraphId, MarkerRef, ObjectId};

/// Domain separator for the marker chain hash.
pub const MARKER_CHAIN_DOMAIN: &[u8] = b"fgdb:commit-marker-chain:v2";

/// The chain value before any marker exists. Genesis chains from this, so
/// every stream has a defined origin rather than an implicit zero.
pub const CHAIN_ORIGIN: Digest = Digest([0u8; 32]);

/// Where a commit's effects came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectSource {
    /// Local apply: the already-built capsule and delta template.
    Local {
        capsule_ref: ObjectId,
        logical_delta_template_digest: Digest,
    },
}

impl EffectSource {
    fn write_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Local {
                capsule_ref,
                logical_delta_template_digest,
            } => {
                // The arm tag is part of the transcript: a reader that does
                // not know a future tag must reject the marker, not skip it.
                out.push(0x01);
                out.extend_from_slice(&capsule_ref.0);
                out.extend_from_slice(&logical_delta_template_digest.0);
            }
        }
    }
}

/// One branch head this commit advances.
///
/// `expected_previous` is the compare-and-swap: `None` means "this branch has
/// no head yet", and `Some(marker)` means "advance only if the head is still
/// exactly that marker". The marker itself becomes the new head implicitly —
/// it does not name its own successor, which is what keeps the graph acyclic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadUpdate {
    pub graph: GraphId,
    pub branch: BranchId,
    pub expected_previous: Option<MarkerRef>,
}

impl HeadUpdate {
    fn write_into(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.graph.0.to_be_bytes());
        out.extend_from_slice(&self.branch.0.to_be_bytes());
        match self.expected_previous {
            None => out.push(0x00),
            Some(previous) => {
                out.push(0x01);
                out.extend_from_slice(&previous.marker_oid.0);
                out.extend_from_slice(&previous.commit_seq.0.to_be_bytes());
            }
        }
    }
}

/// A canonical commit marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMarker {
    pub logical_command_seq: u64,
    pub commit_seq: u64,
    pub effect_source: EffectSource,
    pub prev_global: Option<MarkerRef>,
    /// Canonically sorted by `(graph, branch)` — the sort is part of the
    /// transcript, so two apply paths producing the same updates in different
    /// orders produce the same marker.
    pub head_updates: Vec<HeadUpdate>,
    pub merge_record_oid: Option<ObjectId>,
    pub coordinate_schema_transition_digest: Digest,
    pub topology_epoch: u64,
    pub policy_epoch: u64,
    pub revocation_index: u64,
    pub txn_token: [u8; 16],
    pub commit_hlc: u64,
    pub final_effect_digest: Digest,
    pub authorization_decision_digest: Digest,
    pub resource_effect_digest: Digest,
    pub payload_availability_certificate_oid: Option<ObjectId>,
    pub flags: u32,
}

impl CommitMarker {
    /// The marker's canonical bytes, EXCLUDING the chain hash — which is
    /// exactly the transcript the chain hash is computed over. There is no
    /// second, shorter encoding: this function is the only definition of what
    /// a marker's bytes are.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.logical_command_seq.to_be_bytes());
        out.extend_from_slice(&self.commit_seq.to_be_bytes());
        self.effect_source.write_into(&mut out);
        match self.prev_global {
            None => out.push(0x00),
            Some(previous) => {
                out.push(0x01);
                out.extend_from_slice(&previous.marker_oid.0);
                out.extend_from_slice(&previous.commit_seq.0.to_be_bytes());
            }
        }
        out.extend_from_slice(&(self.head_updates.len() as u32).to_be_bytes());
        for update in &self.head_updates {
            update.write_into(&mut out);
        }
        match self.merge_record_oid {
            None => out.push(0x00),
            Some(oid) => {
                out.push(0x01);
                out.extend_from_slice(&oid.0);
            }
        }
        out.extend_from_slice(&self.coordinate_schema_transition_digest.0);
        out.extend_from_slice(&self.topology_epoch.to_be_bytes());
        out.extend_from_slice(&self.policy_epoch.to_be_bytes());
        out.extend_from_slice(&self.revocation_index.to_be_bytes());
        out.extend_from_slice(&self.txn_token);
        out.extend_from_slice(&self.commit_hlc.to_be_bytes());
        out.extend_from_slice(&self.final_effect_digest.0);
        out.extend_from_slice(&self.authorization_decision_digest.0);
        out.extend_from_slice(&self.resource_effect_digest.0);
        match self.payload_availability_certificate_oid {
            None => out.push(0x00),
            Some(oid) => {
                out.push(0x01);
                out.extend_from_slice(&oid.0);
            }
        }
        out.extend_from_slice(&self.flags.to_be_bytes());
        out
    }

    /// `chain_hash` hashes the prior chain value plus marker bytes excluding
    /// `chain_hash` (plan a10:1938).
    pub fn chain_hash(&self, prior_chain: Digest) -> Digest {
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(MARKER_CHAIN_DOMAIN);
        hasher.update(&prior_chain.0);
        hasher.update(&self.canonical_bytes());
        hasher.finalize()
    }

    /// Whether the head updates are canonically sorted and free of duplicate
    /// coordinates. A duplicate `(graph, branch)` would make the marker's own
    /// effect on that head ambiguous.
    fn head_updates_are_canonical(&self) -> bool {
        self.head_updates
            .windows(2)
            .all(|pair| (pair[0].graph, pair[0].branch) < (pair[1].graph, pair[1].branch))
    }
}

/// Typed evidence for a failed branch-head compare-and-swap.
///
/// The detail is boxed inside [`ChainError`] because two canonical marker
/// references plus 128-bit graph and branch identities are intentionally
/// large. Allocation is confined to the refused-write path; successful
/// appends and the other error variants remain compact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadCasMismatch {
    pub graph: GraphId,
    pub branch: BranchId,
    pub expected: Option<MarkerRef>,
    pub actual: Option<MarkerRef>,
}

/// Why a marker could not be appended to a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// `commit_seq` is not exactly one past the current tail. The commit
    /// sequence is gap-free by construction: a gap would make "the history up
    /// to N" ambiguous, and a repeat would make it contradictory.
    NonContiguousCommitSeq { expected: u64, found: u64 },
    /// `logical_command_seq` did not advance. Two commits cannot share one
    /// logical command position.
    NonMonotonicCommandSeq { previous: u64, found: u64 },
    /// Head updates are unsorted or contain a duplicate `(graph, branch)`.
    NonCanonicalHeadUpdates,
    /// A branch head compare-and-swap failed: the branch's head is not what
    /// this marker expected. THE WRITE IS REFUSED — this is the mechanism that
    /// makes concurrent branch advancement safe, so it must never be a
    /// warning.
    HeadCasMismatch(Box<HeadCasMismatch>),
}

impl core::fmt::Display for ChainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NonContiguousCommitSeq { expected, found } => {
                write!(f, "commit_seq {found} is not the expected {expected}")
            }
            Self::NonMonotonicCommandSeq { previous, found } => {
                write!(f, "logical_command_seq {found} does not exceed {previous}")
            }
            Self::NonCanonicalHeadUpdates => {
                f.write_str("head updates are unsorted or contain a duplicate coordinate")
            }
            Self::HeadCasMismatch(mismatch) => write!(
                f,
                "branch ({:?},{:?}) head is {:?}, marker expected {:?}",
                mismatch.graph, mismatch.branch, mismatch.actual, mismatch.expected
            ),
        }
    }
}

impl core::error::Error for ChainError {}

/// Why a whole-chain verification failed, and where.
///
/// Carries the sequence AND the cause. The previous return type was a bare `u64`,
/// which could only ever mean "the chain hash disagrees here" — once `verify`
/// enforces the structural laws too, a caller that cannot tell a broken hash from
/// a sequence gap cannot act on the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainVerifyFailure {
    pub commit_seq: u64,
    pub cause: ChainVerifyCause,
}

/// What was wrong with the entry `verify` stopped at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainVerifyCause {
    /// A law `validate` enforces at append time does not hold for this entry.
    Structure(ChainError),
    /// The entry's stored `chain_hash` or `marker_oid` is not what replaying the
    /// prefix produces.
    DerivedValueMismatch,
}

impl core::fmt::Display for ChainVerifyFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match &self.cause {
            ChainVerifyCause::Structure(cause) => {
                write!(f, "commit {}: {cause}", self.commit_seq)
            }
            ChainVerifyCause::DerivedValueMismatch => write!(
                f,
                "commit {}: stored chain hash or marker id does not match the replayed prefix",
                self.commit_seq
            ),
        }
    }
}

impl core::error::Error for ChainVerifyFailure {}

/// An appended marker together with the chain value it produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainedMarker {
    pub marker: CommitMarker,
    pub marker_oid: ObjectId,
    /// The chain value AFTER this marker — the commitment to all history
    /// through `marker.commit_seq`.
    pub chain_hash: Digest,
}

/// The commit stream: an append-only chain of markers with branch heads.
///
/// This is deliberately the whole of B1's version universe in one structure.
/// A reader asking "what was the state at sequence N", "what is this branch's
/// head", "what changed between N and M", or "what must a replica apply next"
/// is asking the same object four different questions.
#[derive(Debug, Clone, Default)]
pub struct MarkerChain {
    entries: Vec<ChainedMarker>,
    heads: Vec<((GraphId, BranchId), MarkerRef)>,
}

impl MarkerChain {
    pub fn new() -> Self {
        Self::default()
    }

    /// The chain value committing to all history so far.
    pub fn chain_value(&self) -> Digest {
        self.entries
            .last()
            .map_or(CHAIN_ORIGIN, |entry| entry.chain_hash)
    }

    /// The next `commit_seq` this chain will accept. Sequences start at 1, so
    /// 0 can never be a valid commit and an uninitialised field cannot look
    /// like the first commit.
    pub fn next_commit_seq(&self) -> u64 {
        self.entries
            .last()
            .map_or(1, |entry| entry.marker.commit_seq + 1)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[ChainedMarker] {
        &self.entries
    }

    /// Fold one entry's head updates into the head index.
    ///
    /// Extracted so the head index has exactly ONE writer. A second copy in a
    /// constructor would be a duplicated law, and duplicated laws drift — the same
    /// reason `verify` below re-uses `validate` rather than restating its checks.
    fn advance_heads(&mut self, chained: &ChainedMarker) {
        for update in &chained.marker.head_updates {
            let new_head = MarkerRef {
                marker_oid: chained.marker_oid,
                commit_seq: CommitSeq(chained.marker.commit_seq),
            };
            match self
                .heads
                .iter_mut()
                .find(|((g, b), _)| *g == update.graph && *b == update.branch)
            {
                Some((_, head)) => *head = new_head,
                None => self.heads.push(((update.graph, update.branch), new_head)),
            }
        }
    }

    /// Reconstruct a chain from stored entries, validating the complete prefix.
    ///
    /// Raw storage is an untrusted representation, not a second unchecked
    /// constructor. Every entry therefore follows the same [`validate`](Self::validate)
    /// path as [`append`](Self::append), both derived values must match, and the
    /// head index advances only after that entry is accepted. An error returns no
    /// partially trusted `MarkerChain`.
    pub fn from_entries(entries: &[ChainedMarker]) -> Result<Self, ChainVerifyFailure> {
        let mut rebuilt = Self::new();
        for entry in entries {
            let commit_seq = entry.marker.commit_seq;
            let chained = rebuilt
                .validate(&entry.marker)
                .map_err(|cause| ChainVerifyFailure {
                    commit_seq,
                    cause: ChainVerifyCause::Structure(cause),
                })?;
            if chained.chain_hash != entry.chain_hash || chained.marker_oid != entry.marker_oid {
                return Err(ChainVerifyFailure {
                    commit_seq,
                    cause: ChainVerifyCause::DerivedValueMismatch,
                });
            }
            rebuilt.adopt(chained).map_err(|cause| ChainVerifyFailure {
                commit_seq,
                cause: ChainVerifyCause::Structure(cause),
            })?;
        }
        Ok(rebuilt)
    }

    /// Verify raw stored entries without constructing a usable chain.
    ///
    /// This is the discriminator for recovery and mutation fixtures: malformed
    /// bytes may be represented as `ChainedMarker` values, but they never become
    /// authoritative heads or a chain value.
    pub fn verify_entries(entries: &[ChainedMarker]) -> Result<(), ChainVerifyFailure> {
        Self::from_entries(entries).map(drop)
    }

    /// A branch's current head, if it has one.
    pub fn head(&self, graph: GraphId, branch: BranchId) -> Option<MarkerRef> {
        self.heads
            .iter()
            .find(|((g, b), _)| *g == graph && *b == branch)
            .map(|(_, marker)| *marker)
    }

    /// Append one marker, enforcing every structural law before anything is
    /// mutated — so a refused append leaves the chain and every head exactly
    /// as they were.
    pub fn append(&mut self, marker: CommitMarker) -> Result<&ChainedMarker, ChainError> {
        let chained = self.validate(&marker)?;
        self.adopt(chained)
    }

    /// Check every law and compute what the marker WOULD become, without
    /// touching the chain.
    ///
    /// This split exists for the durable commit path. A marker must be fully
    /// validated before it is written, but the in-memory chain must not move
    /// until the write is durable — otherwise a crash between the two leaves
    /// memory claiming a commit the disk never got. Separating the decision
    /// from the mutation makes that ordering structural instead of a comment,
    /// and it is what lets the coordinator avoid cloning the whole chain per
    /// commit just to have something safe to validate against.
    pub fn validate(&self, marker: &CommitMarker) -> Result<ChainedMarker, ChainError> {
        let expected_seq = self.next_commit_seq();
        if marker.commit_seq != expected_seq {
            return Err(ChainError::NonContiguousCommitSeq {
                expected: expected_seq,
                found: marker.commit_seq,
            });
        }
        if let Some(last) = self.entries.last()
            && marker.logical_command_seq <= last.marker.logical_command_seq
        {
            return Err(ChainError::NonMonotonicCommandSeq {
                previous: last.marker.logical_command_seq,
                found: marker.logical_command_seq,
            });
        }
        if !marker.head_updates_are_canonical() {
            return Err(ChainError::NonCanonicalHeadUpdates);
        }

        // Every head compare-and-swap is checked BEFORE any is applied: a
        // marker that advances three branches must advance all three or none,
        // or a partial failure would leave the stream describing a state that
        // never existed.
        for update in &marker.head_updates {
            let actual = self.head(update.graph, update.branch);
            if actual != update.expected_previous {
                return Err(ChainError::HeadCasMismatch(Box::new(HeadCasMismatch {
                    graph: update.graph,
                    branch: update.branch,
                    expected: update.expected_previous,
                    actual,
                })));
            }
        }

        let chain_hash = marker.chain_hash(self.chain_value());
        // The marker's identity is its own canonical bytes under the chain
        // value, so two markers with identical content at different points in
        // history are distinct objects — which is what makes a MarkerRef a
        // history identity rather than a content coincidence.
        let marker_oid = ObjectId(chain_hash.0);

        Ok(ChainedMarker {
            marker: marker.clone(),
            marker_oid,
            chain_hash,
        })
    }

    /// Adopt a marker that [`validate`](Self::validate) already approved.
    ///
    /// Crate-private on purpose: an outside caller must not be able to push a
    /// marker that was never checked. The sequence is re-checked here because
    /// a `ChainedMarker` validated against an older state is stale — its chain
    /// hash was computed over a history that has since moved — and adopting it
    /// would silently fork the chain value.
    pub(crate) fn adopt(&mut self, chained: ChainedMarker) -> Result<&ChainedMarker, ChainError> {
        let expected_seq = self.next_commit_seq();
        if chained.marker.commit_seq != expected_seq {
            return Err(ChainError::NonContiguousCommitSeq {
                expected: expected_seq,
                found: chained.marker.commit_seq,
            });
        }

        self.advance_heads(&chained);
        self.entries.push(chained);
        Ok(self.entries.last().expect("just pushed"))
    }

    /// Verify the whole chain from the origin: **this chain is exactly what
    /// appending its markers in order would have produced.**
    ///
    /// IT USED TO CHECK CHAIN-HASH CONTINUITY AND NOTHING ELSE (fgdb-dcq7), while
    /// every test in this crate reads it as the general "is this chain sound"
    /// question. So a chain carrying a sequence gap, a non-advancing command
    /// sequence, unsorted head updates, or a head CAS that never held would have
    /// verified — each of them a law `validate` enforces at append time.
    ///
    /// Closed by REPLAY rather than by restating the predicates: this rebuilds the
    /// chain from the origin through `validate`, which is the same code path an
    /// append takes, and compares the derived values at every step. So `verify`
    /// enforces every law `validate` does, by construction and forever — a law
    /// added to `validate` tomorrow is enforced here the same day. Restating the
    /// four checks would have been the obvious fix and would have created two
    /// copies of one law, which is how they drift apart.
    ///
    /// Still needs nothing beyond the entries themselves — no index, no future
    /// object — because markers carry no forward references, so a replica or a
    /// recovery pass can verify a stream PREFIX without waiting for the rest.
    pub fn verify(&self) -> Result<(), ChainVerifyFailure> {
        Self::verify_entries(&self.entries)
    }
}

/// Decode a marker from its canonical bytes, or `None` if they are short or
/// malformed.
///
/// This is the inverse of [`CommitMarker::canonical_bytes`] and exists because
/// the durable commit log stores exactly those bytes: recovery must be able to
/// rebuild a marker from the stream WITHOUT any index. Returning `None` rather
/// than erroring is deliberate — the caller distinguishes a torn tail (normal
/// after a crash) from corruption (not), and only the caller knows which
/// position the entry occupied.
pub fn decode_canonical(bytes: &[u8]) -> Option<CommitMarker> {
    let (marker, consumed) = decode_canonical_prefix(bytes)?;
    (consumed == bytes.len()).then_some(marker)
}

/// Decode one canonical marker prefix and report its exact byte extent.
///
/// The commit-log framing layer uses this only when the declared frame length
/// runs past EOF. Finding a complete marker plus the fixed framing suffix proves
/// that the bytes are a complete damaged frame, not a genuinely torn write.
/// Other callers must use [`decode_canonical`], which rejects trailing bytes.
pub(crate) fn decode_canonical_prefix(bytes: &[u8]) -> Option<(CommitMarker, usize)> {
    let mut cursor = ByteReader::new(bytes);

    let logical_command_seq = cursor.u64()?;
    let commit_seq = cursor.u64()?;

    let effect_source = match cursor.byte()? {
        0x01 => EffectSource::Local {
            capsule_ref: ObjectId(cursor.array32()?),
            logical_delta_template_digest: Digest(cursor.array32()?),
        },
        // An unknown arm tag is rejected, never skipped: a reader that does
        // not know a tag must not pretend to have understood the marker.
        _ => return None,
    };

    let prev_global = cursor.optional_marker_ref()?;

    let head_count = cursor.u32()? as usize;
    let mut head_updates = Vec::with_capacity(head_count.min(1024));
    for _ in 0..head_count {
        head_updates.push(HeadUpdate {
            graph: GraphId(cursor.u128()?),
            branch: BranchId(cursor.u128()?),
            expected_previous: cursor.optional_marker_ref()?,
        });
    }

    let merge_record_oid = match cursor.byte()? {
        0x00 => None,
        0x01 => Some(ObjectId(cursor.array32()?)),
        _ => return None,
    };
    let coordinate_schema_transition_digest = Digest(cursor.array32()?);
    let topology_epoch = cursor.u64()?;
    let policy_epoch = cursor.u64()?;
    let revocation_index = cursor.u64()?;
    let txn_token = cursor.array16()?;
    let commit_hlc = cursor.u64()?;
    let final_effect_digest = Digest(cursor.array32()?);
    let authorization_decision_digest = Digest(cursor.array32()?);
    let resource_effect_digest = Digest(cursor.array32()?);
    let payload_availability_certificate_oid = match cursor.byte()? {
        0x00 => None,
        0x01 => Some(ObjectId(cursor.array32()?)),
        _ => return None,
    };
    let flags = cursor.u32()?;

    let consumed = cursor.position();
    Some((
        CommitMarker {
            logical_command_seq,
            commit_seq,
            effect_source,
            prev_global,
            head_updates,
            merge_record_oid,
            coordinate_schema_transition_digest,
            topology_epoch,
            policy_epoch,
            revocation_index,
            txn_token,
            commit_hlc,
            final_effect_digest,
            authorization_decision_digest,
            resource_effect_digest,
            payload_availability_certificate_oid,
            flags,
        },
        consumed,
    ))
}

/// A bounds-checked big-endian reader. Every accessor returns `Option` so a
/// truncated buffer can never produce a partially-populated marker.
struct ByteReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn position(&self) -> usize {
        self.position
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(len)?;
        let slice = self.bytes.get(self.position..end)?;
        self.position = end;
        Some(slice)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|slice| slice[0])
    }

    fn u32(&mut self) -> Option<u32> {
        let slice = self.take(4)?;
        Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        let slice = self.take(8)?;
        let mut value = [0u8; 8];
        value.copy_from_slice(slice);
        Some(u64::from_be_bytes(value))
    }

    fn u128(&mut self) -> Option<u128> {
        let slice = self.take(16)?;
        let mut value = [0u8; 16];
        value.copy_from_slice(slice);
        Some(u128::from_be_bytes(value))
    }

    fn array16(&mut self) -> Option<[u8; 16]> {
        let slice = self.take(16)?;
        let mut value = [0u8; 16];
        value.copy_from_slice(slice);
        Some(value)
    }

    fn array32(&mut self) -> Option<[u8; 32]> {
        let slice = self.take(32)?;
        let mut value = [0u8; 32];
        value.copy_from_slice(slice);
        Some(value)
    }

    /// The `Option<MarkerRef>` encoding: a presence byte then, if present, the
    /// oid and sequence. The outer Option is "did the bytes parse"; the inner
    /// one is "was a ref present".
    fn optional_marker_ref(&mut self) -> Option<Option<MarkerRef>> {
        match self.byte()? {
            0x00 => Some(None),
            0x01 => Some(Some(MarkerRef {
                marker_oid: ObjectId(self.array32()?),
                commit_seq: CommitSeq(self.u64()?),
            })),
            _ => None,
        }
    }
}
