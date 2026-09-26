//! Prepared creation through the existing collector and authorized native staging.
//! The collector owns expression semantics; the engine owns every allocated ID.
//! No unchecked proposal, identity receipt or workspace escapes before completion.

use super::{Authority, CapabilityToken, Database, Error, Execution, Vfs};
use super::{Workspace, WriteBatch, WriteTxn, WriteTxnError, selection, stage};
use fgdb_gql::GqlQueryError;
use fgdb_gql::insertion::{
    GraphInsertError, GraphInsertIntent, GraphInsertPolicy, GraphInsertStats, PreparedGraphInsert,
};
use fgdb_types::{CommitCx, EId, EmbeddedTxnCompletion, QueryCx, TxnCx, VId};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

// Keep the collector's typed resource/value errors. Its interruption carrier
// also transports live authorization refusals from the one shared allowance.
type Fault = GqlQueryError<GraphInsertError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type Receipt = (GraphInsertStats, Vec<VId>, Vec<EId>, EmbeddedTxnCompletion);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphInsertError::Source(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute prepared INSERT/CREATE under one capability and one native commit.
    ///
    /// Expressions use the ordinary GLA insertion collector. Every resulting
    /// original creation is checked before/after native staging, including
    /// relation and endpoint scope. All relation groups publish in one native
    /// commit; a denied tail, quota failure, expiry or cancellation discards the
    /// private workspace. No caller-selected identity or allocator is accepted.
    /// Standalone creation needs Write rights. MATCH-selected creation requires
    /// ReadWrite rights and uses the SAME masked source/GLA body as authorized
    /// reads: hidden vertices/edges never enter matching, and expressions see
    /// only permitted properties/labels. The selection is frozen before any
    /// creation; it does not recursively select the vertices it just inserts.
    /// A zero-match statement allocates no IDs and completes ReadClosed without
    /// publishing a capsule, marker or new sequence.
    ///
    /// A single signed permit covers selection, collection, image admission, preparation
    /// and final validation. The supplied native policy additionally bounds
    /// evaluator work, scratch and creation counts. This stats-only operation
    /// delivers no identity rows and works with max_rows = 0. Stats and the
    /// completion value are control receipts, not graph-query result rows.
    ///
    /// Allocation uses the existing engine high-water allocator, including
    /// retired committed identities. Reservations are never reclaimed on this
    /// handle, even after refusal; uncommitted reservations are not durable ID
    /// leases. Sequential IDs expose allocation order: this API removes chosen
    /// collision probes, NOT that metadata channel or timing channels. It does
    /// not establish the final privacy-preserving allocator, mandatory secure
    /// view, persistent revocation, durable audit protocol or full SSI.
    ///
    /// The trusted host supplies the issuer, branch routing and monotone clock.
    /// Signature/rights checks precede database access. There is no fallible
    /// authorization check after native completion which could misreport a
    /// committed write as refused. The normal unknown/recovery contract applies.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_insert_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        insertion: &PreparedGraphInsert,
        policy: GraphInsertPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphInsertStats, EmbeddedTxnCompletion), Fault> {
        self.insert_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, insertion, policy, clock, false,
        )
        .await
        .map(|(stats, _, _, completion)| (stats, completion))
    }

    /// The same authorized creation, returning engine-issued identities only
    /// after successful completion. Ordering is occurrence then declaration,
    /// separately for vertices and edges, as on the native insertion surface.
    /// Each returned identity consumes one signed result-row unit BEFORE commit
    /// admission. An insufficient row allowance refuses the whole operation;
    /// it cannot commit successfully and then fail while constructing a receipt.
    /// Write-only authority may receive identities of its own checked creations.
    /// Existing graph reads are not granted by this receipt.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_insert_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        insertion: &PreparedGraphInsert,
        policy: GraphInsertPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        self.insert_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, insertion, policy, clock, true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_authorized_inner(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        insertion: &PreparedGraphInsert,
        policy: GraphInsertPolicy,
        mut clock: impl FnMut() -> u64,
        returning: bool,
    ) -> Result<Receipt, Fault> {
        if authority.namespace() != self.keys.namespace {
            return Err(source(WriteTxnError::Authorization(Error::WrongAuthority)));
        }
        let now = clock();
        let verified = authority
            .verify_at(token, branch, now)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        let permit = verified
            .begin_write_at(branch, now)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        if insertion.selection().is_some() && !verified.predicates().rights().can_read() {
            return Err(source(WriteTxnError::Authorization(
                Error::PermissionDenied,
            )));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                execution.checkpoint().map_err(source)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(|error| source(WriteTxnError::Write(error)))?,
                ));
                let (stats, vertices, edges) = apply(
                    workspace.transaction(), self, query_cx, insertion, policy,
                    verified.predicates(), &mut execution, returning,
                )?;
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)?;
                Ok((stats, vertices, edges, completion))
            })
            .await
    }
}

