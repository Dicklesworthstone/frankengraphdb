//! Bounded, incarnation-scoped waits for payload-authorized local proposals.
//!
//! This is volatile orchestration, not the canonical idempotency/outcome ledger.
//! A visible result establishes the data/application/audit gates for one admitted
//! invocation only. Fresh authorization, canonical outcome publication and safe
//! delivery remain required before reporting client success. Cancellation and
//! Unknown never abort an ordered command or retire prepared payload ownership.

use std::sync::Arc;

use fgdb_order::{PersistentState, Role};

use super::proposal::batch::{
    BatchProposalError, BatchProposalLimits, MemberBatchProposalOutput, PayloadBatchAuthority,
    ProposalRange,
};
use super::proposal::{MemberProposalError, MemberProposalOutput};
use super::{Application, ApplicationProgress, ApplicationStateError, AppliedReplica, position_at};
use crate::application::AppliedPosition;
use crate::availability::proposal::{PayloadProposalAuthority, ProposalPosition};
use crate::availability::{AvailabilityInput, AvailabilityLimits};
use crate::driver::RaftPublisher;

/// Local invocation identity. Retaining it prevents address reuse after restart
/// from making an old invocation valid in a newly recovered member.
#[derive(Clone, Debug)]
pub struct WriteId {
    incarnation: Arc<()>,
    serial: u64,
}

impl PartialEq for WriteId {
    fn eq(&self, other: &Self) -> bool {
        self.serial == other.serial && Arc::ptr_eq(&self.incarnation, &other.incarnation)
    }
}
impl Eq for WriteId {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteError {
    Member(ApplicationStateError),
    InvalidLimit,
    Backpressure,
    CounterExhausted,
    AllocationFailed,
    UnknownInvocation,
    HistoryMismatch,
}
impl core::fmt::Display for WriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis write wait: {self:?}")
    }
}
impl core::error::Error for WriteError {}

#[derive(Debug)]
pub enum SubmitError<A, I> {
    Admission(WriteError),
    Proposal(MemberProposalError<A, I>),
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::fmt::Display for SubmitError<A, I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis tracked proposal: {self:?}")
    }
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::error::Error for SubmitError<A, I> {}

/// Dispatch the output before waiting. This is not a client commit receipt.
#[derive(Debug)]
pub struct WriteSubmission<C> {
    pub id: WriteId,
    pub output: MemberProposalOutput<C>,
}

/// One invocation per command, in the same order as the locally published
/// group. Use ordinary try_write/cancel_write for each ID; there is deliberately
/// no aggregate success bit that could conceal partial quorum commitment.
#[derive(Debug)]
pub struct WriteBatchSubmission<C> {
    pub ids: Vec<WriteId>,
    pub output: MemberBatchProposalOutput<C>,
}

#[derive(Debug)]
pub enum BatchSubmitError<A, I> {
    Admission(WriteError),
    Proposal(BatchProposalError<A, I>),
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::fmt::Display for BatchSubmitError<A, I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis tracked batch: {self:?}")
    }
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::error::Error for BatchSubmitError<A, I> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnknownReason {
    LeadershipLost,
    RecoveryRequired,
    HistoryUnavailable,
}

/// No negative transaction conclusion follows from this result. Resolve the
/// original operation through the canonical authorized outcome/idempotency path.
#[derive(Debug)]
pub struct UnknownWrite {
    id: WriteId,
    reason: UnknownReason,
}
impl UnknownWrite {
    pub fn id(&self) -> &WriteId {
        &self.id
    }
    pub fn reason(&self) -> UnknownReason {
        self.reason
    }
}

/// One consumed invocation's verified progress floor, NOT a read view, lease,
/// logical sequence, signed receipt, Warden permit or public success response.
#[derive(Debug)]
pub struct VisibleWrite {
    id: WriteId,
    position: ProposalPosition,
    progress: ApplicationProgress,
}
impl VisibleWrite {
    pub fn id(&self) -> &WriteId {
        &self.id
    }
    pub fn position(&self) -> ProposalPosition {
        self.position
    }
    pub fn progress(&self) -> ApplicationProgress {
        self.progress
    }
}

