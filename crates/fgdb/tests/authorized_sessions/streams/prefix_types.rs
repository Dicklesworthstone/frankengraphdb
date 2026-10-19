use super::*;
use fgdb_types::{
    CanonicalText, CollationResolver, CollationResolverError, NonBinaryTextBinding, ObjectId,
    TzdbResolver,
};
use fgdb_warden::{CapabilityToken, Restriction};
use std::sync::Arc;

struct ReverseSpelling;
impl CollationResolver for ReverseSpelling {
    fn artifact_available(&self, _: &ObjectId) -> bool {
        true
    }
    fn canonical_sort_key_len(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
    ) -> Result<usize, CollationResolverError> {
        Ok(text.len())
    }
    fn write_canonical_sort_key(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
        output: &mut [u8],
    ) -> Result<usize, CollationResolverError> {
        for (target, byte) in output.iter_mut().zip(text.bytes().rev()) {
            *target = byte;
        }
        Ok(text.len())
    }
    fn canonical_sort_key_matches(
        &self,
        _: &NonBinaryTextBinding,
        text: &str,
        candidate: &[u8],
    ) -> Result<bool, CollationResolverError> {
        Ok(candidate.iter().copied().eq(text.bytes().rev()))
    }
}
impl TzdbResolver for ReverseSpelling {
    fn contains_tzdb(&self, _: &ObjectId) -> bool {
        false
    }
    fn canonical_utc_offset_seconds(&self, _: &ObjectId, _: &str, _: i128) -> Option<i32> {
        None
    }
}
fn text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn collated(value: &str) -> CanonicalScalar {
    let binding = NonBinaryTextBinding::new(
        ObjectId([1; 32]),
        ObjectId([2; 32]),
        ObjectId([3; 32]),
        ObjectId([4; 32]),
    );
    CanonicalScalar::Text(CanonicalText::new_non_binary(value, binding, &ReverseSpelling).unwrap())
}
fn prefix_keys() -> DatabaseKeys {
    DatabaseKeys::new([1; 32], NS, [2; 32]).with_scalar_resolver(Arc::new(ReverseSpelling))
}
async fn prefix_database(cx: &CommitCx, hidden: u8) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, prefix_keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (vid, value) in [
        (VId(0), CanonicalScalar::Int(0)),
        (VId(1), text("pre-a")),
        (VId(3), text("outside")),
        (VId(5), text("pre-stale")),
        (VId(7), CanonicalScalar::Null),
        (
            VId(u128::MAX),
            if hidden == 3 {
                collated("pre-b")
            } else {
                text("pre-b")
            },
        ),
    ] {
        batch.create_vertex(vid, vec![LabelId(1)], vec![(PropertyKeyId(1), value)]);
    }
    batch.create_vertex(VId(9), vec![LabelId(1)], vec![]);
    for id in 100..228_u128 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), text(&format!("outside-{id}")))],
        );
    }
    if hidden > 0 {
        // Hidden values alone switch from the small heap to the broad route.
        for id in 0..80_u128 {
            batch.create_vertex(
                VId(1000 + id),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), text(&format!("pre-hidden-{id}")))],
            );
        }
    }
    if hidden > 1 {
        // This hidden alternate binding switches again to an unfiltered scan.
        batch.create_vertex(
            VId(2000),
            vec![LabelId(99)],
            vec![(PropertyKeyId(1), collated("pre-hidden"))],
        );
    }
    db.write(cx, batch).await.unwrap();
    let mut update = WriteBatch::new(RelationId(1));
    update.set_vertex_property(VId(5), PropertyKeyId(1), Some(text("outside")));
    db.write(cx, update).await.unwrap();
    db
}
fn expected(hidden: u8) -> Vec<GraphValueRow> {
    [(VId(1), "pre-a"), (VId(u128::MAX), "pre-b")]
        .into_iter()
        .map(|(vid, value)| {
            GraphValueRow::from_owned_values(vec![
                GraphValue::Vertex(vid),
                GraphValue::Scalar(if hidden == 3 && vid == VId(u128::MAX) {
                    collated(value)
                } else {
                    text(value)
                }),
            ])
        })
        .collect()
}
const PREFIX: &str = "MATCH (n:L) WHERE n.p STARTS WITH 'pre' RETURN n AS id, n.p AS p";

