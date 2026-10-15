//! Compiled CALL pipelines over an exact-session prepared retrieval corpus.
//! The ordinary GLA owns expressions, ordering and sets; only its source changes.

use super::*;
use super::super::super::{AuthorizedPreparedRead, check_branch, selector_error};
use crate::query::authorized::relational::set_error;
use crate::query::{EndpointFilter, HybridCallError, HybridSearch, ProcedureError, hybrid_refusal};
use crate::{EmbeddedReadView, GqlError, PreparedNativeRead, QueryResult};
use fgdb_beacon::GraphHybridQuery;
use fgdb_delta_types::ElementId;
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryExecution, GqlQueryPolicy,
    PreparedProcedureCall,
};
use fgdb_warden::PlannerPredicates;
use std::cell::Cell;
use std::collections::BTreeSet;

type Live<'cx, 'permit, 'clock> = super::super::Live<'cx, 'permit, 'clock>;
type CallResult = Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, QueryError>>;

fn source_refusal(message: &'static str) -> GqlError {
    GqlError::Procedure(ProcedureError::Search(HybridCallError::Combination(message)))
}

// Equality is about the logical corpus and numeric projection, not build knobs.
// An exact call may use a link-bearing generation; ANN still uses its frozen
// topology. An unused lane neither constrains nor revalidates its projection.
fn matching_definition(
    frozen: &Options,
    requested: &Options,
    query: GraphHybridQuery<'_>,
) -> Result<(), BeaconError> {
    if query.graph_enabled() {
        return Err(BeaconError::InvalidQuery(
            "prepared native index execution supports text/vector calls, not graph expansion",
        ));
    }
    if frozen.vertex_label != requested.vertex_label {
        return Err(BeaconError::InvalidQuery(
            "prepared index label selection must match the CALL",
        ));
    }
    let (vector, text) = Search::Hybrid(query.source_query()).lanes();
    if text {
        if frozen.index.text.is_none() {
            return Err(BeaconError::Disabled("text"));
        }
        if frozen.projection.text != requested.projection.text {
            return Err(BeaconError::InvalidQuery(
                "prepared index text property must match the CALL",
            ));
        }
    }
    if vector {
        let stored = frozen.index.vector.as_ref().ok_or(BeaconError::Disabled("vector"))?;
        let requested_vector = requested.index.vector.as_ref().ok_or(BeaconError::Invariant(
            "bound vector CALL has no vector definition",
        ))?;
        if frozen.projection.vector != requested.projection.vector
            || frozen.projection.encoding != requested.projection.encoding
            || stored.dimensions != requested_vector.dimensions
            || stored.metric != requested_vector.metric
        {
            return Err(BeaconError::InvalidQuery(
                "prepared index vector projection and metric must match the CALL",
            ));
        }
    }
    Ok(())
}

