//! Atomic, capability-scoped embedded writes over the native transaction spine.
//!
//! Original intents are checked one at a time, before normalization can erase
//! attempted field writes. Before/after images come from the native pinned
//! overlay, never from a second mutation interpreter. The workspace and permit
//! stay private until native completion; no reusable unchecked write escapes.

use super::{WriteTxn, WriteTxnError};
use crate::{Database, PendingRow, VertexRow, WriteBatch, WriteError};
use asupersync::fs::Vfs;
use fgdb_delta_types::{ElementId, LabelId, PropertyKeyId};
use fgdb_types::{CommitCx, CommitSeq, EmbeddedTxnCompletion, TxnCx, VId};
use fgdb_warden::{
    Authority, CapabilityToken, Error, ExecutionPermit, VertexWriteFields, VertexWriteImage,
    WriteAccess,
};
use std::collections::BTreeSet;

// This file is loaded through #[path] from write_txn.rs, so its children resolve
// like mod.rs children (in write_txn_parts/). Name the real location.
#[path = "authorized/graph.rs"]
mod graph;
#[path = "authorized/insert.rs"]
mod insert;
#[path = "authorized/mutation.rs"]
mod mutation;
#[path = "authorized/selection.rs"]
mod selection;

#[cfg(test)]
#[path = "authorized/ordered_tests.rs"]
mod ordered_tests;

#[cfg(test)]
#[path = "authorized/target_tests.rs"]
mod target_tests;

struct Workspace(Option<WriteTxn>);
impl Workspace {
    fn transaction(&mut self) -> &mut WriteTxn {
        self.0
            .as_mut()
            .expect("workspace owns its transaction until drop")
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        if let Some(transaction) = self.0.take() {
            transaction.abort();
        }
    }
}

struct Execution<'cx, 'permit, Clock> {
    cx: &'cx CommitCx,
    permit: ExecutionPermit<'permit, WriteAccess>,
    clock: Clock,
}
impl<Clock: FnMut() -> u64> Execution<'_, '_, Clock> {
    fn work(&mut self, amount: u64) -> Result<(), WriteTxnError> {
        self.cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
        self.permit
            .charge_work_at((self.clock)(), amount)
            .map_err(WriteTxnError::Authorization)
    }

    fn checkpoint(&mut self) -> Result<(), WriteTxnError> {
        self.work(1)
    }

    /// Cancellation only, no charge: for walking candidates the capability
    /// may not see, so their count never reaches a signed limit (FG-INV-20).
    fn poll(&mut self) -> Result<(), WriteTxnError> {
        self.cx.checkpoint().map_err(WriteTxnError::Interrupted)
    }

    fn fields(
        &mut self,
        labels: impl IntoIterator<Item = LabelId>,
        properties: impl IntoIterator<Item = PropertyKeyId>,
    ) -> Result<Fields, WriteTxnError> {
        let mut admitted_labels = BTreeSet::new();
        let mut admitted_properties = BTreeSet::new();
        for label in labels {
            self.checkpoint()?;
            if !self.permit.predicates().allows_label(label) {
                return Err(denied());
            }
            admitted_labels.insert(label);
        }
        for property in properties {
            self.checkpoint()?;
            if !self.permit.predicates().allows_property(property) {
                return Err(denied());
            }
            admitted_properties.insert(property);
        }
        Ok(Fields {
            labels: admitted_labels.into_iter().collect(),
            properties: admitted_properties.into_iter().collect(),
        })
    }

    fn vertex<V: Vfs + Clone>(
        &mut self,
        transaction: &WriteTxn,
        database: &Database<V>,
        vid: VId,
    ) -> Result<Option<VertexRow>, WriteTxnError> {
        self.checkpoint()?;
        let row = transaction.vertex(database, vid).map_err(redacted)?;
        // A hidden target and an absent target must reach ScopeDenied after
        // the same signed lookup charge. Do not let image-validation work or
        // node charges turn a caller-controlled limit into an existence probe.
        // Keep ORIGINAL admitted rows: masking an after-image could conceal a
        // forbidden scope escape or a change to a preserved hidden property.
        if row
            .as_ref()
            .is_some_and(|row| !self.permit.predicates().allows_vertex(&row.labels))
        {
            return Err(denied());
        }
        Ok(row)
    }

    fn check_vertex(
        &mut self,
        before: Option<&VertexRow>,
        after: Option<&VertexRow>,
        fields: &Fields,
    ) -> Result<(), WriteTxnError> {
        self.checkpoint()?;
        self.permit
            .check_vertex_write_at(
                (self.clock)(),
                before.map(vertex_image),
                after.map(vertex_image),
                fields.borrow(),
            )
            .map_err(WriteTxnError::Authorization)
    }
}

