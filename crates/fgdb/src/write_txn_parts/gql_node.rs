impl WriteTxn {
    /// Execute the node-only scan over overlay vertex rows. Staged labels and
    /// properties are visible, staged deletes hide rows, and no edge machinery
    /// is consulted when the bound plan has no relation.
    fn execute_prepared_node_scan<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if plan.src_label.is_none() {
            return Ok(Vec::new());
        }
        let logical = fgdb_gql::algebra::GlaPlan::lower(plan);
        // Bulk admission observes each retained/staged vertex through the
        // existing transaction path. Predicate evaluation reuses those exact
        // rows instead of replaying the basis for each property comparison.
        let rows: std::collections::BTreeMap<VId, VertexRow> = self
            .vertices(database)?
            .into_iter()
            .map(|row| (row.vid, row))
            .collect();
        let vids = logical.execute(rows.keys().copied(), [], |vid, predicates| {
            Ok::<_, WriteTxnError>(rows.get(&vid).is_some_and(|row| {
                predicates
                    .iter()
                    .all(|predicate| predicate.matches(&row.labels, &row.props))
            }))
        })?;
        self.read_set
            .borrow_mut()
            .extend(vids.iter().copied().map(ElementId::Vertex));
        Ok(vids)
    }
}
