//! Committed graph updates through public products and owned native circuits.
use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, ZWeight};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GraphIntegerBinary as Binary, GraphIntegerExpression, GraphIntegerOp as Op, GraphSetProjection,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x51; 32],
        DatabaseSecurityNamespaceId([0x52; 32]),
        [0x53; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn pattern(text: &str) -> PreparedGraphPattern<GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn leaf(name: &str) -> PreparedGraphSet {
    pattern(&format!("MATCH (n) RETURN n.p AS {name}")).into()
}
fn product() -> PreparedGraphSet {
    leaf("a").cross_join(leaf("b")).unwrap()
}
fn distinct_product() -> PreparedGraphSet {
    product()
        .nested()
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("a", GraphSetValue::Column(0)),
                GraphSetProjection::new("b", GraphSetValue::Column(1)),
            ],
            GraphSetQuantifier::Distinct,
        )
        .unwrap()
}
fn expected<V: Vfs + Clone>(db: &Database<V>, distinct: bool) -> (CommitSeq, QueryResult) {
    let at = db.frontier().unwrap();
    let values: Vec<_> = db
        .vertices_at(at)
        .unwrap()
        .into_iter()
        .map(|v| {
            GraphValue::Scalar(
                v.props
                    .iter()
                    .find(|(k, _)| *k == P)
                    .map(|(_, v)| v.clone())
                    .unwrap_or(CanonicalScalar::Null),
            )
        })
        .collect();
    let mut rows = Vec::new();
    for left in &values {
        for right in &values {
            rows.push(vec![left.clone(), right.clone()]);
        }
    }
    rows.sort();
    if distinct {
        rows.dedup();
    }
    (
        at,
        QueryResult::Rows {
            columns: vec!["a".into(), "b".into()],
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(QueryValue::Value).collect())
                .collect(),
        },
    )
}

