//! Native and maintained set schemas agree before data and across later ticks.
//! The independent fixture below computes bag multiplicities, not query plans.
use super::*;
use crate::{DatabaseKeys, MemVfs, PreparedNativeRead, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder};
use fgdb_gql::{
    GqlParameters, GraphAggregate, GraphSetOperation, GraphSetProjection, GraphSetQuantifier,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphSet, PreparedGraphText,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::collections::BTreeMap;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn operations() -> [(SetOperation, GraphSetOperation, GraphSetQuantifier); 6] {
    use GraphSetOperation::{Except, Intersect, Union};
    use GraphSetQuantifier::{All, Distinct};
    [
        (SetOperation::UnionAll, Union, All),
        (SetOperation::UnionDistinct, Union, Distinct),
        (SetOperation::IntersectAll, Intersect, All),
        (SetOperation::IntersectDistinct, Intersect, Distinct),
        (SetOperation::ExceptAll, Except, All),
        (SetOperation::ExceptDistinct, Except, Distinct),
    ]
}
fn declared(types: &[GraphSetColumnType], empty: bool) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .project(
            types
                .iter()
                .enumerate()
                .map(|(at, kind)| {
                    GraphSetProjection::new(format!("c{at}"), GraphSetValue::TypedNull(*kind))
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_page(0, empty.then_some(0))
}

#[test]
fn all_declared_domains_match_native_admission_for_empty_and_null_operands() {
    use GraphSetColumnType::{Any, Edge, Edges, List, Path, Scalar, Vertex, Vertices};
    let ((), report) = run_async_under_lab(0xd0a1_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let kinds = [Vertex, Scalar, Path, Vertices, Edges, Edge, List, Any];
        for empty in [false, true] {
            for left_kind in kinds {
                for right_kind in kinds {
                    let left = declared(&[left_kind], empty);
                    let right = declared(&[right_kind], empty);
                    let l = db.register_standing_relation(&cx, &left, policy()).unwrap();
                    let r = db
                        .register_standing_relation(&cx, &right, policy())
                        .unwrap();
                    // Explicit expectation: all NULL values must NOT license
                    // different concrete schemas, nor narrow an open schema.
                    let expected = if left_kind == Any || right_kind == Any {
                        Some(Any)
                    } else if left_kind == right_kind {
                        Some(left_kind)
                    } else {
                        None
                    };
                    assert_eq!(left_kind.common_set_domain(right_kind), expected);
                    for (operation, native, quantifier) in operations() {
                        let definition = left.clone().combine(native, quantifier, right.clone());
                        let before = db.standing_queries.len();
                        let result = db.register_standing_set(&cx, &l, &r, operation, policy());
                        match expected {
                            Some(kind) => {
                                assert_eq!(definition.unwrap().column_types(), &[kind]);
                                let handle = result.unwrap();
                                assert_eq!(
                                    column_type(&db.standing_queries[handle.index], 0),
                                    Some(kind)
                                );
                                assert_eq!(db.standing_set_columns(&cx, &handle).unwrap(), &["c0"]);
                                let count = if empty {
                                    0
                                } else {
                                    match operation {
                                        SetOperation::UnionAll => 2,
                                        SetOperation::ExceptAll | SetOperation::ExceptDistinct => 0,
                                        _ => 1,
                                    }
                                };
                                assert_eq!(
                                    db.standing_set_total(&cx, &handle).unwrap().to_i128(),
                                    Some(count)
                                );
                                assert!(db.standing_set_delta(&cx, &handle).unwrap().is_none());
                            }
                            None => {
                                let error = GraphSetBuildError::ColumnType {
                                    column: 0,
                                    left: left_kind,
                                    right: right_kind,
                                };
                                assert_eq!(definition.unwrap_err(), error);
                                match result {
                                    Err(StandingQueryError::SetSchema(observed)) => {
                                        assert_eq!(observed, error);
                                    }
                                    _ => panic!("expected the incompatible declared-domain error"),
                                }
                                assert_eq!(db.standing_queries.len(), before);
                            }
                        }
                    }
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_open_prefix_does_not_bypass_later_schema_or_resource_refusals() {
    use GraphSetColumnType::{Any, Scalar, Vertex};
    let ((), report) = run_async_under_lab(0xd0a1_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        for empty in [false, true] {
            let left = db
                .register_standing_relation(&cx, &declared(&[Any, Vertex], empty), policy())
                .unwrap();
            let right = db
                .register_standing_relation(&cx, &declared(&[Scalar, Scalar], empty), policy())
                .unwrap();
            for (operation, _, _) in operations() {
                let before = db.standing_queries.len();
                assert!(matches!(
                    db.register_standing_set(&cx, &left, &right, operation, policy()),
                    Err(StandingQueryError::SetSchema(
                        GraphSetBuildError::ColumnType {
                            column: 1,
                            left: Vertex,
                            right: Scalar,
                        }
                    ))
                ));
                assert_eq!(db.standing_queries.len(), before);
            }
            let narrow = db
                .register_standing_relation(&cx, &declared(&[Any], empty), policy())
                .unwrap();
            assert!(matches!(
                db.register_standing_set(&cx, &narrow, &right, SetOperation::UnionAll, policy()),
                Err(StandingQueryError::SetSchema(
                    GraphSetBuildError::ColumnCount { left: 1, right: 2 }
                ))
            ));
        }
        let a = db
            .register_standing_relation(&cx, &declared(&[Any], false), policy())
            .unwrap();
        let b = db
            .register_standing_relation(&cx, &declared(&[Scalar], false), policy())
            .unwrap();
        for (allowance, reason) in [
            (
                GqlQueryPolicy::new(1, 2, 1_000_000, 1_000_000),
                StandingQueryFailure::SnapshotBudget,
            ),
            (
                GqlQueryPolicy::new(2, 1, 1_000_000, 1_000_000),
                StandingQueryFailure::ResultBudget,
            ),
            (
                GqlQueryPolicy::new(2, 2, 0, 1_000_000),
                StandingQueryFailure::WorkBudget,
            ),
            (
                GqlQueryPolicy::new(2, 2, 1_000_000, 0),
                StandingQueryFailure::ScratchBudget,
            ),
        ] {
            let before = db.standing_queries.len();
            assert!(matches!(
                db.register_standing_set(&cx, &a, &b, SetOperation::UnionAll, allowance),
                Err(StandingQueryError::Maintenance(observed)) if observed == reason
            ));
            assert_eq!(db.standing_queries.len(), before);
        }
        let handle = db
            .register_standing_set(&cx, &a, &b, SetOperation::UnionAll, policy())
            .unwrap();
        assert_eq!(
            db.standing_set_total(&cx, &handle).unwrap().to_i128(),
            Some(2)
        );
        let mut other = Database::open_memory(&commit, keys()).await.unwrap();
        let foreign = other
            .register_standing_relation(&cx, &declared(&[Any], false), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_set(&cx, &a, &foreign, SetOperation::UnionAll, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        _ => None,
    }
}
fn leaf(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, resolve)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}
fn payload() -> GraphValue {
    GraphValue::map(vec![(
        "items".into(),
        GraphValue::List(
            vec![
                GraphValue::Scalar(CanonicalScalar::Int(2)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]
            .into_boxed_slice(),
        ),
    )])
    .unwrap()
}
fn dynamic() -> PreparedGraphSet {
    leaf("MATCH (n:L) RETURN n.p AS p,n AS id")
        .unwind(
            "item".into(),
            GraphSetValue::List(vec![
                GraphSetValue::Column(0),
                GraphSetValue::Column(0),
                GraphSetValue::Column(1),
                GraphSetValue::Value(payload()),
                GraphSetValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
            ]),
        )
        .unwrap()
        .project(
            vec![GraphSetProjection::new("value", GraphSetValue::Column(2))],
            GraphSetQuantifier::All,
        )
        .unwrap()
}
fn bag(values: impl IntoIterator<Item = GraphValueRow>) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        values.into_iter().map(|row| (row, ZWeight::ONE)),
        LIMBS,
        &mut |_| Ok::<_, ()>(()),
    )
    .unwrap()
}
fn snapshot(db: &Database<MemVfs>, cx: &QueryCx, query: &PreparedGraphSet) -> Vec<GraphValueRow> {
    query
        .execute_governed(
            policy(),
            |pattern, allowance| {
                db.execute_graph_pattern_governed_at(cx, pattern, db.frontier().unwrap(), allowance)
            },
            || cx.checkpoint(),
        )
        .unwrap()
        .value
}
fn expected(
    state: &[(u128, CanonicalScalar)],
    vertex: bool,
    reverse: bool,
    operation: SetOperation,
) -> ZSet<GraphValueRow> {
    let mut a = BTreeMap::<GraphValue, i128>::new();
    let mut b = BTreeMap::<GraphValue, i128>::new();
    for (id, value) in state {
        let scalar = GraphValue::Scalar(value.clone());
        for value in [
            scalar.clone(),
            scalar.clone(),
            GraphValue::Vertex(VId(*id)),
            payload(),
            GraphValue::Scalar(CanonicalScalar::Null),
        ] {
            *a.entry(value).or_default() += 1;
        }
        let value = if vertex {
            GraphValue::Vertex(VId(*id))
        } else {
            scalar
        };
        *b.entry(value).or_default() += 1;
    }
    if reverse {
        core::mem::swap(&mut a, &mut b);
    }
    let support = a
        .keys()
        .chain(b.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let mut updates = Vec::new();
    for value in support {
        let l = a.get(&value).copied().unwrap_or(0);
        let r = b.get(&value).copied().unwrap_or(0);
        let count = match operation {
            SetOperation::UnionAll => l + r,
            SetOperation::UnionDistinct => i128::from(l != 0 || r != 0),
            SetOperation::IntersectAll => l.min(r),
            SetOperation::IntersectDistinct => i128::from(l != 0 && r != 0),
            SetOperation::ExceptAll => (l - r).max(0),
            SetOperation::ExceptDistinct => i128::from(l != 0 && r == 0),
        };
        updates.push((
            GraphValueRow::from_owned_values(vec![value]),
            ZWeight::from_i128(count),
        ));
    }
    ZSet::from_updates(updates, LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}

#[test]
fn mixed_value_bags_keep_signed_multiplicities_and_feed_grouped_ranked_circuits() {
    let ((), report) = run_async_under_lab(0xd0a1_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let open = dynamic();
        assert_eq!(open.column_types(), &[GraphSetColumnType::Any]);
        let a = db.register_standing_relation(&cx, &open, policy()).unwrap();
        let mut cases = Vec::new();
        let mut grouped = Vec::new();
        for vertex in [false, true] {
            let concrete = leaf(if vertex {
                "MATCH (n:L) RETURN n AS value"
            } else {
                "MATCH (n:L) RETURN n.p AS value"
            });
            let b = db
                .register_standing_relation(&cx, &concrete, policy())
                .unwrap();
            for reverse in [false, true] {
                for (operation, native, quantifier) in operations() {
                    let (l, r, left, right) = if reverse {
                        (&b, &a, &concrete, &open)
                    } else {
                        (&a, &b, &open, &concrete)
                    };
                    let query = left
                        .clone()
                        .combine(native, quantifier, right.clone())
                        .unwrap();
                    let h = db
                        .register_standing_set(&cx, l, r, operation, policy())
                        .unwrap();
                    let group = query
                        .clone()
                        .group_by(&[0], &[GraphAggregate::count_rows("copies")])
                        .unwrap()
                        .with_order_by(&[GraphValueOrder::descending(1)])
                        .unwrap()
                        .with_page(1, Some(3));
                    let g = db
                        .register_standing_relation(&cx, &group, policy())
                        .unwrap();
                    grouped.push((group, g));
                    cases.push((vertex, reverse, operation, query, h));
                }
            }
        }
        let states = [
            vec![],
            vec![
                (1, CanonicalScalar::Int(2)),
                (2, CanonicalScalar::Int(2)),
                (u128::MAX, CanonicalScalar::Null),
            ],
            vec![
                (1, CanonicalScalar::Float(CanonicalF64::new(2.0))),
                (2, CanonicalScalar::Int(2)),
                (u128::MAX, CanonicalScalar::Null),
            ],
            vec![
                (1, CanonicalScalar::Float(CanonicalF64::new(2.0))),
                (u128::MAX, CanonicalScalar::Null),
            ],
            vec![
                (1, CanonicalScalar::Bool(true)),
                (3, CanonicalScalar::Int(-1)),
                (u128::MAX, CanonicalScalar::Int(2)),
            ],
        ];
        for (step, state) in states.iter().enumerate() {
            if step != 0 {
                let previous = cases
                    .iter()
                    .map(|(_, _, _, _, h)| {
                        db.standing_set(&cx, h)
                            .unwrap()
                            .rows()
                            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
                            .unwrap()
                    })
                    .collect::<Vec<_>>();
                let mut change = WriteBatch::new(RelationId(1));
                for (id, _) in &states[step - 1] {
                    if !state.iter().any(|(next, _)| next == id) {
                        change.delete_vertex(VId(*id));
                    }
                }
                for (id, value) in state {
                    match states[step - 1].iter().find(|(old, _)| old == id) {
                        None => {
                            change.create_vertex(
                                VId(*id),
                                vec![LabelId(1)],
                                vec![(PropertyKeyId(1), value.clone())],
                            );
                        }
                        Some((_, old)) if old != value => {
                            change.set_vertex_property(
                                VId(*id),
                                PropertyKeyId(1),
                                Some(value.clone()),
                            );
                        }
                        _ => {}
                    }
                }
                let at = db.write(&commit, change).await.unwrap();
                for ((_, _, _, _, h), mut integrated) in cases.iter().zip(previous) {
                    let delta = db.standing_set_delta(&cx, h).unwrap().unwrap();
                    assert_eq!(delta.frontier(), at);
                    integrated
                        .integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                        .unwrap();
                    assert_eq!(&integrated, db.standing_set(&cx, h).unwrap().rows());
                }
            }
            for (vertex, reverse, operation, query, h) in &cases {
                let oracle = expected(state, *vertex, *reverse, *operation);
                assert_eq!(db.standing_set(&cx, h).unwrap().rows(), &oracle);
                assert_eq!(bag(snapshot(&db, &cx, query)), oracle);
                assert_eq!(
                    column_type(&db.standing_queries[h.index], 0),
                    Some(GraphSetColumnType::Any)
                );
            }
            for (query, h) in &grouped {
                let expected = QueryResult::Rows {
                    columns: query.columns().to_vec(),
                    rows: snapshot(&db, &cx, query)
                        .into_iter()
                        .map(|row| {
                            row.values()
                                .iter()
                                .cloned()
                                .map(QueryValue::Value)
                                .collect()
                        })
                        .collect(),
                };
                let delivered = db
                    .standing_native_query(&cx, h, GqlQueryPolicy::new(0, 3, 1_000_000, 1_000_000))
                    .unwrap()
                    .1;
                assert_eq!(delivered, expected);
            }
        }
        for (query, h) in &grouped {
            let before = db.standing_native_query(&cx, h, policy()).unwrap();
            db.rebuild_standing_query(&cx, h, policy()).unwrap();
            assert_eq!(db.standing_native_query(&cx, h, policy()).unwrap(), before);
            assert_eq!(
                query.column_types(),
                &[GraphSetColumnType::Any, GraphSetColumnType::Scalar]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_unwind_union_bindings_keep_independent_ranked_results_after_rebuild() {
    let ((), report) = run_async_under_lab(0xd0a1_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let text = "MATCH (n:L) UNWIND [n.p,$extra] AS x RETURN x \
            UNION ALL MATCH (n:L) RETURN n.p AS x ORDER BY x LIMIT 3";
        let low = GqlParameters::new().with_int64("extra", -100).unwrap();
        let high = GqlParameters::new().with_int64("extra", 100).unwrap();
        let prepared = PreparedNativeRead::prepare(text, &low, resolve).unwrap();
        let (nodes, width) = prepared.subscription_footprint(&cx, &low).unwrap();
        assert!(nodes > 1 && width > 0);
        let a = prepared
            .register_standing(&mut db, &cx, &low, policy())
            .unwrap();
        let b = prepared
            .register_standing(&mut db, &cx, &high, policy())
            .unwrap();
        drop(prepared);
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=3 {
            seed.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
        }
        db.write(&commit, seed).await.unwrap();
        for step in 0..2 {
            for (args, handle) in [(&low, &a), (&high, &b)] {
                let expected = db.query(&cx, text, args, resolve, policy()).unwrap();
                let actual = db.standing_native_query(&cx, handle, policy()).unwrap().1;
                assert_eq!(actual, expected);
                let QueryResult::Rows { rows, .. } = actual else {
                    panic!("row result");
                };
                assert_eq!(
                    db.standing_native_cursor(&cx, handle, policy())
                        .unwrap()
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap(),
                    rows
                );
                db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap().1,
                    expected
                );
            }
            assert_ne!(
                db.standing_native_query(&cx, &a, policy()).unwrap().1,
                db.standing_native_query(&cx, &b, policy()).unwrap().1
            );
            if step == 0 {
                let mut change = WriteBatch::new(RelationId(1));
                change.delete_vertex(VId(2));
                change.set_vertex_property(
                    VId(3),
                    PropertyKeyId(1),
                    Some(CanonicalScalar::Int(-200)),
                );
                db.write(&commit, change).await.unwrap();
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
