//! Reusable Beacon generations that cannot escape their session's authority.
//! An opaque handle is not an index grant: it has no standalone search method.

use super::{
    AuthorizedReadSession, Error, GqlBudgetDimension, GqlQueryError, GraphSymbolResolver,
    Meter, Options, QueryCx, QueryError, ReadPolicy, RefCell, Rows, Search, SharedWork,
    WardenError, WorkControl, admit_query, build, charge, definition, finish, policy, settle,
};
use fgdb_beacon::{BeaconError, IndexSnapshot};
use fgdb_types::CommitSeq;
use std::sync::Arc;

/// An immutable resident search generation owned by exactly one authorized
/// session. Obtain it through `AuthorizedReadSession::prepare_beacon_index`;
/// its concrete type may be inferred. Only that session can search it.
///
/// Cloning shares the same private index, definition and session identity.
/// No corpus statistics, document values, raw snapshot or authority escape.
/// Closing/expiring the session disables every handle, including its clones;
/// retained index memory is released when its last handle is dropped.
///
/// This is not a durable IndexDefinition/DerivedIdentity, an advancing index,
/// a transferable capability or a full AnswerContract certificate.
#[derive(Clone)]
pub struct AuthorizedBeaconIndex {
    owner: Arc<()>,
    definition: Arc<Options>,
    index: IndexSnapshot,
    sequence: CommitSeq,
    admitted: u64,
}

impl core::fmt::Debug for AuthorizedBeaconIndex {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("AuthorizedBeaconIndex([REDACTED])")
    }
}

impl AuthorizedBeaconIndex {
    /// The explicitly selected source cut, not a current writer watermark.
    #[must_use]
    pub fn source_sequence(&self) -> CommitSeq {
        self.sequence
    }
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Build both explicitly enabled retrieval lanes once, from this session's
    /// capability-visible corpus. `as_of` selects a retained cut of its pin;
    /// None selects the session frontier, never a newer writer generation.
    ///
    /// Scope and property masking precede scalar validation, BM25 statistics
    /// and ANN topology. All enabled projections are validated even when the
    /// first search will use only one lane. The owned definition cannot change
    /// through later edits to `options`. Build refusal returns no handle.
    ///
    /// The normal session gate governs construction and final return. This
    /// admits no result rows; it does not retain an execution permit. Source,
    /// staging and work budgets are checked during preparation. This is a
    /// bounded resident generation, not disk spill or commit-fed maintenance.
    pub fn prepare_beacon_index(
        &mut self,
        cx: &QueryCx,
        options: &Options,
    ) -> Result<super::AuthorizedBeaconIndex, Error> {
        let owner = Arc::clone(&self.owner);
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            let at = options.as_of.unwrap_or(view.frontier());
            view.snapshot.check_frontier(at).map_err(QueryError::Read)?;
            let work = RefCell::new(Meter::new(
                policy(options.policy, host).max_work_units,
                |units| charge(execution, units),
            ));
            let result = (|| {
                work.borrow_mut().charge(1)?;
                let mut frozen = definition(
                    options,
                    options.index.clone(),
                    host,
                    &mut SharedWork(&work),
                )?;
                frozen.as_of = Some(at);
                let (index, admitted) = build(view, at, scope, host, execution, &frozen, &work)?;
                work.borrow_mut().charge(1)?;
                Ok(AuthorizedBeaconIndex {
                    owner,
                    definition: Arc::new(frozen),
                    index: index.snapshot(),
                    sequence: at,
                    admitted,
                })
            })();
            Ok((settle(work, result)?, 0))
        }))
    }

    /// Search an exact-session generation without scanning graph history,
    /// projecting documents, rebuilding BM25 statistics or rebuilding HNSW.
    /// A handle from ANY other session refuses, even with equal credentials,
    /// database namespace, historical cut or a narrower scope. Output filtering
    /// cannot make another authority's corpus statistics or ANN topology safe.
    ///
    /// Every call takes a fresh live permit and performs final delivery checks.
    /// Signed nodes and the host's snapshot-record ceiling apply to the stored
    /// admitted population (before user label selection); actual output rows
    /// are charged once by the session. Work pays for this search, not a rebuild.
    /// Request work/result caps may narrow but never widen the frozen definition
    /// or host policy. Source-scratch/staging caps are unused during reuse,
    /// because no source scan or staging occurs; they governed preparation.
    ///
    /// Empty/zero-k answers still reauthorize. Expiry, issuer retirement and
    /// clock rollback close the session. Ordinary query/resource/cancellation
    /// failures leave the handle and session available for a later retry.
    pub fn search_beacon_index(
        &mut self,
        cx: &QueryCx,
        prepared: &super::AuthorizedBeaconIndex,
        query: Search<'_>,
        request: ReadPolicy,
    ) -> Result<Rows, Error> {
        let owner = Arc::clone(&self.owner);
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            // Current credentials are checked by run BEFORE even an owner
            // refusal. Keep the old owner's corpus/definition unobserved.
            if !Arc::ptr_eq(&owner, &prepared.owner) {
                return Err(QueryError::Authorization(WardenError::ScopeDenied));
            }
            admit_query(query, scope, host, execution)?;
            view.snapshot
                .check_frontier(prepared.sequence)
                .map_err(QueryError::Read)?;
            host.rows
                .check(GqlBudgetDimension::SnapshotRecords, prepared.admitted)
                .map_err(|error| QueryError::Pattern(GqlQueryError::Rows(error)))?;
            {
                let mut live = execution.borrow_mut();
                live.checkpoint()?;
                let now = (live.clock)();
                let charged = live.permit.charge_nodes_at(now, prepared.admitted);
                charged.map_err(|error| live.refusal(error))?;
            }
            let mut allowance = policy(request, host);
            allowance.max_work_units = allowance
                .max_work_units
                .min(prepared.definition.policy.max_work_units);
            allowance.max_result_rows = allowance
                .max_result_rows
                .min(prepared.definition.policy.max_result_rows);
            let work = RefCell::new(Meter::new(allowance.max_work_units, |units| {
                charge(execution, units)
            }));
            let result = (|| {
                work.borrow_mut().charge(1)?;
                if query.k() > allowance.max_result_rows {
                    return Err(BeaconError::ResourceLimit {
                        resource: "result rows",
                        limit: allowance.max_result_rows,
                    });
                }
                let config = &prepared.definition.index;
                let (vector, text) = query.lanes();
                if vector && config.vector.is_none() {
                    return Err(BeaconError::Disabled("vector"));
                }
                if text && config.text.is_none() {
                    return Err(BeaconError::Disabled("text"));
                }
                query.validate(config, &mut SharedWork(&work))?;
                let rows = query.execute(&prepared.index, &mut SharedWork(&work))?;
                work.borrow_mut().charge(1)?;
                Ok(rows)
            })();
            let result = settle(work, result)?;
            let rows = result.as_ref().map_or(0, Rows::len);
            Ok((result, rows))
        }))
    }
}

#[cfg(test)]
#[path = "prepared_tests.rs"]
mod tests;
