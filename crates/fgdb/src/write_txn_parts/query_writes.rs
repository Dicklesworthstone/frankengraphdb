// A relational source and its dependent write script share the ordinary
// transaction overlay, read witnesses, rollback guard and execution allowance.

impl WriteTxn {
    /// Read the complete source once, bind every occurrence, and execute one
    /// sequential atomic program. Duplicate MERGE keys observe earlier creates;
    /// source rows and source-derived operands never change between steps.
    ///
    /// Source admission uses the canonical transaction reader (including prior
    /// staged effects and negative-read witnesses). The expanded statement cap
    /// bounds source output BEFORE materialization, without adding LIMIT or
    /// truncating the query. Source reads, argument retention and the write
    /// program spend one policy. A late bind, execution or final-checkpoint
    /// failure restores the original staged prefix but retains observations.
    /// Identity allocations are external and are never reclaimed on rollback.
    ///
    /// No intermediate commit, retry or public savepoint occurs. The receipt
    /// remains transaction-local until the caller finishes the transaction.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // one terminal report retaining native causes
    pub fn execute_graph_query_write_governed<V: Vfs + Clone, A>(
        &mut self,
        database: &mut Database<V>,
        cx: &fgdb_types::QueryCx,
        query: &fgdb_gql::query_write::PreparedGraphQueryWrite,
        policy: fgdb_gql::GraphWriteProgramPolicy,
        max_statements: usize,
        mut allocate: impl FnMut(fgdb_gql::GraphWriteIdentityRequest) -> Result<ElementId, A>,
    ) -> Result<
        fgdb_gql::query_write::GraphQueryWriteExecution,
        fgdb_gql::query_write::GraphQueryWriteExecutionError<
            WriteTxnError, A, Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::query_write::GraphQueryWriteExecutionError as Error;
        // Ownership/lifecycle errors precede even a zero allowance or an
        // already-cancelled query, as on the ordinary native program adapter.
        self.ensure_database(database).map_err(Error::Preflight)?;
        cx.with_restriction(|| {
            let workspace = MutationProgramWorkspace::new(self);
            let execution = {
                // The shared executor invokes read and stage sequentially.
                // Both callbacks borrow the SAME private transaction and
                // database; no second overlay, copied snapshot or writer lives
                // in the composition layer. Each borrow ends in its callback.
                let host = std::cell::RefCell::new((&mut *workspace.txn, database));
                query.execute_governed(
                    policy,
                    max_statements,
                    |source, allowance| {
                        let host = host.borrow();
                        host.0.execute_graph_set_governed(&*host.1, cx, source, allowance)
                    },
                    |batch, allowance| {
                        let mut host = host.borrow_mut();
                        let (txn, database) = &mut *host;
                        txn.execute_bound_graph_write_script_batch_governed(
                            database, cx, batch, allowance, &mut allocate,
                        )
                    },
                    || cx.checkpoint(),
                )
            }?;
            // execute_governed's final checkpoint is AFTER the inner program
            // accepted. This outer guard still covers a late interruption,
            // statistics refusal or unwind, preserving the original prefix.
            workspace.accept();
            Ok(execution)
        })
    }
}

