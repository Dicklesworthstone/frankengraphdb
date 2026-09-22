//! Aegis's deterministic Raft transition kernel (plan §14.1).
//!
//! This crate performs no I/O and owns no threads, clocks, sockets, or codecs.
//! A runtime supplies authenticated, configuration-bound messages and seeded
//! election timeouts. Commands must already name validated, durably owned
//! payload closures: consensus is NOT a payload-availability certificate.
//!
//! Every transition yields a [`Persistence`] view. If it requires a write,
//! publish that exact state through Chronicle's immutable root closure and
//! sync the root before calling [`Raft::persisted`]. Otherwise the existing
//! published root already covers the transition. Only `persisted` releases
//! messages and committed entries. An unknown/failed publication requires
//! recovery; cancellation leaves the node blocked, never able to vote or
//! reply from speculative state.
//!
//! Snapshot offers are not installations. [`SnapshotTransfer`] asks the runtime
//! to acquire and verify the exact snapshot closure using ATP. Only after that
//! succeeds may it submit [`Event::SnapshotReady`]; publication of the resulting
//! persistence view must atomically install the application snapshot and Raft
//! state before the successful reply can escape. [`Event::Compact`] likewise
//! requires the generated log-to-state and complete retention-floor verifiers,
//! including the audit-visible applied cut. A committed index alone is NOT
//! permission to compact. No snapshot operation changes configuration.
//!
//! These are in-process transition types, NOT an alternate durable/wire format.
//! The Appendix A serializer, payload certificate verifier, root publisher,
//! authenticated transport, and application state machine remain separate
//! integration obligations. No database clustering capability is enabled here.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// A member coordinate resolved inside the authenticated consensus domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MemberId(pub u128);

/// Full digest of the canonical database/namespace/incarnation/role/group tuple.
/// Supplied by the domain verifier, never inferred from a group number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Domain(pub [u8; 32]);

/// A verified stable or joint configuration. Learners cannot vote or lead.
/// A joint configuration requires separate majorities of its old and new
/// voter sets. Their union is for routing, never a replacement quorum rule.
/// A configuration digest is required even when the member lists are equal.
///
/// This is one fixed, authenticated configuration per machine incarnation.
/// Constructing a different configuration does not authorize a live membership
/// change: the ordered transition, payload floor and retirement protocol must
/// first publish it as the authoritative configuration before recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    domain: Domain,
    identity: [u8; 32],
    voters: BTreeSet<MemberId>,
    learners: BTreeSet<MemberId>,
    joint: Option<JointVoters>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct JointVoters {
    old: BTreeSet<MemberId>,
    new: BTreeSet<MemberId>,
}

impl Configuration {
    fn collect_voters(
        voters: impl IntoIterator<Item = MemberId>,
    ) -> Result<BTreeSet<MemberId>, Error> {
        let mut members = BTreeSet::new();
        for member in voters {
            if member.0 == 0 || members.len() >= 1024 || !members.insert(member) {
                return Err(Error::InvalidConfiguration);
            }
        }
        if members.is_empty() {
            return Err(Error::InvalidConfiguration);
        }
        Ok(members)
    }

    pub fn stable(
        domain: Domain,
        identity: [u8; 32],
        voters: impl IntoIterator<Item = MemberId>,
        learners: impl IntoIterator<Item = MemberId>,
    ) -> Result<Self, Error> {
        let voter_set = Self::collect_voters(voters)?;
        let mut learner_set = BTreeSet::new();
        for member in learners {
            if member.0 == 0
                || voter_set.len() + learner_set.len() >= 1024
                || voter_set.contains(&member)
                || !learner_set.insert(member)
            {
                return Err(Error::InvalidConfiguration);
            }
        }
        Ok(Self {
            domain,
            identity,
            voters: voter_set,
            learners: learner_set,
            joint: None,
        })
    }

    /// Construct from the exact authenticated joint configuration. Overlap
    /// between old/new voters is legal and counts once within each group;
    /// duplicates inside a group or overlap with learners are invalid.
    /// The limit of 1024 applies to the unique union including learners.
    pub fn joint(
        domain: Domain,
        identity: [u8; 32],
        old_voters: impl IntoIterator<Item = MemberId>,
        new_voters: impl IntoIterator<Item = MemberId>,
        learners: impl IntoIterator<Item = MemberId>,
    ) -> Result<Self, Error> {
        let old = Self::collect_voters(old_voters)?;
        let new = Self::collect_voters(new_voters)?;
        let mut configuration = Self::stable(domain, identity, old.union(&new).copied(), learners)?;
        configuration.joint = Some(JointVoters { old, new });
        Ok(configuration)
    }

    /// Routing/election-candidate universe, NOT a pooled joint quorum.
    pub fn voters(&self) -> &BTreeSet<MemberId> {
        &self.voters
    }

    pub fn learners(&self) -> &BTreeSet<MemberId> {
        &self.learners
    }

    /// The exact independent voter groups, or None for a stable configuration.
    pub fn joint_voters(&self) -> Option<(&BTreeSet<MemberId>, &BTreeSet<MemberId>)> {
        self.joint.as_ref().map(|joint| (&joint.old, &joint.new))
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }

