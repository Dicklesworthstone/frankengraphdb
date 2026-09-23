//! Restoring an applied snapshot whose audit-visible sub-prefix is earlier.
//!
//! This is the application projection of the existing canonical snapshot, not
//! another wire/disk record. The backend's verifier and interpreter restore the
//! actual queue and historical view; this module binds that result to the live
//! consensus cut and rejects attempts to turn applied-but-hidden into visible.

use fgdb_chronicle::seed::SeedAuditCut;
use fgdb_order::{PersistentState, SnapshotCut};
use fgdb_types::ObjectId;

use super::ApplicationStateError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredSnapshot {
    snapshot: SnapshotCut,
    audit_cut: SeedAuditCut,
}

impl RestoredSnapshot {
    /// Call only after restoring all planes from this exact authenticated
    /// snapshot: applied effects, full audit pipeline (including unresolved
    /// candidates), prepared ownership, and the earlier visible state. Matching
    /// roots/positions here is not a replacement for the log-to-all-planes proof.
    pub fn from_authenticated_parts(
        snapshot: SnapshotCut,
        audit_cut: SeedAuditCut,
    ) -> Result<Self, ApplicationStateError> {
        audit_cut.validate_at(snapshot.index(), snapshot.term(), ObjectId(snapshot.state_root()))
            .map_err(|_| ApplicationStateError::SnapshotAuditMismatch)?;
        Ok(Self { snapshot, audit_cut })
    }

    pub fn snapshot(&self) -> &SnapshotCut {
        &self.snapshot
    }

    pub fn audit_cut(&self) -> SeedAuditCut {
        self.audit_cut
    }
}

pub(super) fn validate_restoration<C>(
    state: &PersistentState<C>,
    restored: Option<&RestoredSnapshot>,
) -> Result<(), ApplicationStateError> {
    if let Some(restored) = restored {
        if state.snapshot() != Some(restored.snapshot()) {
            return Err(ApplicationStateError::SnapshotAuditMismatch);
        }
        restored.audit_cut.validate_at(
            restored.snapshot.index(), restored.snapshot.term(), ObjectId(restored.snapshot.state_root()),
        ).map_err(|_| ApplicationStateError::SnapshotAuditMismatch)?;
    }
    Ok(())
}
