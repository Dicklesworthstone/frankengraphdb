/// The limited path admits its source table once, then executes the same GLA
/// body used by transaction queries. No counting pass discards a table only to
/// read it again. Source admission still precedes the evaluator's work/shape
/// limits; this is not storage-I/O or allocator-byte preemption.
fn execute_limited_snapshot_query<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    query: &fgdb_gql::PreparedGqlQuery,
    as_of: CommitSeq,
    limits: fgdb_gql::GlaExecutionLimits,
) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
    let logical = fgdb_gql::algebra::GlaPlan::lower(query.plan());
    if logical.scans_edges() {
        let records = reader.gql_edges_at(as_of)
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GlaExecutionError::Source)?;
        logical.execute_with_limits(
            [],
            records.into_iter().map(|record| (record.entry.src, record.entry.relation, record.entry.dst)),
            |vid, predicates| {
                let row = reader.gql_vertex_at(vid, as_of).map_err(GqlError::Read)?;
                Ok(row.is_some_and(|row| predicates.iter().all(|p| p.matches(&row.labels, &row.props))))
            },
            limits,
        )
    } else {
        let rows: std::collections::BTreeMap<VId, VertexRow> = reader.gql_vertices_at(as_of)
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GlaExecutionError::Source)?
            .into_iter().map(|row| (row.vid, row)).collect();
        logical.execute_with_limits(
            rows.keys().copied(),
            [],
            |vid, predicates| {
                Ok(rows.get(&vid).is_some_and(|row| predicates.iter().all(|p| p.matches(&row.labels, &row.props))))
            },
            limits,
        )
    }
}

impl<V: Vfs + Clone> Database<V> {
    /// Execute one coherent prepared query with early evaluator work/scratch
    /// limits. Success includes exact counters; refusal returns no partial rows.
    /// These limits do not replace storage admission or the final-row budget.
    pub fn execute_prepared_query_limited(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        let as_of = self.frontier().map_err(GqlError::Read)
            .map_err(fgdb_gql::GlaExecutionError::Source)?;
        self.execute_prepared_query_limited_at(query, as_of, limits)
    }

    /// The historical limited path preserves ordinary fenced/future snapshot
    /// refusals before it starts evaluator work or reports a budget error.
    pub fn execute_prepared_query_limited_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        execute_limited_snapshot_query(self, query, as_of, limits)
    }
}

impl crate::EmbeddedReadView {
    /// Execute under work/scratch limits at this immutable generation's frontier.
    pub fn execute_prepared_query_limited(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        self.execute_prepared_query_limited_at(query, self.frontier(), limits)
    }

    /// Execute at a retained sequence without observing any later generation.
    pub fn execute_prepared_query_limited_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        execute_limited_snapshot_query(self, query, as_of, limits)
    }
}

impl WriteTxn {
    /// Execute the prepared query over the pinned basis plus staged effects.
    /// Even an evaluator refusal retains the admitted table/element witnesses;
    /// continuing the transaction cannot turn a failed query into an invisible
    /// read. A foreign handle is refused before admission or witness mutation.
    pub fn execute_prepared_query_limited<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<WriteTxnError>> {
        self.ensure_database(database).map_err(fgdb_gql::GlaExecutionError::Source)?;
        let logical = fgdb_gql::algebra::GlaPlan::lower(query.plan());
        if logical.scans_edges() {
            let graph = self.overlay_graph(database).map_err(fgdb_gql::GlaExecutionError::Source)?;
            if let Some(relation) = query.plan().relation {
                self.match_expansions.borrow_mut()
                    .extend(graph.vertices.iter().copied().map(|src| (src, relation)));
            }
            let edges = graph.edges.values()
                .filter(|edge| graph.vertices.contains(&edge.0) && graph.vertices.contains(&edge.2))
                .copied();
            logical.execute_with_limits([], edges, |vid, predicates| {
                Ok(self.vertex(database, vid)?.is_some_and(|row| {
                    predicates.iter().all(|p| p.matches(&row.labels, &row.props))
                }))
            }, limits)
        } else {
            let rows: std::collections::BTreeMap<VId, VertexRow> = self.vertices(database)
                .map_err(fgdb_gql::GlaExecutionError::Source)?
                .into_iter().map(|row| (row.vid, row)).collect();
            logical.execute_with_limits(rows.keys().copied(), [], |vid, predicates| {
                Ok(rows.get(&vid).is_some_and(|row| {
                    predicates.iter().all(|p| p.matches(&row.labels, &row.props))
                }))
            }, limits)
        }
    }
}

#[cfg(test)]
mod limited_snapshot_admission_tests {
    use super::*;
    use std::cell::Cell;

    struct Reader {
        edge_reads: Cell<usize>,
        vertex_reads: Cell<usize>,
        point_reads: Cell<usize>,
    }

    impl crate::gql_exec::GqlSnapshotReader for Reader {
        fn gql_vertex_at(&self, _: VId, _: CommitSeq) -> Result<Option<VertexRow>, ReadError> {
            self.point_reads.set(self.point_reads.get() + 1);
            Ok(None)
        }

        fn gql_vertices_at(&self, _: CommitSeq) -> Result<Vec<VertexRow>, ReadError> {
            self.vertex_reads.set(self.vertex_reads.get() + 1);
            Ok(vec![])
        }

        fn gql_edges_at(&self, as_of: CommitSeq) -> Result<Vec<EdgeRecord>, ReadError> {
            self.edge_reads.set(self.edge_reads.get() + 1);
            if as_of.0 > 1 {
                return Err(ReadError::BeyondFrontier { asked: as_of, frontier: CommitSeq(1) });
            }
            Ok(vec![EdgeRecord {
                entry: AdjacencyEntry {
                    src: VId(1), relation: RelationId(1), dst: VId(2), eid: EId(10),
                    created_at: CommitSeq(1), retired_at: None,
                },
                props: vec![],
            }])
        }
    }

    #[test]
    fn limited_execution_does_not_read_the_source_table_twice() {
        let reader = Reader { edge_reads: Cell::new(0), vertex_reads: Cell::new(0), point_reads: Cell::new(0) };
        let query = fgdb_gql::PreparedGqlQuery::prepare(
            "MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", RelationId(1)),
        ).expect("prepare");
        let result = execute_limited_snapshot_query(&reader, &query, CommitSeq(1),
            fgdb_gql::GlaExecutionLimits::new(100, 100)).expect("limited read");
        assert_eq!(result.value, vec![VId(2)]);
        assert_eq!(reader.edge_reads.get(), 1);
        assert_eq!(reader.vertex_reads.get(), 0);
        assert_eq!(reader.point_reads.get(), 0);
    }

    #[test]
    fn invalid_snapshot_is_not_masked_by_a_zero_work_limit() {
        let reader = Reader { edge_reads: Cell::new(0), vertex_reads: Cell::new(0), point_reads: Cell::new(0) };
        let query = fgdb_gql::PreparedGqlQuery::prepare(
            "MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", RelationId(1)),
        ).expect("prepare");
        let error = execute_limited_snapshot_query(&reader, &query, CommitSeq(2),
            fgdb_gql::GlaExecutionLimits::new(0, 0)).expect_err("future snapshot");
        assert!(matches!(error, fgdb_gql::GlaExecutionError::Source(
            GqlError::Read(ReadError::BeyondFrontier { .. })
        )));
    }
}
