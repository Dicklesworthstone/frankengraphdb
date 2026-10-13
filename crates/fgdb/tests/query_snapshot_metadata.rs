//! Successful eager reads report the cut used by their native bound plan.
//! No mock history or second evaluator supplies the expected row identities.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue, WriteBatch,
};
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn symbols(_: GraphSymbolKind, _: &str) -> Option<GraphSymbol> {
    None
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100, 100, 1_000_000, 1_000_000)
}

async fn history(contexts: &PurposeContexts) -> Database<MemVfs> {
    let mut db = Database::open_memory(&contexts.commit(), keys())
        .await
        .unwrap();
    for id in 1..=2 {
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(id), vec![], vec![]);
        db.write(&contexts.commit(), batch).await.unwrap();
    }
    assert_eq!(db.frontier().unwrap(), CommitSeq(2));
    db
}

fn rows(result: QueryResult) -> Vec<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read returned a write receipt");
    };
    rows
}

fn identities(count: u64) -> Vec<Vec<QueryValue>> {
    (1..=count)
        .map(|id| vec![QueryValue::Value(GraphValue::Vertex(VId(u128::from(id))))])
        .collect()
}

#[test]
fn historical_pattern_and_aggregate_results_carry_their_actual_cut_including_zero() {
    let ((), report) = run_async_under_lab(0x5e90_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = history(&contexts).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        for cut in 0..=2 {
            let pattern = format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN n ORDER BY n");
            let (result, seq) = view
                .query_with_snapshot(&contexts.query(), &pattern, &params, symbols, policy())
                .unwrap();
            assert_eq!(seq, CommitSeq(cut));
            assert_eq!(rows(result), identities(cut));
            let aggregate =
                format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS total");
            let (result, seq) = view
                .query_with_snapshot(&contexts.query(), &aggregate, &params, symbols, policy())
                .unwrap();
            assert_eq!(seq, CommitSeq(cut));
            assert_eq!(rows(result), vec![vec![QueryValue::Count(cut)]]);
        }
        let (empty, seq) = view
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n LIMIT 0",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        assert!(rows(empty).is_empty());
        assert_eq!(seq, CommitSeq(1));
        assert_eq!(view.frontier(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compound_and_pipeline_reads_preserve_the_selected_snapshot_and_multiplicity() {
    let ((), report) = run_async_under_lab(0x5e90_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = history(&contexts).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        for cut in 0..=2 {
            let text = format!(
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN n AS node \
                 UNION ALL MATCH (m) RETURN m AS node ORDER BY node"
            );
            let (result, seq) = view
                .query_with_snapshot(&contexts.query(), &text, &params, symbols, policy())
                .unwrap();
            let expected: Vec<_> = identities(cut)
                .into_iter()
                .flat_map(|row| [row.clone(), row])
                .collect();
            assert_eq!(seq, CommitSeq(cut));
            assert_eq!(rows(result), expected);
            let pipeline = format!(
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {cut} WITH n RETURN COUNT(*) AS total"
            );
            let (result, seq) = view
                .query_with_snapshot(&contexts.query(), &pipeline, &params, symbols, policy())
                .unwrap();
            assert_eq!(seq, CommitSeq(cut));
            assert_eq!(rows(result), vec![vec![QueryValue::Count(cut)]]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_temporal_rebinding_changes_rows_and_metadata_together() {
    let ((), report) = run_async_under_lab(0x5e90_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = history(&contexts).await;
        let view = db.read_session().unwrap();
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n ORDER BY n";
        let first = GqlParameters::new().with_uint64("cut", 1).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &first, symbols).unwrap();
        for cut in [1, 2, 0, 1] {
            let params = GqlParameters::new().with_uint64("cut", cut).unwrap();
            let (result, seq) = prepared
                .execute_in_view_with_snapshot(&view, &contexts.query(), &params, policy())
                .unwrap();
            assert_eq!(seq, CommitSeq(cut));
            assert_eq!(rows(result.clone()), identities(cut));
            assert_eq!(
                result,
                prepared
                    .execute_in_view(&view, &contexts.query(), &params, policy())
                    .unwrap()
            );
        }
        assert_eq!(prepared.statement(), text);
        assert_eq!(view.frontier(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn plain_reads_and_explain_keep_the_retained_view_frontier_after_writer_advances() {
    let ((), report) = run_async_under_lab(0x5e90_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut db = history(&contexts).await;
        let retained = db.read_session().unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(VId(3), vec![], vec![]);
        db.write(&contexts.commit(), batch).await.unwrap();
        let params = GqlParameters::new();
        for text in [
            "MATCH (n) RETURN n ORDER BY n",
            "MATCH (n) WITH n RETURN n ORDER BY n",
        ] {
            let (result, seq) = retained
                .query_with_snapshot(&contexts.query(), text, &params, symbols, policy())
                .unwrap();
            assert_eq!(seq, CommitSeq(2));
            assert_eq!(rows(result), identities(2));
        }
        let text = "MATCH (n) WITH n RETURN COUNT(*) AS total";
        let (result, seq) = retained
            .query_with_snapshot(&contexts.query(), text, &params, symbols, policy())
            .unwrap();
        assert_eq!(seq, CommitSeq(2));
        assert_eq!(rows(result), vec![vec![QueryValue::Count(2)]]);
        let text = "EXPLAIN MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n";
        let (result, seq) = retained
            .query_with_snapshot(&contexts.query(), text, &params, symbols, policy())
            .unwrap();
        assert_eq!(
            seq,
            CommitSeq(2),
            "EXPLAIN lists the retained view's template"
        );
        assert_eq!(
            result,
            retained
                .query(&contexts.query(), text, &params, symbols, policy())
                .unwrap()
        );
        assert!(!rows(result).is_empty());
        let current = db.read_session().unwrap();
        let (result, seq) = current
            .query_with_snapshot(
                &contexts.query(),
                "MATCH (n) RETURN n ORDER BY n",
                &params,
                symbols,
                policy(),
            )
            .unwrap();
        assert_eq!(seq, CommitSeq(3));
        assert_eq!(rows(result), identities(3));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn refusals_never_return_successful_snapshot_metadata_or_hide_behind_empty_pages() {
    let ((), report) = run_async_under_lab(0x5e90_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = history(&contexts).await;
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        for text in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 3 RETURN n",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 3 RETURN n LIMIT 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 3 RETURN COUNT(*) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 3 RETURN n AS node UNION ALL MATCH (m) RETURN m AS node LIMIT 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $missing RETURN n",
            "CREATE (n)",
        ] {
            assert!(
                view.query_with_snapshot(&contexts.query(), text, &params, symbols, policy(),)
                    .is_err(),
                "{text}"
            );
        }
        assert!(
            view.query_with_snapshot(
                &contexts.query(),
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n",
                &params,
                symbols,
                GqlQueryPolicy::new(100, 100, 0, 1_000_000),
            )
            .is_err()
        );
        assert_eq!(view.frontier(), CommitSeq(2));
        assert_eq!(db.frontier().unwrap(), CommitSeq(2));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
