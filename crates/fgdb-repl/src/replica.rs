//! Durability-gated member driver and clock-free, fresh-quorum read barriers.
//!
//! The driver owns its Raft machine from creation/recovery and observes EVERY
//! released outbound request. This is essential: a retransmitted Append can
//! have a delayed pre-read reply, so only request IDs first issued after a read
//! began can confirm it. Existing in-flight appends continue normally; newly
//! issued pipeline ranges can also confirm a read without draining the window.
//! A healthy, quiescent quorum needs one heartbeat round, without a read-only log entry.
//!
//! A read barrier establishes only a consensus data floor. The application must
//! apply through that floor, wait for audit visibility, pin the chosen snapshot,
//! and enforce fresh Warden authority before observation. It is not a lease,
//! public logical position, payload certificate, or reusable read authorization.
//! The runtime owns authenticated transport, ReplCx deadlines and publication.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

mod download;

use fgdb_chronicle::seed::SeedPlan;
use fgdb_order::{
    Committed, Configuration, Domain, Error as RaftError, Event, Limits, MemberId, Message, Output,
    PersistentState, Raft, Role, SnapshotTransfer,
};
use fgdb_types::DatabaseSecurityNamespaceId;

use crate::driver::{
    RaftPublisher, SeedDriveError, SeedObjectSource, SeedPublisher, SequenceError, sequence,
};
use crate::{CatchupError, SnapshotCatchup};

/// In-process request identity; recovery and another member cannot reuse it.
#[derive(Clone, Debug)]
pub struct ReadIndexId {
    incarnation: Arc<()>,
    serial: u64,
}

impl PartialEq for ReadIndexId {
    fn eq(&self, other: &Self) -> bool {
        self.serial == other.serial && Arc::ptr_eq(&self.incarnation, &other.incarnation)
    }
}
impl Eq for ReadIndexId {}

/// Result for ONE already-admitted read (or read batch admitted before its probe).
/// This value deliberately is not Clone. Do not reuse its floor for a later read.
#[derive(Debug)]
pub struct ReadIndexReady {
    id: ReadIndexId,
    domain: Domain,
    configuration: [u8; 32],
    leader: MemberId,
    term: u64,
    index: u64,
}

impl ReadIndexReady {
    pub fn id(&self) -> &ReadIndexId {
        &self.id
    }
    pub fn domain(&self) -> Domain {
        self.domain
    }
    pub fn configuration(&self) -> [u8; 32] {
        self.configuration
    }
    pub fn leader(&self) -> MemberId {
        self.leader
    }
    pub fn term(&self) -> u64 {
        self.term
    }
    /// Minimum applied DATA position, not permission to skip audit or authority.
    pub fn index(&self) -> u64 {
        self.index
    }
}

#[derive(Debug)]
pub enum ReadResolution {
    Ready(ReadIndexReady),
    /// No read floor was released; retry at the current authority leader.
    LeadershipLost(ReadIndexId),
}

/// Native consensus output remains distinct from application/client visibility.
#[derive(Debug)]
pub struct ReplicaOutput<C> {
    pub consensus: Output<C>,
    pub reads: Vec<ReadResolution>,
}

#[derive(Debug)]
pub enum ReplicaError<E> {
    Raft(RaftError),
    Sequence(SequenceError<E>),
    CurrentTermNotCommitted,
    ReadBackpressure,
    ReadCounterExhausted,
}

impl<E: core::fmt::Debug> core::fmt::Display for ReplicaError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis replica: {self:?}")
    }
}
impl<E: core::fmt::Debug> core::error::Error for ReplicaError<E> {}

struct PendingRead {
    id: ReadIndexId,
    term: u64,
    index: u64,
    after_request: u64,
    confirmations: BTreeSet<MemberId>,
}

/// Owns the complete output history of one Raft machine. No mutable Raft escape
/// hatch exists: missed outputs would invalidate the read-probe freshness fence.
/// Creation/recovery still requires the actual exclusive writer/member fence.
/// Membership is the kernel's fixed authenticated stable/joint configuration;
/// this driver does not authorize live reconfiguration or service promotion.
pub struct Replica<C> {
    raft: Raft<C>,
    highest_request: u64,
    incarnation: Arc<()>,
    next_read: u64,
    maximum_reads: usize,
    reads: BTreeMap<u64, PendingRead>,
}

impl<C: Clone + Eq> Replica<C> {
    pub fn new(
        id: MemberId,
        configuration: Configuration,
        limits: Limits,
        maximum_reads: usize,
    ) -> Result<Self, RaftError> {
        Self::from_new_machine(Raft::new(id, configuration, limits)?, maximum_reads)
    }

