//! Exact grouped statistics consume complete committed upstream row derivatives.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::row_aggregate::{RowAggregateBuildError, RowAggregateRow};
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet};

const K: PropertyKeyId = PropertyKeyId(1);
const V: PropertyKeyId = PropertyKeyId(2);
type Bag = BTreeMap<Vec<GraphValue>, i128>;
type Signature = (
    Vec<GraphValue>,
    i128,
    i128,
    i128,
    Option<i128>,
    Option<i128>,
    Option<i128>,
    Option<i128>,
    Option<(i128, i128)>,
    Option<(i128, i128)>,
);
type Summaries = BTreeMap<Signature, i128>;
// Group key -> (total row weight, every integer value with its weight).
type OracleGroups = BTreeMap<Vec<GraphValue>, (i128, Vec<(i128, i128)>)>;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 30_000_000, 30_000_000)
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
        (GraphSymbolKind::Property, "v") => Some(GraphSymbol::Property(V)),
        _ => None,
    }
}
fn definition(text: &str) -> PreparedGraphPattern<GraphValueRow> {
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
        definition(&format!("MATCH (n:{label}) RETURN n.k AS k, n.v AS v")),
        budget,
    )
    .unwrap()
}
fn add(batch: &mut WriteBatch, id: u128, label: u64, key: Option<i64>, value: Option<i64>) {
    let mut props = Vec::new();
    if let Some(key) = key {
        props.push((K, CanonicalScalar::Int(key)));
    }
    if let Some(value) = value {
        props.push((V, CanonicalScalar::Int(value)));
    }
    batch.create_vertex(VId(id), vec![LabelId(label)], props);
}
fn source(db: &Database<MemVfs>, label: u64) -> Bag {
    let mut out = Bag::new();
    for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
        if row.labels.contains(&LabelId(label)) {
            let values = [K, V].map(|key| {
                GraphValue::Scalar(
                    row.props
                        .iter()
                        .find(|(k, _)| *k == key)
                        .map_or(CanonicalScalar::Null, |(_, value)| value.clone()),
                )
            });
            *out.entry(values.to_vec()).or_default() += 1;
        }
    }
    out
}
fn outer(left: &Bag, right: &Bag) -> Bag {
    let mut out = Bag::new();
    for (left, lw) in left {
        let matched: Vec<_> = right
            .iter()
            .filter(|(r, _)| !left[0].is_null() && !r[0].is_null() && left[0] == r[0])
            .collect();
        if matched.is_empty() {
            let mut row = left.clone();
            row.extend((0..2).map(|_| GraphValue::Scalar(CanonicalScalar::Null)));
            *out.entry(row).or_default() += lw;
        } else {
            for (right, rw) in matched {
                *out.entry(left.iter().chain(right).cloned().collect())
                    .or_default() += lw * rw;
            }
        }
    }
    out
}
// Independent full bag grouping: never invokes maintained or aggregate code.
fn oracle(input: &Bag, keys: &[usize], column: usize) -> Summaries {
    let mut groups: OracleGroups = BTreeMap::new();
    if keys.is_empty() {
        groups.insert(vec![], (0, vec![]));
    }
    for (row, weight) in input {
        let group = groups
            .entry(keys.iter().map(|i| row[*i].clone()).collect())
            .or_default();
        group.0 += weight;
        if let GraphValue::Scalar(CanonicalScalar::Int(value)) = &row[column] {
            group.1.push((i128::from(*value), *weight));
        }
    }
    groups
        .into_iter()
        .map(|(key, (rows, values))| {
            let count: i128 = values.iter().map(|(_, w)| w).sum();
            let sum: i128 = values.iter().map(|(v, w)| v * w).sum();
            let distinct: BTreeSet<_> = values.iter().map(|(v, _)| *v).collect();
            let dc = distinct.len() as i128;
            let ds: i128 = distinct.iter().sum();
            (
                (
                    key,
                    rows,
                    count,
                    dc,
                    (count > 0).then_some(sum),
                    (dc > 0).then_some(ds),
                    distinct.first().copied(),
                    distinct.last().copied(),
                    (count > 0).then_some((sum, count)),
                    (dc > 0).then_some((ds, dc)),
                ),
                1,
            )
        })
        .collect()
}
fn observed(rows: &ZSet<RowAggregateRow>) -> Summaries {
    let n = |w: &ZWeight| w.to_i128().unwrap();
    rows.iter()
        .map(|(r, w)| {
            (
                (
                    r.keys().to_vec(),
                    n(r.count_rows()),
                    n(r.count_values()),
                    n(r.count_distinct()),
                    r.sum().map(n),
                    r.sum_distinct().map(n),
                    r.minimum(),
                    r.maximum(),
                    r.average_parts().map(|(a, b)| (n(a), n(b))),
                    r.average_distinct_parts().map(|(a, b)| (n(a), n(b))),
                ),
                n(w),
            )
        })
        .collect()
}
fn difference(next: &Summaries, old: &Summaries) -> Summaries {
    let mut out = next.clone();
    for (row, w) in old {
        *out.entry(row.clone()).or_default() -= w;
    }
    out.retain(|_, w| *w != 0);
    out
}

