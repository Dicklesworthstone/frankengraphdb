//! Ordered maintained pages against independent complete-storage sorting.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, WriteBatch};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{PropertyKeyId, RelationId, ZSet};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSetQuantifier, GraphSymbol, GraphSymbolKind,
    PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<GraphValue>, i128>;
const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc4; 32],
        DatabaseSecurityNamespaceId([0xc5; 32]),
        [0xc6; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, u64::MAX, 20_000_000, 20_000_000)
}
fn quota(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 20_000_000, 20_000_000)
}
fn definition() -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", |kind, name: &str| {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
            _ => None,
        }
    })
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}
fn add(batch: &mut WriteBatch, id: u128, value: Option<i64>) {
    batch.create_vertex(
        VId(id),
        vec![],
        value
            .map(|v| vec![(P, CanonicalScalar::Int(v))])
            .unwrap_or_default(),
    );
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (row.values().to_vec(), weight.to_i128().unwrap()))
        .collect()
}
fn bag(rows: &[Vec<GraphValue>]) -> Bag {
    let mut out = Bag::new();
    for row in rows {
        *out.entry(row.clone()).or_default() += 1;
    }
    out
}
fn difference(new: &Bag, old: &Bag) -> Bag {
    let mut result = new.clone();
    for (row, weight) in old {
        *result.entry(row.clone()).or_default() -= weight;
    }
    result.retain(|_, weight| *weight != 0);
    result
}
fn reference(
    db: &Database<MemVfs>,
    distinct: bool,
    descending: bool,
    nulls_first: bool,
) -> Vec<Vec<GraphValue>> {
    let mut rows: Vec<_> = db
        .vertices_at(db.frontier().unwrap())
        .unwrap()
        .into_iter()
        .map(|vertex| {
            let value = vertex
                .props
                .iter()
                .find(|(key, _)| *key == P)
                .map_or(CanonicalScalar::Null, |(_, value)| value.clone());
            vec![GraphValue::Scalar(value)]
        })
        .collect();
    rows.sort_by(|a, b| {
        let an = a[0].is_null();
        let bn = b[0].is_null();
        if an != bn {
            return if nulls_first {
                bn.cmp(&an)
            } else {
                an.cmp(&bn)
            };
        }
        if descending { b.cmp(a) } else { a.cmp(b) }
    });
    if distinct {
        rows.dedup();
    }
    rows.into_iter().skip(1).take(3).collect()
}

