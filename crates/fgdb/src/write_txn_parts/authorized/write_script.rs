//! Parameterized ingestion shares the program's one permit and completion.
//! Definition binding finishes before a snapshot pin or graph ID can exist.

use super::{
    Authority, CapabilityToken, Database, Error, Execution, GraphWriteProgramPolicy,
    GraphWriteProgramReceipt, GraphWriteProgramStats, GraphWriteStepReceipt,
    PreparedGraphWriteProgram, Vfs, WriteTxnError, preflight,
};
use fgdb_gql::{
    BoundGraphWriteScriptBatch, GqlParameters, GraphWriteScriptBatchBindError,
    GraphWriteScriptExecutionError, GraphWriteTemplateStatement, PreparedGraphWriteScript,
};
use fgdb_types::{CommitCx, EmbeddedTxnCompletion, QueryCx, TxnCx};
use fgdb_warden::PlannerPredicates;

type Fault = GraphWriteScriptExecutionError<WriteTxnError, WriteTxnError, WriteTxnError>;

fn admission(error: WriteTxnError) -> Fault {
    Fault::Program(preflight(error))
}

enum Input<'a> {
    Script(&'a PreparedGraphWriteScript, &'a GqlParameters),
    Batch(&'a PreparedGraphWriteScript, &'a [GqlParameters], usize),
    Bound(&'a BoundGraphWriteScriptBatch),
}

enum Bound<'a> {
    Script(PreparedGraphWriteProgram),
    Batch(BoundGraphWriteScriptBatch),
    Borrowed(&'a BoundGraphWriteScriptBatch),
}
impl Bound<'_> {
    fn program(&self) -> &PreparedGraphWriteProgram {
        match self {
            Self::Script(program) => program,
            Self::Batch(batch) => batch.program(),
            Self::Borrowed(batch) => batch.program(),
        }
    }
    fn error(&self, source: super::Fault) -> Fault {
        match self {
            Self::Script(_) => Fault::Program(source),
            Self::Batch(batch) => batch.execution_error(source),
            Self::Borrowed(batch) => batch.execution_error(source),
        }
    }
}

