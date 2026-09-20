//! Maintained outer/existence joins over real committed graph changes.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZSet};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::row_join::{RowJoinBuildError, RowJoinKind};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<GraphValue>, i128>;
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn bounded(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy {
        rows: fgdb_gql::GqlExecutionBudget::new(100_000, rows),
        ..policy()
    }
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(K)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn input(db: &mut Database<MemVfs>, cx: &QueryCx, label: &str) -> StandingQueryHandle {
    db.register_standing_rows(
        cx,
        definition(&format!("MATCH (n:{label}) RETURN n.k AS k, n.p AS p")),
        policy(),
    )
    .unwrap()
}
fn add(batch: &mut WriteBatch, id: u128, label: u64, key: Option<i64>, value: i64) {
    let mut props = Vec::new();
    if let Some(key) = key {
        props.push((K, CanonicalScalar::Int(key)));
    }
    props.push((P, CanonicalScalar::Int(value)));
    batch.create_vertex(VId(id), vec![LabelId(label)], props);
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (row.values().to_vec(), weight.to_i128().unwrap()))
        .collect()
}
// Independent full input scan, not maintained rows or the incremental kernels.
fn source(db: &Database<MemVfs>, label: u64) -> Bag {
    let mut rows = Bag::new();
    for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
        if row.labels.contains(&LabelId(label)) {
            let values = [K, P].map(|key| {
                GraphValue::Scalar(
                    row.props
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map_or(CanonicalScalar::Null, |(_, value)| value.clone()),
                )
            });
            *rows.entry(values.to_vec()).or_default() += 1;
        }
    }
    rows
}
fn evaluate(
    kind: RowJoinKind,
    left: &Bag,
    right: &Bag,
    keys: &[(usize, usize)],
    right_width: usize,
) -> Bag {
    let mut output = Bag::new();
    for (left, lw) in left {
        let matches: Vec<_> = right
            .iter()
            .filter(|(right, _)| {
                keys.iter()
                    .all(|&(a, b)| !left[a].is_null() && !right[b].is_null() && left[a] == right[b])
            })
            .collect();
        match kind {
            RowJoinKind::Inner | RowJoinKind::Left if !matches.is_empty() => {
                for (right, rw) in matches {
                    *output
                        .entry(left.iter().chain(right).cloned().collect())
                        .or_default() += lw * rw;
                }
            }
            RowJoinKind::Left => {
                let mut row = left.clone();
                row.extend((0..right_width).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
                *output.entry(row).or_default() += lw;
            }
            RowJoinKind::Semi if !matches.is_empty() => {
                *output.entry(left.clone()).or_default() += lw;
            }
            RowJoinKind::Anti if matches.is_empty() => {
                *output.entry(left.clone()).or_default() += lw;
            }
            _ => {}
        }
    }
    output
}
fn difference(next: &Bag, previous: &Bag) -> Bag {
    let mut result = next.clone();
    for (row, weight) in previous {
        *result.entry(row.clone()).or_default() -= weight;
    }
    result.retain(|_, weight| *weight != 0);
    result
}

#[test]
fn first_last_duplicate_witnesses_and_simultaneous_changes_propagate_through_composed_views() {
    let ((), report) = run_async_under_lab(0x726a_0301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [1, 2] {
            add(&mut seed, id, 1, Some(1), 10);
        }
        for id in [3, 4] {
            add(&mut seed, id, 2, Some(1), 20);
        }
        add(&mut seed, 5, 1, Some(2), 30);
        add(&mut seed, 6, 1, None, 60);
        add(&mut seed, 7, 2, None, 70);
        db.write(&commit, seed).await.unwrap();
        let left = input(&mut db, &cx, "L");
        let right = input(&mut db, &cx, "R");
        let kinds = [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti];
        let mut joins = Vec::new();
        let mut previous = Vec::new();
        for kind in kinds {
            let handle = db
                .register_standing_join_with_kind(&cx, &left, &right, &[(0, 0)], kind, policy())
                .unwrap();
            assert_eq!(db.standing_join_kind(&cx, &handle).unwrap(), kind);
            assert!(db.standing_join_delta(&cx, &handle).unwrap().is_none());
            let names = db.standing_join_columns(&cx, &handle).unwrap();
            assert_eq!(names.len(), if kind == RowJoinKind::Left { 4 } else { 2 });
            assert_eq!(&names[..2], &["left.k", "left.p"]);
            let expected = evaluate(kind, &source(&db, 1), &source(&db, 2), &[(0, 0)], 2);
            assert_eq!(
                plain(db.standing_join(&cx, &handle).unwrap().rows()),
                expected
            );
            previous.push(expected);
            joins.push(handle);
        }
        // Semi and Anti partition the complete left bag, including NULL keys.
        let partition = db
            .register_standing_set(&cx, &joins[1], &joins[2], SetOperation::UnionAll, policy())
            .unwrap();
        // A NULL-extended frame is a legal child input, but cannot be a witness.
        let nested = db
            .register_standing_join_with_kind(
                &cx,
                &joins[0],
                &right,
                &[(2, 0)],
                RowJoinKind::Semi,
                policy(),
            )
            .unwrap();
        for step in 0..6 {
            let mut batch = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    batch.delete_vertex(VId(3));
                }
                1 => {
                    batch.delete_vertex(VId(4));
                    add(&mut batch, 8, 2, Some(1), 21);
                    batch.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(11)));
                }
                2 => {
                    batch.delete_vertex(VId(8));
                }
                3 => {
                    add(&mut batch, 9, 2, Some(2), 40);
                    batch.set_vertex_property(VId(1), K, Some(CanonicalScalar::Int(2)));
                }
                4 => {
                    batch.delete_vertex(VId(6));
                    batch.delete_vertex(VId(7));
                }
                _ => {
                    batch.set_vertex_property(
                        VId(1),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Int(1)),
                    );
                }
            }
            let at = db.write(&commit, batch).await.unwrap();
            let l = source(&db, 1);
            let r = source(&db, 2);
            for (index, kind) in kinds.into_iter().enumerate() {
                let expected = evaluate(kind, &l, &r, &[(0, 0)], 2);
                let result = db.standing_join(&cx, &joins[index]).unwrap();
                assert_eq!(result.frontier(), at);
                assert_eq!(plain(result.rows()), expected);
                let delta = db.standing_join_delta(&cx, &joins[index]).unwrap().unwrap();
                assert_eq!(delta.frontier(), at);
                assert_eq!(plain(delta.rows()), difference(&expected, &previous[index]));
                if step == 5 {
                    assert!(delta.rows().is_empty());
                }
                assert_eq!(
                    db.standing_join_total(&cx, &joins[index])
                        .unwrap()
                        .to_i128(),
                    Some(expected.values().sum())
                );
                previous[index] = expected;
            }
            assert_eq!(plain(db.standing_set(&cx, &partition).unwrap().rows()), l);
            assert_eq!(
                plain(db.standing_join(&cx, &nested).unwrap().rows()),
                evaluate(RowJoinKind::Semi, &previous[0], &r, &[(2, 0)], 2)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn quota_failure_isolated_from_durable_commits_and_rebuild_retains_each_join_kind() {
    let ((), report) = run_async_under_lab(0x726a_0302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for kind in [RowJoinKind::Left, RowJoinKind::Semi, RowJoinKind::Anti] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            add(&mut seed, 1, 1, Some(1), 10);
            let matching = kind == RowJoinKind::Semi;
            add(&mut seed, 2, 2, Some(if matching { 1 } else { 2 }), 20);
            db.write(&commit, seed).await.unwrap();
            let left = input(&mut db, &cx, "L");
            let right = input(&mut db, &cx, "R");
            let limited = db
                .register_standing_join_with_kind(&cx, &left, &right, &[(0, 0)], kind, bounded(1))
                .unwrap();
            let healthy = db
                .register_standing_join_with_kind(&cx, &left, &right, &[(0, 0)], kind, policy())
                .unwrap();
            let child = db
                .register_standing_set(&cx, &limited, &limited, SetOperation::UnionAll, policy())
                .unwrap();
            let before = plain(db.standing_join(&cx, &limited).unwrap().rows());
            assert!(matches!(
                db.rebuild_standing_query(&cx, &limited, bounded(0)),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::ResultBudget
                ))
            ));
            assert_eq!(
                plain(db.standing_join(&cx, &limited).unwrap().rows()),
                before
            );
            assert_eq!(db.standing_join_kind(&cx, &limited).unwrap(), kind);
            let mut grow = WriteBatch::new(RelationId(1));
            add(&mut grow, 3, 1, Some(1), 30);
            let at = db.write(&commit, grow).await.unwrap();
            assert_eq!(db.frontier().unwrap(), at);
            assert!(matches!(
                db.standing_join(&cx, &limited),
                Err(StandingQueryError::Unavailable {
                    reason: StandingQueryFailure::ResultBudget,
                    ..
                })
            ));
            assert!(matches!(
                db.standing_set(&cx, &child),
                Err(StandingQueryError::Unavailable {
                    reason: StandingQueryFailure::DependencyUnavailable,
                    ..
                })
            ));
            assert_eq!(
                db.standing_join_total(&cx, &healthy).unwrap().to_i128(),
                Some(2)
            );
            assert!(db.rebuild_standing_query(&cx, &child, policy()).is_err());
            assert!(
                db.rebuild_standing_query(&cx, &limited, bounded(1))
                    .is_err()
            );
            assert_eq!(
                db.rebuild_standing_query(&cx, &limited, policy()).unwrap(),
                at
            );
            assert_eq!(db.standing_join_kind(&cx, &limited).unwrap(), kind);
            assert!(db.standing_join_delta(&cx, &limited).unwrap().is_none());
            db.rebuild_standing_query(&cx, &child, policy()).unwrap();
            let mut next = WriteBatch::new(RelationId(1));
            next.set_vertex_property(
                VId(2),
                K,
                Some(CanonicalScalar::Int(if matching { 2 } else { 1 })),
            );
            db.write(&commit, next).await.unwrap();
            let expected = evaluate(kind, &source(&db, 1), &source(&db, 2), &[(0, 0)], 2);
            for handle in [&limited, &healthy] {
                assert_eq!(
                    plain(db.standing_join(&cx, handle).unwrap().rows()),
                    expected
                );
            }
            let twice: Bag = expected
                .iter()
                .map(|(row, count)| (row.clone(), count * 2))
                .collect();
            assert_eq!(plain(db.standing_set(&cx, &child).unwrap().rows()), twice);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn nullable_vertex_schemas_compose_and_old_handles_refuse_after_reopen() {
    let ((), report) = run_async_under_lab(0x726a_0303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(0), vec![LabelId(1), LabelId(2)], vec![]);
        seed.create_vertex(VId(u128::MAX), vec![LabelId(1)], vec![]);
        db.write(&commit, seed).await.unwrap();
        let left = db
            .register_standing_rows(&cx, definition("MATCH (n:L) RETURN n AS id"), policy())
            .unwrap();
        let right = db
            .register_standing_rows(&cx, definition("MATCH (n:R) RETURN n AS id"), policy())
            .unwrap();
        let outer = db
            .register_standing_join_with_kind(
                &cx,
                &left,
                &right,
                &[(0, 0)],
                RowJoinKind::Left,
                policy(),
            )
            .unwrap();
        let present = db
            .register_standing_join_with_kind(
                &cx,
                &outer,
                &right,
                &[(1, 0)],
                RowJoinKind::Semi,
                policy(),
            )
            .unwrap();
        let absent = db
            .register_standing_join_with_kind(
                &cx,
                &outer,
                &right,
                &[(1, 0)],
                RowJoinKind::Anti,
                policy(),
            )
            .unwrap();
        let combined = db
            .register_standing_set(&cx, &present, &absent, SetOperation::UnionAll, policy())
            .unwrap();
        let expected = Bag::from([
            (
                vec![GraphValue::Vertex(VId(0)), GraphValue::Vertex(VId(0))],
                1,
            ),
            (
                vec![
                    GraphValue::Vertex(VId(u128::MAX)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ],
                1,
            ),
        ]);
        assert_eq!(
            plain(db.standing_join(&cx, &outer).unwrap().rows()),
            expected
        );
        assert_eq!(
            plain(db.standing_set(&cx, &combined).unwrap().rows()),
            expected
        );
        let scalar = input(&mut db, &cx, "L");
        assert!(matches!(
            db.register_standing_join_with_kind(
                &cx,
                &left,
                &scalar,
                &[(0, 0)],
                RowJoinKind::Anti,
                policy()
            ),
            Err(StandingQueryError::JoinSchema(
                RowJoinBuildError::KeyType { .. }
            ))
        ));
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.register_standing_join_with_kind(
                &cx,
                &left,
                &right,
                &[(0, 0)],
                RowJoinKind::Left,
                policy()
            ),
            Err(StandingQueryError::ForeignHandle)
        ));
        db.compact(&commit).await.unwrap();
        assert_eq!(
            plain(db.standing_join(&cx, &outer).unwrap().rows()),
            expected
        );
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_join_kind(&cx, &outer),
            Err(StandingQueryError::ForeignHandle)
        ));
        let left = db
            .register_standing_rows(&cx, definition("MATCH (n:L) RETURN n AS id"), policy())
            .unwrap();
        let right = db
            .register_standing_rows(&cx, definition("MATCH (n:R) RETURN n AS id"), policy())
            .unwrap();
        let restored = db
            .register_standing_join_with_kind(
                &cx,
                &left,
                &right,
                &[(0, 0)],
                RowJoinKind::Left,
                policy(),
            )
            .unwrap();
        assert_eq!(
            plain(db.standing_join(&cx, &restored).unwrap().rows()),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_witness_decrements_do_not_scan_the_same_key_cartesian_product() {
    let ((), report) = run_async_under_lab(0x726a_0304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for kind in [RowJoinKind::Semi, RowJoinKind::Anti] {
            let mut measured = Vec::new();
            for size in [2_u128, 128] {
                let mut db = Database::open_memory(&commit, keys()).await.unwrap();
                let mut seed = WriteBatch::new(RelationId(1));
                for id in 0..size {
                    add(&mut seed, id + 1, 1, Some(1), id as i64);
                    add(&mut seed, size + id + 1, 2, Some(1), id as i64);
                }
                db.write(&commit, seed).await.unwrap();
                let left = input(&mut db, &cx, "L");
                let right = input(&mut db, &cx, "R");
                let join = db
                    .register_standing_join_with_kind(&cx, &left, &right, &[(0, 0)], kind, policy())
                    .unwrap();
                let mut remove = WriteBatch::new(RelationId(1));
                remove.delete_vertex(VId(size + 1));
                db.write(&commit, remove).await.unwrap();
                let view = db.standing_join(&cx, &join).unwrap();
                measured.push(*view.last_maintenance());
                assert_eq!(
                    plain(view.rows()),
                    evaluate(kind, &source(&db, 1), &source(&db, 2), &[(0, 0)], 2)
                );
                assert!(
                    db.standing_join_delta(&cx, &join)
                        .unwrap()
                        .unwrap()
                        .rows()
                        .is_empty()
                );
            }
            assert_eq!(measured[0], measured[1]);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
