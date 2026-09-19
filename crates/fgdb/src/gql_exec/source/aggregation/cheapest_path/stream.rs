//! Weighted result streaming over the ordinary admitted database source.
//! The native stream owns one topology/cost image and one cumulative meter.

use super::*;
use fgdb_gql::{
    BoundGraphCheapestPathQuery, GlaExecutionStats, GraphCheapestPathStream,
    GraphCheapestPathStreamIterator,
};

type OpenStream<'cx> = Result<
    GraphCheapestPathStreamIterator<'cx, Box<asupersync::error::Error>>,
    GqlQueryError<GraphCheapestPathError<GqlError>, Box<asupersync::error::Error>>,
>;

// A read-only handoff from the existing source meter. Its authority, charging
// and counters remain owned by gql_exec::AdmissionUsage; no allowance resets.
impl AdmissionUsage {
    pub(crate) fn path_stream_usage(self) -> GlaExecutionStats {
        GlaExecutionStats { work_units: self.work_units, scratch_entries: self.scratch_entries }
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Open a pull-driven ranked query at the current frontier. The returned
    /// iterator borrows QueryCx, not Database or the prepared query. It owns
    /// its admitted graph image, so later writes/compaction/drop cannot change
    /// its answers. Source and cost admission finish before the first delivery.
    pub fn stream_graph_cheapest_paths_governed<'cx>(
        &self, cx: &'cx QueryCx, query: &PreparedGraphCheapestPath,
        count: u64, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        let as_of = self.frontier().map_err(|error| {
            GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error)))
        })?;
        self.stream_graph_cheapest_paths_governed_at(cx, query, count, as_of, policy)
    }

    /// Health and exact-frontier fences precede cancellation/resource refusal.
    /// This pins an admitted historical image, not a server lease or restart
    /// token. Every complete pull remains inside this same QueryCx restriction.
    pub fn stream_graph_cheapest_paths_governed_at<'cx>(
        &self, cx: &'cx QueryCx, query: &PreparedGraphCheapestPath,
        count: u64, as_of: CommitSeq, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.ensure_readable().and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error))))?;
        let stream = cx.with_restriction(|| open_at(self, query, count, as_of, policy, || cx.checkpoint()))?;
        Ok(stream.into_scoped_iterator(as_of, move |stream| {
            cx.with_restriction(|| stream.next_with_checkpoint(|| cx.checkpoint()))
        }))
    }

    /// Stream a completely bound weighted text request without reparsing or
    /// catalog access. ANY CHEAPEST selects K=1 on the ranked physical stream;
    /// its rows agree with ANY, but its accounting is the ranked specialization,
    /// not the separate eager single-answer dynamic program.
    pub fn stream_graph_cheapest_path_text_governed<'cx>(
        &self, cx: &'cx QueryCx, request: &BoundGraphCheapestPathQuery, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.stream_graph_cheapest_paths_governed(cx, request.query(), request.ranked_count().unwrap_or(1), policy)
    }

    pub fn stream_graph_cheapest_path_text_governed_at<'cx>(
        &self, cx: &'cx QueryCx, request: &BoundGraphCheapestPathQuery,
        as_of: CommitSeq, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.stream_graph_cheapest_paths_governed_at(cx, request.query(), request.ranked_count().unwrap_or(1), as_of, policy)
    }
}

impl EmbeddedReadView {
    /// Stream this view's pinned generation, even after its database advances.
    pub fn stream_graph_cheapest_paths_governed<'cx>(
        &self, cx: &'cx QueryCx, query: &PreparedGraphCheapestPath,
        count: u64, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.stream_graph_cheapest_paths_governed_at(cx, query, count, self.frontier(), policy)
    }

    pub fn stream_graph_cheapest_paths_governed_at<'cx>(
        &self, cx: &'cx QueryCx, query: &PreparedGraphCheapestPath,
        count: u64, as_of: CommitSeq, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.snapshot.check_frontier(as_of)
            .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error))))?;
        let stream = cx.with_restriction(|| open_at(self, query, count, as_of, policy, || cx.checkpoint()))?;
        Ok(stream.into_scoped_iterator(as_of, move |stream| {
            cx.with_restriction(|| stream.next_with_checkpoint(|| cx.checkpoint()))
        }))
    }

    pub fn stream_graph_cheapest_path_text_governed<'cx>(
        &self, cx: &'cx QueryCx, request: &BoundGraphCheapestPathQuery, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.stream_graph_cheapest_paths_governed(cx, request.query(), request.ranked_count().unwrap_or(1), policy)
    }

    pub fn stream_graph_cheapest_path_text_governed_at<'cx>(
        &self, cx: &'cx QueryCx, request: &BoundGraphCheapestPathQuery,
        as_of: CommitSeq, policy: GqlQueryPolicy,
    ) -> OpenStream<'cx> {
        self.stream_graph_cheapest_paths_governed_at(cx, request.query(), request.ranked_count().unwrap_or(1), as_of, policy)
    }
}

