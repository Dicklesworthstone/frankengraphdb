// Included inside the canonical query_source module, just like aggregates.
// All transaction observations are retained before the owned stream escapes.

impl WriteTxn {
    /// Open a ranked result stream over this statement's basis plus canonical
    /// staged image. Source/cost admission and conflict observation finish at
    /// open; not polling, closing early or later refusal cannot erase them.
    /// Subsequent staging or transaction completion does not rewrite the owned
    /// image. Later pulls derive rows from that image without reading the txn.
    /// This is not transaction ownership transfer or a durable overlay cursor.
    pub fn stream_graph_cheapest_paths_governed<'cx, V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &'cx fgdb_types::QueryCx,
        query: &fgdb_gql::PreparedGraphCheapestPath,
        count: u64,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GraphCheapestPathStreamIterator<'cx, Box<asupersync::error::Error>>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphCheapestPathError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphCheapestPathError, GqlQueryError};
        let stream = cx.with_restriction(|| {
            let snapshot = self.query_snapshot(database)
                .map_err(|error| GqlQueryError::Source(GraphCheapestPathError::Source(error)))?;
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let mut usage = crate::gql_exec::AdmissionUsage::default();
            let pattern = query.input_pattern();
            let source = self.query_source_over_logical(
                snapshot, pattern.plan().clone(), pattern.required_vertex_label(),
                &mut |event| {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    usage.observe(policy, event)
                },
            )?;
            query.stream_governed_with_admission(
                count, source.snapshot_records as u64, source.vertex_ids(), source.identified_edges(),
                |eid, key| Ok::<_, WriteTxnError>(source.edge_property(eid, key)),
                usage.path_stream_usage(), policy, || cx.checkpoint(),
            )
        })?;
        Ok(stream.into_scoped_iterator(self.basis, move |stream| {
            cx.with_restriction(|| stream.next_with_checkpoint(|| cx.checkpoint()))
        }))
    }

    /// Both text selectors use the ranked stream; ANY is its K=1 prefix.
    /// Bind has already resolved aliases/arguments. No mutable catalog or AST
    /// reaches this execution path, and zero K still observes the source.
    pub fn stream_graph_cheapest_path_text_governed<'cx, V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        cx: &'cx fgdb_types::QueryCx,
        request: &fgdb_gql::BoundGraphCheapestPathQuery,
        policy: fgdb_gql::GqlQueryPolicy,
    ) -> Result<
        fgdb_gql::GraphCheapestPathStreamIterator<'cx, Box<asupersync::error::Error>>,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphCheapestPathError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        self.stream_graph_cheapest_paths_governed(database, cx, request.query(), request.ranked_count().unwrap_or(1), policy)
    }
}
