// One directed relationship MERGE collector for native and authorized hosts.
// Source adapters own visibility and conflict witnesses, never uniqueness,
// creation semantics, cumulative budgets or a second commit mechanism.

struct EdgeMergeProposal<Output = fgdb_gql::GraphEdgeMergeOutcome> {
    stats: fgdb_gql::GraphEdgeMergeStats,
    outcome: Output,
    creation: Option<WriteBatch>,
}

// The singular primitive and the native program differ only in admitted
// cardinality and receipt ownership. They share selection, existence decisions,
// creation admission and staging. Singular calls allocate no new receipt vector.
trait EdgeMergeOutput: Sized {
    fn for_pairs(pairs: usize) -> Option<Self>;
    fn record<C>(
        &mut self,
        outcome: fgdb_gql::GraphEdgeMergeOutcome,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C>;
    fn result_rows(&self) -> u64;
}

impl EdgeMergeOutput for fgdb_gql::GraphEdgeMergeOutcome {
    fn for_pairs(pairs: usize) -> Option<Self> {
        (pairs <= 1).then_some(Self::NoInput)
    }

    fn record<C>(
        &mut self,
        outcome: Self,
        _control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        *self = outcome;
        Ok(())
    }

    fn result_rows(&self) -> u64 {
        u64::from(self.edge().is_some())
    }
}

impl EdgeMergeOutput for fgdb_gql::GraphWriteStepReceipt {
    fn for_pairs(pairs: usize) -> Option<Self> {
        Some(if pairs <= 1 {
            Self::EdgeMerge {
                outcome: fgdb_gql::GraphEdgeMergeOutcome::NoInput,
            }
        } else {
            Self::EdgeMergeBatch {
                outcomes: Vec::new(),
                created: Vec::new(),
            }
        })
    }

    fn record<C>(
        &mut self,
        next: fgdb_gql::GraphEdgeMergeOutcome,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        match self {
            Self::EdgeMerge { outcome } => *outcome = next,
            Self::EdgeMergeBatch { outcomes, created } => {
                control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
                if let fgdb_gql::GraphEdgeMergeOutcome::Created(edge) = next {
                    control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
                    created.push(edge);
                }
                outcomes.push(next);
            }
            _ => unreachable!("MERGE collector owns its receipt kind"),
        }
        Ok(())
    }

