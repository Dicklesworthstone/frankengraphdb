//! Reusable Beacon generations that cannot escape their session's authority.
//! An opaque handle is not an index grant: it has no standalone search method.

use super::{
    AuthorizedReadSession, Error, GqlBudgetDimension, GqlQueryError, GraphSymbolResolver, Meter,
    Options, QueryCx, QueryError, ReadPolicy, RefCell, Rows, Search, SharedWork, WardenError,
    WorkControl, admit_query, allows, build, charge, definition, finish, policy, settle,
    validate_eligibility,
};
use fgdb_beacon::{BeaconError, IndexSnapshot};
use fgdb_types::{CommitSeq, VId};
use std::sync::Arc;

#[path = "prepared_graph.rs"]
mod graph;
#[path = "prepared_native.rs"]
mod native;

/// An immutable resident search generation owned by exactly one authorized
/// session. Obtain it through `AuthorizedReadSession::prepare_beacon_index`;
/// its concrete type may be inferred. Only that session can search it.
/// `search_beacon_graph_index` pairs its saved search lanes with the session's
/// native topology at this same source sequence, without another index build.
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

// Every reused retrieval lane crosses the SAME exact-session, historical-cut
// and source-population gates. In particular, a graph-only request cannot use
// the source of another prepared handle or evade the original node allowance.
fn admit_prepared(
    owner: &Arc<()>,
    prepared: &AuthorizedBeaconIndex,
    view: &crate::EmbeddedReadView,
    scope: &fgdb_warden::PlannerPredicates,
    host: fgdb_gql::GqlQueryPolicy,
    execution: &super::Live<'_, '_, '_>,
    query: Search<'_>,
) -> Result<(), QueryError> {
    // run checks current credentials before even an owner diagnostic.
    if !Arc::ptr_eq(owner, &prepared.owner) {
        return Err(QueryError::Authorization(WardenError::ScopeDenied));
    }
    admit_query(query, scope, host, execution)?;
    view.snapshot
        .check_frontier(prepared.sequence)
        .map_err(QueryError::Read)?;
    host.rows
        .check(GqlBudgetDimension::SnapshotRecords, prepared.admitted)
        .map_err(|error| QueryError::Pattern(GqlQueryError::Rows(error)))?;
    let mut live = execution.borrow_mut();
    live.checkpoint()?;
    live.settle()?;
    let now = (live.clock)();
    let charged = live.permit.charge_nodes_at(now, prepared.admitted);
    charged.map_err(|error| live.refusal(error))
}

fn prepared_policy(
    prepared: &AuthorizedBeaconIndex,
    request: ReadPolicy,
    host: fgdb_gql::GqlQueryPolicy,
) -> ReadPolicy {
    let mut allowance = policy(request, host);
    allowance.max_work_units = allowance
        .max_work_units
        .min(prepared.definition.policy.max_work_units);
    allowance.max_result_rows = allowance
        .max_result_rows
        .min(prepared.definition.policy.max_result_rows);
    allowance
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
    /// The explicit native result ceiling is frozen independently of the host's
    /// final statement-row ceiling: a private CALL input may feed an aggregate.
    /// Direct searches still intersect both ceilings before returning any hits.
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
                let mut frozen =
                    definition(options, options.index.clone(), host, &mut SharedWork(&work))?;
                frozen.as_of = Some(at);
                // Building returns no hit rows. Keep the caller's native source
                // capacity, not the session's final-output limit, in this pin.
                // Direct search applies the host cap again in prepared_policy
                // and admit_query; pipelines enforce it after their last stage.
                frozen.policy.max_result_rows = options.policy.max_result_rows;
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
        self.search_beacon_index_with_eligibility(cx, prepared, query, request, None)
    }

    /// Reuse this exact session's index with per-execution endpoint eligibility.
    /// Filtering precedes text/vector top-k and hybrid candidate depths, not
    /// just final delivery. Ineligible documents remain in corpus statistics
    /// and ANN routing; no graph scan, projection or index rebuild is performed.
    ///
    /// This is NOT a new authorization mechanism. The normal live session,
    /// exact owner, source-population, query and budget checks all still run;
    /// the same permit admits final results. Reject-all does not waive these
    /// checks or the final live admission. No caller callback receives IDs.
    ///
    /// Supply at most 100,000 strictly ascending, unique IDs; an empty slice
    /// allows none. Validation charges one work unit per supplied ID and
    /// retains no selector copy. Unknown/hidden IDs are simply nonmatches.
    /// Derive graph-dependent eligibility at the prepared source sequence,
    /// not a newer writer frontier. ANN keeps its approximation boundary.
    pub fn search_beacon_index_filtered(
        &mut self,
        cx: &QueryCx,
        prepared: &super::AuthorizedBeaconIndex,
        query: Search<'_>,
        request: ReadPolicy,
        eligible: &[VId],
    ) -> Result<Rows, Error> {
        self.search_beacon_index_with_eligibility(cx, prepared, query, request, Some(eligible))
    }

    fn search_beacon_index_with_eligibility(
        &mut self,
        cx: &QueryCx,
        prepared: &super::AuthorizedBeaconIndex,
        query: Search<'_>,
        request: ReadPolicy,
        eligible: Option<&[VId]>,
    ) -> Result<Rows, Error> {
        let owner = Arc::clone(&self.owner);
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            admit_prepared(&owner, prepared, view, scope, host, execution, query)?;
            let allowance = prepared_policy(prepared, request, host);
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
                validate_eligibility(eligible, &mut SharedWork(&work))?;
                let rows = query.execute_filtered(
                    &prepared.index,
                    |id| allows(eligible, id),
                    &mut SharedWork(&work),
                )?;
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
