impl WriteTxn {
    /// Borrow pinned rows and apply canonical prepared effects once. The source
    /// owner preserves label-scoped insertion and negative-read dependencies.
    fn execute_prepared_node_scan<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
    ) -> Result<Vec<VId>, WriteTxnError> {
        self.query_source(database, plan)?.execute()
    }
}
