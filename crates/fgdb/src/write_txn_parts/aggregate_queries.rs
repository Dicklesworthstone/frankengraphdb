// Included inside query_source, so aggregates and sets use its private canonical
// source directly. No visibility widening or second transaction overlay is required.

impl WriteTxn {
    /// Summarize complete matches over the pinned basis plus canonical staged
    /// effects. Observed rows and insertion witnesses survive later arithmetic,
    /// output-budget or cancellation failures; wrong owners never admit data.
    /// Captured paths preserve canonical overlay EIds through grouping, even
    /// when the final result contains only a count or numeric summary.
    pub fn execute_graph_aggregate_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        aggregate: &fgdb_gql::PreparedGraphAggregate,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::GraphAggregateRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphAggregateError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphAggregateError, GqlQueryError};
        cx.with_restriction(|| {
            let snapshot = self.query_snapshot(database)
                .map_err(|error| GqlQueryError::Source(GraphAggregateError::Source(error)))?;
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let mut usage = crate::gql_exec::AdmissionUsage::default();
            let pattern = aggregate.input_pattern();
            let source = self.query_source_over_logical(
                snapshot,
                pattern.plan().clone(),
                pattern.required_vertex_label(),
                &mut |event| {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(policy, event)
                },
            )?;
            let result = aggregate.execute_governed_with_element_properties(
                source.snapshot_records as u64,
                source.vertex_ids(),
                source.identified_edges(),
                |vid, predicates| Ok::<_, WriteTxnError>(source.matches(vid, predicates)),
                |vid, key| Ok(source.property(vid, key)),
                |eid, key| Ok(source.edge_property(eid, key)),
                usage.remaining(policy),
                || cx.checkpoint(),
            );
            usage.finish(policy, result)
        })
    }

    /// Evaluate every operand against this same basis and canonical staged
    /// template. The shared borrow prevents staging changes between operands.
    /// Both positive and negative domains use the ordinary overlay read/scan
    /// tracking, even for an empty set or LIMIT 0. A later set-stage refusal
    /// never erases observations already retained by an operand.
    pub fn execute_graph_set_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphSet,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::algebra::GraphValueRow>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphSetExecutionError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphSetExecutionError, GqlQueryError};
        cx.with_restriction(|| {
            // Wrong ownership/health/frontier is a source error even when a
            // zero query allowance would otherwise reject before the first leaf.
            let _ = self.query_snapshot(database)
                .map_err(|error| GqlQueryError::Source(GraphSetExecutionError::Source(error)))?;
            query.execute_governed(
                policy,
                |pattern, allowance| self.execute_graph_pattern_governed(database, cx, pattern, allowance),
                || cx.checkpoint(),
            )
        })
    }

    /// Find one exact cheapest bounded WALK over this transaction's basis and
    /// canonical staged effects. Costs are borrowed from the same overlay as
    /// topology, including property edits and cascaded edge retirement.
    /// Ordinary point/scan witnesses survive cost, output or budget refusal;
    /// minimizing the answer must not minimize the transaction's dependencies.
    pub fn execute_graph_cheapest_path_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphCheapestPath,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::GraphCostPath>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphCheapestPathError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        self.execute_cheapest_paths_over(database, cx, query, None, policy)
    }

    /// Return a ranked prefix over the basis plus canonical staged effects.
    /// Source point/scan dependencies survive a shorter K, zero K, an empty
    /// answer, cost refusal or later output refusal. No extra query can repair
    /// a lost witness; the ordinary canonical source records it once here.
    pub fn execute_graph_cheapest_paths_governed<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphCheapestPath,
        count: u64,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::GraphCostPath>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphCheapestPathError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        self.execute_cheapest_paths_over(database, cx, query, Some(count), policy)
    }

    // The source admission and conflict witnesses are shared by the existing
    // single-answer path and the lazy ranked specialization.
    fn execute_cheapest_paths_over<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphCheapestPath,
        count: Option<u64>,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GqlQueryExecution<fgdb_gql::GraphCostPath>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphCheapestPathError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphCheapestPathError, GqlQueryError};
        cx.with_restriction(|| {
            let snapshot = self.query_snapshot(database)
                .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(error)))?;
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let mut usage = crate::gql_exec::AdmissionUsage::default();
            let pattern = query.input_pattern();
            let source = self.query_source_over_logical(
                snapshot,
                pattern.plan().clone(),
                pattern.required_vertex_label(),
                &mut |event| {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(policy, event)
                },
            )?;
            let result = match count {
                Some(count) => query.execute_k_governed_with_edge_properties(
                    count,
                    source.snapshot_records as u64,
                    source.vertex_ids(),
                    source.identified_edges(),
                    |eid, key| Ok::<_, WriteTxnError>(source.edge_property(eid, key)),
                    usage.remaining(policy),
                    || cx.checkpoint(),
                ),
                None => query.execute_governed_with_edge_properties(
                    source.snapshot_records as u64,
                    source.vertex_ids(),
                    source.identified_edges(),
                    |eid, key| Ok::<_, WriteTxnError>(source.edge_property(eid, key)),
                    usage.remaining(policy),
                    || cx.checkpoint(),
                ),
            };
            usage.finish(policy, result)
        })
    }
}

include!("weighted_stream.rs");
