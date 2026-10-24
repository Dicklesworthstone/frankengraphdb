//! Batch quorum admission through the same bounded application read owner.

use crate::application::member::{AppliedReplica, AppliedReplicaOutput, MemberError, WaitingRead};
use crate::application::{Application, ApplicationStateError};
use crate::driver::RaftPublisher;
use crate::replica::ReadIndexId;

impl<C: Clone + Eq, A: Application<C> + RaftPublisher<C>> AppliedReplica<C, A> {
    /// Admit a nonempty batch of already-received reads using one ordinary
    /// heartbeat turn. Every request consumes its own application read slot,
    /// including while its quorum is ready but application/audit work is pending.
    /// A batch exceeding the remaining capacity is rejected as a whole.
    ///
    /// Each returned ID uses the ordinary probes, cancellation and `try_read`
    /// path. No raw quorum result, application data or snapshot escapes here.
    /// The common captured floor is not a promise that separately resolved
    /// queries will pin one identical snapshot; it is their minimum read floor.
    /// Network and audit progress remain independently driven by the host.
    pub async fn read_index_batch(
        &mut self,
        count: usize,
    ) -> Result<
        (Vec<ReadIndexId>, AppliedReplicaOutput<C>),
        MemberError<<A as RaftPublisher<C>>::Error>,
    > {
        self.available()?;
        if count == 0 {
            return Err(ApplicationStateError::InvalidLimits.into());
        }
        if count > self.maximum_reads.saturating_sub(self.waiting.len()) {
            return Err(ApplicationStateError::ReadBackpressure.into());
        }
        let (ids, output) = self
            .replica
            .read_index_batch(&mut self.application.application, count)
            .await
            .map_err(MemberError::Replica)?;
        // Recovery reserved maximum_reads capacity. The entire batch was checked
        // above, and the exclusive borrow spans consensus admission. No await
        // separates recording every ID from absorbing immediate quorum results.
        for id in &ids {
            self.waiting.push(WaitingRead {
                id: id.clone(),
                quorum: None,
            });
        }
        let output = self.absorb(output).map_err(MemberError::State)?;
        Ok((ids, output))
    }
}

#[cfg(test)]
mod tests;
