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
use fgdb_types::{
    BranchId, CommitSeq, CommitSeqExhausted as CommitSeqExhaustion, GraphId, MarkerRef, ObjectId,
};

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

/// Bit 0 of [`CommitMarker::flags`]: the marker carries [`IdentityCounters`]
/// immediately after `flags`. A marker written before the field existed has
/// the bit clear and decodes unchanged (additive minor). A reader that
/// predates the field meets trailing bytes and refuses the marker.
pub const MARKER_FLAG_IDENTITY_COUNTERS: u32 = 1;

/// The engine identity allocator's persisted counters (fgdb-hxgm1 channel 2,
/// owner ruling 2026-10-09).
///
/// Engine-issued identities are a keyed permutation of a per-kind counter.
/// Every counter at or below these values may already have issued an
/// identity, and the next engine allocation of each kind takes a larger one.
/// Each engine commit records the counters, so recovery accumulates and
/// validates the allocator floor instead of inverting the permutation over
/// every retained identity. A marker without counters preserves both floors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IdentityCounters {
    pub vertex: u64,
    pub edge: u64,
}

/// The independently monotonic domain of an engine identity counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityCounterKind {
    Vertex,
    Edge,
}

impl IdentityCounters {
    fn validate_successor(self, next: Self) -> Result<(), ChainError> {
        for (kind, previous, found) in [
            (IdentityCounterKind::Vertex, self.vertex, next.vertex),
            (IdentityCounterKind::Edge, self.edge, next.edge),
        ] {
            if found < previous {
                return Err(ChainError::IdentityCounterRegression {
                    kind,
                    previous,
                    found,
                });
            }
        }
        Ok(())
    }
}

/// A canonical commit marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMarker {
    pub logical_command_seq: u64,
    pub commit_seq: u64,
    pub effect_source: EffectSource,
    /// An explicit global predecessor, when supplied, must name the exact
    /// preceding marker. The landed local format also admits `None` after
    /// genesis; existing writers use that encoding and remain readable.
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
    /// Strictly increasing committed logical time. Zero is the genesis floor,
    /// not a committed instant; the largest value is assignable exactly once.
    pub commit_hlc: u64,
    pub final_effect_digest: Digest,
    pub authorization_decision_digest: Digest,
    pub resource_effect_digest: Digest,
    pub payload_availability_certificate_oid: Option<ObjectId>,
    /// Encoded verbatim. Bit 0 is [`MARKER_FLAG_IDENTITY_COUNTERS`] and must
    /// agree with `identity_counters`.
    pub flags: u32,
    /// Present exactly when `flags` carries [`MARKER_FLAG_IDENTITY_COUNTERS`].
    pub identity_counters: Option<IdentityCounters>,
}

