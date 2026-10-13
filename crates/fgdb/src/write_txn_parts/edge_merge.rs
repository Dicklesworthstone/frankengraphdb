// One directed relationship MERGE collector for native and authorized hosts.
// Source adapters own visibility and conflict witnesses, never uniqueness,
// creation semantics, cumulative budgets or a second commit mechanism.

struct EdgeMergeProposal<Output = fgdb_gql::GraphEdgeMergeOutcome> {
    stats: fgdb_gql::GraphEdgeMergeStats,
    outcome: Output,
    creation: Option<WriteBatch>,
    /// The frozen MATCH rows, in selection order, for a RETURN owner.
    selected: Vec<fgdb_gql::algebra::GraphValueRow>,
}

/// One edge an endpoint pair resolved to: the pair it answers, and the
/// edge's own source and destination (an undirected match may be reversed).
#[derive(Clone, Copy)]
struct ResolvedEdge {
    pair: (VId, VId),
    endpoints: (VId, VId),
}

// The singular primitive and the native program differ only in admitted
// cardinality and receipt ownership. They share selection, existence decisions,
// creation admission and staging. Singular calls allocate no new receipt vector.
trait EdgeMergeOutput: Sized {
    /// Whether one pair may match several parallel edges (fgdb-at06q). The
    /// singular primitive refuses them as ambiguous; a program keeps each.
    const PARALLEL: bool;
    fn for_pairs(pairs: usize) -> Option<Self>;
    fn record<C>(
        &mut self,
        outcome: fgdb_gql::GraphEdgeMergeOutcome,
        resolved: ResolvedEdge,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C>;
    fn result_rows(&self) -> u64;
}

/// Each endpoint pair's resolved edges, with each edge's own endpoints.
type ResolvedPairEdges = std::collections::BTreeMap<(VId, VId), Vec<(EId, (VId, VId))>>;

/// The RETURN owner's resolution (fgdb-at06q): every edge each endpoint pair
/// resolved to, in canonical pair and identity order, with its endpoints.
#[derive(Default)]
struct EdgeMergeRows {
    edges: ResolvedPairEdges,
    resolved: u64,
}

/// The shared collector's result for one output kind, with the frozen MATCH
/// rows a RETURN owner projects.
type EdgeMergeOutputResult<Output, A> = Result<
    (
        fgdb_gql::GraphEdgeMergeStats,
        Output,
        Vec<fgdb_gql::algebra::GraphValueRow>,
    ),
    TxnGqlError<fgdb_gql::GraphEdgeMergeError<WriteTxnError, A>>,
>;

impl EdgeMergeOutput for EdgeMergeRows {
    const PARALLEL: bool = true;

    fn for_pairs(_pairs: usize) -> Option<Self> {
        Some(Self::default())
    }

    fn record<C>(
        &mut self,
        outcome: fgdb_gql::GraphEdgeMergeOutcome,
        resolved: ResolvedEdge,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        let Some(edge) = outcome.edge() else {
            return Ok(());
        };
        control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
        self.edges
            .entry(resolved.pair)
            .or_default()
            .push((edge, resolved.endpoints));
        self.resolved += 1;
        Ok(())
    }

    fn result_rows(&self) -> u64 {
        self.resolved
    }
}

impl EdgeMergeOutput for fgdb_gql::GraphEdgeMergeOutcome {
    const PARALLEL: bool = false;

    fn for_pairs(pairs: usize) -> Option<Self> {
        (pairs <= 1).then_some(Self::NoInput)
    }