impl<'a> Input<'a> {
    // Once-per-script report retains original binding/program diagnostics,
    // following the native write_scripts.rs boundary rather than boxing them.
    #[allow(clippy::result_large_err)]
    fn bind<Clock: FnMut() -> u64>(
        self,
        cx: &QueryCx,
        scope: &PlannerPredicates,
        execution: &mut Execution<'_, '_, Clock>,
    ) -> Result<Bound<'a>, Fault> {
        cx.with_restriction(|| {
            // Check immutable operation classes before touching argument values
            // or billing definition work. Empty MATCH cannot grant Write-only
            // authority a read, and a forbidden relationship tail cannot reserve
            // identities through an earlier, otherwise legal creation record.
            let script = match &self {
                Self::Script(script, _) | Self::Batch(script, _, _) => Some(*script),
                Self::Bound(_) => None,
            };
            if let Some(script) = script {
                if script.requires_read() && !scope.rights().can_read() {
                    return Err(admission(WriteTxnError::Authorization(Error::PermissionDenied)));
                }
                for statement in script.statements() {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(admission)?;
                    execution.poll().map_err(admission)?;
                    if matches!(statement,
                        GraphWriteTemplateStatement::EdgeMerge(_)
                        | GraphWriteTemplateStatement::EdgeUpsert(_)
                    ) && !scope.allows_relation(statement.relation()) {
                        return Err(admission(WriteTxnError::Authorization(Error::ScopeDenied)));
                    }
                }
            }
            match self {
                Self::Script(script, arguments) => {
                    cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(admission)?;
                    // One logical binding unit per prepared statement. Native
                    // GLA counts remain query/proposal counts; this extra work
                    // is charged to the same SIGNED ceiling as later execution.
                    execution.work(script.statements().len() as u64).map_err(admission)?;
                    let program = script.bind_parameters(arguments).map_err(Fault::Binding)?;
                    cx.checkpoint().map_err(WriteTxnError::Interrupted).map_err(admission)?;
                    execution.checkpoint().map_err(admission)?;
                    Ok(Bound::Script(program))
                }
                Self::Batch(script, arguments, limit) => {
                    let mut before_allocation = true;
                    let batch = script.bind_parameter_sets_controlled(arguments, limit, |at| {
                        cx.checkpoint().map_err(WriteTxnError::Interrupted)?;
                        if at.is_none() && before_allocation {
                            before_allocation = false;
                            // The core has already proved the expanded hard
                            // bound. Reserve the ENTIRE statement-instance bill
                            // before Vec::with_capacity, not one cheap record
                            // charge ahead of a large expanded allocation.
                            let expanded = arguments.len() as u128
                                * script.statements().len() as u128;
                            let work = u64::try_from(expanded).map_err(|_| {
                                WriteTxnError::Authorization(Error::TooLarge)
                            })?;
                            execution.work(work)
                        } else {
                            execution.checkpoint()
                        }
                    }).map_err(|error| match error {
                        GraphWriteScriptBatchBindError::Binding(error) => Fault::BatchBinding(error),
                        // No statement has executed yet. Preserve the live cause
                        // as preflight; do not invent a completed-program index
                        // or relabel expiry/cancellation as a bad argument value.
                        GraphWriteScriptBatchBindError::Interrupted { source, .. } => admission(source),
                    })?;
                    Ok(Bound::Batch(batch))
                }
                // No rebind or deep clone. The common program preflight still
                // checks every typed step under THIS execution's authority.
                Self::Bound(batch) => Ok(Bound::Borrowed(batch)),
            }
        })
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Bind and atomically execute a prepared script under one live capability.
    /// Signature/namespace/Write rights precede argument work. Typed ReadWrite
    /// and relationship-MERGE scope preflight precede binding even for no input.
    /// All arguments bind before a database read, pin, identity reservation or
    /// mutation. The same permit then enters the ordinary authorized program;
    /// binding costs cannot reset its signed work, node or result allowance.
    ///
    /// One signed work unit is admitted per bound statement, plus final binding
    /// acceptance. Native GLA counters retain their query/proposal meaning.
    /// Stats-only execution delivers no identities and accepts max_rows=0.
    /// Script preparation, name resolution and parameter-map construction are
    /// caller-owned definition work, outside this call's allowance. No allocator
    /// callback, unchecked mutation or raw transaction escapes to a token holder.
    ///
    /// Native unknown/recovery completion remains authoritative; no retry or
    /// fallible authorization follows publication. The existing resident-source,
    /// sequential-ID metadata, trusted issuer/branch/clock and non-SSI boundaries
    /// of the authorized program apply. This is not a mandatory secure facade,
    /// byte-bounded bulk loader, durable audit/revocation or prepared-plan lease.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-script diagnostic, as on native scripts
    pub async fn execute_graph_write_script_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        script: &PreparedGraphWriteScript,
        arguments: &GqlParameters,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.authorized_script_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch,
            Input::Script(script, arguments), policy, clock, false, |stats, _| stats,
        ).await
    }

    /// Execute the same script and release its ordered identity receipts only
    /// after native completion. Receipt admission/assembly uses the program's
    /// shared result allowance and happens BEFORE final commit admission.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // once-per-script diagnostic, as on native scripts
    pub async fn execute_graph_write_script_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        script: &PreparedGraphWriteScript,
        arguments: &GqlParameters,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramReceipt, EmbeddedTxnCompletion), Fault> {
        self.authorized_script_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch,
            Input::Script(script, arguments), policy, clock, true, GraphWriteProgramReceipt::new,
        ).await
    }

    /// Admit and bind ALL records, then execute them record-major as ONE atomic
    /// program. A malformed final record reserves no graph IDs and leaves no
    /// committed prefix. The expanded statement cap applies before allocation;
    /// the GQL hard ceiling remains in force. All statement-instance binding
    /// work is reserved before expanded allocation; one further signed work
    /// checkpoint precedes each record and final acceptance. No record, chunk or
    /// statement receives another signed permit or native execution allowance.
    ///
    /// Successful later records see earlier canonical staged effects, including
    /// MERGE/upsert branches and deletions. Execution refusals retain the native
    /// program error plus its argument-set/statement/span coordinates. Auth or
    /// binding-control preflight has no fabricated executed-record coordinate;
    /// final completion has no record location. No partial receipt accompanies
    /// any error. These record-boundary controls do not preempt work within one
    /// bounded script record and are not byte-memory or streaming guarantees.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // retain native record coordinates beside the cause
    pub async fn execute_graph_write_script_batch_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        script: &PreparedGraphWriteScript,
        arguments: &[GqlParameters],
        max_statements: usize,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.authorized_script_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch,
            Input::Batch(script, arguments, max_statements), policy, clock, false, |stats, _| stats,
        ).await
    }

    /// Returning form of the same atomic ingestion. Repeated identity receipts
    /// across records are charged repeatedly; NoInput yields no identity. The
    /// complete receipt is built before the one native publication boundary.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // retain native record coordinates beside the cause
    pub async fn execute_graph_write_script_batch_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        script: &PreparedGraphWriteScript,
        arguments: &[GqlParameters],
        max_statements: usize,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramReceipt, EmbeddedTxnCompletion), Fault> {
        self.authorized_script_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch,
            Input::Batch(script, arguments, max_statements), policy, clock, true,
            GraphWriteProgramReceipt::new,
        ).await
    }

    /// Reuse a previously bound batch without granting it authority or cloning
    /// its definitions. Verify the current token and execute through the same
    /// whole-program preflight. There is no binding bill, because no binding
    /// occurs; every native statement and signed execution charge still applies.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // retain native record coordinates beside the cause
    pub async fn execute_bound_graph_write_script_batch_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        batch: &BoundGraphWriteScriptBatch,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramStats, EmbeddedTxnCompletion), Fault> {
        self.authorized_script_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch,
            Input::Bound(batch), policy, clock, false, |stats, _| stats,
        ).await
    }

    /// Return a bound batch's ordered outcomes under this call's token. The
    /// batch's record_receipts method slices them by original argument set; it
    /// is shape metadata, not proof that a receipt belongs to this execution.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // retain native record coordinates beside the cause
    pub async fn execute_bound_graph_write_script_batch_returning_authorized(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        batch: &BoundGraphWriteScriptBatch,
        policy: GraphWriteProgramPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<(GraphWriteProgramReceipt, EmbeddedTxnCompletion), Fault> {
        self.authorized_script_inner(
            txn_cx, query_cx, commit_cx, authority, token, branch,
            Input::Bound(batch), policy, clock, true, GraphWriteProgramReceipt::new,
        ).await
    }

    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // one terminal script/batch report
    async fn authorized_script_inner<Receipt, Clock: FnMut() -> u64>(
        &mut self,
        txn_cx: &TxnCx,
        query_cx: &QueryCx,
        commit_cx: &CommitCx,
        authority: &Authority,
        token: &CapabilityToken,
        branch: &str,
        input: Input<'_>,
        policy: GraphWriteProgramPolicy,
        mut clock: Clock,
        returning: bool,
        receipt: impl FnOnce(GraphWriteProgramStats, Vec<GraphWriteStepReceipt>) -> Receipt,
    ) -> Result<(Receipt, EmbeddedTxnCompletion), Fault> {
        let refusal = |error| admission(WriteTxnError::Authorization(error));
        if authority.namespace() != self.keys.namespace {
            return Err(refusal(Error::WrongAuthority));
        }
        let now = clock();
        let verified = authority.verify_at(token, branch, now).map_err(refusal)?;
        let permit = verified.begin_write_at(branch, now).map_err(refusal)?;
        let mut execution = Execution { cx: commit_cx, permit, clock };
        commit_cx.with_restriction_async(async {
            let bound = input.bind(query_cx, verified.predicates(), &mut execution)?;
            self.complete_authorized_program(
                txn_cx, query_cx, commit_cx, bound.program(), policy,
                verified.predicates(), &mut execution, returning, receipt,
            ).await.map_err(|error| bound.error(error))
        }).await
    }
}

#[cfg(test)]
#[path = "write_script_tests.rs"]
mod tests;
