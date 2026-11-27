//! Restart a fenced member from its paired consensus and application roots.

use fgdb_order::{Configuration, Limits, MemberId, PersistentState};
use fgdb_repl::application::member::AppliedReplica;
use fgdb_repl::application::{Application, ApplicationError, ApplicationStateError};
use fgdb_repl::driver::RaftPublisher;

use super::AegisReplicaCoordinator;

impl<C: Clone + Eq> AegisReplicaCoordinator<C> {
    /// Reopen an exclusively fenced member and apply its committed suffix before
    /// handing it to the serving loop. The backend authenticates the application
    /// projection from the same store as the supplied Raft publication.
    ///
    /// Each batch publishes its commands and applied cursor together. After an
    /// error or cancellation, reopen the actual published roots and retry. The
    /// returned member owns both state machines and preserves the application's
    /// independent audit-visible frontier for subsequent reads.
    pub async fn recover_application<A: Application<C> + RaftPublisher<C>>(
        member: MemberId,
        expected: &Configuration,
        state: PersistentState<C>,
        limits: Limits,
        maximum_reads: usize,
        maximum_batch_entries: usize,
        backend: A,
    ) -> Result<AppliedReplica<C, A>, ApplicationError<<A as Application<C>>::Error>> {
        let coordinator = Self::recover(member, expected, state, limits, maximum_reads)
            .map_err(ApplicationStateError::Raft)?;
        let mut applied = AppliedReplica::recover(
            coordinator.replica,
            backend,
            maximum_batch_entries,
            maximum_reads,
        )
        .await?;
        while applied.apply_next().await?.is_some() {}
        Ok(applied)
    }
}

#[cfg(test)]
mod tests;
