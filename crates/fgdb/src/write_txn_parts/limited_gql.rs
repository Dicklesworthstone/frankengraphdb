/// Limited and unlimited durable reads share one admission owner and evaluator.
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
    /// Evaluator-only work/scratch limits; source admission is outside them.
    pub fn execute_prepared_query_limited(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<GqlError>> {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
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

    /// One source/evaluator allowance and the real query context. No partial
    /// result or success certificate is issued on refusal.
    pub fn execute_prepared_query_governed(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        self.execute_prepared_query_governed_at(cx, query, as_of, policy)
    }

    pub fn execute_prepared_query_governed_at(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        self.ensure_readable()
            .and_then(|()| self.snapshot.check_frontier(as_of))
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_governed_snapshot_query(self, query, as_of, policy, || cx.checkpoint())
        })
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
    ) -> Result<
        fgdb_gql::GqlQueryExecution,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        self.execute_prepared_query_governed_at(cx, query, self.frontier(), policy)
    }

    pub fn execute_prepared_query_governed_at(
        &self,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution,
        fgdb_gql::GqlQueryError<GqlError, Box<asupersync::error::Error>>,
    > {
        self.snapshot
            .check_frontier(as_of)
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        cx.with_restriction(|| {
            execute_governed_snapshot_query(self, query, as_of, policy, || cx.checkpoint())
        })
    }
}