    fn members(&self) -> impl Iterator<Item = MemberId> + '_ {
        self.voters.union(&self.learners).copied()
    }

    fn contains(&self, member: MemberId) -> bool {
        self.voters.contains(&member) || self.learners.contains(&member)
    }

    fn quorum(&self, members: &BTreeSet<MemberId>) -> bool {
        let majority = |voters: &BTreeSet<MemberId>| {
            voters.intersection(members).count() > voters.len() / 2
        };
        match &self.joint {
            None => majority(&self.voters),
            Some(joint) => majority(&joint.old) && majority(&joint.new),
        }
    }

    fn quorum_index(&self, matched: impl Fn(MemberId) -> u64) -> u64 {
        let majority_index = |voters: &BTreeSet<MemberId>| {
            // Every admitted group is nonempty. Select the strict-majority
            // order statistic without sorting the entire membership vector.
            let mut indices: Vec<_> = voters.iter().copied().map(&matched).collect();
            let rank = (indices.len() - 1) / 2;
            *indices.select_nth_unstable(rank).1
        };
        match &self.joint {
            None => majority_index(&self.voters),
            Some(joint) => majority_index(&joint.old).min(majority_index(&joint.new)),
        }
    }
}

/// A verifier-produced view of an exact canonical Raft snapshot, not a format.
/// Full object identities are retained; numeric positions are never authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotCut {
    domain: Domain,
    configuration: [u8; 32],
    manifest: [u8; 32],
    state_root: [u8; 32],
    retention_floor: [u8; 32],
    index: u64,
    term: u64,
}

impl SnapshotCut {
    /// Call only after authenticating the canonical snapshot manifest and
    /// proving its role/configuration, exact state-at-cut and retention floor.
    /// For Compact, the cut must additionally be locally applied and visible.
    /// Structural validation here cannot replace either proof. An offered cut
    /// still needs its complete closure transferred and verified before Ready.
    pub fn from_authenticated_parts(
        configuration: &Configuration,
        manifest: [u8; 32],
        state_root: [u8; 32],
        retention_floor: [u8; 32],
        index: u64,
        term: u64,
    ) -> Result<Self, Error> {
        if index == 0 || index == u64::MAX || term == 0 {
            return Err(Error::InvalidSnapshot);
        }
        Ok(Self {
            domain: configuration.domain,
            configuration: configuration.identity,
            manifest,
            state_root,
            retention_floor,
            index,
            term,
        })
    }

    pub fn domain(&self) -> Domain {
        self.domain
    }

    pub fn configuration(&self) -> [u8; 32] {
        self.configuration
    }

    pub fn manifest(&self) -> [u8; 32] {
        self.manifest
    }

    pub fn state_root(&self) -> [u8; 32] {
        self.state_root
    }

    pub fn retention_floor(&self) -> [u8; 32] {
        self.retention_floor
    }

    pub fn index(&self) -> u64 {
        self.index
    }

    pub fn term(&self) -> u64 {
        self.term
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Bounds the retained suffix, not the lifetime absolute Raft index.
    pub max_log_entries: usize,
    pub max_append_entries: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_log_entries: 65_536,
            max_append_entries: 128,
        }
    }
}

/// A Raft no-op has no command and MUST NOT advance LogicalCommandSeq/CommitSeq.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry<C> {
    pub term: u64,
    pub command: Option<C>,
}

/// Logical contents to encode into the exact Appendix A root closure.
/// Private fields prevent unchecked mutation of a live node's persistent state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistentState<C> {
    configuration: Configuration,
    term: u64,
    voted_for: Option<MemberId>,
    commit_index: u64,
    snapshot: Option<SnapshotCut>,
    entries: Vec<Entry<C>>,
}

impl<C> PersistentState<C> {
    pub fn term(&self) -> u64 {
        self.term
    }

    pub fn voted_for(&self) -> Option<MemberId> {
        self.voted_for
    }

    pub fn commit_index(&self) -> u64 {
        self.commit_index
    }

    /// The suffix starts at snapshot.index + 1, or at index 1 without a snapshot.
    pub fn entries(&self) -> &[Entry<C>] {
        &self.entries
    }

    pub fn snapshot(&self) -> Option<&SnapshotCut> {
        self.snapshot.as_ref()
    }

    pub fn configuration(&self) -> &Configuration {
        &self.configuration
    }

    /// Construct an uncompacted state after authenticating the entire closure.
    /// Structural validation still occurs in [`Raft::recover`].
    pub fn from_authenticated_parts(
        configuration: Configuration,
        term: u64,
        voted_for: Option<MemberId>,
        commit_index: u64,
        entries: Vec<Entry<C>>,
    ) -> Self {
        Self {
            configuration,
            term,
            voted_for,
            commit_index,
            snapshot: None,
            entries,
        }
    }

    /// Recover a verified installed snapshot plus its contiguous retained suffix.
    /// The application root and Raft cut must have been published together;
    /// merely downloading a snapshot does not license this constructor.
    pub fn from_authenticated_snapshot(
        configuration: Configuration,
        term: u64,
        voted_for: Option<MemberId>,
        commit_index: u64,
        snapshot: SnapshotCut,
        entries: Vec<Entry<C>>,
    ) -> Self {
        Self {
            configuration,
            term,
            voted_for,
            commit_index,
            snapshot: Some(snapshot),
            entries,
        }
    }

    fn base_index(&self) -> u64 {
        self.snapshot.as_ref().map_or(0, SnapshotCut::index)
    }

