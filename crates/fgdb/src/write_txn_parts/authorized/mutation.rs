//! Query-selected updates and detach deletion over the shared masked source,
//! ordinary simultaneous-assignment collector, and authorized native staging.

use super::{Authority, CapabilityToken, Database, Error, Execution, Vfs};
use super::{Workspace, WriteBatch, WriteTxnError, selection, stage};
use fgdb_delta_types::ElementId;
use fgdb_gql::{
    GqlQueryError, GraphMutationError, GraphMutationIntent, GraphMutationPolicy,
    GraphMutationStats, PreparedGraphMutation,
};
use fgdb_types::{CommitCx, EId, EmbeddedTxnCompletion, QueryCx, TxnCx, VId};
use std::cell::RefCell;
use std::collections::BTreeSet;

type Fault = GqlQueryError<GraphMutationError<WriteTxnError>, WriteTxnError>;
type Receipt = (GraphMutationStats, Vec<VId>, Vec<EId>, EmbeddedTxnCompletion);

fn source(error: WriteTxnError) -> Fault {
    GqlQueryError::Source(GraphMutationError::Source(error))
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute MATCH-selected SET, REMOVE, or DETACH DELETE under one capability.
    ///
    /// ReadWrite rights are required before database access, including empty
    /// selections. Matching and every RHS expression see the same masked graph
    /// as authorized reads. The ordinary mutation collector freezes selection,
    /// evaluates simultaneous assignments, ignores null OPTIONAL targets, and
    /// rejects conflicting duplicates before any native intent is staged.
    ///
    /// Every surviving element/field intent then passes original before/after
    /// image authorization, even when SET or REMOVE would normalize to a no-op.
    /// Hidden fields are preserved, never replaced with their masked read values.
    /// Edge updates authorize the actual relation and both original endpoints,
    /// not just the statement's default relation. A scope escape or refused tail
    /// discards the whole private workspace. DETACH DELETE retains the native
    /// rule requiring a capability that hides no labels, properties or relations;
    /// a restricted holder cannot probe hidden incidence through delete success.
    ///
    /// Selection, collection, staging and final validation share ONE signed
    /// allowance. The native policy separately bounds query work and effects.
    /// Stats are proposal/control counts, not graph result rows, so max_rows=0
    /// is supported. An empty proposal closes read-only without a new commit.
    /// Only native completion acknowledges publication; no fallible permission
    /// check follows commit admission. Unknown/recovery outcomes are unchanged.
    ///
    /// The trusted host owns issuer selection, branch routing and a monotone
    /// issuer-epoch clock. This is not a mandatory facade over privileged APIs,
    /// durable revocation/audit ordering, timing isolation or full SSI.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        mutation: &PreparedGraphMutation,
        policy: GraphMutationPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphMutationStats, EmbeddedTxnCompletion), Fault> {
        self.mutation_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, mutation, policy, clock,
            false,
        )
        .await
        .map(|(stats, _, _, completion)| (stats, completion))
    }

    /// Return distinct proposed target identities only after native completion.
    /// Vertex and edge IDs are independently sorted, just as on the ordinary
    /// returning mutation surface. Several fields or duplicate MATCH rows cost
    /// one result row per distinct target, not one per assignment. Admission
    /// precedes receipt allocation and publication: insufficient max_rows cannot
    /// commit successfully and then refuse the response. Equal-to-current SET
    /// targets are included; this is not a count of physically changed records.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        mutation: &PreparedGraphMutation,
        policy: GraphMutationPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<Receipt, Fault> {
        self.mutation_authorized_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch, mutation, policy, clock,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn mutation_authorized_inner(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        mutation: &PreparedGraphMutation,
        policy: GraphMutationPolicy,
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
        if !verified.predicates().rights().can_read() {
            return Err(source(WriteTxnError::Authorization(Error::PermissionDenied)));
        }
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution { cx: commit_cx, permit, clock };
                execution.checkpoint().map_err(source)?;
                let mut workspace = Workspace(Some(self.begin(txn_cx).map_err(|error| {
                    source(WriteTxnError::Write(error))
                })?));
                let proposal = query_cx.with_restriction(|| {
                    // These sequential callback borrows end before staging and
                    // before any await. No RefCell or query result escapes.
                    let execution = RefCell::new(&mut execution);
                    mutation.execute_governed(
                        policy,
                        |pattern, allowance| {
                            selection::select(
                                self, query_cx, pattern, verified.predicates(), allowance,
                                &execution,
                            )
                        },
                        || {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.borrow_mut().checkpoint()
                        },
                    )
                })?;
                let stats = proposal.stats();
                let mut targets = BTreeSet::new();
                for intent in proposal.into_intents() {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(source)?;
                    execution.checkpoint().map_err(source)?;
                    let mut batch = WriteBatch::new(mutation.relation());
                    let target = match intent {
                        GraphMutationIntent::Property { vertex, key, value } => {
                            batch.set_vertex_property(vertex, key, value);
                            ElementId::Vertex(vertex)
                        }
                        GraphMutationIntent::EdgeProperty { edge, key, value } => {
                            batch.set_edge_property(edge, key, value);
                            ElementId::Edge(edge)
                        }
                        GraphMutationIntent::Label { vertex, label, present } => {
                            batch.set_vertex_label(vertex, label, present);
                            ElementId::Vertex(vertex)
                        }
                        GraphMutationIntent::DetachDelete { vertex } => {
                            batch.delete_vertex(vertex);
                            ElementId::Vertex(vertex)
                        }
                    };
                    if returning {
                        // Price fixed-width tree lookup before touching it;
                        // its size is bounded by the admitted effect count.
                        let work = (usize::BITS - targets.len().leading_zeros()).max(1);
                        execution.work(u64::from(work)).map_err(source)?;
                        if !targets.contains(&target) {
                            execution.permit.charge_rows_at((execution.clock)(), 1)
                                .map_err(|error| source(WriteTxnError::Authorization(error)))?;
                            targets.insert(target);
                        }
                    }
                    for row in batch.rows {
                        stage(workspace.transaction(), self, batch.relation, row, &mut execution)
                            .map_err(source)?;
                    }
                }
                // Build the complete private receipt before commit admission.
                // The stats-only path allocated no target entries or vectors.
                let mut vertices = Vec::new();
                let mut edges = Vec::new();
                for target in targets {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(source)?;
                    execution.checkpoint().map_err(source)?;
                    match target {
                        ElementId::Vertex(vertex) => vertices.push(vertex),
                        ElementId::Edge(edge) => edges.push(edge),
                    }
                }
                let completion = workspace.transaction()
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

#[cfg(test)]
#[path = "mutation_tests.rs"]
mod tests;