struct Fields {
    labels: Vec<LabelId>,
    properties: Vec<PropertyKeyId>,
}
impl Fields {
    fn borrow(&self) -> VertexWriteFields<'_> {
        VertexWriteFields {
            labels: &self.labels,
            properties: &self.properties,
        }
    }
}
fn vertex_image(row: &VertexRow) -> VertexWriteImage<'_> {
    VertexWriteImage {
        id: row.vid,
        labels: &row.labels,
        properties: &row.props,
    }
}
fn denied() -> WriteTxnError {
    WriteTxnError::Authorization(Error::ScopeDenied)
}
fn redacted(_: WriteTxnError) -> WriteTxnError {
    // Native preparation errors may carry CAS.actual or protected identities.
    // This is ONLY for prepublication observations/preparation, never completion.
    WriteTxnError::AuthorizedMutationRefused
}

fn stage_vertex<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    relation: fgdb_delta_types::RelationId,
    row: PendingRow,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    let (vid, creates, fields) = match &row {
        PendingRow::Vertex {
            vid, labels, props, ..
        } => (
            *vid,
            true,
            execution.fields(labels.iter().copied(), props.iter().map(|(key, _)| *key))?,
        ),
        PendingRow::SetLabel { vid, label, .. } => (*vid, false, execution.fields([*label], [])?),
        PendingRow::SetProperty { vid, key, .. }
        | PendingRow::CompareAndSet {
            elem: ElementId::Vertex(vid),
            key,
            ..
        } => (*vid, false, execution.fields([], [*key])?),
        _ => return Err(WriteTxnError::AuthorizedMutationRefused),
    };
    let before = execution.vertex(transaction, database, vid)?;
    if let Some(before) = &before {
        // Admit the original target and ALL attempted fields before the native
        // evaluator can run a conditional comparison or derive any effects.
        execution.check_vertex(Some(before), Some(before), &fields)?;
    } else if !creates {
        // Hidden and absent update targets have the same public refusal.
        return Err(denied());
    } else if let PendingRow::Vertex { labels, .. } = &row {
        execution.checkpoint()?;
        execution
            .permit
            .charge_nodes_at((execution.clock)(), 1)
            .map_err(WriteTxnError::Authorization)?;
        if !execution.permit.predicates().allows_vertex(labels) {
            return Err(denied());
        }
    }
    stage_native(transaction, database, relation, row, execution)?;
    let after = execution.vertex(transaction, database, vid)?;
    execution.check_vertex(before.as_ref(), after.as_ref(), &fields)
}

fn stage_native<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    relation: fgdb_delta_types::RelationId,
    row: PendingRow,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    // Native staging replays the entire intent prefix. Charge that logical
    // input work, not merely one unit for a growing quadratic preparation.
    let prefix = u64::try_from(transaction.staged.len())
        .ok()
        .and_then(|len| len.checked_add(1))
        .ok_or(WriteTxnError::Authorization(Error::TooLarge))?;
    execution.work(prefix)?;
    let batch = WriteBatch {
        relation,
        rows: vec![row],
    };
    if transaction
        .staged
        .first()
        .is_some_and(|first| first.relation != relation)
    {
        // Introduce a dependent relation explicitly. Subsequent ordinary
        // writes already retain ordered composition once the prefix spans
        // relations. Never prepare independent groups: a later intent must
        // observe the exact prefix whose before/after images we authorized.
        transaction
            .write_ordered(database, vec![batch])
            .map_err(redacted)
    } else {
        // Preserve the existing single-relation evaluator and birth ordering.
        transaction.write(database, batch).map_err(redacted)
    }
}

