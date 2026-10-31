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

#[test]
fn historical_winner_properties_survive_reopen_compaction_and_repeated_spill_reads() {
    let ((), report) = run_async_under_lab(0xc01d_4705, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let history = [
            Some(collection()),
            Some(scalar(83)),
            Some(document()),
            Some(null()),
            None,
        ];
        let mut birth = WriteBatch::new(RelationId(1));
        birth.create_vertex_values(
            WIDE_VERTEX,
            vec![],
            vec![
                (
                    PropertyKeyId(1),
                    history[0].as_ref().unwrap().to_property_value().unwrap(),
                ),
                (
                    PropertyKeyId(2),
                    CanonicalScalar::bytes(vec![0x45; 2048]).unwrap().into(),
                ),
            ],
        );
        assert_eq!(db.write(&commit, birth).await.unwrap(), CommitSeq(1));
        let old_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &old_pool).await;
        for (at, value) in history.iter().enumerate().skip(1) {
            let mut change = WriteBatch::new(RelationId(1));
            change.set_vertex_property_value(
                WIDE_VERTEX,
                PropertyKeyId(1),
                value
                    .as_ref()
                    .map(|value| value.to_property_value().unwrap()),
            );
            assert_eq!(
                db.write(&commit, change).await.unwrap(),
                CommitSeq(at as u64 + 1)
            );
        }
        let mut deletion = WriteBatch::new(RelationId(1));
        deletion.delete_vertex(WIDE_VERTEX);
        assert_eq!(db.write(&commit, deletion).await.unwrap(), CommitSeq(6));
        let resident = db.read_session().unwrap();
        let before_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut before = view(&commit, &vfs, &before_pool).await;
        db.compact(&commit).await.unwrap();
        drop(db);
        let after_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut after = view(&commit, &vfs, &after_pool).await;
        let params = GqlParameters::new()
            .with_list(
                "items",
                vec![
                    GraphValue::Vertex(WIDE_VERTEX),
                    GraphValue::Vertex(WIDE_VERTEX),
                    null(),
                ],
            )
            .unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        // Deliberately revisit an old cut after newer cuts and a different
        // physical generation. The literal timeline includes present NULL,
        // removal and deletion, which all render NULL but have distinct owners.
        for cold in [&mut before, &mut after] {
            for cut in [0_u64, 1, 4, 5, 6, 3, 2, 1] {
                let literal_value = cut
                    .checked_sub(1)
                    .and_then(|at| history.get(at as usize))
                    .and_then(Option::as_ref);
                let literal_property =
                    literal_value.map(|value| value.to_property_value().unwrap());
                let selected = cold
                    .vertex_property_at(&cx, WIDE_VERTEX, PropertyKeyId(1), CommitSeq(cut))
                    .await
                    .unwrap();
                assert_eq!(selected.as_deref(), literal_property.as_ref());
                assert_eq!(
                    literal_property,
                    expected_property(&resident, WIDE_VERTEX, PropertyKeyId(1), CommitSeq(cut)),
                );
                drop(selected);
                let value = literal_value.cloned().unwrap_or_else(null);
                let expected = vec![
                    vec![scalar(0), value.clone(), value.clone()],
                    vec![scalar(1), value.clone(), value],
                    vec![scalar(2), null(), null()],
                ];
                let text = format!(
                    "UNWIND [0,1,2] AS ordinal FOR SYSTEM_TIME AS OF SEQ {cut} \
                     WITH ordinal,$items[ordinal] AS n \
                     RETURN ordinal,n.p AS first,n.p AS repeated ORDER BY ordinal"
                );
                let prepared = prepare(&text, &params);
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = prepared
                    .spool_in_view(
                        cold,
                        &cx,
                        GqlQueryPolicy::new(4, 3, WORK, WORK),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        1,
                        16,
                        1,
                        16,
                        127,
                        32 * 1024,
                        16,
                        WORK,
                    )
                    .await
                    .unwrap();
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(spool.row_stats().snapshot_records, 4);
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            }
        }
        let retained = old
            .vertex_property(&cx, WIDE_VERTEX, PropertyKeyId(1))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            retained.as_ref(),
            &collection().to_property_value().unwrap()
        );
        let bytes = retained.charged_bytes();
        drop((old, before, after));
        assert_eq!(old_pool.used(), bytes);
        assert_eq!(before_pool.used(), 0);
        assert_eq!(after_pool.used(), 0);
        assert_eq!(spill_pool.used(), 0);
        drop(retained);
        assert_eq!(old_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const WIDE_EDGE: EId = EId((2_u128 << 100) | 7);

async fn edge_fixture(cx: &CommitCx) -> (MemVfs, Database<MemVfs>) {
    let vfs = fixture(cx).await;
    let mut db = Database::open_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut change = WriteBatch::new(RelationId(1));
    for eid in [EId(1), EId(2), EId(3)] {
        change.set_edge_property(
            eid,
            PropertyKeyId(2),
            Some(CanonicalScalar::ucs_basic_text(&"x".repeat(PAYLOAD_BYTES)).unwrap()),
        );
    }
    assert_eq!(db.write(cx, change).await.unwrap(), CommitSeq(3));
    (vfs, db)
}

async fn advance_edge_properties(db: &mut Database<MemVfs>, cx: &CommitCx) {
    let mut change = WriteBatch::new(RelationId(1));
    // This existing parallel edge loses its entire sidecar, not just p.
    change.set_edge_property(EId(1), PropertyKeyId(1), None);
    change.set_edge_property(EId(1), PropertyKeyId(2), None);
    change.set_edge_property_value(
        EId(2),
        PropertyKeyId(1),
        Some(collection().to_property_value().unwrap()),
    );
    // The writer's ordinary cascade retires edge 3 at this same cut.
    change.delete_vertex(VId(3));
    change.add_edge_values(
        WIDE_EDGE,
        VId(2),
        VId(2),
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
    change.add_edge(EId(5), VId(2), VId(1), vec![]);
    assert_eq!(db.write(cx, change).await.unwrap(), CommitSeq(4));
}

#[test]
fn selected_edge_properties_match_sidecar_history_and_retain_only_their_value() {
    let ((), report) = run_async_under_lab(0xc01d_4703, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db) = edge_fixture(&commit).await;
        advance_edge_properties(&mut db, &commit).await;
        let resident = db.read_session().unwrap();
        drop(db);
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;

        // Compare the complete resident winner against a single requested
        // field. Parallel edges, a loop, a removed sidecar, a propertyless
        // birth, retirement and the full-width identity all retain their cut.
        for seq in 0..=4 {
            for eid in [EId(1), EId(2), EId(3), WIDE_EDGE, EId(5), EId(999)] {
                for key in [PropertyKeyId(1), PropertyKeyId(2), PropertyKeyId(777)] {
                    let expected = resident
                        .edge_at(eid, CommitSeq(seq))
                        .unwrap()
                        .and_then(|row| {
                            row.props
                                .into_iter()
                                .find(|(candidate, _)| *candidate == key)
                        })
                        .map(|(_, value)| value);
                    let selected = cold
                        .edge_property_at(&cx, eid, key, CommitSeq(seq))
                        .await
                        .unwrap();
                    assert_eq!(
                        selected.as_deref(),
                        expected.as_ref(),
                        "edge {eid:?}, key {key:?}, cut {seq}",
                    );
                }
            }
        }

        let complete = cold
            .edge_at(&cx, EId(2), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        let selected = cold
            .edge_property_at(&cx, EId(2), PropertyKeyId(1), CommitSeq(3))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            selected.as_ref(),
            &fgdb_types::CanonicalPropertyValue::Scalar(CanonicalScalar::Int(8)),
        );
        assert!(
            selected.charged_bytes() + PAYLOAD_BYTES <= complete.charged_bytes(),
            "the selected edge value must not retain the unrelated large q payload",
        );
        drop((complete, selected));

        let before = cold.buffer_stats();
        assert!(matches!(
            cold.edge_property_at(&cx, EId(2), PropertyKeyId(1), CommitSeq(5))
                .await,
            Err(BufferedReadError::BeyondPublication {
                requested: CommitSeq(5),
                publication: CommitSeq(4),
            })
        ));
        assert_eq!(
            cold.buffer_stats(),
            before,
            "future cuts do not fault storage"
        );

        let retained = cold
            .edge_property(&cx, WIDE_EDGE, PropertyKeyId(1))
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
fn selective_edge_spill_reads_keep_pinned_cuts_and_each_logical_record() {
    let ((), report) = run_async_under_lab(0xc01d_4704, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db) = edge_fixture(&commit).await;
        let old_resident = db.read_session().unwrap();
        let old_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &old_pool).await;
        advance_edge_properties(&mut db, &commit).await;
        let new_resident = db.read_session().unwrap();
        let new_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let mut new = view(&commit, &vfs, &new_pool).await;
        drop(db);
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let params = GqlParameters::new()
            .with_list(
                "items",
                vec![
                    GraphValue::Edge(EId(3)),
                    GraphValue::Edge(EId(1)),
                    GraphValue::Edge(EId(2)),
                    GraphValue::Edge(EId(3)),
                    GraphValue::Edge(EId(999)),
                    GraphValue::Edge(WIDE_EDGE),
                    null(),
                ],
            )
            .unwrap();
        let text = "UNWIND [0,1,2,3,4,5,6] AS ordinal \
                    WITH ordinal,$items[ordinal] AS e \
                    RETURN ordinal,e.p AS first,e.p AS repeated ORDER BY ordinal";

        for pass in 0..3 {
            let (cold, resident, cut, values) = if pass == 1 {
                (
                    &mut new,
                    &new_resident,
                    CommitSeq(4),
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
                    CommitSeq(3),
                    vec![
                        scalar(9),
                        scalar(7),
                        scalar(8),
                        scalar(9),
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
                panic!("edge property projections return rows")
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

            // Propertyless, absent, retired and repeated edges are still
            // reached identities. Only the NULL receiver skips the source.
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
