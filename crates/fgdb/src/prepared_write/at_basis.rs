//! Historical mutation preparation through the ordinary evaluator.
//!
//! Reconstruct only the derived writer/version state at the requested cut;
//! never reinterpret an old batch against the live writer. The scope exposes
//! preparation/staging operations only, and restores live state on return or
//! unwind. No publication, validator installation, allocation of identities or
//! await occurs.

use crate::{
    Database, PreparedWrite, RebuildError, Snapshot, WriteBatch, WriteError, WriteTxn,
    WriteTxnError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::DeltaRow;
use fgdb_strata::writer::BlockWriter;
use fgdb_types::CommitSeq;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// A synchronous, preparation-only borrow. In particular there is deliberately
/// no DerefMut or accessor exposing the temporarily selected database to writes.
pub(crate) struct PreparationBasis<'a, V: Vfs> {
    database: &'a mut Database<V>,
    original: Option<(BlockWriter, Arc<Snapshot>)>,
}

impl<V: Vfs> Drop for PreparationBasis<'_, V> {
    fn drop(&mut self) {
        if let Some((writer, snapshot)) = self.original.take() {
            self.database.writer = writer;
            self.database.snapshot = snapshot;
        }
    }
}

impl<V: Vfs + Clone> PreparationBasis<'_, V> {
    pub(crate) fn prepare_write(
        &mut self,
        batch: WriteBatch,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.database
            .prepare_write_checked(batch)
            .map_err(Into::into)
    }

    pub(crate) fn prepare_atomic_writes(
        &mut self,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.database.prepare_atomic_writes(batches)
    }

    pub(crate) fn prepare_ordered_writes(
        &mut self,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.database.prepare_ordered_writes(batches)
    }

    // These adapters call only the existing synchronous staging entries. They
    // deliberately do not expose the database or a publication-capable borrow.
    pub(crate) fn stage_write(
        &mut self,
        txn: &mut WriteTxn,
        batch: WriteBatch,
    ) -> Result<(), WriteTxnError> {
        txn.write(self.database, batch)
    }

    pub(crate) fn stage_atomic_writes(
        &mut self,
        txn: &mut WriteTxn,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        txn.write_atomic(self.database, batches)
    }

    pub(crate) fn stage_ordered_writes(
        &mut self,
        txn: &mut WriteTxn,
        batches: Vec<WriteBatch>,
    ) -> Result<(), WriteTxnError> {
        txn.write_ordered(self.database, batches)
    }

    pub(crate) fn stage_ordered_writes_bounded(
        &mut self,
        txn: &mut WriteTxn,
        batches: Vec<WriteBatch>,
        max_expanded_rows: u64,
    ) -> Result<(), WriteTxnError> {
        txn.write_ordered_bounded(self.database, batches, max_expanded_rows)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Select a preparation basis without issuing a new read view, changing an
    /// ownership token, or touching the current publication/commit coordinator.
    /// All reconstruction succeeds before any handle state is displaced.
    pub(crate) fn preparation_basis(
        &mut self,
        basis: CommitSeq,
    ) -> Result<PreparationBasis<'_, V>, WriteTxnError> {
        self.ensure_writable()?;
        self.snapshot.check_frontier(basis)?;
        let original = if basis == self.snapshot.frontier {
            None
        } else {
            // The native index guarantees a gap-free retained prefix. A retired
            // origin is NOT an empty history, even when the target is old.
            let history = self
                .snapshot
                .delta_index
                .since(CommitSeq(0))
                .map_err(WriteError::PreparedHistory)?;
            let mut writer = BlockWriter::new(crate::GRAPH, crate::BRANCH, crate::PARTITION);
            let mut versions = BTreeMap::new();
            let mut touched = BTreeSet::new();
            let mut births = 0_u64;
            for batch in history.take_while(|batch| batch.commit_seq().0 <= basis.0) {
                let at = batch.commit_seq();
                for coordinate in batch.coordinate_entries() {
                    if (coordinate.graph, coordinate.branch) != (crate::GRAPH, crate::BRANCH) {
                        continue;
                    }
                    for row in &coordinate.rows {
                        writer
                            .apply(self.keys.block_keys(), at, row)
                            .map_err(|error| basis_rebuild(at, error))?;
                        crate::touched_elements(row, &mut touched);
                        if matches!(
                            row,
                            DeltaRow::CreateVertex { .. } | DeltaRow::CreateEdge { .. }
                        ) {
                            births += 1;
                        }
                    }
                }
                crate::fold_statement_versions(&mut versions, &touched, &writer).map_err(
                    |error| {
                        WriteTxnError::BasisRebuild(Box::new(RebuildError::Version {
                            commit_seq: at.0,
                            error,
                        }))
                    },
                )?;
                touched.clear();
                // Match the native per-commit seal law. The writer's live and
                // permanently-spent identity state is the production fold's,
                // including creations subsequently deleted before this basis.
                let _ = writer
                    .seal(self.keys.block_keys())
                    .map_err(|error| basis_rebuild(at, error))?;
                let _ = writer
                    .seal_vertices(self.keys.block_keys())
                    .map_err(|error| basis_rebuild(at, error))?;
            }
            // Historical lookup in these authenticated blocks/patches already
            // selects by frontier. Versions and spent identities, unlike read
            // lookup, must be reconstructed without any post-basis statements.
            let mut snapshot = (*self.snapshot).clone();
            snapshot.frontier = basis;
            snapshot.versions = versions;
            snapshot.next_birth_ordinal = births;
            let snapshot = Arc::new(snapshot);
            Some((
                std::mem::replace(&mut self.writer, writer),
                std::mem::replace(&mut self.snapshot, snapshot),
            ))
        };
        Ok(PreparationBasis {
            database: self,
            original,
        })
    }

    /// Prepare one batch against exactly `basis`, without advancing that basis.
    ///
    /// Uses the same mutation evaluator, storage admission and canonical form
    /// as `prepare_write`. Commit still checks the complete intervening suffix
    /// with first-committer-wins; successful preparation is not commit approval.
    /// Future cuts, unavailable retained history and unhealthy handles refuse.
    ///
    /// An older cut reconstructs native writer/version state from the retained
    /// delta prefix and copies snapshot metadata. This is an in-memory
    /// preparation path, not an O(delta) preparation, retention lease or SSI claim.
    pub fn prepare_write_at(
        &mut self,
        basis: CommitSeq,
        batch: WriteBatch,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.preparation_basis(basis)?.prepare_write(batch)
    }

    /// Prepare independent relation groups at one exact historical basis.
    /// The ordinary atomic composition restrictions and commit validator apply.
    pub fn prepare_atomic_writes_at(
        &mut self,
        basis: CommitSeq,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.preparation_basis(basis)?
            .prepare_atomic_writes(batches)
    }

    /// Prepare dependent, source-ordered relation groups at an unchanged basis.
    /// This does not refresh a transaction or silently rebase its effects.
    pub fn prepare_ordered_writes_at(
        &mut self,
        basis: CommitSeq,
        batches: Vec<WriteBatch>,
    ) -> Result<PreparedWrite, WriteTxnError> {
        self.preparation_basis(basis)?
            .prepare_ordered_writes(batches)
    }
}

fn basis_rebuild(at: CommitSeq, error: fgdb_strata::writer::WriteError) -> WriteTxnError {
    WriteTxnError::BasisRebuild(Box::new(RebuildError::Fold {
        commit_seq: at.0,
        error,
    }))
}

#[cfg(test)]
#[path = "at_basis_tests.rs"]
mod tests;
