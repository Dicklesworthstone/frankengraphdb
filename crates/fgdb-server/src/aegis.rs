//! Server-hosted Aegis replica coordinator (plan §14).
//!
//! Binds [`fgdb_order`] consensus state transitions and [`fgdb_repl`]'s
//! durability-gated [`Replica`] driver into `fgdb-server`. A host supplies
//! authenticated peer delivery, election and heartbeat timers, an exclusive
//! member fence, and the publisher that owns the persistent state.

use fgdb_order::{
    Committed, Configuration, Domain, Envelope, Error as OrderError, Event, Limits, MemberId,
    PersistentState, ReadIndexAck, ReadIndexError, ReadIndexProbe, Role,
};
use fgdb_repl::driver::RaftPublisher;
use fgdb_repl::replica::{ReadIndexId, ReadIndexReady, ReadResolution, Replica, ReplicaError, ReplicaOutput};

/// In-process durability publisher retaining the latest published [`PersistentState`]
/// for hosts that sequence single-member or embedded replica transitions.
#[derive(Debug)]
pub struct SingleMemberPublisher<C> {
    published: Option<PersistentState<C>>,
    publications: u64,
}

impl<C> Default for SingleMemberPublisher<C> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C> SingleMemberPublisher<C> {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            published: None,
            publications: 0,
        }
    }

    #[must_use]
    pub const fn publications(&self) -> u64 {
        self.publications
    }

    #[must_use]
    pub fn latest(&self) -> Option<&PersistentState<C>> {
        self.published.as_ref()
    }
}

impl<C: Clone> RaftPublisher<C> for SingleMemberPublisher<C> {
    type Error = core::convert::Infallible;

    async fn publish(&mut self, state: &PersistentState<C>) -> Result<(), Self::Error> {
        self.published = Some(state.clone());
        self.publications = self.publications.saturating_add(1);
        Ok(())
    }
}

/// Server-owned Aegis replica coordinator wrapping [`Replica`] and [`Configuration`].
pub struct AegisReplicaCoordinator<C> {
    replica: Replica<C>,
}

impl<C: Clone + Eq> AegisReplicaCoordinator<C> {
    /// Construct a single-voter consensus group for one served database domain.
    pub fn single_voter(
        domain: Domain,
        configuration_identity: [u8; 32],
        member: MemberId,
        limits: Limits,
        maximum_reads: usize,
    ) -> Result<Self, OrderError> {
        let configuration = Configuration::stable(
            domain,
            configuration_identity,
            [member],
            core::iter::empty(),
        )?;
        Ok(Self {
            replica: Replica::new(member, configuration, limits, maximum_reads)?,
        })
    }

    /// Construct from an explicit verified multi-member [`Configuration`].
    pub fn from_configuration(
        member: MemberId,
        configuration: Configuration,
        limits: Limits,
        maximum_reads: usize,
    ) -> Result<Self, OrderError> {
        Ok(Self {
            replica: Replica::new(member, configuration, limits, maximum_reads)?,
        })
    }

    /// Resume the exclusively fenced member from its authenticated publication.
    /// The host binds the publication to this member and its currently authorized
    /// configuration. Terms, votes and committed entries survive recovery.
    /// Outstanding reads and authority to lead are reacquired in this incarnation.
    pub fn recover(
        member: MemberId,
        expected: &Configuration,
        state: PersistentState<C>,
        limits: Limits,
        maximum_reads: usize,
    ) -> Result<Self, OrderError> {
        if state.configuration().domain() != expected.domain() {
            return Err(OrderError::WrongDomain);
        }
        if state.configuration() != expected {
            return Err(OrderError::WrongConfiguration);
        }
        Ok(Self {
            replica: Replica::recover(member, state, limits, maximum_reads)?,
        })
    }