impl WriteTxn {
    /// Preserve the existing evaluator-only limit contract, using the same
    /// canonical borrowed overlay as ordinary and row-budgeted queries.
    pub fn execute_prepared_query_limited<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        limits: fgdb_gql::GlaExecutionLimits,
    ) -> Result<fgdb_gql::GlaExecution, fgdb_gql::GlaExecutionError<WriteTxnError>> {
        let source = self
            .query_source(database, query.plan())
            .map_err(fgdb_gql::GlaExecutionError::Source)?;
        source.logical.execute_with_limits(
            source.vertex_ids(),
            source.edge_triples(),
            |vid, predicates| Ok(source.matches(vid, predicates)),
            limits,
        )
    }

    /// Govern history selection, negative-read/witness retention, canonical
    /// overlay construction and evaluation with one allowance. Scratch counts
    /// logical entries, not allocator bytes or the whole transaction lifetime.
    pub fn execute_prepared_query_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution,
        fgdb_gql::GqlQueryError<WriteTxnError, Box<asupersync::error::Error>>,
    > {
        cx.with_restriction(|| {
            self.execute_governed_with_checkpoint(database, query, policy, || cx.checkpoint())
        })
    }

    fn execute_governed_with_checkpoint<V: Vfs + Clone, C>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        policy: fgdb_gql::GqlQueryPolicy,
        mut checkpoint: impl FnMut() -> Result<(), C>,
    ) -> Result<fgdb_gql::GqlQueryExecution, fgdb_gql::GqlQueryError<WriteTxnError, C>> {
        let snapshot = self
            .query_snapshot(database)
            .map_err(fgdb_gql::GqlQueryError::Source)?;
        checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
        let mut usage = crate::gql_exec::AdmissionUsage::default();
        let source = self.query_source_over(snapshot, query.plan(), &mut |event| {
            checkpoint().map_err(fgdb_gql::GqlQueryError::Interrupted)?;
            usage.observe::<WriteTxnError, C>(policy, event)
        })?;
        let result = source.logical.execute_governed(
            count_as_u64(source.snapshot_records),
            source.vertex_ids(),
            source.edge_triples(),
            |vid, predicates| Ok::<_, WriteTxnError>(source.matches(vid, predicates)),
            usage.remaining(policy),
            checkpoint,
        );
        usage.finish(policy, result)
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
                return Err(ReadError::BeyondFrontier {
                    asked: as_of,
                    frontier: CommitSeq(1),
                });
            }
            Ok(vec![EdgeRecord {
                entry: AdjacencyEntry {
                    src: VId(1),
                    relation: RelationId(1),
                    dst: VId(2),
                    eid: EId(10),
                    created_at: CommitSeq(1),
                    retired_at: None,
                },
                props: vec![],
            }])
        }
    }

    #[test]
    fn limited_execution_does_not_read_the_source_table_twice() {
        let reader = Reader {
            edge_reads: Cell::new(0),
            vertex_reads: Cell::new(0),
            point_reads: Cell::new(0),
        };
        let query = fgdb_gql::PreparedGqlQuery::prepare(
            "MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", RelationId(1)),
        )
        .expect("prepare");
        let result = execute_limited_snapshot_query(
            &reader,
            &query,
            CommitSeq(1),
            fgdb_gql::GlaExecutionLimits::new(100, 100),
        )
        .expect("limited read");
        assert_eq!(result.value, vec![VId(2)]);
        assert_eq!(reader.edge_reads.get(), 1);
        assert_eq!(reader.vertex_reads.get(), 0);
        assert_eq!(reader.point_reads.get(), 0);
    }

    #[test]
    fn invalid_snapshot_is_not_masked_by_a_zero_work_limit() {
        let reader = Reader {
            edge_reads: Cell::new(0),
            vertex_reads: Cell::new(0),
            point_reads: Cell::new(0),
        };
        let query = fgdb_gql::PreparedGqlQuery::prepare(
            "MATCH (a)-[:R]->(b) RETURN b",
            &RelationBind::new().with_relation("R", RelationId(1)),
        )
        .expect("prepare");
        let error = execute_limited_snapshot_query(
            &reader,
            &query,
            CommitSeq(2),
            fgdb_gql::GlaExecutionLimits::new(0, 0),
        )
        .expect_err("future snapshot");
        assert!(matches!(
            error,
            fgdb_gql::GlaExecutionError::Source(GqlError::Read(ReadError::BeyondFrontier { .. }))
        ));
    }

    #[test]
    fn ordinary_and_budgeted_execution_also_admit_the_source_once() {
        for budgeted in [false, true] {
            let reader = Reader {
                edge_reads: Cell::new(0),
                vertex_reads: Cell::new(0),
                point_reads: Cell::new(0),
            };
            let query = fgdb_gql::PreparedGqlQuery::prepare(
                "MATCH (a)-[:R]->(b) RETURN b",
                &RelationBind::new().with_relation("R", RelationId(1)),
            )
            .expect("prepare");
            let rows = if budgeted {
                let run = execute_budgeted_at(
                    &reader,
                    &query,
                    CommitSeq(1),
                    fgdb_gql::GqlExecutionBudget::new(1, 1),
                )
                .expect("exact admission budget");
                assert_eq!(run.stats.snapshot_records, 1);
                run.value
            } else {
                crate::gql_exec::execute_at(query.plan(), &reader, CommitSeq(1))
                    .expect("ordinary read")
            };
            assert_eq!(rows, vec![VId(2)]);
            assert_eq!(reader.edge_reads.get(), 1);
            assert_eq!(reader.vertex_reads.get(), 0);
        }
    }

    #[test]
    fn governed_cancellation_checks_before_admission_and_uses_one_source() {
        for cancelled in [false, true] {
            let reader = Reader {
                edge_reads: Cell::new(0),
                vertex_reads: Cell::new(0),
                point_reads: Cell::new(0),
            };
            let query = fgdb_gql::PreparedGqlQuery::prepare(
                "MATCH (a)-[:R]->(b) RETURN b",
                &RelationBind::new().with_relation("R", RelationId(1)),
            )
            .unwrap();
            let result = execute_governed_snapshot_query(
                &reader,
                &query,
                CommitSeq(1),
                fgdb_gql::GqlQueryPolicy::new(1, 1, 100, 100),
                || {
                    if cancelled {
                        Err("already cancelled")
                    } else {
                        Ok(())
                    }
                },
            );
            if cancelled {
                assert!(matches!(
                    result,
                    Err(fgdb_gql::GqlQueryError::Interrupted("already cancelled"))
                ));
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
    fn governed_node_overlay_checks_every_phase_and_shares_one_allowance() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_gql::{GlaLimitDimension, GqlBudgetDimension, GqlQueryError, GqlQueryPolicy};
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        let ((), report) = run_async_under_lab(0xc0a1_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let keys = crate::DatabaseKeys::new(
                [0x94; 32],
                DatabaseSecurityNamespaceId([0x95; 32]),
                [0x96; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let label = LabelId(1);
            let key = PropertyKeyId(1);
            let mut base = WriteBatch::new(RelationId(1));
            for vid in 1..=4 {
                base.create_vertex(
                    VId(vid),
                    vec![if vid == 3 { LabelId(2) } else { label }],
                    vec![
                        (key, CanonicalScalar::Int(vid as i64)),
                        (
                            PropertyKeyId(2),
                            CanonicalScalar::bytes(vec![0x51; 4_000]).unwrap(),
                        ),
                    ],
                );
            }
            db.write(&commit, base).await.unwrap();
            let query = fgdb_gql::PreparedGqlQuery::prepare(
                "MATCH (a:L) WHERE a.n=42 RETURN a",
                &RelationBind::new()
                    .with_label("L", label)
                    .with_property("n", key),
            )
            .unwrap();
            let staged = || {
                let mut batch = WriteBatch::new(RelationId(1));
                batch.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(42)));
                batch.delete_vertex(VId(2));
                batch.set_vertex_label(VId(3), label, true);
                batch.set_vertex_label(VId(3), LabelId(2), false);
                batch.set_vertex_property(VId(3), key, Some(CanonicalScalar::Int(42)));
                batch.set_vertex_property(VId(4), key, None);
                batch.create_vertex(VId(9), vec![label], vec![(key, CanonicalScalar::Int(42))]);
                batch.ensure_vertex(VId(1), vec![], vec![(key, CanonicalScalar::Int(-1))]);
                batch.compare_and_set_vertex_property(
                    VId(1),
                    key,
                    Some(CanonicalScalar::Int(-1)),
                    CanonicalScalar::Int(-2),
                    crate::WriteMismatchPolicy::NoOp,
                );
                batch.create_vertex(VId(12), vec![label], vec![]);
                batch.delete_vertex(VId(12));
                batch
            };
            let policy = GqlQueryPolicy::new(4, 3, 10_000, 10_000);
            let mut txn = db.begin(&txn_cx).unwrap();
            txn.write(&mut db, staged()).unwrap();
            let mut total = 0;
            let complete = txn
                .execute_governed_with_checkpoint(&db, &query, policy, || {
                    total += 1;
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert_eq!(complete.value, vec![VId(1), VId(3), VId(9)]);
            assert_eq!(complete.rows.snapshot_records, 4);
            assert_eq!(complete.rows.result_rows, 3);
            assert_eq!(
                txn.execute_prepared_query(&db, &query).unwrap(),
                complete.value
            );
            assert_eq!(
                txn.execute_governed_with_checkpoint(&db, &query, policy, || Ok::<_, ()>(()))
                    .unwrap(),
                complete,
                "warming the persistent read set must not change policy cost"
            );
            txn.abort();

            // Fresh transactions keep the prior observation state identical,
            // so every source, effect and evaluator checkpoint is tested.
            for stop in 1..=total {
                let mut txn = db.begin(&txn_cx).unwrap();
                txn.write(&mut db, staged()).unwrap();
                let mut calls = 0;
                let result = txn.execute_governed_with_checkpoint(&db, &query, policy, || {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                });
                assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
                assert_eq!(calls, stop);
                txn.abort();
            }
            let work = complete.evaluator.work_units;
            let scratch = complete.evaluator.scratch_entries;
            for (work_cap, scratch_cap, dimension) in [
                (work, scratch, None),
                (work - 1, scratch, Some(GlaLimitDimension::WorkUnits)),
                (work, scratch - 1, Some(GlaLimitDimension::ScratchEntries)),
                (0, scratch, Some(GlaLimitDimension::WorkUnits)),
                (work, 0, Some(GlaLimitDimension::ScratchEntries)),
            ] {
                let mut txn = db.begin(&txn_cx).unwrap();
                txn.write(&mut db, staged()).unwrap();
                let result = txn.execute_governed_with_checkpoint(
                    &db,
                    &query,
                    GqlQueryPolicy::new(4, 3, work_cap, scratch_cap),
                    || Ok::<_, ()>(()),
                );
                if let Some(dimension) = dimension {
                    let Err(GqlQueryError::Evaluator(exceeded)) = result else {
                        panic!("{result:?}")
                    };
                    assert_eq!(exceeded.dimension, dimension);
                    let limit = if dimension == GlaLimitDimension::WorkUnits {
                        work_cap
                    } else {
                        scratch_cap
                    };
                    assert_eq!(exceeded.limit, limit);
                    assert_eq!(exceeded.observed, u128::from(limit) + 1);
                } else {
                    assert_eq!(result.unwrap(), complete);
                }
                txn.abort();
            }
            let mut txn = db.begin(&txn_cx).unwrap();
            txn.write(&mut db, staged()).unwrap();
            let result = txn.execute_governed_with_checkpoint(
                &db,
                &query,
                GqlQueryPolicy::new(3, 3, work, scratch),
                || Ok::<_, ()>(()),
            );
            assert!(matches!(result, Err(GqlQueryError::Rows(exceeded))
                if exceeded.dimension == GqlBudgetDimension::SnapshotRecords
                    && exceeded.limit == 3 && exceeded.observed == 4));
            txn.abort();

            // The basis has four live vertices, but its final staged table is
            // empty. A zero snapshot-record allowance must accept that table.
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut deleted = WriteBatch::new(RelationId(1));
            for id in 1..=4 {
                deleted.delete_vertex(VId(id));
            }
            txn.write(&mut db, deleted).unwrap();
            let result = txn
                .execute_governed_with_checkpoint(
                    &db,
                    &query,
                    GqlQueryPolicy::new(0, 0, 10_000, 10_000),
                    || Ok::<_, ()>(()),
                )
                .unwrap();
            assert!(result.value.is_empty());
            assert_eq!(result.rows.snapshot_records, 0);
            assert_eq!(result.rows.result_rows, 0);
            txn.abort();
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn interrupted_overlay_keeps_only_admissions_that_actually_happened() {
        use asupersync::lab::run_async_under_lab;
        use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
        let ((), report) = run_async_under_lab(0xc0a1_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            // Checkpoints: 1=pre-admission, 2=source start, 3=label witness
            // reservation, 4/5=staged metadata after the witness was retained.
            for stop in [1, 2, 3, 4, 5] {
                let keys = crate::DatabaseKeys::new(
                    [0x91; 32],
                    DatabaseSecurityNamespaceId([0x92; 32]),
                    [0x93; 32],
                );
                let mut db = Database::open_memory(&commit, keys).await.unwrap();
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut stage = WriteBatch::new(RelationId(1));
                stage.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, stage).unwrap();
                let query = fgdb_gql::PreparedGqlQuery::prepare(
                    "MATCH (a:L) RETURN a",
                    &RelationBind::new().with_label("L", fgdb_delta_types::LabelId(1)),
                )
                .unwrap();
                let calls = Cell::new(0);
                let result = txn.execute_governed_with_checkpoint(
                    &db,
                    &query,
                    fgdb_gql::GqlQueryPolicy::new(10, 10, 100, 100),
                    || {
                        calls.set(calls.get() + 1);
                        if calls.get() == stop {
                            Err(stop)
                        } else {
                            Ok(())
                        }
                    },
                );
                assert!(
                    matches!(result, Err(fgdb_gql::GqlQueryError::Interrupted(at)) if at == stop)
                );
                assert_eq!(
                    txn.scanned_vertex_labels
                        .borrow()
                        .contains(&fgdb_delta_types::LabelId(1)),
                    stop >= 4
                );
                let mut winner = WriteBatch::new(RelationId(1));
                winner.create_vertex(VId(1), vec![fgdb_delta_types::LabelId(1)], vec![]);
                db.write(&commit, winner).await.unwrap();
                let result = txn.commit(&mut db, &commit).await;
                if stop <= 3 {
                    result.expect("interrupted before witness retention and data admission");
                    assert!(db.vertex(VId(99)).unwrap().is_some());
                } else {
                    assert!(matches!(
                        result,
                        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                            law: "FG-LAW-FCW-READ-01",
                            ..
                        }))
                    ));
                    assert!(db.vertex(VId(99)).unwrap().is_none());
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
