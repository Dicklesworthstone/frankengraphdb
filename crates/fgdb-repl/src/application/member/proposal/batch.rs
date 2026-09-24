//! Independently assessed payloads, one scoped authority, one local publication.
//!
//! This composes the existing ProposeBatch evaluator, not a second sequencer.
//! The authority must validate the ORDERED group, including each command's
//! predecessor and exact position. Sharing an earlier base is not sufficient.
//! A group is not a transaction: quorum commitment may stop inside its range.

use std::future::Future;

use super::{Application, ApplicationStateError, AppliedReplica, AppliedReplicaOutput};
use crate::availability::proposal::ProposalPosition;
use crate::availability::{
    AvailabilityError, AvailabilityInput, AvailabilityLimits, StorageSets, SystematicAssessment,
    assess_systematic,
};
use crate::driver::RaftPublisher;
use crate::replica::ReplicaError;
use fgdb_order::{Error as RaftError, Event};

/// Per-command input/compute ceilings plus shared calculation budgets. Retained
/// input is bounded by max_commands times the individual input ceilings; work
/// and failure enumeration are charged cumulatively, not reset per command.
/// Command bytes, signature work and backend allocations need separate host
/// admission. These limits do not authorize constructing a canonical group.
#[derive(Clone, Copy, Debug)]
pub struct BatchProposalLimits {
    pub max_commands: usize,
    pub per_command: AvailabilityLimits,
    pub max_work: u64,
    pub max_failure_cases: u64,
}
impl Default for BatchProposalLimits {
    fn default() -> Self {
        let per_command = AvailabilityLimits::default();
        Self {
            max_commands: 128,
            max_work: per_command.max_work,
            max_failure_cases: per_command.max_failure_cases,
            per_command,
        }
    }
}

/// Internal ordered positions, not a client receipt or a reusable authority.
/// Construction checks the complete range before exposing these coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalRange {
    first: ProposalPosition,
    count: usize,
}
impl ProposalRange {
    pub fn first(&self) -> ProposalPosition {
        self.first
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    pub fn position(&self, offset: usize) -> Option<ProposalPosition> {
        if offset >= self.count {
            return None;
        }
        Some(ProposalPosition {
            index: self.first.index + offset as u64,
            ..self.first
        })
    }
}

/// Borrowed, exactly ordered command/assessment pairs. Only this driver creates
/// the view, after every calculation succeeds. It is not a batch certificate.
pub struct AssessedBatch<'a, C> {
    commands: &'a [C],
    assessments: &'a [SystematicAssessment<'a>],
    range: ProposalRange,
}
impl<'a, C> AssessedBatch<'a, C> {
    pub fn range(&self) -> ProposalRange {
        self.range
    }
    pub fn entries(
        &self,
    ) -> impl ExactSizeIterator<Item = (ProposalPosition, &C, &SystematicAssessment<'a>)> {
        let first = self.range.first;
        self.commands.iter().zip(self.assessments).enumerate().map(
            move |(offset, (command, assessment))| {
                (
                    ProposalPosition {
                        index: first.index + offset as u64,
                        ..first
                    },
                    command,
                    assessment,
                )
            },
        )
    }
}

/// The owned backend must implement this explicitly; a scalar permit cannot
/// authorize a whole batch and there is deliberately no permissive fallback.
///
/// Authenticate every canonical command/certificate/receipt/ownership/key closure
/// under the same requirements as PayloadProposalAuthority. Check exact input
/// equality, and validate each command at its actual ORDERED predecessor, not
/// independently at a shared stale basis. Refuse configuration, authority or
/// preparation barriers that cannot be grouped without an intervening apply.
///
/// Acquire only read/verify and ephemeral custody. Do not append, sign a new
/// certificate, mutate prepared roots, or release ownership in this method.
/// The returned publisher holds ALL accepted pins and the SAME store/writer
/// fence, checking every freshness/authority guard again at publication. It
/// must publish exactly this complete ordered group, not only the last command.
/// Its lifetime spans the await; dropping it releases only local reservations.
/// Work remains in the owning narrowed-Cx region with no detached tasks.
///
/// This is the mandatory canonical verifier/backend boundary, not a production
/// implementation of signatures, writer fencing or durable closure construction.
pub trait PayloadBatchAuthority<C> {
    type Error;
    type Permit<'a>: RaftPublisher<C, Error = Self::Error>
    where
        Self: 'a;

    fn acquire_batch<'a>(
        &'a mut self,
        batch: AssessedBatch<'_, C>,
    ) -> impl Future<Output = Result<Self::Permit<'a>, Self::Error>>;
}

#[derive(Debug)]
pub enum BatchProposalError<A, I> {
    State(ApplicationStateError),
    Raft(RaftError),
    InvalidLimits,
    CommandBudget,
    InputCount,
    AllocationFailed,
    WrongBasis {
        offset: usize,
    },
    WrongStorageForm {
        offset: usize,
    },
    Availability {
        offset: usize,
        error: AvailabilityError<I>,
    },
    WorkBudget,
    FailureCaseBudget,
    Authority(A),
    Interrupted(I),
    Replica(ReplicaError<A>),
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::fmt::Display for BatchProposalError<A, I> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis payload-authorized batch: {self:?}")
    }
}
impl<A: core::fmt::Debug, I: core::fmt::Debug> core::error::Error for BatchProposalError<A, I> {}