    fn result_rows(&self) -> u64 {
        match self {
            Self::EdgeMerge { outcome } => outcome.result_rows(),
            Self::EdgeMergeBatch { outcomes, .. } => outcomes.len() as u64,
            _ => unreachable!("MERGE collector owns its receipt kind"),
        }
    }
}

type EdgeMergeFault<E, A, C> = fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeMergeError<E, A>, C>;
type EdgeMergeSelection<E, C> = Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<E, C>,
>;

// Private trusted seam: all endpoint rows are selected and validated before
// any existence inspection. Each callback must retain ALL matching live EIds
// in its admitted domain and native absence witnesses, including on refusal.
// Canonical duplicate pairs collapse for plain MERGE; actions use the separate
// singular upsert owner until a per-occurrence action contract is implemented.
fn collect_edge_merge_output<Output: EdgeMergeOutput, E, A, C>(
    merge: &fgdb_gql::PreparedGraphEdgeMerge,
    policy: fgdb_gql::GraphEdgeMergePolicy,
    source: impl FnOnce(
        &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> EdgeMergeSelection<E, C>,
    mut edge_source: impl FnMut(VId, VId) -> Result<Vec<EdgeRecord>, fgdb_gql::GqlQueryError<E, C>>,
    mut allocate: impl FnMut(usize) -> Result<ElementId, A>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<EdgeMergeProposal<Output>, EdgeMergeFault<E, A, C>> {
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError,
        GraphEdgeMergeError, GraphEdgeMergeOutcome, GraphEdgeMergeStats,
    };
    let selected = source(merge.selection(), policy.query)
        .map_err(|error| error.map_source(GraphEdgeMergeError::Source))?;
    if u64::try_from(selected.value.len()).ok() != Some(selected.rows.result_rows) {
        return Err(GqlQueryError::Source(
            GraphEdgeMergeError::InvalidSourceStatistics,
        ));
    }

    let mut evaluator = selected.evaluator;
    // None retains a cancellation-only boundary from the native adapter.
    // Keep all calls through this closure so its mutable checkpoint borrow is
    // never aliased while later charged events are still possible.
    let mut event = |kind: Option<GlaExecutionEvent>| -> Result<(), EdgeMergeFault<E, A, C>> {
        checkpoint().map_err(GqlQueryError::Interrupted)?;
        let Some(kind) = kind else { return Ok(()) };
        let work = u128::from(evaluator.work_units) + 1;
        let scratch = u128::from(evaluator.scratch_entries)
            + u128::from(kind == GlaExecutionEvent::ScratchEntry);
        for (observed, limit, dimension) in [
            (
                work,
                policy.query.evaluator.max_work_units,
                GlaLimitDimension::WorkUnits,
            ),
            (
                scratch,
                policy.query.evaluator.max_scratch_entries,
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            if observed > u128::from(limit) {
                return Err(GqlQueryError::Evaluator(GlaLimitExceeded {
                    dimension,
                    limit,
                    observed,
                }));
            }
        }
        evaluator.work_units = work as u64;
        evaluator.scratch_entries = scratch as u64;
        Ok(())
    };

    event(Some(GlaExecutionEvent::Work))?;
    let width = merge.selection().columns().len();
    let source_column = merge.source_column();
    let destination_column = merge.destination_column();
    let mut pairs = std::collections::BTreeSet::new();
    for (row_at, row) in selected.value.iter().enumerate() {
        event(Some(GlaExecutionEvent::Work))?;
        if row.len() != width {
            return Err(GqlQueryError::Source(GraphEdgeMergeError::InputSchema {
                row: row_at,
                column: row.len().min(width),
            }));
        }
        let endpoint = |column: usize| -> Result<VId, EdgeMergeFault<E, A, C>> {
            let value = &row.values()[column];
            if value.is_null() {
                return Err(GqlQueryError::Source(GraphEdgeMergeError::NullEndpoint {
                    row: row_at,
                    column,
                }));
            }
            value.as_vertex().ok_or_else(|| {
                GqlQueryError::Source(GraphEdgeMergeError::InputSchema {
                    row: row_at,
                    column,
                })
            })
        };
        let pair = (endpoint(source_column)?, endpoint(destination_column)?);
        if !pairs.contains(&pair) {
            event(Some(GlaExecutionEvent::ScratchEntry))?;
            pairs.insert(pair);
        }
    }
    event(None)?;
    let mut outcome = Output::for_pairs(pairs.len()).ok_or_else(|| {
        GqlQueryError::Source(GraphEdgeMergeError::AmbiguousEndpointPairs {
            observed: pairs.len() as u64,
        })
    })?;
    let mut overlay_edges = 0u64;
    let mut created_edges = 0u64;
    let mut creation: Option<WriteBatch> = None;
    for (pair_at, (source, destination)) in pairs.into_iter().enumerate() {
        // A scoped host admits candidates BEFORE they reach these counters or
        // the uniqueness reducer. Repeated physical scans spend the SAME cap.
        let edges = edge_source(source, destination)
            .map_err(|error| error.map_source(GraphEdgeMergeError::Source))?;
        let observed = u128::from(overlay_edges) + edges.len() as u128;
        let records = u128::from(selected.rows.snapshot_records) + observed;
        policy
            .query
            .rows
            .check(
                GqlBudgetDimension::SnapshotRecords,
                records.try_into().unwrap_or(u64::MAX),
            )
            .map_err(GqlQueryError::Rows)?;
        if records > u128::from(u64::MAX) {
            return Err(GqlQueryError::Source(
                GraphEdgeMergeError::InvalidSourceStatistics,
            ));
        }
        overlay_edges = observed as u64;

        let mut matching = None;
        for edge in &edges {
            event(Some(GlaExecutionEvent::Work))?;
            if edge.entry.relation == merge.relation()
                && edge.entry.src == source
                && edge.entry.dst == destination
            {
                if matching.is_some() {
                    return Err(GqlQueryError::Source(
                        GraphEdgeMergeError::AmbiguousRelationships { observed: 2 },
                    ));
                }
                event(Some(GlaExecutionEvent::ScratchEntry))?;
                matching = Some(edge.entry.eid);
            }
        }
        if let Some(edge) = matching {
            outcome.record(GraphEdgeMergeOutcome::Matched(edge), &mut event)?;
            continue;
        }

        // Refuse the cumulative creation count before payload ownership or the
        // allocator. Existing matches never consume this separate allowance.
        let observed = u128::from(created_edges) + 1;
        if observed > u128::from(policy.max_created_edges) {
            return Err(GqlQueryError::Source(GraphEdgeMergeError::CreationLimit {
                limit: policy.max_created_edges,
                observed,
            }));
        }
        let mut properties = Vec::with_capacity(merge.properties().len());
        for (key, value) in merge.properties() {
            event(Some(GlaExecutionEvent::ScratchEntry))?;
            properties.push((*key, value.value().clone()));
        }
        event(None)?;
        let identity = allocate(pair_at)
            .map_err(|error| GqlQueryError::Source(GraphEdgeMergeError::IdentitySource(error)))?;
        let ElementId::Edge(edge) = identity else {
            return Err(GqlQueryError::Source(GraphEdgeMergeError::IdentityKind));
        };
        outcome.record(GraphEdgeMergeOutcome::Created(edge), &mut event)?;
        creation
            .get_or_insert_with(|| WriteBatch::new(merge.relation()))
            .add_edge(edge, source, destination, properties);
        created_edges += 1;
        // All proposals remain private until every pair has succeeded.
        event(None)?;
    }
    Ok(EdgeMergeProposal {
        stats: GraphEdgeMergeStats {
            match_selection: selected.rows,
            overlay_edges,
            evaluator,
            created_edges,
        },
        outcome,
        creation,
    })
}

impl WriteTxn {
    /// Resolve one selected directed endpoint pair against this transaction's
    /// complete edge overlay. No input is a read-only NoInput outcome; exactly
    /// one matching edge is Matched; parallel matching edges refuse; no matching
    /// edge asks the caller for one fresh EId and stages one ordinary edge create.
    ///
    /// Duplicate MATCH occurrences of the same endpoint pair collapse before
    /// edge lookup. More than one distinct endpoint pair refuses before reading
    /// the edge overlay or invoking the allocator. The complete overlay edge scan
    /// becomes a conflict witness, so a concurrent matching relationship created
    /// after a zero-edge decision invalidates completion rather than allowing a
    /// duplicate MERGE to serialize first.
    pub fn execute_graph_edge_merge_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphEdgeMerge,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        mut allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeMergeStats,
            fgdb_gql::GraphEdgeMergeOutcome,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeMergeError<WriteTxnError, A>>,
    > {
        self.execute_graph_edge_merge_output_governed(database, cx, merge, policy, |_| {
            allocate(fgdb_gql::GraphEdgeMergeRequest)
        })
    }

    /// Resolve every distinct directed endpoint pair from one frozen MATCH.
    /// Canonical pair order fixes decisions, allocation requests and receipts;
    /// repeated pairs create at most one edge. Existing parallel matches still
    /// refuse. This plain MERGE owner does not execute ON or trailing actions.
    ///
    /// Selection, every existence scan, receipt ownership and all creations
    /// share the original allowances. A later refusal stages none of the new
    /// edges, while observations and issued identity reservations remain. The
    /// complete ordinary WriteBatch is staged once after every pair succeeds.
    /// Zero/one pair retains the singular receipt; multiple pairs return the
    /// explicit EdgeMergeBatch receipt. No transaction is committed here.
    pub fn execute_graph_edge_merges_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphEdgeMerge,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeMergeStats,
            fgdb_gql::GraphWriteStepReceipt,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeMergeError<WriteTxnError, A>>,
    > {
        self.execute_graph_edge_merge_output_governed(database, cx, merge, policy, |row| {
            allocate(fgdb_gql::insertion::GraphInsertRequest::Edge { row, edge: 0 })
        })
    }

    fn execute_graph_edge_merge_output_governed<Output: EdgeMergeOutput, V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphEdgeMerge,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        allocate: impl FnMut(usize) -> Result<ElementId, A>,
    ) -> Result<
        (fgdb_gql::GraphEdgeMergeStats, Output),
        TxnGqlError<fgdb_gql::GraphEdgeMergeError<WriteTxnError, A>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeMergeError};
        let infrastructure = |error| GqlQueryError::Source(GraphEdgeMergeError::Source(error));

        self.ensure_database(database).map_err(infrastructure)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(infrastructure)?;
        if live != self.basis {
            return Err(infrastructure(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self
                .staged
                .iter()
                .all(|batch| batch.relation == first.relation)
            && merge.relation() != first.relation
        {
            return Err(infrastructure(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: merge.relation(),
            }));
        }

        cx.with_restriction(|| {
            let proposal = collect_edge_merge_output(
                merge,
                policy,
                |pattern, allowance| {
                    self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                |_, _| self.edges(database).map_err(GqlQueryError::Source),
                allocate,
                || cx.checkpoint(),
            )?;
            if let Some(batch) = proposal.creation {
                // The collector's final checkpoint already preceded staging.
                // Preserve the native adapter's atomic write and error carrier.
                self.write(database, batch).map_err(infrastructure)?;
            }
            Ok((proposal.stats, proposal.outcome))
        })
    }
}
