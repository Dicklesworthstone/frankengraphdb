//! One borrowed live guard shared by projection and cursor-kernel operations.
//! This is not an authority constructor: the real QueryCx always checkpoints
//! first. The guard can only interrupt work, never replace source admission.

use crate::SealedProjectionError;
use fgdb_strata::tiered::sealed::SealedError;
use fgdb_types::QueryCx;

pub(crate) struct Control<'a> {
    pub(crate) query: &'a QueryCx,
    guard: &'a dyn Fn() -> Result<(), SealedProjectionError>,
}

impl<'a> Control<'a> {
    pub(crate) fn new(
        query: &'a QueryCx,
        guard: &'a dyn Fn() -> Result<(), SealedProjectionError>,
    ) -> Self {
        Self { query, guard }
    }

    pub(crate) fn checkpoint(&self) -> Result<(), SealedProjectionError> {
        self.query.checkpoint().map_err(SealedError::Interrupted)?;
        self.guard()
    }

    // Strata's *_with_checkpoint functions enforce QueryCx independently, so
    // their callback invokes only the extra guard, not a second query check.
    pub(crate) fn guard(&self) -> Result<(), SealedProjectionError> {
        (self.guard)()
    }
}