    fn base_term(&self) -> u64 {
        self.snapshot.as_ref().map_or(0, SnapshotCut::term)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Follower,
    Candidate,
    Leader,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message<C> {
    RequestVote {
        term: u64,
        last_index: u64,
        last_term: u64,
    },
    Vote {
        term: u64,
        granted: bool,
    },
    Append {
        term: u64,
        request: u64,
        prev_index: u64,
        prev_term: u64,
        entries: Vec<Entry<C>>,
        leader_commit: u64,
    },
    Appended {
        term: u64,
        request: u64,
        success: bool,
        /// Advisory next-index hint; never authority for match_index.
        conflict_next: u64,
    },
    /// Offer only. Receipt of this message never installs application state.
    InstallSnapshot {
        term: u64,
        request: u64,
        snapshot: SnapshotCut,
    },
    /// Acknowledges the exact in-flight cut, not an arbitrary reported index.
    SnapshotInstalled {
        term: u64,
        request: u64,
    },
}

impl<C> Message<C> {
    fn term(&self) -> u64 {
        match self {
            Self::RequestVote { term, .. }
            | Self::Vote { term, .. }
            | Self::Append { term, .. }
            | Self::Appended { term, .. }
            | Self::InstallSnapshot { term, .. }
            | Self::SnapshotInstalled { term, .. } => *term,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope<C> {
    pub domain: Domain,
    pub configuration: [u8; 32],
    pub from: MemberId,
    pub to: MemberId,
    pub message: Message<C>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event<C> {
    ElectionTimeout,
    Heartbeat,
    Propose(C),
    Receive(Envelope<C>),
    /// The verifier-proved applied/visible cut and complete floor permit retiring
    /// this log prefix. Runtime publication must retain the snapshot closure.
    Compact(SnapshotCut),
    /// The exact requested closure has been acquired, authenticated and durably
    /// owned. The resulting Persistence still needs atomic application/Raft
    /// installation before persisted may release the successful response.
    SnapshotReady(SnapshotTransferId),
    /// Cancel one transfer, without acknowledging or changing committed state.
    /// A later retransmitted offer can start a fresh transfer.
    SnapshotFailed(SnapshotTransferId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    InvalidLimits,
    InvalidRecoveryState,
    WrongDomain,
    WrongConfiguration,
    WrongRecipient,
    UnknownMember,
    NotVoter,
    NotLeader,
    AwaitingDurability,
    StalePersistence,
    RecoveryRequired,
    LogFull,
    AppendTooLarge,
    InvalidMessage,
    CommittedConflict,
    EntryIdentityConflict,
    CounterExhausted,
    InvalidSnapshot,
    SnapshotRequired,
    SnapshotConflict,
    StaleSnapshotTransfer,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis Raft: {self:?}")
    }
}

impl core::error::Error for Error {}

/// Publication generation bound to this exact machine incarnation.
/// Retaining an old token retains its allocation, preventing address reuse from
/// making a token valid after recovery. No clock, entropy, or global ID is used.
#[derive(Clone, Debug)]
pub struct PersistenceId {
    incarnation: Arc<()>,
    generation: u64,
}

impl PartialEq for PersistenceId {
    fn eq(&self, other: &Self) -> bool {
        self.generation == other.generation && Arc::ptr_eq(&self.incarnation, &other.incarnation)
    }
}

impl Eq for PersistenceId {}

/// An in-process transfer capability. It cannot be decoded from network bytes
/// or reused on another voter, after recovery, or after a newer leader offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotTransferId(PersistenceId);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotTransfer {
    id: SnapshotTransferId,
    source: MemberId,
    snapshot: SnapshotCut,
}

impl SnapshotTransfer {
    pub fn id(&self) -> SnapshotTransferId {
        self.id.clone()
    }

    pub fn source(&self) -> MemberId {
        self.source
    }

    pub fn snapshot(&self) -> &SnapshotCut {
        &self.snapshot
    }
}

/// Contains no outbound messages or apply-ready commands.
#[derive(Debug)]
pub struct Persistence<'a, C> {
    id: PersistenceId,
    state: &'a PersistentState<C>,
    requires_write: bool,
}

impl<C> Persistence<'_, C> {
    pub fn id(&self) -> PersistenceId {
        self.id.clone()
    }

    pub fn state(&self) -> &PersistentState<C> {
        self.state
    }

    /// False means the existing published root already covers this transition.
    /// Heartbeats, duplicate replies and volatile vote tallies need no new fsync.
    pub fn requires_write(&self) -> bool {
        self.requires_write
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Committed<C> {
    pub index: u64,
    pub entry: Entry<C>,
}

/// Released only after the corresponding root publication is acknowledged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output<C> {
    pub messages: Vec<Envelope<C>>,
    pub committed: Vec<Committed<C>>,
    /// Start or idempotently resume each exact transfer. No donor bytes are yet
    /// considered installed, committed or available for reads/voting.
    pub snapshot_transfers: Vec<SnapshotTransfer>,
    /// Cancel the matching transfer region after its leader/request is fenced.
    /// Cancellation retires no durable object or prepared ownership promise.
    pub cancelled_snapshot_transfers: Vec<SnapshotTransferId>,
    /// Notification of the atomically published application/Raft snapshot cut.
    /// Compaction alone does not produce an application installation event.
    pub installed_snapshot: Option<SnapshotCut>,
    /// A granted vote, campaign, or non-stale leader append resets the timer.
    pub reset_election_timer: bool,
    pub role: Role,
    pub leader: Option<MemberId>,
}

#[derive(Clone, Debug)]
enum InFlight {
    Append { request: u64, prev: u64, last: u64 },
    Snapshot { request: u64, snapshot: SnapshotCut },
}

#[derive(Clone, Debug)]
struct Progress {
    matched: u64,
    next: u64,
    in_flight: Option<InFlight>,
}

#[derive(Clone, Debug)]
struct IncomingSnapshot {
    transfer: SnapshotTransfer,
    term: u64,
    request: u64,
}

/// One owned transition machine. Copying a live voter would duplicate voting
/// authority, so there is no Clone implementation. Recovery must hold the
/// runtime's exclusive writer fence and authenticate the published root.
pub struct Raft<C> {
    id: MemberId,
    state: PersistentState<C>,
    role: Role,
    leader: Option<MemberId>,
    votes: BTreeSet<MemberId>,
    progress: BTreeMap<MemberId, Progress>,
    limits: Limits,
    generation: u64,
    incarnation: Arc<()>,
    initialized: bool,
    log_changed: bool,
    request: u64,
    incoming_snapshot: Option<IncomingSnapshot>,
    pending: Option<(PersistenceId, Output<C>)>,
    poisoned: bool,
}

impl<C: Clone + Eq> Raft<C> {
    pub fn new(id: MemberId, configuration: Configuration, limits: Limits) -> Result<Self, Error> {
        let mut node = Self::recover(
            id,
            PersistentState::from_authenticated_parts(configuration, 0, None, 0, Vec::new()),
            limits,
        )?;
        node.initialized = false;
        Ok(node)
    }

    pub fn recover(id: MemberId, state: PersistentState<C>, limits: Limits) -> Result<Self, Error> {
        if limits.max_log_entries == 0
            || limits.max_append_entries == 0
            || limits.max_append_entries > limits.max_log_entries
        {
            return Err(Error::InvalidLimits);
        }
        if !state.configuration.contains(id) {
            return Err(Error::UnknownMember);
        }
        let last = state
            .base_index()
            .checked_add(state.entries.len() as u64)
            .filter(|last| *last < u64::MAX)
            .ok_or(Error::InvalidRecoveryState)?;
        if state.entries.len() > limits.max_log_entries
            || state.commit_index < state.base_index()
            || state.commit_index > last
            || state.base_term() > state.term
            || state.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.domain != state.configuration.domain
                    || snapshot.configuration != state.configuration.identity
            })
            || (state.term == 0 && state.voted_for.is_some())
            || state
                .voted_for
                .is_some_and(|member| !state.configuration.voters.contains(&member))
        {
            return Err(Error::InvalidRecoveryState);
        }
        let mut previous = state.base_term();
        for entry in &state.entries {
            if entry.term == 0 || entry.term < previous || entry.term > state.term {
                return Err(Error::InvalidRecoveryState);
            }
            previous = entry.term;
        }
        Ok(Self {
            id,
            state,
            role: Role::Follower,
            leader: None,
            votes: BTreeSet::new(),
            progress: BTreeMap::new(),
            limits,
            generation: 0,
            incarnation: Arc::new(()),
            initialized: true,
            log_changed: false,
            request: 0,
            incoming_snapshot: None,
            pending: None,
            poisoned: false,
        })
    }

    pub fn id(&self) -> MemberId {
        self.id
    }

    /// No speculative state can be mistaken for the published recovery state.
    pub fn durable_state(&self) -> Result<&PersistentState<C>, Error> {
        self.available()?;
        Ok(&self.state)
    }

    pub fn role(&self) -> Result<Role, Error> {
        self.available()?;
        Ok(self.role)
    }

    /// Replay is bounded by the durable committed prefix. The application owns
    /// a durable applied cursor and applies entries idempotently. A cursor below
    /// the retained log requires installation of the published snapshot first.
    pub fn committed_after(&self, applied: u64) -> Result<Vec<Committed<C>>, Error> {
        self.available()?;
        if applied > self.state.commit_index {
            return Err(Error::InvalidRecoveryState);
        }
        if applied < self.state.base_index() {
            return Err(Error::SnapshotRequired);
        }
        Ok(self.committed_range(applied))
    }

    fn available(&self) -> Result<(), Error> {
        if self.poisoned {
            Err(Error::RecoveryRequired)
        } else if self.pending.is_some() {
            Err(Error::AwaitingDurability)
        } else {
            Ok(())
        }
    }

    /// A failed/unknown fsync is not a rollback. Reopen from the durable root.
    pub fn publication_failed(&mut self) {
        self.poisoned = true;
    }

    /// Evaluate one input. No other input can overtake its publication.
    pub fn step(&mut self, event: Event<C>) -> Result<Persistence<'_, C>, Error> {
        self.available()?;
        self.validate_event(&event)?;
        let generation = self.generation.checked_add(1).ok_or(Error::CounterExhausted)?;
        let before = self.state.commit_index;
        let previous_transfer = self.incoming_snapshot.as_ref().map(|pending| pending.transfer.id());
        let old_hard = (self.state.term, self.state.voted_for, before);
        self.log_changed = false;
        let mut output = Output {
            messages: Vec::new(),
            committed: Vec::new(),
            snapshot_transfers: Vec::new(),
            cancelled_snapshot_transfers: Vec::new(),
            installed_snapshot: None,
            reset_election_timer: false,
            role: self.role,
            leader: self.leader,
        };
        // Unexpected exhaustion after an internal transition fails closed.
        self.poisoned = true;
        self.generation = generation;
        match event {
            Event::ElectionTimeout => self.campaign(&mut output)?,
            Event::Heartbeat => {
                if self.role == Role::Leader {
                    self.broadcast(&mut output)?;
                }
            }
            Event::Propose(command) => {
                self.log_changed = true;
                self.state.entries.push(Entry {
                    term: self.state.term,
                    command: Some(command),
                });
                self.advance_commit();
                self.broadcast(&mut output)?;
            }
            Event::Receive(envelope) => self.receive(envelope, &mut output)?,
            Event::Compact(snapshot) => self.compact(snapshot),
            Event::SnapshotReady(_) => self.install_snapshot(&mut output)?,
            Event::SnapshotFailed(_) => self.incoming_snapshot = None,
        }
        if let Some(previous) = previous_transfer {
            if output.installed_snapshot.is_none()
                && !self.incoming_snapshot.as_ref().is_some_and(|pending| pending.transfer.id == previous)
            {
                output.cancelled_snapshot_transfers.push(previous);
            }
        }
        output.committed = self.committed_range(before);
        output.role = self.role;
        output.leader = self.leader;
        let requires_write = !self.initialized
            || self.log_changed
            || old_hard != (self.state.term, self.state.voted_for, self.state.commit_index);
        let id = PersistenceId {
            incarnation: Arc::clone(&self.incarnation),
            generation,
        };
        self.pending = Some((id.clone(), output));
        self.poisoned = false;
        Ok(Persistence {
            id,
            state: &self.state,
            requires_write,
        })
    }

    /// Call after the exact state from step has completed root publication,
    /// or immediately when requires_write was false. Snapshot installation must
    /// publish BOTH the verified application cut and this Raft state. Stale
    /// tokens cannot release a later transition, node, or recovered incarnation.
    pub fn persisted(&mut self, id: PersistenceId) -> Result<Output<C>, Error> {
        if self.poisoned {
            return Err(Error::RecoveryRequired);
        }
        match self.pending.as_ref() {
            Some((expected, _)) if expected == &id => {}
            _ => return Err(Error::StalePersistence),
        }
        let Some((_, output)) = self.pending.take() else {
            return Err(Error::StalePersistence);
        };
        self.initialized = true;
        Ok(output)
    }

    fn validate_snapshot(&self, snapshot: &SnapshotCut) -> Result<(), Error> {
        if snapshot.domain != self.state.configuration.domain {
            return Err(Error::WrongDomain);
        }
        if snapshot.configuration != self.state.configuration.identity {
            return Err(Error::WrongConfiguration);
        }
        Ok(())
    }

    fn validate_event(&self, event: &Event<C>) -> Result<(), Error> {
        match event {
            Event::ElectionTimeout if !self.state.configuration.voters.contains(&self.id) => {
                return Err(Error::NotVoter);
            }
            Event::Propose(_) => {
                if self.role != Role::Leader {
                    return Err(Error::NotLeader);
                }
                if self.state.entries.len() >= self.limits.max_log_entries {
                    return Err(Error::LogFull);
                }
                if self.last_index() == u64::MAX - 1 {
                    return Err(Error::CounterExhausted);
                }
            }
            Event::Compact(snapshot) => {
                self.validate_snapshot(snapshot)?;
                if self.state.snapshot.as_ref() != Some(snapshot)
                    && (snapshot.index <= self.state.base_index()
                        || snapshot.index > self.state.commit_index
                        || self.term_at(snapshot.index) != Some(snapshot.term))
                {
                    return Err(Error::InvalidSnapshot);
                }
            }
            Event::SnapshotReady(id) | Event::SnapshotFailed(id) => {
                if !self.incoming_snapshot.as_ref().is_some_and(|pending| {
                    pending.transfer.id == *id
                        && pending.term == self.state.term
                        && self.leader == Some(pending.transfer.source)
                }) {
                    return Err(Error::StaleSnapshotTransfer);
                }
            }
            Event::Receive(envelope) => {
                if envelope.domain != self.state.configuration.domain {
                    return Err(Error::WrongDomain);
                }
                if envelope.configuration != self.state.configuration.identity {
                    return Err(Error::WrongConfiguration);
                }
                if envelope.to != self.id || envelope.from == self.id {
                    return Err(Error::WrongRecipient);
                }
                if !self.state.configuration.contains(envelope.from) {
                    return Err(Error::UnknownMember);
                }
                if !matches!(
                    &envelope.message,
                    Message::Appended { .. } | Message::SnapshotInstalled { .. }
                ) && !self.state.configuration.voters.contains(&envelope.from)
                {
                    return Err(Error::NotVoter);
                }
                if envelope.message.term() == 0 {
                    return Err(Error::InvalidMessage);
                }
                match &envelope.message {
                    Message::Append {
                        term,
                        prev_index,
                        prev_term,
                        entries,
                        ..
                    } => {
                        if entries.len() > self.limits.max_append_entries {
                            return Err(Error::AppendTooLarge);
                        }
                        if (*prev_index == 0) != (*prev_term == 0) || prev_term > term {
                            return Err(Error::InvalidMessage);
                        }
                        let last = prev_index
                            .checked_add(entries.len() as u64)
                            .filter(|last| *last < u64::MAX)
                            .ok_or(Error::InvalidMessage)?;
                        let mut previous = *prev_term;
                        for entry in entries {
                            if entry.term == 0 || entry.term < previous || entry.term > *term {
                                return Err(Error::InvalidMessage);
                            }
                            previous = entry.term;
                        }
                        // A distant predecessor needs a conflict reply, not a
                        // capacity error: it does not allocate the absent gap.
                        if *term >= self.state.term && self.term_at(*prev_index) == Some(*prev_term) {
                            if last - self.state.base_index() > self.limits.max_log_entries as u64 {
                                return Err(Error::LogFull);
                            }
                            for (offset, entry) in entries.iter().enumerate() {
                                let index = *prev_index + offset as u64 + 1;
                                if let Some(local) = self.entry_at(index) {
                                    if local.term != entry.term {
                                        if index <= self.state.commit_index {
                                            return Err(Error::CommittedConflict);
                                        }
                                        break;
                                    }
                                    if local.command != entry.command {
                                        return Err(Error::EntryIdentityConflict);
                                    }
                                }
                            }
                        }
                    }
                    Message::RequestVote { term, last_index, last_term } => {
                        if (*last_index == 0) != (*last_term == 0)
                            || last_term > term
                            || *last_index == u64::MAX
                        {
                            return Err(Error::InvalidMessage);
                        }
                    }
                    Message::InstallSnapshot { term, request, snapshot } => {
                        self.validate_snapshot(snapshot)?;
                        if snapshot.term > *term {
                            return Err(Error::InvalidSnapshot);
                        }
                        if *term >= self.state.term {
                            if snapshot.index <= self.state.commit_index
                                && self.term_at(snapshot.index).is_some_and(|local| local != snapshot.term)
                            {
                                return Err(Error::SnapshotConflict);
                            }
                            if self.incoming_snapshot.as_ref().is_some_and(|pending| {
                                pending.term == *term
                                    && pending.transfer.source == envelope.from
                                    && pending.request == *request
                                    && pending.transfer.snapshot != *snapshot
                            }) {
                                return Err(Error::SnapshotConflict);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn last_index(&self) -> u64 {
        self.state.base_index() + self.state.entries.len() as u64
    }

    fn entry_at(&self, index: u64) -> Option<&Entry<C>> {
        let position = index.checked_sub(self.state.base_index())?.checked_sub(1)?;
        self.state.entries.get(usize::try_from(position).ok()?)
    }

    fn term_at(&self, index: u64) -> Option<u64> {
        if index == self.state.base_index() {
            Some(self.state.base_term())
        } else {
            self.entry_at(index).map(|entry| entry.term)
        }
    }

    fn last_term(&self) -> u64 {
        self.state.entries.last().map_or(self.state.base_term(), |entry| entry.term)
    }

    fn committed_range(&self, after: u64) -> Vec<Committed<C>> {
        let base = self.state.base_index();
        let after = after.max(base);
        self.state.entries[(after - base) as usize..(self.state.commit_index - base) as usize]
            .iter()
            .enumerate()
            .map(|(offset, entry)| Committed {
                index: after + offset as u64 + 1,
                entry: entry.clone(),
            })
            .collect()
    }

    fn emit(&self, to: MemberId, message: Message<C>, output: &mut Output<C>) {
        output.messages.push(Envelope {
            domain: self.state.configuration.domain,
            configuration: self.state.configuration.identity,
            from: self.id,
            to,
            message,
        });
    }

    fn follow(&mut self, term: u64) {
        if term > self.state.term {
            self.state.term = term;
            self.state.voted_for = None;
            self.incoming_snapshot = None;
        }
        self.role = Role::Follower;
        self.leader = None;
        self.votes.clear();
        self.progress.clear();
    }

    fn accept_leader(&mut self, from: MemberId, term: u64, output: &mut Output<C>) {
        self.follow(term);
        if self.incoming_snapshot.as_ref().is_some_and(|pending| pending.transfer.source != from) {
            self.incoming_snapshot = None;
        }
        self.leader = Some(from);
        output.reset_election_timer = true;
    }

    fn campaign(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        if self.role == Role::Leader {
            return Ok(());
        }
        let term = self.state.term.checked_add(1).ok_or(Error::CounterExhausted)?;
        self.follow(term);
        self.role = Role::Candidate;
        self.state.voted_for = Some(self.id);
        self.votes.insert(self.id);
        output.reset_election_timer = true;
        if self.state.configuration.quorum(&self.votes) {
            return self.become_leader(output);
        }
        for member in &self.state.configuration.voters {
            if *member != self.id {
                self.emit(
                    *member,
                    Message::RequestVote {
                        term,
                        last_index: self.last_index(),
                        last_term: self.last_term(),
                    },
                    output,
                );
            }
        }
        Ok(())
    }

    fn become_leader(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        self.role = Role::Leader;
        self.leader = Some(self.id);
        let next = self.last_index() + 1;
        for member in self.state.configuration.members() {
            if member != self.id {
                self.progress.insert(member, Progress { matched: 0, next, in_flight: None });
            }
        }
        // Only a current-term quorum commits inherited entries. At capacity or
        // absolute-index exhaustion, never infer commitment from older terms.
        if self.state.entries.len() < self.limits.max_log_entries && next < u64::MAX {
            self.log_changed = true;
            self.state.entries.push(Entry { term: self.state.term, command: None });
        }
        self.advance_commit();
        self.broadcast(output)
    }

    fn advance_commit(&mut self) -> bool {
        let candidate = self.state.configuration.quorum_index(|member| {
            if member == self.id {
                self.last_index()
            } else {
                self.progress.get(&member).map_or(0, |progress| progress.matched)
            }
        });
        if candidate > self.state.commit_index && self.term_at(candidate) == Some(self.state.term) {
            self.state.commit_index = candidate;
            true
        } else {
            false
        }
    }

    fn broadcast(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        let peers: Vec<_> = self.progress.keys().copied().collect();
        for peer in peers {
            self.send_append(peer, output)?;
        }
        Ok(())
    }

    fn send_append(&mut self, peer: MemberId, output: &mut Output<C>) -> Result<(), Error> {
        let Some(progress) = self.progress.get(&peer).cloned() else {
            return Ok(());
        };
        let flight = if let Some(flight) = progress.in_flight {
            flight
        } else {
            self.request = self.request.checked_add(1).ok_or(Error::CounterExhausted)?;
            if progress.next <= self.state.base_index() {
                let snapshot = self.state.snapshot.clone().ok_or(Error::InvalidRecoveryState)?;
                InFlight::Snapshot { request: self.request, snapshot }
            } else {
                let prev = progress.next - 1;
                let last = self.last_index().min(prev.saturating_add(self.limits.max_append_entries as u64));
                InFlight::Append { request: self.request, prev, last }
            }
        };
        let message = match &flight {
            InFlight::Append { request, prev, last } => {
                let prev_term = self.term_at(*prev).ok_or(Error::InvalidRecoveryState)?;
                let base = self.state.base_index();
                let entries = self.state.entries[(*prev - base) as usize..(*last - base) as usize].to_vec();
                Message::Append {
                    term: self.state.term,
                    request: *request,
                    prev_index: *prev,
                    prev_term,
                    entries,
                    leader_commit: self.state.commit_index,
                }
            }
            InFlight::Snapshot { request, snapshot } => Message::InstallSnapshot {
                term: self.state.term,
                request: *request,
                snapshot: snapshot.clone(),
            },
        };
        if let Some(progress) = self.progress.get_mut(&peer) {
            progress.in_flight = Some(flight);
        }
        self.emit(peer, message, output);
        Ok(())
    }

    fn compact(&mut self, snapshot: SnapshotCut) {
        if self.state.snapshot.as_ref() == Some(&snapshot) {
            return;
        }
        let count = (snapshot.index - self.state.base_index()) as usize;
        self.state.entries.drain(..count);
        self.state.snapshot = Some(snapshot);
        self.log_changed = true;
        // A retransmission may name a now-retired predecessor or old snapshot.
        // Invalidate request IDs without fabricating any new match evidence.
        for progress in self.progress.values_mut() {
            progress.in_flight = None;
        }
    }

    fn offer_snapshot(
        &mut self,
        from: MemberId,
        term: u64,
        request: u64,
        snapshot: SnapshotCut,
        output: &mut Output<C>,
    ) {
        if term < self.state.term {
            // Use an ordinary term-bearing rejection, never a successful seed ack.
            self.emit(from, Message::Appended {
                term: self.state.term, request, success: false, conflict_next: self.last_index() + 1,
            }, output);
            return;
        }
        self.accept_leader(from, term, output);
        if snapshot.index <= self.state.commit_index {
            // The already-durable committed prefix covers this cut. Never roll
            // back the application root or replay snapshot-covered commands.
            self.emit(from, Message::SnapshotInstalled { term, request }, output);
            return;
        }
        if let Some(pending) = &self.incoming_snapshot {
            if pending.term == term
                && pending.request == request
                && pending.transfer.source == from
                && pending.transfer.snapshot == snapshot
            {
                output.snapshot_transfers.push(pending.transfer.clone());
                return;
            }
            // Old retransmissions must not continually cancel a newer transfer.
            if pending.term == term && pending.transfer.source == from && request < pending.request {
                return;
            }
        }
        let transfer = SnapshotTransfer {
            id: SnapshotTransferId(PersistenceId {
                incarnation: Arc::clone(&self.incarnation),
                generation: self.generation,
            }),
            source: from,
            snapshot,
        };
        self.incoming_snapshot = Some(IncomingSnapshot { transfer: transfer.clone(), term, request });
        output.snapshot_transfers.push(transfer);
    }

    fn install_snapshot(&mut self, output: &mut Output<C>) -> Result<(), Error> {
        let pending = self.incoming_snapshot.take().ok_or(Error::StaleSnapshotTransfer)?;
        let snapshot = pending.transfer.snapshot;
        if snapshot.index <= self.state.commit_index {
            return Err(Error::StaleSnapshotTransfer);
        }
        // Retain the suffix only when the exact included index AND term match.
        // Otherwise every discarded entry is uncommitted (validated cut > commit).
        if self.term_at(snapshot.index) == Some(snapshot.term) {
            let count = (snapshot.index - self.state.base_index()) as usize;
            self.state.entries.drain(..count);
        } else {
            self.state.entries.clear();
        }
        self.state.commit_index = snapshot.index;
        self.state.snapshot = Some(snapshot.clone());
        self.log_changed = true;
        output.installed_snapshot = Some(snapshot);
        self.emit(
            pending.transfer.source,
            Message::SnapshotInstalled { term: self.state.term, request: pending.request },
            output,
        );
        Ok(())
    }

    fn acknowledge(&mut self, from: MemberId, last: u64, output: &mut Output<C>) -> Result<(), Error> {
        if let Some(progress) = self.progress.get_mut(&from) {
            progress.matched = progress.matched.max(last);
            progress.next = progress.matched + 1;
            progress.in_flight = None;
        }
        if self.advance_commit() {
            self.broadcast(output)?;
        } else if last < self.last_index() {
            self.send_append(from, output)?;
        }
        Ok(())
    }

    fn receive(&mut self, envelope: Envelope<C>, output: &mut Output<C>) -> Result<(), Error> {
        let from = envelope.from;
        let term = envelope.message.term();
        if term > self.state.term {
            self.follow(term);
        }
        match envelope.message {
            Message::RequestVote { last_index, last_term, .. } => {
                let granted = term == self.state.term
                    && self.state.configuration.voters.contains(&self.id)
                    && (self.state.voted_for.is_none() || self.state.voted_for == Some(from))
                    && (last_term, last_index) >= (self.last_term(), self.last_index());
                if granted {
                    self.state.voted_for = Some(from);
                    output.reset_election_timer = true;
                }
                self.emit(from, Message::Vote { term: self.state.term, granted }, output);
            }
            Message::Vote { granted, .. } => {
                if term == self.state.term && self.role == Role::Candidate && granted {
                    self.votes.insert(from);
                    if self.state.configuration.quorum(&self.votes) {
                        self.become_leader(output)?;
                    }
                }
            }
            Message::Append { request, prev_index, prev_term, entries, leader_commit, .. } => {
                if term < self.state.term {
                    self.emit(from, Message::Appended {
                        term: self.state.term, request, success: false, conflict_next: self.last_index() + 1,
                    }, output);
                    return Ok(());
                }
                self.accept_leader(from, term, output);
                if self.term_at(prev_index) != Some(prev_term) {
                    let base = self.state.base_index();
                    let mut next = self.last_index().saturating_add(1).min(prev_index.max(1));
                    if prev_index < base {
                        next = base + 1;
                    } else if let Some(conflict_term) = self.term_at(prev_index) {
                        while next > base + 1 && self.term_at(next - 1) == Some(conflict_term) {
                            next -= 1;
                        }
                    }
                    self.emit(from, Message::Appended { term, request, success: false, conflict_next: next }, output);
                    return Ok(());
                }
                let matched = prev_index + entries.len() as u64;
                for (offset, entry) in entries.into_iter().enumerate() {
                    let position = (prev_index - self.state.base_index()) as usize + offset;
                    if self.state.entries.get(position).is_some_and(|local| local.term != entry.term) {
                        self.log_changed = true;
                        self.state.entries.truncate(position);
                    }
                    if position == self.state.entries.len() {
                        self.log_changed = true;
                        self.state.entries.push(entry);
                    }
                }
                // A short append proves only its prefix, not our divergent tail.
                self.state.commit_index = self.state.commit_index.max(leader_commit.min(matched));
                if self.incoming_snapshot.as_ref().is_some_and(|pending| {
                    pending.transfer.snapshot.index <= self.state.commit_index
                }) {
                    self.incoming_snapshot = None;
                }
                self.emit(from, Message::Appended { term, request, success: true, conflict_next: 0 }, output);
            }
            Message::Appended { request, success, conflict_next, .. } => {
                if term != self.state.term || self.role != Role::Leader {
                    return Ok(());
                }
                let Some(progress) = self.progress.get(&from).cloned() else {
                    return Ok(());
                };
                let Some(InFlight::Append { request: expected, prev, last }) = progress.in_flight else {
                    return Ok(());
                };
                if expected != request {
                    return Ok(());
                }
                if success {
                    self.acknowledge(from, last, output)?;
                } else {
                    let ceiling = self.last_index() + 1;
                    if let Some(progress) = self.progress.get_mut(&from) {
                        // A follower that compacted ahead can suggest a forward
                        // probe. This changes next only, never matched/commit.
                        let hint = if conflict_next > prev + 1 {
                            conflict_next.min(ceiling)
                        } else {
                            conflict_next.max(1).min(prev.max(1))
                        };
                        progress.next = hint.max(progress.matched + 1);
                        progress.in_flight = None;
                    }
                    self.send_append(from, output)?;
                }
            }
            Message::InstallSnapshot { request, snapshot, .. } => {
                self.offer_snapshot(from, term, request, snapshot, output);
            }
            Message::SnapshotInstalled { request, .. } => {
                if term != self.state.term || self.role != Role::Leader {
                    return Ok(());
                }
                let Some(InFlight::Snapshot { request: expected, snapshot }) = self
                    .progress.get(&from).and_then(|progress| progress.in_flight.as_ref())
                else {
                    return Ok(());
                };
                if *expected == request {
                    let index = snapshot.index;
                    self.acknowledge(from, index, output)?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod quorum_tests;
