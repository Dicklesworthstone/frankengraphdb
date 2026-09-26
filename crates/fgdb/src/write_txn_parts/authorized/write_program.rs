//! Mixed CRUD programs use the existing program meter and per-statement
//! authorized writers. No step publishes or gets an independent capability.

use super::super::super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxnError,
    deletion, insert, mutation,
};
use fgdb_gql::{
    GqlQueryError, GraphMutationError, GraphMutationProgramError, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteProgramReceipt, GraphWriteProgramStats, GraphWriteStatement,
    GraphWriteStepError, GraphWriteStepReceipt, GraphWriteStepStats, PreparedGraphWriteProgram,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use std::cell::RefCell;

#[path = "vertex_merge.rs"]
mod vertex;

type Fault = GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

fn preflight(error: WriteTxnError) -> Fault {
    GraphMutationProgramError::Preflight(error).into()
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute an atomic authorized CRUD program over the canonical native
    /// transaction overlay. Supports INSERT/CREATE, SET/REMOVE/DETACH DELETE
    /// plain DELETE, unique vertex MERGE and ON MATCH/ON CREATE vertex actions.
    /// A later statement observes preceding checked effects;
    /// each individual statement still freezes its MATCH and expressions before
    /// staging. Every field/image and every plain-DELETE incidence proof uses
    /// the same rules as the corresponding authorized single-statement method.
    ///
    /// All statements share one live permit and the existing cumulative native
    /// program meter. Creations later deleted still consume creation allowance;
    /// updates later reversed still consume mutation allowance. Nothing commits
    /// until every statement and final validation succeed. Failure or unwinding
    /// discards the whole unpublished workspace. Empty selections may read-close.
    /// Stats are control receipts, so no signed result-row allowance is needed.
    ///
    /// An insert-only program with no MATCH requires Write rights. Any selected
    /// creation, update or deletion requires ReadWrite, even when no rows match.
    /// Vertex MERGE always requires ReadWrite. Uniqueness is checked only among
    /// visible matches: hidden vertices never suppress creation or cause an
    /// ambiguous-match error. This is visible get-or-create, NOT a global
    /// uniqueness constraint or a disclosure contract for hidden constraints.
    /// The ordinary native reducer decides the branch once; only the chosen
    /// ON MATCH/ON CREATE actions are lowered and authorized. Both outcomes
    /// cost one delivered identity, not one row per action or creation callback.
    /// Relationship MERGE/upsert remain refused during whole-program preflight,
    /// before any database read, pin or identity reservation. No privileged
    /// existence/uniqueness path is used as an authorization fallback.
    /// Relation coordinates can differ; native ordered composition remains the
    /// only writer. No caller-selected identity or allocator callback is exposed.
    ///
    /// Engine ID reservations are not reclaimed on failure and are not durable
    /// leases. Sequential ID allocation still exposes allocation-order metadata.
    /// Native overlay materialization and preparation remain resident operations,
    /// not bounded-memory or preemptible query execution. This is not a mandatory
    /// facade over privileged APIs, durable audit/revocation protocol or full SSI.
    /// The host owns issuer, branch routing and monotone clock selection. Native
    /// unknown/recovery outcomes apply after the sole final commit admission;
    /// no fallible authorization or receipt construction follows publication.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_program_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        program: &PreparedGraphWriteProgram,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.write_program_authorized_inner(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            program,
            policy,
            clock,
            false,
            |stats, _| stats,
        )
        .await
    }

    /// Return source-ordered per-statement receipts only after the entire
    /// program completes. Every identity occurrence in those receipts consumes
    /// one signed row before publication. A repeated target in two statements
    /// costs two rows; duplicate MATCH targets within one mutation cost one.
    /// Creation receipts may name identities deleted by a later statement:
    /// these are execution receipts, not a promise that every ID remains live.
    /// Receipt vectors are constructed before final admission, never afterward.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_write_program_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        program: &PreparedGraphWriteProgram,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramReceipt, EmbeddedTxnCompletion), Fault> {
        self.write_program_authorized_inner(
            txn_cx,
            query_cx,
            commit_cx,
            authority,
            token,
            branch,
            program,
            policy,
            clock,
            true,
            GraphWriteProgramReceipt::new,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn write_program_authorized_inner<Receipt>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        program: &PreparedGraphWriteProgram,
        policy: GraphWriteProgramPolicy,
        mut clock: impl FnMut() -> u64,
        returning: bool,
        receipt: impl FnOnce(GraphWriteProgramStats, Vec<GraphWriteStepReceipt>) -> Receipt,
    ) -> Result<(Receipt, EmbeddedTxnCompletion), Fault> {
        let refusal = |error| preflight(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        commit_cx
            .with_restriction_async(async {
                let mut execution = Execution {
                    cx: commit_cx,
                    permit,
                    clock,
                };
                // Validate the COMPLETE immutable shape before even opening a
                // workspace. A valid prefix cannot allocate IDs ahead of an
                // unsupported tail, and zero work cannot bypass required rights.
                query_cx.with_restriction(|| {
                    for statement in program.statements() {
                        query_cx
                            .checkpoint()
                            .map_err(WriteTxnError::Interrupted)
                            .map_err(preflight)?;
                        execution.poll().map_err(preflight)?;
                        let reads = match statement {
                            GraphWriteStatement::Insert(input) => input.selection().is_some(),
                            GraphWriteStatement::Mutation(_)
                            | GraphWriteStatement::Delete(_)
                            | GraphWriteStatement::VertexMerge(_)
                            | GraphWriteStatement::VertexUpsert(_) => true,
                            GraphWriteStatement::EdgeMerge(_)
                            | GraphWriteStatement::EdgeUpsert(_) => {
                                return Err(preflight(WriteTxnError::AuthorizedMutationRefused));
                            }
                        };
                        if reads && !verified.predicates().rights().can_read() {
                            return Err(refusal(Error::PermissionDenied));
                        }
                    }
                    Ok(())
                })?;
                execution.checkpoint().map_err(preflight)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(WriteTxnError::Write)
                        .map_err(preflight)?,
                ));
                workspace.transaction().program_multi_relation = true;
                let mut receipts = Vec::new();
                let stats = query_cx.with_restriction(|| {
                    let execution = RefCell::new(&mut execution);
                    program.execute_governed(
                        policy,
                        |_, statement, remaining| {
                            let mut borrowed = execution.borrow_mut();
                            let execution = &mut **borrowed;
                            let (stats, step) = match statement {
                                GraphWriteStatement::Mutation(input) => {
                                    let (stats, targets, edges) = mutation::apply(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.mutations,
                                        verified.predicates(),
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::Mutation)?;
                                    (
                                        GraphWriteStepStats::Mutation(stats),
                                        GraphWriteStepReceipt::Mutation { targets, edges },
                                    )
                                }
                                GraphWriteStatement::Insert(input) => {
                                    let (stats, vertices, edges) = insert::apply(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.insertion_policy(),
                                        verified.predicates(),
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::Insert)?;
                                    (
                                        GraphWriteStepStats::Insert(stats),
                                        GraphWriteStepReceipt::Insert { vertices, edges },
                                    )
                                }
                                GraphWriteStatement::Delete(input) => {
                                    let (stats, targets, edges) = deletion::apply(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.deletion_policy(),
                                        verified.predicates(),
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::Delete)?;
                                    (
                                        GraphWriteStepStats::Delete(stats),
                                        GraphWriteStepReceipt::Delete { targets, edges },
                                    )
                                }
                                GraphWriteStatement::VertexMerge(input) => {
                                    let (stats, outcome) = vertex::merge(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.vertex_merge_policy(),
                                        verified.predicates(),
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::VertexMerge)?;
                                    (
                                        GraphWriteStepStats::VertexMerge(stats),
                                        GraphWriteStepReceipt::VertexMerge { outcome },
                                    )
                                }
                                GraphWriteStatement::VertexUpsert(input) => {
                                    let (stats, outcome) = vertex::upsert(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.vertex_upsert_policy(),
                                        verified.predicates(),
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::VertexUpsert)?;
                                    (
                                        GraphWriteStepStats::VertexUpsert(stats),
                                        GraphWriteStepReceipt::VertexUpsert { outcome },
                                    )
                                }
                                GraphWriteStatement::EdgeMerge(_)
                                | GraphWriteStatement::EdgeUpsert(_) => {
                                    // Preflight excludes these immutable arms. Keep
                                    // the dispatch fail-closed, never a privileged
                                    // fallback, even if its caller is later changed.
                                    return Err(GraphWriteStepError::Mutation(
                                        GqlQueryError::Source(GraphMutationError::Source(
                                            WriteTxnError::AuthorizedMutationRefused,
                                        )),
                                    ));
                                }
                            };
                            if returning {
                                // Identities were admitted by the statement helper.
                                // This fixed-shape envelope is bounded by the native
                                // prepared program's admitted statement count.
                                receipts.push(step);
                            }
                            Ok(stats)
                        },
                        || {
                            query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                            execution.borrow_mut().checkpoint()
                        },
                    )
                })?;
                let completed_statements = stats.completed_statements;
                // Both public APIs build their final value here. There is no
                // allocation, optional-receipt unwrap or auth check after commit.
                let receipt = receipt(stats, receipts);
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(|source| {
                        Fault::Program(GraphMutationProgramError::Interrupted {
                            completed_statements,
                            source,
                        })
                    })?;
                Ok((receipt, completion))
            })
            .await
    }
}

#[cfg(test)]
#[path = "write_program_tests.rs"]
mod tests;
