// Autocommit relationship upsert is begin -> atomic MERGE+branch actions -> finish.

impl<V: Vfs + Clone> Database<V> {
    pub async fn execute_graph_edge_upsert_autocommit_governed<A>(
        &mut self,
        txcx: &TxnCx,
        query_cx: &fgdb_types::QueryCx,
        commit_cx: &CommitCx,
        upsert: &fgdb_gql::PreparedGraphEdgeUpsert,
        policy: fgdb_gql::GraphEdgeUpsertPolicy,
        allocate: impl FnMut(fgdb_gql::GraphEdgeMergeRequest) -> Result<ElementId, A>,
    ) -> Result<
        (
            fgdb_gql::GraphEdgeUpsertStats,
            fgdb_gql::GraphEdgeMergeOutcome,
            EmbeddedTxnCompletion,
        ),
        fgdb_gql::GqlQueryError<
            fgdb_gql::GraphEdgeUpsertError<WriteTxnError, A>,
            Box<asupersync::error::Error>,
        >,
    > {
        use fgdb_gql::{GqlQueryError, GraphEdgeUpsertError};
        let infrastructure = |error| GqlQueryError::Source(GraphEdgeUpsertError::Staging(error));
        let mut transaction = self
            .begin(txcx)
            .map_err(|error| infrastructure(WriteTxnError::Write(error)))?;
        let (stats, outcome) = match transaction
            .execute_graph_edge_upsert_governed(self, query_cx, upsert, policy, allocate)
        {
            Ok(value) => value,
            Err(error) => {
                transaction.abort();
                return Err(error);
            }
        };
        let completion = transaction
            .finish(self, commit_cx)
            .await
            .map_err(infrastructure)?;
        Ok((stats, outcome, completion))
    }
}

#[cfg(test)]
mod native_edge_upsert_autocommit_return_tests {
    use super::*;
    use crate::{DatabaseKeys, MemVfs};
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::{PropertyKeyId, RelationId};
    use fgdb_gql::{
        GqlParameters, GqlQueryError, GqlQueryPolicy, GraphEdgeMergePolicy,
        GraphEdgeUpsertPolicy, GraphEdgeUpsertQueryError, GraphSymbol, GraphSymbolKind,
        PreparedGraphEdgeUpsertQueryText,
    };
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn keys() -> DatabaseKeys {
        DatabaseKeys::new(
            [0x51; 32],
            DatabaseSecurityNamespaceId([0x52; 32]),
            [0x53; 32],
        )
    }

    #[test]
    fn upsert_return_autocommit_aborts_failure_and_reopens_success() {
        let ((), report) = run_async_under_lab(0xe6e0_0003, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let cx = contexts.query();
            let txcx = contexts.txn();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let relation = RelationId(1);
            let property = PropertyKeyId(1);
            let symbols = |kind, name: &str| match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(relation)),
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(property)),
                _ => None,
            };
            let mut seed = WriteBatch::new(relation);
            seed.create_vertex(VId(1), vec![], vec![(property, CanonicalScalar::Int(1))]);
            seed.create_vertex(VId(2), vec![], vec![(property, CanonicalScalar::Int(2))]);
            db.write(&commit, seed).await.unwrap();
            let before = db.frontier().unwrap();
            let policy = GraphEdgeUpsertPolicy::new(
                GraphEdgeMergePolicy::new(GqlQueryPolicy::new(
                    1_000, 100, 5_000_000, 2_000_000,
                )),
                100,
            );
            let next = AtomicU64::new(100);
            let mut allocate = |_| -> Result<ElementId, ()> {
                Ok(ElementId::Edge(EId(u128::from(
                    next.fetch_add(1, Ordering::Relaxed),
                ))))
            };
            let failing = PreparedGraphEdgeUpsertQueryText::prepare(
                "MATCH (a),(b) WHERE a.p=1 AND b.p=2 \
                 MERGE (a)-[r:R]->(b) ON CREATE SET r.p=0 RETURN 10/r.p AS bad",
                relation,
                &symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let result = db
                .execute_graph_edge_upsert_query_autocommit_governed(
                    &txcx, &cx, &commit, &failing, policy, &mut allocate,
                )
                .await;
            assert!(matches!(
                result,
                Err(GqlQueryError::Source(GraphEdgeUpsertQueryError::Returning(_)))
            ));
            assert!(db.edges().unwrap().is_empty());
            assert_eq!(db.frontier().unwrap(), before);
            assert_eq!(txcx.outstanding_obligations(), 0);
            let successful = PreparedGraphEdgeUpsertQueryText::prepare(
                "MATCH (a),(b) WHERE a.p=1 AND b.p=2 \
                 MERGE (a)-[r:R {p:7}]-(b) ON CREATE SET r.p=r.p+1 RETURN r,r.p AS p",
                relation,
                &symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
            let (_, outcome, rows, _) = db
                .execute_graph_edge_upsert_query_autocommit_governed(
                    &txcx, &cx, &commit, &successful, policy, &mut allocate,
                )
                .await
                .unwrap();
            assert_eq!(outcome.edge(), Some(EId(101)));
            assert!(outcome.created());
            assert_eq!(rows.value.len(), 1);
            assert_eq!(db.frontier().unwrap().0, before.0 + 1);
            assert_eq!(txcx.outstanding_obligations(), 0);
            db.compact(&commit).await.unwrap();
            drop(db);
            let reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert_eq!(reopened.edges().unwrap().len(), 1);
            assert!(reopened.edge(EId(100)).unwrap().is_none());
            assert_eq!(
                reopened.edge(EId(101)).unwrap().unwrap().props,
                vec![(property, CanonicalScalar::Int(8).into())]
            );
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
