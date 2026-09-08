/// Limited and unlimited reads share one snapshot admission owner and one
/// lowered evaluator. Source errors precede evaluator refusals; no counting
/// pass discards a table merely to admit it again.
fn execute_limited_snapshot_query<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    query: &fgdb_gql::PreparedGqlQuery,
    as_of: CommitSeq,
    limits: fgdb_gql::GlaExecutionLimits,
) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
    crate::gql_exec::AdmittedGqlSnapshot::admit(query.plan(), reader, as_of)
        .map_err(GqlError::Read)
        .map_err(fgdb_gql::GlaExecutionError::Source)?
        .execute_limited(limits)
        .map_err(|error| match error {
            fgdb_gql::GlaExecutionError::Source(error) => {
                fgdb_gql::GlaExecutionError::Source(GqlError::Read(error))
            }
            fgdb_gql::GlaExecutionError::Limit(error) => fgdb_gql::GlaExecutionError::Limit(error),
        })
}

impl<V: Vfs + Clone> Database<V> {
    /// Bound evaluator work and scratch. Source admission is outside these
    /// limits; success returns exact counters, refusal never partial rows.
    pub fn execute_prepared_query_limited(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        let as_of = self.frontier().map_err(GqlError::Read)
            .map_err(fgdb_gql::GlaExecutionError::Source)?;
        self.execute_prepared_query_limited_at(query, as_of, limits)
    }

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
    pub fn execute_prepared_query_limited(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        self.execute_prepared_query_limited_at(query, self.frontier(), limits)
    }

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
    /// Limits do not erase observed dependencies. A foreign handle refuses
    /// before any admission or witness mutation on the owner's transaction.
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
            let rows: std::collections::BTreeMap<VId, VertexRow> = self
                .vertices_for_scan(database, query.plan().src_label)
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

    #[test]
    fn ordinary_and_budgeted_execution_also_admit_the_source_once() {
        for budgeted in [false, true] {
            let reader = Reader { edge_reads: Cell::new(0), vertex_reads: Cell::new(0), point_reads: Cell::new(0) };
            let query = fgdb_gql::PreparedGqlQuery::prepare(
                "MATCH (a)-[:R]->(b) RETURN b",
                &RelationBind::new().with_relation("R", RelationId(1)),
            ).expect("prepare");
            let rows = if budgeted {
                let run = execute_budgeted_at(&reader, &query, CommitSeq(1),
                    fgdb_gql::GqlExecutionBudget::new(1, 1)).expect("exact admission budget");
                assert_eq!(run.stats.snapshot_records, 1);
                run.value
            } else {
                crate::gql_exec::execute_at(query.plan(), &reader, CommitSeq(1)).expect("ordinary read")
            };
            assert_eq!(rows, vec![VId(2)]);
            assert_eq!(reader.edge_reads.get(), 1);
            assert_eq!(reader.vertex_reads.get(), 0);
        }
    }
}
