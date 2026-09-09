impl WriteTxn {
    /// Preparation alone interprets intentions. Queries borrow the admitted
    /// basis and consume the same canonical effects that commit publishes.
    fn execute_prepared_edge_match<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
        edge_relation: RelationId,
    ) -> Result<Vec<VId>, WriteTxnError> {
        debug_assert_eq!(plan.relation, Some(edge_relation));
        self.query_source(database, plan)?.execute()
    }
}
