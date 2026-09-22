//! Bounded application of the durably committed Raft suffix (plan §14.1).
//!
//! Ordering, application durability and audit visibility are separate gates.
//! This driver never applies an outbound proposal or a speculative persistence
//! view. It borrows a published replica, selects only its next committed slice,
//! and advances its cursor only after the application's atomic publication.
//!
//! The application implements the existing canonical command interpreter and
//! Chronicle publisher under its exclusive writer fence and narrowed Cx. This
//! module supplies sequencing and cancellation ownership, not a new interpreter,
//! durable format, root verifier, payload certificate or authorization scheme.

use std::future::Future;
use std::marker::PhantomData;

use fgdb_order::{Domain, Entry, Error as RaftError, PersistentState};
use fgdb_types::ObjectId;

use crate::replica::Replica;

pub mod member;

/// Internal Raft coordinates. These are not public logical-command positions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedPosition {
    pub index: u64,
    pub term: u64,
}

/// A verifier-produced projection of ONE authenticated published root.
///
/// `visible_index` is the largest contiguous Raft prefix whose application AND
/// audit-visible effects are closed. It is not the raw audit counter. No-ops
/// may extend that prefix only when they skip no hidden command. `state_root`
/// names application state; `publication_root` also owns the applied cursor.
///
/// The backend must verify the canonical log-to-state relation and the complete
/// root closure before returning this projection. Matching positions/digests
/// here does not replace that verification or make a peer's assertion trusted.
/// Raft-only publication may supersede the outer root without changing this
/// application projection. Apply must preserve those newer protocol roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplicationProgress {
    pub domain: Domain,
    pub configuration: [u8; 32],
    pub applied: AppliedPosition,
    pub visible_index: u64,
    pub state_root: ObjectId,
    pub publication_root: ObjectId,
    pub publication_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationStateError {
    InvalidLimits,
    RecoveryRequired,
    Raft(RaftError),
    WrongDomain,
    WrongConfiguration,
    InvalidPosition,
    AppliedAheadOfCommit,
    SnapshotRequired,
    SnapshotStateMismatch,
    VisibilityRegression,
    InvalidPublication,
    PendingReadsAtAttach,
    ReadBackpressure,
    UnknownRead,
    ReadHistoryMismatch,
    LeadershipLost,
    InvalidReadSnapshot,
    CompactionNotVisible,
    AllocationFailed,
}

impl core::fmt::Display for ApplicationStateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis application state: {self:?}")
    }
}
impl core::error::Error for ApplicationStateError {}

#[derive(Debug)]
pub enum ApplicationError<E> {
    State(ApplicationStateError),
    Backend(E),
}

impl<E: core::fmt::Debug> core::fmt::Display for ApplicationError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Aegis application: {self:?}")
    }
}
impl<E: core::fmt::Debug> core::error::Error for ApplicationError<E> {}
impl<E> From<ApplicationStateError> for ApplicationError<E> {
    fn from(error: ApplicationStateError) -> Self {
        Self::State(error)
    }
}

/// A nonempty, contiguous, bounded borrow of the committed log. Only this
/// driver constructs it; commands are not cloned or collected into a second
/// unbounded replay queue. The replica cannot process another input while this
/// view is borrowed. This is an in-process view, never another durable format.
pub struct ApplicationBatch<'a, C> {
    basis: &'a ApplicationProgress,
    consensus: &'a PersistentState<C>,
    entries: &'a [Entry<C>],
    first_index: u64,
    last: AppliedPosition,
}

