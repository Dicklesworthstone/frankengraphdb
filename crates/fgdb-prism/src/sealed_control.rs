//! One borrowed live guard shared by projection and cursor-kernel operations.
//! This is not an authority constructor: the real QueryCx always checkpoints
//! first. The guard can only interrupt work, never replace source admission.

use crate::SealedProjectionError;
use fgdb_strata::tiered::sealed::{SealedBuildEvent, SealedError};
use fgdb_types::QueryCx;

pub(crate) type GuardResult = Result<(), SealedProjectionError>;
pub(crate) type GuardFn = fn() -> GuardResult;

/// Keep the guard's concrete type through suspended work. An owner call's
/// stateless guard is Sync, so its future can move between runtime workers.
/// Local FnMut guards still use their existing RefCell adapter and need not
/// be Send or Sync. Neither path adds a lock or erases a caller's live guard.
pub(crate) trait Guard {
    fn check(&self) -> GuardResult;

    fn check_event(&self, _event: SealedBuildEvent) -> GuardResult {
        self.check()
    }
}

impl<F: Fn() -> GuardResult> Guard for F {
    fn check(&self) -> GuardResult {
        self()
    }
}

pub(crate) struct Control<'a, G: Guard = GuardFn> {
    pub(crate) query: &'a QueryCx,
    guard: &'a G,
}

impl<'a, G: Guard> Control<'a, G> {
    pub(crate) fn new(query: &'a QueryCx, guard: &'a G) -> Self {
        Self { query, guard }
    }

    pub(crate) fn checkpoint(&self) -> Result<(), SealedProjectionError> {
        self.checkpoint_event(SealedBuildEvent::Work)
    }

    pub(crate) fn checkpoint_event(
        &self,
        event: SealedBuildEvent,
    ) -> Result<(), SealedProjectionError> {
        self.query.checkpoint().map_err(SealedError::Interrupted)?;
        self.guard_event(event)
    }

    // Strata's *_with_checkpoint functions enforce QueryCx independently, so
    // their callback invokes only the extra guard, not a second query check.
    pub(crate) fn guard(&self) -> Result<(), SealedProjectionError> {
        self.guard_event(SealedBuildEvent::Work)
    }

    pub(crate) fn guard_event(&self, event: SealedBuildEvent) -> GuardResult {
        self.guard.check_event(event)
    }
}