impl<R: GraphSymbolResolver, C: FnMut() -> u64> AuthorizedReadSession<'_, R, C> {
    /// Execute a compiled CALL/YIELD/WITH/RETURN pipeline using an existing
    /// authorized text/vector index, without rescanning or rebuilding its corpus.
    /// Obtain `prepared` with this session's ordinary `prepare` method and the
    /// index with `prepare_beacon_index`. Both handles must belong to THIS session.
    /// Parameters rebind without resolving catalog symbols again.
    ///
    /// Each hybrid.search CALL must name the frozen label, active properties,
    /// vector encoding and metric. A mismatch refuses rather than changing the
    /// corpus. The frozen text scoring configuration and ANN topology remain
    /// authoritative. Result eligibility applies before candidate truncation.
    /// Every CALL spends the same statement's remaining native allowance and
    /// live signed permit; no set operand can reset them. Only final statement
    /// rows spend signed delivery, so intermediate search k is not a row charge.
    ///
    /// All property reads use the index's source_sequence, even when it predates
    /// the session frontier. Properties remain scope-masked. Only vertices
    /// retrieved by a CALL in this execution may be dereferenced: caller-supplied
    /// element identities cannot turn an index handle into a raw snapshot reader.
    ///
    /// This explicit source profile accepts the compiled Set/pipeline class and
    /// text/vector hybrid.search sources. Pattern sources, other procedures,
    /// graph expansion, temporal wrappers and aggregate classes refuse; there is
    /// no one-shot fallback. Use the existing typed graph-index API for three-lane
    /// reuse. The normal execute method and its supported sources are unchanged.
    ///
    /// Expiry, retirement and backwards time close the session, including on
    /// empty results or late errors. Other query/resource failures allow retry.
    /// This is resident prepared execution, not a durable/advancing index, spill,
    /// exhaustive hybrid answer certificate or physical timing-isolation claim.
    pub fn execute_beacon_index(
        &mut self,
        cx: &QueryCx,
        index: &AuthorizedBeaconIndex,
        prepared: &AuthorizedPreparedRead,
        params: &GqlParameters,
    ) -> Result<QueryResult, QueryError> {
        let owner = Arc::clone(&self.owner);
        self.run(cx, |view, branch, scope, _, policy, execution| {
            // run performs current credential admission before either handle
            // is inspected, including when a query would return zero rows.
            if !Arc::ptr_eq(&owner, &prepared.owner) {
                return Err(QueryError::Authorization(WardenError::WrongAuthority));
            }
            if !Arc::ptr_eq(&owner, &index.owner) {
                return Err(QueryError::Authorization(WardenError::ScopeDenied));
            }
            let selected = prepared.selector.bind_parameters(params).map_err(selector_error)?;
            check_branch(&selected, branch)?;
            let PreparedNativeRead::Set(template) = prepared.native.as_ref() else {
                return Err(QueryError::Unsupported {
                    diagnostics: vec![
                        "prepared index execution requires a CALL text/vector pipeline".to_owned(),
                    ],
                });
            };
            view.snapshot.check_frontier(index.sequence).map_err(QueryError::Read)?;
            let bound = template.bind_parameters(selected.parameters()).map_err(QueryError::SetText)?;
            // This is a statement-local authorization provenance set, not an
            // eligibility predicate or a second source. Admit storage in call_at
            // before insertion, and discard it with the enclosing execution.
            let retrieved = RefCell::new(BTreeSet::new());
            let result = bound.execute_governed_with_elements(
                policy,
                |_, _| Err(GqlQueryError::Source(source_refusal(
                    "prepared index execution does not scan pattern sources",
                ))),
                |call, arguments, remaining| call_at(
                    &owner, index, view, scope, execution, call, arguments, remaining, &retrieved,
                ),
                |element, key| {
                    if !scope.allows_property(key) {
                        return Ok(None);
                    }
                    if !matches!(element, ElementId::Vertex(id) if retrieved.borrow().contains(&id)) {
                        return Err(source_refusal(
                            "prepared index property reads require a retrieved vertex",
                        ));
                    }
                    crate::gql_exec::read_element_property(
                        element, key,
                        |id| view.snapshot.vertex_at(id, index.sequence),
                        |id| view.snapshot.edge_at(id, index.sequence),
                    )
                },
                || execution.borrow_mut().checkpoint(),
            ).map_err(set_error)?;
            let count = result.value.len();
            Ok((crate::query::values(template.columns().to_vec(), result.value), count))
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn call_at(
    owner: &Arc<()>,
    index: &AuthorizedBeaconIndex,
    view: &EmbeddedReadView,
    scope: &PlannerPredicates,
    execution: &Live<'_, '_, '_>,
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    remaining: GqlQueryPolicy,
    retrieved: &RefCell<BTreeSet<VId>>,
) -> CallResult {
    if !crate::query::is_hybrid(call) {
        return Err(hybrid_refusal(HybridCallError::Combination(
            "prepared index execution accepts only hybrid.search sources",
        )));
    }
    let (search, eligible) = HybridSearch::bind_snapshot_filtered(
        call, arguments, index.sequence, remaining,
    ).map_err(hybrid_refusal)?;
    let query = search.query();
    matching_definition(&index.definition, search.options(), query)
        .map_err(|error| hybrid_refusal(HybridCallError::Index(error)))?;
    admit_prepared(
        owner, index, view, scope, remaining, execution, Search::Hybrid(query.retrieval),
    ).map_err(GqlQueryError::Interrupted)?;
    let allowance = prepared_policy(index, search.options().policy, remaining);
    let spent = Cell::new(0_u64);
    let work = RefCell::new(Meter::new(allowance.max_work_units, |units| {
        charge(execution, units)?;
        spent.set(spent.get().saturating_add(units as u64));
        Ok(())
    }));
    let result = (|| {
        work.borrow_mut().charge(1)?;
        query.candidate_depths()?;
        if query.retrieval.k > allowance.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows", limit: allowance.max_result_rows,
            });
        }
        Search::Hybrid(query.source_query()).validate(&index.definition.index, &mut SharedWork(&work))?;
        let filter = EndpointFilter::prepare(
            eligible, query.retrieval.k, remaining.evaluator.max_scratch_entries,
            &mut SharedWork(&work),
        )?;
        let hits = filter.finish(index.index.hybrid_search_graph_filtered(
            query, &[], |id| filter.admits(id, &work), &mut SharedWork(&work),
        ))?;
        // Reserve a row, all YIELD cells and a provenance-set entry per hit
        // before allocating output. Repeated identities conservatively pay
        // again, so warming the private set cannot widen the query allowance.
        let scratch = (call.outputs().len() as u128 + 2)
            .checked_mul(hits.len() as u128)
            .and_then(|count| count.checked_add(u128::from(filter.scratch_entries())))
            .filter(|&count| count <= u128::from(remaining.evaluator.max_scratch_entries))
            .ok_or(BeaconError::ResourceLimit {
                resource: "prepared CALL scratch entries",
                limit: usize::try_from(remaining.evaluator.max_scratch_entries).unwrap_or(usize::MAX),
            })?;
        for hit in &hits {
            let entries = retrieved.borrow().len();
            work.borrow_mut().charge(1 + entries.checked_ilog2().unwrap_or(0) as usize)?;
            for _ in call.outputs() {
                work.borrow_mut().charge(1)?;
            }
            retrieved.borrow_mut().insert(hit.id);
        }
        work.borrow_mut().charge(1)?;
        Ok((search.rows(hits), scratch as u64))
    })();
    let (value, scratch_entries) = settle(work, result)
        .map_err(GqlQueryError::Interrupted)?
        .map_err(|error| hybrid_refusal(HybridCallError::Index(error)))?;
    Ok(GqlQueryExecution {
        rows: GqlExecutionStats {
            snapshot_records: index.admitted,
            result_rows: value.len() as u64,
        },
        evaluator: GlaExecutionStats { work_units: spent.get(), scratch_entries },
        value,
    })
}

#[cfg(test)]
#[path = "prepared_native_tests.rs"]
mod tests;
