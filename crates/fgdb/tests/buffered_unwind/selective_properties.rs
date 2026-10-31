//! Selective property materialization through the public cold owner and the
//! real external projection stages. Large unrelated payloads stay in storage.

use super::*;
use fgdb::{BufferedReadError, EmbeddedReadView};

const WIDE_VERTEX: VId = VId((1_u128 << 100) | 7);

fn collection() -> GraphValue {
    GraphValue::List(vec![scalar(21), null(), scalar(22)].into_boxed_slice())
}

fn document() -> GraphValue {
    GraphValue::map(vec![("items".into(), collection())]).unwrap()
}

async fn advance_properties(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_property(VId(1), PropertyKeyId(1), None);
    change.set_vertex_property_value(
        VId(2),
        PropertyKeyId(1),
        Some(collection().to_property_value().unwrap()),
    );
    change.delete_vertex(VId(3));
    change.create_vertex_values(
        WIDE_VERTEX,
        vec![],
        vec![
            (PropertyKeyId(1), document().to_property_value().unwrap()),
            (
                PropertyKeyId(2),
                CanonicalScalar::ucs_basic_text(&"x".repeat(PAYLOAD_BYTES))
                    .unwrap()
                    .into(),
            ),
        ],
    );
    assert_eq!(db.write(cx, change).await.unwrap(), CommitSeq(3));
}

fn expected_property(
    resident: &EmbeddedReadView,
    vid: VId,
    key: PropertyKeyId,
    cut: CommitSeq,
) -> Option<fgdb_types::CanonicalPropertyValue> {
    resident
        .vertex_at(vid, cut)
        .unwrap()
        .and_then(|row| {
            row.props
                .into_iter()
                .find(|(candidate, _)| *candidate == key)
        })
        .map(|(_, value)| value)
}