    fn record<C>(
        &mut self,
        outcome: Self,
        _resolved: ResolvedEdge,
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
    const PARALLEL: bool = true;

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
        _resolved: ResolvedEdge,
        control: &mut impl FnMut(Option<fgdb_gql::GlaExecutionEvent>) -> Result<(), C>,
    ) -> Result<(), C> {
        match self {
            // One pair matching parallel edges (fgdb-at06q) reports every
            // match, so its singular receipt becomes the batch receipt.
            Self::EdgeMerge { outcome } if outcome.edge().is_some() => {
                let first = *outcome;
                control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
                control(Some(fgdb_gql::GlaExecutionEvent::ScratchEntry))?;
                let created = match first {
                    fgdb_gql::GraphEdgeMergeOutcome::Created(edge) => vec![edge],
                    _ => Vec::new(),
                };
                *self = Self::EdgeMergeBatch {
                    outcomes: vec![first, next],
                    created,
                };
            }
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

/// RETURN cells after relationship MERGE (fgdb-at06q): one row per selected
/// row and edge its endpoint pair resolved to, in selection then identity
/// order. `property` reads one final-state stored field of the edge or an
/// endpoint; a scoped host masks a hidden key there, before any storage
/// read. A stored List or Map is the query value it holds (fgdb-k3nub); one
/// beyond the query value bounds is a data exception, never truncated.
fn edge_merge_query_rows<E, C>(
    query: &fgdb_gql::PreparedGraphEdgeMergeQuery,
    selected: &[fgdb_gql::algebra::GraphValueRow],
    resolved: &EdgeMergeRows,
    mut checkpoint: impl FnMut() -> Result<(), C>,
    mut property: impl FnMut(
        ElementId,
        fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, E>,
) -> Result<Vec<Vec<fgdb_gql::algebra::GraphValue>>, fgdb_gql::GqlQueryError<E, C>> {
    use fgdb_gql::GqlQueryError;
    use fgdb_gql::GraphEdgeMergeBinding;
    use fgdb_gql::algebra::GraphValue;
    let mut read = |element, key| -> Result<GraphValue, GqlQueryError<E, C>> {
        match property(element, key).map_err(GqlQueryError::Source)? {
            None => Ok(GraphValue::Scalar(CanonicalScalar::Null)),
            Some(value) => GraphValue::from_property_value(&value).ok_or(GqlQueryError::Data(
                fgdb_gql::GraphIntegerError {
                    instruction: 0,
                    kind: fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
                },
            )),
        }
    };
    let (source_column, destination_column) = (
        query.merge().source_column(),
        query.merge().destination_column(),
    );
    let mut rows = Vec::new();
    for row in selected {
        let values = row.values();
        // The collector already refused NULL or non-vertex endpoints.
        let (Some(source), Some(destination)) = (
            values[source_column].as_vertex(),
            values[destination_column].as_vertex(),
        ) else {
            continue;
        };
        let Some(edges) = resolved.edges.get(&(source, destination)) else {
            continue;
        };
        for &(edge, (src, dst)) in edges {
            checkpoint().map_err(GqlQueryError::Interrupted)?;
            let endpoint = |start: bool| if start { src } else { dst };
            let mut cells = Vec::with_capacity(query.bindings().len());
            for binding in query.bindings() {
                cells.push(match *binding {
                    GraphEdgeMergeBinding::Input(column) => values[column].clone(),
                    GraphEdgeMergeBinding::Edge => GraphValue::Edge(edge),
                    GraphEdgeMergeBinding::Endpoint { start } => {
                        GraphValue::Vertex(endpoint(start))
                    }
                    GraphEdgeMergeBinding::Property(key) => read(ElementId::Edge(edge), key)?,
                    GraphEdgeMergeBinding::EndpointProperty { start, key } => {
                        read(ElementId::Vertex(endpoint(start)), key)?
                    }
                });
            }
            rows.push(cells);
        }
    }
    Ok(rows)
}

type EdgeMergeFault<E, A, C> = fgdb_gql::GqlQueryError<fgdb_gql::GraphEdgeMergeError<E, A>, C>;
type EdgeMergeSelection<E, A, C> =
    Result<fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>, EdgeMergeFault<E, A, C>>;

// Private trusted seam: all endpoint rows are selected and validated before
// any existence inspection. Each callback must retain ALL matching live EIds
// in its admitted domain and native absence witnesses, including on refusal.
// Canonical duplicate pairs collapse for plain MERGE; actions use the separate
// singular upsert owner until a per-occurrence action contract is implemented.
fn collect_edge_merge_output<Output: EdgeMergeOutput, E, A, C>(
    merge: &fgdb_gql::PreparedGraphEdgeMerge,
    policy: fgdb_gql::GraphEdgeMergePolicy,
    // The endpoint rows (`PreparedGraphEdgeMerge::select_governed` over the
    // host's own pattern adapter), under the MERGE's query allowance.
    select: impl FnOnce(fgdb_gql::GqlQueryPolicy) -> EdgeMergeSelection<E, A, C>,
    mut edge_source: impl FnMut(VId, VId) -> Result<Vec<EdgeRecord>, fgdb_gql::GqlQueryError<E, C>>,
    mut allocate: impl FnMut(usize) -> Result<ElementId, A>,
    mut checkpoint: impl FnMut() -> Result<(), C>,
) -> Result<EdgeMergeProposal<Output>, EdgeMergeFault<E, A, C>> {
    use fgdb_gql::{
        GlaExecutionEvent, GlaLimitDimension, GlaLimitExceeded, GqlBudgetDimension, GqlQueryError,
        GraphEdgeMergeError, GraphEdgeMergeOutcome, GraphEdgeMergeStats,
    };
    let selected = select(policy.query)?;
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
    let width = merge.input_column_types().len();
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

        // An edge matches when its relation and orientation agree and every
        // property of the MERGE map equals its stored value (fgdb-at06q).
        // Each map comparison is one charged step. A program keeps every
        // match in identity order; the singular primitive refuses a second.
        let mut matching = Vec::new();
        for edge in &edges {
            event(Some(GlaExecutionEvent::Work))?;
            if edge.entry.relation != merge.relation()
                || !merge
                    .accepts_orientation((source, destination), (edge.entry.src, edge.entry.dst))
            {
                continue;
            }
            for _ in merge.properties() {
                event(Some(GlaExecutionEvent::Work))?;
            }
            // MERGE map values are scalars, so a stored List or Map never
            // equals one: it reads as no scalar and the edge does not match.
            let stored = |key| {
                edge.props
                    .binary_search_by_key(&key, |(candidate, _)| *candidate)
                    .ok()
                    .and_then(|at| edge.props[at].1.as_scalar())
            };
            if !merge.matches_properties(stored) {
                continue;
            }
            if !Output::PARALLEL && !matching.is_empty() {
                return Err(GqlQueryError::Source(
                    GraphEdgeMergeError::AmbiguousRelationships { observed: 2 },
                ));
            }
            event(Some(GlaExecutionEvent::ScratchEntry))?;
            matching.push((edge.entry.eid, (edge.entry.src, edge.entry.dst)));
        }
        if !matching.is_empty() {
            matching.sort_unstable();
            for (edge, endpoints) in matching {
                outcome.record(
                    GraphEdgeMergeOutcome::Matched(edge),
                    ResolvedEdge {
                        pair: (source, destination),
                        endpoints,
                    },
                    &mut event,
                )?;
            }
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
        outcome.record(
            GraphEdgeMergeOutcome::Created(edge),
            ResolvedEdge {
                pair: (source, destination),
                endpoints: (source, destination),
            },
            &mut event,
        )?;
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
        selected: selected.value,
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
        self.execute_graph_edge_merge_output_governed(database, cx, merge, policy, None, |_| {
            allocate(fgdb_gql::GraphEdgeMergeRequest)
        })
        .map(|(stats, outcome, _)| (stats, outcome))
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
        self.execute_graph_edge_merges_over(database, cx, merge, policy, None, allocate)
    }

    /// [`Self::execute_graph_edge_merges_governed`] for the last write of a
    /// chain: `owned` are the previous write's rows, which the MERGE's row
    /// relation reads through its owned leaf (fgdb-cpny9).
    pub(crate) fn execute_graph_edge_merges_over<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphEdgeMerge,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        owned: Option<Vec<fgdb_gql::algebra::GraphValueRow>>,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeMergeStats,
            fgdb_gql::GraphWriteStepReceipt,
        ),
        TxnGqlError<fgdb_gql::GraphEdgeMergeError<WriteTxnError, A>>,
    > {
        self.execute_graph_edge_merge_output_governed(database, cx, merge, policy, owned, |row| {
            allocate(fgdb_gql::insertion::GraphInsertRequest::Edge { row, edge: 0 })
        })
        .map(|(stats, receipt, _)| (stats, receipt))
    }

    /// `MATCH ... MERGE ... RETURN` (fgdb-at06q). Every distinct endpoint pair
    /// resolves as in a native program: every matching edge is kept, and a
    /// pair without one creates exactly one. RETURN then sees one row per
    /// selected row and relationship its pair resolved to, in selection
    /// order and then identity order. A relationship's properties and its
    /// endpoints' properties are read after the write, through governed point
    /// reads that this transaction witnesses like any other read.
    ///
    /// MERGE and RETURN share one rollback boundary. A result expression,
    /// budget, cancellation or unwind failure restores the prior staged prefix,
    /// while retaining read witnesses and already-issued external identities.
    /// Successful rows remain transaction-local; this never commits the caller's
    /// transaction. LIMIT 0 suppresses output, not writes or RETURN failures.
    pub fn execute_graph_edge_merge_query_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeMergeQuery,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> EdgeMergeQueryResult<A> {
        self.execute_graph_edge_merge_query_over(database, cx, query, policy, None, allocate)
    }

    /// [`Self::execute_graph_edge_merge_query_governed`] for the last write
    /// of a chain: `owned` are the previous write's rows (fgdb-cpny9).
    pub(crate) fn execute_graph_edge_merge_query_over<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeMergeQuery,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        owned: Option<Vec<fgdb_gql::algebra::GraphValueRow>>,
        mut allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> EdgeMergeQueryResult<A> {
        use fgdb_gql::{GqlQueryError, GraphEdgeMergeError, GraphEdgeMergeQueryError};
        self.ensure_database(database).map_err(|error| {
            GqlQueryError::Source(GraphEdgeMergeQueryError::Merge(
                GraphEdgeMergeError::Source(error),
            ))
        })?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let (stats, resolved, selected) = workspace
                .txn
                .execute_graph_edge_merge_output_governed::<EdgeMergeRows, V, A>(
                    database,
                    cx,
                    query.merge(),
                    policy,
                    owned,
                    |row| allocate(fgdb_gql::insertion::GraphInsertRequest::Edge { row, edge: 0 }),
                )
                .map_err(|error| error.map_source(GraphEdgeMergeQueryError::Merge))?;
            let rows = edge_merge_query_rows(
                query,
                &selected,
                &resolved,
                || cx.checkpoint(),
                |element, key| workspace.txn.merge_return_property(database, element, key),
            )
            .map_err(|error| {
                error.map_source(|error| {
                    GraphEdgeMergeQueryError::Merge(GraphEdgeMergeError::Source(error))
                })
            })?;
            let returning = query
                .finish_governed(
                    rows,
                    stats.match_selection,
                    stats.evaluator,
                    policy.query,
                    || cx.checkpoint(),
                )
                .map_err(|error| error.map_source(GraphEdgeMergeQueryError::Returning))?;
            // Keep the guard through projection, aggregation, ordering and the
            // last governed checkpoint. No successful write prefix may escape
            // merely because the relationship itself was staged successfully.
            workspace.accept();
            Ok((stats, returning))
        })
    }

    /// Relationship MERGE RETURN with identities reserved by the owning
    /// database.
    pub fn execute_graph_edge_merge_query_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphEdgeMergeQuery,
        policy: fgdb_gql::GraphEdgeMergePolicy,
    ) -> EdgeMergeQueryResult<WriteTxnError> {
        let source = |error| {
            fgdb_gql::GqlQueryError::Source(fgdb_gql::GraphEdgeMergeQueryError::Merge(
                fgdb_gql::GraphEdgeMergeError::Source(error),
            ))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_edge_merge_query_governed(database, cx, query, policy, allocate)
    }

    /// One final-state stored property of a MERGE RETURN element, read
    /// through the governed point projection and witnessed. A missing element
    /// or field is None.
    fn merge_return_property<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        element: ElementId,
        key: fgdb_delta_types::PropertyKeyId,
    ) -> Result<Option<CanonicalPropertyValue>, WriteTxnError> {
        let field = PointReadField::Property(key);
        let projected = self.point_projection_with_control(
            database,
            element,
            field,
            &mut |_| Ok::<(), WriteTxnError>(()),
            &|error| error,
        )?;
        let value = projected.property.cloned();
        self.point_reads.borrow_mut().record(element, field);
        Ok(value)
    }

