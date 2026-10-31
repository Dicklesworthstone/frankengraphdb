use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphPatternBuilder, IntegerComparison, VertexPredicate};
use fgdb_gql::stream::VertexScanState;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::cell::Cell;
use std::sync::Arc;

const LABEL: LabelId = LabelId(3);
const KEY: PropertyKeyId = PropertyKeyId(7);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn pattern(count: Option<u64>) -> PreparedGraphPattern {
    let mut builder = GraphPatternBuilder::new();
    builder.vertex("n").unwrap();
    builder
        .filter("n", VertexPredicate::HasLabel(LABEL))
        .unwrap();
    builder
        .filter(
            "n",
            VertexPredicate::IntegerProperty {
                key: KEY,
                comparison: IntegerComparison::GreaterOrEqual,
                value: 0,
            },
        )
        .unwrap();
    builder.prepare("n", 0, count).unwrap()
}

#[test]
fn every_actual_history_source_checkpoint_refuses_once_and_preserves_the_delivered_prefix() {
    let ((), report) = run_async_under_lab(0x51cb_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [0, 1, 2, 3, 4, u128::MAX] {
            seed.create_vertex(VId(id), vec![LABEL], vec![(KEY, CanonicalScalar::Int(1))]);
        }
        db.write(&commit, seed).await.unwrap();
        for seq in 2..=7 {
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(1), KEY, Some(CanonicalScalar::Int(seq - 4)));
            edit.set_vertex_label(VId(3), LABEL, seq % 2 == 0);
            if seq == 3 {
                edit.delete_vertex(VId(2));
            }
            if seq == 5 {
                edit.create_vertex(VId(5), vec![LABEL], vec![(KEY, CanonicalScalar::Int(5))]);
            }
            db.write(&commit, edit).await.unwrap();
        }
        let view = db.read_session().unwrap();
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        builder
            .filter("n", VertexPredicate::HasLabel(LABEL))
            .unwrap();
        builder
            .filter(
                "n",
                VertexPredicate::IntegerProperty {
                    key: KEY,
                    comparison: IntegerComparison::Equal,
                    value: 1,
                },
            )
            .unwrap();
        let prepared = builder.prepare("n", 0, None).unwrap();
        let plan = VertexScanPlan::compile(prepared.plan()).unwrap();
        for as_of in [CommitSeq(0), CommitSeq(1), CommitSeq(3), CommitSeq(7)] {
            let expected = db
                .execute_graph_pattern_governed_at(&cx, &prepared, as_of, wide())
                .unwrap()
                .value;
            let calls = Cell::new(0);
            let mut cursor = VertexScanCursor::new(
                SnapshotVertexSource {
                    view: view.clone(),
                    cx: &cx,
                    as_of,
                    after: None,
                    candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
                }
                .with_root_equality(&plan, |_| true),
                plan.clone(),
                wide(),
                || {
                    calls.set(calls.get() + 1);
                    Ok::<_, usize>(())
                },
            );
            assert_eq!(
                cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                expected
            );
            assert_eq!(cursor.state(), VertexScanState::Exhausted);
            let total = calls.get();
            drop(cursor);
            for stop in 1..=total {
                let refs = Arc::strong_count(&view.snapshot);
                let calls = Cell::new(0);
                let mut cursor = VertexScanCursor::new(
                    SnapshotVertexSource {
                        view: view.clone(),
                        cx: &cx,
                        as_of,
                        after: None,
                        candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
                    }
                    .with_root_equality(&plan, |_| true),
                    plan.clone(),
                    wide(),
                    || {
                        let at = calls.get() + 1;
                        calls.set(at);
                        if at == stop { Err(stop) } else { Ok(()) }
                    },
                );
                assert_eq!(Arc::strong_count(&view.snapshot), refs + 1);
                let mut prefix = Vec::new();
                loop {
                    match cursor.next().expect("selected checkpoint must be reached") {
                        Ok(vid) => prefix.push(vid),
                        Err(GqlQueryError::Interrupted(at)) => {
                            assert_eq!(at, stop);
                            break;
                        }
                        Err(other) => panic!("unexpected refusal: {other:?}"),
                    }
                }
                assert!(expected.starts_with(&prefix));
                assert_eq!(cursor.row_stats().result_rows, prefix.len() as u64);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                assert_eq!(
                    Arc::strong_count(&view.snapshot),
                    refs,
                    "failed source must release its generation immediately"
                );
                assert!(cursor.next().is_none());
                cursor.close();
                assert_eq!(calls.get(), stop);
                assert_eq!(cursor.state(), VertexScanState::Failed);
                let retry = VertexScanCursor::new(
                    SnapshotVertexSource {
                        view: view.clone(),
                        cx: &cx,
                        as_of,
                        after: None,
                        candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
                    }
                    .with_root_equality(&plan, |_| true),
                    plan.clone(),
                    wide(),
                    || Ok::<_, usize>(()),
                )
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
                assert_eq!(retry, expected);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn terminal_cursors_release_the_last_snapshot_owner_without_waiting_for_cursor_drop() {
    let ((), report) = run_async_under_lab(0x51cb_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for mode in 0..6 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            seed.create_vertex(VId(0), vec![LABEL], vec![(KEY, CanonicalScalar::Int(0))]);
            seed.create_vertex(
                VId(u128::MAX),
                vec![LABEL],
                vec![(KEY, CanonicalScalar::Int(1))],
            );
            db.write(&commit, seed).await.unwrap();
            let view = db.read_session().unwrap();
            let generation = Arc::downgrade(&view.snapshot);
            let prepared = pattern(match mode {
                1 => Some(0),
                2 => Some(1),
                _ => None,
            });
            // Mode 4 fails on the record allowance. The cursor may charge a
            // scratch entry before its first record (lazy candidates), so
            // scratch keeps an allowance and Rows is the first refusal.
            let policy = if mode == 4 {
                GqlQueryPolicy::new(0, 0, 100, 100)
            } else {
                wide()
            };
            let mut cursor = view
                .stream_graph_vertices_governed(&cx, &prepared, policy)
                .unwrap();
            drop(prepared);
            drop(view);
            drop(db);
            assert!(
                generation.upgrade().is_some(),
                "open cursor owns the pinned generation"
            );
            match mode {
                0 => {
                    assert_eq!(
                        cursor.by_ref().collect::<Result<Vec<_>, _>>().unwrap(),
                        vec![VId(0), VId(u128::MAX)]
                    );
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                }
                1 => {
                    assert!(cursor.next().is_none());
                }
                2 => {
                    assert_eq!(cursor.next().unwrap().unwrap(), VId(0));
                    assert_eq!(cursor.state(), VertexScanState::Exhausted);
                }
                3 => {
                    cursor.close();
                    assert_eq!(cursor.state(), VertexScanState::Closed);
                }
                4 => {
                    assert!(matches!(cursor.next(), Some(Err(GqlQueryError::Rows(_)))));
                    assert_eq!(cursor.state(), VertexScanState::Failed);
                }
                _ => {
                    drop(cursor);
                    assert!(generation.upgrade().is_none());
                    continue;
                }
            }
            assert!(
                generation.upgrade().is_none(),
                "terminal cursor retained the generation"
            );
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            assert!(cursor.next().is_none());
            cursor.close();
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn snapshot_successors_are_sorted_and_visibility_uses_the_shared_mvcc_resolver() {
    let ((), report) = run_async_under_lab(0x51cb_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [u128::MAX, 7, 0, 1_u128 << 100, 3, 1] {
            seed.create_vertex(VId(id), vec![], vec![]);
        }
        let first = db.write(&commit, seed).await.unwrap();
        let mut edits = WriteBatch::new(RelationId(1));
        edits.delete_vertex(VId(3));
        edits.set_vertex_property(VId(7), KEY, Some(CanonicalScalar::Int(12)));
        edits.create_vertex(VId(2), vec![], vec![]);
        let second = db.write(&commit, edits).await.unwrap();
        let view = db.read_session().unwrap();
        for cut in [CommitSeq::ORIGIN, first, second] {
            let mut source = SnapshotVertexSource {
                view: view.clone(),
                cx: &cx,
                as_of: cut,
                after: None,
                candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
            };
            let mut candidates = Vec::new();
            let mut visible = Vec::new();
            let mut control = |_| Ok::<_, ()>(());
            while let Some(vid) = source.next_vertex(&mut control).unwrap() {
                candidates.push(vid);
                if source.vertex(vid, &mut control).unwrap().is_some() {
                    visible.push(vid);
                }
            }
            assert_eq!(
                candidates,
                [0, 1, 2, 3, 7, 1_u128 << 100, u128::MAX].map(VId)
            );
            assert_eq!(
                visible,
                db.vertices_at(cut)
                    .unwrap()
                    .iter()
                    .map(|row| row.vid)
                    .collect::<Vec<_>>()
            );
            assert!(source.next_vertex(&mut control).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_plain_streams_keep_property_postings_unbuilt_through_pages_history_and_compaction() {
    use fgdb_gql::algebra::{GraphColumn, GraphValue};

    let ((), report) = run_async_under_lab(0x51cb_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = crate::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in (0..64).chain([u128::MAX]) {
            batch.create_vertex(
                VId(id),
                vec![LABEL],
                vec![
                    (KEY, CanonicalScalar::Int(id as i64)),
                    (
                        PropertyKeyId(8),
                        CanonicalScalar::ucs_basic_text(&format!("{id}:{}", "payload".repeat(256)))
                            .unwrap(),
                    ),
                ],
            );
        }
        let first = db.write(&commit, batch).await.unwrap();
        let mut changed = WriteBatch::new(RelationId(1));
        changed.set_vertex_property(VId(0), KEY, Some(CanonicalScalar::Int(100)));
        changed.set_vertex_label(VId(1), LABEL, false);
        changed.delete_vertex(VId(2));
        changed.create_vertex(VId(64), vec![LABEL], vec![(KEY, CanonicalScalar::Int(64))]);
        let latest = db.write(&commit, changed).await.unwrap();
        let prepare = |offset, count| {
            let mut builder = GraphPatternBuilder::new();
            builder.vertex("n").unwrap();
            builder
                .filter("n", VertexPredicate::HasLabel(LABEL))
                .unwrap();
            builder
                .prepare_values(
                    &[
                        GraphColumn::vertex("n", "n"),
                        GraphColumn::property("value", "n", KEY),
                    ],
                    offset,
                    count,
                )
                .unwrap()
        };
        let expected_at = |cut| {
            db.vertices_at(cut)
                .unwrap()
                .into_iter()
                .filter(|row| row.labels.contains(&LABEL))
                .map(|row| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(row.vid),
                        GraphValue::from_property_value(
                            &row.props.iter().find(|(key, _)| *key == KEY).unwrap().1,
                        )
                        .unwrap(),
                    ])
                })
                .collect::<Vec<_>>()
        };
        let expected_latest = expected_at(latest);
        let cold = Database::open_read_view_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        assert!(cold.snapshot.property_index.get().is_none());
        for cut in [CommitSeq::ORIGIN, first, latest] {
            let expected = expected_at(cut);
            for (offset, count) in [(0, None), (0, Some(0)), (0, Some(1)), (1, Some(2))] {
                let prepared = prepare(offset, count);
                let actual = cold
                    .stream_graph_values_governed_at(&cx, &prepared, cut, wide())
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let page: Vec<_> = expected
                    .iter()
                    .skip(offset as usize)
                    .take(count.map_or(usize::MAX, |n| n as usize))
                    .cloned()
                    .collect();
                assert_eq!(actual, page, "cut={cut:?} offset={offset} count={count:?}");
                assert!(
                    cold.snapshot.property_index.get().is_none(),
                    "a plain stream must not encode/sort the unrequested properties"
                );
            }
        }
        let full = prepare(0, None);
        let mut pinned = cold
            .stream_graph_values_governed(&cx, &full, wide())
            .unwrap();
        assert_eq!(pinned.next().unwrap().unwrap(), expected_latest[0]);
        // Another query may warm the same generation while a cold cursor is
        // paused. It must not restart that cursor or change its pinned winner.
        cold.snapshot.property_index();
        let warm = cold
            .stream_graph_values_governed(&cx, &full, wide())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(warm, expected_latest);
        db.compact(&commit).await.unwrap();
        drop(db);
        assert_eq!(
            pinned.collect::<Result<Vec<_>, _>>().unwrap(),
            expected_latest[1..]
        );
        let reopened = Database::open_read_view_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(reopened.snapshot.property_index.get().is_none());
        assert_eq!(
            reopened
                .stream_graph_values_governed(&cx, &full, wide())
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            expected_latest
        );
        assert!(reopened.snapshot.property_index.get().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_authorized_streams_mask_history_before_charging_visible_allowances() {
    use asupersync::security::key::AuthKey;
    use fgdb_delta_types::SchemaEpoch;
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind};
    use fgdb_warden::{Authority, CapabilityToken, Grant, QueryLimits, Restriction, Scope};

    let ((), report) = run_async_under_lab(0x51cb_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let hidden_label = LabelId(99);
        let hidden_property = PropertyKeyId(8);
        let mut views = Vec::new();
        // The extra population includes more than one patch of hidden roots
        // before the first permitted identity, plus their retained histories.
        for hidden_count in [0, 300] {
            let vfs = crate::MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for (id, labels, value) in
                [(1000, vec![LABEL, hidden_label], 1), (2000, vec![LABEL], 2)]
            {
                batch.create_vertex(
                    VId(id),
                    labels,
                    vec![
                        (KEY, CanonicalScalar::Int(value)),
                        (hidden_property, CanonicalScalar::Int(55)),
                    ],
                );
            }
            for id in 0..hidden_count {
                batch.create_vertex(
                    VId(id),
                    vec![hidden_label],
                    vec![(KEY, CanonicalScalar::Int(99))],
                );
            }
            assert_eq!(db.write(&commit, batch).await.unwrap(), CommitSeq(1));
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_label(VId(1000), LABEL, false);
            update.set_vertex_property(VId(2000), KEY, Some(CanonicalScalar::Int(20)));
            update.create_vertex(VId(3000), vec![LABEL], vec![(KEY, CanonicalScalar::Int(3))]);
            for id in 0..hidden_count {
                if id % 2 == 0 {
                    update.delete_vertex(VId(id));
                } else {
                    update.set_vertex_property(VId(id), KEY, Some(CanonicalScalar::Int(100)));
                }
            }
            assert_eq!(db.write(&commit, update).await.unwrap(), CommitSeq(2));
            drop(db);
            let view = Database::open_read_view_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            assert!(view.snapshot.property_index.get().is_none());
            views.push(view);
        }
        let issuer = Authority::new(
            AuthKey::from_seed(0x51cb_0006),
            DatabaseSecurityNamespaceId([0x92; 32]),
            "graph",
            SchemaEpoch(0),
            1,
        )
        .unwrap();
        let mut grant = Grant::read_only(
            "main",
            1000,
            QueryLimits {
                max_nodes: 1000,
                max_work: 1_000_000,
                max_rows: 1000,
            },
        );
        grant.labels = Scope::only([LABEL]);
        grant.properties = Scope::only([KEY]);
        let token = issuer.issue_at(&grant, 100).unwrap();
        let symbols = |kind, name: &str| match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(KEY)),
            (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(hidden_property)),
            _ => None,
        };
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut \
                    RETURN n AS id, n.p AS p, n.hidden AS hidden";
        let run = |view: &EmbeddedReadView, cut, token: &CapabilityToken, policy| {
            let args = GqlParameters::new().with_uint64("cut", cut).unwrap();
            let mut session = view
                .authorized_read_session(&cx, &issuer, token, "main", symbols, policy, || 100)
                .unwrap();
            let prepared = match session.prepare(&cx, text, &args) {
                Ok(prepared) => prepared,
                Err(error) => {
                    assert!(view.snapshot.property_index.get().is_none());
                    return vec![Err(format!("{error:?}"))];
                }
            };
            let transcript = match session.stream(&cx, &prepared, &args) {
                Ok(cursor) => cursor
                    .map(|row| row.map_err(|error| format!("{error:?}")))
                    .collect::<Vec<_>>(),
                Err(error) => vec![Err(format!("{error:?}"))],
            };
            assert!(view.snapshot.property_index.get().is_none());
            transcript
        };
        let threshold = |admits: &dyn Fn(u64) -> bool| {
            let (mut low, mut high) = (0, 1000);
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
        for (cut, visible) in [(1, [(1000, 1), (2000, 2)]), (2, [(2000, 20), (3000, 3)])] {
            let expected = visible
                .into_iter()
                .map(|(id, value)| {
                    Ok(GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(VId(id)),
                        GraphValue::Scalar(CanonicalScalar::Int(value)),
                        GraphValue::Scalar(CanonicalScalar::Null),
                    ]))
                })
                .collect::<Vec<_>>();
            for view in &views {
                assert_eq!(run(view, cut, &token, wide()), expected);
            }
            let signed: [fn(u64) -> Restriction; 3] = [
                Restriction::MaxNodes,
                Restriction::MaxRows,
                Restriction::MaxWork,
            ];
            for restriction in signed {
                let boundary = [&views[0], &views[1]].map(|view| {
                    threshold(&|k| {
                        run(view, cut, &token.attenuate(restriction(k)).unwrap(), wide())
                            .iter()
                            .all(Result::is_ok)
                    })
                });
                assert_eq!(
                    boundary[0], boundary[1],
                    "signed limit must ignore private history"
                );
                for k in [boundary[0].saturating_sub(1), boundary[0]] {
                    let limited = token.attenuate(restriction(k)).unwrap();
                    assert_eq!(
                        run(&views[0], cut, &limited, wide()),
                        run(&views[1], cut, &limited, wide())
                    );
                }
            }
            let native: [fn(u64) -> GqlQueryPolicy; 4] = [
                |k| GqlQueryPolicy::new(k, 1000, 1_000_000, 1_000_000),
                |k| GqlQueryPolicy::new(1000, k, 1_000_000, 1_000_000),
                |k| GqlQueryPolicy::new(1000, 1000, k, 1_000_000),
                |k| GqlQueryPolicy::new(1000, 1000, 1_000_000, k),
            ];
            for policy_at in native {
                let boundary = [&views[0], &views[1]].map(|view| {
                    threshold(&|k| {
                        run(view, cut, &token, policy_at(k))
                            .iter()
                            .all(Result::is_ok)
                    })
                });
                assert_eq!(
                    boundary[0], boundary[1],
                    "native meter must ignore private history"
                );
                for k in [boundary[0].saturating_sub(1), boundary[0]] {
                    assert_eq!(
                        run(&views[0], cut, &token, policy_at(k)),
                        run(&views[1], cut, &token, policy_at(k))
                    );
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn graph_probes_keep_the_indexed_source_instead_of_seeking_every_patch_per_probe() {
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};

    let ((), report) = run_async_under_lab(0x51cb_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = crate::MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..4 {
            batch.create_vertex(
                VId(id),
                vec![],
                vec![(KEY, CanonicalScalar::Int(id as i64))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let view = Database::open_read_view_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(view.snapshot.property_index.get().is_none());
        let prepared = PreparedGraphText::prepare(
            "MATCH (n) WHERE EXISTS { MATCH (m) WHERE m.p > n.p } RETURN n",
            |kind, _| match kind {
                GraphSymbolKind::Property => Some(GraphSymbol::Property(KEY)),
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let plan = VertexScanPlan::compile(prepared.plan()).unwrap();
        assert!(plan.has_graph_probes());
        let actual = view
            .stream_graph_values_governed(&cx, &prepared, wide())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            actual,
            [0, 1, 2]
                .into_iter()
                .map(|id| GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(id))]))
                .collect::<Vec<_>>(),
        );
        assert!(view.snapshot.property_index.get().is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// Paired maintenance measurement of the complete open/read/release path.
/// Both sources execute the same compiled operator over fresh authenticated
/// read views of the same durable UnixVfs fixture. The incumbent explicitly
/// retains the original Histories source; the candidate uses the normal source
/// selection. Fixture writes and logical preparation are outside the timer.
/// Kernel/filesystem caches are not flushed, so this is cold derived state,
/// not a cold-disk or competitor-throughput claim. Fixtures are retained.
/// Run with `cargo test -p fgdb --release --lib
/// benchmark_cold_unkeyed_streams -- --ignored --nocapture`.
#[test]
#[ignore = "manual paired production-source measurement; no wall-clock threshold"]
fn benchmark_cold_unkeyed_streams() {
    use asupersync::{Budget, runtime::RuntimeBuilder};
    use fgdb_gql::algebra::{GraphColumn, GraphValue};
    use fgdb_types::CommitCx;
    use std::path::Path;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    struct Timing {
        open: Duration,
        setup: Duration,
        first: Duration,
        remainder: Duration,
        release: Duration,
        total: Duration,
        work: u64,
        scratch: u64,
        records: u64,
    }

    async fn sample(
        commit: &CommitCx,
        cx: &QueryCx,
        path: &Path,
        prepared: &PreparedGraphPattern<GraphValueRow>,
        candidate: bool,
    ) -> (Vec<GraphValueRow>, Timing) {
        let total = Instant::now();
        let view = Database::open_read_view(commit, path, keys())
            .await
            .unwrap();
        let open = total.elapsed();
        assert!(view.snapshot.property_index.get().is_none());
        let setup_start = Instant::now();
        let as_of = view.frontier();
        view.snapshot.check_frontier(as_of).unwrap();
        let plan = VertexScanPlan::compile(prepared.plan()).unwrap();
        cx.with_restriction(|| cx.checkpoint()).unwrap();
        let source = SnapshotVertexSource {
            view: view.clone(),
            cx,
            as_of,
            after: None,
            candidates: RootCandidates::Histories(VertexHistoryScanCursor::default()),
        };
        let source = if candidate {
            source.with_root_equality(&plan, |_| true)
        } else {
            source
        };
        let mut cursor = VertexScanCursor::new(source, plan, wide(), || {
            cx.with_restriction(|| cx.checkpoint())
        });
        let setup = setup_start.elapsed();
        let first_start = Instant::now();
        let mut rows = Vec::new();
        if let Some(row) = cursor.next() {
            rows.push(row.unwrap());
        }
        let first = first_start.elapsed();
        let remainder_start = Instant::now();
        for row in cursor.by_ref() {
            rows.push(row.unwrap());
        }
        let remainder = remainder_start.elapsed();
        let work = cursor.evaluator_stats().work_units;
        let scratch = cursor.evaluator_stats().scratch_entries;
        let records = cursor.row_stats().snapshot_records;
        assert_eq!(view.snapshot.property_index.get().is_none(), candidate);
        let release_start = Instant::now();
        drop(cursor);
        drop(view);
        let release = release_start.elapsed();
        (
            rows,
            Timing {
                open,
                setup,
                first,
                remainder,
                release,
                total: total.elapsed(),
                work,
                scratch,
                records,
            },
        )
    }

    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    runtime.block_on(async {
        let commit = contexts.commit();
        let cx = contexts.query();
        for vertices in [1024_u128, 8192] {
            let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let path = std::env::temp_dir().join(format!(
                "fgdb-cold-stream-{}-{vertices}-{nonce}",
                std::process::id()
            ));
            let mut db = Database::create(&commit, &path, keys()).await.unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            let payload = "x".repeat(512);
            for id in 0..vertices {
                let mut properties = vec![(KEY, CanonicalScalar::Int(id as i64))];
                for key in 8..12 {
                    properties.push((
                        PropertyKeyId(key),
                        CanonicalScalar::ucs_basic_text(&format!("{id}:{key}:{payload}")).unwrap(),
                    ));
                }
                batch.create_vertex(VId(id), vec![LABEL], properties);
            }
            db.write(&commit, batch).await.unwrap();
            drop(db);
            eprintln!(
                "cold_stream_fixture vertices={vertices} text_properties=4 payload_bytes=512 durable=true path={}",
                path.display()
            );
            for (name, limit) in [("limit1", Some(1)), ("limit128", Some(128)), ("full", None)] {
                let mut builder = GraphPatternBuilder::new();
                builder.vertex("n").unwrap();
                let prepared = builder
                    .prepare_values(
                        &[
                            GraphColumn::vertex("n", "n"),
                            GraphColumn::property("value", "n", KEY),
                        ],
                        0,
                        limit,
                    )
                    .unwrap();
                let expected: Vec<_> = (0..vertices.min(u128::from(limit.unwrap_or(u64::MAX))))
                    .map(|id| {
                        GraphValueRow::from_owned_values(vec![
                            GraphValue::Vertex(VId(id)),
                            GraphValue::Scalar(CanonicalScalar::Int(id as i64)),
                        ])
                    })
                    .collect();
                let mut totals = [Vec::new(), Vec::new()];
                let mut first_rows = [Vec::new(), Vec::new()];
                for repeat in 0..5 {
                    for candidate in if repeat % 2 == 0 { [false, true] } else { [true, false] } {
                        let (rows, timing) = sample(&commit, &cx, &path, &prepared, candidate).await;
                        assert_eq!(rows, expected);
                        let source = if candidate { "patches" } else { "histories" };
                        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
                        eprintln!(
                            "cold_stream_sample vertices={vertices} query={name} repeat={repeat} source={source} rows={} open_ms={:.3} setup_ms={:.3} first_ms={:.3} remainder_ms={:.3} release_ms={:.3} total_ms={:.3} work={} scratch={} records={}",
                            rows.len(), ms(timing.open), ms(timing.setup), ms(timing.first),
                            ms(timing.remainder), ms(timing.release), ms(timing.total),
                            timing.work, timing.scratch, timing.records,
                        );
                        totals[usize::from(candidate)].push(timing.total);
                        first_rows[usize::from(candidate)].push(timing.first);
                    }
                }
                for timings in totals.iter_mut().chain(first_rows.iter_mut()) {
                    timings.sort_unstable();
                }
                eprintln!(
                    "cold_stream_maintenance vertices={vertices} query={name} pairs=5 equal_rows=true incumbent_total_ms={:.3} candidate_total_ms={:.3} incumbent_first_ms={:.3} candidate_first_ms={:.3}",
                    totals[0][2].as_secs_f64() * 1000.0,
                    totals[1][2].as_secs_f64() * 1000.0,
                    first_rows[0][2].as_secs_f64() * 1000.0,
                    first_rows[1][2].as_secs_f64() * 1000.0,
                );
            }
        }
    });
}