// Execute one step only. The caller owns the same private transaction, permit
// and eventual native completion; no per-step commit or fresh allowance exists.
#[allow(clippy::too_many_arguments)]
pub(super) fn apply<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    query_cx: &QueryCx,
    insertion: &PreparedGraphInsert,
    policy: GraphInsertPolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphInsertStats, Vec<VId>, Vec<EId>), Fault> {
    let proposal = query_cx.with_restriction(|| {
        // Sequential collector callbacks share the exact write
        // allowance and owner. These borrows die BEFORE staging or
        // awaiting completion, keeping the public future Send when
        // its clock/host are Send. Allocation starts only after the
        // masked selection and all computed values are validated.
        let execution = RefCell::new(&mut *execution);
        let database = RefCell::new(&mut *database);
        insertion.execute_governed(
            policy,
            |pattern, allowance| {
                selection::select_overlay(
                    transaction,
                    &database.borrow(),
                    query_cx,
                    pattern,
                    scope,
                    allowance,
                    &execution,
                )
            },
            |request| database.borrow_mut().allocate_identity(query_cx, request),
            || {
                query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                execution.borrow_mut().checkpoint()
            },
        )
    })?;
    let stats = proposal.stats();
    let mut vertices = Vec::new();
    let mut edges = Vec::new();
    for intent in proposal.into_intents() {
        query_cx
            .checkpoint()
            .map_err(WriteTxnError::Interrupted)
            .map_err(source)?;
        execution.checkpoint().map_err(source)?;
        // Receipt admission precedes both its allocation and native
        // publication. The stats-only path allocates no ID vectors.
        if returning {
            execution
                .permit
                .charge_rows_at((execution.clock)(), 1)
                .map_err(|error| source(WriteTxnError::Authorization(error)))?;
        }
        let batch = match intent {
            GraphInsertIntent::Vertex {
                vertex,
                labels,
                properties,
            } => {
                if returning {
                    vertices.push(vertex);
                }
                let mut batch = WriteBatch::new(insertion.relation());
                batch.create_vertex(vertex, labels, properties);
                batch
            }
            GraphInsertIntent::Edge {
                edge,
                relation,
                source,
                destination,
                properties,
            } => {
                if returning {
                    edges.push(edge);
                }
                let mut batch = WriteBatch::new(relation);
                batch.add_edge(edge, source, destination, properties);
                batch
            }
        };
        for row in batch.rows {
            stage(
                transaction,
                database,
                batch.relation,
                row,
                execution,
            )
            .map_err(source)?;
        }
    }
    Ok((stats, vertices, edges))
}

type MergeFault = super::super::VertexMergeFault<WriteTxnError, WriteTxnError, WriteTxnError>;
type MergeReceipt = (
    fgdb_gql::GraphVertexMergeStats,
    fgdb_gql::GraphVertexMergeOutcome,
    EmbeddedTxnCompletion,
);

