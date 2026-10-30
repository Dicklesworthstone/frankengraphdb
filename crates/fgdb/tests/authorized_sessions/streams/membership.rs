//! Indexed IN roots through the real authorized row and aggregate sessions.
use super::*;
use fgdb_types::{CanonicalF64, CanonicalList, CanonicalPropertyValue};
use fgdb_warden::{CapabilityToken, Restriction};

fn arguments() -> GqlParameters {
    GqlParameters::new()
        .with_list(
            "members",
            [
                CanonicalScalar::Int(7),
                CanonicalScalar::Float(CanonicalF64::new(7.0)),
                CanonicalScalar::Null,
                CanonicalScalar::Int(7),
            ]
            .into_iter()
            .chain((1000..1080).map(CanonicalScalar::Int))
            .map(GraphValue::Scalar)
            .collect(),
        )
        .unwrap()
}

fn collection() -> CanonicalPropertyValue {
    CanonicalPropertyValue::List(
        CanonicalList::try_new(vec![CanonicalScalar::Int(7).into()]).unwrap(),
    )
}

async fn membership_database(cx: &CommitCx, hidden: u8) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([1; 32], NS, [2; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (vid, value) in [
        (VId(0), CanonicalScalar::Int(0)),
        (VId(1), CanonicalScalar::Int(7)),
        (VId(3), CanonicalScalar::Float(CanonicalF64::new(7.0))),
        (VId(5), CanonicalScalar::Int(7)),
        (VId(u128::MAX), CanonicalScalar::Null),
    ] {
        batch.create_vertex(vid, vec![LabelId(1)], vec![(PropertyKeyId(1), value)]);
    }
    batch.create_vertex(VId(9), vec![LabelId(1)], vec![]);
    for id in 100..612 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(-1))],
        );
    }
    if hidden == 1 {
        // Only hidden values push this source beyond its 64 posting heads.
        for id in 1000..1080 {
            batch.create_vertex(
                VId(id),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
        }
    }
    if hidden >= 2 {
        // The scalar-only proof must fail even for an invisible collection.
        batch.create_vertex_values(
            VId(2000),
            vec![LabelId(99)],
            vec![(PropertyKeyId(1), collection())],
        );
    }
    db.write(cx, batch).await.unwrap();
    let mut update = WriteBatch::new(RelationId(1));
    update.set_vertex_property(VId(5), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
    if hidden == 3 {
        // A retired collection still forces fallback; its current scalar does
        // not make the historical domain scalar-only.
        update.set_vertex_property(VId(2000), PropertyKeyId(1), Some(CanonicalScalar::Int(0)));
    }
    db.write(cx, update).await.unwrap();
    db
}

fn expected() -> Vec<GraphValueRow> {
    vec![
        expected_row(1, 7),
        GraphValueRow::from_owned_values(vec![
            GraphValue::Vertex(VId(3)),
            GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(7.0))),
        ]),
    ]
}