#[cfg(test)]
mod query_write_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::query_write::{
        GraphQueryWriteExecutionError, PreparedGraphQueryWrite,
    };
    use fgdb_gql::{
        GqlParameterType, GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind,
        GraphWriteProgramPolicy, PreparedGraphSet, PreparedGraphSetText, PreparedGraphWriteScript,
    };
    use fgdb_types::{CanonicalScalarKind, DatabaseSecurityNamespaceId, PurposeContexts};

    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Label, "City") => Some(GraphSymbol::Label(LabelId(2))),
            (GraphSymbolKind::Label, "Missing") => Some(GraphSymbol::Label(LabelId(3))),
            (GraphSymbolKind::Property, "bornIn") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Property, "seen") => Some(GraphSymbol::Property(PropertyKeyId(3))),
            _ => None,
        }
    }
    fn read(text: &str) -> PreparedGraphSet {
        PreparedGraphSetText::prepare(text, resolve).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap()
    }
    fn query(source: &str, write: &str, declarations: &[(&str, GqlParameterType)])
        -> PreparedGraphQueryWrite
    {
        let mut resolver = resolve;
        let script = PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
            write, RelationId(1), declarations, &mut resolver,
        ).unwrap();
        PreparedGraphQueryWrite::prepare(read(source), script).unwrap()
    }
    fn policy() -> GraphWriteProgramPolicy {
        GraphWriteProgramPolicy::new(GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000),
            10_000, 10_000, 10_000)
    }
    fn text(value: &str) -> CanonicalScalar {
        CanonicalScalar::ucs_basic_text(value).unwrap()
    }
    fn keys() -> crate::DatabaseKeys {
        crate::DatabaseKeys::new([0x71; 32], DatabaseSecurityNamespaceId([0x72; 32]), [0x73; 32])
    }

    #[test]
    fn match_rows_merge_duplicate_cities_with_per_row_create_and_match_actions() {
        let ((), report) = run_async_under_lab(0x71c1, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for (index, city) in ["Paris", "Rome", "Paris"].into_iter().enumerate() {
                seed.create_vertex(VId(index as u128 + 1), vec![LabelId(1)],
                    vec![(PropertyKeyId(1), text(city))]);
            }
            db.write(&commit, seed).await.unwrap();
            let prepared = query(
                "MATCH (p:Person) RETURN p.bornIn AS city",
                "MERGE (c:City {name:$city}) ON CREATE SET c.seen=1 ON MATCH SET c.seen=c.seen+1",
                &[("city", GqlParameterType::Scalar(CanonicalScalarKind::of(&text("Paris"))))],
            );
            let mut txn = db.begin(&txcx).unwrap();
            let mut allocations = Vec::new();
            let receipt = txn.execute_graph_query_write_governed(
                &mut db, &cx, &prepared, policy(), 3, |request| {
                    allocations.push(request);
                    Ok::<_, ()>(ElementId::Vertex(VId(100 + allocations.len() as u128)))
                },
            ).unwrap();
            assert_eq!(receipt.input_rows, 3);
            assert_eq!(receipt.receipt.as_ref().unwrap().steps().len(), 3);
            assert_eq!(allocations.len(), 2);
            let cities = txn.execute_graph_set_governed(&db, &cx,
                &read("MATCH (c:City) RETURN c.name AS city, c.seen AS seen ORDER BY city"),
                policy().mutations.query).unwrap().value;
            assert_eq!(cities.len(), 2);
            assert_eq!(cities[0].values(), &[
                GraphValue::Scalar(text("Paris")), GraphValue::Scalar(CanonicalScalar::Int(2)),
            ]);
            assert_eq!(cities[1].values(), &[
                GraphValue::Scalar(text("Rome")), GraphValue::Scalar(CanonicalScalar::Int(1)),
            ]);
            txn.finish(&mut db, &commit).await.unwrap();
            assert_eq!(db.execute_graph_set_governed(&cx,
                &read("MATCH (c:City) RETURN c.name AS city, c.seen AS seen ORDER BY city"),
                policy().mutations.query).unwrap().value, cities);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn source_is_not_rerun_when_writes_expand_its_match_domain() {
        let ((), report) = run_async_under_lab(0x71c2, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for vid in 1..=3 {
                seed.create_vertex(VId(vid), vec![LabelId(1)],
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(vid as i64))]);
            }
            db.write(&commit, seed).await.unwrap();
            let prepared = query("MATCH (p:Person) RETURN p.bornIn AS city",
                "CREATE (p:Person {bornIn:$city})", &[]);
            let mut txn = db.begin(&txcx).unwrap();
            let mut allocations = 0;
            let receipt = txn.execute_graph_query_write_governed(
                &mut db, &cx, &prepared, policy(), 3, |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(100 + allocations)))
                },
            ).unwrap();
            assert_eq!(receipt.input_rows, 3);
            assert_eq!(allocations, 3);
            assert_eq!(txn.execute_graph_set_governed(&db, &cx,
                &read("MATCH (p:Person) RETURN p"), policy().mutations.query).unwrap().value.len(), 6);
            txn.finish(&mut db, &commit).await.unwrap();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn late_execution_failure_restores_target_and_keeps_an_earlier_staged_prefix() {
        let ((), report) = run_async_under_lab(0x71c3, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(1), vec![LabelId(2)], vec![
                (PropertyKeyId(2), CanonicalScalar::Int(1)),
                (PropertyKeyId(3), CanonicalScalar::Int(10)),
            ]);
            db.write(&commit, seed).await.unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let prefix = PreparedGraphWriteScript::prepare("CREATE (p:Person)", RelationId(1), resolve).unwrap();
            txn.execute_graph_write_script_governed(&mut db, &cx, &prefix, &GqlParameters::new(),
                policy(), |_| Ok::<_, ()>(ElementId::Vertex(VId(99)))).unwrap();
            let prepared = query(
                "UNWIND [2,0] AS denominator RETURN 1 AS key, denominator ORDER BY denominator DESC",
                "MERGE (c:City {name:$key}) ON MATCH SET c.seen=c.seen / $denominator", &[],
            );
            let error = txn.execute_graph_query_write_governed(
                &mut db, &cx, &prepared, policy(), 2, |_| Err::<ElementId, _>(()),
            ).unwrap_err();
            assert!(matches!(error, GraphQueryWriteExecutionError::Program(_)));
            let target = txn.execute_graph_set_governed(&db, &cx,
                &read("MATCH (c:City) RETURN c.seen AS seen"), policy().mutations.query).unwrap().value;
            assert_eq!(target.len(), 1);
            assert_eq!(target[0].values(), &[GraphValue::Scalar(CanonicalScalar::Int(10))]);
            assert_eq!(txn.execute_graph_set_governed(&db, &cx,
                &read("MATCH (p:Person) RETURN p"), policy().mutations.query).unwrap().value.len(), 1);
            txn.finish(&mut db, &commit).await.unwrap();
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn null_keys_refuse_before_allocation_and_empty_source_closes_read_only() {
        let ((), report) = run_async_under_lab(0x71c4, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let prepared = query("UNWIND [1,NULL] AS key RETURN key",
                "MERGE (c:City {name:$key})",
                &[("key", GqlParameterType::Scalar(CanonicalScalarKind::of(&CanonicalScalar::Int(1))))]);
            let mut txn = db.begin(&txcx).unwrap();
            let mut allocations = 0;
            assert!(txn.execute_graph_query_write_governed(
                &mut db, &cx, &prepared, policy(), 2, |_| {
                    allocations += 1;
                    Ok::<_, ()>(ElementId::Vertex(VId(100 + allocations)))
                },
            ).is_err());
            assert_eq!(allocations, 0);
            let empty = query("MATCH (p:Missing) RETURN 1 AS key",
                "MERGE (c:City {name:$key})", &[]);
            let receipt = txn.execute_graph_query_write_governed(
                &mut db, &cx, &empty, policy(), 0, |_| Err::<ElementId, _>(()),
            ).unwrap();
            assert!(receipt.receipt.is_none());
            assert_eq!(receipt.input_rows, 0);
            assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::ReadClosed { .. }));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn binding_cannot_refresh_work_already_spent_by_the_source() {
        let ((), report) = run_async_under_lab(0x71c5, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let prepared = query("RETURN 1 AS key", "CREATE (c:City {name:$key})", &[]);
            let mut txn = db.begin(&txcx).unwrap();
            let source = txn.execute_graph_set_governed(&db, &cx, prepared.source(),
                policy().mutations.query).unwrap();
            let mut tight = policy();
            tight.mutations.query.evaluator.max_work_units = source.evaluator.work_units;
            let error = txn.execute_graph_query_write_governed(
                &mut db, &cx, &prepared, tight, 1, |_| Err::<ElementId, _>(()),
            ).unwrap_err();
            assert!(matches!(error, GraphQueryWriteExecutionError::Binding(
                fgdb_gql::query_write::GraphQueryWriteBindError::Interrupted {
                    source: fgdb_gql::GqlQueryError::Evaluator(_), ..
                }
            )));
            assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(),
                EmbeddedTxnCompletion::ReadClosed { .. }));
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
