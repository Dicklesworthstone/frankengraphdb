//! Native text and reusable templates over one Warden-scoped execution.
//! Dispatch sees only the admitted generation and permit, not a Database, so
//! no branch can fall back to privileged readers or reset an input's authority.

use super::*;
use crate::{PreparedNativeRead, QueryResult, QueryValue};
use crate::query::{aggregates, values};
use fgdb_gql::{GqlParameters, GraphSymbolResolver, PreparedGraphBranchText};

// Only metadata and complete result rows leave the dispatcher. The outer
// authorized owner still performs the sole signed delivery admission.
fn rows_of(result: QueryResult, columns: &mut Vec<String>) -> Result<Vec<Vec<QueryValue>>, QueryError> {
    match result {
        QueryResult::Rows { columns: names, rows } => {
            *columns = names;
            Ok(rows)
        }
        QueryResult::Write { .. } => Err(QueryError::Unsupported {
            diagnostics: vec!["authorized native execution covers reads only".to_owned()],
        }),
    }
}

fn native_at<Clock: FnMut() -> u64>(
    prepared: &PreparedNativeRead,
    params: &GqlParameters,
    snapshot: &Snapshot,
    at: CommitSeq,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Result<QueryResult, QueryError> {
    execution.borrow_mut().checkpoint()?;
    let result = (|| match prepared {
        PreparedNativeRead::Pattern(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::PatternText)?;
            let rows = pattern_at(snapshot, at, &query, scope, execution, policy)
                .map_err(query_error)?.value;
            Ok(values(query.columns().to_vec(), rows))
        }
        PreparedNativeRead::Aggregate(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::PatternText)?;
            let rows = graph::graph_at(snapshot, at, &query, scope, execution, policy)?;
            Ok(aggregates(prepared.columns().to_vec(), prepared.output_slots(), rows))
        }
        PreparedNativeRead::PipelineAggregate(prepared) => {
            // Exactly the native classifier's source-free distinction. A real
            // row pipeline is never reduced to its first MATCH leaf.
            let rows = if prepared.is_source_free() {
                let query = prepared.bind_relation_parameters(params).map_err(QueryError::PipelineText)?;
                aggregate_at(snapshot, at, &query, scope, execution, policy)?
            } else {
                let query = prepared.bind_parameters(params).map_err(QueryError::PipelineText)?;
                graph::graph_at(snapshot, at, &query, scope, execution, policy)?
            };
            Ok(aggregates(prepared.columns().to_vec(), prepared.output_slots(), rows))
        }
        PreparedNativeRead::Set(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::SetText)?;
            let rows = set_at(snapshot, at, &query, scope, execution, policy)?;
            Ok(values(prepared.columns().to_vec(), rows))
        }
        PreparedNativeRead::TemporalPattern(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::TemporalText)?;
            snapshot.check_frontier(query.as_of()).map_err(QueryError::Read)?;
            let rows = pattern_at(snapshot, query.as_of(), query.pattern(), scope, execution, policy)
                .map_err(query_error)?.value;
            Ok(values(query.pattern().columns().to_vec(), rows))
        }
        PreparedNativeRead::TemporalAggregate(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::TemporalText)?;
            snapshot.check_frontier(query.as_of()).map_err(QueryError::Read)?;
            let rows = graph::graph_at(snapshot, query.as_of(), query.aggregate(), scope, execution, policy)?;
            Ok(aggregates(prepared.columns().to_vec(), prepared.output_slots(), rows))
        }
        PreparedNativeRead::TemporalSet(prepared) => {
            let query = prepared.bind_parameters(params).map_err(QueryError::TemporalSetText)?;
            snapshot.check_frontier(query.as_of()).map_err(QueryError::Read)?;
            let rows = set_at(snapshot, query.as_of(), query.query(), scope, execution, policy)?;
            Ok(values(prepared.columns().to_vec(), rows))
        }
    })();
    // Binding and result-shaping are inside the live boundary too. A late
    // credential invalidation wins before returning even a binding error.
    execution.borrow_mut().checkpoint()?;
    result
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute native GQL text under the host-selected Warden capability.
    /// Authentication, rights and database namespace checks precede parsing,
    /// parameter binding and all symbol-resolver callbacks. The existing native
    /// classifier selects exactly one facade; a failed bind/source/execution
    /// never retries an unscoped query or a different engine.
    ///
    /// Pattern, aggregate, row-pipeline, set and temporal native reads use the
    /// same scoped graph source. Temporal selectors bind one exact cut for all
    /// leaves and cannot bypass current credential validity. One live permit
    /// covers preparation boundaries, all input work and final result delivery.
    /// The result is the ordinary lossless QueryResult, with the same aliases,
    /// aggregate output slots, NULLs, exact numeric values and row ordering.
    ///
    /// AT BRANCH may name only the exact branch already selected by the trusted
    /// host. It does not resolve another view or grant access. Literal/parameter
    /// selectors use the existing branch parser; branch-only arguments are
    /// consumed there, while all other arguments retain native validation.
    ///
    /// Keep the issuer, raw Database, catalog mapping and monotone clock in the
    /// trusted host. Ordinary Database methods are still privileged. Writes,
    /// EXPLAIN/certified replay and unsupported classes refuse; this adds no
    /// server session, streaming delivery, spill or physical noninterference.
    #[allow(clippy::too_many_arguments)]
    pub fn query_authorized(
        &self, cx: &QueryCx, authority: &Authority, token: &CapabilityToken,
        branch: &str, text: &str, params: &GqlParameters,
        resolver: impl GraphSymbolResolver, policy: GqlQueryPolicy,
        clock: impl FnMut() -> u64,
    ) -> Result<QueryResult, QueryError> {
        let mut columns = Vec::new();
        let rows = authorized(self, cx, authority, token, branch, None, clock, |snapshot, at, scope, execution| {
            let selector_error = |error: fgdb_gql::GraphBranchTextError| QueryError::Unsupported {
                diagnostics: vec![error.to_string()],
            };
            let selector = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
            let selected = selector.bind_parameters(params).map_err(selector_error)?;
            if selected.branch().is_some_and(|name| name != branch) {
                return Err(QueryError::Authorization(fgdb_warden::Error::ScopeDenied));
            }
            execution.borrow_mut().checkpoint()?;
            let prepared = PreparedNativeRead::prepare(selected.statement(), selected.parameters(), resolver);
            // No RefCell borrow spans caller-controlled resolver code. Check
            // invalidation even when preparation itself reports an error.
            execution.borrow_mut().checkpoint()?;
            let prepared = prepared?;
            rows_of(native_at(&prepared, selected.parameters(), snapshot, at, scope, execution, policy)?, &mut columns)
        })?;
        Ok(QueryResult::Rows { columns, rows })
    }
}