#[test]
fn windows_refill_after_deletions_and_feed_nested_bag_consumers() {
    let ((), report) = run_async_under_lab(0x7769_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [
            (0, None),
            (1, Some(7)),
            (2, Some(7)),
            (3, Some(2)),
            (u128::MAX, Some(9)),
        ] {
            add(&mut seed, id, value);
        }
        db.write(&commit, seed).await.unwrap();
        let source = db
            .register_standing_rows(&cx, definition(), policy())
            .unwrap();
        let modes = [
            (false, true, false),
            (true, true, false),
            (false, false, true),
            (true, false, true),
        ];
        let mut windows = Vec::new();
        for (distinct, descending, nulls_first) in modes {
            windows.push(
                db.register_standing_window(
                    &cx,
                    &source,
                    &[GraphValueOrder {
                        column: 0,
                        descending,
                        nulls_first,
                    }],
                    if distinct {
                        GraphSetQuantifier::Distinct
                    } else {
                        GraphSetQuantifier::All
                    },
                    1,
                    3,
                    quota(3),
                )
                .unwrap(),
            );
        }
        let twice = db
            .register_standing_set(
                &cx,
                &windows[0],
                &windows[0],
                SetOperation::UnionAll,
                policy(),
            )
            .unwrap();
        let nested = db
            .register_standing_window(&cx, &twice, &[], GraphSetQuantifier::All, 1, 2, quota(2))
            .unwrap();
        let mut previous: Vec<_> = windows
            .iter()
            .map(|h| plain(db.standing_window(&cx, h).unwrap().rows()))
            .collect();
        for h in &windows {
            assert!(db.standing_window_delta(&cx, h).unwrap().is_none());
        }
        for step in 0..5 {
            let mut change = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    change.delete_vertex(VId(u128::MAX));
                }
                1 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(1)));
                }
                2 => {
                    change.delete_vertex(VId(1));
                    add(&mut change, 6, Some(8));
                }
                3 => {
                    change.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(3)));
                }
                _ => {
                    change.set_vertex_property(
                        VId(3),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            for (index, h) in windows.iter().enumerate() {
                let (distinct, descending, nulls_first) = modes[index];
                let ordered = reference(&db, distinct, descending, nulls_first);
                let wanted = bag(&ordered);
                let view = db.standing_window(&cx, h).unwrap();
                assert_eq!(view.frontier(), at);
                assert_eq!(plain(view.rows()), wanted);
                assert!(view.ordered_rows().is_none());
                let expanded: Vec<_> = db
                    .standing_window_ordered(&cx, h)
                    .unwrap()
                    .flat_map(|(row, weight)| {
                        std::iter::repeat_n(
                            row.values().to_vec(),
                            weight.to_i128().unwrap() as usize,
                        )
                    })
                    .collect();
                assert_eq!(expanded, ordered);
                assert_eq!(
                    plain(db.standing_window_delta(&cx, h).unwrap().unwrap().rows()),
                    difference(&wanted, &previous[index])
                );
                assert_eq!(
                    db.standing_window_total(&cx, h).unwrap().to_i128(),
                    Some(ordered.len() as i128)
                );
                assert_eq!(db.standing_window_columns(&cx, h).unwrap(), &["p"]);
                previous[index] = wanted;
            }
            let mut expanded: Vec<_> = previous[0]
                .iter()
                .flat_map(|(row, weight)| std::iter::repeat_n(row.clone(), (2 * weight) as usize))
                .collect();
            expanded.sort();
            let wanted: Vec<_> = expanded.into_iter().skip(1).take(2).collect();
            assert_eq!(
                plain(db.standing_window(&cx, &nested).unwrap().rows()),
                bag(&wanted)
            );
        }
        assert_eq!(
            db.standing_window(&cx, &windows[0])
                .unwrap()
                .last_maintenance()
                .delta_rows,
            0
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn quotas_fence_only_failed_views_and_rebuild_preserves_window_definition() {
    let ((), report) = run_async_under_lab(0x7769_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 1, Some(7));
        let basis = db.write(&commit, seed).await.unwrap();
        let source = db
            .register_standing_rows(&cx, definition(), policy())
            .unwrap();
        let order = [GraphValueOrder {
            column: 0,
            descending: true,
            nulls_first: false,
        }];
        let window = db
            .register_standing_window(
                &cx,
                &source,
                &order,
                GraphSetQuantifier::All,
                0,
                2,
                quota(1),
            )
            .unwrap();
        let child = db
            .register_standing_window(&cx, &window, &[], GraphSetQuantifier::All, 0, 1, quota(1))
            .unwrap();
        let zero = db
            .register_standing_window(
                &cx,
                &source,
                &order,
                GraphSetQuantifier::All,
                0,
                0,
                quota(0),
            )
            .unwrap();
        let mut growth = WriteBatch::new(RelationId(1));
        add(&mut growth, 2, Some(9));
        let at = db.write(&commit, growth).await.unwrap();
        assert!(
            matches!(db.standing_window(&cx, &window), Err(StandingQueryError::Unavailable {
            frontier, reason: StandingQueryFailure::ResultBudget }) if frontier == basis)
        );
        assert!(matches!(
            db.standing_window(&cx, &child),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        assert_eq!(db.standing_window(&cx, &zero).unwrap().frontier(), at);
        assert!(db.standing_window(&cx, &zero).unwrap().rows().is_empty());
        assert_eq!(db.standing_rows(&cx, &source).unwrap().frontier(), at);
        assert!(db.rebuild_standing_query(&cx, &window, quota(1)).is_err());
        assert!(
            matches!(db.standing_window(&cx, &window), Err(StandingQueryError::Unavailable { frontier, .. }) if frontier == basis)
        );
        db.rebuild_standing_query(&cx, &window, quota(2)).unwrap();
        assert!(db.standing_window_delta(&cx, &window).unwrap().is_none());
        let ordered: Vec<_> = db
            .standing_window_ordered(&cx, &window)
            .unwrap()
            .map(|(row, _)| row.values()[0].clone())
            .collect();
        assert_eq!(
            ordered,
            [
                GraphValue::Scalar(CanonicalScalar::Int(9)),
                GraphValue::Scalar(CanonicalScalar::Int(7))
            ]
        );
        db.rebuild_standing_query(&cx, &child, quota(1)).unwrap();
        let mut swap = WriteBatch::new(RelationId(1));
        swap.delete_vertex(VId(2));
        add(&mut swap, 3, Some(1));
        db.write(&commit, swap).await.unwrap();
        assert_eq!(
            db.standing_window_total(&cx, &window).unwrap().to_i128(),
            Some(2)
        );
        assert_eq!(
            db.standing_window_total(&cx, &child).unwrap().to_i128(),
            Some(1)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn million_occurrence_pages_remain_compressed_and_offsets_do_not_expand_runs() {
    let ((), report) = run_async_under_lab(0x7769_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=4 {
            add(&mut seed, id, Some(7));
        }
        db.write(&commit, seed).await.unwrap();
        let mut source = db
            .register_standing_rows(&cx, definition(), policy())
            .unwrap();
        for _ in 0..20 {
            source = db
                .register_standing_set(&cx, &source, &source, SetOperation::UnionAll, policy())
                .unwrap();
        }
        let window = db
            .register_standing_window(
                &cx,
                &source,
                &[],
                GraphSetQuantifier::All,
                1_000_000,
                2_000_000,
                quota(2_000_000),
            )
            .unwrap();
        assert_eq!(db.standing_window(&cx, &window).unwrap().rows().len(), 1);
        let mut ordered = db.standing_window_ordered(&cx, &window).unwrap();
        assert_eq!(ordered.len(), 1);
        assert_eq!(ordered.next().unwrap().1.to_i128(), Some(2_000_000));
        assert!(ordered.next().is_none());
        drop(ordered);
        let distinct = db
            .register_standing_window(
                &cx,
                &source,
                &[],
                GraphSetQuantifier::Distinct,
                0,
                2,
                quota(1),
            )
            .unwrap();
        let mut remove = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            remove.delete_vertex(VId(id));
        }
        db.write(&commit, remove).await.unwrap();
        assert_eq!(
            db.standing_window_total(&cx, &window).unwrap().to_i128(),
            Some(48_576)
        );
        assert_eq!(
            db.standing_window_total(&cx, &distinct).unwrap().to_i128(),
            Some(1)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn owner_schema_and_source_admission_remain_exact_through_compaction_reopen() {
    let ((), report) = run_async_under_lab(0x7769_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 0, None);
        add(&mut seed, u128::MAX, Some(9));
        db.write(&commit, seed).await.unwrap();
        let source = db
            .register_standing_rows(&cx, definition(), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_window(
                &cx,
                &source,
                &[GraphValueOrder {
                    column: 1,
                    descending: false,
                    nulls_first: false
                }],
                GraphSetQuantifier::All,
                0,
                0,
                policy()
            ),
            Err(StandingQueryError::WindowSchema(_))
        ));
        assert!(matches!(
            db.register_standing_window(
                &cx,
                &source,
                &[],
                GraphSetQuantifier::All,
                0,
                0,
                GqlQueryPolicy::new(1, 0, 20_000_000, 20_000_000)
            ),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
        let window = db
            .register_standing_window(&cx, &source, &[], GraphSetQuantifier::All, 0, 1, quota(1))
            .unwrap();
        let expected = plain(db.standing_window(&cx, &window).unwrap().rows());
        assert!(matches!(
            db.standing_window(&cx, &source),
            Err(StandingQueryError::Unsupported)
        ));
        db.compact(&commit).await.unwrap();
        db.rebuild_standing_query(&cx, &window, quota(1)).unwrap();
        assert_eq!(
            plain(db.standing_window(&cx, &window).unwrap().rows()),
            expected
        );
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_window(&cx, &window),
            Err(StandingQueryError::ForeignHandle)
        ));
        let source = db
            .register_standing_rows(&cx, definition(), policy())
            .unwrap();
        let next = db
            .register_standing_window(&cx, &source, &[], GraphSetQuantifier::All, 0, 1, quota(1))
            .unwrap();
        assert_eq!(
            plain(db.standing_window(&cx, &next).unwrap().rows()),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
