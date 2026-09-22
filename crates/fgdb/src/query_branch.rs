//! Host-resolved branch reads over the native immutable-view executor.
//!
//! A selector does not manufacture branch authority. The host resolves the
//! exact name to a view it has already admitted, and supplies the corresponding
//! symbol resolver. No branch miss or source error falls back to the default.

use super::{Database, EmbeddedReadView, QueryError, QueryResult};
use asupersync::fs::Vfs;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbolResolver, PreparedGraphBranchText};
use fgdb_types::QueryCx;

impl EmbeddedReadView {
    /// Execute an optionally branch-qualified native read.
    ///
    /// `resolve_branch` is called exactly once when the statement contains
    /// `AT BRANCH`, and never for an unqualified statement. It must resolve the
    /// exact, case-sensitive name to an authorized immutable generation; an
    /// unknown or unauthorized name must return an error. The supplied symbol
    /// resolver must describe that generation's catalog.
    ///
    /// The selected view owns every operand of a compound read and is retained
    /// for the entire execution. Temporal selectors choose history within that
    /// view, never within the default view or a live writer. Branch-only
    /// parameters are consumed by routing; shared and unknown arguments remain
    /// subject to native schema validation. Values are never interpolated.
    ///
    /// This method is read-only. It does not create, fork, merge, or write a
    /// branch, install a persistent branch catalog, or grant additional access.
    /// Ordinary `query` deliberately continues to refuse unresolved selectors.
    pub fn query_with_branch_resolver(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        resolve_branch: impl FnOnce(&str) -> Result<EmbeddedReadView, QueryError>,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        query_selected_view(
            || Ok(self.clone()),
            cx,
            text,
            params,
            resolver,
            resolve_branch,
            policy,
        )
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute a native read using an explicit host branch resolver.
    ///
    /// Unqualified statements acquire a healthy read session from this
    /// database. Qualified statements instead use exactly the immutable view
    /// returned by `resolve_branch`; they do not acquire a default generation
    /// or retry there after a refusal. See
    /// [`EmbeddedReadView::query_with_branch_resolver`] for the resolver's
    /// authorization/catalog obligations and statement-wide selector semantics.
    pub fn query_with_branch_resolver(
        &self,
        cx: &QueryCx,
        text: &str,
        params: &GqlParameters,
        resolver: impl GraphSymbolResolver,
        resolve_branch: impl FnOnce(&str) -> Result<EmbeddedReadView, QueryError>,
        policy: GqlQueryPolicy,
    ) -> Result<QueryResult, QueryError> {
        query_selected_view(
            || self.read_session().map_err(QueryError::Read),
            cx,
            text,
            params,
            resolver,
            resolve_branch,
            policy,
        )
    }
}

fn query_selected_view(
    default_view: impl FnOnce() -> Result<EmbeddedReadView, QueryError>,
    cx: &QueryCx,
    text: &str,
    params: &GqlParameters,
    resolver: impl GraphSymbolResolver,
    resolve_branch: impl FnOnce(&str) -> Result<EmbeddedReadView, QueryError>,
    policy: GqlQueryPolicy,
) -> Result<QueryResult, QueryError> {
    // Selector errors contain only structural classes and original offsets.
    // Native graph syntax, binding, budgets and source errors retain their
    // existing variants. Nothing here executes a parse-shaped graph statement.
    let selector_error = |error: fgdb_gql::GraphBranchTextError| QueryError::Unsupported {
        diagnostics: vec![error.to_string()],
    };
    let prepared = PreparedGraphBranchText::prepare(text).map_err(selector_error)?;
    let bound = prepared.bind_parameters(params).map_err(selector_error)?;
    let view = match bound.branch() {
        Some(name) => resolve_branch(name)?,
        None => default_view()?,
    };
    view.query(cx, bound.statement(), bound.parameters(), resolver, policy)
}
