impl WriteTxn {
    /// Parse and bind one overlay MATCH exactly once, then execute the bound
    /// plan through [`WriteTxn::execute_prepared_gql`].
    pub fn execute_gql<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: &str,
        bind: &RelationBind,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let plan = Self::bind_gql_plan(src, bind)?;
        self.execute_prepared_gql(database, &plan)
    }

    /// Execute one already-bound MATCH over the durable basis plus staged
    /// vertex and edge mutations, without reparsing, rebinding, or publishing.
    ///
    /// This is the single transaction-overlay execution body. Text and
    /// certified entrypoints delegate here so read-set tracking, read-your-own-
    /// writes semantics, projection, ordering, `SKIP`, and `LIMIT` cannot drift.
    pub fn execute_prepared_gql<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
    ) -> Result<Vec<VId>, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        match plan.relation {
            None => self.execute_prepared_node_scan(database, plan),
            Some(relation) => self.execute_prepared_edge_match(database, plan, relation),
        }
    }
}