fn merge_source(error: WriteTxnError) -> MergeFault {
    GqlQueryError::Source(fgdb_gql::GraphVertexMergeError::Source(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Get or create one visible vertex under one ReadWrite capability.
    ///
    /// The same GLA source as authorized reads masks the graph BEFORE matching.
    /// A hidden matching vertex is not an existing match; a missing visible
    /// match creates one engine-identified vertex through the ordinary insertion
    /// collector and before/after authorization. Repeated occurrences of one
    /// visible identity are one match; multiple visible identities refuse.
    /// Matching an existing vertex performs a read close, not an empty write.
    /// Selection, uniqueness and creation use the native MERGE collector and
    /// one cumulative native policy, never a second interpreter or fresh quota.
    ///
    /// The identity/outcome is returned only after native completion. Exactly
    /// one signed result-row unit is reserved before matching, independently of
    /// which branch is taken. Signature, namespace and ReadWrite rights are
    /// checked before opening a workspace or observing the graph. One permit
    /// covers matching, reduction, creation admission and completion; refusal,
    /// cancellation or expiry discards every unpublished effect and releases
    /// the pin. No fallible authorization follows native publication.
    ///
    /// Uniqueness is scoped to the admitted MATCH, NOT a global unique-key
    /// constraint or a privacy-preserving allocation protocol. The allocator,
    /// resident-source, timing, global-constraint and durable-outcome boundaries
    /// of authorized insertion still apply. This does not add full SSI,
    /// long-lived authorized transactions or ON MATCH/ON CREATE actions.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_vertex_merge_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        merge: &fgdb_gql::PreparedGraphVertexMerge,
        policy: fgdb_gql::GraphVertexMergePolicy,
        mut clock: impl FnMut() -> u64,
    ) -> Result<MergeReceipt, MergeFault> {
        if authority.namespace() != self.keys.namespace {
            return Err(merge_source(WriteTxnError::Authorization(Error::WrongAuthority)));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now)
            .map_err(|error| merge_source(WriteTxnError::Authorization(error)))?;
        let permit = verified.begin_write_at(branch, now)
            .map_err(|error| merge_source(WriteTxnError::Authorization(error)))?;
        if !verified.predicates().rights().can_read() {
            return Err(merge_source(WriteTxnError::Authorization(Error::PermissionDenied)));
        }
        commit_cx.with_restriction_async(async {
            let mut execution = Execution { cx: commit_cx, permit, clock };
            execution.checkpoint().map_err(merge_source)?;
            execution.permit.charge_rows_at((execution.clock)(), 1)
                .map_err(|error| merge_source(WriteTxnError::Authorization(error)))?;
            let mut workspace = Workspace(Some(
                self.begin(txn_cx).map_err(|error| merge_source(WriteTxnError::Write(error)))?,
            ));
            let proposal = query_cx.with_restriction(|| {
                // All RefCell borrows end before staging or the first await.
                // The permit is shared, not reissued for the creation arm.
                let execution = RefCell::new(&mut execution);
                let database = RefCell::new(&mut *self);
                super::super::collect_vertex_merge(
                    merge, policy,
                    |pattern, allowance| selection::select(
                        &database.borrow(), query_cx, pattern, verified.predicates(),
                        allowance, &execution,
                    ),
                    |request| database.borrow_mut().allocate_identity(query_cx, request),
                    || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.borrow_mut().checkpoint()
                    },
                )
            })?;
            if let Some(creation) = proposal.creation {
                for intent in creation.into_intents() {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(merge_source)?;
                    let GraphInsertIntent::Vertex { vertex, labels, properties } = intent else {
                        unreachable!("validated vertex MERGE contains no edge creation")
                    };
                    let mut batch = WriteBatch::new(merge.relation());
                    batch.create_vertex(vertex, labels, properties);
                    for row in batch.rows {
                        stage(workspace.transaction(), self, batch.relation, row, &mut execution)
                            .map_err(merge_source)?;
                    }
                }
            }
            let completion = workspace.transaction()
                .complete_controlled(self, commit_cx, None, false, || {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    execution.checkpoint()
                })
                .await
                .map_err(merge_source)?;
            Ok((proposal.stats, proposal.outcome, completion))
        }).await
    }
}

#[cfg(test)]
#[path = "insert_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "vertex_merge_tests.rs"]
mod vertex_merge_tests;