    /// Deliver an envelope received on a channel authenticated as `peer`.
    /// The transport supplies that identity independently of the envelope.
    /// The replica checks the domain, configuration, recipient and membership.
    /// Outbound replies are released only after required publication succeeds.
    pub async fn receive<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
        peer: MemberId,
        envelope: Envelope<C>,
    ) -> Result<ReplicaOutput<C>, ReplicaError<P::Error>> {
        if envelope.from != peer {
            return Err(ReplicaError::Raft(OrderError::InvalidMessage));
        }
        self.replica.step(publisher, Event::Receive(envelope)).await
    }

    /// Drive the host's election interval on voters, including the current leader.
    /// Followers pre-vote, while leaders check whether a quorum is still reachable.
    pub async fn liveness_timeout<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
    ) -> Result<ReplicaOutput<C>, ReplicaError<P::Error>> {
        self.replica.step(publisher, Event::LivenessTimeout).await
    }

    /// Drive a leader heartbeat, including retries for lagging followers.
    pub async fn heartbeat<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
    ) -> Result<ReplicaOutput<C>, ReplicaError<P::Error>> {
        self.replica.step(publisher, Event::Heartbeat).await
    }

    #[must_use]
    pub fn member_id(&self) -> MemberId {
        self.replica.id()
    }

    pub fn role(&self) -> Result<Role, OrderError> {
        self.replica.role()
    }

    pub fn committed_after(&self, applied: u64) -> Result<Vec<Committed<C>>, OrderError> {
        self.replica.committed_after(applied)
    }

    /// Campaign unconditionally in a single-voter or explicitly coordinated group.
    pub async fn campaign<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
    ) -> Result<ReplicaOutput<C>, ReplicaError<P::Error>> {
        self.replica.step(publisher, Event::ElectionTimeout).await
    }

    /// Propose one validated command through the durability-gated [`Replica`].
    pub async fn propose<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
        command: C,
    ) -> Result<ReplicaOutput<C>, ReplicaError<P::Error>> {
        self.replica.step(publisher, Event::Propose(command)).await
    }

    /// Obtain a fresh-quorum read barrier (`ReadIndex`) from the current leader.
    pub async fn read_index<P: RaftPublisher<C>>(
        &mut self,
        publisher: &mut P,
    ) -> Result<(ReadIndexId, ReplicaOutput<C>), ReplicaError<P::Error>> {
        self.replica.read_index(publisher).await
    }

    /// Release a pending request on disconnect or deadline, freeing its read slot.
    /// IDs are scoped to the originating coordinator incarnation.
    #[must_use]
    pub fn cancel_read(&mut self, id: &ReadIndexId) -> bool {
        self.replica.cancel_read(id)
    }

    /// Obtain dedicated quorum probes for an admitted read whose append replies
    /// are delayed. Retries cover the same round and only its unconfirmed voters.
    pub fn read_index_probes(&self, id: &ReadIndexId) -> Result<Vec<ReadIndexProbe>, ReadIndexError> {
        self.replica.read_index_probes(id)
    }

    /// Answer from published voter state on a channel authenticated as `peer`.
    pub fn acknowledge_read_index(
        &self,
        peer: MemberId,
        probe: ReadIndexProbe,
    ) -> Result<ReadIndexAck, ReadIndexError> {
        if probe.from != peer {
            return Err(OrderError::InvalidMessage.into());
        }
        self.replica.acknowledge_read_index(probe)
    }

    /// Credit a peer's authenticated response to the original local request.
    /// A ready floor still requires application and audit visibility through
    /// that index before the host can serve an authorized snapshot.
    pub fn confirm_read_index(
        &mut self,
        id: &ReadIndexId,
        peer: MemberId,
        ack: ReadIndexAck,
    ) -> Result<Option<ReadIndexReady>, ReadIndexError> {
        if ack.from != peer {
            return Err(OrderError::InvalidMessage.into());
        }
        self.replica.confirm_read_index(id, ack)
    }

    /// Extract ready read floors from a [`ReplicaOutput`].
    #[must_use]
    pub fn ready_read_indices(output: &ReplicaOutput<C>) -> Vec<u64> {
        output
            .reads
            .iter()
            .filter_map(|resolution| match resolution {
                ReadResolution::Ready(ready) => Some(ready.index()),
                ReadResolution::LeadershipLost(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::{Budget, runtime::RuntimeBuilder};

    #[test]
    fn single_voter_aegis_coordinator_elects_proposes_and_resolves_read_index() {
        let runtime = RuntimeBuilder::new().build().expect("runtime");
        let _root = runtime.request_cx_with_budget(Budget::INFINITE);

        runtime.block_on(async {
            let mut coordinator = AegisReplicaCoordinator::<u64>::single_voter(
                Domain([0x11; 32]),
                [0x22; 32],
                MemberId(1),
                Limits::default(),
                16,
            )
            .expect("single voter coordinator");
            let mut publisher = SingleMemberPublisher::new();

            assert_eq!(coordinator.role().expect("role"), Role::Follower);
            let elected = coordinator
                .campaign(&mut publisher)
                .await
                .expect("single-voter campaign");
            assert_eq!(coordinator.role().expect("leader"), Role::Leader);
            // Election appends and commits the current-term no-op at index 1 in a 1-node group.
            assert_eq!(elected.consensus.committed.len(), 1);
            assert_eq!(elected.consensus.committed[0].entry.command, None);

            let proposed = coordinator
                .propose(&mut publisher, 42)
                .await
                .expect("propose command");
            assert_eq!(proposed.consensus.committed.len(), 1);
            assert_eq!(proposed.consensus.committed[0].index, 2);
            assert_eq!(proposed.consensus.committed[0].entry.command, Some(42));

            let (_read_id, read_out) = coordinator
                .read_index(&mut publisher)
                .await
                .expect("read_index");
            assert_eq!(
                AegisReplicaCoordinator::ready_read_indices(&read_out),
                vec![2]
            );
            assert!(publisher.publications() >= 2);
            assert_eq!(
                coordinator
                    .committed_after(0)
                    .expect("committed suffix")
                    .len(),
                2
            );
        });
    }

    #[test]
    fn server_limits_resource_ceiling_admits_within_bounds_and_refuses_overflow() {
        let limits = crate::ServerLimits::default();
        let ok = limits
            .admit_frame_resources(u64::from(limits.max_frame_len), limits.max_window_bytes)
            .expect("exact frame and window bounds fit");
        assert_eq!(ok.vector().network_bytes, u64::from(limits.max_frame_len));
        assert_eq!(
            limits.admit_frame_resources(u64::from(limits.max_frame_len) + 1, 0),
            Err(crate::ResourceError::CeilingExceeded {
                axis: crate::ResourceAxis::NetworkBytes,
                requested: u64::from(limits.max_frame_len) + 1,
                ceiling: u64::from(limits.max_frame_len),
            })
        );
        assert_eq!(
            limits.admit_frame_resources(0, limits.max_window_bytes + 1),
            Err(crate::ResourceError::CeilingExceeded {
                axis: crate::ResourceAxis::MemoryBytes,
                requested: limits.max_window_bytes + 1,
                ceiling: limits.max_window_bytes,
            })
        );
    }
}

#[cfg(test)]
#[path = "aegis_peer_tests.rs"]
mod peer_tests;
