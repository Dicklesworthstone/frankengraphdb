//! Registered analytics through the session's fixed generation and live permit.
//! Binding and execution reuse Prism; no raw graph, issuer or certificate escapes.

use super::{AuthorizedReadSession, Live};
use crate::gql_exec::AdmissionUsage;
use crate::query::authorized::{analytics, query_error};
use crate::{EmbeddedReadView, QueryError, ReadError};
use fgdb_gql::{GqlQueryPolicy, GraphSymbolResolver};
use fgdb_prism::{
    FnxCallSpec, FnxOutputColumn, FnxParameters, FnxReadError, FnxReadOptions, FnxValue,
    SnapshotBinding,
};
use fgdb_types::{CommitSeq, QueryCx};
use fgdb_warden::{Error as WardenError, PlannerPredicates};
use std::sync::Arc;

type Error = FnxReadError<ReadError, QueryError>;
type Rows = Vec<Vec<FnxValue>>;

/// One bound registered call and projection recipe owned by exactly one session.
/// Obtain it with `AuthorizedReadSession::prepare_fnx`; its type may be inferred.
///
/// Parameters are bound values, frozen with the selected historical sequence,
/// direction, edge reduction and resource ceilings. Executing does not reparse
/// text or consult the host's native GQL catalog. To use different arguments,
/// prepare another call or use `call_fnx` directly.
///
/// This contains no graph, execution permit or generation pin. Clones share
/// only the immutable call. Closing or expiring the session disables them all.
/// It is not a transferable capability or a reusable authorization decision.
#[derive(Clone)]
pub struct AuthorizedPreparedFnxCall {
    owner: Arc<()>,
    call: Arc<FnxCallSpec>,
    options: FnxReadOptions,
    sequence: CommitSeq,
}

impl core::fmt::Debug for AuthorizedPreparedFnxCall {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedPreparedFnxCall([REDACTED])")
    }
}

impl AuthorizedPreparedFnxCall {
    /// Request-defined YIELD columns in their original order, including aliases.
    pub fn outputs(&self) -> &[FnxOutputColumn] {
        self.call.outputs()
    }

    /// The exact cut of the session's pin selected during preparation.
    pub fn source_sequence(&self) -> CommitSeq {
        self.sequence
    }
}

// Native failures stay values until run performs its final live checks. Live
// authorization failures remain outer QueryErrors, preserving session terminality.
fn settle<T>(result: Result<T, Error>) -> Result<Result<T, Error>, QueryError> {
    match result {
        Err(Error::Cancelled(error)) => Err(error),
        Err(Error::Read(error)) => Err(QueryError::Read(error)),
        result => Ok(result),
    }
}

fn finish<T>(result: Result<Result<T, Error>, QueryError>) -> Result<T, Error> {
    match result {
        Ok(result) => result,
        Err(QueryError::Read(error)) => Err(Error::Read(error)),
        Err(error) => Err(Error::Cancelled(error)),
    }
}

fn policy(mut request: FnxReadOptions, host: GqlQueryPolicy) -> FnxReadOptions {
    let cap = |requested: usize, allowed: u64| {
        usize::try_from(allowed).map_or(requested, |allowed| requested.min(allowed))
    };
    request.source_limits.max_work_units = request
        .source_limits
        .max_work_units
        .min(host.evaluator.max_work_units);
    request.source_limits.max_scratch_entries = request
        .source_limits
        .max_scratch_entries
        .min(host.evaluator.max_scratch_entries);
    request.execution_limits.max_estimated_work = cap(
        request.execution_limits.max_estimated_work,
        host.evaluator.max_work_units,
    );
    if let Some(rows) = host.rows.max_result_rows() {
        request.execution_limits.max_result_rows =
            cap(request.execution_limits.max_result_rows, rows);
    }
    request
}

