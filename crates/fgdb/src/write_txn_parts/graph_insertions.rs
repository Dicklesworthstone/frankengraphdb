// Query-selected graph creation stages one ordinary WriteBatch. Selection and
// computed payloads are frozen before allocation; commit remains explicit.

impl WriteTxn {
    /// Create graph structures once per selected occurrence in this workspace.
    /// Matched endpoints and property reads use the canonical staged overlay.
    /// Newly created vertices are available as endpoints within their own row.
    ///
    /// The allocator supplies fresh typed IDs under the caller's identity
    /// policy. This API never rolls back an allocator or recycles issued IDs;
    /// retain its request-to-ID mapping for deterministic retries/replay. All
    /// database writes still pass ordinary creation/non-revival validation.
    ///
    /// Matching, property, identity, resource and preparation failures leave
    /// earlier staged work intact. Observed read/scan dependencies remain even
    /// on failure. Success only stages; finish/commit publishes later. Counts
    /// describe proposed creations, not a durable acknowledgment. No cancellation
    /// checkpoint follows the synchronous atomic staging boundary.
    pub fn execute_graph_insert_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::insertion::GraphInsertStats,
        fgdb_gql::GqlQueryError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertIntent};
        use fgdb_gql::GqlQueryError;
        let source = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        // Never allocate even one ID before ownership, health, basis and
        // relation-coordinate checks, including for empty or zero-budget input.
        self.ensure_database(database).map_err(source)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && insertion.relation() != first.relation
        {
            return Err(source(WriteTxnError::RelationMismatch {
                expected: first.relation, found: insertion.relation(),
            }));
        }
        cx.with_restriction(|| {
            let proposal = insertion.execute_governed(
                policy,
                |pattern, allowance| self.execute_graph_pattern_governed(database, cx, pattern, allowance),
                allocate,
                || cx.checkpoint(),
            )?;
            let stats = proposal.stats();
            let mut batch = WriteBatch::new(insertion.relation());
            for intent in proposal.into_intents() {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                match intent {
                    GraphInsertIntent::Vertex { vertex, labels, properties } => {
                        batch.create_vertex(vertex, labels, properties);
                    }
                    GraphInsertIntent::Edge { edge, source, destination, properties } => {
                        batch.add_edge(edge, source, destination, properties);
                    }
                }
            }
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            if !batch.is_empty() { self.write(database, batch).map_err(source)?; }
            Ok(stats)
        })
    }
}
