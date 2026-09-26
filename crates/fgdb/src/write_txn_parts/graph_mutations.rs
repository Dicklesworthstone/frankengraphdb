// Query-selected writes share the existing canonical staged overlay and the
// ordinary WriteTxn::write rollback/preparation boundary. No alternate writer.

impl WriteTxn {
    /// Stage one simultaneous query-selected element mutation. This does NOT
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
        TxnGqlError<fgdb_gql::GraphMutationError<WriteTxnError>>,
    > {
        self.execute_graph_mutation_governed_inner(database, cx, mutation, policy, false)
            .map(|(stats, _, _)| stats)
    }

    /// Stage the same simultaneous mutation and return distinct vertex and edge
    /// IDs whose canonical proposal contains at least one intent. Each domain
    /// is sorted independently and appears once even when several fields change
    /// or many MATCH occurrences collapse to the same assignment.
    ///
    /// The returned IDs describe the accepted STAGED statement, not durable
    /// state and not necessarily changed storage rows: an equal-to-current SET
    /// can later normalize away during ordinary write preparation. The target
    /// vector is bounded by `max_effects` and is returned only after synchronous
    /// staging succeeds. Failure exposes no partial target receipt.
    pub fn execute_graph_mutation_returning_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        mutation: &fgdb_gql::PreparedGraphMutation,
        policy: fgdb_gql::GraphMutationPolicy,
    ) -> Result<
        WithAffectedIds<fgdb_gql::GraphMutationStats>,
        TxnGqlError<fgdb_gql::GraphMutationError<WriteTxnError>>,
    > {
        self.execute_graph_mutation_governed_inner(database, cx, mutation, policy, true)
    }

    fn execute_graph_mutation_governed_inner<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        mutation: &fgdb_gql::PreparedGraphMutation,
        policy: fgdb_gql::GraphMutationPolicy,
        retain_targets: bool,
    ) -> Result<
        WithAffectedIds<fgdb_gql::GraphMutationStats>,
        TxnGqlError<fgdb_gql::GraphMutationError<WriteTxnError>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphMutationError, GraphMutationIntent};
        let source = |error| GqlQueryError::Source(GraphMutationError::Source(error));
        // Even an empty/zero-budget selection cannot bypass ownership, health,
        // basis or the existing relation-coordinate contract.
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
        if let Some(first) = self.staged.first()
            && !self.program_multi_relation
            && self
                .staged
                .iter()
                .all(|batch| batch.relation == first.relation)
            && mutation.relation() != first.relation
        {
            return Err(source(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: mutation.relation(),
            }));
        }
        cx.with_restriction(|| {
            let proposal = mutation.execute_governed(
                policy,
                |pattern, budget| {
                    self.execute_graph_pattern_governed(database, cx, pattern, budget)
                },
                || cx.checkpoint(),
            )?;
            let stats = proposal.stats();
            let (targets, edges) = if retain_targets {
                let mut targets = std::collections::BTreeSet::new();
                let mut edges = std::collections::BTreeSet::new();
                for intent in proposal.intents() {
                    match intent {
                        GraphMutationIntent::Property { vertex, .. }
                        | GraphMutationIntent::Label { vertex, .. }
                        | GraphMutationIntent::DetachDelete { vertex } => {
                            targets.insert(*vertex);
                        }
                        GraphMutationIntent::EdgeProperty { edge, .. } => {
                            edges.insert(*edge);
                        }
                    }
                }
                (targets.into_iter().collect(), edges.into_iter().collect())
            } else {
                (Vec::new(), Vec::new())
            };
            let mut batch = WriteBatch::new(mutation.relation());
            for intent in proposal.into_intents() {
                // Only a private batch is changing here. Cancellation drops it
                // whole and cannot expose a partially appended statement.
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                match intent {
                    GraphMutationIntent::Property { vertex, key, value } => {
                        batch.set_vertex_property(vertex, key, value);
                    }
                    GraphMutationIntent::EdgeProperty { edge, key, value } => {
                        batch.set_edge_property(edge, key, value);
                    }
                    GraphMutationIntent::Label {
                        vertex,
                        label,
                        present,
                    } => {
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
            if !batch.is_empty() {
                self.write(database, batch).map_err(source)?;
            }
            Ok((stats, targets, edges))
        })
    }
}