fn open_at<R: GqlSnapshotReader + ?Sized, C>(
    reader: &R, query: &PreparedGraphCheapestPath, count: u64,
    as_of: CommitSeq, policy: GqlQueryPolicy, mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<GraphCheapestPathStream, GqlQueryError<GraphCheapestPathError<GqlError>, C>> {
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    let mut admitted = AdmittedGqlSnapshot::admit_logical(query.input_pattern().plan().clone(), reader, as_of)
        .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(GqlError::Read(error))))?;
    let mut usage = AdmissionUsage::default();
    admitted.materialize(&mut |event| {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        usage.observe::<GraphCheapestPathError<ReadError>, C>(policy, event)
    }).map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))?;
    query.stream_governed_with_admission(
        count, admitted.snapshot_records, admitted.vertex_ids(), admitted.identified_edges(),
        |eid, key| Ok::<_, ReadError>(admitted.edge_property(eid, key)),
        usage.path_stream_usage(), policy, checkpoint,
    ).map_err(|error| error.map_source(|error| error.map_source(GqlError::Read)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{GraphCheapestPathMode, GraphCheapestPathStreamState, GraphWalkBounds};
    use fgdb_gql::algebra::GlaDirection;
    use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

    #[test]
    fn every_database_admission_and_pull_checkpoint_refuses_once_and_retries_identically() {
        let ((), report) = run_async_under_lab(0xc0a5_3016, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let keys = crate::DatabaseKeys::new([0xb1; 32], DatabaseSecurityNamespaceId([0xb2; 32]), [0xb3; 32]);
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let relation = RelationId(1); let weight = PropertyKeyId(7);
            let mut batch = crate::WriteBatch::new(relation);
            batch.create_vertex(VId(0), vec![], vec![]);
            batch.create_vertex(VId(1), vec![], vec![]);
            batch.add_edge(EId(1), VId(0), VId(1), vec![(weight, CanonicalScalar::Int(4))]);
            batch.add_edge(EId(2), VId(0), VId(0), vec![(weight, CanonicalScalar::Int(-1))]);
            let basis = db.write(&commit, batch).await.unwrap();
            let policy = GqlQueryPolicy::new(4, 3, 100_000, 100_000);
            for mode in [GraphCheapestPathMode::Walk, GraphCheapestPathMode::Trail,
                GraphCheapestPathMode::Acyclic, GraphCheapestPathMode::Simple] {
                let query = PreparedGraphCheapestPath::new(VId(0), VId(1), relation, GlaDirection::Forward,
                    weight, GraphWalkBounds::new(0, 3).unwrap()).unwrap().with_mode(mode);
                let mut total = 0;
                let mut good = open_at(&db, &query, 3, basis, policy, || { total += 1; Ok::<_, usize>(()) }).unwrap();
                let mut expected = Vec::new();
                while let Some(row) = good.next_with_checkpoint(|| { total += 1; Ok::<_, usize>(()) }).unwrap() { expected.push(row); }
                let expected_stats = good.evaluator_stats();
                for stop in 1..=total {
                    let mut calls = 0;
                    let mut checkpoint = || { calls += 1; if calls == stop { Err(stop) } else { Ok(()) } };
                    match open_at(&db, &query, 3, basis, policy, &mut checkpoint) {
                        Err(GqlQueryError::Interrupted(at)) => assert_eq!(at, stop),
                        Err(error) => panic!("unexpected admission error: {error:?}"),
                        Ok(mut stream) => {
                            let mut delivered = Vec::new();
                            loop {
                                match stream.next_with_checkpoint(&mut checkpoint) {
                                    Ok(Some(row)) => delivered.push(row),
                                    Err(GqlQueryError::Interrupted(at)) => { assert_eq!(at, stop); break; }
                                    other => panic!("interruption became a different outcome: {other:?}"),
                                }
                            }
                            assert_eq!(delivered, expected[..delivered.len()]);
                            assert_eq!(stream.row_stats().result_rows, delivered.len() as u64);
                            assert_eq!(stream.state(), GraphCheapestPathStreamState::Failed);
                            assert!(stream.next_with_checkpoint(|| -> Result<(), usize> { panic!("terminal retry"); }).unwrap().is_none());
                        }
                    }
                    assert_eq!(calls, stop);
                    assert_eq!(db.frontier().unwrap(), basis);
                    let mut retry = open_at(&db, &query, 3, basis, policy, || Ok::<_, usize>(())).unwrap();
                    let mut rows = Vec::new();
                    while let Some(row) = retry.next_with_checkpoint(|| Ok::<_, usize>(())).unwrap() { rows.push(row); }
                    assert_eq!(rows, expected);
                    assert_eq!(retry.evaluator_stats(), expected_stats);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
