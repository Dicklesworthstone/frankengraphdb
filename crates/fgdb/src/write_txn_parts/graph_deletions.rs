// Plain DELETE is intentionally separate from DETACH DELETE. The GQL kernel
// freezes/deduplicates targets; this adapter alone can prove that each target
// has no live incident relationship in the canonical transaction overlay.

impl WriteTxn {
    /// Stage a non-detaching query-selected vertex deletion. Every selected
    /// target must have zero live incident relationships after all earlier
    /// staged effects in this transaction. Otherwise the complete statement
    /// refuses and no delete batch is appended.
    ///
    /// The incident-edge scan is also a conflict witness: a concurrent edge
    /// insertion after this check invalidates completion instead of allowing a
    /// plain DELETE to turn into an implicit cascade. Success still only stages;
    /// the caller explicitly finishes or commits the transaction.
    pub fn execute_graph_delete_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        fgdb_gql::GraphDeleteStats,
        fgdb_gql::GqlQueryError<fgdb_gql::GraphDeleteError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        self.execute_graph_delete_returning_governed(database, cx, deletion, policy)
            .map(|(stats, _)| stats)
    }

    /// The same plain DELETE with the exact distinct staged target identities.
    /// This is a transaction-local receipt, not a durability acknowledgement.
    pub fn execute_graph_delete_returning_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        (fgdb_gql::GraphDeleteStats, Vec<VId>),
        fgdb_gql::GqlQueryError<fgdb_gql::GraphDeleteError<WriteTxnError>, Box<asupersync::error::Error>>,
    > {
        use fgdb_gql::{GraphDeleteError, GqlQueryError};
        let source = |error| GqlQueryError::Source(GraphDeleteError::Source(error));

        self.ensure_database(database).map_err(source)?;
        let live = database.frontier().map_err(WriteTxnError::from).map_err(source)?;
        if live != self.basis {
            return Err(source(WriteTxnError::SnapshotAdvanced { pinned: self.basis, live }));
        }
        if let Some(first) = self.staged.first()
            && self.staged.iter().all(|batch| batch.relation == first.relation)
            && deletion.relation() != first.relation
        {
            return Err(source(WriteTxnError::RelationMismatch {
                expected: first.relation, found: deletion.relation(),
            }));
        }

        cx.with_restriction(|| {
            let proposal = deletion.execute_governed(
                policy,
                |pattern, budget| self.execute_graph_pattern_governed(database, cx, pattern, budget),
                || cx.checkpoint(),
            )?;
            let stats = proposal.stats();
            let targets = proposal.into_targets();
            if targets.is_empty() {
                return Ok((stats, targets));
            }

            // Read the exact staged overlay once. Edge deletions already staged
            // by the transaction disappear here; staged edge creations appear.
            // edges() retains both point and scan dependencies for completion.
            let edges = self.edges(database).map_err(source)?;
            for target in &targets {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                for edge in &edges {
                    cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                    if edge.entry.src == *target || edge.entry.dst == *target {
                        return Err(GqlQueryError::Source(GraphDeleteError::IncidentRelationships));
                    }
                }
            }

            let mut batch = WriteBatch::new(deletion.relation());
            for target in &targets {
                cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
                // Storage still uses its ordinary vertex-delete representation.
                // The proof above guarantees its cascade set is empty in this
                // workspace; concurrent topology changes are caught at finish.
                batch.delete_vertex(*target);
            }
            cx.checkpoint().map_err(GqlQueryError::Interrupted)?;
            self.write(database, batch).map_err(source)?;
            Ok((stats, targets))
        })
    }
}