impl PreparedNativeRead {
    /// Rebind this complete native template against a freshly authenticated
    /// capability, without changing its class, symbol mapping or definition.
    /// Reuse never freezes permissions: each invocation checks the current
    /// host authority and token before binding parameters or reading a source.
    /// A narrower token masks all leaves before evaluation, including an old
    /// temporal cut; retired/expired credentials cannot reuse prepared access.
    ///
    /// The template itself grants nothing. Its catalog must match the trusted
    /// host's graph/branch mapping. All ordinary authorized-source limitations
    /// apply. Use Database::query_authorized for textual AT BRANCH selectors;
    /// a PreparedNativeRead already contains the branch-free native template.
    #[allow(clippy::too_many_arguments)]
    pub fn execute_authorized<V: Vfs + Clone>(
        &self, database: &Database<V>, cx: &QueryCx,
        authority: &Authority, token: &CapabilityToken, branch: &str,
        params: &GqlParameters, policy: GqlQueryPolicy, clock: impl FnMut() -> u64,
    ) -> Result<QueryResult, QueryError> {
        let mut columns = Vec::new();
        let rows = authorized(database, cx, authority, token, branch, None, clock, |snapshot, at, scope, execution| {
            rows_of(native_at(self, params, snapshot, at, scope, execution, policy)?, &mut columns)
        })?;
        Ok(QueryResult::Rows { columns, rows })
    }
}
