fn prepare_owned_gql_query(
    statement: &str,
    bind: &RelationBind,
) -> Result<fgdb_gql::PreparedGqlQuery, GqlError> {
    fgdb_gql::PreparedGqlQuery::prepare(statement, bind).map_err(|error| match error {
        fgdb_gql::BindError::Parse(parse) => GqlError::Parse(parse),
        unbound => GqlError::Bind(unbound),
    })
}

fn prepared_query_input_certificate(
    query: &fgdb_gql::PreparedGqlQuery,
    snapshot_seq: CommitSeq,
) -> crate::GqlCertificate {
    crate::GqlCertificate {
        snapshot_seq,
        statement_digest: crate::gql_cert::digest_statement(query.statement()),
        bind_digest: crate::gql_cert::digest_bind(query.bind()),
    }
}

fn count_as_u64(len: usize) -> u64 {
    u64::try_from(len).unwrap_or(u64::MAX)
}

fn execute_budgeted_at<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    query: &fgdb_gql::PreparedGqlQuery,
    as_of: CommitSeq,
    budget: fgdb_gql::GqlExecutionBudget,
) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>> {
    // Keep the admitted rows until execution; do not discard a counting scan
    // and then validate/materialize the same source table a second time.
    let admitted = crate::gql_exec::AdmittedGqlSnapshot::admit(query.plan(), reader, as_of)
        .map_err(GqlError::Read)
        .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
    admitted
        .execute_budgeted(budget)
        .map_err(|error| match error {
            fgdb_gql::BudgetedGqlError::Execution(error) => {
                fgdb_gql::BudgetedGqlError::Execution(GqlError::Read(error))
            }
            fgdb_gql::BudgetedGqlError::Budget(error) => fgdb_gql::BudgetedGqlError::Budget(error),
        })
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare one coherent reusable GQL definition.
    ///
    /// Unlike [`Database::prepare_gql_plan`], this retains the exact statement
    /// bytes and an owned canonical bind map beside the derived plan. Execution
    /// lowers the same bound definition without parsing or binding again.
    pub fn prepare_gql_query(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<fgdb_gql::PreparedGqlQuery, GqlError> {
        prepare_owned_gql_query(statement, bind)
    }

    /// Execute one owned prepared definition at the live frontier.
    pub fn execute_prepared_query(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql(query.plan())
    }

    /// Execute one owned prepared definition at an exact retained sequence.
    pub fn execute_prepared_query_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql_at(query.plan(), as_of)
    }

    /// Execute under deterministic admission and final-row bounds.
    pub fn execute_prepared_query_budgeted(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>>
    {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        self.execute_prepared_query_budgeted_at(query, as_of, budget)
    }

    /// Exact retained-sequence execution. No partial rows escape refusal.
    pub fn execute_prepared_query_budgeted_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>>
    {
        execute_budgeted_at(self, query, as_of, budget)
    }

    /// Input, plan, and exact ordered-result evidence at the same frontier.
    pub fn execute_prepared_query_with_result_digest(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<
        (
            Vec<VId>,
            crate::GqlCertificate,
            crate::GqlPlanCertificate,
            fgdb_crypto::Digest,
        ),
        GqlError,
    > {
        let as_of = self.frontier().map_err(GqlError::Read)?;
        self.execute_prepared_query_with_result_digest_at(query, as_of)
    }

    /// Execute and evidence at `as_of`, without reparsing or rebinding.
    pub fn execute_prepared_query_with_result_digest_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<
        (
            Vec<VId>,
            crate::GqlCertificate,
            crate::GqlPlanCertificate,
            fgdb_crypto::Digest,
        ),
        GqlError,
    > {
        let (rows, plan_certificate, result_digest) =
            self.execute_prepared_gql_with_result_digest_at(query.plan(), as_of)?;
        let input_certificate = prepared_query_input_certificate(query, as_of);
        Ok((rows, input_certificate, plan_certificate, result_digest))
    }
}

impl crate::EmbeddedReadView {
    pub fn prepare_gql_query(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<fgdb_gql::PreparedGqlQuery, GqlError> {
        prepare_owned_gql_query(statement, bind)
    }

    pub fn execute_prepared_query(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql(query.plan())
    }

    pub fn execute_prepared_query_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql_at(query.plan(), as_of)
    }

    pub fn execute_prepared_query_budgeted(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>>
    {
        self.execute_prepared_query_budgeted_at(query, self.frontier(), budget)
    }

    pub fn execute_prepared_query_budgeted_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>>
    {
        execute_budgeted_at(self, query, as_of, budget)
    }

    pub fn execute_prepared_query_with_result_digest(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<
        (
            Vec<VId>,
            crate::GqlCertificate,
            crate::GqlPlanCertificate,
            fgdb_crypto::Digest,
        ),
        GqlError,
    > {
        self.execute_prepared_query_with_result_digest_at(query, self.frontier())
    }

    pub fn execute_prepared_query_with_result_digest_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<
        (
            Vec<VId>,
            crate::GqlCertificate,
            crate::GqlPlanCertificate,
            fgdb_crypto::Digest,
        ),
        GqlError,
    > {
        let (rows, plan_certificate, result_digest) =
            self.execute_prepared_gql_with_result_digest_at(query.plan(), as_of)?;
        let input_certificate = prepared_query_input_certificate(query, as_of);
        Ok((rows, input_certificate, plan_certificate, result_digest))
    }
}

impl WriteTxn {
    /// Preparation is snapshot-independent, but requires a live transaction.
    pub fn prepare_gql_query(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<fgdb_gql::PreparedGqlQuery, WriteTxnError> {
        if self.pin.is_none() {
            return Err(WriteTxnError::Finished);
        }
        prepare_owned_gql_query(statement, bind).map_err(WriteTxnError::Gql)
    }

    pub fn execute_prepared_query<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<Vec<VId>, WriteTxnError> {
        self.execute_prepared_gql(database, query.plan())
    }

    /// Counting the overlay is a transaction read. Admission refusals retain
    /// the conservative dependency footprint even when no query rows return.
    pub fn execute_prepared_query_budgeted<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<WriteTxnError>>
    {
        let logical = fgdb_gql::algebra::GlaPlan::lower(query.plan());
        if logical.scans_edges() {
            // edges() applies the exact prepared net effects and retains even
            // absent staged identities. Count and execute these same rows;
            // neither a second storage merge nor raw-intent replay is needed.
            let rows = self
                .edges(database)
                .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
            // Retain endpoint observations for every admitted edge, including
            // those predicates, DISTINCT or LIMIT will discard. edges() owns
            // the table-wide phantom and negative-EId witnesses already.
            self.read_set
                .borrow_mut()
                .extend(rows.iter().flat_map(|row| {
                    [
                        ElementId::Vertex(row.entry.src),
                        ElementId::Vertex(row.entry.dst),
                    ]
                }));
            logical.execute_budgeted(
                count_as_u64(rows.len()),
                [],
                rows.iter()
                    .map(|row| (row.entry.src, row.entry.relation, row.entry.dst)),
                |vid, predicates| {
                    Ok(self.vertex(database, vid)?.is_some_and(|row| {
                        predicates
                            .iter()
                            .all(|p| p.matches(&row.labels, &row.props))
                    }))
                },
                budget,
            )
        } else {
            let rows: std::collections::BTreeMap<VId, VertexRow> = self
                .vertices_for_scan(database, query.plan().src_label)
                .map_err(fgdb_gql::BudgetedGqlError::Execution)?
                .into_iter()
                .map(|row| (row.vid, row))
                .collect();
            logical.execute_budgeted(
                count_as_u64(rows.len()),
                rows.keys().copied(),
                [],
                |vid, predicates| {
                    Ok(rows.get(&vid).is_some_and(|row| {
                        predicates
                            .iter()
                            .all(|p| p.matches(&row.labels, &row.props))
                    }))
                },
                budget,
            )
        }
    }

    /// Plan-certify at the durable basis. This certificate does not bind the
    /// staged overlay, so this method deliberately issues no result digest.
    pub fn execute_prepared_query_certified<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<(Vec<VId>, crate::GqlPlanCertificate), WriteTxnError> {
        self.execute_prepared_gql_certified(database, query.plan())
    }

    pub fn prepared_query_plan_certificate(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<crate::GqlPlanCertificate, WriteTxnError> {
        self.prepared_gql_plan_certificate(query.plan())
    }
}