impl<C> ApplicationBatch<'_, C> {
    pub fn basis(&self) -> &ApplicationProgress {
        self.basis
    }
    pub fn consensus(&self) -> &PersistentState<C> {
        self.consensus
    }
    pub fn first_index(&self) -> u64 {
        self.first_index
    }
    pub fn last(&self) -> AppliedPosition {
        self.last
    }
    pub fn entries(&self) -> &[Entry<C>] {
        self.entries
    }

    /// All entries, including no-ops, must advance the persisted Raft cursor.
    pub fn indexed_entries(&self) -> impl ExactSizeIterator<Item = (AppliedPosition, &Entry<C>)> {
        self.entries.iter().enumerate().map(|(offset, entry)| {
            (
                AppliedPosition {
                    index: self.first_index + offset as u64,
                    term: entry.term,
                },
                entry,
            )
        })
    }

    /// No-ops are not logical commands and never consume LogicalCommandSeq,
    /// CommitSeq or HLC. The interpreter classifies the remaining commands.
    pub fn commands(&self) -> impl Iterator<Item = (u64, &C)> {
        self.indexed_entries().filter_map(|(position, entry)| {
            entry
                .command
                .as_ref()
                .map(|command| (position.index, command))
        })
    }
}

/// The canonical state-machine and publication capability, not a peer client.
///
/// Both methods run under the actual exclusive writer fence and narrowed Cx;
/// futures must own their effects in that region, with no detached work. Load
/// authenticates the same root closure used to recover the supplied replica.
/// Apply interprets the exact ordered commands and atomically publishes their
/// effects AND the last applied Raft cursor, then syncs and rereads that root.
///
/// A no-op updates only Raft/application progress. A hidden applied command is
/// not visible merely because it is committed; only verified audit-visibility
/// controls advance `visible_index`. Do not wait for audit release inside apply:
/// later committed controls may be exactly what releases the hidden prefix.
///
/// The backend compares the application projection with `batch.basis()` and
/// preserves the exact newer protocol/log state in `batch.consensus()`. It must
/// not overwrite a later Raft root with the older outer root named by the basis.
/// Multi-member append-reply and post-apply publications MUST remain separate.
pub trait Application<C> {
    type Error;

    fn load(&mut self) -> impl Future<Output = Result<ApplicationProgress, Self::Error>>;

    fn apply(
        &mut self,
        batch: ApplicationBatch<'_, C>,
    ) -> impl Future<Output = Result<ApplicationProgress, Self::Error>>;
}

/// Owned replay state. There is no mutable application escape hatch or Clone.
/// An uncertain application publication can only be resolved by recovering the
/// authenticated root into a NEW driver, never by retrying the cached cursor.
/// The entry bound is not a byte/CPU claim: the backend must additionally admit
/// decoded payloads, interpreter work and immutable output via its Cx budgets.
pub struct ApplicationDriver<C, A> {
    application: A,
    progress: ApplicationProgress,
    maximum_batch_entries: usize,
    poisoned: bool,
    command: PhantomData<fn(C)>,
}

impl<C: Clone + Eq, A: Application<C>> ApplicationDriver<C, A> {
    pub async fn recover(
        mut application: A,
        replica: &Replica<C>,
        maximum_batch_entries: usize,
    ) -> Result<Self, ApplicationError<A::Error>> {
        if maximum_batch_entries == 0 || maximum_batch_entries > 65_536 {
            return Err(ApplicationStateError::InvalidLimits.into());
        }
        // Hold the borrow through load: the verified log cannot change under
        // the recovered application cut, even when load suspends.
        let state = replica
            .durable_state()
            .map_err(ApplicationStateError::Raft)?;
        let progress = application
            .load()
            .await
            .map_err(ApplicationError::Backend)?;
        validate_progress(state, &progress)?;
        Ok(Self {
            application,
            progress,
            maximum_batch_entries,
            poisoned: false,
            command: PhantomData,
        })
    }

    pub fn progress(&self) -> Result<ApplicationProgress, ApplicationStateError> {
        self.available()?;
        Ok(self.progress)
    }