#[test]
fn public_relational_products_deliver_exact_bags_and_both_input_cross_terms_after_commits() {
    let ((), report) = run_async_under_lab(0x6372_6f01, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(1, Some(1)), (2, Some(1)), (3, None)] {
            seed.create_vertex(
                VId(id),
                vec![],
                value
                    .map(|v| vec![(P, CanonicalScalar::Int(v))])
                    .unwrap_or_default(),
            );
        }
        db.write(&commit, seed).await.unwrap();
        // These temporary bound trees are dropped immediately after registration.
        let all = db
            .register_standing_relation(&cx, &product(), policy())
            .unwrap();
        let distinct = db
            .register_standing_relation(&cx, &distinct_product(), policy())
            .unwrap();
        let doubled = db
            .register_standing_set(&cx, &all, &all, SetOperation::UnionAll, policy())
            .unwrap();
        assert_eq!(db.standing_native_columns(&cx, &all).unwrap(), &["a", "b"]);
        assert_eq!(
            db.standing_join_columns(&cx, &all).unwrap(),
            &["left.a", "right.b"]
        );
        assert!(db.standing_join_delta(&cx, &all).unwrap().is_none());
        assert_eq!(
            db.standing_native_query(&cx, &all, policy()).unwrap(),
            expected(&db, false)
        );
        for tick in 0..5 {
            let mut integrated = db
                .standing_join(&cx, &all)
                .unwrap()
                .rows()
                .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                .unwrap();
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2)));
                }
                1 => {
                    change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(3)));
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
            for (handle, is_distinct) in [(&all, false), (&distinct, true)] {
                assert_eq!(
                    db.standing_native_query(
                        &cx,
                        handle,
                        GqlQueryPolicy::new(0, 100_000, 10_000_000, 10_000_000)
                    )
                    .unwrap(),
                    expected(&db, is_distinct)
                );
            }
            integrated
                .integrate(
                    db.standing_join_delta(&cx, &all).unwrap().unwrap().rows(),
                    LimbLimit::new(4),
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(&integrated, db.standing_join(&cx, &all).unwrap().rows());
            let total = db.standing_join_total(&cx, &all).unwrap();
            assert_eq!(
                db.standing_set_total(&cx, &doubled).unwrap(),
                &total
                    .checked_mul(&ZWeight::from_i128(2), LimbLimit::new(4))
                    .unwrap()
            );
        }
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
fn product_circuit_admission_and_rebuild_are_atomic_at_every_stage_and_rebase_both_inputs() {
    let ((), report) = run_async_under_lab(0x6372_6f02, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(2))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = db
            .register_standing_relation(&cx, &leaf("sibling"), policy())
            .unwrap();
        let before = saved(&db.standing_queries);
        let definition = distinct_product();
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
        assert_eq!(saved(&db.standing_queries), before);
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
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(VId(2), vec![], vec![]);
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected(&db, true)
        );
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_shared_products_fence_only_dependents_and_rebuild_their_unconditional_definition() {
    let ((), report) = run_async_under_lab(0x6372_6f03, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(1))]);
        }
        db.write(&commit, seed).await.unwrap();
        let parent = db
            .register_standing_rows(&cx, pattern("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_join(&cx, &parent, &parent, &[], policy()),
            Err(StandingQueryError::JoinSchema(
                fgdb_gql::row_join::RowJoinBuildError::EmptyKeys
            ))
        ));
        let cross = db
            .register_standing_cross_join(
                &cx,
                &parent,
                &parent,
                GqlQueryPolicy::new(100_000, 4, 10_000_000, 10_000_000),
            )
            .unwrap();
        let child = db
            .register_standing_set(&cx, &cross, &cross, SetOperation::UnionDistinct, policy())
            .unwrap();
        assert_eq!(
            db.standing_join_total(&cx, &cross).unwrap(),
            &ZWeight::from_i128(4)
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.create_vertex(VId(3), vec![], vec![]);
        db.write(&commit, change).await.unwrap();
        assert!(matches!(
            db.standing_join(&cx, &cross),
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
        db.standing_rows(&cx, &parent).unwrap();
        assert_eq!(db.vertices_at(db.frontier().unwrap()).unwrap().len(), 3);
        db.rebuild_standing_query(&cx, &cross, policy()).unwrap();
        assert!(db.standing_join_delta(&cx, &cross).unwrap().is_none());
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert_eq!(
            db.standing_join_total(&cx, &cross).unwrap(),
            &ZWeight::from_i128(9)
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(1));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_join_total(&cx, &cross).unwrap(),
            &ZWeight::from_i128(4)
        );
        db.standing_set(&cx, &child).unwrap();
        let before = db.standing_queries.len();
        assert!(matches!(
            db.register_standing_cross_join(
                &cx,
                &parent,
                &parent,
                GqlQueryPolicy::new(0, 100, 10_000_000, 10_000_000)
            ),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
        assert_eq!(db.standing_queries.len(), before);
        let mut foreign = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            foreign.register_standing_cross_join(&cx, &parent, &parent, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_empty_operand_never_suppresses_peer_errors_or_unsupported_scope_semantics() {
    let ((), report) = run_async_under_lab(0x6372_6f04, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = db
            .register_standing_relation(&cx, &leaf("sibling"), policy())
            .unwrap();
        let before = saved(&db.standing_queries);
        let empty: PreparedGraphSet = pattern("MATCH (n) RETURN n.p AS a LIMIT 0").into();
        let divide = GraphIntegerExpression::prepare(&[
            Op::Literal(Some(1)),
            Op::Column(0),
            Op::Binary(Binary::Divide),
        ])
        .unwrap();
        let bad = leaf("b")
            .project(
                vec![GraphSetProjection::new("b", GraphSetValue::Integer(divide))],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let definition = empty.clone().cross_join(bad).unwrap();
        assert!(matches!(
            db.register_standing_relation(&cx, &definition, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::OutputExpression { .. }
            ))
        ));
        assert_eq!(saved(&db.standing_queries), before);
        for definition in [
            product().with_page(0, Some(0)),
            empty.cross_join(leaf("b").with_page(0, Some(0))).unwrap(),
        ] {
            assert!(
                db.register_standing_relation(&cx, &definition, policy())
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), before);
        }
        let healthy = db
            .register_standing_relation(&cx, &product(), policy())
            .unwrap();
        assert!(matches!(
            db.standing_native_query(&cx, &healthy, GqlQueryPolicy::new(0, 0, 100_000, 100_000)),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert_eq!(
            db.standing_native_query(&cx, &healthy, policy()).unwrap(),
            expected(&db, false)
        );
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_list_products_and_compaction_reopen_keep_domains_and_session_ownership() {
    let ((), report) = run_async_under_lab(0x6372_6f05, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let ids: PreparedGraphSet = pattern("MATCH (n) RETURN n AS id").into();
        let lists = ids
            .project(
                vec![GraphSetProjection::new(
                    "xs",
                    GraphSetValue::List(vec![GraphSetValue::Column(0)]),
                )],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let definition = lists.cross_join(leaf("p")).unwrap();
        let handle = db
            .register_standing_relation(&cx, &definition, policy())
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(u128::MAX), vec![], vec![]);
        db.write(&commit, seed).await.unwrap();
        let expected = db.standing_native_query(&cx, &handle, policy()).unwrap();
        let QueryResult::Rows { rows, .. } = &expected.1 else {
            panic!("row result")
        };
        assert_eq!(
            *rows,
            vec![vec![
                QueryValue::Value(GraphValue::List(
                    vec![GraphValue::Vertex(VId(u128::MAX))].into_boxed_slice()
                )),
                QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Null))
            ]]
        );
        db.compact(&commit).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected
        );
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_native_query(&cx, &handle, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        let handle = db
            .register_standing_relation(&cx, &definition, policy())
            .unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(u128::MAX));
        db.write(&commit, change).await.unwrap();
        assert!(db.standing_join(&cx, &handle).unwrap().rows().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