impl CommitMarker {
    /// The marker's canonical bytes, EXCLUDING the chain hash — which is
    /// exactly the transcript the chain hash is computed over. There is no
    /// second, shorter encoding: this function is the only definition of what
    /// a marker's bytes are.
    ///
    /// Returns [`ChainError::HeadUpdateCountOverflow`] when the in-memory
    /// update count cannot be represented by the format's `u32` count field,
    /// and [`ChainError::IdentityCountersFlagMismatch`] when `flags` bit 0
    /// disagrees with `identity_counters`.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ChainError> {
        if (self.flags & MARKER_FLAG_IDENTITY_COUNTERS != 0) != self.identity_counters.is_some() {
            return Err(ChainError::IdentityCountersFlagMismatch);
        }
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
        out.extend_from_slice(&encoded_head_update_count(self.head_updates.len())?);
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
        if let Some(counters) = self.identity_counters {
            out.extend_from_slice(&counters.vertex.to_be_bytes());
            out.extend_from_slice(&counters.edge.to_be_bytes());
        }
        Ok(out)
    }

    /// This marker carrying `counters`, with flags bit 0 set to match.
    #[must_use]
    pub fn with_identity_counters(mut self, counters: IdentityCounters) -> Self {
        self.flags |= MARKER_FLAG_IDENTITY_COUNTERS;
        self.identity_counters = Some(counters);
        self
    }

    /// `chain_hash` hashes the prior chain value plus marker bytes excluding
    /// `chain_hash` (plan a10:1938). It returns the same encoding error as
    /// [`Self::canonical_bytes`] rather than hashing a truncated transcript.
    pub fn chain_hash(&self, prior_chain: Digest) -> Result<Digest, ChainError> {
        let mut hasher = fgdb_crypto::Hasher::new();
        hasher.update(MARKER_CHAIN_DOMAIN);
        hasher.update(&prior_chain.0);
        hasher.update(&self.canonical_bytes()?);
        Ok(hasher.finalize())
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

fn encoded_head_update_count(count: usize) -> Result<[u8; 4], ChainError> {
    let encoded = u32::try_from(count).map_err(|_| ChainError::HeadUpdateCountOverflow {
        count,
        max: u32::MAX as usize,
    })?;
    Ok(encoded.to_be_bytes())
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

/// A supplied global predecessor disagrees with the actual committed tail.
/// Boxed in [`ChainError`] so the two full marker identities do not enlarge
/// every successful validation result or unrelated refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlobalPredecessorMismatch {
    pub expected: Option<MarkerRef>,
    pub found: MarkerRef,
}

/// Why a marker could not be appended to a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainError {
    /// The persisted chain frontier is the largest representable sequence, so
    /// no next marker exists in the gap-free commit domain.
    CommitSeqExhausted(CommitSeqExhaustion),
    /// The persisted semantic-command frontier is the largest representable
    /// position. No marker can advance it, so later appends are permanently
    /// refused rather than reported as ordinary non-monotonic offers.
    LogicalCommandSeqExhausted { frontier: u64 },
    /// `commit_seq` is not exactly one past the current tail. The commit
    /// sequence is gap-free by construction: a gap would make "the history up
    /// to N" ambiguous, and a repeat would make it contradictory.
    NonContiguousCommitSeq { expected: u64, found: u64 },
    /// `logical_command_seq` did not advance. Two commits cannot share one
    /// logical command position.
    NonMonotonicCommandSeq { previous: u64, found: u64 },
    /// The committed logical clock has reached its largest value. No later
    /// marker can advance it; wrapping or reusing that instant is forbidden.
    CommitHlcExhausted { frontier: u64 },
    /// A marker's committed logical clock did not advance beyond the previous
    /// committed instant (or the zero genesis floor).
    NonMonotonicCommitHlc { previous: u64, found: u64 },
    /// An explicit predecessor names a different marker or exists at genesis.
    /// An omitted predecessor remains legal in the landed local format.
    GlobalPredecessorMismatch(Box<GlobalPredecessorMismatch>),
    /// The canonical marker format carries the head-update count as a `u32`.
    /// Refuse an in-memory marker outside that domain rather than truncating
    /// its count and hashing an ambiguous transcript.
    HeadUpdateCountOverflow { count: usize, max: usize },
    /// Head updates are unsorted or contain a duplicate `(graph, branch)`.
    NonCanonicalHeadUpdates,
    /// `flags` bit 0 says identity counters follow, but the marker carries
    /// none, or carries counters with the bit clear. Either would encode a
    /// transcript that decodes to a different marker.
    IdentityCountersFlagMismatch,
    /// A marker would forget previously issued engine identities. Equality is
    /// legal, and a legacy marker without counters preserves both floors.
    IdentityCounterRegression {
        kind: IdentityCounterKind,
        previous: u64,
        found: u64,
    },
    /// A branch head compare-and-swap failed: the branch's head is not what
    /// this marker expected. THE WRITE IS REFUSED — this is the mechanism that
    /// makes concurrent branch advancement safe, so it must never be a
    /// warning.
    HeadCasMismatch(Box<HeadCasMismatch>),
}

impl core::fmt::Display for ChainError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CommitSeqExhausted(cause) => write!(f, "{cause}"),
            Self::LogicalCommandSeqExhausted { frontier } => write!(
                f,
                "logical command sequence space is exhausted at {frontier}"
            ),
            Self::NonContiguousCommitSeq { expected, found } => {
                write!(f, "commit_seq {found} is not the expected {expected}")
            }
            Self::NonMonotonicCommandSeq { previous, found } => {
                write!(f, "logical_command_seq {found} does not exceed {previous}")
            }
            Self::CommitHlcExhausted { frontier } => {
                write!(f, "committed logical clock is exhausted at {frontier}")
            }
            Self::NonMonotonicCommitHlc { previous, found } => {
                write!(f, "commit_hlc {found} does not exceed {previous}")
            }
            Self::GlobalPredecessorMismatch(mismatch) => write!(
                f,
                "global predecessor {:?} does not match the committed tail {:?}",
                mismatch.found, mismatch.expected
            ),
            Self::HeadUpdateCountOverflow { count, max } => write!(
                f,
                "head-update count {count} exceeds the canonical-format maximum {max}"
            ),
            Self::NonCanonicalHeadUpdates => {
                f.write_str("head updates are unsorted or contain a duplicate coordinate")
            }
            Self::IdentityCountersFlagMismatch => {
                f.write_str("marker flags disagree with the presence of identity counters")
            }
            Self::IdentityCounterRegression {
                kind,
                previous,
                found,
            } => write!(
                f,
                "{kind:?} identity counter {found} is below its durable floor {previous}"
            ),
            Self::HeadCasMismatch(mismatch) => write!(
                f,
                "branch ({:?},{:?}) head is {:?}, marker expected {:?}",
                mismatch.graph, mismatch.branch, mismatch.actual, mismatch.expected
            ),
        }
    }
}

