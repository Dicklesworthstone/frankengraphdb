//! Source-free subtrees are frozen as complete relations, never graph snapshots.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder, GraphValue, GraphValueOrder};
use fgdb_gql::{GraphAggregate, GraphSetPredicateOp, GraphSetProjection, GraphSetValue};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

const P: PropertyKeyId = PropertyKeyId(1);
const LIMBS: LimbLimit = LimbLimit::new(4);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xca; 32],
        DatabaseSecurityNamespaceId([0xcb; 32]),
        [0xcc; 32],
    )
}
fn scalar(n: Option<i64>) -> GraphValue {
    GraphValue::Scalar(n.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}
fn copy<T: Ord + Clone>(rows: &ZSet<T>) -> ZSet<T> {
    rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap()
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, p) in [(1, Some(1)), (2, Some(1)), (3, Some(2)), (u128::MAX, None)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(P, p.map_or(CanonicalScalar::Null, CanonicalScalar::Int))],
        );
    }
    batch
}
fn leaf() -> PreparedGraphSet {
    let mut b = GraphPatternBuilder::new();
    b.vertex("n").unwrap();
    b.prepare_values(&[GraphColumn::property("p", "n", P)], 0, None)
        .unwrap()
        .with_duplicates()
        .into()
}
fn literal(n: Option<i64>) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .project(
            vec![GraphSetProjection::new(
                "p",
                GraphSetValue::Value(scalar(n)),
            )],
            GraphSetQuantifier::All,
        )
        .unwrap()
}
fn constants() -> PreparedGraphSet {
    literal(Some(1))
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            literal(Some(1)),
        )
        .unwrap()
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            literal(Some(3)),
        )
        .unwrap()
        .combine(
            GraphSetOperation::Union,
            GraphSetQuantifier::All,
            literal(None),
        )
        .unwrap()
}
fn list_page() -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "x".into(),
            GraphSetValue::List(
                [Some(9), Some(1), Some(3), None]
                    .into_iter()
                    .map(|n| GraphSetValue::Value(scalar(n)))
                    .collect(),
            ),
        )
        .unwrap()
        .with_page(1, Some(2))
}
fn product() -> PreparedGraphSet {
    leaf()
        .cross_join(list_page())
        .unwrap()
        .with_order_by(&[GraphValueOrder::ascending(0), GraphValueOrder::ascending(1)])
        .unwrap()
        .with_page(0, Some(100))
}
fn ordinary<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
) -> QueryResult {
    let rows = query
        .execute_governed(
            policy(),
            |pattern, allowance| {
                db.execute_graph_pattern_governed_at(cx, pattern, db.frontier().unwrap(), allowance)
            },
            || cx.checkpoint(),
        )
        .unwrap()
        .value;
    QueryResult::Rows {
        columns: query.columns().to_vec(),
        rows: rows
            .into_iter()
            .map(|row| {
                row.values()
                    .iter()
                    .cloned()
                    .map(QueryValue::Value)
                    .collect()
            })
            .collect(),
    }
}
fn range(handle: &StandingQueryHandle) -> (usize, usize) {
    let Some(Layout::Circuit { first, .. } | Layout::GroupCircuit { first, .. }) =
        handle.native.as_deref()
    else {
        panic!("owned circuit");
    };
    (*first, handle.index)
}
fn independent_set<V: Vfs + Clone>(
    db: &Database<V>,
    op: GraphSetOperation,
    q: GraphSetQuantifier,
) -> QueryResult {
    let mut live = BTreeMap::<GraphValue, usize>::new();
    for vertex in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let value = vertex
            .props
            .iter()
            .find(|(key, _)| *key == P)
            .map(|(_, v)| v.clone())
            .unwrap_or(CanonicalScalar::Null);
        *live.entry(GraphValue::Scalar(value)).or_default() += 1;
    }
    let fixed = BTreeMap::from([
        (scalar(Some(1)), 2),
        (scalar(Some(3)), 1),
        (scalar(None), 1),
    ]);
    for key in fixed.keys() {
        live.entry(key.clone()).or_default();
    }
    let mut rows = Vec::new();
    for (key, a) in live {
        let b = fixed.get(&key).copied().unwrap_or(0);
        let n = match (op, q) {
            (GraphSetOperation::Union, GraphSetQuantifier::All) => a + b,
            (GraphSetOperation::Intersect, GraphSetQuantifier::All) => a.min(b),
            (GraphSetOperation::Except, GraphSetQuantifier::All) => a.saturating_sub(b),
            (GraphSetOperation::Union, GraphSetQuantifier::Distinct) => {
                usize::from(a != 0 || b != 0)
            }
            (GraphSetOperation::Intersect, GraphSetQuantifier::Distinct) => {
                usize::from(a != 0 && b != 0)
            }
            (GraphSetOperation::Except, GraphSetQuantifier::Distinct) => {
                usize::from(a != 0 && b == 0)
            }
        };
        for _ in 0..n {
            rows.push(vec![QueryValue::Value(key.clone())]);
        }
    }
    QueryResult::Rows {
        columns: vec!["p".into()],
        rows,
    }
}