#[test]
fn grouped_join_and_distinct_set_statistics_match_storage_oracle_after_every_commit() {
    let ((), report) = run_async_under_lab(0x7261_0401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in [1, 2] {
            add(&mut seed, id, 1, Some(1), Some(10));
        }
        add(&mut seed, 3, 1, Some(2), None);
        add(&mut seed, 4, 1, None, Some(-1));
        for id in [11, 12] {
            add(&mut seed, id, 2, Some(1), Some(3));
        }
        add(&mut seed, 13, 2, Some(1), Some(7));
        add(&mut seed, 14, 2, None, Some(5));
        db.write(&commit, seed).await.unwrap();
        let left = input(&mut db, &cx, "L", policy());
        let right = input(&mut db, &cx, "R", policy());
        let join = db
            .register_standing_join_with_kind(
                &cx,
                &left,
                &right,
                &[(0, 0)],
                RowJoinKind::Left,
                policy(),
            )
            .unwrap();
        let twice = db
            .register_standing_set(&cx, &join, &join, SetOperation::UnionAll, policy())
            .unwrap();
        let unique = db
            .register_standing_set(&cx, &left, &left, SetOperation::UnionDistinct, policy())
            .unwrap();
        let joined = db
            .register_standing_reduction(&cx, &twice, &[0], 3, policy())
            .unwrap();
        let global = db
            .register_standing_reduction(&cx, &unique, &[], 1, policy())
            .unwrap();
        assert_eq!(
            db.standing_reduction_group_names(&cx, &joined).unwrap(),
            &["left.k"]
        );
        assert_eq!(
            db.standing_reduction_spec(&cx, &joined)
                .unwrap()
                .value_column(),
            3
        );
        assert!(db.standing_reduction_delta(&cx, &joined).unwrap().is_none());
        let expected = |db: &Database<MemVfs>| {
            let left = source(db, 1);
            let right = source(db, 2);
            let double: Bag = outer(&left, &right)
                .into_iter()
                .map(|(r, w)| (r, 2 * w))
                .collect();
            let unique: Bag = left.into_keys().map(|r| (r, 1)).collect();
            [oracle(&double, &[0], 3), oracle(&unique, &[], 1)]
        };
        let mut previous = expected(&db);
        for (i, handle) in [&joined, &global].into_iter().enumerate() {
            assert_eq!(
                observed(db.standing_reduction(&cx, handle).unwrap().rows()),
                previous[i]
            );
        }
        for step in 0..6 {
            let mut batch = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    batch.delete_vertex(VId(11));
                }
                1 => {
                    batch.set_vertex_property(VId(12), V, Some(CanonicalScalar::Int(9)));
                }
                2 => {
                    batch.set_vertex_property(VId(2), K, Some(CanonicalScalar::Int(2)));
                    batch.set_vertex_property(VId(13), K, Some(CanonicalScalar::Int(2)));
                }
                3 => {
                    batch.delete_vertex(VId(12));
                }
                4 => {
                    batch.set_vertex_property(VId(3), V, Some(CanonicalScalar::Int(4)));
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
            let next = expected(&db);
            for (i, handle) in [&joined, &global].into_iter().enumerate() {
                let view = db.standing_reduction(&cx, handle).unwrap();
                assert_eq!(view.frontier(), at);
                assert_eq!(observed(view.rows()), next[i]);
                assert!(view.ordered_rows().is_none());
                let delta = db.standing_reduction_delta(&cx, handle).unwrap().unwrap();
                assert_eq!(delta.frontier(), at);
                assert_eq!(observed(delta.rows()), difference(&next[i], &previous[i]));
                if step == 5 {
                    assert!(delta.rows().is_empty());
                }
            }
            previous = next;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dependency_data_and_group_quota_failures_preserve_commits_and_require_explicit_repair() {
    let ((), report) = run_async_under_lab(0x7261_0402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, 1, 1, Some(1), Some(10));
        db.write(&commit, seed).await.unwrap();
        let low = input(&mut db, &cx, "L", bounded(1));
        let high = input(&mut db, &cx, "L", policy());
        let dependent = db
            .register_standing_reduction(&cx, &low, &[0], 1, policy())
            .unwrap();
        let limited = db
            .register_standing_reduction(&cx, &high, &[0], 1, bounded(1))
            .unwrap();
        let healthy = db
            .register_standing_reduction(&cx, &high, &[0], 1, policy())
            .unwrap();
        let before = observed(db.standing_reduction(&cx, &healthy).unwrap().rows());
        assert!(
            db.rebuild_standing_query(&cx, &healthy, bounded(0))
                .is_err()
        );
        assert_eq!(
            observed(db.standing_reduction(&cx, &healthy).unwrap().rows()),
            before
        );
        let mut grow = WriteBatch::new(RelationId(1));
        add(&mut grow, 2, 1, Some(2), Some(20));
        let at = db.write(&commit, grow).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(matches!(
            db.standing_reduction(&cx, &dependent),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        assert!(matches!(
            db.standing_reduction(&cx, &limited),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert_eq!(
            db.standing_reduction(&cx, &healthy).unwrap().rows().len(),
            2
        );
        assert!(
            db.rebuild_standing_query(&cx, &dependent, policy())
                .is_err()
        );
        db.rebuild_standing_query(&cx, &low, policy()).unwrap();
        for handle in [&dependent, &limited] {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_reduction_delta(&cx, handle).unwrap().is_none());
        }
        let mut bad = WriteBatch::new(RelationId(1));
        bad.set_vertex_property(
            VId(2),
            V,
            Some(CanonicalScalar::ucs_basic_text("not numeric").unwrap()),
        );
        let bad_at = db.write(&commit, bad).await.unwrap();
        assert_eq!(db.frontier().unwrap(), bad_at);
        assert_eq!(db.standing_rows(&cx, &high).unwrap().rows().len(), 2);
        for handle in [&dependent, &limited, &healthy] {
            assert!(matches!(
                db.standing_reduction(&cx, handle),
                Err(StandingQueryError::Unavailable {
                    reason: StandingQueryFailure::NonIntegerSum,
                    ..
                })
            ));
            assert!(db.rebuild_standing_query(&cx, handle, policy()).is_err());
        }
        let mut repair = WriteBatch::new(RelationId(1));
        repair.set_vertex_property(VId(2), V, Some(CanonicalScalar::Int(-5)));
        db.write(&commit, repair).await.unwrap();
        for handle in [&dependent, &limited, &healthy] {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert_eq!(
                observed(db.standing_reduction(&cx, handle).unwrap().rows()),
                oracle(&source(&db, 1), &[0], 1)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_global_and_native_identity_groups_obey_schema_ownership_and_reopen_rules() {
    let ((), report) = run_async_under_lab(0x7261_0403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let values = input(&mut db, &cx, "L", policy());
        let global = db
            .register_standing_reduction(&cx, &values, &[], 1, bounded(1))
            .unwrap();
        let empty = db.standing_reduction(&cx, &global).unwrap();
        assert_eq!(empty.rows().len(), 1);
        let row = empty.rows().iter().next().unwrap().0;
        assert_eq!(row.count_rows(), &ZWeight::ZERO);
        assert!(row.sum().is_none());
        assert!(matches!(
            db.register_standing_reduction(&cx, &values, &[], 1, bounded(0)),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert!(matches!(
            db.register_standing_reduction(&cx, &values, &[0, 0], 1, policy()),
            Err(StandingQueryError::ReductionSchema(
                RowAggregateBuildError::DuplicateKey { .. }
            ))
        ));
        let ids = db
            .register_standing_rows(
                &cx,
                definition("MATCH (n:L) RETURN n AS id, n.v AS v"),
                policy(),
            )
            .unwrap();
        assert!(matches!(
            db.register_standing_reduction(&cx, &ids, &[], 0, policy()),
            Err(StandingQueryError::ReductionSchema(
                RowAggregateBuildError::RequiresScalarArgument
            ))
        ));
        let grouped = db
            .register_standing_reduction(&cx, &ids, &[0], 1, policy())
            .unwrap();
        assert!(
            db.standing_reduction(&cx, &grouped)
                .unwrap()
                .rows()
                .is_empty()
        );
        let mut seed = WriteBatch::new(RelationId(1));
        add(&mut seed, u128::MAX, 1, None, None);
        add(&mut seed, 0, 1, None, Some(7));
        db.write(&commit, seed).await.unwrap();
        let view = db.standing_reduction(&cx, &grouped).unwrap();
        let group_keys: Vec<_> = view
            .rows()
            .iter()
            .map(|(row, _)| row.keys().to_vec())
            .collect();
        assert_eq!(
            group_keys,
            vec![
                vec![GraphValue::Vertex(VId(0))],
                vec![GraphValue::Vertex(VId(u128::MAX))]
            ]
        );
        let expected = observed(view.rows());
        assert!(matches!(
            db.standing_rows(&cx, &grouped),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.register_standing_reduction(&cx, &global, &[], 0, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.register_standing_reduction(&cx, &values, &[], 1, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        db.compact(&commit).await.unwrap();
        assert_eq!(
            observed(db.standing_reduction(&cx, &grouped).unwrap().rows()),
            expected
        );
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_reduction(&cx, &grouped),
            Err(StandingQueryError::ForeignHandle)
        ));
        let ids = db
            .register_standing_rows(
                &cx,
                definition("MATCH (n:L) RETURN n AS id, n.v AS v"),
                policy(),
            )
            .unwrap();
        let reopened = db
            .register_standing_reduction(&cx, &ids, &[0], 1, policy())
            .unwrap();
        assert_eq!(
            observed(db.standing_reduction(&cx, &reopened).unwrap().rows()),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn reduction_tick_work_depends_on_changed_groups_not_unrelated_retained_input() {
    let ((), report) = run_async_under_lab(0x7261_0404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut measured = Vec::new();
        for size in [2_u128, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 0..size {
                add(&mut seed, id, 1, Some(id as i64), Some(id as i64));
            }
            db.write(&commit, seed).await.unwrap();
            let input = input(&mut db, &cx, "L", policy());
            let reduced = db
                .register_standing_reduction(&cx, &input, &[0], 1, policy())
                .unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(0), V, Some(CanonicalScalar::Int(-1)));
            db.write(&commit, update).await.unwrap();
            let view = db.standing_reduction(&cx, &reduced).unwrap();
            measured.push(*view.last_maintenance());
            assert_eq!(observed(view.rows()), oracle(&source(&db, 1), &[0], 1));
        }
        assert_eq!(measured[0], measured[1]);
        assert_eq!(measured[0].delta_rows, 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