impl core::error::Error for ChainError {}

impl From<CommitSeqExhaustion> for ChainError {
    fn from(cause: CommitSeqExhaustion) -> Self {
        Self::CommitSeqExhausted(cause)
    }
}

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
    /// Cumulative floors, including across later markers without counters.
    /// Keeping them at admission avoids a history scan on every append.
    identity_counters: IdentityCounters,
}

/// The sufficient state for validating the next marker, without retaining any
/// prior marker body. Read-only checkpoint selection needs this state while it
/// scans, but never the complete history a writer or history reader owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MarkerFrontier {
    commit_seq: CommitSeq,
    marker_oid: ObjectId,
    logical_command_seq: u64,
    commit_hlc: u64,
    chain_hash: Digest,
}

impl From<&ChainedMarker> for MarkerFrontier {
    fn from(chained: &ChainedMarker) -> Self {
        Self {
            commit_seq: CommitSeq(chained.marker.commit_seq),
            marker_oid: chained.marker_oid,
            logical_command_seq: chained.marker.logical_command_seq,
            commit_hlc: chained.marker.commit_hlc,
            chain_hash: chained.chain_hash,
        }
    }
}

/// Streaming verification state. Its only growing collection is the current
/// head per distinct graph/branch, which is necessary to verify later CASes.
#[derive(Debug, Default)]
pub(crate) struct MarkerPrefix {
    frontier: Option<MarkerFrontier>,
    heads: Vec<((GraphId, BranchId), MarkerRef)>,
    identity_counters: IdentityCounters,
}

impl MarkerPrefix {
    pub(crate) fn frontier(&self) -> CommitSeq {
        self.frontier
            .map_or(CommitSeq::ORIGIN, |last| last.commit_seq)
    }

    pub(crate) fn chain_value(&self) -> Digest {
        self.frontier.map_or(CHAIN_ORIGIN, |last| last.chain_hash)
    }

    pub(crate) fn next_commit_seq(&self) -> Result<CommitSeq, ChainError> {
        next_commit_seq(self.frontier)
    }

    pub(crate) fn append(&mut self, marker: CommitMarker) -> Result<(), ChainError> {
        let chain_hash =
            validate_marker(&marker, self.frontier, &self.heads, self.identity_counters)?;
        let chained = ChainedMarker {
            marker,
            marker_oid: ObjectId(chain_hash.0),
            chain_hash,
        };
        self.frontier = Some(adopt_marker(
            &chained,
            self.frontier,
            &mut self.heads,
            &mut self.identity_counters,
        )?);
        Ok(())
    }
}

fn next_commit_seq(frontier: Option<MarkerFrontier>) -> Result<CommitSeq, ChainError> {
    match frontier {
        None => Ok(CommitSeq::FIRST),
        Some(last) => Ok(last.commit_seq.checked_successor()?),
    }
}

fn branch_head(
    heads: &[((GraphId, BranchId), MarkerRef)],
    graph: GraphId,
    branch: BranchId,
) -> Option<MarkerRef> {
    heads
        .iter()
        .find(|((g, b), _)| *g == graph && *b == branch)
        .map(|(_, marker)| *marker)
}

