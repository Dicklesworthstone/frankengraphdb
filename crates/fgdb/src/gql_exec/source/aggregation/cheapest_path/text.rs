//! Bound weighted text delegates to the same public governed path entrypoints.
//! There is no text-only graph source, meter, transaction overlay or fallback.

use super::*;
use crate::{WriteTxn, WriteTxnError};
use fgdb_gql::BoundGraphCheapestPathQuery;

impl<V: Vfs + Clone> Database<V> {
    /// Execute a completely bound weighted text request at the live frontier.
    /// Preparation and numeric/identity binding have already finished. Runtime
    /// health, cost admission, cancellation and budgets are the native ones.
    pub fn execute_graph_cheapest_path_text_governed(
        &self, cx: &QueryCx, request: &BoundGraphCheapestPathQuery, policy: GqlQueryPolicy,
    ) -> CheapestResult {
        match request.ranked_count() {
            Some(count) => self.execute_graph_cheapest_paths_governed(cx, request.query(), count, policy),
            None => self.execute_graph_cheapest_path_governed(cx, request.query(), policy),
        }
    }

    /// The exact-sequence fence runs before resource or cancellation refusal.
    /// Topology and edge costs come from the same retained historical sequence.
    pub fn execute_graph_cheapest_path_text_governed_at(
        &self, cx: &QueryCx, request: &BoundGraphCheapestPathQuery, as_of: CommitSeq, policy: GqlQueryPolicy,
    ) -> CheapestResult {
        match request.ranked_count() {
            Some(count) => self.execute_graph_cheapest_paths_governed_at(cx, request.query(), count, as_of, policy),
            None => self.execute_graph_cheapest_path_governed_at(cx, request.query(), as_of, policy),
        }
    }
}

impl EmbeddedReadView {
    /// Execute against this immutable view, never the database's newer frontier.
    pub fn execute_graph_cheapest_path_text_governed(
        &self, cx: &QueryCx, request: &BoundGraphCheapestPathQuery, policy: GqlQueryPolicy,
    ) -> CheapestResult {
        match request.ranked_count() {
            Some(count) => self.execute_graph_cheapest_paths_governed(cx, request.query(), count, policy),
            None => self.execute_graph_cheapest_path_governed(cx, request.query(), policy),
        }
    }

    pub fn execute_graph_cheapest_path_text_governed_at(
        &self, cx: &QueryCx, request: &BoundGraphCheapestPathQuery, as_of: CommitSeq, policy: GqlQueryPolicy,
    ) -> CheapestResult {
        match request.ranked_count() {
            Some(count) => self.execute_graph_cheapest_paths_governed_at(cx, request.query(), count, as_of, policy),
            None => self.execute_graph_cheapest_path_governed_at(cx, request.query(), as_of, policy),
        }
    }
}

impl WriteTxn {
    /// The native canonical overlay supplies both staged topology and costs.
    /// Owner/health checks precede admission. Short, zero, empty and refused
    /// requests retain ordinary point/scan dependencies for later validation.
    pub fn execute_graph_cheapest_path_text_governed<V: Vfs + Clone>(
        &self, database: &Database<V>, cx: &QueryCx, request: &BoundGraphCheapestPathQuery, policy: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphCostPath>,
        GqlQueryError<GraphCheapestPathError<WriteTxnError>, Box<asupersync::error::Error>>> {
        match request.ranked_count() {
            Some(count) => self.execute_graph_cheapest_paths_governed(database, cx, request.query(), count, policy),
            None => self.execute_graph_cheapest_path_governed(database, cx, request.query(), policy),
        }
    }
}
