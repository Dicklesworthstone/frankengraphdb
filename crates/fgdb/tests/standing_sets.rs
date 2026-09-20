//! Set-view circuits are compared with independently recomputed result bags.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId, RelationId, ZSet, ZWeight};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphSetBuildError, GraphSetColumnType, GraphSymbol,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};
use std::collections::{BTreeMap, BTreeSet};

const LIMBS: LimbLimit = LimbLimit::new(4);
const P: PropertyKeyId = PropertyKeyId(1);
const OPERATIONS: [SetOperation; 6] = [
    SetOperation::UnionAll,
    SetOperation::UnionDistinct,
    SetOperation::IntersectAll,
    SetOperation::IntersectDistinct,
    SetOperation::ExceptAll,
    SetOperation::ExceptDistinct,
];
type Bag = BTreeMap<GraphValueRow, i128>;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn query(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (row.clone(), weight.to_i128().unwrap()))
        .collect()
}
fn eager(db: &Database<MemVfs>, cx: &QueryCx, q: &PreparedGraphPattern<GraphValueRow>) -> Bag {
    let mut bag = Bag::new();
    for row in db
        .execute_graph_pattern_governed(cx, q, policy())
        .unwrap()
        .value
    {
        *bag.entry(row).or_default() += 1;
    }
    bag
}
fn oracle(left: &Bag, right: &Bag, operation: SetOperation) -> Bag {
    left.keys()
        .chain(right.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|row| {
            let l = left.get(row).copied().unwrap_or(0);
            let r = right.get(row).copied().unwrap_or(0);
            let count = match operation {
                SetOperation::UnionAll => l + r,
                SetOperation::UnionDistinct => i128::from(l > 0 || r > 0),
                SetOperation::IntersectAll => l.min(r),
                SetOperation::IntersectDistinct => i128::from(l > 0 && r > 0),
                SetOperation::ExceptAll => (l - r).max(0),
                SetOperation::ExceptDistinct => i128::from(l > 0 && r == 0),
            };
            (count > 0).then(|| (row.clone(), count))
        })
        .collect()
}
fn check(db: &Database<MemVfs>, cx: &QueryCx, handle: &StandingQueryHandle, wanted: &Bag) {
    let view = db.standing_set(cx, handle).unwrap();
    assert_eq!(view.frontier(), db.frontier().unwrap());
    assert!(view.ordered_rows().is_none());
    assert_eq!(plain(view.rows()), *wanted);
    assert_eq!(
        db.standing_set_total(cx, handle).unwrap().to_i128(),
        Some(wanted.values().sum())
    );
}