    /// The caller authenticates the canonical persisted closure and holds its
    /// exclusive fence. Pending reads are volatile and NEVER restored as ready.
    pub fn recover(
        id: MemberId,
        state: PersistentState<C>,
        limits: Limits,
        maximum_reads: usize,
    ) -> Result<Self, RaftError> {
        Self::from_new_machine(Raft::recover(id, state, limits)?, maximum_reads)
    }

    fn from_new_machine(raft: Raft<C>, maximum_reads: usize) -> Result<Self, RaftError> {
        if maximum_reads == 0 || maximum_reads > 1024 {
            return Err(RaftError::InvalidLimits);
        }
        Ok(Self {
            raft,
            highest_request: 0,
            incarnation: Arc::new(()),
            next_read: 0,
            maximum_reads,
            reads: BTreeMap::new(),
        })
    }

    pub fn id(&self) -> MemberId {
        self.raft.id()
    }
    pub fn role(&self) -> Result<Role, RaftError> {
        self.raft.role()
    }
    pub fn durable_state(&self) -> Result<&PersistentState<C>, RaftError> {
        self.raft.durable_state()
    }
    pub fn committed_after(&self, applied: u64) -> Result<Vec<Committed<C>>, RaftError> {
        self.raft.committed_after(applied)
    }
    pub fn pending_reads(&self) -> usize {
        self.reads.len()
    }

    /// Preflight the entire group before payload authority acquisition. This
    /// grants no reservation, permission or durable acknowledgement.
    pub fn check_proposal_count(&self, count: usize) -> Result<(), RaftError> {
        self.raft.check_proposal_count(count)
    }

    /// Observe the handoff started with step(TransferLeadership). Its exact ID
    /// may be used with AbortLeadershipTransfer for a local deadline. Continue
    /// heartbeat/liveness delivery; a missing attempt is not election success.
    pub fn leadership_transfer(&self) -> Result<Option<&fgdb_order::LeadershipTransfer>, RaftError> {
        self.raft.leadership_transfer()
    }

    /// Configure bounded pipelining before campaigning. The kernel refuses
    /// reconfiguration of a leader, candidate or unpublished transition. Keep
    /// this owner for all subsequent outputs: read freshness uses every issued
    /// append identity, not just the last request sent to each peer.
    pub fn configure_append_pipeline(&mut self, maximum: usize) -> Result<(), RaftError> {
        self.raft.configure_append_pipeline(maximum)
    }

    pub fn append_pipeline_window(&self) -> usize {
        self.raft.append_pipeline_window()
    }

    /// Local request cancellation affects no log entry or durable obligation.
    pub fn cancel_read(&mut self, id: &ReadIndexId) -> bool {
        Arc::ptr_eq(&id.incarnation, &self.incarnation) && self.reads.remove(&id.serial).is_some()
    }

