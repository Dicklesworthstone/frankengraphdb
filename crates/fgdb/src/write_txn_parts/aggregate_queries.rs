// Included inside query_source, so aggregates use its private canonical source
// directly. No visibility widening or second transaction overlay is required.

impl WriteTxn {
    /// Summarize complete matches over the pinned basis plus canonical staged
    /// effects. Observed rows and insertion witnesses survive later arithmetic,
    /// output-budget or cancellation failures; wrong owners never admit data.
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
            let result = aggregate.execute_governed(
                source.snapshot_records as u64,
                source.vertex_ids(),
                source.edge_triples(),
                |vid, predicates| Ok::<_, WriteTxnError>(source.matches(vid, predicates)),
                |vid, key| Ok(source.property(vid, key)),
                usage.remaining(policy),
                || cx.checkpoint(),
            );
            usage.finish(policy, result)
        })
    }
}
