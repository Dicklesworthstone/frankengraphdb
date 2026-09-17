// Unique vertex MERGE composes the existing overlay matcher and standalone
// insertion adapter. It owns only uniqueness reduction and cumulative accounting.

fn merge_remaining_budget(
    original: fgdb_gql::GqlExecutionBudget,
    used: fgdb_gql::GqlExecutionStats,
) -> fgdb_gql::GqlExecutionBudget {
    let snapshot = original.max_snapshot_records().map(|limit| {
        limit.checked_sub(used.snapshot_records)
            .expect("successful match selection is within its snapshot limit")
    });
    let results = original.max_result_rows().map(|limit| {
        limit.checked_sub(used.result_rows)
            .expect("successful match selection is within its result limit")
    });
    match (snapshot, results) {
        (None, None) => fgdb_gql::GqlExecutionBudget::UNLIMITED,
        (Some(snapshot), None) => fgdb_gql::GqlExecutionBudget::snapshot_records(snapshot),
        (None, Some(results)) => fgdb_gql::GqlExecutionBudget::result_rows(results),
        (Some(snapshot), Some(results)) => fgdb_gql::GqlExecutionBudget::new(snapshot, results),
    }
}

impl WriteTxn {
    /// Match exactly one distinct vertex or create exactly one new vertex.
    /// Repeated occurrences of the same vertex are one match; two or more
    /// distinct identities refuse. A missing match invokes the definition's
    /// validated standalone single-vertex insertion through the ordinary
    /// allocator and storage-preparation path.
    ///
    /// The match scan remains a transaction conflict witness. Therefore a
    /// concurrent insertion that changes the matched domain after a zero-match
    /// decision invalidates completion instead of silently creating a duplicate.
    /// Success only stages the create branch; an existing match is read-only.
    pub fn execute_graph_vertex_merge_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphVertexMerge,
        policy: fgdb_gql::GraphVertexMergePolicy,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphVertexMergeStats, fgdb_gql::GraphVertexMergeOutcome),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphVertexMergeError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{
            GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
            GqlQueryError, GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergeStats,
        };
        let infrastructure = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));

        self.ensure_database(database).map_err(infrastructure)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && merge.relation() != first.relation
        {
            return Err(infrastructure(WriteTxnError::RelationMismatch {
                expected: first.relation, found: merge.relation(),
            }));
        }

        cx.with_restriction(|| {
            let selected = self.execute_graph_pattern_governed(
                database, cx, merge.selection(), policy.query,
            ).map_err(|error| error.map_source(GraphVertexMergeError::Source))?;
            if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
                return Err(GqlQueryError::Source(GraphVertexMergeError::InvalidSourceStatistics));
            }

            let mut evaluator = selected.evaluator;
            let mut event = |kind: GlaExecutionEvent| -> Result<
                (),
                GqlQueryError<GraphVertexMergeError<WriteTxnError, A>, Box<asupersync::error::Error>>,
            > {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                let work = u128::from(evaluator.work_units) + 1;
                let scratch = u128::from(evaluator.scratch_entries)
                    + u128::from(kind == GlaExecutionEvent::ScratchEntry);
                for (observed, limit, dimension) in [
                    (work, policy.query.evaluator.max_work_units, GlaLimitDimension::WorkUnits),
                    (scratch, policy.query.evaluator.max_scratch_entries, GlaLimitDimension::ScratchEntries),
                ] {
                    if observed > u128::from(limit) {
                        return Err(GqlQueryError::Evaluator(GlaLimitExceeded { dimension, limit, observed }));
                    }
                }
                evaluator.work_units = work as u64;
                evaluator.scratch_entries = scratch as u64;
                Ok(())
            };

            event(GlaExecutionEvent::Work)?;
            let expected_columns = merge.selection().columns().len();
            let target = merge.target_column();
            let mut matches = std::collections::BTreeSet::new();
            for (row_at, row) in selected.value.iter().enumerate() {
                event(GlaExecutionEvent::Work)?;
                if row.len() != expected_columns || target >= row.len() {
                    return Err(GqlQueryError::Source(GraphVertexMergeError::InputSchema {
                        row: row_at,
                        column: row.len().min(expected_columns),
                    }));
                }
                let value = &row.values()[target];
                if value.is_null() { continue; }
                let Some(vertex) = value.as_vertex() else {
                    return Err(GqlQueryError::Source(GraphVertexMergeError::InputSchema {
                        row: row_at, column: target,
                    }));
                };
                if !matches.contains(&vertex) {
                    event(GlaExecutionEvent::ScratchEntry)?;
                    matches.insert(vertex);
                }
            }
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            if matches.len() > 1 {
                return Err(GqlQueryError::Source(GraphVertexMergeError::AmbiguousMatches {
                    observed: matches.len() as u64,
                }));
            }
            if let Some(vertex) = matches.into_iter().next() {
                return Ok((
                    GraphVertexMergeStats {
                        match_selection: selected.rows,
                        evaluator,
                        created_vertices: 0,
                    },
                    GraphVertexMergeOutcome::Matched(vertex),
                ));
            }

            // The creation arm receives only the caller's REMAINING allowance.
            // Its standalone unit therefore cannot obtain a second full query
            // budget merely because MERGE selected zero rows.
            let remaining_rows = merge_remaining_budget(policy.query.rows, selected.rows);
            let remaining = fgdb_gql::GqlQueryPolicy {
                rows: remaining_rows,
                evaluator: fgdb_gql::GlaExecutionLimits::new(
                    policy.query.evaluator.max_work_units
                        .checked_sub(evaluator.work_units)
                        .expect("successful reduction is within the work limit"),
                    policy.query.evaluator.max_scratch_entries
                        .checked_sub(evaluator.scratch_entries)
                        .expect("successful reduction is within the scratch limit"),
                ),
            };
            let insertion_policy = fgdb_gql::insertion::GraphInsertPolicy::new(
                remaining, policy.max_created_vertices, 0,
            );
            let created = self.execute_graph_insert_returning_governed(
                database, cx, merge.creation(), insertion_policy, &mut allocate,
            );
            let (insert_stats, vertices, edges) = match created {
                Ok(value) => value,
                Err(GqlQueryError::Source(error)) => {
                    return Err(GqlQueryError::Source(GraphVertexMergeError::Creation(error)));
                }
                Err(GqlQueryError::Rows(mut error)) => {
                    let used = match error.dimension {
                        GqlBudgetDimension::SnapshotRecords => selected.rows.snapshot_records,
                        GqlBudgetDimension::ResultRows => selected.rows.result_rows,
                    };
                    let original = match error.dimension {
                        GqlBudgetDimension::SnapshotRecords => policy.query.rows.max_snapshot_records(),
                        GqlBudgetDimension::ResultRows => policy.query.rows.max_result_rows(),
                    }.expect("a remaining row-budget error implies an original finite limit");
                    error.limit = original;
                    error.observed = error.observed.checked_add(used)
                        .expect("observed row counts fit u64");
                    return Err(GqlQueryError::Rows(error));
                }
                Err(GqlQueryError::Evaluator(mut error)) => {
                    match error.dimension {
                        GlaLimitDimension::WorkUnits => {
                            error.limit = policy.query.evaluator.max_work_units;
                            error.observed += u128::from(evaluator.work_units);
                        }
                        GlaLimitDimension::ScratchEntries => {
                            error.limit = policy.query.evaluator.max_scratch_entries;
                            error.observed += u128::from(evaluator.scratch_entries);
                        }
                    }
                    return Err(GqlQueryError::Evaluator(error));
                }
                Err(GqlQueryError::Interrupted(error)) => return Err(GqlQueryError::Interrupted(error)),
                Err(GqlQueryError::IdentifiedEdgesRequired) => return Err(GqlQueryError::IdentifiedEdgesRequired),
            };
            debug_assert_eq!(insert_stats.created_vertices, 1);
            debug_assert_eq!(insert_stats.created_edges, 0);
            debug_assert!(edges.is_empty());
            let [vertex] = vertices.as_slice() else {
                unreachable!("validated MERGE creation produces exactly one vertex")
            };
            // Successful insertion cannot exceed its remaining limits, so these
            // sums are bounded by the caller's original u64 limits. No fallible
            // operation follows staging.
            let evaluator = fgdb_gql::GlaExecutionStats {
                work_units: evaluator.work_units.checked_add(insert_stats.evaluator.work_units)
                    .expect("MERGE work is bounded by the original u64 policy"),
                scratch_entries: evaluator.scratch_entries.checked_add(insert_stats.evaluator.scratch_entries)
                    .expect("MERGE scratch is bounded by the original u64 policy"),
            };
            Ok((
                GraphVertexMergeStats {
                    match_selection: selected.rows,
                    evaluator,
                    created_vertices: 1,
                },
                GraphVertexMergeOutcome::Created(*vertex),
            ))
        })
    }
}
