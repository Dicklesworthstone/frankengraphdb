//! Three-way retrieval on a narrowed session, using the native graph evaluator.

use super::{
    AuthorizedReadSession, Error, GqlBudgetDimension, GqlQueryError, GraphSymbolResolver,
    Meter, Options, QueryCx, QueryError, RefCell, Scan, Search, SharedWork, WorkControl,
    admit_query, charge, definition, finish, policy, settle,
};
use crate::query::beacon::graph as native;
use fgdb_beacon::expansion::ExpansionSpec;
use fgdb_beacon::{BeaconError, GraphHybridHit, GraphHybridQuery};
use fgdb_delta_types::RelationId;
use std::cell::Cell;

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Fuse text, vector and graph candidates on this session's pinned,
    /// capability-visible historical cut. The request cannot supply a database,
    /// issuer, branch mapping, clock or a precomputed privileged graph.
    ///
    /// The existing native evaluator selects winners before authorization,
    /// masks properties before index construction and admits edges only after
    /// relation and BOTH selected endpoint checks. User label selection also
    /// constrains transit nodes. Hidden/missing seeds are absent, not bridges.
    ///
    /// One live permit and native work allowance cover construction, traversal,
    /// fusion and final delivery. Host snapshot-record and source-scratch caps
    /// cover admitted vertices PLUS admitted logical edges, before parallel-edge
    /// collapse. Hidden history consumes neither allowance. Signed nodes count
    /// admitted vertices before user label selection; output rows are charged
    /// exactly once. Graph structure/seed/visited limits are additionally capped
    /// by the host/request scratch ceiling, not independently widenable.
    ///
    /// This builds per execution; it is not prepared-index graph reuse, disk
    /// spill, a durable lease or timing-isolation proof. Candidate-limited fusion
    /// and ANN retain their existing answer scope. Expiry/retirement close the
    /// session even on empty output or late refusal; ordinary errors allow retry.
    pub fn beacon_search_graph(
        &mut self,
        cx: &QueryCx,
        options: &Options,
        query: GraphHybridQuery<'_>,
        expansion: ExpansionSpec<'_, RelationId>,
    ) -> Result<Vec<GraphHybridHit>, Error> {
        finish(self.run(cx, |view, _, scope, _, host, execution| {
            // source_query() deliberately has k=0; admission must check the
            // requested FINAL graph-fused k, not that internal candidate query.
            admit_query(Search::Hybrid(query.retrieval), scope, host, execution)?;
            let at = options.as_of.unwrap_or(view.frontier());
            view.snapshot.check_frontier(at).map_err(QueryError::Read)?;
            let work = RefCell::new(Meter::new(
                policy(options.policy, host).max_work_units,
                |units| charge(execution, units),
            ));
            let result = (|| {
                work.borrow_mut().charge(1)?;
                let config = options.config_for_graph(query)?;
                let options = definition(options, config, host, &mut SharedWork(&work))?;
                let mut expansion = expansion;
                if query.graph_enabled() {
                    let cap = options.policy.max_source_scratch;
                    expansion.limits.max_vertices = expansion.limits.max_vertices.min(cap);
                    expansion.limits.max_input_edges = expansion.limits.max_input_edges.min(cap);
                    expansion.limits.max_visited_vertices = expansion.limits.max_visited_vertices.min(cap);
                    expansion.limits.max_seed_ids = expansion.limits.max_seed_ids.min(cap);
                    expansion.limits.max_source_scratch = expansion.limits.max_source_scratch.min(cap);
                }
                let records = Cell::new(0_u64);
                let record = || -> Result<(), BeaconError> {
                    work.borrow_mut().charge(1)?;
                    let used = records.get();
                    let next = used.checked_add(1).ok_or(BeaconError::ResourceLimit {
                        resource: "admitted graph records",
                        limit: usize::MAX,
                    })?;
                    host.rows.check(GqlBudgetDimension::SnapshotRecords, next)
                        .map_err(|error| work.borrow_mut()
                            .refuse(QueryError::Pattern(GqlQueryError::Rows(error))))?;
                    if u128::from(used) >= options.policy.max_source_scratch as u128 {
                        return Err(BeaconError::ResourceLimit {
                            resource: "admitted graph scratch",
                            limit: options.policy.max_source_scratch,
                        });
                    }
                    records.set(next);
                    Ok(())
                };
                let mut poll = || execution.borrow_mut().poll()
                    .map_err(|error| work.borrow_mut().refuse(error));
                native::evaluate_with_edge_admission(
                    &view.snapshot,
                    at,
                    &options,
                    query,
                    expansion,
                    &work,
                    Scan::Unmetered(&mut poll),
                    |row| {
                        if !scope.allows_vertex(&row.labels) {
                            return Ok(false);
                        }
                        execution.borrow_mut().node()
                            .map_err(|error| work.borrow_mut().refuse(error))?;
                        record()?;
                        Ok(true)
                    },
                    |label| scope.allows_label(label),
                    |key| scope.allows_property(key),
                    |relation| scope.allows_relation(relation),
                    record,
                )
            })();
            let result = settle(work, result)?;
            let rows = result.as_ref().map_or(0, Vec::len);
            Ok((result, rows))
        }))
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
