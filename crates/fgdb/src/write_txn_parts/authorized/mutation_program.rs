//! Ordered mutations keep one private native workspace and one live permit.

use super::{
    Authority, CapabilityToken, CommitCx, Database, EmbeddedTxnCompletion, Error,
    Execution, GraphMutationPolicy, QueryCx, RefCell, TxnCx, Vfs, Workspace,
    WriteTxnError, apply,
};
use fgdb_gql::{GraphMutationProgramError, GraphMutationProgramStats, PreparedGraphMutationProgram};

type Fault = GraphMutationProgramError<WriteTxnError, WriteTxnError>;

impl<V: Vfs + Clone> Database<V> {
    /// Execute an ordered, prepared mutation program atomically under ReadWrite
    /// authority. Later MATCH/WHERE/RHS expressions observe earlier statements'
    /// canonical native effects, then apply the SAME masking and GLA evaluation
    /// as authorized reads. Assignments inside each statement stay simultaneous.
    /// No intermediate statement publishes, returns rows, or receives a fresh
    /// permit. Every original field and before/after image still passes the
    /// ordinary authorized staging rules, including restricted detach refusal.
    ///
    /// The existing program kernel sums native selected rows, source records,
    /// evaluator work, scratch and proposed effects, even when a later statement
    /// cancels an earlier effect. Its remaining policy is passed to each step.
    /// One signed allowance covers the entire program and native completion.
    /// Stats are control receipts, so max_rows=0 is supported. All-empty programs
    /// read-close without a new sequence. Any denied tail, expired permit, quota
    /// refusal, cancellation or unwind discards the entire private workspace.
    ///
    /// Native overlay reads retain their read/scan dependencies. Materializing
    /// those resident rows is not bounded-memory or preemptible within a read;
    /// hidden records are not billed to observable signed/native query limits.
    /// The exclusive borrow prevents another commit between program statements.
    /// The host still owns issuer/branch/clock routing. This is not a public
    /// transaction handle, mandatory secure facade, full SSI, or durable audit.
    /// Native unknown/recovery outcomes apply after the sole commit admission;
    /// no fallible authorization check follows publication.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_graph_mutation_program_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        program: &PreparedGraphMutationProgram,
        policy: GraphMutationPolicy,
        mut clock: impl FnMut() -> u64,
    ) -> Result<(GraphMutationProgramStats, EmbeddedTxnCompletion), Fault> {
        let refusal = |error| Fault::Preflight(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        if !verified.predicates().rights().can_read() {
            return Err(refusal(Error::PermissionDenied));
        }
        commit_cx.with_restriction_async(async {
            let mut execution = Execution { cx: commit_cx, permit, clock };
            execution.checkpoint().map_err(Fault::Preflight)?;
            let mut workspace = Workspace(Some(
                self.begin(txn_cx).map_err(WriteTxnError::Write).map_err(Fault::Preflight)?,
            ));
            let stats = query_cx.with_restriction(|| {
                let execution = RefCell::new(&mut execution);
                program.execute_governed(
                    policy,
                    |statement, remaining| {
                        apply(
                            workspace.transaction(), self, query_cx, statement, remaining,
                            verified.predicates(), &mut **execution.borrow_mut(), false,
                        ).map(|(stats, _, _)| stats)
                    },
                    || {
                        query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        execution.borrow_mut().checkpoint()
                    },
                )
            })?;
            let completion = workspace.transaction()
                .complete_controlled(self, commit_cx, None, false, || {
                    query_cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                    execution.checkpoint()
                })
                .await
                .map_err(|source| Fault::Interrupted {
                    completed_statements: stats.completed_statements, source,
                })?;
            Ok((stats, completion))
        }).await
    }
}

#[cfg(test)]
#[path = "mutation_program_tests.rs"]
mod tests;
