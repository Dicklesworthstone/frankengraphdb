//! Bounded multi-request admission, not a reusable quorum lease.
//!
//! All requests must already exist when the host calls this operation. One
//! heartbeat transition follows admission of the whole set. Fresh append
//! replies may confirm the set together through the ordinary observer; each
//! request retains its own typed probe nonce, captured floor and cancellation.

use std::collections::BTreeSet;
use std::sync::Arc;

use fgdb_order::{Error as RaftError, Event, ReadIndexError, Role};

use crate::driver::RaftPublisher;
use crate::replica::{PendingRead, ReadIndexId, Replica, ReplicaError, ReplicaOutput};

impl<C: Clone + Eq> Replica<C> {
    /// Admit a nonempty batch of already-received reads, then drive ONE ordinary
    /// heartbeat transition. Admission consumes `count` of the existing read
    /// slots, not one slot for an arbitrarily large batch. Zero is InvalidLimits;
    /// a batch that does not fit is ReadBackpressure, with no prefix admitted.
    ///
    /// Returned IDs are in admission order and independently cancellable. Every
    /// read captures the same pre-probe committed floor. A fresh append reply can
    /// satisfy several reads, but a dedicated probe reply still belongs to its
    /// exact request. It is never relabelled as evidence for another request.
    ///
    /// This is not an atomic multi-query snapshot: the application owner must
    /// still apply, wait for audit visibility and pin each accepted request.
    /// The host must not append newly arrived requests to this batch afterwards.
    /// Failure or cancellation retires this batch's unreturned IDs only; an
    /// uncertain publication still fences the entire replica in the usual way.
    pub async fn read_index_batch<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
        count: usize,
    ) -> Result<(Vec<ReadIndexId>, ReplicaOutput<C>), ReplicaError<P::Error>> {
        if self.raft.role().map_err(ReplicaError::Raft)? != Role::Leader {
            return Err(ReplicaError::Raft(RaftError::NotLeader));
        }
        if count == 0 {
            return Err(ReplicaError::Raft(RaftError::InvalidLimits));
        }
        if count > self.maximum_reads.saturating_sub(self.reads.len()) {
            return Err(ReplicaError::ReadBackpressure);
        }
        let last = self
            .next_read
            .checked_add(count as u64)
            .ok_or(ReplicaError::ReadCounterExhausted)?;
        let first = self.next_read + 1; // checked nonempty range above
        let state = self.raft.durable_state().map_err(ReplicaError::Raft)?;
        let (term, index) = (state.term(), state.commit_index());
        let after_request = self.highest_request;
        let mut ids = Vec::with_capacity(count);
        let mut admission = BatchAdmission {
            replica: self,
            first,
            last,
            armed: true,
        };
        // Never reuse reserved local IDs after a partially admitted refusal.
        // Only volatile counters can advance on that path; no ID escapes.
        admission.replica.next_read = last;
        for serial in first..=last {
            let probe = admission
                .replica
                .raft
                .begin_read_index()
                .map_err(|error| match error {
                    ReadIndexError::Raft(error) => ReplicaError::Raft(error),
                    ReadIndexError::CurrentTermNotCommitted => {
                        ReplicaError::CurrentTermNotCommitted
                    }
                    error => ReplicaError::ReadIndex(error),
                })?;
            let id = ReadIndexId {
                incarnation: Arc::clone(&admission.replica.incarnation),
                serial,
            };
            let member = admission.replica.raft.id();
            admission.replica.reads.insert(
                serial,
                PendingRead {
                    id: id.clone(),
                    term,
                    index,
                    after_request,
                    confirmations: BTreeSet::from([member]),
                    probe,
                },
            );
            ids.push(id);
        }
        let output = admission.replica.step(publisher, Event::Heartbeat).await?;
        admission.armed = false;
        Ok((ids, output))
    }
}

/// The complete reservation is owned before any admission or cancellable work.
/// Earlier requests lie outside this checked, never-reused serial interval.
struct BatchAdmission<'a, C> {
    replica: &'a mut Replica<C>,
    first: u64,
    last: u64,
    armed: bool,
}

impl<C> Drop for BatchAdmission<'_, C> {
    fn drop(&mut self) {
        if self.armed {
            for serial in self.first..=self.last {
                self.replica.reads.remove(&serial);
            }
        }
    }
}

#[cfg(test)]
mod tests;
