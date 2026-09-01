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

fn snapshot_record_count<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    plan: &BoundPlan,
    as_of: CommitSeq,
) -> Result<u64, GqlError> {
    if plan.relation.is_some() {
        reader
            .gql_edges_at(as_of)
            .map(|rows| count_as_u64(rows.len()))
            .map_err(GqlError::Read)
    } else {
        reader
            .gql_vertices_at(as_of)
            .map(|rows| count_as_u64(rows.len()))
            .map_err(GqlError::Read)
    }
}

fn execute_budgeted_at<R: crate::gql_exec::GqlSnapshotReader + ?Sized>(
    reader: &R,
    query: &fgdb_gql::PreparedGqlQuery,
    as_of: CommitSeq,
    budget: fgdb_gql::GqlExecutionBudget,
) -> Result<
    fgdb_gql::BudgetedGqlExecution<Vec<VId>>,
    fgdb_gql::BudgetedGqlError<GqlError>,
> {
    let snapshot_records = snapshot_record_count(reader, query.plan(), as_of)
        .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
    budget
        .check(
            fgdb_gql::GqlBudgetDimension::SnapshotRecords,
            snapshot_records,
        )
        .map_err(fgdb_gql::BudgetedGqlError::Budget)?;

    let rows = crate::gql_exec::execute_at(query.plan(), reader, as_of)
        .map_err(GqlError::Read)
        .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
    let result_rows = count_as_u64(rows.len());
    budget
        .check(fgdb_gql::GqlBudgetDimension::ResultRows, result_rows)
        .map_err(fgdb_gql::BudgetedGqlError::Budget)?;

    Ok(fgdb_gql::BudgetedGqlExecution {
        value: rows,
        stats: fgdb_gql::GqlExecutionStats {
            snapshot_records,
            result_rows,
        },
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare one coherent reusable GQL definition.
    ///
    /// Unlike [`Database::prepare_gql_plan`], this retains the exact statement
    /// bytes and an owned canonical bind map beside the derived plan. Execution
    /// still uses the same bound-plan kernel and does not parse or bind again.
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

    /// Execute one owned prepared definition under deterministic admission
    /// and final-row bounds at the live frontier.
    pub fn execute_prepared_query_budgeted(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<
        fgdb_gql::BudgetedGqlExecution<Vec<VId>>,
        fgdb_gql::BudgetedGqlError<GqlError>,
    > {
        let as_of = self
            .frontier()
            .map_err(GqlError::Read)
            .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        self.execute_prepared_query_budgeted_at(query, as_of, budget)
    }

    /// Execute one owned prepared definition under deterministic bounds at one
    /// exact retained sequence. No partial rows escape a budget refusal.
    pub fn execute_prepared_query_budgeted_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<
        fgdb_gql::BudgetedGqlExecution<Vec<VId>>,
        fgdb_gql::BudgetedGqlError<GqlError>,
    > {
        execute_budgeted_at(self, query, as_of, budget)
    }

    /// Execute one owned prepared definition and return input, plan, and exact
    /// ordered-result evidence aligned to the same live frontier.
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

    /// Execute one owned prepared definition at `as_of` and return every
    /// current evidence layer without reparsing or rebinding.
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
        Ok((
            rows,
            input_certificate,
            plan_certificate,
            result_digest,
        ))
    }
}

impl crate::EmbeddedReadView {
    /// Prepare one coherent reusable GQL definition for this read surface.
    pub fn prepare_gql_query(
        &self,
        statement: &str,
        bind: &RelationBind,
    ) -> Result<fgdb_gql::PreparedGqlQuery, GqlError> {
        prepare_owned_gql_query(statement, bind)
    }

    /// Execute one owned prepared definition at this view's pinned frontier.
    pub fn execute_prepared_query(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql(query.plan())
    }

    /// Execute one owned prepared definition at a sequence retained by this
    /// immutable view.
    pub fn execute_prepared_query_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<Vec<VId>, GqlError> {
        self.execute_prepared_gql_at(query.plan(), as_of)
    }

    /// Execute one owned prepared definition under deterministic bounds at this
    /// immutable view's pinned frontier.
    pub fn execute_prepared_query_budgeted(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<
        fgdb_gql::BudgetedGqlExecution<Vec<VId>>,
        fgdb_gql::BudgetedGqlError<GqlError>,
    > {
        self.execute_prepared_query_budgeted_at(query, self.frontier(), budget)
    }

    /// Execute one owned prepared definition under deterministic bounds at a
    /// sequence retained by this immutable view.
    pub fn execute_prepared_query_budgeted_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<
        fgdb_gql::BudgetedGqlExecution<Vec<VId>>,
        fgdb_gql::BudgetedGqlError<GqlError>,
    > {
        execute_budgeted_at(self, query, as_of, budget)
    }

    /// Execute one owned prepared definition and return every current evidence
    /// layer aligned to this view's pinned frontier.
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

    /// Execute one owned prepared definition at a retained sequence and return
    /// every current evidence layer without reparsing or rebinding.
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
        Ok((
            rows,
            input_certificate,
            plan_certificate,
            result_digest,
        ))
    }
}

impl WriteTxn {
    /// Prepare one coherent reusable GQL definition while this transaction is
    /// live. Preparation itself is snapshot-independent; the finished check
    /// keeps the transaction API's lifecycle refusals uniform.
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

    /// Execute one owned prepared definition over the durable basis plus staged
    /// read-your-own-writes state.
    pub fn execute_prepared_query<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<Vec<VId>, WriteTxnError> {
        self.execute_prepared_gql(database, query.plan())
    }

    /// Execute one owned prepared definition over the staged overlay under
    /// deterministic admitted-record and final-row bounds.
    ///
    /// Counting the overlay is itself a transaction read. A snapshot-record
    /// refusal therefore leaves the conservative read set populated even though
    /// no query rows are returned.
    pub fn execute_prepared_query_budgeted<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<
        fgdb_gql::BudgetedGqlExecution<Vec<VId>>,
        fgdb_gql::BudgetedGqlError<WriteTxnError>,
    > {
        let snapshot_records = if query.plan().relation.is_some() {
            self.edges(database)
                .map(|rows| count_as_u64(rows.len()))
        } else {
            self.vertices(database)
                .map(|rows| count_as_u64(rows.len()))
        }
        .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        budget
            .check(
                fgdb_gql::GqlBudgetDimension::SnapshotRecords,
                snapshot_records,
            )
            .map_err(fgdb_gql::BudgetedGqlError::Budget)?;

        let rows = self
            .execute_prepared_query(database, query)
            .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        let result_rows = count_as_u64(rows.len());
        budget
            .check(fgdb_gql::GqlBudgetDimension::ResultRows, result_rows)
            .map_err(fgdb_gql::BudgetedGqlError::Budget)?;

        Ok(fgdb_gql::BudgetedGqlExecution {
            value: rows,
            stats: fgdb_gql::GqlExecutionStats {
                snapshot_records,
                result_rows,
            },
        })
    }

    /// Execute and plan-certify one owned prepared definition at the durable
    /// transaction basis.
    ///
    /// This deliberately returns no ordered-result digest: the current plan
    /// certificate does not bind the staged overlay that produced the rows.
    pub fn execute_prepared_query_certified<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<(Vec<VId>, crate::GqlPlanCertificate), WriteTxnError> {
        self.execute_prepared_gql_certified(database, query.plan())
    }

    /// Certify the plan retained by one owned prepared definition at this
    /// transaction's durable basis without executing it.
    pub fn prepared_query_plan_certificate(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<crate::GqlPlanCertificate, WriteTxnError> {
        self.prepared_gql_plan_certificate(query.plan())
    }
}
