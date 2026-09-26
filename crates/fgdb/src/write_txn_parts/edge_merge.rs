// One directed relationship MERGE collector for native and authorized hosts.
// Source adapters own visibility and conflict witnesses, never uniqueness,
// creation semantics, cumulative budgets or a second commit mechanism.

struct EdgeMergeProposal {
    stats: fgdb_gql::GraphEdgeMergeStats,
    outcome: fgdb_gql::GraphEdgeMergeOutcome,
    creation: Option<WriteBatch>,
}

type EdgeMergeFault<E, A, C> =
    fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeMergeError<E, A>, C>;
type EdgeMergeSelection<E, C> = Result<
    fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
    fgdb_gql::GqlQueryError<E, C>,
>;

// Private trusted seam: edge_source is called only after one distinct non-null
// endpoint pair was selected. It must retain ALL matching live EIds in its
// admitted domain and native absence witnesses; it cannot normalize parallels.
fn collect_edge_merge<E, A, C>(
    merge: &fgdb_gql::PreparedGraphEdgeMerge,
    policy: fgdb_gql::GraphEdgeMergePolicy,
    source: impl FnOnce(
        &fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryPolicy,
    ) -> EdgeMergeSelection<E, C>,
    edge_source: impl FnOnce(VId, VId) -> Result<Vec<EdgeRecord>, fgdb_gql::GqlQueryError<E, C>>,
    mut allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<EdgeMergeProposal, EdgeMergeFault<E, A, C>> {
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension,
        GqlQueryError, GraphEdgeMergeError, GraphEdgeMergeOutcome, GraphEdgeMergeRequest,
        GraphEdgeMergeStats,
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
    if pairs.len() > 1 {
        return Err(GqlQueryError::Source(
            GraphEdgeMergeError::AmbiguousEndpointPairs {
                observed: pairs.len() as u64,
            },
        ));
    }
    let Some((source, destination)) = pairs.into_iter().next() else {
        return Ok(EdgeMergeProposal {
            stats: GraphEdgeMergeStats {
                match_selection: selected.rows,
                overlay_edges: 0,
                evaluator,
                created_edges: 0,
            },
            outcome: GraphEdgeMergeOutcome::NoInput,
            creation: None,
        });
    };

    // The host supplies its canonical edge domain and retains the native
    // negative/topology witness. A scoped host admits candidates BEFORE
    // they reach these counters or the uniqueness reducer.
    let edges = edge_source(source, destination)
        .map_err(|error| error.map_source(GraphEdgeMergeError::Source))?;
    let overlay_edges = u64::try_from(edges.len()).expect("in-memory edge count fits u64");
    let cumulative_records = selected
        .rows
        .snapshot_records
        .checked_add(overlay_edges)
        .expect("two in-memory source counts fit u64");
    policy
        .query
        .rows
        .check(GqlBudgetDimension::SnapshotRecords, cumulative_records)
        .map_err(GqlQueryError::Rows)?;

    let mut matching = Vec::new();
    for edge in &edges {
        event(Some(GlaExecutionEvent::Work))?;
        if edge.entry.relation == merge.relation()
            && edge.entry.src == source
            && edge.entry.dst == destination
        {
            if matching.is_empty() {
                event(Some(GlaExecutionEvent::ScratchEntry))?;
            }
            matching.push(edge.entry.eid);
            if matching.len() > 1 {
                return Err(GqlQueryError::Source(
                    GraphEdgeMergeError::AmbiguousRelationships {
                        observed: matching.len() as u64,
                    },
                ));
            }
        }
    }
    if let Some(edge) = matching.first().copied() {
        return Ok(EdgeMergeProposal {
            stats: GraphEdgeMergeStats {
                match_selection: selected.rows,
                overlay_edges,
                evaluator,
                created_edges: 0,
            },
            outcome: GraphEdgeMergeOutcome::Matched(edge),
            creation: None,
        });
    }

    // Match and NoInput do not spend creation quota. A missing edge does:
    // refuse before payload cloning or any external identity is issued.
    if policy.max_created_edges == 0 {
        return Err(GqlQueryError::Source(GraphEdgeMergeError::CreationLimit {
            limit: policy.max_created_edges,
            observed: 1,
        }));
    }

    // Freeze creation payload before allocation. The prepared definition
    // already admitted scalar sizes/types; reserve one logical entry per
    // owned property copy under the same evaluator allowance.
    let mut properties = Vec::with_capacity(merge.properties().len());
    for (key, value) in merge.properties() {
        event(Some(GlaExecutionEvent::ScratchEntry))?;
        properties.push((*key, value.value().clone()));
    }
    event(None)?;
    let identity = allocate(GraphEdgeMergeRequest).map_err(|error| {
        GqlQueryError::Source(GraphEdgeMergeError::IdentitySource(error))
    })?;
    let ElementId::Edge(edge) = identity else {
        return Err(GqlQueryError::Source(GraphEdgeMergeError::IdentityKind));
    };

    let mut batch = WriteBatch::new(merge.relation());
    batch.add_edge(edge, source, destination, properties);
    // Last cancellable boundary before synchronous atomic staging. Never
    // report cancellation after the workspace changes.
    event(None)?;
    Ok(EdgeMergeProposal {
        stats: GraphEdgeMergeStats {
            match_selection: selected.rows,
            overlay_edges,
            evaluator,
            created_edges: 1,
        },
        outcome: GraphEdgeMergeOutcome::Created(edge),
        creation: Some(batch),
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
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeMergeStats,
            fgdb_gql::GraphEdgeMergeOutcome,
        ),
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
            let proposal = collect_edge_merge(
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