fn stage<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    relation: fgdb_delta_types::RelationId,
    row: PendingRow,
    execution: &mut Execution<'_, '_, Clock>,
) -> Result<(), WriteTxnError> {
    // An added intent family must acquire an explicit authorization rule.
    match &row {
        PendingRow::Vertex { .. }
        | PendingRow::SetLabel { .. }
        | PendingRow::SetProperty { .. }
        | PendingRow::CompareAndSet {
            elem: ElementId::Vertex(_),
            ..
        } => stage_vertex(transaction, database, relation, row, execution),
        PendingRow::Edge { .. }
        | PendingRow::DeleteEdge { .. }
        | PendingRow::SetEdgeProperty { .. }
        | PendingRow::CompareAndSet {
            elem: ElementId::Edge(_),
            ..
        } => graph::stage_edge(transaction, database, relation, row, execution),
        PendingRow::DeleteVertex { .. } => {
            graph::delete_vertex(transaction, database, relation, row, execution)
        }
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Commit one capability-scoped graph write batch through native Chronicle.
    ///
    /// Supports every native WriteBatch intent: vertex/edge creation and ensure,
    /// label/property updates, CAS and deletion. Every cascade edge is checked
    /// with both original endpoints; whole-object deletion needs authority over
    /// every removed field. Vertex deletion also needs a capability that hides
    /// nothing (no relation scope, label clause, property scope or denial) and
    /// is refused before any observation otherwise, so the refusal cannot reveal
    /// hidden incidence or fields. Missing and hidden non-create targets both refuse
    /// ScopeDenied, including if-present deletes and removal of absent targets.
    /// Edge deletion requires an unrestricted property scope with no property
    /// denials, checked before any target read; otherwise even a property-free
    /// or absent edge is refused. Label/relation scopes may still restrict an
    /// edge delete because its endpoints and other incidence are preserved.
    /// The trusted host supplies its current issuer, exact branch routing and a
    /// monotone issuer-epoch clock. Never expose the raw Database or Authority
    /// to token holders. Namespace/signature/rights checks precede observation.
    ///
    /// One non-cloneable allowance covers original intent checks, native prefix
    /// preparation, actual before/after images and final conflict validation.
    /// Forbidden attempted fields refuse even when ensure/CAS/normalization
    /// would produce no effect. Untouched hidden fields may be preserved.
    /// Preparation refusals are redacted; no partial effects or rows escape.
    /// The commit sequence is a control receipt, not a row charged to max_rows.
    ///
    /// Expiry, retirement and cancellation are checked immediately before native
    /// commit admission. Once admitted, the ordinary commit outcome/unknown/
    /// recovery contract applies: there is NO post-commit authorization check
    /// that could misreport a durable write as refused. Dropping the future or
    /// unwinding releases the native workspace/pin without guessing rollback.
    ///
    /// This is resident-source, per-execution enforcement, not the complete
    /// durable Warden protocol, persistent revocation, lifetime quota, full SSI,
    /// timing isolation, or allocation/I/O preemption inside native preparation.
    #[allow(clippy::too_many_arguments)]
    pub async fn write_authorized(
        &mut self,
        txn_cx: &TxnCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        batch: WriteBatch,
        clock: impl FnMut() -> u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        self.write_ordered_authorized(
            txn_cx,
            commit_cx,
            authority,
            token,
            branch,
            vec![batch],
            clock,
        )
        .await
    }

    /// Commit source-ordered, dependent relation batches under one capability.
    ///
    /// Later batches see earlier creations, updates and ensure aliases through
    /// the native transaction overlay. Every original intent is authorized
    /// before and after staging; normalization cannot hide a forbidden no-op.
    /// Identity-addressed edge mutations authorize the edge's actual relation,
    /// not merely the batch's default relation. No unchecked prepared write or
    /// reusable transaction handle escapes to the caller.
    ///
    /// All batches share ONE execution permit, snapshot pin and native commit.
    /// Limits, expiry and retirement are not reset at batch boundaries. A
    /// refused tail discards the entire unpublished prefix. Completion keeps
    /// the ordinary unknown/recovery contract; it never reports a durable
    /// write as rolled back because authorization changed after admission.
    /// Empty input or an empty batch is refused after capability verification.
    /// Grouping adjacent same-relation intents does not change signed charges.
    ///
    /// This has the same security and residency boundaries as
    /// [`Self::write_authorized`], including its client-selected identity
    /// surface; it is not a zero-disclosure global uniqueness contract, a
    /// long-lived authorized transaction, full SSI or a second commit lane.
    #[allow(clippy::too_many_arguments)]
    pub async fn write_ordered_authorized(
        &mut self,
        txn_cx: &TxnCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        batches: Vec<WriteBatch>,
        mut clock: impl FnMut() -> u64,
    ) -> Result<CommitSeq, WriteTxnError> {
        if authority.namespace() != self.keys.namespace {
            return Err(WriteTxnError::Authorization(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(WriteTxnError::Authorization)?;
        let permit = verified
            .begin_write_at(branch, now)
            .map_err(WriteTxnError::Authorization)?;
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                execution.checkpoint()?;
                if batches.is_empty() {
                    return Err(WriteError::EmptyBatch.into());
                }
                // Poll structural input validation without making a batch
                // boundary consume another allowance. Every intent below
                // still pays its original authorization/preparation charges.
                for batch in &batches {
                    execution.poll()?;
                    if batch.is_empty() {
                        return Err(WriteError::EmptyBatch.into());
                    }
                }
                let mut workspace = Workspace(Some(self.begin(txn_cx)?));
                for batch in batches {
                    for row in batch.rows {
                        execution.checkpoint()?;
                        stage(
                            workspace.transaction(),
                            self,
                            batch.relation,
                            row,
                            &mut execution,
                        )?;
                    }
                }
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, true, || execution.checkpoint())
                    .await?;
                match completion {
                    EmbeddedTxnCompletion::WriteCommitted { commit_seq } => Ok(commit_seq),
                    EmbeddedTxnCompletion::ReadClosed { .. } => {
                        unreachable!("write-only completion refuses before read close")
                    }
                }
            })
            .await
    }
}
