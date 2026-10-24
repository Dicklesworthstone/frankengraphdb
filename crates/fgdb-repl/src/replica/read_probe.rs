//! A read's fresh voter confirmations need not wait for log or snapshot I/O.
//!
//! The host binds each probe/reply to authenticated transport and retains the
//! local ReadIndexId that originated it. These are the kernel's existing typed
//! in-process messages, NOT a new wire format or an authority to serve data.
//! Ordinary Raft traffic still establishes the follower's published leader.

mod batch;

use fgdb_order::{ReadIndexAck, ReadIndexError, ReadIndexProbe};

use super::{PendingRead, ReadIndexId, ReadIndexReady, Replica, quorum};

impl<C: Clone + Eq> Replica<C> {
    /// Return this admitted read's still-unconfirmed voters in member order.
    /// Retries keep the same nonce. At most the configuration's voter count is
    /// returned; no append slot, snapshot transfer or publication is required.
    /// The host owns transport budgets, deadlines and cancellation for the read.
    ///
    /// A completed/cancelled read or another replica's ID is refused. Fresh
    /// append acknowledgements and dedicated replies suppress the same peers.
    pub fn read_index_probes(
        &self,
        id: &ReadIndexId,
    ) -> Result<Vec<ReadIndexProbe>, ReadIndexError> {
        let read = self.probe_read(id)?;
        let mut probes = read.probe.probes(&self.raft)?;
        probes.retain(|probe| !read.confirmations.contains(&probe.to));
        Ok(probes)
    }

    /// Reply from this voter's already-published state without waiting for log
    /// catch-up. This never changes its term, leader, liveness timer, log, match
    /// index or application cursor. The normal consensus path must establish
    /// and publish the leader first. A poisoned/unpublished voter cannot reply.
    pub fn acknowledge_read_index(
        &self,
        probe: ReadIndexProbe,
    ) -> Result<ReadIndexAck, ReadIndexError> {
        self.raft.acknowledge_read_index(probe)
    }

    /// Confirm one original read using the exact authenticated probe response.
    /// This shares its voter set with independently fresh append successes:
    /// mixed confirmations count once per voter and still need BOTH majorities
    /// in a joint configuration. No later commit can raise the captured floor.
    ///
    /// None means the quorum is incomplete. Some consumes the pending request,
    /// so the ordinary step path cannot release it again. This is only the
    /// consensus gate; application/audit visibility and snapshot admission are
    /// unchanged. In particular, this does not acknowledge any replicated data.
    pub fn confirm_read_index(
        &mut self,
        id: &ReadIndexId,
        ack: ReadIndexAck,
    ) -> Result<Option<ReadIndexReady>, ReadIndexError> {
        // Check the local owner before touching a different machine's same
        // numeric serial. Kernel validation then checks every transport field,
        // publication state, term, leadership and handoff before mutating votes.
        self.probe_read(id)?;
        let read = self
            .reads
            .get_mut(&id.serial)
            .ok_or(ReadIndexError::ForeignRound)?;
        read.probe.acknowledge(&self.raft, ack)?;
        read.confirmations.insert(ack.from);
        let state = self.raft.durable_state()?;
        let configuration = state.configuration();
        if !quorum(configuration, &read.confirmations) {
            return Ok(None);
        }
        let read = self
            .reads
            .remove(&id.serial)
            .ok_or(ReadIndexError::ForeignRound)?;
        Ok(Some(ReadIndexReady {
            id: read.id,
            domain: configuration.domain(),
            configuration: configuration.identity(),
            leader: self.raft.id(),
            term: read.term,
            index: read.index,
        }))
    }

    fn probe_read(&self, id: &ReadIndexId) -> Result<&PendingRead, ReadIndexError> {
        self.reads
            .get(&id.serial)
            .filter(|read| &read.id == id)
            .ok_or(ReadIndexError::ForeignRound)
    }
}

#[cfg(test)]
mod tests;
