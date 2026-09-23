//! Durability-gated member driver and clock-free, fresh-quorum read barriers.
//!
//! The driver owns its Raft machine from creation/recovery and observes EVERY
//! released outbound request. This is essential: a retransmitted Append can
//! have a delayed pre-read reply, so only request IDs first issued after a read
//! began can confirm it. Existing in-flight appends continue normally; subsequent
//! heartbeats/proposals issue fresh probes when those appends finish. A healthy,
//! quiescent quorum needs one heartbeat round, without a read-only log entry.
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum FlightKind {
    Append,
    Snapshot,
}

#[derive(Clone, Copy)]
struct Sent {
    term: u64,
    request: u64,
    kind: FlightKind,
}

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
    sent: BTreeMap<MemberId, Sent>,
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
            sent: BTreeMap::new(),
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
        let compact = matches!(&event, Event::Compact(_));
        let output = sequence(&mut self.raft, publisher, event)
            .await
            .map_err(ReplicaError::Sequence)?;
        // No await separates durability from observation of the released output.
        if compact {
            self.sent.clear();
        }
        if let Some((member, _, _, _)) = reply {
            self.sent.remove(&member);
        }
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
        let sent = self.sent.get(&envelope.from)?;
        let (term, request, kind, success) = match &envelope.message {
            Message::Appended {
                term,
                request,
                success,
                ..
            } => (*term, *request, FlightKind::Append, *success),
            Message::SnapshotInstalled { term, request } => {
                (*term, *request, FlightKind::Snapshot, false)
            }
            _ => return None,
        };
        (term == sent.term && request == sent.request && kind == sent.kind).then_some((
            envelope.from,
            term,
            request,
            success,
        ))
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
            self.sent.clear();
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
            let sent = match &envelope.message {
                Message::Append { term, request, .. } => Some(Sent {
                    term: *term,
                    request: *request,
                    kind: FlightKind::Append,
                }),
                Message::InstallSnapshot { term, request, .. } => Some(Sent {
                    term: *term,
                    request: *request,
                    kind: FlightKind::Snapshot,
                }),
                _ => None,
            };
            if let Some(sent) = sent {
                self.highest_request = self.highest_request.max(sent.request);
                self.sent.insert(envelope.to, sent);
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
