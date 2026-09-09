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

/// Product callers check their inexpensive owner/frontier fences first. The
/// checkpoint then runs before table admission, in the evaluator and at exit.
fn execute_governed_snapshot_query<R: crate::gql_exec::GqlSnapshotReader + ?Sized, C>(
    reader: &R,
    query: &fgdb_gql::PreparedGqlQuery,
    as_of: CommitSeq,
    policy: fgdb_gql::GqlQueryPolicy,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<GqlError, C>> {
    checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
    crate::gql_exec::AdmittedGqlSnapshot::admit(query.plan(), reader, as_of)
        .map_err(GqlError::Read)
        .map_err(fgdb_gql::GqlQueryError::Source)?
        .execute_governed(policy, checkpoint)
        .map_err(|error| error.map_source(GqlError::Read))
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

    /// Apply scan/result/work/scratch limits and the real QueryCx checkpoint
    /// to one execution. No certificate or partial rows are issued on failure.
    pub fn execute_prepared_query_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        let as_of = self.frontier().map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        self.execute_prepared_query_governed_at(cx, query, as_of, policy)
    }

    /// Owner/frontier validation precedes cancellation and all source work.
    /// Checkpoints surround source admission, and run inside evaluator work
    /// and output copying. They do not preempt a storage read or sort midway,
    /// and the policy does not bound allocator bytes or snapshot materialization.
    pub fn execute_prepared_query_governed_at(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        self.ensure_readable().and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read).map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| execute_governed_snapshot_query(self, query, as_of, policy, || cx.checkpoint()))
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

    pub fn execute_prepared_query_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        self.execute_prepared_query_governed_at(cx, query, self.frontier(), policy)
    }

    pub fn execute_prepared_query_governed_at(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>> {
        self.snapshot.check_frontier(as_of).map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| execute_governed_snapshot_query(self, query, as_of, policy, || cx.checkpoint()))
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

    /// A governed overlay read preserves its admitted conflict witnesses even
    /// if the caller's context interrupts it or a resource dimension refuses.
    /// A failure before admission records no fictitious table observation.
    pub fn execute_prepared_query_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<WriteTxnError, Box<asupersync::error::Error>>> {
        cx.with_restriction(|| self.execute_governed_with_checkpoint(database, query, policy, || cx.checkpoint()))
    }

    fn execute_governed_with_checkpoint<V: Vfs + Clone, C>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<WriteTxnError, C>> {
        self.ensure_database(database).map_err(fgdb_gql::GqlQueryError::Source)?;
        database.ensure_readable().and_then(|()| database.snapshot.check_frontier(self.basis))
            .map_err(WriteTxnError::Read).map_err(fgdb_gql::GqlQueryError::Source)?;
        checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
        let logical = fgdb_gql::algebra::GlaPlan::lower(query.plan());
        if logical.scans_edges() {
            let graph = self.overlay_graph(database).map_err(fgdb_gql::GqlQueryError::Source)?;
            if let Some(relation) = query.plan().relation {
                self.match_expansions.borrow_mut()
                    .extend(graph.vertices.iter().copied().map(|src| (src, relation)));
            }
            let edges = graph.edges.values()
                .filter(|edge| graph.vertices.contains(&edge.0) && graph.vertices.contains(&edge.2))
                .copied();
            let count = u64::try_from(edges.clone().count()).expect("an admitted table count fits u64");
            logical.execute_governed(count, [], edges, |vid, predicates| {
                Ok::<_, WriteTxnError>(self.vertex(database, vid)?.is_some_and(|row| {
                    predicates.iter().all(|p| p.matches(&row.labels, &row.props))
                }))
            }, policy, checkpoint)
        } else {
            let mut usage = crate::gql_exec::AdmissionUsage::default();
            let rows = self.governed_node_rows(database, query.plan().src_label, &mut |event| {
                checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
                usage.observe::<WriteTxnError, C>(policy, event)
            })?;
            let count = u64::try_from(rows.len()).expect("an admitted table count fits u64");
            let result = logical.execute_governed(count, rows.keys().copied(), [], |vid, predicates| {
                Ok::<_, WriteTxnError>(rows.get(&vid).is_some_and(|row| row.matches(predicates)))
            }, usage.remaining(policy), checkpoint);
            usage.finish(policy, result)
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

    #[test]
    fn governed_cancellation_checks_before_admission_and_uses_one_source() {
        for cancelled in [false, true] {
            let reader = Reader { edge_reads: Cell::new(0), vertex_reads: Cell::new(0), point_reads: Cell::new(0) };
            let query = fgdb_gql::PreparedGqlQuery::prepare("MATCH (a)-[:R]->(b) RETURN b",
                &RelationBind::new().with_relation("R", RelationId(1))).unwrap();
            let result = execute_governed_snapshot_query(&reader, &query, CommitSeq(1),
                fgdb_gql::GqlQueryPolicy::new(1, 1, 100, 100), || {
                    if cancelled { Err("already cancelled") } else { Ok(()) }
                });
            if cancelled {
                assert!(matches!(result, Err(fgdb_gql::GqlQueryError::Interrupted("already cancelled"))));
                assert_eq!(reader.edge_reads.get(), 0);
            } else {
                let result = result.unwrap();
                assert_eq!(result.value, vec![VId(2)]);
                assert_eq!(result.rows.snapshot_records, 1);
                assert_eq!(reader.edge_reads.get(), 1);
            }
            assert_eq!(reader.vertex_reads.get(), 0);
            assert_eq!(reader.point_reads.get(), 0);
        }
    }

    #[test]
    fn interrupted_overlay_keeps_only_admissions_that_actually_happened() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        let ((), report) = run_async_under_lab(0xc0a1_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            for stop in [1, 2] {
                let keys = crate::DatabaseKeys::new([0x91; 32], DatabaseSecurityNamespaceId([0x92; 32]), [0x93; 32]);
                let mut db = Database::open_memory(&commit, keys).await.unwrap();
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(RelationId(1));
                stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let query = fgdb_gql::PreparedGqlQuery::prepare("MATCH (a:L) RETURN a",
                    &RelationBind::new().with_label("L", fgdb_delta_types::LabelId(1))).unwrap();
                let calls = Cell::new(0);
                let result = txn.execute_governed_with_checkpoint(&db, &query,
                    fgdb_gql::GqlQueryPolicy::new(10, 10, 100, 100), || {
                        calls.set(calls.get() + 1);
                        if calls.get() == stop { Err(stop) } else { Ok(()) }
                    });
                assert!(matches!(result, Err(fgdb_gql::GqlQueryError::Interrupted(at)) if at == stop));
                let mut winner = WriteBatch::new(RelationId(1));
                winner.create_vertex(VId(1), vec![fgdb_delta_types::LabelId(1)], vec![]);
                db.write(&commit, winner).await.unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if stop == 1 {
                    result.expect("a query cancelled before admission did not observe the table");
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(result, Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
