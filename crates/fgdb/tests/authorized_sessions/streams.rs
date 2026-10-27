//! Real lazy scans through the narrowed session; no privileged source escapes.
use super::*;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::stream::VertexScanState;
use fgdb_types::EId;

fn rows_result(columns: &[String], rows: Vec<GraphValueRow>) -> QueryResult {
    QueryResult::Rows {
        columns: columns.to_vec(),
        rows: rows
            .into_iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(QueryValue::Value)
                    .collect()
            })
            .collect(),
    }
}
fn expected_row(id: u128, p: i64) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        GraphValue::Vertex(VId(id)),
        GraphValue::Scalar(CanonicalScalar::Int(p)),
    ])
}

#[test]
fn temporal_projected_streams_share_masked_history_and_keep_the_session_pin() {
    let ((), report) = run_async_under_lab(0x5ec0_6111, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut \
                    RETURN n AS id, n.p AS p, n.hidden AS hidden, $tag AS tag";
        let args = |cut, tag| {
            GqlParameters::new()
                .with_uint64("cut", cut)
                .unwrap()
                .with_int64("tag", tag)
                .unwrap()
        };
        let expected = |id, p, tag| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(VId(id)),
                GraphValue::Scalar(CanonicalScalar::Int(p)),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Int(tag)),
            ])
        };
        let mut pinned = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = pinned.prepare(&cx, text, &args(1, 17)).unwrap();
        let mut cursor = pinned.stream_send(&cx, &prepared, &args(1, 17)).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        change.delete_vertex(VId(3));
        change.create_vertex(
            VId(4),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(31))],
        );
        db.write(&commit, change).await.unwrap();
        db.compact(&commit).await.unwrap();
        assert_eq!(cursor.snapshot_seq(), fgdb_types::CommitSeq(1));
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![expected(1, 7, 17), expected(3, 19, 17)]
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        drop(cursor);
        assert!(matches!(
            pinned.stream(&cx, &prepared, &args(2, 17)),
            Err(QueryError::Read(fgdb::ReadError::BeyondFrontier {
                asked: fgdb_types::CommitSeq(2),
                frontier: fgdb_types::CommitSeq(1),
            }))
        ));
        assert!(!pinned.is_closed());

        let mut current = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let template = current.prepare(&cx, text, &args(1, 17)).unwrap();
        for (cut, tag, expected) in [
            (0, 5, Vec::new()),
            (1, 23, vec![expected(1, 7, 23), expected(3, 19, 23)]),
            (2, 29, vec![expected(1, 70, 29), expected(4, 31, 29)]),
        ] {
            let params = args(cut, tag);
            let eager = current.execute(&cx, &template, &params).unwrap();
            let mut local = current.stream(&cx, &template, &params).unwrap();
            assert_eq!(local.snapshot_seq(), fgdb_types::CommitSeq(cut));
            let columns = local.columns().to_vec();
            let rows = local.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected);
            assert_eq!(rows_result(&columns, rows), eager);
            assert_eq!(local.state(), VertexScanState::Exhausted);
            drop(local);
            let mut send = current.stream_send(&cx, &template, &params).unwrap();
            assert_eq!(send.snapshot_seq(), fgdb_types::CommitSeq(cut));
            assert_eq!(
                send.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(send.state(), VertexScanState::Exhausted);
        }
        assert!(matches!(
            current.stream_send(&cx, &prepared, &args(1, 17)),
            Err(QueryError::Authorization(Error::WrongAuthority))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn authorized_equality_streams_seek_only_admitted_candidates_in_both_drivers() {
    let ((), report) = run_async_under_lab(0x5ec0_6101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 100..228_u128 {
            batch.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
            // Hidden equality candidates must not spend the native record
            // allowance or alter the signed visible-node charge.
            batch.create_vertex(
                VId(id + 1000),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
            );
        }
        batch.create_vertex(
            VId(u128::MAX),
            vec![LabelId(1)],
            vec![(
                PropertyKeyId(1),
                CanonicalScalar::Float(fgdb_types::CanonicalF64::new(7.0)),
            )],
        );
        db.write(&commit, batch).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new().with_int64("value", 7).unwrap();
        let text = "MATCH (n) WHERE n.p = $value RETURN n AS id";
        let eager = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap()
            .query(&cx, text, &args)
            .unwrap();
        let expected = vec![
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]),
            GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(u128::MAX))]),
        ];
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                GqlQueryPolicy::new(2, 2, 1_000_000, 1000),
                || 100,
            )
            .unwrap();
        let prepared = session.prepare(&cx, text, &args).unwrap();
        // Scoped, not dropped: the local cursor is not Send, and a binding
        // that outlives an await keeps the lab future from being Send.
        {
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            let columns = cursor.columns().to_vec();
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected);
            assert_eq!(rows_result(&columns, rows), eager);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
        }
        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&cursor);
        // Root keys are still unresolved, and must resolve against the pin
        // captured at opening rather than the writer's later index generation.
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(8)));
        change.delete_vertex(VId(u128::MAX));
        db.write(&commit, change).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            expected
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_equality_properties_cannot_change_stream_quota_refusals() {
    let ((), report) = run_async_under_lab(0x5ec0_6102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for hidden in [55, 99] {
            let mut db = Database::open_memory(&commit, DatabaseKeys::new([1; 32], NS, [2; 32]))
                .await
                .unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=2 {
                batch.create_vertex(
                    VId(id),
                    vec![LabelId(1)],
                    vec![(PropertyKeyId(2), CanonicalScalar::Int(hidden))],
                );
            }
            db.write(&commit, batch).await.unwrap();
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    GqlQueryPolicy::new(1, 10, 1_000_000, 1000),
                    || 100,
                )
                .unwrap();
            let args = GqlParameters::new();
            let prepared = session
                .prepare(&cx, "MATCH (n) WHERE n.hidden = 55 RETURN n AS id", &args)
                .unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            assert!(matches!(
                cursor.next(),
                Some(Err(QueryError::Stream(GqlQueryError::Rows(error))))
                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                        && error.limit == 1 && error.observed == 2
            ));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[path = "streams/ranges.rs"]
mod ranges;

#[test]
fn keyed_aggregate_streams_share_candidate_bounds_in_local_send_and_async_drivers() {
    let ((), report) = run_async_under_lab(0x5ec0_6103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 100..228_u128 {
            batch.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
            batch.create_vertex(
                VId(id + 1000),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
            );
        }
        batch.create_vertex(
            VId(u128::MAX),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        db.write(&commit, batch).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new().with_int64("key", 7).unwrap();
        let text = "MATCH (n) WHERE n.p = $key RETURN COUNT(*) AS total, SUM(n.p) AS sum";
        let eager = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap()
            .query(&cx, text, &args)
            .unwrap();
        let mut exact = grant();
        exact.limits.max_nodes = 2;
        let token = issuer.issue_at(&exact, 100).unwrap();
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                GqlQueryPolicy::new(2, 1, 1_000_000, 1000),
                || 100,
            )
            .unwrap();
        let prepared = session.prepare(&cx, text, &args).unwrap();
        for driver in 0..3 {
            let (columns, rows) = match driver {
                0 => {
                    let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
                    let columns = cursor.columns().to_vec();
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    (columns, rows)
                }
                1 => {
                    let mut cursor = session
                        .stream_aggregate_send(&cx, &prepared, &args)
                        .unwrap();
                    fn is_send<T: Send>(_: &T) {}
                    is_send(&cursor);
                    let columns = cursor.columns().to_vec();
                    let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    (columns, rows)
                }
                _ => {
                    let mut cursor = session
                        .stream_aggregate_async(&cx, &prepared, &args)
                        .unwrap();
                    fn is_send<T: Send>(_: &T) {}
                    is_send(&cursor);
                    let columns = cursor.columns().to_vec();
                    let mut rows = Vec::new();
                    while let Some(row) = cursor.next_row().await.unwrap() {
                        rows.push(row);
                    }
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                    (columns, rows)
                }
            };
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].values()[0].as_count(), Some(2));
            assert_eq!(rows[0].values()[1].as_integer(), Some(14));
            assert_eq!(
                QueryResult::Rows {
                    columns,
                    rows: rows.into_iter().map(|row| row.values().to_vec()).collect(),
                },
                eager,
                "driver {driver}",
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_aggregate_equality_keeps_the_same_quota_refusal_in_every_driver() {
    let ((), report) = run_async_under_lab(0x5ec0_6104, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for hidden in [55, 99] {
            let mut db = Database::open_memory(&commit, DatabaseKeys::new([1; 32], NS, [2; 32]))
                .await
                .unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=2 {
                batch.create_vertex(
                    VId(id),
                    vec![LabelId(1)],
                    vec![(PropertyKeyId(2), CanonicalScalar::Int(hidden))],
                );
            }
            db.write(&commit, batch).await.unwrap();
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    GqlQueryPolicy::new(1, 1, 1_000_000, 1000),
                    || 100,
                )
                .unwrap();
            let args = GqlParameters::new();
            let prepared = session
                .prepare(
                    &cx,
                    "MATCH (n) WHERE n.hidden = 55 RETURN COUNT(*) AS total",
                    &args,
                )
                .unwrap();
            for driver in 0..3 {
                let error = match driver {
                    0 => {
                        let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
                        let error = cursor.next().unwrap().unwrap_err();
                        assert_eq!(cursor.state(), VertexScanState::Failed);
                        assert!(cursor.next().is_none());
                        error
                    }
                    1 => {
                        let mut cursor = session
                            .stream_aggregate_send(&cx, &prepared, &args)
                            .unwrap();
                        let error = cursor.next().unwrap().unwrap_err();
                        assert_eq!(cursor.state(), VertexScanState::Failed);
                        assert!(cursor.next().is_none());
                        error
                    }
                    _ => {
                        let mut cursor = session
                            .stream_aggregate_async(&cx, &prepared, &args)
                            .unwrap();
                        let error = cursor.next_row().await.unwrap_err();
                        assert_eq!(cursor.state(), VertexScanState::Failed);
                        assert!(cursor.next_row().await.unwrap().is_none());
                        error
                    }
                };
                assert!(matches!(
                    error,
                    QueryError::AggregateStream(GqlQueryError::Rows(error))
                        if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                            && error.limit == 1 && error.observed == 2
                ));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// fgdb-7xqxb: a set whose canonical order is its one vertex scan's order
/// (the vertex first, then columns, constants and parameters) pulls through
/// that scan's native cursor, projecting each row exactly as the eager set
/// does. Every other set shape still refuses streaming; nothing falls back
/// to an eager result.
#[test]
fn a_vertex_set_with_constant_columns_pulls_through_its_scan() {
    let ((), report) = run_async_under_lab(0x5ec0_6011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        for text in [
            "MATCH (n) RETURN n AS id, $value AS value",
            "MATCH (n) RETURN n AS id, 7 AS seven, n.p AS p, $value AS value",
        ] {
            let parse = GqlParameters::new().with_int64("value", 1).unwrap();
            let prepared = session.prepare(&cx, text, &parse).unwrap();
            // One prepared set rebinds to each argument without reparsing.
            for value in [42, -3] {
                let args = GqlParameters::new().with_int64("value", value).unwrap();
                let eager = session.execute(&cx, &prepared, &args).unwrap();
                let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
                let columns = cursor.columns().to_vec();
                let streamed = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert!(!streamed.is_empty(), "{text}");
                assert!(streamed.iter().all(|row| {
                    row.values().last() == Some(&GraphValue::Scalar(CanonicalScalar::Int(value)))
                }));
                assert_eq!(rows_result(&columns, streamed), eager, "{text}");
                assert_eq!(cursor.state(), VertexScanState::Exhausted);
                assert!(cursor.next().is_none());
            }
        }
        let args = GqlParameters::new().with_int64("value", 1).unwrap();
        for text in [
            "MATCH (n) RETURN $value AS value, n AS id",
            "MATCH (n) RETURN DISTINCT n AS id, $value AS value",
            "MATCH (n) RETURN n AS id, $value AS value LIMIT 1",
            "MATCH (n) RETURN n AS id, $value AS value ORDER BY value",
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            assert!(
                matches!(
                    session.stream(&cx, &prepared, &args),
                    Err(QueryError::StreamingUnsupported { .. })
                ),
                "{text}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn streamed_predicates_and_projection_see_masked_fields_with_exact_order_and_pages() {
    let ((), report) = run_async_under_lab(0x5ec0_6001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let args = GqlParameters::new();
        for (text, expected) in [
            (
                "MATCH (n) WHERE n.hidden IS NULL OR n.p = 100 RETURN n AS id, n.p AS p",
                vec![expected_row(1, 7), expected_row(3, 19)],
            ),
            (
                "MATCH (n) WHERE NOT (n.hidden = 55) RETURN n AS id, n.p AS p",
                vec![],
            ),
            ("MATCH (n:H) RETURN n AS id, n.p AS p", vec![]),
            (
                "MATCH (n) RETURN n AS id, n.p AS p SKIP 1 LIMIT 1",
                vec![expected_row(3, 19)],
            ),
            ("MATCH (n) RETURN n AS id, n.p AS p LIMIT 0", vec![]),
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let eager = session.execute(&cx, &prepared, &args).unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            let columns = cursor.columns().to_vec();
            let streamed = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(streamed, expected);
            assert_eq!(rows_result(&columns, streamed), eager);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert_eq!(cursor.size_hint(), (0, Some(0)));
            assert!(cursor.next().is_none());
        }
        let prepared = session
            .prepare(
                &cx,
                "MATCH (n) RETURN n AS id, n.hidden AS hidden, n AS repeated",
                &args,
            )
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        let columns = cursor.columns().to_vec();
        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(
            rows_result(&columns, rows),
            result(
                &["id", "hidden", "repeated"],
                vec![
                    vec![
                        vertex(1),
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                        vertex(1)
                    ],
                    vec![
                        vertex(3),
                        QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                        vertex(3)
                    ],
                ]
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

// Keep the actual Send cursor alive across a write suspension. Its compile-time
// Send proof covers the retained permit/source/pin, while the old Cell clocks
// in the other laws continue to exercise the thread-local constructor.
#[test]
fn opening_and_limit_one_do_not_admit_the_unread_graph_or_retain_the_template() {
    let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    runtime.block_on(async {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = std::sync::atomic::AtomicI32::new(0);
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                GqlQueryPolicy::new(1, 10, 100_000, 100_000),
                || {
                    calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    100
                },
            )
            .unwrap();
        let args = GqlParameters::new();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p LIMIT 1", &args)
            .unwrap();
        // Eager admission sees more than one permitted vertex before the page.
        assert!(session.execute(&cx, &prepared, &args).is_err());
        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&cursor);
        drop(prepared); // Opening freezes the native physical definition.
        let at = cursor.snapshot_seq();
        let before_pause = calls.load(std::sync::atomic::Ordering::Relaxed);
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        db.write(&commit, change).await.unwrap();
        assert!(db.frontier().unwrap() > at);
        drop(db);
        drop(token);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            before_pause,
            "a paused pull cursor does no source/clock work"
        );
        assert_eq!(cursor.next().unwrap().unwrap(), expected_row(1, 7));
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        let before_close = calls.load(std::sync::atomic::Ordering::Relaxed);
        cursor.close();
        cursor.close();
        assert!(cursor.next().is_none());
        drop(cursor);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            before_close
        );
        assert!(!session.is_closed());
        // Closing before the first pull also needs no records, including with a
        // remaining source admission budget too small for the complete result.
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
        let before_close = calls.load(std::sync::atomic::Ordering::Relaxed);
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
        drop(cursor);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            before_close
        );
        assert!(!session.is_closed());
    });
}

#[test]
fn historical_winners_are_scoped_before_streaming_and_future_zero_pages_refuse() {
    let ((), report) = run_async_under_lab(0x5ec0_6003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let at = db.frontier().unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_label(VId(1), LabelId(1), false);
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(70)));
        db.write(&commit, change).await.unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        for (text, expected) in [
            (
                "MATCH (n) RETURN n AS id, n.p AS p".to_owned(),
                vec![expected_row(3, 19)],
            ),
            (
                format!(
                    "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {} RETURN n AS id, n.p AS p",
                    at.0
                ),
                vec![expected_row(1, 7), expected_row(3, 19)],
            ),
        ] {
            let prepared = session.prepare(&cx, &text, &GqlParameters::new()).unwrap();
            let mut cursor = session
                .stream(&cx, &prepared, &GqlParameters::new())
                .unwrap();
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            drop(cursor);
            let mut send = session
                .stream_send(&cx, &prepared, &GqlParameters::new())
                .unwrap();
            assert_eq!(
                send.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
        }
        let future = db.frontier().unwrap().0 + 1;
        let prepared = session
            .prepare(
                &cx,
                &format!("MATCH (n) FOR SYSTEM_TIME AS OF SEQ {future} RETURN n AS id LIMIT 0"),
                &GqlParameters::new(),
            )
            .unwrap();
        assert!(matches!(
            session.stream(&cx, &prepared, &GqlParameters::new()),
            Err(QueryError::Read(_))
        ));
        assert!(!session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_and_native_limits_span_all_pulls_and_errors_are_not_silent_eof() {
    let ((), report) = run_async_under_lab(0x5ec0_6004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let args = GqlParameters::new();
        for dimension in [LimitDimension::Rows, LimitDimension::Nodes] {
            let mut grant = grant();
            match dimension {
                LimitDimension::Rows => grant.limits.max_rows = 1,
                _ => grant.limits.max_nodes = 1,
            }
            let token = issuer.issue_at(&grant, 100).unwrap();
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap();
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
                .unwrap();
            let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), expected_row(1, 7));
            assert!(
                // ubs:ignore -- test assertion on a limit dimension, not secret material.
                matches!(cursor.next(), Some(Err(QueryError::Authorization(Error::LimitExceeded(actual)))) if actual == dimension)
            );
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            cursor.close();
            drop(cursor);
            assert!(
                !session.is_closed(),
                "per-execution limits do not revoke the entire session"
            );
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p LIMIT 1", &args)
                .unwrap();
            assert_eq!(
                session
                    .stream_send(&cx, &prepared, &args)
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                vec![expected_row(1, 7)]
            );
        }
        // fgdb-7xqxb: a root costs its node when admitted even if no record is
        // read, so an identity-only stream cannot bypass max_nodes either.
        let mut limited = grant();
        limited.limits.max_nodes = 1;
        let token = issuer.issue_at(&limited, 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
        assert!(cursor.next().unwrap().is_ok());
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Nodes
            ))))
        ));
        assert!(cursor.next().is_none());
        drop(cursor);
        // The aggregate cursors admit roots through the cooperative step
        // (next_vertex_step), which charges the same node. count(*) reads no
        // record either. Two roots are visible (VId 1 and 3). Under max_nodes
        // 1 the second refuses before any group is delivered, never a short
        // count. Under max_nodes 2 the count is exact.
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN count(*) AS n", &args)
            .unwrap();
        let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Error::LimitExceeded(
                LimitDimension::Nodes
            ))))
        ));
        assert!(cursor.next().is_none());
        drop(cursor);
        let mut exact = grant();
        exact.limits.max_nodes = 2;
        let token = issuer.issue_at(&exact, 100).unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN count(*) AS n", &args)
            .unwrap();
        let mut cursor = session.stream_aggregate(&cx, &prepared, &args).unwrap();
        let row = cursor.next().unwrap().unwrap();
        assert_eq!(row.get(0).and_then(|value| value.as_count()), Some(2));
        assert!(cursor.next().is_none());
        drop(cursor);
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                GqlQueryPolicy::new(1, 10, 100_000, 100_000),
                || 100,
            )
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
            .unwrap();
        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
        assert_eq!(cursor.next().unwrap().unwrap(), expected_row(1, 7));
        // Candidate id=2 is hidden. Its history still consumes native admission,
        // and that refusal must not become apparent end-of-stream.
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Stream(GqlQueryError::Rows(_))))
        ));
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_live_poll_clock_cut_is_terminal_and_preserves_only_successfully_delivered_prefixes() {
    let ((), report) = run_async_under_lab(0x5ec0_6005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let calls = Cell::new(0);
        let active = Cell::new(false);
        let stop = Cell::new(usize::MAX);
        let clock = || {
            if active.get() {
                calls.set(calls.get() + 1);
            }
            if active.get() && calls.get() == stop.get() {
                1000
            } else {
                100
            }
        };
        let total = {
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), clock)
                .unwrap();
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
                .unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            active.set(true);
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                vec![expected_row(1, 7), expected_row(3, 19)]
            );
            active.set(false);
            calls.get()
        };
        assert!(total > 0);
        for cut in 1..=total {
            calls.set(0);
            stop.set(cut);
            active.set(false);
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), clock)
                .unwrap();
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
                .unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            active.set(true);
            let mut prefix = Vec::new();
            loop {
                match cursor.next() {
                    Some(Ok(row)) => prefix.push(row),
                    Some(Err(QueryError::Authorization(Error::Expired))) => break,
                    other => panic!("expected expiry at cut {cut}, got {other:?}"),
                }
            }
            assert!([expected_row(1, 7), expected_row(3, 19)].starts_with(&prefix));
            assert_eq!(calls.get(), cut);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
            assert_eq!(calls.get(), cut);
            active.set(false);
            drop(cursor);
            assert!(session.is_closed());
            assert!(matches!(
                session.query(&cx, "RETURN 1 AS n", &args),
                Err(QueryError::Authorization(Error::ExecutionStopped))
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retirement_unwind_and_cross_statement_clock_rollback_release_session_access() {
    let ((), report) = run_async_under_lab(0x5ec0_6006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        assert!(cursor.next().unwrap().is_ok());
        issuer.retire();
        assert!(matches!(
            cursor.next(),
            Some(Err(QueryError::Authorization(Error::AuthorityRetired)))
        ));
        assert!(cursor.next().is_none());
        drop(cursor);
        assert!(session.is_closed());
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let panic_now = Cell::new(false);
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || {
                assert!(!panic_now.get(), "injected host clock unwind");
                100
            })
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        panic_now.set(true);
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cursor.next();
        }));
        assert!(stopped.is_err());
        panic_now.set(false);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
        cursor.close();
        drop(cursor);
        assert!(session.is_closed());
        let now = Cell::new(100);
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || {
                now.get()
            })
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        now.set(300);
        assert!(cursor.next().unwrap().is_ok());
        cursor.close();
        drop(cursor);
        assert!(!session.is_closed());
        now.set(200);
        assert!(matches!(
            session.query(&cx, "RETURN 1 AS n", &args),
            Err(QueryError::Authorization(Error::ClockWentBackwards))
        ));
        assert!(session.is_closed());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_shapes_and_foreign_prepared_handles_never_fall_back_to_privileged_reads() {
    let ((), report) = run_async_under_lab(0x5ec0_6007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let mut first = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        // A zero-record native allowance proves refusal happens without source
        // admission, rather than after executing a hidden eager fallback.
        let mut session = db
            .authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols,
                GqlQueryPolicy::new(0, 100, 100_000, 100_000),
                || 100,
            )
            .unwrap();
        let foreign = first
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        assert!(matches!(
            session.stream(&cx, &foreign, &args),
            Err(QueryError::Authorization(Error::WrongAuthority))
        ));
        for text in [
            "MATCH (n) RETURN n.p AS p LIMIT 0",
            "MATCH (n) RETURN n AS id ORDER BY id DESC LIMIT 0",
            "MATCH (n) RETURN COUNT(*) AS n",
            "RETURN 1 AS n",
            "MATCH (n) RETURN n AS id UNION ALL MATCH (m) RETURN m AS id",
        ] {
            let prepared = session.prepare(&cx, text, &args).unwrap();
            assert!(matches!(
                session.stream(&cx, &prepared, &args),
                Err(QueryError::Stream(GqlQueryError::Source(_)))
                    | Err(QueryError::StreamingUnsupported { .. })
            ));
            assert!(!session.is_closed());
        }
        // Independent probes are now admitted, but LIMIT 0 still opens no
        // candidate history under this session's zero-record native allowance.
        let probe = session
            .prepare(
                &cx,
                "MATCH (n) WHERE NOT EXISTS { MATCH (m) } RETURN n AS id LIMIT 0",
                &args,
            )
            .unwrap();
        assert!(session.stream(&cx, &probe, &args).unwrap().next().is_none());
        let params = GqlParameters::new().with_text("route", "main").unwrap();
        let prepared = session
            .prepare(
                &cx,
                "AT BRANCH $route MATCH (n) RETURN n AS id LIMIT 0",
                &params,
            )
            .unwrap();
        let other = GqlParameters::new().with_text("route", "other").unwrap();
        assert!(matches!(
            session.stream(&cx, &prepared, &other),
            Err(QueryError::Authorization(Error::ScopeDenied))
        ));
        assert!(
            session
                .stream(&cx, &prepared, &params)
                .unwrap()
                .next()
                .is_none()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_signed_work_is_cumulative_through_eof_not_refreshed_for_each_row() {
    let ((), report) = run_async_under_lab(0x5ec0_6008, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = database(&c.commit()).await;
        let issuer = authority();
        let args = GqlParameters::new();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let calls = Cell::new(0);
        let work = {
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || {
                    calls.set(calls.get() + 1);
                    100
                })
                .unwrap();
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
                .unwrap();
            let before = calls.get();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                vec![expected_row(1, 7), expected_row(3, 19)]
            );
            // Every work charge samples time once. The only other samples are
            // one opening time, two admitted-node charges, and three delivery
            // charges (two rows plus EOF). No internal usage is exposed.
            calls.get() - before - 1 - 2 - 3
        };
        assert!(work > 10);
        for limit in [work, work - 1] {
            let mut grant = grant();
            grant.limits.max_work = limit;
            let token = issuer.issue_at(&grant, 100).unwrap();
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap();
            let prepared = session
                .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
                .unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            assert_eq!(cursor.next().unwrap().unwrap(), expected_row(1, 7));
            assert_eq!(cursor.next().unwrap().unwrap(), expected_row(3, 19));
            // ubs:ignore -- test comparison of two limit values, not secret material.
            if limit == work {
                assert!(cursor.next().is_none());
            } else {
                assert!(matches!(
                    cursor.next(),
                    Some(Err(QueryError::Authorization(Error::LimitExceeded(
                        LimitDimension::Work
                    ))))
                ));
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert!(cursor.next().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn full_width_identities_and_opening_unwind_keep_source_position_and_lifetime_boundaries() {
    let ((), report) = run_async_under_lab(0x5ec0_6009, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let mut change = WriteBatch::new(RelationId(1));
        for id in [0, u128::MAX] {
            change.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
            );
        }
        db.write(&commit, change).await.unwrap();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id, n.p AS p", &args)
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        assert_eq!(
            cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
            vec![
                expected_row(0, 1),
                expected_row(1, 7),
                expected_row(3, 19),
                expected_row(u128::MAX, 1)
            ]
        );
        drop(cursor);
        let panic_now = Cell::new(false);
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || {
                assert!(!panic_now.get(), "injected stream-opening clock unwind");
                100
            })
            .unwrap();
        let prepared = session
            .prepare(&cx, "MATCH (n) RETURN n AS id", &args)
            .unwrap();
        panic_now.set(true);
        let stopped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _opened = session.stream(&cx, &prepared, &args);
        }));
        assert!(stopped.is_err());
        panic_now.set(false);
        assert!(session.is_closed());
        assert!(matches!(
            session.query(&cx, "RETURN 1 AS n", &args),
            Err(QueryError::Authorization(Error::ExecutionStopped))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Two visible vertices, optionally with everything the session grant hides:
/// a hidden label and property on a visible vertex, hidden vertices, and a
/// hidden update history.
async fn stream_database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([1; 32], NS, [2; 32]))
        .await
        .unwrap();
    let int = CanonicalScalar::Int;
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, p) in [(1_u128, 7), (3, 19)] {
        let mut labels = vec![LabelId(1)];
        let mut props = vec![(PropertyKeyId(1), int(p))];
        if hidden {
            labels.push(LabelId(99));
            props.push((PropertyKeyId(2), int(55)));
        }
        batch.create_vertex(VId(id), labels, props);
    }
    if hidden {
        for id in 100..130_u128 {
            batch.create_vertex(VId(id), vec![LabelId(99)], vec![(PropertyKeyId(2), int(1))]);
        }
    }
    for (eid, src, dst) in [(10_u128, 1_u128, 3_u128), (11, 3, 1)] {
        batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
    }
    if hidden {
        // Allowed relation, hidden far endpoint, in both directions.
        for id in 100..130_u128 {
            batch.add_edge(EId(1000 + id), VId(1), VId(id), vec![]);
            batch.add_edge(EId(2000 + id), VId(id), VId(3), vec![]);
        }
    }
    db.write(cx, batch).await.unwrap();
    if hidden {
        let mut other = WriteBatch::new(RelationId(2));
        for eid in 0..20_u128 {
            other.add_edge(
                EId(3000 + eid),
                VId(1 + 2 * (eid % 2)),
                VId(3 - 2 * (eid % 2)),
                vec![],
            );
        }
        db.write(cx, other).await.unwrap();
        for round in 0..5 {
            let mut update = WriteBatch::new(RelationId(1));
            for id in 100..130_u128 {
                update.set_vertex_property(VId(id), PropertyKeyId(2), Some(int(round)));
            }
            db.write(cx, update).await.unwrap();
        }
    }
    db
}

/// FG-INV-20 noninterference for the lazy session stream (fgdb-2qm4o). The
/// rows and the refusal point of every signed limit a holder may attenuate,
/// and of every native meter, must not move with hidden vertices: the stream
/// once counted "candidate histories, including invisible/forbidden
/// candidates", which let a holder count hidden vertices by bisecting a limit.
#[test]
fn streamed_limits_and_native_meters_cannot_count_hidden_vertices() {
    use fgdb_warden::{CapabilityToken, Restriction};
    // A signed limit restriction paired with the dimension its refusal reports.
    type SignedLimitCase = (fn(u64) -> Restriction, LimitDimension);
    // A native meter's name paired with a policy that sets that meter to `k`.
    type NativeMeterCase<'a> = (&'a str, fn(u64) -> GqlQueryPolicy);
    let ((), report) = run_async_under_lab(0x5ec0_6101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let visible_only = stream_database(&c.commit(), false).await;
        let with_hidden = stream_database(&c.commit(), true).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n) RETURN n AS id, n.p AS p",
            "MATCH (n:L) RETURN n AS id",
            "MATCH (n) RETURN n AS id LIMIT 1",
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n AS id",
            "MATCH (n) WHERE NOT EXISTS { MATCH (n)<-[:R]-(m:L) } RETURN n AS id",
        ] {
            fn stream_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
                match (kind, name) {
                    (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
                    (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
                    _ => symbols(kind, name),
                }
            }
            let run = |db: &Database<MemVfs>, token: &CapabilityToken, policy| {
                let mut session = db
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        token,
                        "main",
                        stream_symbols,
                        policy,
                        || 100,
                    )
                    .unwrap();
                let prepared = session.prepare(&cx, text, &args).unwrap();
                let cursor = session.stream(&cx, &prepared, &args);
                cursor.and_then(|cursor| cursor.collect::<Result<Vec<_>, _>>())
            };
            assert_eq!(
                run(&visible_only, &token, policy()).unwrap(),
                run(&with_hidden, &token, policy()).unwrap(),
                "{text}"
            );
            let threshold = |admits: &dyn Fn(u64) -> bool| {
                let (mut low, mut high) = (0_u64, 1_000_000_u64);
                assert!(admits(high));
                while low < high {
                    let middle = low + (high - low) / 2;
                    if admits(middle) {
                        high = middle;
                    } else {
                        low = middle + 1;
                    }
                }
                low
            };
            let signed: [SignedLimitCase; 3] = [
                (Restriction::MaxWork, LimitDimension::Work),
                (Restriction::MaxNodes, LimitDimension::Nodes),
                (Restriction::MaxRows, LimitDimension::Rows),
            ];
            for (limit, dimension) in signed {
                let at = |db: &Database<MemVfs>| {
                    threshold(
                        &|k| match run(db, &token.attenuate(limit(k)).unwrap(), policy()) {
                            Ok(_) => true,
                            Err(QueryError::Authorization(Error::LimitExceeded(actual)))
                                if actual == dimension =>
                            {
                                false
                            }
                            Err(other) => panic!("{text}: {dimension:?} {k}: {other:?}"),
                        },
                    )
                };
                assert_eq!(
                    at(&visible_only),
                    at(&with_hidden),
                    "{text}: signed {dimension:?}"
                );
            }
            let native: [NativeMeterCase<'_>; 3] = [
                ("snapshot records", |k| {
                    GqlQueryPolicy::new(k, 1000, 1_000_000, 1_000_000)
                }),
                ("work units", |k| {
                    GqlQueryPolicy::new(1000, 1000, k, 1_000_000)
                }),
                ("scratch entries", |k| {
                    GqlQueryPolicy::new(1000, 1000, 1_000_000, k)
                }),
            ];
            for (meter, policy_at) in native {
                let at = |db: &Database<MemVfs>| {
                    threshold(&|k| match run(db, &token, policy_at(k)) {
                        Ok(_) => true,
                        Err(QueryError::Stream(
                            GqlQueryError::Evaluator(_) | GqlQueryError::Rows(_),
                        )) => false,
                        Err(other) => panic!("{text}: {meter} {k}: {other:?}"),
                    })
                };
                assert_eq!(
                    at(&visible_only),
                    at(&with_hidden),
                    "{text}: native {meter}"
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// EXPLAIN through an authorized session (fgdb-ooiik). The listing comes from
/// the statement text and the session's catalog, so two databases differing
/// only in records hidden from the token answer byte-identically. The
/// certificate form, whose digest binds the snapshot sequence that hidden
/// commits advance, refuses with a public reason.
#[test]
fn authorized_explain_lists_the_text_plan_and_cannot_see_hidden_records() {
    let ((), report) = run_async_under_lab(0x5ec0_6102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let visible_only = stream_database(&c.commit(), false).await;
        let with_hidden = stream_database(&c.commit(), true).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        fn explain_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            match (kind, name) {
                (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
                _ => symbols(kind, name),
            }
        }
        let explain = |db: &Database<MemVfs>, text: &str| {
            db.authorized_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                explain_symbols,
                policy(),
                || 100,
            )
            .unwrap()
            .query(&cx, text, &args)
        };
        for text in [
            "MATCH (n) RETURN n AS id, n.p AS p",
            "MATCH (n:L) RETURN n AS id",
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n AS id",
        ] {
            let statement = format!("EXPLAIN {text}");
            let listed = explain(&visible_only, &statement).unwrap();
            assert_eq!(listed, explain(&with_hidden, &statement).unwrap(), "{text}");
            assert!(
                matches!(&listed, QueryResult::Rows { columns, rows }
                    if *columns == ["operator", "detail"] && !rows.is_empty()),
                "{text}"
            );
        }
        assert!(matches!(
            explain(
                &with_hidden,
                "EXPLAIN (CERTIFICATE) MATCH (n) RETURN n AS id"
            ),
            Err(QueryError::Unsupported { .. })
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Capability-scoped reads refuse openCypher id()/elementId() (fgdb-j687q,
/// owner ruling 2026-09-30): engine identities are not masked, so they would
/// reveal allocation order across scopes. Every authorized text entry point
/// refuses typed before preparation; the same word as an alias, a property
/// key or inside quoted text is not the function and still runs.
#[test]
fn authorized_reads_refuse_identity_functions_at_every_text_entry_point() {
    let ((), report) = run_async_under_lab(0x5ec0_6103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = stream_database(&c.commit(), false).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let session = || {
            db.authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap()
        };
        let denied = |result: Result<(), QueryError>| {
            matches!(result, Err(QueryError::Authorization(Error::ScopeDenied)))
        };
        for text in [
            "MATCH (n) RETURN id(n) AS i",
            "MATCH (n) RETURN elementId(n) AS e",
            "MATCH (n) RETURN ID /* split */ (n) AS i",
            "MATCH (n) WITH n RETURN n.p AS p ORDER BY id(n)",
            "EXPLAIN MATCH (n) RETURN id(n) AS i",
        ] {
            assert!(
                denied(session().query(&cx, text, &args).map(drop)),
                "query: {text}"
            );
            assert!(
                denied(session().prepare(&cx, text, &args).map(drop)),
                "prepare: {text}"
            );
            assert!(
                denied(
                    db.query_authorized(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        text,
                        &args,
                        symbols,
                        policy(),
                        || 100
                    )
                    .map(drop)
                ),
                "query_authorized: {text}"
            );
        }
        for text in [
            "MATCH (n) RETURN n AS id",
            "MATCH (n) RETURN 'id(n)' AS quoted",
        ] {
            assert!(session().query(&cx, text, &args).is_ok(), "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn range_and_prefix_streams_keep_scoped_quotas_across_capacity_and_type_fallbacks() {
    let ((), report) = run_async_under_lab(0x5ec0_6201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        for prefix in [false, true] {
            // Two roots fit. Hidden and stale distinct values cross the fixed
            // source capacity; a hidden nontext value is independently
            // UNKNOWN. Neither changes visible candidate cost.
            for population in [0_u128, 1, 70] {
                let mut db =
                    Database::open_memory(&commit, DatabaseKeys::new([1; 32], NS, [2; 32]))
                        .await
                        .unwrap();
                let make_value = |number: i64| {
                    if prefix {
                        CanonicalScalar::ucs_basic_text(&format!("ok-{number}")).unwrap()
                    } else {
                        CanonicalScalar::Int(number)
                    }
                };
                let outside = || {
                    if prefix {
                        CanonicalScalar::ucs_basic_text("outside").unwrap()
                    } else {
                        CanonicalScalar::Int(0)
                    }
                };
                let mut batch = WriteBatch::new(RelationId(1));
                batch.create_vertex(
                    VId(0),
                    vec![LabelId(1)],
                    vec![(PropertyKeyId(1), outside())],
                );
                for (id, value) in [(1, 20), (u128::MAX, 30)] {
                    batch.create_vertex(
                        VId(id),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), make_value(value))],
                    );
                }
                for id in 0..population {
                    batch.create_vertex(
                        VId(100 + id),
                        vec![LabelId(99)],
                        vec![(PropertyKeyId(1), make_value(200 + id as i64))],
                    );
                    batch.create_vertex(
                        VId(1000 + id),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), make_value(500 + id as i64))],
                    );
                }
                if prefix && population == 1 {
                    batch.create_vertex(
                        VId(999),
                        vec![LabelId(99)],
                        vec![(PropertyKeyId(1), CanonicalScalar::Int(0))],
                    );
                }
                db.write(&commit, batch).await.unwrap();
                if population != 0 {
                    let mut change = WriteBatch::new(RelationId(1));
                    for id in 0..population {
                        change.set_vertex_property(
                            VId(1000 + id),
                            PropertyKeyId(1),
                            Some(outside()),
                        );
                    }
                    db.write(&commit, change).await.unwrap();
                }
                let filter = if prefix {
                    "n.p STARTS WITH 'ok-'"
                } else {
                    "n.p >= 10 AND n.p < 1000"
                };
                let row_text = format!("MATCH (n) WHERE {filter} RETURN n AS id");
                let aggregate_text = format!("MATCH (n) WHERE {filter} RETURN COUNT(*) AS total");
                let args = GqlParameters::new();
                let broad_token = issuer.issue_at(&grant(), 100).unwrap();
                let eager = db
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        &broad_token,
                        "main",
                        symbols,
                        policy(),
                        || 100,
                    )
                    .unwrap()
                    .query(&cx, &row_text, &args)
                    .unwrap();
                let expected = vec![
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(1))]),
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(u128::MAX))]),
                ];
                let mut exact = grant();
                exact.limits.max_nodes = 2;
                let token = issuer.issue_at(&exact, 100).unwrap();
                let mut session = db
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols,
                        GqlQueryPolicy::new(2, 2, 1_000_000, 1000),
                        || 100,
                    )
                    .unwrap();
                let prepared = session.prepare(&cx, &row_text, &args).unwrap();
                for send in [false, true] {
                    let (columns, rows) = if send {
                        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
                        fn is_send<T: Send>(_: &T) {}
                        is_send(&cursor);
                        let columns = cursor.columns().to_vec();
                        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(cursor.state(), VertexScanState::Exhausted);
                        (columns, rows)
                    } else {
                        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
                        let columns = cursor.columns().to_vec();
                        let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                        assert_eq!(cursor.state(), VertexScanState::Exhausted);
                        (columns, rows)
                    };
                    assert_eq!(
                        rows, expected,
                        "prefix={prefix}, population={population}, send={send}"
                    );
                    assert_eq!(rows_result(&columns, rows), eager);
                }
                let prepared = session.prepare(&cx, &aggregate_text, &args).unwrap();
                for driver in 0..3 {
                    let rows = match driver {
                        0 => {
                            let mut cursor =
                                session.stream_aggregate(&cx, &prepared, &args).unwrap();
                            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                            assert_eq!(cursor.state(), VertexScanState::Exhausted);
                            rows
                        }
                        1 => {
                            let mut cursor = session
                                .stream_aggregate_send(&cx, &prepared, &args)
                                .unwrap();
                            fn is_send<T: Send>(_: &T) {}
                            is_send(&cursor);
                            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                            assert_eq!(cursor.state(), VertexScanState::Exhausted);
                            rows
                        }
                        _ => {
                            let mut cursor = session
                                .stream_aggregate_async(&cx, &prepared, &args)
                                .unwrap();
                            fn is_send<T: Send>(_: &T) {}
                            is_send(&cursor);
                            let mut rows = Vec::new();
                            while let Some(row) = cursor.next_row().await.unwrap() {
                                rows.push(row);
                            }
                            assert_eq!(cursor.state(), VertexScanState::Exhausted);
                            rows
                        }
                    };
                    assert_eq!(rows.len(), 1);
                    assert_eq!(
                        rows[0].values()[0].as_count(),
                        Some(2),
                        "prefix={prefix}, population={population}, driver={driver}"
                    );
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_range_and_prefix_keys_keep_the_same_native_record_refusal() {
    let ((), report) = run_async_under_lab(0x5ec0_6202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for prefix in [false, true] {
            for matches in [false, true] {
                let mut db =
                    Database::open_memory(&commit, DatabaseKeys::new([1; 32], NS, [2; 32]))
                        .await
                        .unwrap();
                let value = if prefix {
                    CanonicalScalar::ucs_basic_text(if matches { "Ad" } else { "other" }).unwrap()
                } else {
                    CanonicalScalar::Int(if matches { 55 } else { 99 })
                };
                let mut batch = WriteBatch::new(RelationId(1));
                for id in 1..=2 {
                    batch.create_vertex(
                        VId(id),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(2), value.clone())],
                    );
                }
                db.write(&commit, batch).await.unwrap();
                let filter = if prefix {
                    "n.hidden STARTS WITH 'Ad'"
                } else {
                    "n.hidden >= 50 AND n.hidden < 60"
                };
                let text = format!("MATCH (n) WHERE {filter} RETURN n AS id");
                let args = GqlParameters::new();
                let mut session = db
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols,
                        GqlQueryPolicy::new(1, 2, 1_000_000, 1000),
                        || 100,
                    )
                    .unwrap();
                let prepared = session.prepare(&cx, &text, &args).unwrap();
                for send in [false, true] {
                    let error = if send {
                        let mut cursor = session.stream_send(&cx, &prepared, &args).unwrap();
                        let error = cursor.next().unwrap().unwrap_err();
                        assert_eq!(cursor.state(), VertexScanState::Failed);
                        assert!(cursor.next().is_none());
                        error
                    } else {
                        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
                        let error = cursor.next().unwrap().unwrap_err();
                        assert_eq!(cursor.state(), VertexScanState::Failed);
                        assert!(cursor.next().is_none());
                        error
                    };
                    assert!(
                        matches!(error,
                            QueryError::Stream(GqlQueryError::Rows(error))
                                if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                                    && error.limit == 1 && error.observed == 2
                        ),
                        "prefix={prefix}, matches={matches}, send={send}"
                    );
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[path = "streams/prefix_types.rs"]
mod prefix_types;

#[path = "streams/membership.rs"]
mod membership;
