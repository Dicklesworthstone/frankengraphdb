//! Weighted path selection over the same admitted source as graph patterns.
//! Snapshot topology and costs have one generation and one shared allowance.

mod text;

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
        cx.with_restriction(|| execute_at(self, query, None, as_of, policy, || cx.checkpoint()))
    }

    /// Return at most `count` exact cost-ranked finite WALK alternatives.
    /// The pathfinding cursor is pulled only for this prefix; it does not
    /// enumerate the remaining path bag. Admission still reads one snapshot.
    pub fn execute_graph_cheapest_paths_governed(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        count: u64,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
        })?;
        self.execute_graph_cheapest_paths_governed_at(cx, query, count, as_of, policy)
    }

    /// Topology, historical weights and the ranked result use the same exact
    /// retained sequence. Count zero is not a source-validation bypass.
    pub fn execute_graph_cheapest_paths_governed_at(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        count: u64,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| {
                GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
            })?;
        cx.with_restriction(|| execute_at(self, query, Some(count), as_of, policy, || cx.checkpoint()))
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
        cx.with_restriction(|| execute_at(self, query, None, as_of, policy, || cx.checkpoint()))
    }

    /// Rank alternatives at this immutable view's pinned frontier.
    pub fn execute_graph_cheapest_paths_governed(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        count: u64,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        self.execute_graph_cheapest_paths_governed_at(cx, query, count, self.frontier(), policy)
    }

    pub fn execute_graph_cheapest_paths_governed_at(
        &self,
        cx: &QueryCx,
        query: &PreparedGraphCheapestPath,
        count: u64,
        as_of: CommitSeq,
        policy: GqlQueryPolicy,
    ) -> CheapestResult {
        self.snapshot.check_frontier(as_of).map_err(|error| {
            GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
        })?;
        cx.with_restriction(|| execute_at(self, query, Some(count), as_of, policy, || cx.checkpoint()))
    }
}

fn execute_at<R: GqlSnapshotReader + ?Sized, C>(
    reader: &R,
    query: &PreparedGraphCheapestPath,
    count: Option<u64>,
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
    let result = match count {
        Some(count) => query.execute_k_governed_with_edge_properties(
            count,
            admitted.snapshot_records,
            admitted.vertex_ids(),
            admitted.identified_edges(),
            |eid, key| Ok::<_, ReadError>(admitted.edge_property(eid, key)),
            usage.remaining(policy),
            checkpoint,
        ),
        None => query.execute_governed_with_edge_properties(
            admitted.snapshot_records,
            admitted.vertex_ids(),
            admitted.identified_edges(),
            |eid, key| Ok::<_, ReadError>(admitted.edge_property(eid, key)),
            usage.remaining(policy),
            checkpoint,
        ),
    };
    usage
        .finish(policy, result)
        .map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))
}

#[cfg(test)]
mod ranked_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GqlQueryPolicy, GraphWalkBounds};
    use fgdb_gql::algebra::GlaDirection;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

    #[test]
    fn ranked_source_and_search_checkpoint_refusals_retry_the_same_snapshot() {
        let ((), report) = run_async_under_lab(0xc0a5_1005, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = crate::DatabaseKeys::new([0x61; 32], DatabaseSecurityNamespaceId([0x62; 32]), [0x63; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let relation = RelationId(1); let weight = PropertyKeyId(7);
            let mut batch = crate::WriteBatch::new(relation);
            batch.create_vertex(VId(0), vec![], vec![]);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.add_edge(EId(1), VId(0), VId(1), vec![(weight, CanonicalScalar::Int(4))]);
            batch.add_edge(EId(2), VId(0), VId(0), vec![(weight, CanonicalScalar::Int(-1))]);
            let basis = db.write(&commit, batch).await.unwrap();
            let query = PreparedGraphCheapestPath::new(VId(0), VId(1), relation, GlaDirection::Forward,
                weight, GraphWalkBounds::new(0, 3).unwrap()).unwrap();
            let policy = GqlQueryPolicy::new(4, 3, 100_000, 100_000);
            let mut total = 0;
            let expected = execute_at(&db, &query, Some(3), basis, policy,
                || { total += 1; Ok::<_, usize>(()) }).unwrap();
            assert_eq!(expected.value.len(), 3);
            for stop in 1..=total {
                let mut calls = 0;
                let refused = execute_at(&db, &query, Some(3), basis, policy,
                    || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } });
                assert!(matches!(refused, Err(GqlQueryError::Interrupted(at)) if at == stop));
                assert_eq!(calls, stop);
                assert_eq!(db.frontier().unwrap(), basis);
                assert_eq!(execute_at(&db, &query, Some(3), basis, policy,
                    || Ok::<_, usize>(())).unwrap(), expected);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
