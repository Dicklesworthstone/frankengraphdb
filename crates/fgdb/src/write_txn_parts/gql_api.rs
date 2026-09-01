impl WriteTxn {
    /// Bind once, execute once, and certify the resulting plan against the
    /// transaction basis rather than the database's potentially newer frontier.
    pub fn execute_gql_certified<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        src: &str,
        bind: &RelationBind,
    ) -> Result<(Vec<VId>, crate::GqlPlanCertificate), WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let plan = Self::bind_gql_plan(src, bind)?;
        self.execute_prepared_gql_certified(database, &plan)
    }

    /// Execute and certify one already-bound overlay MATCH without reparsing or
    /// rebinding. Evidence is minted only after successful overlay execution.
    pub fn execute_prepared_gql_certified<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        plan: &BoundPlan,
    ) -> Result<(Vec<VId>, crate::GqlPlanCertificate), WriteTxnError> {
        let rows = self.execute_prepared_gql(database, plan)?;
        let certificate = crate::gql_cert::certify(plan, self.basis());
        Ok((rows, certificate))
    }

    /// Bind one statement exactly once and certify the resulting plan at this
    /// transaction's durable basis without executing it.
    pub fn gql_plan_certificate(
        &self,
        src: &str,
        bind: &RelationBind,
    ) -> Result<crate::GqlPlanCertificate, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        let plan = Self::bind_gql_plan(src, bind)?;
        self.prepared_gql_plan_certificate(&plan)
    }

    /// Certify one already-bound plan at this transaction's durable basis.
    pub fn prepared_gql_plan_certificate(
        &self,
        plan: &BoundPlan,
    ) -> Result<crate::GqlPlanCertificate, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        Ok(crate::gql_cert::certify(plan, self.basis()))
    }

    fn bind_gql_plan(src: &str, bind: &RelationBind) -> Result<BoundPlan, WriteTxnError> {
        bind.bind(src).map_err(|error| {
            WriteTxnError::Gql(match error {
                fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
                unbound => GqlError::Bind(unbound),
            })
        })
    }

}