/// The single structural/hash admission law for retained and streaming scans.
/// Refusal happens before either representation changes its frontier or heads.
fn validate_marker(
    marker: &CommitMarker,
    frontier: Option<MarkerFrontier>,
    heads: &[((GraphId, BranchId), MarkerRef)],
    identity_counters: IdentityCounters,
) -> Result<Digest, ChainError> {
    let expected_seq = next_commit_seq(frontier)?;
    if marker.commit_seq != expected_seq.0 {
        return Err(ChainError::NonContiguousCommitSeq {
            expected: expected_seq.0,
            found: marker.commit_seq,
        });
    }
    if let Some(last) = frontier {
        if last.logical_command_seq == u64::MAX {
            return Err(ChainError::LogicalCommandSeqExhausted {
                frontier: last.logical_command_seq,
            });
        }
        if marker.logical_command_seq <= last.logical_command_seq {
            return Err(ChainError::NonMonotonicCommandSeq {
                previous: last.logical_command_seq,
                found: marker.logical_command_seq,
            });
        }
    }
    if let Some(found) = marker.prev_global {
        let expected = frontier.map(|last| MarkerRef {
            marker_oid: last.marker_oid,
            commit_seq: last.commit_seq,
        });
        if expected != Some(found) {
            return Err(ChainError::GlobalPredecessorMismatch(Box::new(
                GlobalPredecessorMismatch { expected, found },
            )));
        }
    }
    let previous_hlc = frontier.map_or(0, |last| last.commit_hlc);
    if previous_hlc == u64::MAX {
        return Err(ChainError::CommitHlcExhausted {
            frontier: previous_hlc,
        });
    }
    if marker.commit_hlc <= previous_hlc {
        return Err(ChainError::NonMonotonicCommitHlc {
            previous: previous_hlc,
            found: marker.commit_hlc,
        });
    }
    if let Some(next) = marker.identity_counters {
        identity_counters.validate_successor(next)?;
    }
    if !marker.head_updates_are_canonical() {
        return Err(ChainError::NonCanonicalHeadUpdates);
    }

    // All CASes are checked before any is applied: advancing several branches
    // must remain atomic even when an earlier update would have succeeded.
    for update in &marker.head_updates {
        let actual = branch_head(heads, update.graph, update.branch);
        if actual != update.expected_previous {
            return Err(ChainError::HeadCasMismatch(Box::new(HeadCasMismatch {
                graph: update.graph,
                branch: update.branch,
                expected: update.expected_previous,
                actual,
            })));
        }
    }

    marker.chain_hash(frontier.map_or(CHAIN_ORIGIN, |last| last.chain_hash))
}

