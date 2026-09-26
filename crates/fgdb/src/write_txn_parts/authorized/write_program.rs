//! Mixed CRUD programs use the existing program meter and per-statement
//! authorized writers. No step publishes or gets an independent capability.

use super::super::super::{
    Authority, CapabilityToken, Database, Error, Execution, Vfs, Workspace, WriteTxn, WriteTxnError,
    deletion, edge_merge, insert, mutation,
};
use fgdb_gql::{
    GraphMutationProgramError, GraphWriteProgramError,
    GraphWriteProgramPolicy, GraphWriteProgramReceipt, GraphWriteProgramStats, GraphWriteStatement,
    GraphWriteStepError, GraphWriteStepReceipt, GraphWriteStepStats, PreparedGraphWriteProgram,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;
use std::cell::RefCell;

#[cfg(test)]
use fgdb_gql::{GqlQueryError, GraphMutationError};

#[path = "edge_upsert.rs"]
mod edge_upsert;
#[path = "write_query.rs"]
mod query;
#[path = "write_script.rs"]
mod script;
#[path = "vertex_merge.rs"]
mod vertex;

type Fault = GraphWriteProgramError<WriteTxnError, WriteTxnError, WriteTxnError>;

fn preflight(error: WriteTxnError) -> Fault {
    GraphMutationProgramError::Preflight(error).into()
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute an atomic authorized CRUD program over the canonical native
    /// transaction overlay. Supports INSERT/CREATE, SET/REMOVE/DETACH DELETE
    /// plain DELETE, vertex/relationship MERGE and ON MATCH/ON CREATE actions.
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
    /// Relationship MERGE/upsert require ReadWrite and the requested relation
    /// during whole-program preflight, even for NoInput. Their existing native
    /// reducer sees only the selected authorized directed pair. Parallel EIds
    /// refuse; NoInput executes neither branch, allocates no identity and costs
    /// no result row. No privileged existence path is an authorization fallback.
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
        let mut execution = Execution { cx: commit_cx, permit, clock };
        self.complete_authorized_program(
            txn_cx, query_cx, commit_cx, program, policy, verified.predicates(),
            &mut execution, returning, receipt,
        ).await
    }

    // A script/batch binds privately under its SAME live permit, then enters
    // this exact preflight, dispatch and completion body. No second validation
    // allowance, workspace, graph writer or publication path is introduced.
    #[allow(clippy::too_many_arguments)]
    async fn complete_authorized_program<Receipt, Clock: FnMut() -> u64>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        program: &PreparedGraphWriteProgram,
        policy: GraphWriteProgramPolicy,
        scope: &PlannerPredicates,
        execution: &mut Execution<'_, '_, Clock>,
        returning: bool,
        receipt: impl FnOnce(GraphWriteProgramStats, Vec<GraphWriteStepReceipt>) -> Receipt,
    ) -> Result<(Receipt, EmbeddedTxnCompletion), Fault> {
        self.complete_authorized_program_with_output(
            txn_cx, query_cx, commit_cx, program, policy, scope, execution, returning,
            |_, _, _, stats, steps| Ok(receipt(stats, steps)),
            core::convert::identity,
        ).await
    }

    // Result construction may execute a governed query, but is always inside
    // this private workspace and BEFORE the one native completion. The fixed
    // return type cannot borrow transaction rows or return the workspace itself.
    // Pure receipts and queried results share this exact dispatch/commit body.
    #[allow(clippy::too_many_arguments)]
    async fn complete_authorized_program_with_output<
        'cx,
        'permit,
        Receipt,
        Failure,
        Clock: FnMut() -> u64,
    >(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        program: &PreparedGraphWriteProgram,
        policy: GraphWriteProgramPolicy,
        scope: &PlannerPredicates,
        execution: &mut Execution<'cx, 'permit, Clock>,
        returning: bool,
        receipt: impl FnOnce(
            &WriteTxn,
            &Database<V>,
            &mut Execution<'cx, 'permit, Clock>,
            GraphWriteProgramStats,
            Vec<GraphWriteStepReceipt>,
        ) -> Result<Receipt, Failure>,
        map_error: impl Fn(Fault) -> Failure,
    ) -> Result<(Receipt, EmbeddedTxnCompletion), Failure> {
        let fail = |error| map_error(preflight(error));
        let refusal = |error| fail(WriteTxnError::Authorization(error));
        commit_cx
            .with_restriction_async(async {
                // Validate the COMPLETE immutable shape before even opening a
                // workspace. A valid prefix cannot allocate IDs ahead of a
                // denied tail, and zero work cannot bypass required rights.
                query_cx.with_restriction(|| {
                    for statement in program.statements() {
                        query_cx
                            .checkpoint()
                            .map_err(WriteTxnError::Interrupted)
                            .map_err(&fail)?;
                        execution.poll().map_err(&fail)?;
                        let reads = match statement {
                            GraphWriteStatement::Insert(input) => input.selection().is_some(),
                            GraphWriteStatement::Mutation(_)
                            | GraphWriteStatement::Delete(_)
                            | GraphWriteStatement::VertexMerge(_)
                            | GraphWriteStatement::VertexUpsert(_)
                            | GraphWriteStatement::EdgeMerge(_)
                            | GraphWriteStatement::EdgeUpsert(_) => true,
                        };
                        if reads && !scope.rights().can_read() {
                            return Err(refusal(Error::PermissionDenied));
                        }
                        let relation = match statement {
                            GraphWriteStatement::EdgeMerge(input) => Some(input.relation()),
                            GraphWriteStatement::EdgeUpsert(input) => Some(input.merge().relation()),
                            _ => None,
                        };
                        if relation.is_some_and(|relation| {
                            !scope.allows_relation(relation)
                        }) {
                            return Err(refusal(Error::ScopeDenied));
                        }
                    }
                    Ok(())
                })?;
                execution.checkpoint().map_err(&fail)?;
                let mut workspace = Workspace(Some(
                    self.begin(txn_cx)
                        .map_err(WriteTxnError::Write)
                        .map_err(&fail)?,
                ));
                workspace.transaction().program_multi_relation = true;
                let mut receipts = Vec::new();
                let stats = query_cx.with_restriction(|| {
                    let execution = RefCell::new(&mut *execution);
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
                                        scope,
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
                                        scope,
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
                                        scope,
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
                                        scope,
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
                                        scope,
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::VertexUpsert)?;
                                    (
                                        GraphWriteStepStats::VertexUpsert(stats),
                                        GraphWriteStepReceipt::VertexUpsert { outcome },
                                    )
                                }
                                GraphWriteStatement::EdgeMerge(input) => {
                                    let (stats, outcome) = edge_merge::apply(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.edge_merge_policy(),
                                        scope,
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::EdgeMerge)?;
                                    (
                                        GraphWriteStepStats::EdgeMerge(stats),
                                        GraphWriteStepReceipt::EdgeMerge { outcome },
                                    )
                                }
                                GraphWriteStatement::EdgeUpsert(input) => {
                                    let (stats, outcome) = edge_upsert::apply(
                                        workspace.transaction(),
                                        self,
                                        query_cx,
                                        input,
                                        remaining.edge_upsert_policy(),
                                        scope,
                                        execution,
                                        returning,
                                    )
                                    .map_err(GraphWriteStepError::EdgeUpsert)?;
                                    (
                                        GraphWriteStepStats::EdgeUpsert(stats),
                                        GraphWriteStepReceipt::EdgeUpsert { outcome },
                                    )
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
                }).map_err(&map_error)?;
                let completed_statements = stats.completed_statements;
                // Both public APIs build their final value here. There is no
                // allocation, optional-receipt unwrap or auth check after commit.
                let receipt = receipt(workspace.transaction(), self, execution, stats, receipts)?;
                let completion = workspace
                    .transaction()
                    .complete_controlled(self, commit_cx, None, false, || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.checkpoint()
                    })
                    .await
                    .map_err(|source| {
                        map_error(Fault::Program(GraphMutationProgramError::Interrupted {
                            completed_statements,
                            source,
                        }))
                    })?;
                Ok((receipt, completion))
            })
            .await
    }
}

#[cfg(test)]
#[path = "write_program_tests.rs"]
mod tests;
