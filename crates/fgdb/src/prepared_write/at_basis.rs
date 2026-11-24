//! Historical mutation preparation through the ordinary evaluator.
//!
//! Reconstruct only the derived writer/version state at the requested cut;
//! never reinterpret an old batch against the live writer. The scope exposes
//! preparation/staging operations only, and restores live state on return or
//! unwind. No publication, validator installation or await occurs. Authorized
//! statements allocate only through an allocator captured from the live writer
//! before selection; its counters and current spent-identity exclusions never
//! rewind with historical semantic state.

use crate::{
    AuthorizedWriteOutput, AuthorizedWriteTransaction, Database, ElementId, PreparedWrite,
    RebuildError, Snapshot, VersionHeads, WriteBatch, WriteError, WriteHeads, WriteTxn,
    WriteTxnError,
};
use asupersync::fs::Vfs;
use fgdb_delta_types::DeltaRow;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlQueryError, GqlQueryExecution, GraphAggregateRow, GraphWriteChainError, PreparedGraphSet,
    PreparedGraphSetAggregate, PreparedGraphTransactionControl, PreparedGraphWriteChain,
};
use fgdb_strata::writer::BlockWriter;
use fgdb_types::{CommitSeq, QueryCx};
use std::sync::Arc;

type AuthorizedFault =
    GqlQueryError<GraphWriteChainError<WriteTxnError, WriteTxnError>, WriteTxnError>;

/// An exact native fold at this writer lifetime's lazy-open cut. Keep the
/// writer, including its spent identities, rather than deriving old state from
/// newer partition rows. Immutable sealed payloads are shared with the live
/// writer; staging and live maps remain independent. The ordinary fold and
/// per-commit seal remain the reconstruction implementation.
pub(crate) struct PreparationAnchor {
    basis: CommitSeq,
    writer: BlockWriter,
    heads: WriteHeads,
}

/// A synchronous, preparation-only borrow. In particular there is deliberately
/// no DerefMut or accessor exposing the temporarily selected database to writes.
pub(crate) struct PreparationBasis<'a, V: Vfs> {
    database: &'a mut Database<V>,
    original: Option<(BlockWriter, WriteHeads, Arc<Snapshot>)>,
}