#[derive(Debug)]
pub enum WriteState {
    PendingCommit { required: u64, committed: u64 },
    PendingApplication { required: u64, applied: u64 },
    PendingAudit { required: u64, visible: u64 },
    Visible(VisibleWrite),
    Unknown(UnknownWrite),
}

struct PendingWrite {
    id: WriteId,
    position: Option<ProposalPosition>,
    committed: bool,
    lost: bool,
}

pub(super) struct WriteTracker {
    incarnation: Arc<()>,
    serial: u64,
    limit: usize,
    pending: Vec<PendingWrite>,
}
impl WriteTracker {
    pub(super) fn new() -> Self {
        Self {
            incarnation: Arc::new(()),
            serial: 0,
            limit: 1024,
            pending: Vec::new(),
        }
    }
    fn find(&self, id: &WriteId) -> Option<usize> {
        if !Arc::ptr_eq(&self.incarnation, &id.incarnation) {
            return None;
        }
        self.pending.iter().position(|write| write.id == *id)
    }
    fn reserve(&mut self) -> Result<WriteId, WriteError> {
        if self.pending.len() >= self.limit {
            return Err(WriteError::Backpressure);
        }
        let serial = self
            .serial
            .checked_add(1)
            .ok_or(WriteError::CounterExhausted)?;
        self.pending
            .try_reserve(1)
            .map_err(|_| WriteError::AllocationFailed)?;
        let id = WriteId {
            incarnation: Arc::clone(&self.incarnation),
            serial,
        };
        self.serial = serial;
        self.pending.push(PendingWrite {
            id: id.clone(),
            position: None,
            committed: false,
            lost: false,
        });
        Ok(id)
    }
    fn remove(&mut self, id: &WriteId) -> bool {
        if let Some(index) = self.find(id) {
            self.pending.remove(index);
            true
        } else {
            false
        }
    }
    fn reserve_group(&mut self, count: usize) -> Result<Vec<WriteId>, WriteError> {
        if count == 0 {
            return Err(WriteError::InvalidLimit);
        }
        if count > self.limit.saturating_sub(self.pending.len()) {
            return Err(WriteError::Backpressure);
        }
        let count_u64 = u64::try_from(count).map_err(|_| WriteError::CounterExhausted)?;
        let last = self
            .serial
            .checked_add(count_u64)
            .ok_or(WriteError::CounterExhausted)?;
        // Reserve BOTH output IDs and tracker slots before consuming a serial
        // or installing any waiter. Nothing below allocates or calls user code.
        let mut ids = Vec::new();
        ids.try_reserve_exact(count)
            .map_err(|_| WriteError::AllocationFailed)?;
        self.pending
            .try_reserve(count)
            .map_err(|_| WriteError::AllocationFailed)?;
        let previous = self.serial;
        self.serial = last;
        for offset in 1..=count_u64 {
            let id = WriteId {
                incarnation: Arc::clone(&self.incarnation),
                serial: previous + offset,
            };
            self.pending.push(PendingWrite {
                id: id.clone(),
                position: None,
                committed: false,
                lost: false,
            });
            ids.push(id);
        }
        Ok(ids)
    }
    // Only GroupAdmission's complete, ordered reserve_group result reaches
    // this helper. Remove its exact contiguous serial interval in one pass.
    fn remove_group(&mut self, ids: &[WriteId]) {
        let (Some(first), Some(last)) = (ids.first(), ids.last()) else {
            return;
        };
        if !Arc::ptr_eq(&first.incarnation, &self.incarnation) {
            return;
        }
        self.pending
            .retain(|write| write.id.serial < first.serial || write.id.serial > last.serial);
    }
    fn bind_group(&mut self, ids: &[WriteId], positions: ProposalRange) -> Result<(), WriteError> {
        if ids.len() != positions.len() {
            return Err(WriteError::HistoryMismatch);
        }
        let first = ids.first().ok_or(WriteError::HistoryMismatch)?;
        let start = self.find(first).ok_or(WriteError::HistoryMismatch)?;
        let end = start
            .checked_add(ids.len())
            .ok_or(WriteError::HistoryMismatch)?;
        let slots = self
            .pending
            .get_mut(start..end)
            .ok_or(WriteError::HistoryMismatch)?;
        if slots
            .iter()
            .zip(ids)
            .any(|(slot, id)| slot.id != *id || slot.position.is_some())
        {
            return Err(WriteError::HistoryMismatch);
        }
        for (offset, slot) in slots.iter_mut().enumerate() {
            slot.position = Some(
                positions
                    .position(offset)
                    .ok_or(WriteError::HistoryMismatch)?,
            );
        }
        Ok(())
    }
    pub(super) fn observe<C>(&mut self, state: &PersistentState<C>, role: Role) {
        for write in &mut self.pending {
            let Some(position) = write.position else {
                continue;
            };
            if role != Role::Leader
                || position.term != state.term()
                || position.domain != state.configuration().domain()
                || position.configuration != state.configuration().identity()
            {
                write.lost = true;
            }
            // Capture exact durable commitment BEFORE any later compaction.
            // A saved index alone, or a newly installed higher snapshot, is not
            // evidence that this invocation's entry was ever committed.
            if !write.lost
                && !write.committed
                && state.commit_index() >= position.index
                && position_at(state, position.index)
                    == Some(AppliedPosition {
                        index: position.index,
                        term: position.term,
                    })
            {
                write.committed = true;
            }
        }
    }
    fn unknown(&mut self, index: usize, reason: UnknownReason) -> WriteState {
        WriteState::Unknown(UnknownWrite {
            id: self.pending.remove(index).id,
            reason,
        })
    }
}

