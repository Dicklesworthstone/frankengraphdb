// Query-selected writes share the existing canonical staged overlay and the
// ordinary WriteTxn::write rollback/preparation boundary. No alternate writer.

impl WriteTxn {
    /// Stage one simultaneous query-selected vertex mutation. This does NOT
    /// commit: the caller still explicitly commits or aborts this transaction.
    /// Mutable transaction ownership is required; a pinned read view cannot
    /// call this API. The QueryCx governs matching and proposal cancellation.
    ///
    /// Every target and RHS value is frozen against the original staged overlay
    /// before any new intention is appended. Identical assignments collapse;
    /// conflicting assignments refuse. A failed selection, proposal, checkpoint
    /// or ordinary preparation leaves all prior staged effects intact. Read and
    /// scan observations already made remain in conflict validation on failure.
    ///
    /// An empty proposal leaves the workspace unchanged, without inventing an
    /// empty commit or changing this operation into a read-only API. Counters
    /// cover selection and proposals, not storage preparation or cascade costs.
    /// Existing same-relation/atomic-group staging restrictions still apply.
    pub fn execute_graph_mutation_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        mutation: &fgdb_gql::PreparedGraphMutation,
        policy: fgdb_gql::GraphMutationPolicy,
    ) -> Result<
        fgdb_gql::GraphMutationStats,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphMutationError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphMutationError, GraphMutationIntent, GqlQueryError};
        let source = |error| GqlQueryError::Source(GraphMutationError::Source(error));
        // Even an empty/zero-budget selection cannot bypass ownership, health,
        // basis or the existing relation-coordinate contract.
        self.ensure_database(database).map_err(source)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && mutation.relation() != first.relation
        {
            return Err(source(WriteTxnError::RelationMismatch {
                expected: first.relation, found: mutation.relation(),
            }));
        }
        cx.with_restriction(|| {
            let proposal = mutation.execute_governed(
                policy,
                |pattern, budget| self.execute_graph_pattern_governed(database, cx, pattern, budget),
                || cx.checkpoint(),
            )?;
            let stats = proposal.stats();
            let mut batch = WriteBatch::new(mutation.relation());
            for intent in proposal.into_intents() {
                // Only a private batch is changing here. Cancellation drops it
                // whole and cannot expose a partially appended statement.
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                match intent {
                    GraphMutationIntent::Property { vertex, key, value } => {
                        batch.set_vertex_property(vertex, key, value);
                    }
                    GraphMutationIntent::Label { vertex, label, present } => {
                        batch.set_vertex_label(vertex, label, present);
                    }
                    GraphMutationIntent::DetachDelete { vertex } => {
                        batch.delete_vertex(vertex);
                    }
                }
            }
            // Last cancellable boundary before synchronous atomic staging.
            // Never report a new interruption after the workspace has changed.
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            if !batch.is_empty() { self.write(database, batch).map_err(source)?; }
            Ok(stats)
        })
    }
}