fn execute(
    view: &EmbeddedReadView,
    scope: &PlannerPredicates,
    host: GqlQueryPolicy,
    execution: &Live<'_, '_, '_>,
    call: &FnxCallSpec,
    options: FnxReadOptions,
) -> Result<Rows, Error> {
    let at = options.as_of.unwrap_or(view.frontier());
    view.snapshot.check_frontier(at).map_err(Error::Read)?;
    analytics::preflight(call, options.projection.directedness)?;
    let mut allowance = host;
    allowance.evaluator.max_scratch_entries = allowance
        .evaluator
        .max_scratch_entries
        .min(options.source_limits.max_scratch_entries);
    let mut usage = AdmissionUsage::default();
    analytics::execute(
        &view.snapshot,
        SnapshotBinding {
            root: view.partition_root().0,
            as_of: at,
        },
        call,
        policy(options, host),
        scope,
        execution,
        |event| usage.observe(allowance, event).map_err(query_error),
    )
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Run a registered CALL/YIELD on this session's capability-visible graph.
    /// Authentication precedes parsing, projection and every returned error.
    /// None selects the session frontier; a historical cut cannot exceed it.
    /// The original issuer, clock, branch and generation cannot be substituted.
    ///
    /// The ordinary authorized Prism source masks vertices, relations, labels
    /// and weights before graph construction. Direction, parallel-edge and
    /// missing-weight laws stay explicit. No certificate or raw view is returned.
    ///
    /// ONE signed allowance spans binding, source, builder, kernel and delivery.
    /// The fixed host work meter spans source/build/kernel checkpoints, and its
    /// record/scratch meter counts admitted vertices before user selection plus
    /// selected edges. Hidden histories do not spend those meters. Native
    /// projection bytes/adjacency and kernel estimates retain their own caps;
    /// this remains resident execution, not an allocator-byte quota or spill.
    /// Request result/work ceilings cannot widen the host's policy. Successful
    /// rows pass through run's final live delivery gate exactly once.
    pub fn call_fnx(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
    ) -> Result<Rows, Error> {
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            let result = FnxCallSpec::bind(text, parameters)
                .map_err(Error::Bind)
                .and_then(|call| execute(view, scope, host, execution, &call, options));
            let result = settle(result)?;
            let rows = result.as_ref().map_or(0, Vec::len);
            Ok((result, rows))
        }))
    }

    /// Bind once without building a graph or running an algorithm. The handle
    /// freezes its supplied parameter values and complete projection recipe.
    /// Native bind/graph-law errors are returned only after final live checks.
    /// A handle alone does not keep the session's generation or authority alive.
    pub fn prepare_fnx(
        &mut self,
        cx: &QueryCx,
        text: &str,
        parameters: &FnxParameters,
        options: FnxReadOptions,
    ) -> Result<AuthorizedPreparedFnxCall, Error> {
        let owner = Arc::clone(&self.owner);
        finish(self.run(cx, |view, _, _, _, host, _| {
            let result = (|| {
                let call = FnxCallSpec::bind(text, parameters).map_err(Error::Bind)?;
                analytics::preflight(&call, options.projection.directedness)?;
                let sequence = options.as_of.unwrap_or(view.frontier());
                view.snapshot
                    .check_frontier(sequence)
                    .map_err(Error::Read)?;
                let mut options = policy(options, host);
                options.as_of = Some(sequence);
                Ok(AuthorizedPreparedFnxCall {
                    owner,
                    call: Arc::new(call),
                    options,
                    sequence,
                })
            })();
            Ok((settle(result)?, 0))
        }))
    }

    /// Execute only this exact session's bound call with a fresh live permit.
    /// Foreign handles refuse before observing their call, options or graph,
    /// even when database, credentials and selected sequence happen to match.
    /// No cached graph or retained allowance bypasses current authorization.
    pub fn execute_fnx(
        &mut self,
        cx: &QueryCx,
        prepared: &AuthorizedPreparedFnxCall,
    ) -> Result<Rows, Error> {
        let owner = Arc::clone(&self.owner);
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            if !Arc::ptr_eq(&owner, &prepared.owner) {
                return Err(QueryError::Authorization(WardenError::WrongAuthority));
            }
            let result = settle(execute(
                view,
                scope,
                host,
                execution,
                &prepared.call,
                prepared.options,
            ))?;
            let rows = result.as_ref().map_or(0, Vec::len);
            Ok((result, rows))
        }))
    }
}

#[cfg(test)]
#[path = "analytics_tests.rs"]
mod tests;