impl<C: Clone + Eq, A: Application<C> + RaftPublisher<C>> AppliedReplica<C, A> {
    /// Bound all write waits together, including quorum-ready apply/audit waits
    /// and unconsumed Unknown results. Read admission has its own independent
    /// bound. Lowering a limit may not evict invocations already admitted.
    pub fn set_write_limit(&mut self, maximum: usize) -> Result<(), WriteError> {
        if maximum == 0 || maximum > 1024 {
            return Err(WriteError::InvalidLimit);
        }
        if maximum < self.writes.pending.len() {
            return Err(WriteError::Backpressure);
        }
        self.writes.limit = maximum;
        Ok(())
    }
    pub fn pending_writes(&self) -> usize {
        self.writes.pending.len()
    }

    /// Cancel only the local waiter. The command may still commit and apply.
    /// Spent serials and durable prepared ownership are never reset or refunded.
    pub fn cancel_write(&mut self, id: &WriteId) -> bool {
        self.writes.remove(id)
    }

    /// Poll an original invocation without I/O, cloning payloads, or waiting
    /// inside consensus. Pending states keep their admission slot. Visible and
    /// Unknown consume it exactly once. A fenced member returns Unknown rather
    /// than misclassifying uncertain publication/application as a rejected write.
    ///
    /// Commitment observed through this member's exact persisted output history
    /// survives log compaction. Leadership loss is sticky, even if this member
    /// is later reelected. Recovery starts a new identity space; old IDs cannot
    /// complete by matching a reused numeric index in a new incarnation.
    pub fn try_write(&mut self, id: &WriteId) -> Result<WriteState, WriteError> {
        let index = self.writes.find(id).ok_or(WriteError::UnknownInvocation)?;
        if self.available().is_err() {
            return Ok(self.writes.unknown(index, UnknownReason::RecoveryRequired));
        }
        let state = self
            .replica
            .durable_state()
            .map_err(|error| WriteError::Member(ApplicationStateError::Raft(error)))?;
        let role = self
            .replica
            .role()
            .map_err(|error| WriteError::Member(ApplicationStateError::Raft(error)))?;
        self.writes.observe(state, role);
        let write = &self.writes.pending[index];
        if write.lost {
            return Ok(self.writes.unknown(index, UnknownReason::LeadershipLost));
        }
        let position = write.position.ok_or(WriteError::HistoryMismatch)?;
        if !write.committed {
            if position_at(state, position.index)
                != Some(AppliedPosition {
                    index: position.index,
                    term: position.term,
                })
            {
                return Ok(self
                    .writes
                    .unknown(index, UnknownReason::HistoryUnavailable));
            }
            return Ok(WriteState::PendingCommit {
                required: position.index,
                committed: state.commit_index(),
            });
        }
        let progress = self.application.progress;
        if progress.applied.index < position.index {
            return Ok(WriteState::PendingApplication {
                required: position.index,
                applied: progress.applied.index,
            });
        }
        if progress.visible_index < position.index {
            return Ok(WriteState::PendingAudit {
                required: position.index,
                visible: progress.visible_index,
            });
        }
        Ok(WriteState::Visible(VisibleWrite {
            id: self.writes.pending.remove(index).id,
            position,
            progress,
        }))
    }
}