    /// Publish at most one bounded slice, allowing the owning region to service
    /// network/cancellation between slices. None means caught up, not visible.
    /// Even an all-no-op batch persists its cursor; no speculative suffix enters
    /// the interpreter. No await follows successful publication validation.
    pub async fn apply_next(
        &mut self,
        replica: &Replica<C>,
    ) -> Result<Option<ApplicationProgress>, ApplicationError<A::Error>> {
        self.available()?;
        let state = replica
            .durable_state()
            .map_err(ApplicationStateError::Raft)?;
        validate_progress(state, &self.progress)?;
        if self.progress.applied.index == state.commit_index() {
            return Ok(None);
        }
        let base = state.snapshot().map_or(0, |cut| cut.index());
        let start = usize::try_from(self.progress.applied.index - base)
            .map_err(|_| ApplicationStateError::InvalidPosition)?;
        let remaining = usize::try_from(state.commit_index() - self.progress.applied.index)
            .map_err(|_| ApplicationStateError::InvalidPosition)?;
        let count = remaining.min(self.maximum_batch_entries);
        let end = start
            .checked_add(count)
            .ok_or(ApplicationStateError::InvalidPosition)?;
        let entries = state
            .entries()
            .get(start..end)
            .ok_or(ApplicationStateError::InvalidPosition)?;
        let last_entry = entries
            .last()
            .ok_or(ApplicationStateError::InvalidPosition)?;
        let last = AppliedPosition {
            index: self.progress.applied.index + count as u64,
            term: last_entry.term,
        };
        let batch = ApplicationBatch {
            basis: &self.progress,
            consensus: state,
            entries,
            first_index: self.progress.applied.index + 1,
            last,
        };
        // Set BEFORE invoking backend code, not merely before polling its future.
        // Error, panic or cancellation leaves this driver permanently fenced.
        self.poisoned = true;
        let published = self
            .application
            .apply(batch)
            .await
            .map_err(ApplicationError::Backend)?;
        validate_progress(state, &published)?;
        if published.applied != last
            || published.publication_generation <= self.progress.publication_generation
            || published.publication_root == self.progress.publication_root
        {
            return Err(ApplicationStateError::InvalidPublication.into());
        }
        if published.visible_index < self.progress.visible_index {
            return Err(ApplicationStateError::VisibilityRegression.into());
        }
        self.progress = published;
        self.poisoned = false;
        Ok(Some(published))
    }

    fn available(&self) -> Result<(), ApplicationStateError> {
        if self.poisoned {
            Err(ApplicationStateError::RecoveryRequired)
        } else {
            Ok(())
        }
    }
}

fn validate_progress<C>(
    state: &PersistentState<C>,
    progress: &ApplicationProgress,
) -> Result<(), ApplicationStateError> {
    if progress.domain != state.configuration().domain() {
        return Err(ApplicationStateError::WrongDomain);
    }
    if progress.configuration != state.configuration().identity() {
        return Err(ApplicationStateError::WrongConfiguration);
    }
    if progress.publication_generation == 0
        || (progress.applied.index == 0) != (progress.applied.term == 0)
        || progress.visible_index > progress.applied.index
    {
        return Err(ApplicationStateError::InvalidPosition);
    }
    if progress.applied.index > state.commit_index() {
        return Err(ApplicationStateError::AppliedAheadOfCommit);
    }
    let base = state.snapshot().map_or(0, |cut| cut.index());
    if progress.applied.index < base || progress.visible_index < base {
        return Err(ApplicationStateError::SnapshotRequired);
    }
    if position_at(state, progress.applied.index) != Some(progress.applied) {
        return Err(ApplicationStateError::InvalidPosition);
    }
    if let Some(cut) = state.snapshot() {
        if progress.applied.index == base && progress.state_root.0 != cut.state_root() {
            return Err(ApplicationStateError::SnapshotStateMismatch);
        }
    }
    Ok(())
}

fn position_at<C>(state: &PersistentState<C>, index: u64) -> Option<AppliedPosition> {
    let base = state.snapshot().map_or(0, |cut| cut.index());
    let term = if index == base {
        state.snapshot().map_or(0, |cut| cut.term())
    } else {
        let offset = usize::try_from(index.checked_sub(base)?.checked_sub(1)?).ok()?;
        state.entries().get(offset)?.term
    };
    Some(AppliedPosition { index, term })
}

#[cfg(test)]
mod tests;
