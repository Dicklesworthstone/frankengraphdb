// Relational pipelines use the EXISTING set-query entrypoints. Durable and
// pinned readers live in gql_exec/source/aggregation.rs; WriteTxn's reader is
// compiled inside query_source from aggregate_queries.rs. Defining additional
// inherent methods here gives all three public types duplicate methods.
// Keep this included file as a regression at the shared public API boundary,
// not a second source adapter, forwarding API, or alternate transaction reader.

#[cfg(test)]
mod relational_reader_owner_tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphSetExecutionError,
        GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn one_canonical_reader_serves_live_history_pinned_and_transaction_pipelines() {
        let ((), report) = run_async_under_lab(0x51ce_a601, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit(); let cx = contexts.query(); let txcx = contexts.txn();
            let keys = crate::DatabaseKeys::new([0xa6; 32], DatabaseSecurityNamespaceId([0xa7; 32]), [0xa8; 32]);
            let mut db = Database::open_memory(&commit, keys.clone()).await.unwrap();
            let other = Database::open_memory(&commit, keys).await.unwrap();
            let key = fgdb_delta_types::PropertyKeyId(1);
            let mut batch = WriteBatch::new(RelationId(1));
            batch.create_vertex(VId(1), vec![], vec![(key, CanonicalScalar::Int(4))]);
            batch.create_vertex(VId(2), vec![], vec![(key, CanonicalScalar::Int(7))]);
            let basis = db.write(&commit, batch).await.unwrap();
            let query = PreparedGraphSetText::prepare(
                "MATCH (n) WITH n.p AS x WHERE x > 0 RETURN x", |kind, name| {
                    if kind == GraphSymbolKind::Property && name == "p" {
                        Some(GraphSymbol::Property(key))
                    } else { None }
                },
            ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
            let policy = GqlQueryPolicy::new(100, 100, 100_000, 100_000);
            let zero = GqlQueryPolicy::new(0, 0, 0, 0);
            let pinned = db.read_session().unwrap();
            let mut txn = db.begin(&txcx).unwrap();
            let expected = db.execute_graph_set_governed(&cx, &query, policy).unwrap().value;
            assert_eq!(expected.len(), 2);
            assert_eq!(db.execute_graph_set_governed_at(&cx, &query, basis, policy).unwrap().value, expected);
            assert_eq!(pinned.execute_graph_set_governed(&cx, &query, policy).unwrap().value, expected);
            assert_eq!(pinned.execute_graph_set_governed_at(&cx, &query, basis, policy).unwrap().value, expected);
            assert!(matches!(txn.execute_graph_set_governed(&other, &cx, &query, zero),
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(WriteTxnError::WrongDatabase)))));
            assert_eq!(txn.execute_graph_set_governed(&db, &cx, &query, policy).unwrap().value, expected);
            assert!(matches!(db.execute_graph_set_governed_at(&cx, &query, CommitSeq(basis.0 + 1), zero),
                Err(GqlQueryError::Source(GraphSetExecutionError::Source(
                    GqlError::Read(crate::ReadError::BeyondFrontier { .. }))))));
            assert!(matches!(txn.finish(&mut db, &commit).await.unwrap(), EmbeddedTxnCompletion::ReadClosed { .. }));
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(txcx.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
