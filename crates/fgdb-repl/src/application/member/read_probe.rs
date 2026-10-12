//! Dedicated read confirmations inside the existing apply/visibility owner.
//!
//! Quorum completion updates the original waiting read, never returns a raw
//! snapshot or moves an application cursor. The host must still use try_read
//! to wait for application/audit visibility and pin the verified visible cut.

mod batch;

use core::convert::Infallible;

use fgdb_order::{ReadIndexAck, ReadIndexError, ReadIndexProbe};

use super::{AppliedReplica, MemberError};
use crate::application::{Application, ApplicationStateError};
use crate::driver::RaftPublisher;
use crate::replica::{ReadIndexId, ReplicaError};

fn probe_error(error: ReadIndexError) -> MemberError<Infallible> {
    MemberError::Replica(ReplicaError::ReadIndex(error))
}

impl<C: Clone + Eq, A: Application<C> + RaftPublisher<C>> AppliedReplica<C, A> {
    /// Obtain fresh, request-bound probes without waiting for replication I/O.
    /// The read must already have been admitted by read_index. Once its quorum
    /// is complete, no probes remain, but its apply/audit wait still occupies
    /// one admission slot until try_read succeeds or cancel_read retires it.
    ///
    /// The runtime authenticates every field using the existing consensus
    /// domain, and owns transport budgets and deadlines. These in-process
    /// types are not a wire codec, a liveness lease or a data acknowledgement.
    pub fn read_index_probes(
        &self,
        id: &ReadIndexId,
    ) -> Result<Vec<ReadIndexProbe>, MemberError<Infallible>> {
        self.available()?;
        let waiting = self
            .waiting
            .iter()
            .find(|waiting| &waiting.id == id)
            .ok_or(ApplicationStateError::UnknownRead)?;
        if waiting.quorum.is_some() {
            return Ok(Vec::new());
        }
        self.replica.read_index_probes(id).map_err(probe_error)
    }

    /// Respond only from a healthy member's published consensus state. A local
    /// application-publication failure fences this operation too: the caller
    /// cannot bypass the owned member by reaching directly into its Raft voter.
    /// Ordinary consensus traffic must first establish the published leader.
    pub fn acknowledge_read_index(
        &self,
        probe: ReadIndexProbe,
    ) -> Result<ReadIndexAck, MemberError<Infallible>> {
        self.available()?;
        self.replica
            .acknowledge_read_index(probe)
            .map_err(probe_error)
    }

    /// Add a dedicated voter response to one original read. True means ONLY
    /// that its consensus quorum is now complete, not that application data
    /// may be observed. try_read retains all existing application, audit,
    /// history, snapshot-pin and leadership checks after this transition.
    ///
    /// Unknown/cancelled IDs and duplicate completion refuse without disturbing
    /// another waiting read. No await separates consuming the replica proof
    /// from attaching it to its application owner. Both queues retain the same
    /// request identity; no unbounded result queue or second completion exists.
    pub fn confirm_read_index(
        &mut self,
        id: &ReadIndexId,
        ack: ReadIndexAck,
    ) -> Result<bool, MemberError<Infallible>> {
        self.available()?;
        let position = self
            .waiting
            .iter()
            .position(|waiting| &waiting.id == id)
            .ok_or(ApplicationStateError::UnknownRead)?;
        if self.waiting[position].quorum.is_some() {
            return Err(probe_error(ReadIndexError::Completed));
        }
        let Some(ready) = self
            .replica
            .confirm_read_index(id, ack)
            .map_err(probe_error)?
        else {
            return Ok(false);
        };
        self.waiting[position].quorum = Some(ready);
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
