//! Server-hosted Aegis single-voter / replica coordinator (plan §14).
//!
//! Binds [`fgdb_order`] consensus state transitions and [`fgdb_repl`]'s
//! durability-gated [`Replica`] driver into `fgdb-server` without claiming
//! unimplemented multi-node network transport. Single-member quorum bootstrap,
//! proposal sequencing through [`RaftPublisher`], and clock-free `read_index`
//! barriers run through the real Aegis kernels.

use fgdb_order::{
    Committed, Configuration, Domain, Error as OrderError, Event, Limits, MemberId, PersistentState,
    Role,
};
use fgdb_repl::driver::RaftPublisher;
use fgdb_repl::replica::{ReadIndexId, ReadResolution, Replica, ReplicaError, ReplicaOutput};

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
        let configuration =
            Configuration::stable(domain, configuration_identity, [member], core::iter::empty())?;
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
            assert_eq!(AegisReplicaCoordinator::ready_read_indices(&read_out), vec![2]);
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
        assert_eq!(
            ok.vector().network_bytes,
            u64::from(limits.max_frame_len)
        );
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
