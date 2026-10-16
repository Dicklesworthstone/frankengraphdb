//! Compiled CALL pipelines over an exact-session prepared retrieval corpus.
//! The ordinary GLA owns expressions, ordering and sets; only its source changes.

use super::super::super::{AuthorizedPreparedRead, check_branch, selector_error};
use super::*;
use crate::query::authorized::relational::{aggregate_gql_error, set_error};
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
    GqlError::Procedure(ProcedureError::Search(HybridCallError::Combination(
        message,
    )))
}

// Equality is about the logical corpus and numeric projection, not build knobs.
// An exact call may use a link-bearing generation; ANN still uses its frozen
// topology. An unused lane neither constrains nor revalidates its projection.
fn matching_definition(
    frozen: &Options,
    requested: &Options,
    query: GraphHybridQuery<'_>,
) -> Result<(), BeaconError> {
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
        let stored = frozen
            .index
            .vector
            .as_ref()
            .ok_or(BeaconError::Disabled("vector"))?;
        let requested_vector = requested
            .index
            .vector
            .as_ref()
            .ok_or(BeaconError::Invariant(
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
    /// This explicit source profile accepts compiled row and terminal aggregate
    /// pipelines over hybrid.search, including graph-only and mixed expansion.
    /// Graph membership and incidence use the typed prepared graph reader at
    /// the SAME saved sequence. Propertyless vertices remain traversable;
    /// rejecting an endpoint does not remove it from paths. No graph directory
    /// or text/vector index is rebuilt. Each CALL reports the saved population
    /// plus admitted incidence visits to the enclosing source-record allowance.
    /// New visits, incidences and bounded ranking workspace are scratch-admitted
    /// before retention; these are logical counts, not a total byte-memory cap.
    ///
    /// Terminal COUNT/SUM/AVG, DISTINCT, extrema, collections, grouping, HAVING
    /// and output clauses use the existing native aggregate owner. Counts and
    /// exact averages keep their aggregate domains, not narrowed scalar cells.
    /// Private CALL hits and aggregate inputs spend source/work/scratch limits,
    /// not the final signed row allowance. The frozen native source-result cap
    /// still applies to every CALL; final group rows use normal session delivery.
    /// A failed property read or aggregate input is not hidden by LIMIT 0.
    ///
    /// Pattern sources, other procedures and temporal wrappers still refuse;
    /// there is no one-shot fallback. A nonnull graph_access
    /// override refuses: this explicit prepared source profile owns its native
    /// incidence path, rather than silently substituting a requested physical
    /// source. The normal execute method and its source choices are unchanged.
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
            let selected = prepared
                .selector
                .bind_parameters(params)
                .map_err(selector_error)?;
            check_branch(&selected, branch)?;
            view.snapshot
                .check_frontier(index.sequence)
                .map_err(QueryError::Read)?;
            // This is a statement-local authorization provenance set, not an
            // eligibility predicate or a second source. Admit storage in call_at
            // before insertion, and discard it with the enclosing execution.
            let retrieved = RefCell::new(BTreeSet::new());
            let mut procedures = |call: &PreparedProcedureCall,
                                  arguments: &[GraphValue],
                                  remaining: GqlQueryPolicy| {
                call_at(
                    cx, &owner, index, view, scope, execution, call, arguments, remaining,
                    &retrieved,
                )
            };
            let mut elements = |element, key| {
                if !scope.allows_property(key) {
                    return Ok(None);
                }
                if !matches!(element, ElementId::Vertex(id) if retrieved.borrow().contains(&id)) {
                    return Err(source_refusal(
                        "prepared index property reads require a retrieved vertex",
                    ));
                }
                crate::gql_exec::read_element_property(
                    element,
                    key,
                    |id| view.snapshot.vertex_at(id, index.sequence),
                    |id| view.snapshot.edge_at(id, index.sequence),
                )
            };
            match prepared.native.as_ref() {
                PreparedNativeRead::Set(template) => {
                    let bound = template
                        .bind_parameters(selected.parameters())
                        .map_err(QueryError::SetText)?;
                    let result = bound
                        .execute_governed_with_elements(
                            policy,
                            |_, _| {
                                Err(GqlQueryError::Source(source_refusal(
                                    "prepared index execution does not scan pattern sources",
                                )))
                            },
                            &mut procedures,
                            &mut elements,
                            || execution.borrow_mut().checkpoint(),
                        )
                        .map_err(set_error)?;
                    let count = result.value.len();
                    Ok((
                        crate::query::values(template.columns().to_vec(), result.value),
                        count,
                    ))
                }
                PreparedNativeRead::PipelineAggregate(template) => {
                    // Bind the complete relational input, never a fabricated
                    // first graph leaf. Both physical aggregate profiles keep
                    // the same source/element owner and the original allowance.
                    let bound = template
                        .bind_relation_parameters(selected.parameters())
                        .map_err(QueryError::PipelineText)?;
                    let result = bound
                        .execute_governed_with_elements(
                            policy,
                            |_, _| {
                                Err(GqlQueryError::Source(source_refusal(
                                    "prepared index execution does not scan pattern sources",
                                )))
                            },
                            &mut procedures,
                            &mut elements,
                            || execution.borrow_mut().checkpoint(),
                        )
                        .map_err(aggregate_gql_error)?;
                    let count = result.value.len();
                    Ok((
                        crate::query::aggregates(
                            template.columns().to_vec(),
                            template.output_slots(),
                            result.value,
                        ),
                        count,
                    ))
                }
                _ => Err(QueryError::Unsupported {
                    diagnostics: vec![
                        "prepared index execution requires a row or aggregate CALL pipeline"
                            .to_owned(),
                    ],
                }),
            }
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn call_at(
    cx: &QueryCx,
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
    if call
        .argument_names()
        .iter()
        .zip(arguments)
        .any(|(name, value)| {
            name == "graph_access"
                && !matches!(value, GraphValue::Scalar(fgdb_types::CanonicalScalar::Null))
        })
    {
        return Err(hybrid_refusal(HybridCallError::Combination(
            "prepared index execution owns its graph source; omit graph_access",
        )));
    }
    let (search, eligible) =
        HybridSearch::bind_snapshot_filtered(call, arguments, index.sequence, remaining)
            .map_err(hybrid_refusal)?;
    let query = search.query();
    matching_definition(&index.definition, search.options(), query)
        .map_err(|error| hybrid_refusal(HybridCallError::Index(error)))?;
    admit_prepared(
        owner,
        index,
        view,
        scope,
        remaining,
        execution,
        // The procedure's k is private input to the compiled row/group stages,
        // not a delivered page. Admit owner, history and the saved population
        // on this same permit, with zero delivered rows. Actual source k still
        // meets candidate_depths and the frozen native ceiling below; the final
        // statement result alone is charged by the enclosing session gate.
        Search::Hybrid(query.source_query()),
    )
    .map_err(GqlQueryError::Interrupted)?;
    let allowance = prepared_policy(index, search.options().policy, remaining);
    let spent = Cell::new(0_u64);
    let incidences = Cell::new(0_u64);
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
                resource: "result rows",
                limit: allowance.max_result_rows,
            });
        }
        if !query.is_graph_only() {
            Search::Hybrid(query.source_query())
                .validate(&index.definition.index, &mut SharedWork(&work))?;
        }
        let filter = EndpointFilter::prepare(
            eligible,
            query.retrieval.k,
            remaining.evaluator.max_scratch_entries,
            &mut SharedWork(&work),
        )?;
        let graph_scratch = Cell::new(filter.scratch_entries());
        let admit_scratch = |entries: usize| {
            let next = u64::try_from(entries)
                .ok()
                .and_then(|n| graph_scratch.get().checked_add(n))
                .filter(|&n| n <= remaining.evaluator.max_scratch_entries)
                .ok_or(BeaconError::ResourceLimit {
                    resource: "prepared CALL graph scratch entries",
                    limit: usize::try_from(remaining.evaluator.max_scratch_entries)
                        .unwrap_or(usize::MAX),
                })?;
            graph_scratch.set(next);
            Ok(())
        };
        // The bool eligibility ABI retains its first refusal across BOTH
        // expansion and fusion. No intermediate result or fresh permit escapes.
        let hits = filter.finish((|| {
            let graph = if query.graph_enabled() {
                let (hits, visits) = super::graph::expand_at(
                    cx,
                    index,
                    view,
                    scope,
                    remaining,
                    execution,
                    search.expansion(),
                    query.graph_candidates,
                    allowance,
                    &work,
                    |id, _| Ok(filter.admits(id, &work)),
                    &admit_scratch,
                )?;
                incidences.set(visits);
                hits
            } else {
                Vec::new()
            };
            if query.is_graph_only() {
                query.fuse_graph_only(
                    &graph,
                    |id| filter.admits(id, &work),
                    &mut SharedWork(&work),
                )
            } else {
                index.index.hybrid_search_graph_filtered(
                    query,
                    &graph,
                    |id| filter.admits(id, &work),
                    &mut SharedWork(&work),
                )
            }
        })())?;
        // Reserve a row, all YIELD cells and a provenance-set entry per hit
        // before allocating output. Repeated identities conservatively pay
        // again, so warming the private set cannot widen the query allowance.
        let scratch = (call.outputs().len() as u128 + 2)
            .checked_mul(hits.len() as u128)
            .and_then(|count| count.checked_add(u128::from(graph_scratch.get())))
            .filter(|&count| count <= u128::from(remaining.evaluator.max_scratch_entries))
            .ok_or(BeaconError::ResourceLimit {
                resource: "prepared CALL scratch entries",
                limit: usize::try_from(remaining.evaluator.max_scratch_entries)
                    .unwrap_or(usize::MAX),
            })?;
        for hit in &hits {
            let entries = retrieved.borrow().len();
            work.borrow_mut()
                .charge(1 + entries.checked_ilog2().unwrap_or(0) as usize)?;
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
            // expand_at checks this same sum at each admitted incidence, before
            // visiting it. Return it too, so UNION/CALL siblings cannot reset it.
            snapshot_records: index
                .admitted
                .checked_add(incidences.get())
                .ok_or_else(|| {
                    hybrid_refusal(HybridCallError::Index(BeaconError::Invariant(
                        "prepared source-record sum overflowed after admission",
                    )))
                })?,
            result_rows: value.len() as u64,
        },
        evaluator: GlaExecutionStats {
            work_units: spent.get(),
            scratch_entries,
        },
        value,
    })
}

#[cfg(test)]
#[path = "prepared_native_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "prepared_native_graph_tests.rs"]
mod graph_tests;

#[cfg(test)]
#[path = "prepared_native_aggregate_tests.rs"]
mod aggregate_tests;