    /// Drive one input through the ordinary immutable-root publication gate.
    /// Deliver returned messages independently; a failed minority must not block
    /// the majority. Continue normal heartbeats while read barriers are pending.
    /// A failed/ambiguous publication fences the machine, including its pending
    /// reads. Recover the actual root and retry reads as new requests; retained
    /// volatile read IDs are never evidence of completion after such a failure.
    pub async fn step<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
        event: Event<C>,
    ) -> Result<ReplicaOutput<C>, ReplicaError<P::Error>> {
        let reply = self.matching_reply(&event);
        let output = sequence(&mut self.raft, publisher, event)
            .await
            .map_err(ReplicaError::Sequence)?;
        // No await separates durability from observation of the released output.
        self.observe(output, reply).map_err(ReplicaError::Raft)
    }

    /// Admit a read only after a current-term entry is durably committed. Record
    /// its data floor BEFORE probing, and never coalesce it into an older round.
    /// Each pending read has an independent freshness watermark and voter set.
    /// A later fresh acknowledgement may safely finish several earlier reads.
    pub async fn read_index<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
    ) -> Result<(ReadIndexId, ReplicaOutput<C>), ReplicaError<P::Error>> {
        if self.raft.role().map_err(ReplicaError::Raft)? != Role::Leader {
            return Err(ReplicaError::Raft(RaftError::NotLeader));
        }
        let state = self.raft.durable_state().map_err(ReplicaError::Raft)?;
        let index = state.commit_index();
        let base = state.snapshot().map_or(0, |cut| cut.index());
        let committed_term = if index == base {
            state.snapshot().map_or(0, |cut| cut.term())
        } else {
            state.entries()[(index - base - 1) as usize].term
        };
        if committed_term != state.term() {
            return Err(ReplicaError::CurrentTermNotCommitted);
        }
        if self.reads.len() >= self.maximum_reads {
            return Err(ReplicaError::ReadBackpressure);
        }
        let serial = self
            .next_read
            .checked_add(1)
            .ok_or(ReplicaError::ReadCounterExhausted)?;
        let id = ReadIndexId {
            incarnation: Arc::clone(&self.incarnation),
            serial,
        };
        let read = PendingRead {
            id: id.clone(),
            term: state.term(),
            index,
            after_request: self.highest_request,
            confirmations: BTreeSet::from([self.raft.id()]),
        };
        self.next_read = serial;
        self.reads.insert(serial, read);
        let mut admission = ReadAdmission {
            replica: self,
            serial,
            armed: true,
        };
        let output = admission.replica.step(publisher, Event::Heartbeat).await?;
        admission.armed = false;
        Ok((id, output))
    }

    /// Use the existing bonded-source and atomic application/Raft seed driver.
    /// Decoding is never an install acknowledgement. An uncertain root outcome
    /// fences this owned member through SnapshotCatchup's cancellation guard.
    pub async fn install_snapshot<S: SeedObjectSource, P: SeedPublisher<C>>(
        &mut self,
        namespace: DatabaseSecurityNamespaceId,
        transfer: SnapshotTransfer,
        plan: SeedPlan,
        source: &mut S,
        publisher: &mut P,
    ) -> Result<ReplicaOutput<C>, SeedDriveError<S::Error, P::Error>> {
        let catchup = SnapshotCatchup::begin(&mut self.raft, namespace, transfer, plan)
            .map_err(SeedDriveError::Catchup)?;
        let output = catchup.install(source, publisher).await?;
        self.observe(output, None)
            .map_err(|error| SeedDriveError::Catchup(CatchupError::Raft(error)))
    }

    // The kernel validates the envelope and releases output only after required
    // persistence. Matching a sent identity alone is never enough to count it.
    fn matching_reply(&self, event: &Event<C>) -> Option<(MemberId, u64, u64, bool)> {
        let Event::Receive(envelope) = event else {
            return None;
        };
        let Message::Appended {
            term, request, success, ..
        } = &envelope.message else {
            return None;
        };
        // The kernel owns the whole bounded window and its retirement rules.
        // Keeping only a peer's last request loses earlier valid confirmations;
        // retaining a second window ledger risks reviving invalidated requests.
        // Snapshot and quorum-probe replies never satisfy this append query.
        self.raft
            .pending_append_reply(envelope.from, *term, *request)
            .ok()?
            .then_some((envelope.from, *term, *request, *success))
    }

    fn observe(
        &mut self,
        output: Output<C>,
        reply: Option<(MemberId, u64, u64, bool)>,
    ) -> Result<ReplicaOutput<C>, RaftError> {
        let mut resolutions = Vec::new();
        let state = self.raft.durable_state()?;
        let configuration = state.configuration();
        if output.role != Role::Leader {
            for (_, read) in std::mem::take(&mut self.reads) {
                resolutions.push(ReadResolution::LeadershipLost(read.id));
            }
        } else {
            if let Some((member, term, request, true)) = reply {
                if term == state.term() && configuration.voters().contains(&member) {
                    for read in self.reads.values_mut() {
                        if read.term == term && request > read.after_request {
                            read.confirmations.insert(member);
                        }
                    }
                }
            }
            let ready: Vec<_> = self
                .reads
                .iter()
                .filter_map(|(serial, read)| {
                    (read.term == state.term() && quorum(configuration, &read.confirmations))
                        .then_some(*serial)
                })
                .collect();
            for serial in ready {
                if let Some(read) = self.reads.remove(&serial) {
                    resolutions.push(ReadResolution::Ready(ReadIndexReady {
                        id: read.id,
                        domain: configuration.domain(),
                        configuration: configuration.identity(),
                        leader: self.raft.id(),
                        term: read.term,
                        index: read.index,
                    }));
                }
            }
        }
        for envelope in &output.messages {
            if let Message::Append { request, .. } | Message::InstallSnapshot { request, .. } =
                &envelope.message
            {
                self.highest_request = self.highest_request.max(*request);
            }
        }
        Ok(ReplicaOutput {
            consensus: output,
            reads: resolutions,
        })
    }
}

fn quorum(configuration: &Configuration, confirmations: &BTreeSet<MemberId>) -> bool {
    let majority =
        |voters: &BTreeSet<MemberId>| voters.intersection(confirmations).count() > voters.len() / 2;
    match configuration.joint_voters() {
        Some((old, new)) => majority(old) && majority(new),
        None => majority(configuration.voters()),
    }
}

struct ReadAdmission<'a, C> {
    replica: &'a mut Replica<C>,
    serial: u64,
    armed: bool,
}

impl<C> Drop for ReadAdmission<'_, C> {
    fn drop(&mut self) {
        if self.armed {
            self.replica.reads.remove(&self.serial);
        }
    }
}
