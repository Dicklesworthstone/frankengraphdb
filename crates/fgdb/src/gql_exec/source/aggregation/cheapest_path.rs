//! Weighted path selection over the same admitted source as graph patterns.
//! Snapshot topology and costs have one generation and one shared allowance.

use crate::gql_exec::{AdmissionUsage, AdmittedGqlSnapshot, GqlSnapshotReader};
use crate::{Database, EmbeddedReadView, GqlError, ReadError};
use asupersync::fs::Vfs;
use fgdb_gql::{
    GqlQueryError, GqlQueryExecution, GqlQueryPolicy, GraphCheapestPathError,
    GraphCostPath, PreparedGraphCheapestPath,
};
use fgdb_types::{CommitSeq, QueryCx};

type CheapestResult = Result<
    GqlQueryExecution<GraphCostPath>,
    GqlQueryError<GraphCheapestPathError<GqlError>, Box<asupersync::error::Error>>,
>;

impl<V: Vfs + Clone> Database<V> {
    /// Select one minimum-cost finite WALK between the prepared anchors.
    /// Edge weights and topology are read from the same live snapshot. The
    /// result is empty when no admitted route exists, never a fabricated path.
    pub fn execute_graph_cheapest_path_governed(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
        })?;
        self.execute_graph_cheapest_path_governed_at(cx, query, as_of, policy)
    }

    /// Health and exact-sequence fences precede resource/cancellation refusal.
    /// Admission, relaxation, tie-breaking and final path copies share policy.
    pub fn execute_graph_cheapest_path_governed_at(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| {
                GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
            })?;
        cx.with_restriction(|| execute_at(self, query, as_of, policy, || cx.checkpoint()))
    }
}

impl EmbeddedReadView {
    /// Use this view's pinned generation, not the database's current frontier.
    pub fn execute_graph_cheapest_path_governed(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        self.execute_graph_cheapest_path_governed_at(cx, query, self.frontier(), policy)
    }

    pub fn execute_graph_cheapest_path_governed_at(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        self.snapshot.check_frontier(as_of).map_err(|error| {
            GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
        })?;
        cx.with_restriction(|| execute_at(self, query, as_of, policy, || cx.checkpoint()))
    }
}

fn execute_at<R: GqlSnapshotReader + ?Sized, C>(
    reader: &R,
    query: &PreparedGraphCheapestPath,
    as_of: CommitSeq,
    policy: GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GqlQueryExecution<GraphCostPath>, GqlQueryError<GraphCheapestPathError<GqlError>, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    // This compiler-owned WALK input admits vertices (including isolates)
    // and identified edges with borrowed properties. Never execute its bag.
    let mut admitted =
        AdmittedGqlSnapshot::admit_logical(query.input_pattern().plan().clone(), reader, as_of)
            .map_err(|error| {
                GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
            })?;
    let mut usage = AdmissionUsage::default();
    admitted
        .materialize(&mut |event| {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            usage.observe::<GraphCheapestPathError<ReadError>, C>(policy, event)
        })
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
    let result = query.execute_governed_with_edge_properties(
        admitted.snapshot_records,
        admitted.vertex_ids(),
        admitted.identified_edges(),
        |eid, key| Ok::<_, ReadError>(admitted.edge_property(eid, key)),
        usage.remaining(policy),
        checkpoint,
    );
    usage
        .finish(policy, result)
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))
}
