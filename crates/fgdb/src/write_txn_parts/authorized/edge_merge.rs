//! Directed relationship get-or-create on the same native MERGE collector.
//! Selection is masked; existence candidates are confined to its admitted pair.

use super::super::collect_edge_merge;
use super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxn,
    WriteTxnError, redacted, selection, stage,
};
use fgdb_gql::insertion::GraphInsertRequest;
use fgdb_gql::{
    GqlQueryError, GraphEdgeMergeError, GraphEdgeMergeOutcome, GraphEdgeMergePolicy,
    GraphEdgeMergeStats, PreparedGraphEdgeMerge,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

type Fault = GqlQueryError<GraphEdgeMergeError<WriteTxnError, WriteTxnError>, WriteTxnError>;
type Receipt = (GraphEdgeMergeStats, GraphEdgeMergeOutcome, EmbeddedTxnCompletion);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphEdgeMergeError::Source(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute directed relationship MERGE atomically under ReadWrite authority.
    ///
    /// The ordinary native collector reduces duplicate endpoint occurrences,
    /// refuses null or multiple distinct pairs, and distinguishes NoInput,
    /// Matched and Created. It sees only the exact authorized relation/pair's
    /// canonical live EIds. Parallel EIds remain ambiguous, never an arbitrary
    /// first match or an ensure alias. Both endpoints are selected through the
    /// same masked GLA source as other authorized writes and rechecked before
    /// existence inspection. Hidden endpoint data, properties and other
    /// relations cannot alter that inspection's logical resource counts.
    ///
    /// Creation uses an engine-reserved EId and original before/after image
    /// authorization. An existing match preserves all fields, including hidden
    /// properties. Initialization properties apply ONLY on creation, not as an
    /// existing-edge predicate. The requested relation must be permitted even
    /// when selection would be empty; this is a capability-only preflight.
    ///
    /// One live permit covers selection, reduction, creation and final native
    /// completion. Each Matched/Created receipt costs one signed row BEFORE
    /// publication; NoInput costs none. Matched and NoInput close read-only.
    /// Native unknown/recovery outcomes are unchanged; no fallible permission
    /// check or receipt construction follows publication.
    ///
    /// The native edge overlay still materializes resident storage and retains
    /// its conservative scan/point dependencies. Its physical traversal polls
    /// cancellation without billing hidden records; this is NOT bounded-memory
    /// execution, descriptor-I/O isolation or timing noninterference. Uniqueness
    /// is local to the selected visible pair, not a global constraint. The host
    /// owns issuer, branch routing and monotone clock; raw APIs remain trusted.
    /// Engine reservations are not durable leases or reclaimed after refusal;
    /// sequential allocation metadata, durable revocation/audit and full SSI
    /// remain outside this surface.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_edge_merge_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        merge: &PreparedGraphEdgeMerge,
        policy: GraphEdgeMergePolicy,
        mut clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        let refusal = |error| source(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        if !verified.predicates().rights().can_read() {
            return Err(refusal(Error::PermissionDenied));
        }
        if !verified.predicates().allows_relation(merge.relation()) {
            return Err(refusal(Error::ScopeDenied));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution { cx: commit_cx, permit, clock };
                execution.checkpoint().map_err(source)?;
                let mut workspace = Workspace(Some(self.begin(txn_cx).map_err(|error| {
                    source(WriteTxnError::Write(error))
                })?));
                let (stats, outcome) = apply(
                    workspace.transaction(), self, query_cx, merge, policy,
                    verified.predicates(), &mut execution, true,
                )?;
                let completion = workspace.transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(source)?;
                Ok((stats, outcome, completion))
            })
            .await
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply<V: Vfs + Clone, Clock: FnMut() -> u64>(
    transaction: &mut WriteTxn,
    database: &mut Database<V>,
    cx: &QueryCx,
    merge: &PreparedGraphEdgeMerge,
    policy: GraphEdgeMergePolicy,
    scope: &PlannerPredicates,
    execution: &mut Execution<'_, '_, Clock>,
    returning: bool,
) -> Result<(GraphEdgeMergeStats, GraphEdgeMergeOutcome), Fault> {
    if !scope.allows_relation(merge.relation()) {
        return Err(source(WriteTxnError::Authorization(Error::ScopeDenied)));
    }
    let proposal = cx.with_restriction(|| {
        let database = RefCell::new(&mut *database);
        let controls = RefCell::new(&mut *execution);
        collect_edge_merge(
            merge,
            policy,
            |pattern, allowance| {
                selection::select_overlay(
                    transaction, &database.borrow(), cx, pattern, scope, allowance, &controls,
                )
            },
            |src, dst| {
                // The selected pair is the entire existence domain. Recheck
                // original endpoints, never a client-selected or masked image.
                for vertex in [src, dst] {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)
                        .map_err(GqlQueryError::Interrupted)?;
                    if controls.borrow_mut().vertex(transaction, &database.borrow(), vertex)
                        .map_err(GqlQueryError::Source)?.is_none()
                    {
                        return Err(GqlQueryError::Source(WriteTxnError::Authorization(
                            Error::ScopeDenied,
                        )));
                    }
                }
                // Retain the native scan witness even when no edge exists.
                // Raw records stay private. Only exact pair/relation candidates
                // reach the shared collector's counts and uniqueness reducer;
                // every such edge has the two admitted endpoints above.
                let edges = transaction.edges(&database.borrow())
                    .map_err(redacted).map_err(GqlQueryError::Source)?;
                let mut admitted = Vec::new();
                for record in edges {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted)
                        .map_err(GqlQueryError::Interrupted)?;
                    controls.borrow_mut().poll().map_err(GqlQueryError::Interrupted)?;
                    if record.entry.src == src && record.entry.dst == dst
                        && record.entry.relation == merge.relation()
                    {
                        admitted.push(record);
                    }
                }
                Ok(admitted)
            },
            |_| database.borrow_mut().allocate_identity(cx, GraphInsertRequest::Edge { row: 0, edge: 0 }),
            || {
                cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                controls.borrow_mut().checkpoint()
            },
        )
    })?;
    if let Some(batch) = proposal.creation {
        for row in batch.rows {
            cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(source)?;
            execution.checkpoint().map_err(source)?;
            stage(transaction, database, batch.relation, row, execution).map_err(source)?;
        }
    }
    if returning && proposal.outcome.edge().is_some() {
        execution.permit.charge_rows_at((execution.clock)(), 1)
            .map_err(|error| source(WriteTxnError::Authorization(error)))?;
    }
    Ok((proposal.stats, proposal.outcome))
}

#[cfg(test)]
#[path = "edge_merge_tests.rs"]
mod tests;
