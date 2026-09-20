//! Real committed list expansions through native text and bound relation APIs.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, ZWeight};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GraphSetProjection, GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn leaf() -> PreparedGraphSet {
    PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .into()
}
fn expansion() -> PreparedGraphSet {
    leaf()
        .unwind(
            "element".into(),
            GraphSetValue::List(vec![GraphSetValue::Column(0), GraphSetValue::Column(0)]),
        )
        .unwrap()
}
fn composed() -> PreparedGraphSet {
    expansion()
        .project(
            vec![GraphSetProjection::new("element", GraphSetValue::Column(1))],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
}
fn expected<V: Vfs + Clone>(db: &Database<V>, distinct: bool) -> (CommitSeq, QueryResult) {
    let at = db.frontier().unwrap();
    let mut values = Vec::new();
    for vertex in db.vertices_at(at).unwrap() {
        let value = vertex
            .props
            .iter()
            .find(|(k, _)| *k == P)
            .map(|(_, v)| v.clone())
            .unwrap_or(CanonicalScalar::Null);
        match value {
            CanonicalScalar::Int(n) => {
                values.push(GraphValue::Scalar(CanonicalScalar::Int(n)));
                values.push(GraphValue::Scalar(CanonicalScalar::Int(n + 1)));
            }
            CanonicalScalar::Null => {
                values.push(GraphValue::Scalar(CanonicalScalar::Null));
                values.push(GraphValue::Scalar(CanonicalScalar::Null));
            }
            _ => unreachable!("fixture domain"),
        }
    }
    values.sort();
    if distinct {
        values.dedup();
    }
    (
        at,
        QueryResult::Rows {
            columns: vec!["x".into()],
            rows: values
                .into_iter()
                .map(|v| vec![QueryValue::Value(v)])
                .collect(),
        },
    )
}

#[test]
fn native_unwind_pipelines_maintain_duplicates_and_distinct_after_real_property_updates() {
    let ((), report) = run_async_under_lab(0x756e_7701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, Some(2)), (2, Some(2)), (3, None)] {
            seed.create_vertex(
                VId(id),
                vec![],
                value
                    .map(|n| vec![(P, CanonicalScalar::Int(n))])
                    .unwrap_or_default(),
            );
        }
        db.write(&commit, seed).await.unwrap();
        let all_text = "MATCH (n) WITH [n.p,n.p+1] AS xs UNWIND xs AS x RETURN x";
        let distinct_text = "MATCH (n) WITH [n.p,n.p+1] AS xs UNWIND xs AS x RETURN DISTINCT x";
        let all = db
            .register_standing_native(&cx, all_text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        let distinct = db
            .register_standing_native(&cx, distinct_text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        for tick in 0..5 {
            assert_eq!(
                db.standing_native_query(&cx, &all, policy()).unwrap(),
                expected(&db, false)
            );
            assert_eq!(
                db.standing_native_query(&cx, &distinct, policy()).unwrap(),
                expected(&db, true)
            );
            let mut integrated = db
                .standing_projection(&cx, &all)
                .unwrap()
                .rows()
                .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                .unwrap();
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(7)));
                }
                1 => {
                    change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(7)));
                }
                2 => {
                    change.delete_vertex(VId(2));
                }
                3 => {
                    change.create_vertex(VId(4), vec![], vec![]);
                }
                _ => {
                    for id in [1, 3, 4] {
                        change.delete_vertex(VId(id));
                    }
                }
            }
            db.write(&commit, change).await.unwrap();
            integrated
                .integrate(
                    db.standing_projection_delta(&cx, &all)
                        .unwrap()
                        .unwrap()
                        .rows(),
                    LimbLimit::new(4),
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(
                &integrated,
                db.standing_projection(&cx, &all).unwrap().rows()
            );
        }
        assert_eq!(
            db.standing_native_query(&cx, &all, policy()).unwrap(),
            expected(&db, false)
        );
        db.rebuild_standing_query(&cx, &all, policy()).unwrap();
        db.rebuild_standing_query(&cx, &distinct, policy()).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(VId(u128::MAX), vec![], vec![(P, CanonicalScalar::Int(9))]);
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &all, policy()).unwrap(),
            expected(&db, false)
        );
        assert_eq!(
            db.standing_native_query(&cx, &distinct, policy()).unwrap(),
            expected(&db, true)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bound_list_parameters_remain_frozen_and_preserve_nested_values_and_full_width_ids() {
    let ((), report) = run_async_under_lab(0x756e_7702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let nested = GraphValue::List(
            vec![
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Scalar(CanonicalScalar::Null),
            ]
            .into_boxed_slice(),
        );
        let mut args = GqlParameters::new()
            .with_list("xs", vec![nested.clone(), nested.clone()])
            .unwrap();
        let handle = db
            .register_standing_native(
                &cx,
                "MATCH (n) WITH n AS id UNWIND $xs AS x RETURN id,x",
                &args,
                symbols,
                policy(),
            )
            .unwrap();
        args = GqlParameters::new().with_list("xs", vec![]).unwrap();
        assert!(!args.canonical_bytes().is_empty()); // The registration does not borrow/rebind args.
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(5), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let row = vec![
            QueryValue::Value(GraphValue::Vertex(VId(5))),
            QueryValue::Value(nested),
        ];
        let expected = (
            db.frontier().unwrap(),
            QueryResult::Rows {
                columns: vec!["id".into(), "x".into()],
                rows: vec![row.clone(), row],
            },
        );
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected
        );
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected
        );
        assert!(matches!(
            db.standing_native_query(
                &cx,
                &handle,
                GqlQueryPolicy::new(0, 1, 1_000_000, 1_000_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expansion_quota_and_nonlist_failures_fence_dependents_without_undoing_durable_writes() {
    let ((), report) = run_async_under_lab(0x756e_7703, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let hidden = leaf()
            .unwind("element".into(), GraphSetValue::Column(0))
            .unwrap();
        let invalidated = db
            .register_standing_relation(&cx, &hidden, policy())
            .unwrap();
        assert!(
            db.standing_projection(&cx, &invalidated)
                .unwrap()
                .rows()
                .is_empty()
        );
        let expanded = db
            .register_standing_relation(
                &cx,
                &expansion(),
                GqlQueryPolicy::new(100_000, 2, 10_000_000, 10_000_000),
            )
            .unwrap();
        let child = db
            .register_standing_set(
                &cx,
                &expanded,
                &expanded,
                SetOperation::UnionDistinct,
                policy(),
            )
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
        change.create_vertex(VId(2), vec![], vec![]);
        db.write(&commit, change).await.unwrap();
        assert!(matches!(
            db.standing_projection(&cx, &invalidated),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::OutputExpression {
                    column: 1,
                    error: fgdb_gql::GraphIntegerError {
                        kind: fgdb_gql::GraphIntegerErrorKind::IncompatibleOperands,
                        ..
                    }
                },
                ..
            })
        ));
        assert!(matches!(
            db.standing_projection(&cx, &expanded),
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
        assert_eq!(db.vertices_at(db.frontier().unwrap()).unwrap().len(), 2);
        db.rebuild_standing_query(&cx, &expanded, policy()).unwrap();
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert_eq!(
            db.standing_projection_total(&cx, &expanded).unwrap(),
            &ZWeight::from_i128(4)
        );
        assert!(
            db.rebuild_standing_query(&cx, &invalidated, policy())
                .is_err()
        );
        let mut fix = WriteBatch::new(RelationId(1));
        fix.set_vertex_property(VId(1), P, None);
        db.write(&commit, fix).await.unwrap();
        db.rebuild_standing_query(&cx, &invalidated, policy())
            .unwrap();
        assert!(
            db.standing_projection_delta(&cx, &invalidated)
                .unwrap()
                .is_none()
        );
        assert!(
            db.standing_projection(&cx, &invalidated)
                .unwrap()
                .rows()
                .is_empty()
        );
        assert_eq!(
            db.standing_projection_total(&cx, &expanded).unwrap(),
            &ZWeight::from_i128(4)
        );
        db.standing_set(&cx, &child).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

type Saved = (
    usize,
    GqlQueryPolicy,
    CommitSeq,
    Option<StandingQueryFailure>,
    ZSet<GraphValueRow>,
);
fn saved(queries: &[StandingQuery]) -> Vec<Saved> {
    queries
        .iter()
        .map(|q| {
            let rows = sets::rows(q).unwrap();
            let (policy, at, failure) = q.status();
            (
                std::ptr::from_ref(rows) as usize,
                policy,
                at,
                failure,
                rows.checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                    .unwrap(),
            )
        })
        .collect()
}

#[test]
fn every_unwind_circuit_stage_refusal_and_unsupported_page_preserves_existing_nodes() {
    let ((), report) = run_async_under_lab(0x756e_7704, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        db.register_standing_relation(&cx, &leaf(), policy())
            .unwrap();
        let before = saved(&db.standing_queries);
        let definition = composed();
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &definition, policy(), &mut || {
                    calls += 1;
                    Ok(())
                })
                .unwrap();
        }
        for stop in 1..=calls {
            let mut seen = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(
                    staged
                        .compile(&cx, &definition, policy(), &mut || {
                            seen += 1;
                            if seen == stop {
                                Err(StandingQueryError::Maintenance(
                                    StandingQueryFailure::Interrupted,
                                ))
                            } else {
                                Ok(())
                            }
                        })
                        .is_err()
                );
            }
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        let handle = db
            .register_standing_relation(&cx, &definition, policy())
            .unwrap();
        let first = match handle.native.as_deref().unwrap() {
            Layout::Circuit { first, .. } => *first,
            _ => unreachable!(),
        };
        let mut calls = 0;
        rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let accepted = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                rebuild_checked(&mut db, &cx, first, handle.index, policy(), &mut || {
                    seen += 1;
                    if seen == stop {
                        Err(StandingQueryError::Maintenance(
                            StandingQueryFailure::Interrupted,
                        ))
                    } else {
                        Ok(())
                    }
                })
                .is_err()
            );
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        for query in [
            expansion().with_page(0, Some(0)),
            leaf()
                .with_page(0, Some(0))
                .unwind("element".into(), GraphSetValue::Column(0))
                .unwrap(),
        ] {
            assert!(
                db.register_standing_relation(&cx, &query, policy())
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_projection_total(&cx, &handle).unwrap(),
            &ZWeight::ONE
        );
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap().1,
            QueryResult::Rows {
                columns: vec!["element".into()],
                rows: vec![vec![QueryValue::Value(GraphValue::Scalar(
                    CanonicalScalar::Int(4)
                ))]]
            }
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