#[test]
fn source_free_native_rows_preserve_sequence_parameters_and_zero_graph_visits() {
    let ((), report) = run_async_under_lab(0x636f_7001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let p = GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000);
        let text = "UNWIND $xs AS x RETURN x SKIP $off LIMIT $take";
        let args = GqlParameters::new()
            .with_list(
                "xs",
                vec![
                    scalar(Some(9)),
                    scalar(Some(1)),
                    GraphValue::List(vec![scalar(None), scalar(Some(3))].into_boxed_slice()),
                    scalar(Some(1)),
                ],
            )
            .unwrap()
            .with_uint64("off", 1)
            .unwrap()
            .with_uint64("take", 3)
            .unwrap();
        let template = PreparedNativeRead::prepare(text, &args, |_, _: &str| None).unwrap();
        let before = db.standing_queries.len();
        let h = template.register_standing(&mut db, &cx, &args, p).unwrap();
        assert_eq!(db.standing_queries.len(), before + 1);
        assert!(matches!(
            &db.standing_queries[h.index],
            StandingQuery::Constant(_)
        ));
        let expected = db.query(&cx, text, &args, |_, _: &str| None, p).unwrap();
        assert_eq!(db.standing_native_query(&cx, &h, p).unwrap().1, expected);
        let QueryResult::Rows { rows, .. } = &expected else {
            panic!("rows");
        };
        assert_eq!(
            rows,
            &vec![
                vec![QueryValue::Value(scalar(Some(1)))],
                vec![QueryValue::Value(GraphValue::List(
                    vec![scalar(None), scalar(Some(3))].into_boxed_slice()
                ))],
                vec![QueryValue::Value(scalar(Some(1)))]
            ]
        );
        let other = GqlParameters::new()
            .with_list("xs", vec![scalar(Some(7))])
            .unwrap()
            .with_uint64("off", 0)
            .unwrap()
            .with_uint64("take", 1)
            .unwrap();
        let separate = template.register_standing(&mut db, &cx, &other, p).unwrap();
        drop(template);
        drop(args);
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(1));
        let at = db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &h, p).unwrap(),
            (at, expected.clone())
        );
        assert_ne!(
            db.standing_native_query(&cx, &separate, p).unwrap().1,
            expected
        );
        db.rebuild_standing_query(&cx, &h, p).unwrap();
        assert_eq!(db.standing_native_query(&cx, &h, p).unwrap().1, expected);
        assert!(sets::delta(&db.standing_queries[h.index]).is_none());
        let empty = db
            .register_standing_relation(&cx, &PreparedGraphSet::singleton(), p)
            .unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &empty, p).unwrap().1,
            QueryResult::Rows {
                columns: vec![],
                rows: vec![vec![]]
            }
        );
        let sorted = list_page()
            .nested()
            .unwrap()
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap();
        let ordered = db.register_standing_relation(&cx, &sorted, p).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &ordered, p).unwrap().1,
            ordinary(&db, &cx, &sorted)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn six_set_laws_combine_live_and_constant_bags_through_writes_and_rebuilds() {
    let ((), report) = run_async_under_lab(0x636f_7002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let mut cases = Vec::new();
        for op in [
            GraphSetOperation::Union,
            GraphSetOperation::Intersect,
            GraphSetOperation::Except,
        ] {
            for q in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let query = leaf().combine(op, q, constants()).unwrap();
                let h = db
                    .register_standing_relation(&cx, &query, policy())
                    .unwrap();
                let (first, last) = range(&h);
                assert_eq!(
                    db.standing_queries[first..=last]
                        .iter()
                        .filter(|s| matches!(s, StandingQuery::Constant(_)))
                        .count(),
                    1
                );
                cases.push((op, q, query, h));
            }
        }
        for tick in 0..4 {
            let before: Vec<_> = cases
                .iter()
                .map(|(_, _, _, h)| copy(sets::rows(&db.standing_queries[h.index]).unwrap()))
                .collect();
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.delete_vertex(VId(1));
                }
                1 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(3)));
                }
                2 => {
                    change.set_vertex_property(VId(3), P, None);
                }
                _ => {
                    change.set_vertex_property(
                        VId(u128::MAX),
                        PropertyKeyId(99),
                        Some(CanonicalScalar::Bool(true)),
                    );
                }
            }
            let at = db.write(&commit, change).await.unwrap();
            for ((op, q, query, h), mut prior) in cases.iter().zip(before) {
                let delta = sets::delta(&db.standing_queries[h.index]).unwrap();
                if tick == 3 {
                    assert!(delta.is_empty());
                }
                prior
                    .integrate(delta, LIMBS, &mut |_| Ok::<_, ()>(()))
                    .unwrap();
                assert_eq!(&prior, sets::rows(&db.standing_queries[h.index]).unwrap());
                let actual = db.standing_native_query(&cx, h, policy()).unwrap();
                assert_eq!(actual, (at, independent_set(&db, *op, *q)));
                assert_eq!(actual.1, ordinary(&db, &cx, query));
            }
        }
        for (op, q, _, h) in cases {
            db.rebuild_standing_query(&cx, &h, policy()).unwrap();
            assert_eq!(
                db.standing_native_query(&cx, &h, policy()).unwrap().1,
                independent_set(&db, op, q)
            );
            assert!(sets::delta(&db.standing_queries[h.index]).is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_constant_products_feed_exact_groups_without_spending_final_group_quota() {
    let ((), report) = run_async_under_lab(0x636f_7003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let relation = product();
        let rows = db
            .register_standing_relation(&cx, &relation, policy())
            .unwrap();
        let summary = PreparedGraphSetAggregate::prepare(
            relation.clone(),
            &[],
            &[
                GraphAggregate::count_rows("n"),
                GraphAggregate::sum_int("sum", 1),
                GraphAggregate::count_distinct("distinct", 1),
            ],
            0,
            None,
        )
        .unwrap();
        let p = GqlQueryPolicy::new(100_000, 1, 20_000_000, 20_000_000);
        let h = db
            .register_standing_relation_aggregate(&cx, &summary, p)
            .unwrap();
        for tick in 0..3 {
            let count = db.vertices_at(db.frontier().unwrap()).unwrap().len() as u64;
            let expected = QueryResult::Rows {
                columns: vec!["n".into(), "sum".into(), "distinct".into()],
                rows: vec![vec![
                    QueryValue::Count(2 * count),
                    QueryValue::Integer(i128::from(4 * count)),
                    QueryValue::Count(2),
                ]],
            };
            assert_eq!(db.standing_native_query(&cx, &h, p).unwrap().1, expected);
            assert_eq!(
                db.standing_native_query(&cx, &rows, policy()).unwrap().1,
                ordinary(&db, &cx, &relation)
            );
            let mut prior = copy(db.standing_query(&cx, &h).unwrap().rows());
            let mut change = WriteBatch::new(RelationId(1));
            change.delete_vertex(VId(tick + 1));
            db.write(&commit, change).await.unwrap();
            prior
                .integrate(
                    db.standing_group_delta(&cx, &h).unwrap().unwrap().rows(),
                    LIMBS,
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(&prior, db.standing_query(&cx, &h).unwrap().rows());
        }
        db.rebuild_standing_query(&cx, &h, p).unwrap();
        assert!(db.standing_group_delta(&cx, &h).unwrap().is_none());
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(u128::MAX));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &h, p).unwrap().1,
            QueryResult::Rows {
                columns: vec!["n".into(), "sum".into(), "distinct".into()],
                rows: vec![vec![
                    QueryValue::Count(0),
                    QueryValue::Value(scalar(None)),
                    QueryValue::Count(0)
                ]],
            }
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn static_errors_are_not_hidden_by_empty_live_peers_or_final_zero_pages() {
    let ((), report) = run_async_under_lab(0x636f_7004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let good = db
            .register_standing_relation(&cx, &literal(Some(5)), policy())
            .unwrap();
        let len = db.standing_queries.len();
        let bad = PreparedGraphSet::singleton()
            .unwind("bad".into(), GraphSetValue::Value(scalar(Some(1))))
            .unwrap();
        for page in [None, Some(0)] {
            let query = leaf()
                .cross_join(bad.clone())
                .unwrap()
                .with_order_by(&[GraphValueOrder::ascending(0)])
                .unwrap()
                .with_page(0, page.or(Some(2)));
            assert!(matches!(
                db.register_standing_relation(&cx, &query, policy()),
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::InputExpression { .. }
                ))
            ));
            assert_eq!(db.standing_queries.len(), len);
            assert!(db.standing_native_query(&cx, &good, policy()).is_ok());
        }
        let mixed = leaf()
            .cross_join(list_page())
            .unwrap()
            .with_page(0, Some(1));
        // A frozen operand does NOT prove the ordering of a dynamic product.
        assert!(matches!(
            db.register_standing_relation(&cx, &mixed, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        assert_eq!(db.standing_queries.len(), len);
        let empty = constants()
            .filter(&[GraphSetPredicateOp::Truth(Some(false))])
            .unwrap();
        let h = db
            .register_standing_relation(
                &cx,
                &empty,
                GqlQueryPolicy::new(0, 0, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert!(db.standing_rows(&cx, &h).unwrap().rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_circuit_registration_and_rebuild_checkpoint_preserves_existing_state() {
    let ((), report) = run_async_under_lab(0x636f_7005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let query = product();
        let mut calls = 0;
        let h = register_checked(&mut db, &cx, &query, policy(), &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let before = db.standing_native_query(&cx, &h, policy()).unwrap();
        let len = db.standing_queries.len();
        for stop in 1..=calls {
            let mut seen = 0;
            let result = register_checked(&mut db, &cx, &query, policy(), &mut || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryError::Maintenance(
                        StandingQueryFailure::Interrupted,
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(
                result,
                Err(StandingQueryError::Maintenance(
                    StandingQueryFailure::Interrupted
                ))
            ));
            assert_eq!(seen, stop);
            assert_eq!(db.standing_queries.len(), len);
            assert_eq!(db.standing_native_query(&cx, &h, policy()).unwrap(), before);
        }
        let mut seen = 0;
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = register_checked(&mut db, &cx, &query, policy(), &mut || {
                    seen += 1;
                    assert!(seen != calls, "injected unwind");
                    Ok(())
                });
            }))
            .is_err()
        );
        assert_eq!(db.standing_queries.len(), len);
        let (first, last) = range(&h);
        let mut rebuild_calls = 0;
        rebuild_checked(&mut db, &cx, first, last, policy(), &mut || {
            rebuild_calls += 1;
            Ok(())
        })
        .unwrap();
        for stop in 1..=rebuild_calls {
            let mut seen = 0;
            let result = rebuild_checked(&mut db, &cx, first, last, policy(), &mut || {
                seen += 1;
                if seen == stop {
                    Err(StandingQueryError::Maintenance(
                        StandingQueryFailure::Interrupted,
                    ))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err());
            assert_eq!(seen, stop);
            assert_eq!(db.standing_queries.len(), len);
            assert_eq!(db.standing_native_query(&cx, &h, policy()).unwrap(), before);
        }
        let mut seen = 0;
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                let _ = rebuild_checked(&mut db, &cx, first, last, policy(), &mut || {
                    seen += 1;
                    assert!(seen != rebuild_calls, "injected rebuild unwind");
                    Ok(())
                });
            }))
            .is_err()
        );
        assert_eq!(db.standing_queries.len(), len);
        assert_eq!(db.standing_native_query(&cx, &h, policy()).unwrap(), before);
        db.rebuild_standing_query(&cx, &h, policy()).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(1));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &h, policy()).unwrap().1,
            ordinary(&db, &cx, &query)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