/// The head index has exactly one writer in both representations. As on the
/// durable path, a previously validated marker is refused if it became stale.
fn adopt_marker(
    chained: &ChainedMarker,
    frontier: Option<MarkerFrontier>,
    heads: &mut Vec<((GraphId, BranchId), MarkerRef)>,
    identity_counters: &mut IdentityCounters,
) -> Result<MarkerFrontier, ChainError> {
    let expected_seq = next_commit_seq(frontier)?;
    if chained.marker.commit_seq != expected_seq.0 {
        return Err(ChainError::NonContiguousCommitSeq {
            expected: expected_seq.0,
            found: chained.marker.commit_seq,
        });
    }
    if let Some(next) = chained.marker.identity_counters {
        identity_counters.validate_successor(next)?;
    }
    for update in &chained.marker.head_updates {
        let new_head = MarkerRef {
            marker_oid: chained.marker_oid,
            commit_seq: CommitSeq(chained.marker.commit_seq),
        };
        match heads
            .iter_mut()
            .find(|((g, b), _)| *g == update.graph && *b == update.branch)
        {
            Some((_, head)) => *head = new_head,
            None => heads.push(((update.graph, update.branch), new_head)),
        }
    }
    if let Some(next) = chained.marker.identity_counters {
        *identity_counters = next;
    }
    Ok(MarkerFrontier::from(chained))
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
    pub fn next_commit_seq(&self) -> Result<CommitSeq, ChainError> {
        next_commit_seq(self.entries.last().map(MarkerFrontier::from))
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

    /// The cumulative engine counters admitted by this chain, or zero before
    /// any marker carried them. Counter-less markers cannot reset either
    /// domain. These are committed floors, not reservations for identities
    /// issued later by a live transaction that has not published a reservation.
    pub fn identity_counters(&self) -> IdentityCounters {
        self.identity_counters
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
        branch_head(&self.heads, graph, branch)
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
        let chain_hash = validate_marker(
            marker,
            self.entries.last().map(MarkerFrontier::from),
            &self.heads,
            self.identity_counters,
        )?;
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
        adopt_marker(
            &chained,
            self.entries.last().map(MarkerFrontier::from),
            &mut self.heads,
            &mut self.identity_counters,
        )?;
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
    let (marker, consumed) = decode_canonical_prefix(bytes, bytes.len()).ok()?;
    (consumed == bytes.len()).then_some(marker)
}

/// Why a canonical prefix cannot be decoded.
///
/// A missing byte can be a torn append. A present illegal tag or impossible
/// head count cannot: recovery must preserve that distinction even when the
/// outer frame length is damaged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerDecodeError {
    Truncated,
    Corrupt,
}

/// Decode one canonical marker prefix and report its exact byte extent.
///
/// The commit-log framing layer uses this when the declared frame length runs
/// past EOF. A complete marker plus the fixed framing suffix proves that the
/// bytes are a complete damaged frame. A present illegal tag independently
/// proves corruption; only missing fields remain eligible for torn-tail repair.
/// `max_body_len` is the caller's canonical-body bound, independent of the
/// available prefix: a count that cannot fit that bound is already corruption.
/// Other callers must use [`decode_canonical`], which rejects trailing bytes.
pub(crate) fn decode_canonical_prefix(
    bytes: &[u8],
    max_body_len: usize,
) -> Result<(CommitMarker, usize), MarkerDecodeError> {
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
        _ => return Err(MarkerDecodeError::Corrupt),
    };

    let prev_global = cursor.optional_marker_ref()?;

    let head_count = cursor.u32()? as usize;
    // Every update needs two 128-bit identities and an option tag. The fixed
    // suffix still needs its merge/availability tags, schema and effect
    // digests, epochs, token, HLC and flags; optional values only add bytes.
    // Refuse before allocating or reading updates. Otherwise an impossible
    // count followed by EOF looks like a torn append and permits truncating
    // an acknowledged frame whose leading length was also damaged.
    const MIN_HEAD_UPDATE_BYTES: usize = 16 + 16 + 1;
    const MIN_TRAILING_BYTES: usize = 1 + 32 + 8 + 8 + 8 + 16 + 8 + 32 + 32 + 32 + 1 + 4;
    let minimum_body_len = head_count
        .checked_mul(MIN_HEAD_UPDATE_BYTES)
        .and_then(|heads| cursor.position().checked_add(heads))
        .and_then(|prefix| prefix.checked_add(MIN_TRAILING_BYTES))
        .ok_or(MarkerDecodeError::Corrupt)?;
    if minimum_body_len > max_body_len {
        return Err(MarkerDecodeError::Corrupt);
    }
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
        _ => return Err(MarkerDecodeError::Corrupt),
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
        _ => return Err(MarkerDecodeError::Corrupt),
    };
    let flags = cursor.u32()?;
    let identity_counters = if flags & MARKER_FLAG_IDENTITY_COUNTERS != 0 {
        Some(IdentityCounters {
            vertex: cursor.u64()?,
            edge: cursor.u64()?,
        })
    } else {
        None
    };

    let consumed = cursor.position();
    Ok((
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
            identity_counters,
        },
        consumed,
    ))
}

