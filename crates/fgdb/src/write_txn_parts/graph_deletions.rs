// Plain DELETE is intentionally separate from DETACH DELETE. The GQL kernel
// freezes/deduplicates targets; this adapter alone can prove that each target
// has no live incident relationship in the canonical transaction overlay.

include!("delete_incidence.rs");

impl WriteTxn {
    /// Stage a non-detaching query-selected element deletion. Vertex targets
    /// must have no incident relationships remaining after explicitly selected
    /// edge targets are removed. Relationship deletion keeps both endpoints.
    /// A refusal appends no delete batch.
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
    ) -> Result<fgdb_gql::GraphDeleteStats, TxnGqlError<fgdb_gql::GraphDeleteError<WriteTxnError>>>
    {
        self.execute_graph_delete_returning_governed(database, cx, deletion, policy)
            .map(|(stats, _)| stats)
    }

    /// The same DELETE with its distinct staged vertex targets only; edge
    /// targets are included in statistics, not this vertex-identity receipt.
    /// This is a transaction-local receipt, not a durability acknowledgement.
    /// Returned source/work/scratch totals include the incident-edge validation
    /// and delete proposals, under the SAME allowance as MATCH. Incidence proof
    /// visits the selected vertices' indexed incoming/outgoing identities, not
    /// every graph edge or its properties. It charges each distinct live basis
    /// edge once, even for a self-loop or two selected endpoints; staged effects
    /// use work/scratch, not source records. This is not a byte-memory bound on
    /// MATCH or native write preparation, which retain their own limitations.
    /// A data-dependent incidence refusal or unwind retains conservative read
    /// dependencies without allocating; it never clears earlier observations.
    pub fn execute_graph_delete_returning_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        (fgdb_gql::GraphDeleteStats, Vec<VId>),
        TxnGqlError<fgdb_gql::GraphDeleteError<WriteTxnError>>,
    > {
        self.execute_graph_delete_elements_returning_governed(database, cx, deletion, policy)
            .map(|(stats, vertices, _)| (stats, vertices))
    }

    /// Return disjoint, sorted vertex and relationship target identities.
    /// Receipts describe staging, never successful durability.
    pub fn execute_graph_delete_elements_returning_governed<V: Vfs + Clone>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        deletion: &fgdb_gql::PreparedGraphDelete,
        policy: fgdb_gql::GraphDeletePolicy,
    ) -> Result<
        WithAffectedIds<fgdb_gql::GraphDeleteStats>,
        TxnGqlError<fgdb_gql::GraphDeleteError<WriteTxnError>>,
    > {
        use fgdb_gql::{GqlQueryError, GraphDeleteError};
        let source = |error| GqlQueryError::Source(GraphDeleteError::Source(error));

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
            && deletion.relation() != first.relation
        {
            return Err(source(WriteTxnError::RelationMismatch {
                expected: first.relation,
                found: deletion.relation(),
            }));
        }

        cx.with_restriction(|| {
            let proposal = deletion.execute_governed(
                policy,
                |pattern, budget| {
                    self.execute_graph_pattern_governed(database, cx, pattern, budget)
                },
                || cx.checkpoint(),
            )?;
            self.stage_delete_targets_controlled(
                database,
                deletion.relation(),
                proposal.stats(),
                proposal.into_target_parts(),
                policy,
                || cx.checkpoint(),
            )
        })
    }
}