impl<C, A> AppliedReplica<C, A>
where
    C: Clone + Eq,
    A: Application<C> + RaftPublisher<C> + PayloadProposalAuthority<C>,
{
    /// Reserve one bounded waiter BEFORE any authority acquisition or append,
    /// then use the existing same-backend proposal path. Cancellation/error/panic
    /// removes the reservation but never undoes publication. An error after the
    /// append starts is not a negative outcome; recover the canonical operation.
    /// No await separates successful append from recording its exact position.
    pub async fn submit_available<I, F>(
        &mut self,
        command: C,
        input: &AvailabilityInput,
        limits: AvailabilityLimits,
        checkpoint: &mut F,
    ) -> Result<WriteSubmission<C>, SubmitError<<A as PayloadProposalAuthority<C>>::Error, I>>
    where
        F: FnMut() -> Result<(), I>,
    {
        self.available()
            .map_err(|error| SubmitError::Admission(WriteError::Member(error)))?;
        let id = self.writes.reserve().map_err(SubmitError::Admission)?;
        let mut admission = Admission {
            member: self,
            id: id.clone(),
            armed: true,
        };
        let output = admission
            .member
            .propose_available(command, input, limits, checkpoint)
            .await
            .map_err(SubmitError::Proposal)?;
        let index = admission
            .member
            .writes
            .find(&id)
            .ok_or(SubmitError::Admission(WriteError::HistoryMismatch))?;
        admission.member.writes.pending[index].position = Some(output.position);
        let state = admission.member.replica.durable_state().map_err(|error| {
            SubmitError::Admission(WriteError::Member(ApplicationStateError::Raft(error)))
        })?;
        admission
            .member
            .writes
            .observe(state, output.member.consensus.role);
        admission.armed = false;
        Ok(WriteSubmission { id, output })
    }
}

struct Admission<'a, C, A> {
    member: &'a mut AppliedReplica<C, A>,
    id: WriteId,
    armed: bool,
}
impl<C, A> Drop for Admission<'_, C, A> {
    fn drop(&mut self) {
        if self.armed {
            self.member.writes.remove(&self.id);
        }
    }
}

