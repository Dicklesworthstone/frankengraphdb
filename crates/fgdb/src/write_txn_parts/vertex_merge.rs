// Unique vertex MERGE has one source-parameterized collector. Native overlays
// and capability-masked sources share uniqueness, creation and accounting.

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

// A proposal is private until its owner has staged and completed it. It is not
// a second evaluator: the supplied GLA source and ordinary insertion collector
// still own all expression, matching and allocation semantics.
struct VertexMergeProposal {
    stats: fgdb_gql::GraphVertexMergeStats,
    outcome: fgdb_gql::GraphVertexMergeOutcome,
    creation: Option<fgdb_gql::insertion::GraphInsertBatch>,
}

type VertexMergeFault<E, A, C> =
    fgdb_gql::GqlQueryError<fgdb_gql::GraphVertexMergeError<E, A>, C>;
type VertexMergeCollection<E, A, C> = Result<VertexMergeProposal, VertexMergeFault<E, A, C>>;
type VertexMergeSelection<E, C> = Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<E, C>,
>;

fn collect_vertex_merge<E, A, C>(
    merge: &fgdb_gql::PreparedGraphVertexMerge,
    policy: fgdb_gql::GraphVertexMergePolicy,
    source: impl FnOnce(
        &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> VertexMergeSelection<E, C>,
    allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> VertexMergeCollection<E, A, C> {
    use fgdb_gql::insertion::{GraphInsertIntent, GraphInsertPolicy};
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
        GqlQueryError, GraphVertexMergeError, GraphVertexMergeOutcome, GraphVertexMergeStats,
    };
    let selected = source(merge.selection(), policy.query)
        .map_err(|error| error.map_source(GraphVertexMergeError::Source))?;
    if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
        return Err(GqlQueryError::Source(GraphVertexMergeError::InvalidSourceStatistics));
    }
    let mut evaluator = selected.evaluator;
    let mut event = |kind: GlaExecutionEvent| -> Result<(), VertexMergeFault<E, A, C>> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
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
                row: row_at, column: row.len().min(expected_columns),
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
    checkpoint().map_err(GqlQueryError::Interrupted)?;
    if matches.len() > 1 {
        return Err(GqlQueryError::Source(GraphVertexMergeError::AmbiguousMatches {
            observed: matches.len() as u64,
        }));
    }
    if let Some(vertex) = matches.into_iter().next() {
        return Ok(VertexMergeProposal {
            stats: GraphVertexMergeStats {
                match_selection: selected.rows, evaluator, created_vertices: 0,
            },
            outcome: GraphVertexMergeOutcome::Matched(vertex),
            creation: None,
        });
    }
    // Creation receives only the remaining MATCH/reduction allowance. Its unit
    // input is internal, but must not obtain a fresh query or evaluator quota.
    let remaining = fgdb_gql::GqlQueryPolicy {
        rows: merge_remaining_budget(policy.query.rows, selected.rows),
        evaluator: fgdb_gql::GlaExecutionLimits::new(
            policy.query.evaluator.max_work_units.checked_sub(evaluator.work_units)
                .expect("successful reduction is within the work limit"),
            policy.query.evaluator.max_scratch_entries.checked_sub(evaluator.scratch_entries)
                .expect("successful reduction is within the scratch limit"),
        ),
    };
    let created = merge.creation().execute_governed::<E, A, C>(
        GraphInsertPolicy::new(remaining, policy.max_created_vertices, 0),
        |_, _| unreachable!("validated MERGE creation is standalone"),
        allocate,
        &mut checkpoint,
    );
    let creation = match created {
        Ok(value) => value,
        Err(GqlQueryError::Source(error)) => {
            return Err(GqlQueryError::Source(GraphVertexMergeError::Creation(error)));
        }
        Err(GqlQueryError::Rows(mut error)) => {
            let used = match error.dimension {
                GqlBudgetDimension::SnapshotRecords => selected.rows.snapshot_records,
                GqlBudgetDimension::ResultRows => selected.rows.result_rows,
            };
            error.limit = match error.dimension {
                GqlBudgetDimension::SnapshotRecords => policy.query.rows.max_snapshot_records(),
                GqlBudgetDimension::ResultRows => policy.query.rows.max_result_rows(),
            }.expect("a remaining budget error implies an original finite limit");
            error.observed = error.observed.checked_add(used)
                .ok_or(GqlQueryError::Source(GraphVertexMergeError::AccountingOverflow))?;
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
    let [GraphInsertIntent::Vertex { vertex, .. }] = creation.intents() else {
        unreachable!("validated MERGE creation produces exactly one vertex")
    };
    let outcome = GraphVertexMergeOutcome::Created(*vertex);
    let inserted = creation.stats();
    let stats = GraphVertexMergeStats {
        match_selection: selected.rows,
        evaluator: fgdb_gql::GlaExecutionStats {
            work_units: evaluator.work_units.checked_add(inserted.evaluator.work_units)
                .expect("MERGE work is bounded by the original u64 policy"),
            scratch_entries: evaluator.scratch_entries.checked_add(inserted.evaluator.scratch_entries)
                .expect("MERGE scratch is bounded by the original u64 policy"),
        },
        created_vertices: 1,
    };
    Ok(VertexMergeProposal { stats, outcome, creation: Some(creation) })
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
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphVertexMergeStats, fgdb_gql::GraphVertexMergeOutcome),
        TxnGqlError<fgdb_gql::GraphVertexMergeError<WriteTxnError, A>>,
    > {
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertIntent};
        use fgdb_gql::{GqlQueryError, GraphVertexMergeError};
        let infrastructure = |error| GqlQueryError::Source(GraphVertexMergeError::Source(error));
        self.ensure_database(database).map_err(infrastructure)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && merge.relation() != first.relation
        {
            return Err(infrastructure(WriteTxnError::RelationMismatch {
                expected: first.relation, found: merge.relation(),
            }));
        }
        cx.with_restriction(|| {
            let proposal = collect_vertex_merge(
                merge, policy,
                |pattern, allowance| self.execute_graph_pattern_governed(database, cx, pattern, allowance),
                allocate,
                || cx.checkpoint(),
            )?;
            if let Some(creation) = proposal.creation {
                let mut batch = WriteBatch::new(merge.relation());
                for intent in creation.into_intents() {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    let GraphInsertIntent::Vertex { vertex, labels, properties } = intent else {
                        unreachable!("validated vertex MERGE contains no edge creation")
                    };
                    batch.create_vertex(vertex, labels, properties);
                }
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                // Preserve the insertion adapter's original composition choice.
                let staged = if self.staged.iter().all(|batch| batch.relation == merge.relation()) {
                    self.write(database, batch)
                } else {
                    self.write_atomic(database, vec![batch])
                };
                staged.map_err(|error| GqlQueryError::Source(
                    GraphVertexMergeError::Creation(GraphInsertError::Source(error)),
                ))?;
            }
            // No fallible work follows atomic staging.
            Ok((proposal.stats, proposal.outcome))
        })
    }
}
