impl WriteTxn {
    /// Execute the node-only scan over overlay vertex rows. Staged labels and
    /// properties are visible, staged deletes hide rows, and no edge machinery
    /// is consulted when the bound plan has no relation.
    fn execute_prepared_node_scan<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
    ) -> Result<Vec<VId>, WriteTxnError> {
        let Some(label) = plan.src_label else {
            return Ok(Vec::new());
        };
        let predicates = [
            plan.src_prop,
            plan.src_prop_ne,
            plan.src_prop_gt,
            plan.src_prop_lt,
            plan.src_prop_ge,
            plan.src_prop_le,
        ];
        let mut vids: Vec<VId> = self
            .vertices(database)?
            .into_iter()
            .filter(|row| row.labels.contains(&label))
            .filter(|row| row_matches_property_predicates(row, predicates))
            .map(|row| row.vid)
            .collect();
        vids.sort_unstable();
        vids.dedup();
        let vids = crate::apply_limit(plan, vids);
        self.read_set
            .borrow_mut()
            .extend(vids.iter().copied().map(ElementId::Vertex));
        Ok(vids)
    }
}