    /// `owned` is a chained write's input rows (the previous write's), read
    /// by the MERGE's row relation; `None` for every other statement.
    fn execute_graph_edge_merge_output_governed<Output: EdgeMergeOutput, V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        merge: &fgdb_gql::PreparedGraphEdgeMerge,
        policy: fgdb_gql::GraphEdgeMergePolicy,
        owned: Option<Vec<fgdb_gql::algebra::GraphValueRow>>,
        allocate: impl FnMut(usize) -> Result<ElementId, A>,
    ) -> EdgeMergeOutputResult<Output, A> {
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
                |allowance| {
                    merge.select_governed(
                        owned,
                        allowance,
                        |pattern, allowance| {
                            self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                        },
                        || cx.checkpoint(),
                    )
                },
                |source, destination| {
                    self.merge_candidate_edges(database, merge, source, destination, || {
                        cx.checkpoint()
                    })
                },
                allocate,
                || cx.checkpoint(),
            )?;
            if let Some(batch) = proposal.creation {
                // The collector's final checkpoint already preceded staging.
                // Preserve the native adapter's atomic write and error carrier.
                self.write(database, batch).map_err(infrastructure)?;
            }
            Ok((proposal.stats, proposal.outcome, proposal.selected))
        })
    }
}

#[cfg(test)]
mod edge_merge_return_atomic_tests {
    use super::*;
    use crate::DatabaseKeys;
    use crate::query::{QueryResult, QueryWriteError};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergeQueryError, GraphSymbol,
        GraphSymbolKind, GraphWriteIdentityRequest, GraphWriteProgramPolicy,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn failed_merge_return_restores_prefix_and_success_can_still_commit() {
        let ((), report) = run_async_under_lab(0xe6e0_0001, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let keys = DatabaseKeys::new(
                [0x31; 32],
                DatabaseSecurityNamespaceId([0x32; 32]),
                [0x33; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let before = db.frontier().unwrap();
            let relation = RelationId(1);
            let property = PropertyKeyId(1);
            let symbols = |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(relation)),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(property)),
                _ => None,
            };
            let policy = GraphWriteProgramPolicy::new(
                GqlQueryPolicy::new(1_000, 100, 5_000_000, 2_000_000),
                1_000,
                0,
                1,
            );
            let mut transaction = db.begin(&txcx).unwrap();
            let mut prefix = WriteBatch::new(relation);
            prefix.create_vertex(VId(1), vec![], vec![(property, CanonicalScalar::Int(1))]);
            prefix.create_vertex(VId(2), vec![], vec![(property, CanonicalScalar::Int(2))]);
            transaction.write(&mut db, prefix).unwrap();
            let digest = transaction.staged_effect_digest().unwrap();
            let next = AtomicU64::new(100);
            let mut allocate = |request: GraphWriteIdentityRequest| -> Result<ElementId, ()> {
                assert_eq!(request.statement, 0);
                assert!(matches!(
                    request.request,
                    fgdb_gql::insertion::GraphInsertRequest::Edge { .. }
                ));
                Ok(ElementId::Edge(EId(u128::from(
                    next.fetch_add(1, Ordering::Relaxed),
                ))))
            };
            for suffix in ["", " LIMIT 0"] {
                let text = format!(
                    "MATCH (a),(b) WHERE a.p=1 AND b.p=2 \
                     MERGE (a)-[r:R {{p:0}}]->(b) RETURN 10/r.p AS quotient{suffix}"
                );
                let result = transaction.query_write(
                    &mut db,
                    &cx,
                    &text,
                    &GqlParameters::new(),
                    &symbols,
                    relation,
                    policy,
                    &mut allocate,
                );
                assert!(matches!(
                    result,
                    Err(QueryWriteError::EdgeMerge(GqlQueryError::Source(
                        GraphEdgeMergeQueryError::Returning(_)
                    )))
                ));
                assert_eq!(transaction.staged_effect_digest().unwrap(), digest);
                assert!(transaction.edges(&db).unwrap().is_empty());
                assert_eq!(transaction.vertices(&db).unwrap().len(), 2);
                assert_eq!(db.frontier().unwrap(), before);
            }
            assert_eq!(next.load(Ordering::Relaxed), 102);
            let result = transaction
                .query_write(
                    &mut db,
                    &cx,
                    "MATCH (a),(b) WHERE a.p=1 AND b.p=2 \
                     MERGE (a)-[r:R {p:7}]->(b) RETURN r,r.p AS p",
                    &GqlParameters::new(),
                    &symbols,
                    relation,
                    policy,
                    &mut allocate,
                )
                .unwrap();
            assert!(matches!(result, QueryResult::Rows { rows, .. } if rows.len() == 1));
            assert_eq!(next.load(Ordering::Relaxed), 103);
            assert!(db.edges().unwrap().is_empty());
            transaction.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert!(db.edge(EId(100)).unwrap().is_none());
            assert!(db.edge(EId(101)).unwrap().is_none());
            assert_eq!(
                db.edge(EId(102)).unwrap().unwrap().props,
                vec![(property, CanonicalScalar::Int(7).into())]
            );
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
