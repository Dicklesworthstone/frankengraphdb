//! Native maintained joins must equal independently recomputed complete bags.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZSet};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::row_join::RowJoinBuildError;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId,
};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<GraphValue>, i128>;
const K: PropertyKeyId = PropertyKeyId(1);
const P: PropertyKeyId = PropertyKeyId(2);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn bound(rows: u64) -> GqlQueryPolicy {
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
fn prepare(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn input(
    db: &mut Database<MemVfs>,
    cx: &QueryCx,
    label: &str,
    budget: GqlQueryPolicy,
) -> StandingQueryHandle {
    db.register_standing_rows(
        cx,
        prepare(&format!("MATCH (n:{label}) RETURN n.k AS k, n.p AS p")),
        budget,
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
// Independent input enumeration from actual storage rows, not maintained state.
fn source(db: &Database<MemVfs>, label: u64) -> Bag {
    let mut out = Bag::new();
    for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
        if !row.labels.contains(&LabelId(label)) {
            continue;
        }
        let values = [K, P].map(|key| {
            GraphValue::Scalar(
                row.props
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map_or(CanonicalScalar::Null, |(_, value)| value.clone()),
            )
        });
        *out.entry(values.to_vec()).or_default() += 1;
    }
    out
}
fn product(left: &Bag, right: &Bag, keys: &[(usize, usize)]) -> Bag {
    let mut out = Bag::new();
    for (l, lw) in left {
        for (r, rw) in right {
            if keys
                .iter()
                .all(|&(a, b)| !l[a].is_null() && !r[b].is_null() && l[a] == r[b])
            {
                *out.entry(l.iter().chain(r).cloned().collect()).or_default() += lw * rw;
            }
        }
    }
    out
}
fn difference(next: &Bag, old: &Bag) -> Bag {
    let mut result = next.clone();
    for (row, weight) in old {
        *result.entry(row.clone()).or_default() -= weight;
    }
    result.retain(|_, weight| *weight != 0);
    result
}

#[test]
fn shared_and_nested_join_set_circuits_preserve_cross_terms_bags_nulls_and_exact_deltas() {
    let ((), report) = run_async_under_lab(0x726a_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            add(&mut seed, id, 1, Some(1), 10);
        }
        for id in 3..=5 {
            add(&mut seed, id, 2, Some(1), 20);
        }
        add(&mut seed, 6, 1, None, 10);
        add(&mut seed, 7, 2, None, 20);
        db.write(&commit, seed).await.unwrap();
        let left = input(&mut db, &cx, "L", policy());
        let right = input(&mut db, &cx, "R", policy());
        let join = db
            .register_standing_join(&cx, &left, &right, &[(0, 0)], policy())
            .unwrap();
        let twice = db
            .register_standing_set(&cx, &join, &join, SetOperation::UnionAll, policy())
            .unwrap();
        let nested = db
            .register_standing_join(&cx, &twice, &left, &[(0, 0)], policy())
            .unwrap();
        let self_join = db
            .register_standing_join(&cx, &left, &left, &[(0, 0), (1, 1)], policy())
            .unwrap();
        assert_eq!(
            db.standing_join_total(&cx, &join).unwrap().to_i128(),
            Some(6)
        );
        assert!(db.standing_join_delta(&cx, &join).unwrap().is_none());
        assert_eq!(
            db.standing_join_columns(&cx, &join).unwrap(),
            &["left.k", "left.p", "right.k", "right.p"]
        );
        let mut previous = plain(db.standing_join(&cx, &join).unwrap().rows());
        for step in 0..5 {
            let mut batch = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    batch.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(11)));
                    batch.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(21)));
                }
                1 => {
                    batch.set_vertex_property(VId(1), K, Some(CanonicalScalar::Int(2)));
                    batch.set_vertex_property(VId(3), K, Some(CanonicalScalar::Int(2)));
                }
                2 => {
                    batch.set_vertex_property(VId(6), K, Some(CanonicalScalar::Int(1)));
                    batch.set_vertex_property(VId(7), K, Some(CanonicalScalar::Int(1)));
                }
                3 => {
                    batch.delete_vertex(VId(2));
                    batch.delete_vertex(VId(4));
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
            let (l, r) = (source(&db, 1), source(&db, 2));
            let wanted = product(&l, &r, &[(0, 0)]);
            let double: Bag = wanted.iter().map(|(row, w)| (row.clone(), 2 * w)).collect();
            let result = db.standing_join(&cx, &join).unwrap();
            assert_eq!(result.frontier(), at);
            assert_eq!(plain(result.rows()), wanted);
            assert!(result.ordered_rows().is_none());
            assert_eq!(
                plain(db.standing_join_delta(&cx, &join).unwrap().unwrap().rows()),
                difference(&wanted, &previous)
            );
            assert_eq!(plain(db.standing_set(&cx, &twice).unwrap().rows()), double);
            assert_eq!(
                plain(db.standing_join(&cx, &nested).unwrap().rows()),
                product(&double, &l, &[(0, 0)])
            );
            assert_eq!(
                plain(db.standing_join(&cx, &self_join).unwrap().rows()),
                product(&l, &l, &[(0, 0), (1, 1)])
            );
            assert_eq!(
                db.standing_join_total(&cx, &join).unwrap().to_i128(),
                Some(wanted.values().sum())
            );
            if step == 4 {
                assert!(
                    db.standing_join_delta(&cx, &join)
                        .unwrap()
                        .unwrap()
                        .rows()
                        .is_empty()
                );
            }
            previous = wanted;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failures_fence_children_not_commits_and_rebuild_requires_healthy_parents_and_final_quota() {
    let ((), report) = run_async_under_lab(0x726a_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 1, 1, Some(1), 90);
        add(&mut seed, 2, 2, Some(1), 20);
        db.write(&commit, seed).await.unwrap();
        let low = input(&mut db, &cx, "L", bound(1));
        let high = input(&mut db, &cx, "L", policy());
        let right = input(&mut db, &cx, "R", policy());
        let dependent = db
            .register_standing_join(&cx, &low, &right, &[(0, 0)], policy())
            .unwrap();
        let limited = db
            .register_standing_join(&cx, &high, &right, &[(0, 0)], bound(1))
            .unwrap();
        let healthy = db
            .register_standing_join(&cx, &high, &right, &[(0, 0)], policy())
            .unwrap();
        let mut swap = WriteBatch::new(RelationId(1));
        swap.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
        db.write(&commit, swap).await.unwrap();
        // Insertion sorts before retraction, but final occurrence count is one.
        assert_eq!(
            db.standing_join_total(&cx, &limited).unwrap().to_i128(),
            Some(1)
        );
        let basis = db.frontier().unwrap();
        let mut grow = WriteBatch::new(RelationId(1));
        add(&mut grow, 3, 1, Some(1), 30);
        let at = db.write(&commit, grow).await.unwrap();
        assert!(at > basis);
        assert!(matches!(
            db.standing_join(&cx, &dependent),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        assert!(matches!(
            db.standing_join(&cx, &limited),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert_eq!(
            db.standing_join_total(&cx, &healthy).unwrap().to_i128(),
            Some(2)
        );
        assert!(
            db.rebuild_standing_query(&cx, &dependent, policy())
                .is_err()
        );
        db.rebuild_standing_query(&cx, &low, policy()).unwrap();
        assert!(matches!(
            db.rebuild_standing_query(&cx, &dependent, bound(1)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(db.standing_join(&cx, &dependent).is_err());
        for handle in [&dependent, &limited] {
            assert_eq!(
                db.rebuild_standing_query(&cx, handle, policy()).unwrap(),
                at
            );
            assert!(db.standing_join_delta(&cx, handle).unwrap().is_none());
        }
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(22)));
        db.write(&commit, update).await.unwrap();
        let expected = product(&source(&db, 1), &source(&db, 2), &[(0, 0)]);
        for handle in [&dependent, &limited, &healthy] {
            assert_eq!(
                plain(db.standing_join(&cx, handle).unwrap().rows()),
                expected
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_schemas_full_width_ids_and_ownership_survive_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x726a_0103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let vertex = db
            .register_standing_rows(&cx, prepare("MATCH (n) RETURN n AS id"), policy())
            .unwrap();
        let scalar = db
            .register_standing_rows(&cx, prepare("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_join(&cx, &vertex, &scalar, &[(0, 0)], policy()),
            Err(StandingQueryError::JoinSchema(
                RowJoinBuildError::KeyType { .. }
            ))
        ));
        assert!(matches!(
            db.register_standing_join(&cx, &vertex, &vertex, &[], policy()),
            Err(StandingQueryError::JoinSchema(RowJoinBuildError::EmptyKeys))
        ));
        assert!(matches!(
            db.register_standing_join(&cx, &vertex, &vertex, &[(1, 0)], policy()),
            Err(StandingQueryError::JoinSchema(
                RowJoinBuildError::KeyColumn { .. }
            ))
        ));
        let join = db
            .register_standing_join(&cx, &vertex, &vertex, &[(0, 0)], policy())
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(u128::MAX), vec![], vec![]);
        seed.create_vertex(VId(0), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let wanted = Bag::from([
            (
                vec![GraphValue::Vertex(VId(0)), GraphValue::Vertex(VId(0))],
                1,
            ),
            (
                vec![
                    GraphValue::Vertex(VId(u128::MAX)),
                    GraphValue::Vertex(VId(u128::MAX)),
                ],
                1,
            ),
        ]);
        assert_eq!(plain(db.standing_join(&cx, &join).unwrap().rows()), wanted);
        assert!(matches!(
            db.standing_join(&cx, &vertex),
            Err(StandingQueryError::Unsupported)
        ));
        let foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.standing_join(&cx, &join),
            Err(StandingQueryError::ForeignHandle)
        ));
        db.compact(&commit).await.unwrap();
        assert_eq!(plain(db.standing_join(&cx, &join).unwrap().rows()), wanted);
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_join(&cx, &join),
            Err(StandingQueryError::ForeignHandle)
        ));
        let vertex = db
            .register_standing_rows(&cx, prepare("MATCH (n) RETURN n AS id"), policy())
            .unwrap();
        let reopened = db
            .register_standing_join(&cx, &vertex, &vertex, &[(0, 0)], policy())
            .unwrap();
        assert_eq!(
            plain(db.standing_join(&cx, &reopened).unwrap().rows()),
            wanted
        );
        assert_eq!(db.delta_since(CommitSeq(0)).unwrap().count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_key_maintenance_does_not_visit_unrelated_groups() {
    let ((), report) = run_async_under_lab(0x726a_0104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut outcomes = Vec::new();
        for size in [2, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for i in 0..size {
                add(&mut seed, (2 * i + 1) as u128, 1, Some(i), 10);
                add(&mut seed, (2 * i + 2) as u128, 2, Some(i), 20);
            }
            db.write(&commit, seed).await.unwrap();
            let left = input(&mut db, &cx, "L", policy());
            let right = input(&mut db, &cx, "R", policy());
            let join = db
                .register_standing_join(&cx, &left, &right, &[(0, 0)], policy())
                .unwrap();
            let mut change = WriteBatch::new(RelationId(1));
            change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
            db.write(&commit, change).await.unwrap();
            let view = db.standing_join(&cx, &join).unwrap();
            outcomes.push((
                *view.last_maintenance(),
                plain(db.standing_join_delta(&cx, &join).unwrap().unwrap().rows()),
            ));
        }
        assert_eq!(outcomes[0], outcomes[1]);
        assert_eq!(outcomes[0].0.delta_rows, 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
