// Query-selected graph creation stages one ordinary WriteBatch. Selection and
// computed payloads are frozen before allocation; commit remains explicit.

impl WriteTxn {
    /// INSERT/CREATE with identities reserved by the owning database.
    pub fn execute_graph_insert_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        fgdb_gql::insertion::GraphInsertStats,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, WriteTxnError>>,
    > {
        let source = |error| {
            fgdb_gql::GqlQueryError::Source(fgdb_gql::insertion::GraphInsertError::Source(error))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_insert_governed(database, cx, insertion, policy, allocate)
    }

    /// INSERT/CREATE with an engine-issued identity receipt; publication remains explicit.
    pub fn execute_graph_insert_returning_engine_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, WriteTxnError>>,
    > {
        let source = |error| {
            fgdb_gql::GqlQueryError::Source(fgdb_gql::insertion::GraphInsertError::Source(error))
        };
        self.ensure_database(database).map_err(source)?;
        let allocate = database.engine_allocator(cx).map_err(source)?;
        self.execute_graph_insert_returning_governed(database, cx, insertion, policy, allocate)
    }

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
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        self.execute_graph_insert_governed_inner(database, cx, insertion, policy, allocate, false)
            .map(|(stats, _, _)| stats)
    }

    /// Stage the same insertion while retaining the exact identities accepted
    /// into the workspace. Vertex IDs are ordered by selected occurrence then
    /// vertex declaration; edge IDs use occurrence then edge declaration. The
    /// fixed per-occurrence widths are `insertion.vertices_per_row()` and
    /// `insertion.edges_per_row()`, so callers can recover the row-local mapping
    /// without graph reads or allocator-side bookkeeping.
    ///
    /// These vectors are a staged-operation receipt, NOT proof of durability:
    /// only a later successful transaction finish/commit publishes them. The
    /// identity vectors are bounded by `max_vertices`/`max_edges`; they do not
    /// add an unbounded result surface outside the insertion policy. Failure
    /// returns no receipt even when the external allocator already issued IDs.
    pub fn execute_graph_insert_returning_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        self.execute_graph_insert_governed_inner(database, cx, insertion, policy, allocate, true)
    }

    fn execute_graph_insert_governed_inner<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        insertion: &fgdb_gql::insertion::PreparedGraphInsert,
        policy: fgdb_gql::insertion::GraphInsertPolicy,
        allocate: impl FnMut(fgdb_gql::insertion::GraphInsertRequest) -> Result<ElementId, A>,
        retain_identities: bool,
    ) -> Result<
        WithAffectedIds<fgdb_gql::insertion::GraphInsertStats>,
        TxnGqlError<fgdb_gql::insertion::GraphInsertError<WriteTxnError, A>>,
    > {
        use fgdb_gql::GqlQueryError;
        use fgdb_gql::insertion::{GraphInsertError, GraphInsertIntent};
        let source = |error| GqlQueryError::Source(GraphInsertError::Source(error));
        // Never allocate even one ID before ownership, health, basis and
        // relation-coordinate checks, including for empty or zero-budget input.
        self.ensure_database(database).map_err(source)?;
        let live = database
            .frontier()
            .map_err(WriteTxnError::from)
            .map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced {
                pinned: self.basis,
                live,
            }));
        }
        cx.with_restriction(|| {
            let proposal = insertion.execute_governed(
                policy,
                |pattern, allowance| {
                    self.execute_graph_pattern_governed(database, cx, pattern, allowance)
                },
                allocate,
                || cx.checkpoint(),
            )?;
            let stats = proposal.stats();
            // Preserve the preexisting stats-only physical path: identity result
            // vectors exist only for the explicit returning API.
            let mut vertices = retain_identities.then(Vec::new);
            let mut edges = retain_identities.then(Vec::new);
            // Vertex declarations must lead the shared initializer prefix.
            // Edge groups retain declaration order within each relation.
            let mut batch = WriteBatch::new(insertion.relation());
            let mut groups = std::collections::BTreeMap::new();
            for intent in proposal.into_intents() {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                match intent {
                    GraphInsertIntent::Vertex {
                        vertex,
                        labels,
                        properties,
                    } => {
                        if let Some(vertices) = &mut vertices {
                            vertices.push(vertex);
                        }
                        batch.create_vertex(vertex, labels, properties);
                    }
                    GraphInsertIntent::Edge {
                        edge,
                        relation,
                        source,
                        destination,
                        properties,
                    } => {
                        if let Some(edges) = &mut edges {
                            edges.push(edge);
                        }
                        groups
                            .entry(relation)
                            .or_insert_with(|| WriteBatch::new(relation))
                            .add_edge(edge, source, destination, properties);
                    }
                }
            }
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            let mut batches = Vec::with_capacity(groups.len() + usize::from(!batch.is_empty()));
            if !batch.is_empty() {
                batches.push(batch);
            }
            batches.extend(groups.into_values());
            if let Some(first) = batches.first() {
                let relation = first.relation;
                if batches.iter().all(|batch| batch.relation == relation)
                    && self.staged.iter().all(|batch| batch.relation == relation)
                {
                    let mut combined = batches.remove(0);
                    for batch in batches {
                        combined.rows.extend(batch.rows);
                    }
                    self.write(database, combined).map_err(source)?;
                } else {
                    self.write_atomic(database, batches).map_err(source)?;
                }
            }
            Ok((
                stats,
                vertices.unwrap_or_default(),
                edges.unwrap_or_default(),
            ))
        })
    }
}
