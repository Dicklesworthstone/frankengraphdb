//! Native readers must stay on the view's immutable generation, not the writer.
//! All data is published through the real lab/MemVfs Chronicle/Strata path.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, GqlError, NativeReadClass, PreparedNativeRead, QueryError, QueryResult,
    ReadError, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateValue,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::cell::Cell;

const PROPERTY: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
        _ => None,
    }
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
}

fn batch(vid: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(vid),
        vec![],
        vec![(PROPERTY, CanonicalScalar::Int(value))],
    );
    batch
}

fn integers(values: &[i64]) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["p".to_owned()],
        rows: values
            .iter()
            .map(|value| {
                vec![GraphAggregateValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(*value),
                ))]
            })
            .collect(),
    }
}

fn cases() -> [(&'static str, NativeReadClass); 7] {
    [
        (
            "MATCH (n) RETURN n.p AS p ORDER BY p",
            NativeReadClass::Pattern,
        ),
        (
            "MATCH (n) RETURN COUNT(*) AS total, SUM(n.p) AS amount",
            NativeReadClass::Aggregate,
        ),
        (
            "MATCH (n) WITH n.p AS p RETURN COUNT(*) AS total, SUM(p) AS amount",
            NativeReadClass::PipelineAggregate,
        ),
        (
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            NativeReadClass::Set,
        ),
        (
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p ORDER BY p",
            NativeReadClass::TemporalPattern,
        ),
        (
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS total, SUM(n.p) AS amount",
            NativeReadClass::TemporalAggregate,
        ),
        (
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            NativeReadClass::TemporalSet,
        ),
    ]
}

#[test]
fn all_native_classes_remain_generation_exact_after_writes_and_writer_drop() {
    let ((), report) = run_async_under_lab(0x7669_6501, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        assert_eq!(db.write(&commit, batch(1, 4)).await.unwrap(), CommitSeq(1));
        let basis = db.write(&commit, batch(2, 7)).await.unwrap();
        let view = db.read_session().unwrap();
        let clone = view.clone();
        assert!(view.shares_decoded_state_with(&clone));
        let root_before = view.partition_root();
        let manifest_before = view.manifest();
        let params = GqlParameters::new();
        let cases = cases();
        let mut plans = Vec::new();
        let mut expected = Vec::new();
        for (text, class) in &cases {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            assert_eq!(prepared.facade_class(), *class, "{text}");
            let result = db.query(&cx, text, &params, symbols, policy()).unwrap();
            assert_eq!(
                view.query(&cx, text, &params, symbols, policy()).unwrap(),
                result
            );
            assert_eq!(
                prepared
                    .execute_in_view(&clone, &cx, &params, policy())
                    .unwrap(),
                result
            );
            plans.push(prepared);
            expected.push(result);
        }
        assert_eq!(expected[0], integers(&[4, 7]));
        assert_eq!(expected[4], integers(&[4]));

        let advanced = db.write(&commit, batch(3, 11)).await.unwrap();
        assert!(advanced > basis);
        let fresh = db.read_session().unwrap();
        assert!(!view.shares_decoded_state_with(&fresh));
        for (index, (text, _)) in cases.iter().enumerate() {
            let result = plans[index]
                .execute_in_view(&fresh, &cx, &params, policy())
                .unwrap();
            assert_eq!(
                result,
                db.query(&cx, text, &params, symbols, policy()).unwrap()
            );
            if index < 4 {
                assert_ne!(
                    result, expected[index],
                    "the successor really changes this answer"
                );
            } else {
                assert_eq!(
                    result, expected[index],
                    "temporal reads select the older cut"
                );
            }
        }
        drop(db);
        drop(view);
        for (index, (text, _)) in cases.iter().enumerate() {
            assert_eq!(
                clone.query(&cx, text, &params, symbols, policy()).unwrap(),
                expected[index],
                "{text}",
            );
            assert_eq!(
                plans[index]
                    .execute_in_view(&clone, &cx, &params, policy())
                    .unwrap(),
                expected[index],
            );
        }
        assert_eq!(clone.frontier(), basis);
        assert_eq!(clone.partition_root(), root_before);
        assert_eq!(clone.manifest(), manifest_before);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_native_reads_rebind_parameters_without_reopening_or_resolving() {
    let ((), report) = run_async_under_lab(0x7669_6502, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4)).await.unwrap();
        db.write(&commit, batch(2, 11)).await.unwrap();
        let view = db.read_session().unwrap();
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 8).unwrap();
        let calls = Cell::new(0);
        let text = "MATCH (n) WHERE n.p > $floor RETURN n.p AS p ORDER BY p";
        let plan = PreparedNativeRead::prepare(text, &low, |kind: GraphSymbolKind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        })
        .unwrap();
        let prepared_calls = calls.get();
        assert!(prepared_calls > 0);
        db.write(&commit, batch(3, 23)).await.unwrap();
        drop(db);
        for _ in 0..3 {
            assert_eq!(
                plan.execute_in_view(&view, &cx, &low, policy()).unwrap(),
                integers(&[4, 11])
            );
            assert_eq!(
                plan.execute_in_view(&view, &cx, &high, policy()).unwrap(),
                integers(&[11])
            );
        }
        assert!(matches!(
            plan.execute_in_view(
                &view,
                &cx,
                &GqlParameters::new(),
                GqlQueryPolicy::new(0, 0, 0, 0)
            ),
            Err(QueryError::PatternText(_)),
        ));
        assert_eq!(calls.get(), prepared_calls);
        assert_eq!(
            view.query(&cx, text, &low, symbols, policy()).unwrap(),
            integers(&[4, 11])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn future_history_cannot_escape_the_view_even_with_zero_budget_or_empty_output() {
    let ((), report) = run_async_under_lab(0x7669_6503, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4)).await.unwrap();
        let view = db.read_session().unwrap();
        assert_eq!(db.write(&commit, batch(2, 11)).await.unwrap(), CommitSeq(2));
        let params = GqlParameters::new();
        let future = [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p LIMIT 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN COUNT(*) AS total",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n.p AS p EXCEPT MATCH (m) RETURN m.p AS p",
        ];
        for (index, text) in future.iter().enumerate() {
            // The live reader can serve the cut. The old view must not borrow it,
            // even when a query's result would have been empty anyway.
            db.query(&cx, text, &params, symbols, policy()).unwrap();
            let plan = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            for allowance in [policy(), GqlQueryPolicy::new(0, 0, 0, 0)] {
                let error = plan
                    .execute_in_view(&view, &cx, &params, allowance)
                    .unwrap_err();
                let source = match (index, error) {
                    (0, QueryError::Pattern(GqlQueryError::Source(GqlError::Read(error)))) => error,
                    (
                        1,
                        QueryError::Aggregate(GqlQueryError::Source(GraphAggregateError::Source(
                            GqlError::Read(error),
                        ))),
                    ) => error,
                    (
                        2,
                        QueryError::Set(GqlQueryError::Source(GraphSetExecutionError::Source(
                            GqlError::Read(error),
                        ))),
                    ) => error,
                    (_, error) => panic!("history refusal was masked: {error:?}"),
                };
                assert!(matches!(source, ReadError::BeyondFrontier {
                    asked: CommitSeq(2), frontier,
                } if frontier == basis));
            }
        }
        assert_eq!(
            view.query(&cx, "MATCH (n) RETURN n.p AS p", &params, symbols, policy())
                .unwrap(),
            integers(&[4])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_output_refusals_leave_the_retained_generation_reusable() {
    let ((), report) = run_async_under_lab(0x7669_6504, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4)).await.unwrap();
        let view = db.read_session().unwrap();
        let identity = (view.frontier(), view.manifest(), view.partition_root());
        let params = GqlParameters::new();
        let no_output = GqlQueryPolicy::new(10_000, 0, 1_000_000, 1_000_000);
        for (text, _) in cases() {
            let expected = view.query(&cx, text, &params, symbols, policy()).unwrap();
            let error = view
                .query(&cx, text, &params, symbols, no_output)
                .unwrap_err();
            assert!(matches!(
                error,
                QueryError::Pattern(GqlQueryError::Rows(_))
                    | QueryError::Aggregate(GqlQueryError::Rows(_))
                    | QueryError::Set(GqlQueryError::Rows(_))
            ));
            assert_eq!(
                view.query(&cx, text, &params, symbols, policy()).unwrap(),
                expected
            );
        }
        assert_eq!(
            (view.frontier(), view.manifest(), view.partition_root()),
            identity
        );
        assert_eq!(db.frontier().unwrap(), view.frontier());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn collected_values(
    columns: Vec<String>,
    rows: Vec<fgdb_gql::algebra::GraphValueRow>,
) -> QueryResult {
    QueryResult::Rows {
        columns,
        rows: rows
            .into_iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(GraphAggregateValue::Value)
                    .collect()
            })
            .collect(),
    }
}

fn projected_property(row: &fgdb_gql::algebra::GraphValueRow) -> i64 {
    match &row.values()[1] {
        GraphValue::Scalar(CanonicalScalar::Int(value)) => *value,
        other => panic!("unexpected property cell: {other:?}"),
    }
}

#[test]
fn native_pull_queries_match_eager_rows_without_borrowing_the_writer_or_template() {
    use fgdb_gql::stream::VertexScanState;
    let ((), report) = run_async_under_lab(0x7669_6505, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for (vid, value) in [(0, 4), (2, 11), (9, 7), (u128::MAX, 23)] {
            db.write(&commit, batch(vid, value)).await.unwrap();
        }
        let params = GqlParameters::new();
        let queries = [
            "MATCH (n) RETURN n AS id, n.p AS p",
            "MATCH (n) WHERE n.p > 5 RETURN DISTINCT n AS id, n.p AS p SKIP 1 LIMIT 2",
            "MATCH (n) WHERE n.p > 10 OR n.p = 4 RETURN n AS id, n.p AS p",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS id, n.p AS p",
        ];
        let expected_properties: [&[i64]; 4] = [&[4, 11, 7, 23], &[7, 23], &[4, 11, 23], &[4]];
        for (index, text) in queries.iter().enumerate() {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let expected = prepared.execute(&db, &cx, &params, policy()).unwrap();
            let (columns, mut cursor) = prepared.stream(&db, &cx, &params, policy()).unwrap();
            assert_eq!(columns, vec!["id", "p"]);
            assert_eq!(cursor.row_stats().snapshot_records, 0, "open is not a scan");
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            assert_eq!(cursor.evaluator_stats().scratch_entries, 0);
            let mut rows = Vec::new();
            // Deliberately consume in different caller-chosen page sizes. A
            // page is not a new query or a new allowance.
            for page_size in [1, 2, 8] {
                for _ in 0..page_size {
                    match cursor.next() {
                        Some(row) => rows.push(row.unwrap()),
                        None => break,
                    }
                }
            }
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
            assert_eq!(
                rows.iter().map(projected_property).collect::<Vec<_>>(),
                expected_properties[index].to_vec()
            );
            assert_eq!(collected_values(columns, rows), expected);
        }

        let params = GqlParameters::new().with_int64("floor", 5).unwrap();
        let text = String::from("MATCH (n) WHERE n.p > $floor RETURN n AS id, n.p AS p");
        let prepared = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
        let expected = prepared.execute(&db, &cx, &params, policy()).unwrap();
        let (columns, mut cursor) = prepared.stream(&db, &cx, &params, policy()).unwrap();
        let basis = cursor.snapshot_seq();
        drop(prepared);
        drop(text);
        drop(params);
        db.write(&commit, batch(10, 99)).await.unwrap();
        assert_ne!(db.frontier().unwrap(), basis);
        drop(db);
        let rows: Vec<_> = cursor.by_ref().collect::<Result<_, _>>().unwrap();
        assert_eq!(collected_values(columns, rows), expected);
        assert_eq!(cursor.snapshot_seq(), basis);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_pull_close_limits_and_late_errors_preserve_cumulative_accounting() {
    use fgdb_gql::GqlBudgetDimension;
    use fgdb_gql::stream::VertexScanState;
    let ((), report) = run_async_under_lab(0x7669_6506, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for vid in 1..=4 {
            db.write(&commit, batch(vid, vid as i64)).await.unwrap();
        }
        let params = GqlParameters::new();
        let prepared =
            PreparedNativeRead::prepare("MATCH (n) RETURN n AS id, n.p AS p", &params, symbols)
                .unwrap();
        let view = db.read_session().unwrap();
        let (_, mut cursor) = prepared
            .stream_in_view(
                &view,
                &cx,
                &params,
                GqlQueryPolicy::new(10_000, 1, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert_eq!(projected_property(&cursor.next().unwrap().unwrap()), 1);
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(
            matches!(cursor.next(), Some(Err(GqlQueryError::Rows(error)))
            if error.dimension == GqlBudgetDimension::ResultRows)
        );
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(
            cursor.row_stats().result_rows,
            1,
            "failed row never escapes"
        );
        let rows = cursor.row_stats();
        let work = cursor.evaluator_stats();
        cursor.close();
        assert!(cursor.next().is_none());
        assert!(cursor.next().is_none());
        assert_eq!(cursor.row_stats(), rows);
        assert_eq!(cursor.evaluator_stats(), work);
        assert_eq!(cursor.state(), VertexScanState::Failed);

        let (_, mut cursor) = prepared
            .stream_in_view(
                &view,
                &cx,
                &params,
                GqlQueryPolicy::new(2, 10, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert!(cursor.next().unwrap().is_ok());
        assert!(cursor.next().unwrap().is_ok());
        assert!(
            matches!(cursor.next(), Some(Err(GqlQueryError::Rows(error)))
            if error.dimension == GqlBudgetDimension::SnapshotRecords)
        );
        assert_eq!(cursor.row_stats().snapshot_records, 2);
        assert_eq!(cursor.row_stats().result_rows, 2);
        assert_eq!(cursor.state(), VertexScanState::Failed);

        let (_, mut cursor) = prepared
            .stream_in_view(&view, &cx, &params, policy())
            .unwrap();
        assert!(cursor.next().unwrap().is_ok());
        let rows = cursor.row_stats();
        let work = cursor.evaluator_stats();
        cursor.close();
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
        assert_eq!(cursor.row_stats(), rows);
        assert_eq!(
            cursor.evaluator_stats(),
            work,
            "close cannot drain the suffix"
        );

        for limit in [0, 1] {
            let text = format!("MATCH (n) RETURN n AS id, n.p AS p LIMIT {limit}");
            let plan = PreparedNativeRead::prepare(&text, &params, symbols).unwrap();
            let (_, mut cursor) = plan
                .stream_in_view(
                    &view,
                    &cx,
                    &params,
                    GqlQueryPolicy::new(limit, limit, 1_000_000, 1_000_000),
                )
                .unwrap();
            for _ in 0..limit {
                assert!(cursor.next().unwrap().is_ok());
            }
            assert!(cursor.next().is_none());
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert_eq!(cursor.row_stats().snapshot_records, limit);
            assert_eq!(cursor.row_stats().result_rows, limit);
        }
        // The original view remains usable after every cursor terminal state.
        let (_, mut fresh) = prepared
            .stream_in_view(&view, &cx, &params, policy())
            .unwrap();
        assert_eq!(
            fresh.by_ref().collect::<Result<Vec<_>, _>>().unwrap().len(),
            4
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_pull_refuses_nonstreamable_classes_and_operators_without_eager_fallback() {
    use fgdb_gql::stream::VertexScanError;
    let ((), report) = run_async_under_lab(0x7669_6507, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, batch(1, 4)).await.unwrap();
        let view = db.read_session().unwrap();
        let params = GqlParameters::new();
        for (text, class) in cases() {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            prepared
                .execute_in_view(&view, &cx, &params, policy())
                .unwrap();
            let error = prepared
                .stream_in_view(&view, &cx, &params, policy())
                .unwrap_err();
            if matches!(
                class,
                NativeReadClass::Pattern | NativeReadClass::TemporalPattern
            ) {
                // These particular cases project only a property: canonical
                // order requires sorting, which the bounded stream cannot do.
                assert!(matches!(
                    error,
                    QueryError::Stream(GqlQueryError::Source(VertexScanError::Plan(_)))
                ));
            } else {
                assert!(matches!(error, QueryError::StreamingUnsupported { facade }
                    if facade == class));
            }
        }
        assert_eq!(
            view.query(&cx, "MATCH (n) RETURN n.p AS p", &params, symbols, policy())
                .unwrap(),
            integers(&[4])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_pull_rebinds_arguments_and_refuses_future_history_before_zero_limits() {
    use fgdb_gql::stream::VertexScanError;
    let ((), report) = run_async_under_lab(0x7669_6508, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, batch(1, 4)).await.unwrap();
        let view = db.read_session().unwrap();
        db.write(&commit, batch(2, 11)).await.unwrap();
        let params = GqlParameters::new();
        let future = PreparedNativeRead::prepare(
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n AS id, n.p AS p LIMIT 0",
            &params,
            symbols,
        )
        .unwrap();
        assert!(
            future
                .stream(&db, &cx, &params, policy())
                .unwrap()
                .1
                .next()
                .is_none()
        );
        assert!(matches!(
            future.stream_in_view(&view, &cx, &params, GqlQueryPolicy::new(0, 0, 0, 0)),
            Err(QueryError::Stream(GqlQueryError::Source(VertexScanError::Source(
                ReadError::BeyondFrontier { asked: CommitSeq(2), frontier },
            )))) if frontier == basis
        ));
        let low = GqlParameters::new().with_int64("floor", 3).unwrap();
        let high = GqlParameters::new().with_int64("floor", 9).unwrap();
        let calls = Cell::new(0);
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n) WHERE n.p > $floor RETURN n AS id, n.p AS p",
            &low,
            |kind: GraphSymbolKind, name: &str| {
                calls.set(calls.get() + 1);
                symbols(kind, name)
            },
        )
        .unwrap();
        let resolved = calls.get();
        let (_, mut lower) = prepared.stream_in_view(&view, &cx, &low, policy()).unwrap();
        let (_, mut higher) = prepared
            .stream_in_view(&view, &cx, &high, policy())
            .unwrap();
        assert!(matches!(
            prepared.stream_in_view(&view, &cx, &GqlParameters::new(), policy()),
            Err(QueryError::PatternText(_))
        ));
        drop(prepared);
        drop(low);
        drop(high);
        drop(view);
        drop(db);
        assert_eq!(projected_property(&lower.next().unwrap().unwrap()), 4);
        assert!(lower.next().is_none());
        assert!(higher.next().is_none());
        assert_eq!(
            calls.get(),
            resolved,
            "pulling cannot re-enter the resolver"
        );
        assert_eq!(lower.snapshot_seq(), basis);
        assert_eq!(higher.snapshot_seq(), basis);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