#[test]
fn authorized_prefixes_preserve_scope_limits_and_pins_in_every_driver() {
    let ((), report) = run_async_under_lab(0x5ec0_6301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let broad = issuer.issue_at(&grant(), 100).unwrap();
        let mut exact = grant();
        exact.limits.max_nodes = 2;
        let token = issuer.issue_at(&exact, 100).unwrap();
        let args = GqlParameters::new();
        for hidden in 0..4 {
            let mut db = prefix_database(&commit, hidden).await;
            let eager = db
                .authorized_read_session(&cx, &issuer, &broad, "main", symbols, policy(), || 100)
                .unwrap()
                .query(&cx, PREFIX, &args)
                .unwrap();
            let mut session = db
                .authorized_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols,
                    GqlQueryPolicy::new(2, 2, 1_000_000, 10_000),
                    || 100,
                )
                .unwrap();
            let prepared = session.prepare(&cx, PREFIX, &args).unwrap();
            let mut local = session.stream(&cx, &prepared, &args).unwrap();
            let columns = local.columns().to_vec();
            let rows = local.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
            assert_eq!(rows, expected(hidden), "hidden={hidden}");
            assert_eq!(rows_result(&columns, rows), eager);
            assert_eq!(local.state(), VertexScanState::Exhausted);
            drop(local);
            let aggregate = session
                .prepare(
                    &cx,
                    "MATCH (n:L) WHERE n.p STARTS WITH 'pre' RETURN COUNT(*) AS total",
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
            // All three candidate routes are still uninitialized at opening.
            // Their first demand must observe this generation after mutation.
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(1), PropertyKeyId(1), Some(text("outside")));
            update.delete_vertex(VId(u128::MAX));
            db.write(&commit, update).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            assert_eq!(
                sent.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected(hidden)
            );
            assert_eq!(sent.state(), VertexScanState::Exhausted);
            assert!(sent.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn hidden_collation_cannot_change_native_or_signed_prefix_refusals() {
    let ((), report) = run_async_under_lab(0x5ec0_6302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let visible = prefix_database(&c.commit(), 0).await;
        let hidden = prefix_database(&c.commit(), 2).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        let run = |db: &Database<MemVfs>, token: &CapabilityToken, policy| {
            let mut session = db
                .authorized_read_session(&cx, &issuer, token, "main", symbols, policy, || 100)
                .unwrap();
            let prepared = session.prepare(&cx, PREFIX, &args).unwrap();
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
                        GqlQueryPolicy::new(2, 2, limit, 10_000)
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
        for db in [&visible, &hidden] {
            assert!(matches!(
                run(db, &token, GqlQueryPolicy::new(1, 2, 1_000_000, 10_000)),
                Err(QueryError::Stream(GqlQueryError::Rows(error)))
                    if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                        && error.limit == 1 && error.observed == 2
            ));
            let mut session = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap();
            for statement in [
                "MATCH (n:L) WHERE 10 / n.p > 0 AND n.p STARTS WITH 'pre' RETURN n AS id",
                "MATCH (n:L) WHERE n.p STARTS WITH 'pre' AND 10 / n.p > 0 RETURN n AS id",
            ] {
                let prepared = session.prepare(&cx, statement, &args).unwrap();
                let mut cursor = session.stream(&cx, &prepared, &args).unwrap();
                assert!(
                    matches!(
                        cursor.next(),
                        Some(Err(QueryError::Stream(GqlQueryError::Data(error))))
                            if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero
                    ),
                    "{statement}"
                );
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert!(cursor.next().is_none());
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn denied_prefix_properties_keep_local_and_send_refusals_identical() {
    let ((), report) = run_async_under_lab(0x5ec0_6303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let args = GqlParameters::new();
        for value in [text("prefix"), text("outside"), collated("prefix")] {
            let mut db = Database::open_memory(&commit, prefix_keys()).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=2 {
                batch.create_vertex(
                    VId(id),
                    vec![LabelId(1)],
                    vec![(PropertyKeyId(2), value.clone())],
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
                    GqlQueryPolicy::new(1, 2, 1_000_000, 10_000),
                    || 100,
                )
                .unwrap();
            let prepared = session
                .prepare(
                    &cx,
                    "MATCH (n:L) WHERE n.hidden STARTS WITH 'pre' RETURN n AS id",
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