impl<V: Vfs> Drop for PreparationBasis<'_, V> {
    fn drop(&mut self) {
        if let Some((writer, heads, snapshot)) = self.original.take() {
            self.database.writer = writer;
            self.database.heads = heads;
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

    pub(crate) fn stage_graph_write_program<A>(
        &mut self,
        txn: &mut WriteTxn,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<crate::ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramStats,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        txn.execute_graph_write_program_governed(self.database, cx, program, policy, allocate)
    }

    pub(crate) fn stage_graph_write_program_returning<A>(
        &mut self,
        txn: &mut WriteTxn,
        cx: &fgdb_types::QueryCx,
        program: &fgdb_gql::PreparedGraphWriteProgram,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<crate::ElementId, A>,
    ) -> Result<
        fgdb_gql::GraphWriteProgramReceipt,
        fgdb_gql::GraphWriteProgramError<WriteTxnError, A, Box<asupersync::error::Error>>,
    > {
        txn.execute_graph_write_program_returning_governed(
            self.database,
            cx,
            program,
            policy,
            allocate,
        )
    }
    /// These typed adapters never expose the temporarily selected database.
    /// The owner was admitted before reconstruction and immediately takes its
    /// state again, so failure or unwind discards the entire private prefix.
    pub(crate) fn stage_authorized_chain<Clock: FnMut() -> u64>(
        &mut self,
        owner: &mut AuthorizedWriteTransaction<'_, Clock>,
        cx: &QueryCx,
        statement: &PreparedGraphWriteChain,
        allocator: &mut dyn FnMut(GraphInsertRequest) -> Result<ElementId, WriteTxnError>,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, AuthorizedFault> {
        owner.stage_chain_selected(self.database, cx, statement, allocator, admit)
    }

    pub(crate) fn read_authorized<Clock: FnMut() -> u64>(
        &mut self,
        owner: &mut AuthorizedWriteTransaction<'_, Clock>,
        cx: &QueryCx,
        statement: &PreparedGraphSet,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<GqlQueryExecution<GraphValueRow>, AuthorizedFault> {
        owner.read_selected(self.database, cx, statement, admit)
    }

    pub(crate) fn read_authorized_aggregate<Clock: FnMut() -> u64>(
        &mut self,
        owner: &mut AuthorizedWriteTransaction<'_, Clock>,
        cx: &QueryCx,
        statement: &PreparedGraphSetAggregate,
        admit: impl FnOnce(
            &[GraphAggregateRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<GqlQueryExecution<GraphAggregateRow>, AuthorizedFault> {
        owner.read_aggregate_selected(self.database, cx, statement, admit)
    }

    pub(crate) fn control_authorized<Clock: FnMut() -> u64>(
        &mut self,
        owner: &mut AuthorizedWriteTransaction<'_, Clock>,
        cx: &QueryCx,
        control: &PreparedGraphTransactionControl,
        admit: impl FnOnce(
            &[GraphValueRow],
            &mut dyn FnMut(u64) -> Result<(), WriteTxnError>,
            &mut dyn AuthorizedWriteOutput,
        ) -> Result<(), WriteTxnError>,
    ) -> Result<(), AuthorizedFault> {
        owner.control_selected(self.database, cx, control, admit)
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Read-only opens retain no extra writer clone. The first commit or
    /// compaction preserves the exact open state before replacing it, allowing
    /// later stable-basis staging to replay only this handle's loaded suffix.
    pub(crate) fn retain_preparation_anchor(&mut self) {
        if self.delta_materialized_after != CommitSeq::ORIGIN && self.preparation_anchor.is_none() {
            self.preparation_anchor = Some(PreparationAnchor {
                basis: self.snapshot.frontier,
                writer: self.writer.clone(),
                heads: self.heads.clone(),
            });
        }
    }

    /// Select a preparation basis without issuing a new read view, changing an
    /// ownership token, or touching the current publication/commit coordinator.
    /// All reconstruction succeeds before any handle state is displaced.
    pub(crate) fn preparation_basis(
        &mut self,
        basis: CommitSeq,
    ) -> Result<PreparationBasis<'_, V>, WriteTxnError> {
        self.preparation_basis_controlled(basis, || Ok(()))
    }

    /// Checkpoints surround native replay operations and the snapshot copy.
    /// A single row application, cascade, seal or clone remains indivisible;
    /// this is cooperative cancellation, not a memory or CPU-work bound.
    /// No checkpoint follows selection: all failure paths leave live state
    /// untouched, and the returned scope restores it on return or unwind.
    pub(crate) fn preparation_basis_controlled(
        &mut self,
        basis: CommitSeq,
        mut checkpoint: impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<PreparationBasis<'_, V>, WriteTxnError> {
        self.ensure_writable()?;
        self.snapshot.check_frontier(basis)?;
        checkpoint()?;
        let original = if basis == self.snapshot.frontier {
            None
        } else {
            // A lazy-open handle preserves its exact original native state
            // before the first post-open mutation. No post-basis identity,
            // property or version head can enter this seed. Older cuts still
            // require the complete origin prefix and explicitly refuse when it
            // has not been materialized.
            let (after, mut writer, mut versions, mut births) = match self
                .preparation_anchor
                .as_ref()
                .filter(|anchor| anchor.basis <= basis)
            {
                Some(anchor) => {
                    checkpoint()?;
                    (
                        anchor.basis,
                        anchor.writer.clone(),
                        anchor.heads.versions.clone(),
                        anchor.heads.next_birth_ordinal,
                    )
                }
                None => (
                    CommitSeq::ORIGIN,
                    BlockWriter::new(crate::GRAPH, crate::BRANCH, crate::PARTITION),
                    VersionHeads::default(),
                    0_u64,
                ),
            };
            let history = self
                .snapshot
                .delta_index
                .since(after)
                .map_err(WriteError::PreparedHistory)?;
            let mut touched = Vec::new();
            for batch in history.take_while(|batch| batch.commit_seq().0 <= basis.0) {
                checkpoint()?;
                let at = batch.commit_seq();
                for coordinate in batch.coordinate_entries() {
                    checkpoint()?;
                    if (coordinate.graph, coordinate.branch) != (crate::GRAPH, crate::BRANCH) {
                        continue;
                    }
                    for row in &coordinate.rows {
                        checkpoint()?;
                        // Catalog effects were admitted when this immutable
                        // history was loaded. They do not mutate graph rows;
                        // the exact admitted catalog is selected below.
                        if matches!(row, DeltaRow::Schema { .. }) {
                            continue;
                        }
                        writer
                            .apply(self.keys.block_keys(), at, row)
                            .map_err(|error| basis_rebuild(at, error))?;
                        crate::touched_elements(row, &mut touched);
                        if versions.admit_creation(row) {
                            births += 1;
                        }
                    }
                }
                checkpoint()?;
                touched.sort_unstable();
                touched.dedup();
                crate::fold_statement_versions(&mut versions, &touched, &writer).map_err(
                    |error| {
                        WriteTxnError::BasisRebuild(Box::new(RebuildError::Version {
                            commit_seq: at.0,
                            error,
                        }))
                    },
                )?;
                touched.clear();
                checkpoint()?;
                // Match the native per-commit seal law. The writer's live and
                // permanently-spent identity state is the production fold's,
                // including creations subsequently deleted before this basis.
                let _ = writer
                    .seal(self.keys.block_keys())
                    .map_err(|error| basis_rebuild(at, error))?;
                checkpoint()?;
                let _ = writer
                    .seal_vertices(self.keys.block_keys())
                    .map_err(|error| basis_rebuild(at, error))?;
            }
            // Historical lookup in these authenticated blocks/patches already
            // selects by frontier. Versions and spent identities, unlike read
            // lookup, must be reconstructed without any post-basis statements.
            checkpoint()?;
            let schema = self
                .snapshot
                .schema
                .at(basis)
                .map_err(|error| WriteTxnError::BasisRebuild(Box::new(error)))?;
            let mut snapshot = (*self.snapshot).clone();
            snapshot.frontier = basis;
            snapshot.schema = schema;
            let snapshot = Arc::new(snapshot);
            let heads = WriteHeads {
                versions,
                next_birth_ordinal: births,
            };
            checkpoint()?;
            Some((
                std::mem::replace(&mut self.writer, writer),
                std::mem::replace(&mut self.heads, heads),
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
    /// delta prefix or the exact lazy-open anchor plus its suffix, and copies
    /// snapshot metadata. Cuts before that anchor require explicit history
    /// materialization with [`Self::ensure_delta_window`] at [`CommitSeq::ORIGIN`]
    /// so the entire prefix needed to reconstruct the writer is available.
    /// This is an in-memory
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
