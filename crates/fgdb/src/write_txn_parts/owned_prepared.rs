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
    let admitted = crate::gql_exec::AdmittedGqlSnapshot::admit(query.plan(), reader, as_of)
        .map_err(GqlError::Read)
        .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
    admitted.execute_budgeted(budget).map_err(|error| match error {
        fgdb_gql::BudgetedGqlError::Execution(error) => {
            fgdb_gql::BudgetedGqlError::Execution(GqlError::Read(error))
        }
        fgdb_gql::BudgetedGqlError::Budget(error) => fgdb_gql::BudgetedGqlError::Budget(error),
    })
}

impl<V: Vfs + Clone> Database<V> {
    /// Prepare one coherent reusable GQL definition, owning its exact statement
    /// and canonical names beside the bound plan. Execution never parses again.
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

    /// Deterministic admission and final-row bounds on one execution.
    pub fn execute_prepared_query_budgeted(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>> {
        let as_of = self.frontier().map_err(GqlError::Read)
            .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        self.execute_prepared_query_budgeted_at(query, as_of, budget)
    }

    /// Exact retained-sequence execution. No partial rows escape refusal.
    pub fn execute_prepared_query_budgeted_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>> {
        execute_budgeted_at(self, query, as_of, budget)
    }

    /// Input, plan, and ordered-result evidence at the same frontier.
    pub fn execute_prepared_query_with_result_digest(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<(Vec<VId>, crate::GqlCertificate, crate::GqlPlanCertificate, fgdb_crypto::Digest), GqlError> {
        let as_of = self.frontier().map_err(GqlError::Read)?;
        self.execute_prepared_query_with_result_digest_at(query, as_of)
    }

    pub fn execute_prepared_query_with_result_digest_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, crate::GqlCertificate, crate::GqlPlanCertificate, fgdb_crypto::Digest), GqlError> {
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
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>> {
        self.execute_prepared_query_budgeted_at(query, self.frontier(), budget)
    }

    pub fn execute_prepared_query_budgeted_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<GqlError>> {
        execute_budgeted_at(self, query, as_of, budget)
    }

    pub fn execute_prepared_query_with_result_digest(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
    ) -> Result<(Vec<VId>, crate::GqlCertificate, crate::GqlPlanCertificate, fgdb_crypto::Digest), GqlError> {
        self.execute_prepared_query_with_result_digest_at(query, self.frontier())
    }

    pub fn execute_prepared_query_with_result_digest_at(
        &self,
        query: &fgdb_gql::PreparedGqlQuery,
        as_of: CommitSeq,
    ) -> Result<(Vec<VId>, crate::GqlCertificate, crate::GqlPlanCertificate, fgdb_crypto::Digest), GqlError> {
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

    /// Meter and execute the same borrowed canonical overlay. Negative IDs,
    /// observed endpoints and label/table witnesses survive a budget refusal.
    /// This row-only API leaves source/evaluator scratch and work unbounded;
    /// the governed API applies one policy to those phases as well.
    pub fn execute_prepared_query_budgeted<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        query: &fgdb_gql::PreparedGqlQuery,
        budget: fgdb_gql::GqlExecutionBudget,
    ) -> Result<fgdb_gql::BudgetedGqlExecution<Vec<VId>>, fgdb_gql::BudgetedGqlError<WriteTxnError>> {
        let snapshot = self.query_snapshot(database)
            .map_err(fgdb_gql::BudgetedGqlError::Execution)?;
        let source = self.query_source_over(
            snapshot,
            query.plan(),
            &mut crate::gql_exec::snapshot_record_budget::<WriteTxnError>(budget),
        )?;
        source.logical.execute_budgeted(
            count_as_u64(source.snapshot_records), source.vertex_ids(), source.edge_triples(),
            |vid, predicates| Ok(source.matches(vid, predicates)), budget,
        )
    }

    /// Plan-certify at the durable basis, without claiming to bind the overlay.
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

#[cfg(test)]
mod budgeted_overlay_admission_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::{BudgetedGqlError, GqlBudgetDimension, GqlExecutionBudget, PreparedGqlQuery};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn row_budget_counts_canonical_staging_and_stops_before_the_next_output_record() {
        let ((), report) = run_async_under_lab(0xb0d6_0002, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            let keys = crate::DatabaseKeys::new(
                [0xb1; 32],
                DatabaseSecurityNamespaceId([0xb2; 32]),
                [0xb3; 32],
            );
            let mut db = Database::open_memory(&commit, keys).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 1..=6 {
                seed.create_vertex(VId(id), vec![LabelId(1)], vec![]);
                if id > 1 {
                    seed.add_edge(EId(id - 1), VId(id - 1), VId(id), vec![]);
                }
            }
            let basis = db.write(&commit, seed).await.unwrap();
            let bind = RelationBind::new()
                .with_label("L", LabelId(1))
                .with_relation("R", RelationId(1));
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut staged = WriteBatch::new(RelationId(1));
            staged.delete_vertex(VId(6));
            staged.create_vertex(VId(10), vec![LabelId(1)], vec![]);
            staged.add_edge(EId(10), VId(5), VId(10), vec![]);
            txn.write(&mut db, staged).unwrap();
            for (statement, records, expected) in [
                ("MATCH (a:L) RETURN a", 6,
                    vec![VId(1), VId(2), VId(3), VId(4), VId(5), VId(10)]),
                ("MATCH (a)-[:R]->(b) RETURN b", 5,
                    vec![VId(2), VId(3), VId(4), VId(5), VId(10)]),
            ] {
                let query = PreparedGqlQuery::prepare(statement, &bind).unwrap();
                for limit in [0, 1] {
                    let result = txn.execute_prepared_query_budgeted(
                        &db, &query, GqlExecutionBudget::new(limit, 100),
                    );
                    assert!(matches!(result, Err(BudgetedGqlError::Budget(error))
                        if error.dimension == GqlBudgetDimension::SnapshotRecords
                            && error.limit == limit && error.observed == limit + 1));
                }
                let exact = txn.execute_prepared_query_budgeted(
                    &db, &query, GqlExecutionBudget::new(records, expected.len() as u64),
                ).unwrap();
                assert_eq!(exact.value, expected);
                assert_eq!(exact.stats.snapshot_records, records);
                assert_eq!(exact.stats.result_rows, expected.len() as u64);
                assert_eq!(txn.execute_prepared_query(&db, &query).unwrap(), expected);
            }
            assert_eq!(db.frontier().unwrap(), basis);
            assert!(db.vertex(VId(10)).unwrap().is_none());
            txn.abort();

            // Deleting the entire basis is legal with a zero source allowance:
            // count the canonical overlay, not the rows it superseded.
            let mut txn = db.begin(&txn_cx).unwrap();
            let mut deleted = WriteBatch::new(RelationId(1));
            for id in 1..=6 {
                deleted.delete_vertex(VId(id));
            }
            txn.write(&mut db, deleted).unwrap();
            for statement in ["MATCH (a:L) RETURN a", "MATCH (a)-[:R]->(b) RETURN b"] {
                let query = PreparedGqlQuery::prepare(statement, &bind).unwrap();
                let empty = txn.execute_prepared_query_budgeted(
                    &db, &query, GqlExecutionBudget::new(0, 0),
                ).unwrap();
                assert!(empty.value.is_empty());
                assert_eq!(empty.stats.snapshot_records, 0);
                assert_eq!(empty.stats.result_rows, 0);
            }
            txn.abort();
            assert_eq!(db.frontier().unwrap(), basis);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn row_budget_refusal_preserves_point_and_phantom_conflict_witnesses() {
        let ((), report) = run_async_under_lab(0xb0d6_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txn_cx = contexts.txn();
            for phantom in [false, true] {
                let keys = crate::DatabaseKeys::new(
                    [0xc1; 32],
                    DatabaseSecurityNamespaceId([0xc2; 32]),
                    [0xc3; 32],
                );
                let mut db = Database::open_memory(&commit, keys).await.unwrap();
                let mut seed = WriteBatch::new(RelationId(1));
                seed.create_vertex(VId(1), vec![LabelId(1)], vec![]);
                db.write(&commit, seed).await.unwrap();
                let mut txn = db.begin(&txn_cx).unwrap();
                let mut staged = WriteBatch::new(RelationId(1));
                staged.create_vertex(VId(99), vec![], vec![]);
                txn.write(&mut db, staged).unwrap();
                let query = PreparedGqlQuery::prepare(
                    "MATCH (a:L) RETURN a",
                    &RelationBind::new().with_label("L", LabelId(1)),
                ).unwrap();
                let refused = txn.execute_prepared_query_budgeted(
                    &db, &query, GqlExecutionBudget::new(0, 100),
                );
                assert!(matches!(refused, Err(BudgetedGqlError::Budget(error))
                    if error.dimension == GqlBudgetDimension::SnapshotRecords
                        && error.limit == 0 && error.observed == 1));
                assert!(txn.read_set.borrow().contains(&ElementId::Vertex(VId(1))));
                assert!(txn.scanned_vertex_labels.borrow().contains(&LabelId(1)));
                let mut winner = WriteBatch::new(RelationId(1));
                if phantom {
                    winner.create_vertex(VId(2), vec![LabelId(1)], vec![]);
                } else {
                    winner.set_vertex_property(
                        VId(1), fgdb_delta_types::PropertyKeyId(1), Some(CanonicalScalar::Int(7)),
                    );
                }
                let frontier = db.write(&commit, winner).await.unwrap();
                assert!(matches!(txn.commit(&mut db, &commit).await,
                    Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
                        law: "FG-LAW-FCW-READ-01", ..
                    }))));
                assert_eq!(db.frontier().unwrap(), frontier);
                assert!(db.vertex(VId(99)).unwrap().is_none());
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