/// One local append publication. Dispatch every consensus message before
/// waiting; individual entries can subsequently commit/apply in separate slices.
#[derive(Debug)]
pub struct MemberBatchProposalOutput<C> {
    pub positions: ProposalRange,
    pub member: AppliedReplicaOutput<C>,
}

impl<C, A> AppliedReplica<C, A>
where
    C: Clone + Eq,
    A: Application<C> + RaftPublisher<C> + PayloadBatchAuthority<C>,
{
    /// Refuse the complete range before calculation or authority acquisition,
    /// assess EVERY command independently, then hold one same-backend permit
    /// across the ordinary durability gate. No entry is submitted on a partial
    /// assessment/acquisition failure. No command or input is cloned here.
    ///
    /// Successful append does not imply quorum, atomic client transaction,
    /// application, audit visibility or permission to deliver client success.
    /// Once publication starts, errors/panics/cancellation retain the existing
    /// recovery fence and cannot be retried as a known transaction rejection.
    pub async fn propose_batch_available<I, F>(
        &mut self,
        commands: Vec<C>,
        inputs: &[AvailabilityInput],
        limits: BatchProposalLimits,
        checkpoint: &mut F,
    ) -> Result<
        MemberBatchProposalOutput<C>,
        BatchProposalError<<A as PayloadBatchAuthority<C>>::Error, I>,
    >
    where
        F: FnMut() -> Result<(), I>,
    {
        self.available().map_err(BatchProposalError::State)?;
        self.replica
            .check_proposal_count(commands.len())
            .map_err(BatchProposalError::Raft)?;
        if limits.max_commands == 0
            || limits.max_commands > 1024
            || limits.max_work == 0
            || limits.max_failure_cases == 0
        {
            return Err(BatchProposalError::InvalidLimits);
        }
        if commands.len() > limits.max_commands {
            return Err(BatchProposalError::CommandBudget);
        }
        if commands.len() != inputs.len() {
            return Err(BatchProposalError::InputCount);
        }
        let state = self
            .replica
            .durable_state()
            .map_err(BatchProposalError::Raft)?;
        let config = state.configuration();
        let first_index = state
            .snapshot()
            .map_or(0, |cut| cut.index())
            .checked_add(
                u64::try_from(state.entries().len())
                    .map_err(|_| BatchProposalError::Raft(RaftError::CounterExhausted))?,
            )
            .and_then(|last| last.checked_add(1))
            .ok_or(BatchProposalError::Raft(RaftError::CounterExhausted))?;
        let positions = ProposalRange {
            first: ProposalPosition {
                member: self.replica.id(),
                domain: config.domain(),
                configuration: config.identity(),
                term: state.term(),
                index: first_index,
            },
            count: commands.len(),
        };
        // Inspect every basis before any potentially expensive assessment.
        for (offset, input) in inputs.iter().enumerate() {
            checkpoint().map_err(BatchProposalError::Interrupted)?;
            if input.policy.basis.domain != config.domain()
                || input.policy.basis.configuration != config.identity()
            {
                return Err(BatchProposalError::WrongBasis { offset });
            }
            if matches!(&input.policy.storage_sets, StorageSets::Joint { .. })
                != config.joint_voters().is_some()
            {
                return Err(BatchProposalError::WrongStorageForm { offset });
            }
        }
        let mut assessments = Vec::new();
        assessments
            .try_reserve_exact(commands.len())
            .map_err(|_| BatchProposalError::AllocationFailed)?;
        let mut work = limits.max_work;
        let mut cases = limits.max_failure_cases;
        for (offset, input) in inputs.iter().enumerate() {
            if work == 0 {
                return Err(BatchProposalError::WorkBudget);
            }
            if cases == 0 {
                return Err(BatchProposalError::FailureCaseBudget);
            }
            let individual = AvailabilityLimits {
                max_work: limits.per_command.max_work.min(work),
                max_failure_cases: limits.per_command.max_failure_cases.min(cases),
                ..limits.per_command
            };
            let assessment = assess_systematic(input, individual, checkpoint)
                .map_err(|error| BatchProposalError::Availability { offset, error })?;
            work = work
                .checked_sub(assessment.work())
                .ok_or(BatchProposalError::WorkBudget)?;
            cases = cases
                .checked_sub(assessment.checked_failure_cases())
                .ok_or(BatchProposalError::FailureCaseBudget)?;
            assessments.push(assessment);
        }
        let output = {
            let mut permit = self
                .application
                .application
                .acquire_batch(AssessedBatch {
                    commands: &commands,
                    assessments: &assessments,
                    range: positions,
                })
                .await
                .map_err(BatchProposalError::Authority)?;
            checkpoint().map_err(BatchProposalError::Interrupted)?;
            self.replica
                .step(&mut permit, Event::ProposeBatch(commands))
                .await
                .map_err(BatchProposalError::Replica)?
        };
        // Permit custody has ended only after publication. Preserve the same
        // output history for existing read and write waiters; no await follows.
        let member = self.absorb(output).map_err(BatchProposalError::State)?;
        Ok(MemberBatchProposalOutput { positions, member })
    }
}