/// A bounds-checked big-endian reader. Missing input remains distinguishable
/// from invalid tags, so an incomplete frame cannot hide present corruption.
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

    fn take(&mut self, len: usize) -> Result<&'a [u8], MarkerDecodeError> {
        let end = self
            .position
            .checked_add(len)
            .ok_or(MarkerDecodeError::Corrupt)?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or(MarkerDecodeError::Truncated)?;
        self.position = end;
        Ok(slice)
    }

    fn byte(&mut self) -> Result<u8, MarkerDecodeError> {
        self.take(1).map(|slice| slice[0])
    }

    fn u32(&mut self) -> Result<u32, MarkerDecodeError> {
        let slice = self.take(4)?;
        Ok(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn u64(&mut self) -> Result<u64, MarkerDecodeError> {
        let slice = self.take(8)?;
        let mut value = [0u8; 8];
        value.copy_from_slice(slice);
        Ok(u64::from_be_bytes(value))
    }

    fn u128(&mut self) -> Result<u128, MarkerDecodeError> {
        let slice = self.take(16)?;
        let mut value = [0u8; 16];
        value.copy_from_slice(slice);
        Ok(u128::from_be_bytes(value))
    }

    fn array16(&mut self) -> Result<[u8; 16], MarkerDecodeError> {
        let slice = self.take(16)?;
        let mut value = [0u8; 16];
        value.copy_from_slice(slice);
        Ok(value)
    }

    fn array32(&mut self) -> Result<[u8; 32], MarkerDecodeError> {
        let slice = self.take(32)?;
        let mut value = [0u8; 32];
        value.copy_from_slice(slice);
        Ok(value)
    }

    /// The `Option<MarkerRef>` encoding: a presence byte then, if present, the
    /// oid and sequence. An absent presence byte is truncated; an illegal
    /// presence byte is corrupt even when later fields are missing.
    fn optional_marker_ref(&mut self) -> Result<Option<MarkerRef>, MarkerDecodeError> {
        match self.byte()? {
            0x00 => Ok(None),
            0x01 => Ok(Some(MarkerRef {
                marker_oid: ObjectId(self.array32()?),
                commit_seq: CommitSeq(self.u64()?),
            })),
            _ => Err(MarkerDecodeError::Corrupt),
        }
    }
}

#[cfg(test)]
mod commit_seq_exhaustion_tests {
    use super::*;
    use fgdb_types::CommitSeqExhausted;

    fn marker(commit_seq: u64) -> CommitMarker {
        CommitMarker {
            logical_command_seq: commit_seq,
            commit_seq,
            effect_source: EffectSource::Local {
                capsule_ref: ObjectId([0x11; 32]),
                logical_delta_template_digest: Digest([0x22; 32]),
            },
            prev_global: None,
            head_updates: Vec::new(),
            merge_record_oid: None,
            coordinate_schema_transition_digest: Digest([0x33; 32]),
            topology_epoch: 1,
            policy_epoch: 1,
            revocation_index: 1,
            txn_token: [0x44; 16],
            commit_hlc: commit_seq,
            final_effect_digest: Digest([0x55; 32]),
            authorization_decision_digest: Digest([0x66; 32]),
            resource_effect_digest: Digest([0x77; 32]),
            payload_availability_certificate_oid: None,
            flags: 0,
            identity_counters: None,
        }
    }

    #[test]
    fn exhaustion_accepts_max_once_then_permanently_refuses_without_mutation() {
        // A complete real chain cannot be iterated to this boundary in a unit
        // test. Seed only its authenticated tail; append's successor law reads
        // exactly that persisted frontier and is the subject under test.
        let penultimate = marker(u64::MAX - 1);
        let mut chain = MarkerChain {
            entries: vec![ChainedMarker {
                marker: penultimate,
                marker_oid: ObjectId([0x88; 32]),
                chain_hash: Digest([0x99; 32]),
            }],
            heads: Vec::new(),
            identity_counters: IdentityCounters::default(),
        };
        let mut prefix = MarkerPrefix {
            frontier: chain.entries.last().map(MarkerFrontier::from),
            heads: Vec::new(),
            identity_counters: IdentityCounters::default(),
        };

        assert_eq!(chain.next_commit_seq(), Ok(CommitSeq(u64::MAX)));
        assert_eq!(prefix.next_commit_seq(), chain.next_commit_seq());
        prefix.append(marker(u64::MAX)).unwrap();
        chain
            .append(marker(u64::MAX))
            .expect("the final representable sequence is assignable");
        let settled = chain.entries.clone();
        let exhausted = ChainError::CommitSeqExhausted(CommitSeqExhausted {
            frontier: CommitSeq(u64::MAX),
        });

        for _ in 0..2 {
            assert_eq!(chain.next_commit_seq(), Err(exhausted.clone()));
            assert_eq!(prefix.next_commit_seq(), Err(exhausted.clone()));
            assert_eq!(prefix.append(marker(u64::MAX)), Err(exhausted.clone()));
            assert_eq!(
                chain.append(marker(u64::MAX)).err(),
                Some(exhausted.clone())
            );
            assert_eq!(
                chain.entries, settled,
                "an exhausted refusal changes no state"
            );
            assert_eq!(prefix.chain_value(), chain.chain_value());
            assert_eq!(prefix.frontier(), CommitSeq(u64::MAX));
        }
    }

    #[test]
    fn streaming_prefix_matches_history_without_retaining_prior_marker_bodies() {
        let mut chain = MarkerChain::new();
        let mut prefix = MarkerPrefix::default();
        let mut head_capacity = None;
        for seq in 1..=4096 {
            let mut next = marker(seq);
            if seq % 7 == 0 {
                next = next.with_identity_counters(IdentityCounters {
                    vertex: seq,
                    edge: seq * 2,
                });
            }
            // Alternate graphs and branches, including high identity bits.
            // Every third commit advances all three heads atomically.
            let coordinates = [
                (GraphId(1), BranchId(1)),
                (GraphId(1), BranchId(1u128 << 96)),
                (GraphId(1u128 << 96), BranchId(1)),
            ];
            next.head_updates = coordinates
                .into_iter()
                .enumerate()
                .filter(|(i, _)| seq % 3 == 0 || *i == (seq % 3) as usize)
                .map(|(_, (graph, branch))| HeadUpdate {
                    graph,
                    branch,
                    expected_previous: chain.head(graph, branch),
                })
                .collect();
            next.prev_global = chain.entries.last().map(|last| MarkerRef {
                marker_oid: last.marker_oid,
                commit_seq: CommitSeq(last.marker.commit_seq),
            });
            prefix.append(next.clone()).unwrap();
            chain.append(next).unwrap();
            assert_eq!(prefix.frontier(), CommitSeq(seq));
            assert_eq!(prefix.chain_value(), chain.chain_value());
            assert_eq!(prefix.next_commit_seq(), chain.next_commit_seq());
            assert_eq!(prefix.heads, chain.heads);
            let counted = seq / 7 * 7;
            assert_eq!(
                prefix.identity_counters,
                IdentityCounters {
                    vertex: counted,
                    edge: counted * 2,
                }
            );
            assert_eq!(prefix.identity_counters, chain.identity_counters());
            if seq >= 3 {
                assert_eq!(prefix.heads.len(), 3);
                assert_eq!(
                    *head_capacity.get_or_insert(prefix.heads.capacity()),
                    prefix.heads.capacity(),
                    "streaming retention grows with distinct heads, not commits"
                );
            }
        }
        assert_eq!(chain.entries.len(), 4096);
        assert_eq!(prefix.heads.len(), 3);
    }

    #[test]
    fn identity_counter_maximum_preserves_the_streaming_floor_across_legacy_markers() {
        let mut prefix = MarkerPrefix::default();
        let maximum = IdentityCounters {
            vertex: u64::MAX,
            edge: u64::MAX,
        };
        prefix
            .append(marker(1).with_identity_counters(maximum))
            .unwrap();
        prefix.append(marker(2)).unwrap();
        prefix
            .append(marker(3).with_identity_counters(maximum))
            .unwrap();
        let settled = prefix.frontier;
        for (counters, kind) in [
            (
                IdentityCounters {
                    vertex: 0,
                    edge: u64::MAX,
                },
                IdentityCounterKind::Vertex,
            ),
            (
                IdentityCounters {
                    vertex: u64::MAX,
                    edge: 0,
                },
                IdentityCounterKind::Edge,
            ),
        ] {
            assert_eq!(
                prefix.append(marker(4).with_identity_counters(counters)),
                Err(ChainError::IdentityCounterRegression {
                    kind,
                    previous: u64::MAX,
                    found: 0
                })
            );
            assert_eq!(prefix.frontier, settled);
            assert_eq!(prefix.identity_counters, maximum);
        }
        prefix.append(marker(4)).unwrap();
        assert_eq!(prefix.identity_counters, maximum);
    }

    #[test]
    fn streaming_prefix_refuses_every_structural_defect_without_advancing_heads() {
        let mut chain = MarkerChain::new();
        let mut prefix = MarkerPrefix::default();
        let mut first = marker(1);
        first = first.with_identity_counters(IdentityCounters { vertex: 5, edge: 7 });
        first.head_updates = [BranchId(1), BranchId(2)]
            .into_iter()
            .map(|branch| HeadUpdate {
                graph: GraphId(1),
                branch,
                expected_previous: None,
            })
            .collect();
        prefix.append(first.clone()).unwrap();
        chain.append(first).unwrap();
        let mut good = marker(2);
        good = good.with_identity_counters(IdentityCounters { vertex: 5, edge: 7 });
        good.head_updates = [BranchId(1), BranchId(2)]
            .into_iter()
            .map(|branch| HeadUpdate {
                graph: GraphId(1),
                branch,
                expected_previous: chain.head(GraphId(1), branch),
            })
            .collect();

        let mut defects = Vec::new();
        let mut bad = good.clone();
        bad.commit_seq = 3;
        defects.push(bad);
        let mut bad = good.clone();
        bad.logical_command_seq = 1;
        defects.push(bad);
        let mut bad = good.clone();
        bad.prev_global = Some(MarkerRef {
            marker_oid: ObjectId([0xff; 32]),
            commit_seq: CommitSeq(1),
        });
        defects.push(bad);
        for commit_hlc in [0, 1] {
            let mut bad = good.clone();
            bad.commit_hlc = commit_hlc;
            defects.push(bad);
        }
        let mut bad = good.clone();
        bad.head_updates.reverse();
        defects.push(bad);
        let mut bad = good.clone();
        bad.head_updates[1] = bad.head_updates[0];
        defects.push(bad);
        for wrong_head in 0..2 {
            let mut bad = good.clone().with_identity_counters(IdentityCounters {
                vertex: 100,
                edge: 100,
            });
            bad.head_updates[wrong_head].expected_previous = None;
            defects.push(bad);
        }
        let mut bad = good.clone();
        bad.flags = MARKER_FLAG_IDENTITY_COUNTERS;
        bad.identity_counters = None;
        defects.push(bad);
        defects.push(
            good.clone()
                .with_identity_counters(IdentityCounters { vertex: 4, edge: 8 }),
        );
        defects.push(
            good.clone()
                .with_identity_counters(IdentityCounters { vertex: 6, edge: 6 }),
        );

        let settled_frontier = prefix.frontier;
        let settled_heads = prefix.heads.clone();
        let settled_counters = prefix.identity_counters;
        for bad in defects {
            let expected = chain.append(bad.clone()).unwrap_err();
            assert_eq!(prefix.append(bad), Err(expected));
            assert_eq!(chain.len(), 1);
            assert_eq!(prefix.frontier, settled_frontier);
            assert_eq!(prefix.heads, settled_heads);
            assert_eq!(prefix.heads, chain.heads);
            assert_eq!(prefix.identity_counters, settled_counters);
            assert_eq!(prefix.identity_counters, chain.identity_counters());
        }
        // Refusals do not consume the next sequence or partially advance the
        // first head when only the second CAS was wrong.
        good.logical_command_seq = u64::MAX;
        prefix.append(good.clone()).unwrap();
        chain.append(good).unwrap();
        let bad = marker(3);
        let expected = chain.append(bad.clone()).unwrap_err();
        assert!(matches!(
            expected,
            ChainError::LogicalCommandSeqExhausted { .. }
        ));
        assert_eq!(prefix.append(bad), Err(expected));
        assert_eq!(prefix.heads, chain.heads);
        assert_eq!(prefix.chain_value(), chain.chain_value());
    }

    #[test]
    fn streaming_prefix_preserves_the_genesis_and_exhausted_clock_boundaries() {
        let mut chain = MarkerChain::new();
        let mut prefix = MarkerPrefix::default();
        let mut zero = marker(1);
        zero.commit_hlc = 0;
        let expected = ChainError::NonMonotonicCommitHlc {
            previous: 0,
            found: 0,
        };
        assert_eq!(prefix.append(zero.clone()), Err(expected.clone()));
        assert_eq!(chain.append(zero).unwrap_err(), expected);
        assert_eq!(prefix.frontier(), CommitSeq::ORIGIN);

        let mut maximum = marker(1);
        maximum.commit_hlc = u64::MAX;
        prefix.append(maximum.clone()).unwrap();
        chain.append(maximum).unwrap();
        let settled = prefix.frontier;
        for commit_hlc in [0, 1, u64::MAX] {
            let mut next = marker(2);
            next.commit_hlc = commit_hlc;
            let expected = ChainError::CommitHlcExhausted { frontier: u64::MAX };
            assert_eq!(prefix.append(next.clone()), Err(expected.clone()));
            assert_eq!(chain.append(next).unwrap_err(), expected);
            assert_eq!(prefix.frontier, settled);
            assert_eq!(prefix.chain_value(), chain.chain_value());
            assert_eq!(chain.len(), 1);
        }
    }

    #[test]
    fn head_update_count_encoding_is_exact_at_the_u32_boundary() {
        assert_eq!(
            encoded_head_update_count(u32::MAX as usize),
            Ok(u32::MAX.to_be_bytes())
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn head_update_count_encoding_refuses_the_u32_boundary_plus_one() {
        let count = u32::MAX as usize + 1;
        assert_eq!(
            encoded_head_update_count(count),
            Err(ChainError::HeadUpdateCountOverflow {
                count,
                max: u32::MAX as usize,
            })
        );
    }
}