#[test]
fn selected_properties_match_full_history_and_retain_only_their_own_payload() {
    let ((), report) = run_async_under_lab(0xc01d_4701, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let mut db = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        advance_properties(&mut db, &commit).await;
        let resident = db.read_session().unwrap();
        drop(db);
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;

        // The full resident decoder is the incumbent oracle. Include absent
        // identities/keys, a removed property, deletion, a future birth, and
        // scalar/List/Map values over every retained cut.
        for seq in 0..=3 {
            for vid in [VId(1), VId(2), VId(3), WIDE_VERTEX, VId(999)] {
                for key in [PropertyKeyId(1), PropertyKeyId(2), PropertyKeyId(777)] {
                    let expected = expected_property(&resident, vid, key, CommitSeq(seq));
                    let selected = cold
                        .vertex_property_at(&cx, vid, key, CommitSeq(seq))
                        .await
                        .unwrap();
                    assert_eq!(
                        selected.as_deref(),
                        expected.as_ref(),
                        "vertex {vid:?}, key {key:?}, cut {seq}",
                    );
                }
            }
        }

        let complete = cold
            .vertex_at(&cx, VId(2), CommitSeq(2))
            .await
            .unwrap()
            .unwrap();
        let selected = cold
            .vertex_property_at(&cx, VId(2), PropertyKeyId(1), CommitSeq(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            selected.as_ref(),
            &fgdb_types::CanonicalPropertyValue::Scalar(CanonicalScalar::Int(2)),
        );
        assert!(
            selected.charged_bytes() + PAYLOAD_BYTES <= complete.charged_bytes(),
            "the small selected value must not retain the unrelated large q payload",
        );
        drop((complete, selected));

        let before = cold.buffer_stats();
        assert!(matches!(
            cold.vertex_property_at(&cx, VId(2), PropertyKeyId(1), CommitSeq(4))
                .await,
            Err(BufferedReadError::BeyondPublication {
                requested: CommitSeq(4),
                publication: CommitSeq(3),
            })
        ));
        assert_eq!(
            cold.buffer_stats(),
            before,
            "future cuts do not fault storage"
        );

        let retained = cold
            .vertex_property(&cx, WIDE_VERTEX, PropertyKeyId(1))
            .await
            .unwrap()
            .unwrap();
        let reservation = retained.charged_bytes();
        drop(cold);
        assert_eq!(source_pool.used(), reservation);
        assert_eq!(retained.as_ref(), &document().to_property_value().unwrap());
        drop(retained);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selective_spill_reads_keep_pinned_cuts_duplicates_nulls_and_record_limits() {
    let ((), report) = run_async_under_lab(0xc01d_4702, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let mut db = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let old_resident = db.read_session().unwrap();
        let old_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &old_pool).await;
        advance_properties(&mut db, &commit).await;
        let new_resident = db.read_session().unwrap();
        let new_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut new = view(&commit, &vfs, &new_pool).await;
        drop(db);
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let params = GqlParameters::new()
            .with_list(
                "items",
                vec![
                    GraphValue::Vertex(VId(3)),
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Vertex(VId(3)),
                    GraphValue::Vertex(VId(999)),
                    GraphValue::Vertex(WIDE_VERTEX),
                    null(),
                ],
            )
            .unwrap();
        let text = "UNWIND [0,1,2,3,4,5,6] AS ordinal \
                    WITH ordinal,$items[ordinal] AS n \
                    RETURN ordinal,n.p AS first,n.p AS repeated ORDER BY ordinal";

        // Revisit the old immutable owner after reading the successor. The
        // explicit ordinal makes the expected order independent of value kind.
        for pass in 0..3 {
            let (cold, resident, cut, values) = if pass == 1 {
                (
                    &mut new,
                    &new_resident,
                    CommitSeq(3),
                    vec![
                        null(),
                        null(),
                        collection(),
                        null(),
                        null(),
                        document(),
                        null(),
                    ],
                )
            } else {
                (
                    &mut old,
                    &old_resident,
                    CommitSeq(2),
                    vec![
                        scalar(13),
                        scalar(1),
                        scalar(2),
                        scalar(13),
                        null(),
                        null(),
                        null(),
                    ],
                )
            };
            let expected: Vec<Vec<GraphValue>> = values
                .into_iter()
                .enumerate()
                .map(|(ordinal, value)| vec![scalar(ordinal as i64), value.clone(), value])
                .collect();
            let native = PreparedNativeRead::prepare(text, &params, resolve).unwrap();
            let fgdb::QueryResult::Rows { rows, .. } = native
                .execute_in_view(resident, &cx, &params, policy(7))
                .unwrap()
            else {
                panic!("property projections return rows")
            };
            assert_eq!(
                rows,
                expected
                    .iter()
                    .map(|row| row
                        .iter()
                        .cloned()
                        .map(GraphAggregateValue::Value)
                        .collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
            );

            // Six non-NULL identities, including absent/deleted ones, each
            // reach two property reads. A NULL receiver performs no graph read.
            // Repeated keys and identities still cost distinct logical records.
            for (empty_page, records) in [
                (false, 0),
                (false, 12),
                (false, 11),
                (true, 11),
                (true, 12),
                (false, 0),
            ] {
                let statement = if empty_page {
                    format!("{text} LIMIT 0")
                } else {
                    text.into()
                };
                let prepared = prepare(&statement, &params);
                let before = cold.buffer_stats();
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let result = prepared
                    .spool_in_view(
                        cold,
                        &cx,
                        GqlQueryPolicy::new(records, if empty_page { 0 } else { 7 }, WORK, WORK),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        1,
                        512,
                        if pass == 0 { 1 } else { 3 },
                        512,
                        127,
                        32 * 1024,
                        100,
                        WORK,
                    )
                    .await;
                if records < 12 {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(error, NativeSpoolError::BufferedExecute(ref error)
                            if matches!(**error, GqlQueryError::Rows(ref error)
                                if error.dimension == GqlBudgetDimension::SnapshotRecords
                                && error.limit == records && error.observed == records + 1)),
                        "{statement}, pass {pass}: {error:?}",
                    );
                    if records == 0 {
                        assert_eq!(
                            cold.buffer_stats(),
                            before,
                            "refuse before cold or warm I/O"
                        );
                    }
                } else {
                    let (spool, _) = result.unwrap();
                    assert_eq!(spool.snapshot_seq(), cut);
                    assert_eq!(spool.row_stats().snapshot_records, 12);
                    assert_eq!(
                        contents(&spool, &mut destination, &cx).await,
                        if empty_page { vec![] } else { expected.clone() },
                    );
                }
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop((old, new));
        assert_eq!(old_pool.used(), 0);
        assert_eq!(new_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