#[test]
fn membership_candidates_are_masked_and_pinned_in_every_session_stream_driver() {
    let ((), report) = run_async_under_lab(0x5ec0_6301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let mut exact = grant();
        exact.limits.max_nodes = 2;
        let token = issuer.issue_at(&exact, 100).unwrap();
        let args = arguments();
        for hidden in 0..4 {
            let mut db = membership_database(&commit, hidden).await;
            let text = "MATCH (n) WHERE n.p IN $members RETURN n AS id, n.p AS p";
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    GqlQueryPolicy::new(2, 2, 1_000_000, 100_000),
                    || 100,
                )
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            {
                let mut local = session.stream(&cx, &prepared, &args).unwrap();
                assert_eq!(
                    local.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                    expected(),
                    "hidden={hidden}"
                );
                assert_eq!(local.state(), VertexScanState::Exhausted);
            }
            let aggregate = session
                .prepare(
                    &cx,
                    "MATCH (n) WHERE n.p IN $members RETURN COUNT(*) AS total",
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
                    1 => session
                        .stream_aggregate_send(&cx, &aggregate, &args)
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    _ => {
                        let mut cursor = session
                            .stream_aggregate_async(&cx, &aggregate, &args)
                            .unwrap();
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
            let mut update = WriteBatch::new(RelationId(1));
            update.delete_vertex(VId(1));
            update.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(99)));
            db.write(&commit, update).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            assert_eq!(
                sent.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected()
            );
            assert_eq!(sent.state(), VertexScanState::Exhausted);
            assert!(sent.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_membership_histories_do_not_change_native_or_signed_refusal_thresholds() {
    let ((), report) = run_async_under_lab(0x5ec0_6302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = arguments();
        let text = "MATCH (n) WHERE n.p IN $members RETURN n AS id, n.p AS p";
        let run = |db: &Database<MemVfs>, token: &CapabilityToken, policy| {
            let mut session = db
                .authorized_read_session(&cx, &issuer, token, "main", symbols, policy, || 100)
                .unwrap();
            let prepared = session.prepare(&cx, text, &args).unwrap();
            let cursor = session.stream(&cx, &prepared, &args);
            cursor.and_then(|cursor| cursor.collect::<Result<Vec<_>, _>>())
        };
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
        let mut baseline = None;
        for hidden in 0..4 {
            let db = membership_database(&c.commit(), hidden).await;
            assert_eq!(run(&db, &token, policy()).unwrap(), expected());
            let mut thresholds = Vec::new();
            for scratch in [false, true] {
                thresholds.push(threshold(&|limit| {
                    let policy = if scratch {
                        GqlQueryPolicy::new(2, 2, 1_000_000, limit)
                    } else {
                        GqlQueryPolicy::new(2, 2, limit, 100_000)
                    };
                    match run(&db, &token, policy) {
                        Ok(_) => true,
                        Err(QueryError::Stream(GqlQueryError::Evaluator(_))) => false,
                        Err(other) => {
                            panic!("hidden={hidden}, scratch={scratch}, {limit}: {other:?}")
                        }
                    }
                }));
            }
            thresholds.push(threshold(&|limit| match run(
                &db,
                &token.attenuate(Restriction::MaxWork(limit)).unwrap(),
                policy(),
            ) {
                Ok(_) => true,
                Err(QueryError::Authorization(Error::LimitExceeded(LimitDimension::Work))) => false,
                Err(other) => panic!("hidden={hidden}, signed work={limit}: {other:?}"),
            }));
            if let Some(baseline) = &baseline {
                assert_eq!(&thresholds, baseline, "hidden={hidden}");
            } else {
                baseline = Some(thresholds);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_membership_properties_keep_masking_and_exact_source_refusals() {
    let ((), report) = run_async_under_lab(0x5ec0_6303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = arguments();
        for hidden in [7, 99] {
            let mut db =
                Database::open_memory(&c.commit(), DatabaseKeys::new([1; 32], NS, [2; 32]))
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
            db.write(&c.commit(), batch).await.unwrap();
            for records in [1, 2] {
                let mut session = db
                    .authorized_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols,
                        GqlQueryPolicy::new(records, 10, 1_000_000, 100_000),
                        || 100,
                    )
                    .unwrap();
                let prepared = session
                    .prepare(
                        &cx,
                        "MATCH (n) WHERE n.hidden IN $members RETURN n AS id",
                        &args,
                    )
                    .unwrap();
                for send in [false, true] {
                    let result = if send {
                        session
                            .stream_send(&cx, &prepared, &args)
                            .unwrap()
                            .collect::<Result<Vec<_>, _>>()
                    } else {
                        session
                            .stream(&cx, &prepared, &args)
                            .unwrap()
                            .collect::<Result<Vec<_>, _>>()
                    };
                    if records == 2 {
                        assert!(result.unwrap().is_empty(), "a masked key is NULL");
                    } else {
                        assert!(
                            matches!(&result,
                                Err(QueryError::Stream(GqlQueryError::Rows(error)))
                                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                                        && error.limit == 1 && error.observed == 2
                            ),
                            "hidden={hidden}, send={send}: {result:?}"
                        );
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn membership_preserves_visible_collection_and_earlier_arithmetic_errors() {
    let ((), report) = run_async_under_lab(0x5ec0_6304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let mut db = Database::open_memory(&c.commit(), DatabaseKeys::new([1; 32], NS, [2; 32]))
            .await
            .unwrap();
        let oversized = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![
                CanonicalScalar::Null.into();
                GraphValue::MAX_LIST_NODES
            ])
            .unwrap(),
        );
        // Logical conversion keeps its exact bound error. This value cannot
        // reach an authorized stored-row cursor: storage's indivisible-row
        // ceiling is stricter, and must refuse before consuming a sequence.
        let error = GraphValue::admit_property_value::<fgdb_gql::GraphIntegerError>(
            &oversized,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(
            error.kind,
            fgdb_gql::GraphIntegerErrorKind::StoredValueBounds,
        );
        let before = db.frontier().unwrap();
        let mut rejected = WriteBatch::new(RelationId(1));
        rejected.create_vertex_values(
            VId(0),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), oversized)],
        );
        let error = db.write(&c.commit(), rejected).await.unwrap_err();
        assert!(matches!(
            error,
            fgdb::WriteError::VertexStorageAdmission { vid: VId(0), source }
                if matches!(source.as_ref(), fgdb_strata::vertex::VertexPatchError::RowExceedsStorageLimit { bytes, limit } if bytes > limit && *limit == 16_384)
        ));
        assert_eq!(db.frontier().unwrap(), before);
        assert!(db.vertices().unwrap().is_empty());
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex_values(
            VId(0),
            vec![LabelId(1)],
            vec![(
                PropertyKeyId(1),
                CanonicalPropertyValue::List(
                    CanonicalList::try_new(vec![CanonicalScalar::Null.into(); 1_024]).unwrap(),
                ),
            )],
        );
        batch.create_vertex(
            VId(1),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        db.write(&c.commit(), batch).await.unwrap();
        for (members, expected) in [
            (Vec::new(), Vec::new()),
            (
                vec![GraphValue::Scalar(CanonicalScalar::Int(7))],
                vec![GraphValueRow::from_owned_values(vec![GraphValue::Vertex(
                    VId(1),
                )])],
            ),
        ] {
            let args = GqlParameters::new().with_list("members", members).unwrap();
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap();
            let prepared = session
                .prepare(&cx, "MATCH (n) WHERE n.p IN $members RETURN n AS id", &args)
                .unwrap();
            let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
            let rows = cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected);
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            assert!(cursor.next().is_none());
        }
        let db = membership_database(&c.commit(), 0).await;
        let args = arguments();
        let mut session = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let prepared = session
            .prepare(
                &cx,
                "MATCH (n) WHERE 10 / n.p > 0 AND n.p IN $members RETURN n AS id",
                &args,
            )
            .unwrap();
        let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
        let error = cursor.next().unwrap().unwrap_err();
        assert!(matches!(error,
            QueryError::Stream(GqlQueryError::Data(error))
                if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
        ));
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
