use super::*;
use fgdb_types::CanonicalF64;
use fgdb_warden::{CapabilityToken, Restriction};

async fn range_database(cx: &CommitCx, hidden: bool) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([1; 32], NS, [2; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (vid, value) in [
        (VId(0), CanonicalScalar::Bool(false)),
        (VId(1), CanonicalScalar::Int(7)),
        (VId(3), CanonicalScalar::Int(20)),
        (VId(5), CanonicalScalar::Float(CanonicalF64::new(7.25))),
        (
            VId(u128::MAX),
            CanonicalScalar::Float(CanonicalF64::new(7.5)),
        ),
    ] {
        batch.create_vertex(vid, vec![LabelId(1)], vec![(PropertyKeyId(1), value)]);
    }
    if hidden {
        // Distinct hidden values push ONLY this database over the 64-head
        // threshold. Both databases must expose identical rows and quotas.
        for id in 0..80_u128 {
            batch.create_vertex(
                VId(100 + id),
                vec![LabelId(99)],
                vec![(
                    PropertyKeyId(1),
                    CanonicalScalar::Float(CanonicalF64::new(7.0 + id as f64 / 1000.0)),
                )],
            );
        }
    }
    db.write(cx, batch).await.unwrap();
    let mut update = WriteBatch::new(RelationId(1));
    update.set_vertex_property(VId(5), PropertyKeyId(1), Some(CanonicalScalar::Int(20)));
    db.write(cx, update).await.unwrap();
    db
}

#[test]
fn authorized_range_candidates_share_scope_and_mvcc_in_all_stream_drivers() {
    let ((), report) = run_async_under_lab(0x5ec0_6201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let broad_token = issuer.issue_at(&grant(), 100).unwrap();
        let mut exact = grant();
        exact.limits.max_nodes = 2;
        let token = issuer.issue_at(&exact, 100).unwrap();
        let args = GqlParameters::new();
        for hidden in [false, true] {
            let mut db = range_database(&commit, hidden).await;
            let text = "MATCH (n) WHERE n.p >= 7 AND n.p < 8 RETURN n AS id, n.p AS p";
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
                .query(&cx, text, &args)
                .unwrap();
            let expected = vec![
                expected_row(1, 7),
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(u128::MAX)),
                    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(7.5))),
                ]),
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
                let mut local = session.stream(&cx, &prepared, &args).unwrap();
                let columns = local.columns().to_vec();
                let rows = local.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
                assert_eq!(rows, expected);
                assert_eq!(rows_result(&columns, rows), eager);
                assert_eq!(local.state(), VertexScanState::Exhausted);
            }

            let aggregate = session
                .prepare(
                    &cx,
                    "MATCH (n) WHERE n.p >= 7 AND n.p < 8 RETURN COUNT(*) AS total",
                    &args,
                )
                .unwrap();
            for driver in 0..3 {
                let rows = match driver {
                    0 => session
                        .stream_aggregate(&cx, &aggregate, &args)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    1 => {
                        let cursor = session
                            .stream_aggregate_send(&cx, &aggregate, &args)
                            .unwrap();
                        fn is_send<T: Send>(_: &T) {}
                        is_send(&cursor);
                        cursor.collect::<Result<Vec<_>, _>>().unwrap()
                    }
                    _ => {
                        let mut cursor = session
                            .stream_aggregate_async(&cx, &aggregate, &args)
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
                assert_eq!(rows.len(), 1, "hidden={hidden}, driver={driver}");
                assert_eq!(rows[0].values()[0].as_count(), Some(2));
            }

            let mut sent = session.stream_send(&cx, &prepared, &args).unwrap();
            fn is_send<T: Send>(_: &T) {}
            is_send(&sent);
            // Neither range route has resolved its frontier. Its first pull
            // must still use the generation captured before these mutations.
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
            update.delete_vertex(VId(u128::MAX));
            db.write(&commit, update).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            assert_eq!(
                sent.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(sent.state(), VertexScanState::Exhausted);
            assert!(sent.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_range_values_cannot_change_native_or_signed_work_thresholds() {
    let ((), report) = run_async_under_lab(0x5ec0_6202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let visible = range_database(&c.commit(), false).await;
        let hidden = range_database(&c.commit(), true).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let text = "MATCH (n) WHERE n.p >= 7 AND n.p < 8 RETURN n AS id, n.p AS p";
        let run = |db: &Database<MemVfs>, token: &CapabilityToken, policy| {
            let mut session = db
                .authorized_read_session(&cx, &issuer, token, "main", symbols, policy, || 100)
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let cursor = session.stream(&cx, &prepared, &args);
            cursor.and_then(|cursor| cursor.collect::<Result<Vec<_>, _>>())
        };
        assert_eq!(
            run(&visible, &token, policy()).unwrap(),
            run(&hidden, &token, policy()).unwrap()
        );
        let threshold = |admits: &dyn Fn(u64) -> bool| {
            let (mut low, mut high) = (0, 1_000_000);
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
        for scratch in [false, true] {
            let at = |db: &Database<MemVfs>| {
                threshold(&|limit| {
                    let policy = if scratch {
                        GqlQueryPolicy::new(2, 2, 1_000_000, limit)
                    } else {
                        GqlQueryPolicy::new(2, 2, limit, 1000)
                    };
                    match run(db, &token, policy) {
                        Ok(_) => true,
                        Err(QueryError::Stream(GqlQueryError::Evaluator(_))) => false,
                        Err(other) => panic!("scratch={scratch}, limit={limit}: {other:?}"),
                    }
                })
            };
            assert_eq!(at(&visible), at(&hidden), "native scratch={scratch}");
        }
        let signed = |db: &Database<MemVfs>| {
            threshold(&|limit| match run(
                db,
                &token.attenuate(Restriction::MaxWork(limit)).unwrap(),
                policy(),
            ) {
                Ok(_) => true,
                Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Work))) => false,
                Err(other) => panic!("signed work={limit}: {other:?}"),
            })
        };
        assert_eq!(signed(&visible), signed(&hidden));

        // A later range may not remove the visible, nonnumeric first vertex
        // before an earlier arithmetic predicate reports its native data error.
        for db in [&visible, &hidden] {
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap();
            let prepared = session
                .prepare(
                    &cx,
                    "MATCH (n) WHERE 10 / n.p > 0 AND n.p >= 7 RETURN n AS id",
                    &args,
                )
                .unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            assert!(matches!(
                cursor.next(),
                Some(Err(QueryError::Stream(GqlQueryError::Data(_)))),
            ));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_range_keys_keep_identical_refusals_in_local_and_send_streams() {
    let ((), report) = run_async_under_lab(0x5ec0_6203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
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
            let prepared = session
                .prepare(
                    &cx,
                    "MATCH (n) WHERE n.hidden >= 50 AND n.hidden < 60 RETURN n AS id",
                    &args,
                )
                .unwrap();
            let mut local = session.stream(&cx, &prepared, &args).unwrap();
            assert!(matches!(
                local.next(),
                Some(Err(QueryError::Stream(GqlQueryError::Rows(error))))
                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                        && error.limit == 1 && error.observed == 2
            ));
            assert_eq!(local.state(), VertexScanState::Failed);
            assert!(local.next().is_none());
            drop(local);
            let mut sent = session.stream_send(&cx, &prepared, &args).unwrap();
            assert!(matches!(
                sent.next(),
                Some(Err(QueryError::Stream(GqlQueryError::Rows(error))))
                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                        && error.limit == 1 && error.observed == 2
            ));
            assert_eq!(sent.state(), VertexScanState::Failed);
            assert!(sent.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