impl<C, A> AppliedReplica<C, A>
where
    C: Clone + Eq,
    A: Application<C> + RaftPublisher<C> + PayloadBatchAuthority<C>,
{
    /// Reserve the entire set of invocation slots before assessment or group
    /// authority acquisition. Scalar and grouped waits share ONE admission
    /// limit, including committed/audit-hidden and unconsumed Unknown waiters.
    ///
    /// All entries are bound to their exact positions before returning output.
    /// Failed/cancelled acquisition retires every reservation; uncertain append
    /// retains the member's recovery fence. Spent serials are never refunded.
    /// This neither aborts commands nor releases prepared payload ownership.
    pub async fn submit_batch_available<I, F>(
        &mut self,
        commands: Vec<C>,
        inputs: &[AvailabilityInput],
        limits: BatchProposalLimits,
        checkpoint: &mut F,
    ) -> Result<WriteBatchSubmission<C>, BatchSubmitError<<A as PayloadBatchAuthority<C>>::Error, I>>
    where
        F: FnMut() -> Result<(), I>,
    {
        self.available()
            .map_err(|error| BatchSubmitError::Admission(WriteError::Member(error)))?;
        self.replica
            .check_proposal_count(commands.len())
            .map_err(|error| {
                BatchSubmitError::Admission(WriteError::Member(ApplicationStateError::Raft(error)))
            })?;
        let ids = self
            .writes
            .reserve_group(commands.len())
            .map_err(BatchSubmitError::Admission)?;
        let mut admission = GroupAdmission {
            member: self,
            ids,
            armed: true,
        };
        let output = admission
            .member
            .propose_batch_available(commands, inputs, limits, checkpoint)
            .await
            .map_err(BatchSubmitError::Proposal)?;
        if let Err(error) = admission
            .member
            .writes
            .bind_group(&admission.ids, output.positions)
        {
            // The append has already happened. An inconsistent output history
            // cannot be retried as a known pre-publication refusal.
            admission.member.application.poisoned = true;
            return Err(BatchSubmitError::Admission(error));
        }
        let state = admission.member.replica.durable_state().map_err(|error| {
            BatchSubmitError::Admission(WriteError::Member(ApplicationStateError::Raft(error)))
        })?;
        admission
            .member
            .writes
            .observe(state, output.member.consensus.role);
        admission.armed = false;
        Ok(WriteBatchSubmission {
            ids: std::mem::take(&mut admission.ids),
            output,
        })
    }
}

struct GroupAdmission<'a, C, A> {
    member: &'a mut AppliedReplica<C, A>,
    ids: Vec<WriteId>,
    armed: bool,
}
impl<C, A> Drop for GroupAdmission<'_, C, A> {
    fn drop(&mut self) {
        if self.armed {
            self.member.writes.remove_group(&self.ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exhausted_serial_never_wraps_or_reserves_a_slot() {
        let mut tracker = WriteTracker::new();
        tracker.serial = u64::MAX;
        assert_eq!(tracker.reserve(), Err(WriteError::CounterExhausted));
        assert!(tracker.pending.is_empty());
    }

    #[test]
    fn group_reservation_is_all_or_none_at_capacity_and_counter_boundaries() {
        let mut tracker = WriteTracker::new();
        tracker.limit = 3;
        let existing = tracker.reserve().unwrap();
        assert_eq!(tracker.reserve_group(3), Err(WriteError::Backpressure));
        assert_eq!(tracker.serial, existing.serial);
        assert_eq!(tracker.pending.len(), 1);
        let group = tracker.reserve_group(2).unwrap();
        tracker.remove_group(&group);
        assert_eq!(tracker.pending.len(), 1);
        assert_eq!(tracker.pending[0].id, existing);
        let spent = tracker.serial;
        assert_eq!(tracker.reserve().unwrap().serial, spent + 1);

        let mut tracker = WriteTracker::new();
        tracker.serial = u64::MAX - 1;
        assert_eq!(tracker.reserve_group(2), Err(WriteError::CounterExhausted));
        assert!(tracker.pending.is_empty());
        assert_eq!(tracker.serial, u64::MAX - 1);
        let last = tracker.reserve_group(1).unwrap();
        assert_eq!(last[0].serial, u64::MAX);
        tracker.remove_group(&last);
        assert_eq!(tracker.reserve_group(1), Err(WriteError::CounterExhausted));
        assert_eq!(tracker.reserve_group(0), Err(WriteError::InvalidLimit));
    }
}
