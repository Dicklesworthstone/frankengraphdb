//! Filterless owner counts retain native history, resource and output laws.

use asupersync::{CancelKind, lab::run_async_under_lab};
use fgdb::{
    Database, DatabaseKeys, EmbeddedReadView, GqlError, MemVfs, PreparedNativeRead, QueryError,
    QueryResult, QueryValue, ReadError, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphSymbol, GraphSymbolKind, PreparedGraphAggregate, PreparedGraphAggregateText,
};
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    )
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 100, 1_000_000, 100_000)
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Relation, "T") => Some(GraphSymbol::Relation(RelationId(3))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn aggregate(text: &str) -> PreparedGraphAggregate {
    PreparedGraphAggregateText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn count_result(total: u64) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["total".to_owned()],
        rows: vec![vec![QueryValue::Count(total)]],
    }
}

async fn history(db: &mut Database<MemVfs>, cx: &CommitCx) -> EmbeddedReadView {
    let payload = CanonicalScalar::ucs_basic_text(&"property payload ".repeat(256)).unwrap();
    let mut first = WriteBatch::new(R);
    for id in [1, 2, 3, u128::MAX - 1] {
        first.create_vertex(VId(id), vec![], vec![(P, payload.clone())]);
    }
    for (eid, source, destination) in [(10, 1, 2), (11, 1, 2), (12, 2, 2), (13, 3, 1)] {
        first.add_edge(
            EId(u128::MAX - eid),
            VId(source),
            VId(destination),
            vec![(P, payload.clone())],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let first_view = db.read_session().unwrap();
    let mut second = WriteBatch::new(S);
    second.add_edge(EId(20), VId(1), VId(2), vec![(P, payload)]);
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    let mut third = WriteBatch::new(R);
    third.delete_edge(EId(u128::MAX - 10));
    third.set_edge_property(EId(u128::MAX - 11), P, Some(CanonicalScalar::Bool(true)));
    third.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
    third.delete_vertex(VId(u128::MAX - 1));
    third.create_vertex(VId(1 << 127), vec![], vec![]);
    assert_eq!(db.write(cx, third).await.unwrap(), CommitSeq(3));
    first_view
}

#[test]
fn owner_and_pinned_native_counts_keep_history_and_live_multiplicity() {
    let ((), report) = run_async_under_lab(0xc017_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let first_view = history(&mut db, &contexts.commit()).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();

        // Each entry is an independently counted live fixture: isolated
        // vertices count once; parallel edges and self-loops remain occurrences.
        for (cut, vertices, r_edges, s_edges) in
            [(0, 0, 0, 0), (1, 4, 4, 0), (2, 4, 4, 1), (3, 4, 3, 1)]
        {
            for (pattern, expected, records) in [
                ("(n)", vertices, vertices),
                ("(a)-[:R]->(b)", r_edges, r_edges + s_edges),
                ("(a)<-[:R]-(b)", r_edges, r_edges + s_edges),
                ("(a)-[r:S]->(b)", s_edges, r_edges + s_edges),
                ("(a)-[:T]->(b)", 0, r_edges + s_edges),
            ] {
                let text = format!(
                    "MATCH {pattern} FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS total"
                );
                assert_eq!(
                    db.query(&contexts.query(), &text, &params, symbols, policy())
                        .unwrap(),
                    count_result(expected),
                    "{text}"
                );
                let (result, selected) = view
                    .query_with_snapshot(&contexts.query(), &text, &params, symbols, policy())
                    .unwrap();
                assert_eq!(selected, CommitSeq(cut));
                assert_eq!(result, count_result(expected));
                let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
                assert_eq!(
                    prepared
                        .execute_in_view_with_snapshot(&view, &contexts.query(), &params, policy())
                        .unwrap(),
                    (count_result(expected), CommitSeq(cut))
                );

                let query = aggregate(&format!("MATCH {pattern} RETURN COUNT(*) AS total"));
                assert!(query.count_summary_source().is_some());
                let owned = db
                    .execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(cut),
                        policy(),
                    )
                    .unwrap();
                let pinned = view
                    .execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(cut),
                        policy(),
                    )
                    .unwrap();
                assert_eq!(owned, pinned);
                assert_eq!(owned.rows.snapshot_records, records);
                assert_eq!(owned.value[0].get(0).unwrap().as_count(), Some(expected));
            }
        }

        db.compact(&contexts.commit()).await.unwrap();
        for cut in 0..=3 {
            let text = format!(
                "MATCH (a)-[:R]->(b) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS total"
            );
            assert_eq!(
                db.query(&contexts.query(), &text, &params, symbols, policy())
                    .unwrap(),
                count_result([0, 4, 4, 3][cut as usize])
            );
        }
        drop(db);
        let text = "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total";
        assert_eq!(
            first_view
                .query(&contexts.query(), text, &params, symbols, policy())
                .unwrap(),
            count_result(4)
        );
        assert_eq!(
            view.query(&contexts.query(), text, &params, symbols, policy())
                .unwrap(),
            count_result(3)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_count_limits_and_output_expressions_keep_the_native_finalizer() {
    let ((), report) = run_async_under_lab(0xc017_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let _ = history(&mut db, &contexts.commit()).await;
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n) RETURN COUNT(*) AS total",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total,COUNT(*) AS again",
            "MATCH (a)-[:T]->(b) RETURN COUNT(*) AS total",
            "MATCH (n) RETURN COUNT(*) + 1 AS total ORDER BY total DESC LIMIT 1",
            "MATCH (n) RETURN COUNT(*) AS total HAVING total < 0",
            "MATCH (a)-[:R]->(b) RETURN COUNT(*) AS total SKIP 1 LIMIT 0",
        ] {
            let query = aggregate(text);
            assert!(query.count_summary_source().is_some(), "{text}");
            let full = db
                .execute_graph_aggregate_governed(&contexts.query(), &query, policy())
                .unwrap();
            let exact = GqlQueryPolicy::new(
                full.rows.snapshot_records,
                full.rows.result_rows,
                full.evaluator.work_units,
                full.evaluator.scratch_entries,
            );
            assert_eq!(
                db.execute_graph_aggregate_governed(&contexts.query(), &query, exact)
                    .unwrap(),
                full
            );
            assert_eq!(
                view.execute_graph_aggregate_governed(&contexts.query(), &query, exact)
                    .unwrap(),
                full
            );
            for cap in [
                GqlQueryPolicy::new(u64::MAX, u64::MAX, full.evaluator.work_units - 1, u64::MAX),
                GqlQueryPolicy::new(
                    u64::MAX,
                    u64::MAX,
                    u64::MAX,
                    full.evaluator.scratch_entries - 1,
                ),
            ] {
                assert!(matches!(
                    db.execute_graph_aggregate_governed(&contexts.query(), &query, cap),
                    Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1
                ));
                assert!(matches!(
                    view.execute_graph_aggregate_governed(&contexts.query(), &query, cap),
                    Err(GqlQueryError::Evaluator(error)) if error.observed == u128::from(error.limit) + 1
                ));
            }
            let records = full.rows.snapshot_records;
            assert!(matches!(
                db.execute_graph_aggregate_governed(&contexts.query(), &query, GqlQueryPolicy::new(records - 1, u64::MAX, u64::MAX, u64::MAX)),
                Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::SnapshotRecords && error.observed == records
            ));
            if full.rows.result_rows > 0 {
                assert!(matches!(
                    db.execute_graph_aggregate_governed(&contexts.query(), &query, GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX)),
                    Err(GqlQueryError::Rows(error)) if error.dimension == GqlBudgetDimension::ResultRows && error.observed == 1
                ));
            }
        }

        let bad = "MATCH (n) RETURN 1 / (COUNT(*) - COUNT(*)) AS total LIMIT 0";
        let query = aggregate(bad);
        assert!(query.count_summary_source().is_some());
        assert!(matches!(
            db.execute_graph_aggregate_governed(&contexts.query(), &query, policy()),
            Err(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            ))
        ));
        assert!(matches!(
            db.query(
                &contexts.query(),
                bad,
                &GqlParameters::new(),
                symbols,
                policy()
            ),
            Err(QueryError::Aggregate(GqlQueryError::Source(
                GraphAggregateError::OutputExpression { .. }
            )))
        ));
        let filtered = "MATCH (n) WHERE n.p = 9 RETURN COUNT(*) AS total";
        assert!(aggregate(filtered).count_summary_source().is_none());
        assert_eq!(
            db.query(
                &contexts.query(),
                filtered,
                &GqlParameters::new(),
                symbols,
                policy()
            )
            .unwrap(),
            count_result(1)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_count_history_fences_precede_cancellation_and_empty_page_limits() {
    let ((), report) = run_async_under_lab(0xc017_0013, |root| async move {
        let mut handle = root
            .spawn(|root| async move {
                let contexts = PurposeContexts::narrow_runtime_root(&root);
                let mut db = Database::open_memory(&contexts.commit(), keys())
                    .await
                    .unwrap();
                let _ = history(&mut db, &contexts.commit()).await;
                let view = db.read_session().unwrap();
                let query = aggregate("MATCH (n) RETURN COUNT(*) AS total LIMIT 0");
                let refused = GqlQueryPolicy::new(0, 0, 0, 0);
                root.cancel_with(CancelKind::User, Some("owner count cancellation law"));
                for result in [
                    db.execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(4),
                        refused,
                    ),
                    view.execute_graph_aggregate_governed_at(
                        &contexts.query(),
                        &query,
                        CommitSeq(4),
                        refused,
                    ),
                ] {
                    assert!(matches!(
                        result,
                        Err(GqlQueryError::Source(GraphAggregateError::Source(
                            GqlError::Read(ReadError::BeyondFrontier { .. })
                        )))
                    ));
                }
                for result in [
                    db.execute_graph_aggregate_governed(&contexts.query(), &query, refused),
                    view.execute_graph_aggregate_governed(&contexts.query(), &query, refused),
                ] {
                    assert!(matches!(result, Err(GqlQueryError::Interrupted(_))));
                }
            })
            .unwrap();
        assert_eq!(handle.join(&root).await, Ok(()));
        assert!(root.checkpoint().is_ok());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