#[test]
fn all_six_operations_and_nested_shared_inputs_follow_whole_commits() {
    let ((), report) = run_async_under_lab(0x7365_7401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, label, value) in [
            (1, 1, Some(4)),
            (2, 1, Some(4)),
            (3, 2, Some(4)),
            (4, 2, Some(7)),
            (5, 1, None),
            (6, 2, None),
        ] {
            seed.create_vertex(
                VId(id),
                vec![LabelId(label)],
                value
                    .map(|v| (P, CanonicalScalar::Int(v)))
                    .into_iter()
                    .collect(),
            );
        }
        db.write(&commit, seed).await.unwrap();
        let lq = query("MATCH (n:L) RETURN n.p AS left_name");
        let rq = query("MATCH (n:R) RETURN n.p AS right_name");
        let l = db
            .register_standing_rows(&cx, lq.clone(), policy())
            .unwrap();
        let r = db
            .register_standing_rows(&cx, rq.clone(), policy())
            .unwrap();
        let handles: Vec<_> = OPERATIONS
            .iter()
            .map(|&op| db.register_standing_set(&cx, &l, &r, op, policy()).unwrap())
            .collect();
        let nested = db
            .register_standing_set(
                &cx,
                &handles[0],
                &handles[2],
                SetOperation::ExceptAll,
                policy(),
            )
            .unwrap();
        let doubled = db
            .register_standing_set(&cx, &nested, &nested, SetOperation::UnionAll, policy())
            .unwrap();
        let self_except = db
            .register_standing_set(
                &cx,
                &nested,
                &nested,
                SetOperation::ExceptDistinct,
                policy(),
            )
            .unwrap();
        for handle in &handles {
            assert_eq!(
                db.standing_set_columns(&cx, handle).unwrap(),
                &["left_name"]
            );
            assert!(db.standing_set_delta(&cx, handle).unwrap().is_none());
        }
        let mut previous: Vec<_> = handles
            .iter()
            .map(|h| {
                db.standing_set(&cx, h)
                    .unwrap()
                    .rows()
                    .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap()
            })
            .collect();
        // Both sides change the same key in one commit, including NULL support.
        let mut first = WriteBatch::new(RelationId(1));
        first.delete_vertex(VId(1));
        first.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(4)));
        first.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(9)));
        let mut second = WriteBatch::new(RelationId(1));
        second.set_vertex_label(VId(2), LabelId(1), false);
        second.set_vertex_label(VId(2), LabelId(2), true);
        second.delete_vertex(VId(6));
        let mut empty = WriteBatch::new(RelationId(2));
        empty.set_vertex_property(VId(3), PropertyKeyId(99), Some(CanonicalScalar::Int(8)));
        for batch in [first, second, empty] {
            db.write(&commit, batch).await.unwrap();
            let (left, right) = (eager(&db, &cx, &lq), eager(&db, &cx, &rq));
            for ((handle, operation), old) in handles.iter().zip(OPERATIONS).zip(&mut previous) {
                check(&db, &cx, handle, &oracle(&left, &right, operation));
                let delta = db.standing_set_delta(&cx, handle).unwrap().unwrap();
                old.integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(*old, *db.standing_set(&cx, handle).unwrap().rows());
            }
            let wanted = oracle(
                &oracle(&left, &right, SetOperation::UnionAll),
                &oracle(&left, &right, SetOperation::IntersectAll),
                SetOperation::ExceptAll,
            );
            check(&db, &cx, &nested, &wanted);
            check(
                &db,
                &cx,
                &doubled,
                &oracle(&wanted, &wanted, SetOperation::UnionAll),
            );
            check(&db, &cx, &self_except, &Bag::new());
        }
        for handle in handles.iter().chain([&nested, &doubled, &self_except]) {
            let delta = db.standing_set_delta(&cx, handle).unwrap().unwrap();
            assert!(delta.rows().is_empty());
            assert_eq!(delta.last_maintenance().delta_rows, 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn final_occurrence_limits_allow_swaps_and_dependency_failures_require_ordered_repair() {
    let ((), report) = run_async_under_lab(0x7365_7402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(9))]);
        db.write(&commit, seed).await.unwrap();
        let one = GqlQueryPolicy::new(100, 1, 1_000_000, 1_000_000);
        let lq = query("MATCH (n:L) RETURN n.p AS p");
        let l = db.register_standing_rows(&cx, lq.clone(), one).unwrap();
        let r = db
            .register_standing_rows(&cx, query("MATCH (n:R) RETURN n.p AS p"), policy())
            .unwrap();
        let child = db
            .register_standing_set(&cx, &l, &r, SetOperation::UnionAll, one)
            .unwrap();
        let grandchild = db
            .register_standing_set(&cx, &child, &child, SetOperation::UnionDistinct, one)
            .unwrap();
        let sibling = db
            .register_standing_set(&cx, &r, &r, SetOperation::UnionAll, policy())
            .unwrap();
        let mut swap = WriteBatch::new(RelationId(1));
        swap.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
        let basis = db.write(&commit, swap).await.unwrap();
        assert_eq!(db.standing_set_total(&cx, &child).unwrap(), &ZWeight::ONE);
        assert_eq!(
            db.standing_set_delta(&cx, &child)
                .unwrap()
                .unwrap()
                .rows()
                .len(),
            2
        );
        let mut insert = WriteBatch::new(RelationId(1));
        insert.create_vertex(VId(2), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(2))]);
        let at = db.write(&commit, insert).await.unwrap();
        for h in [&child, &grandchild] {
            assert!(
                matches!(db.standing_set(&cx, h), Err(StandingQueryError::Unavailable {
                frontier, reason: StandingQueryFailure::DependencyUnavailable,
            }) if frontier == basis)
            );
            assert!(db.standing_set_delta(&cx, h).is_err());
        }
        assert_eq!(db.standing_set(&cx, &sibling).unwrap().frontier(), at);
        assert!(db.rebuild_standing_query(&cx, &child, policy()).is_err());
        db.rebuild_standing_query(&cx, &l, policy()).unwrap();
        assert!(matches!(
            db.rebuild_standing_query(&cx, &child, one),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::ResultBudget
            ))
        ));
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert!(db.standing_set_delta(&cx, &child).unwrap().is_none());
        assert!(db.standing_set(&cx, &grandchild).is_err());
        db.rebuild_standing_query(&cx, &grandchild, policy())
            .unwrap();
        let mut retire = WriteBatch::new(RelationId(1));
        retire.delete_vertex(VId(2));
        db.write(&commit, retire).await.unwrap();
        check(&db, &cx, &child, &eager(&db, &cx, &lq));
        check(&db, &cx, &grandchild, &eager(&db, &cx, &lq));
        // Now only the child is over quota; healthy inputs must stay available.
        let limited = db
            .register_standing_set(&cx, &l, &r, SetOperation::UnionAll, one)
            .unwrap();
        let mut insert = WriteBatch::new(RelationId(1));
        insert.create_vertex(VId(3), vec![LabelId(1)], vec![(P, CanonicalScalar::Int(1))]);
        db.write(&commit, insert).await.unwrap();
        assert!(db.standing_rows(&cx, &l).is_ok());
        assert!(matches!(
            db.standing_set(&cx, &limited),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert_eq!(
            db.standing_set_total(&cx, &child).unwrap().to_i128(),
            Some(2)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn operand_windows_wide_identities_schema_and_owner_checks_are_not_sampled_from_rows() {
    let ((), report) = run_async_under_lab(0x7365_7403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let vertex = db
            .register_standing_rows(&cx, query("MATCH (n) RETURN n AS id"), policy())
            .unwrap();
        let scalar = db
            .register_standing_rows(&cx, query("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_set(&cx, &vertex, &scalar, SetOperation::UnionAll, policy()),
            Err(StandingQueryError::SetSchema(
                GraphSetBuildError::ColumnType {
                    column: 0,
                    left: GraphSetColumnType::Vertex,
                    right: GraphSetColumnType::Scalar,
                }
            ))
        ));
        let pair = db
            .register_standing_rows(&cx, query("MATCH (n) RETURN n AS id, n.p AS p"), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_set(&cx, &pair, &vertex, SetOperation::UnionAll, policy()),
            Err(StandingQueryError::SetSchema(
                GraphSetBuildError::ColumnCount { left: 2, right: 1 }
            ))
        ));
        let topology = db
            .register_standing_components(&cx, RelationId(1), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_set(&cx, &topology, &vertex, SetOperation::UnionAll, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        let identities = db
            .register_standing_set(&cx, &vertex, &vertex, SetOperation::UnionDistinct, policy())
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(0, 4), (1, 4), (1_u128 << 100, 7), (u128::MAX, 9)] {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(value))]);
        }
        db.write(&commit, seed).await.unwrap();
        assert!(
            db.standing_set(&cx, &identities)
                .unwrap()
                .rows()
                .iter()
                .any(|(r, _)| r.values() == [GraphValue::Vertex(VId(u128::MAX))])
        );
        let lq = query("MATCH (n) RETURN n.p AS p ORDER BY p SKIP 1 LIMIT 2");
        let rq = query("MATCH (n) RETURN DISTINCT n.p AS p ORDER BY p SKIP 1 LIMIT 1");
        let left = db
            .register_standing_rows(&cx, lq.clone(), policy())
            .unwrap();
        let right = db
            .register_standing_rows(&cx, rq.clone(), policy())
            .unwrap();
        let window = db
            .register_standing_set(&cx, &left, &right, SetOperation::ExceptAll, policy())
            .unwrap();
        check(
            &db,
            &cx,
            &window,
            &oracle(
                &eager(&db, &cx, &lq),
                &eager(&db, &cx, &rq),
                SetOperation::ExceptAll,
            ),
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(0));
        db.write(&commit, change).await.unwrap();
        check(
            &db,
            &cx,
            &window,
            &oracle(
                &eager(&db, &cx, &lq),
                &eager(&db, &cx, &rq),
                SetOperation::ExceptAll,
            ),
        );
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        let local = foreign
            .register_standing_rows(&cx, query("MATCH (n) RETURN n AS id"), policy())
            .unwrap();
        assert!(matches!(
            foreign.register_standing_set(&cx, &local, &vertex, SetOperation::UnionAll, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            db.standing_rows(&cx, &window),
            Err(StandingQueryError::Unsupported)
        ));
        assert!(matches!(
            db.standing_set(&cx, &left),
            Err(StandingQueryError::Unsupported)
        ));
        db.compact(&commit).await.unwrap();
        drop(db);
        let mut reopened = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            reopened.standing_set(&cx, &window),
            Err(StandingQueryError::ForeignHandle)
        ));
        let l = reopened
            .register_standing_rows(&cx, lq.clone(), policy())
            .unwrap();
        let r = reopened
            .register_standing_rows(&cx, rq.clone(), policy())
            .unwrap();
        let fresh = reopened
            .register_standing_set(&cx, &l, &r, SetOperation::ExceptAll, policy())
            .unwrap();
        check(
            &reopened,
            &cx,
            &fresh,
            &oracle(
                &eager(&reopened, &cx, &lq),
                &eager(&reopened, &cx, &rq),
                SetOperation::ExceptAll,
            ),
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_operand_changes_do_not_rescan_large_retained_set_inputs() {
    let ((), report) = run_async_under_lab(0x7365_7404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut measurements = Vec::new();
        for size in [4, 128] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 0..size {
                seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
            }
            db.write(&commit, seed).await.unwrap();
            let parent = db
                .register_standing_rows(&cx, query("MATCH (n) RETURN n.p AS p"), policy())
                .unwrap();
            let h = db
                .register_standing_set(&cx, &parent, &parent, SetOperation::UnionAll, policy())
                .unwrap();
            let mut edit = WriteBatch::new(RelationId(1));
            edit.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(1000)));
            db.write(&commit, edit).await.unwrap();
            let changed = *db.standing_set(&cx, &h).unwrap().last_maintenance();
            assert_eq!(changed.delta_rows, 4);
            let mut irrelevant = WriteBatch::new(RelationId(1));
            irrelevant.set_vertex_property(
                VId(0),
                PropertyKeyId(99),
                Some(CanonicalScalar::Int(1)),
            );
            db.write(&commit, irrelevant).await.unwrap();
            let empty = *db.standing_set(&cx, &h).unwrap().last_maintenance();
            assert_eq!(empty.delta_rows, 0);
            assert!(
                db.standing_set_delta(&cx, &h)
                    .unwrap()
                    .unwrap()
                    .rows()
                    .is_empty()
            );
            measurements.push((
                changed.work_units,
                changed.scratch_entries,
                empty.work_units,
                empty.scratch_entries,
            ));
        }
        assert_eq!(
            measurements[0], measurements[1],
            "set work depends on changed keys, not retained support"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
