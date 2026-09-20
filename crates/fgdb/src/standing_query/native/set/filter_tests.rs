//! Exercise the real registry, graph commits, native delivery and rebuild paths.
//! Expected results come from independent property scans, not the filter kernel.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, ZWeight};
use fgdb_gql::algebra::{GraphValue, IntegerComparison};
use fgdb_gql::{
    GqlScalarParameter, GraphIntegerBinary as Binary, GraphIntegerExpression, GraphIntegerOp as Op,
    GraphSetColumnType, GraphSetOperand, GraphSetPredicateOp as Predicate, GraphSetProjection,
    GraphSetValue, GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
const LIMBS: LimbLimit = LimbLimit::new(4);
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
fn leaf() -> PreparedGraphSet {
    PreparedGraphSet::from(
        PreparedGraphText::prepare("MATCH (n) RETURN n.p AS p", |kind, name: &str| {
            match (kind, name) {
                (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
                _ => None,
            }
        })
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap(),
    )
}
fn compare(comparison: IntegerComparison, value: i64) -> Predicate {
    Predicate::Compare {
        left: GraphSetOperand::Column(0),
        comparison,
        right: GraphSetOperand::Literal(
            GqlScalarParameter::new(CanonicalScalar::Int(value)).unwrap(),
        ),
    }
}
fn positive() -> Predicate {
    compare(IntegerComparison::Greater, 0)
}
fn expression(binary: Binary) -> Vec<GraphSetProjection> {
    let code = if binary == Binary::Divide {
        vec![Op::Literal(Some(12)), Op::Column(0), Op::Binary(binary)]
    } else {
        vec![Op::Column(0), Op::Literal(Some(2)), Op::Binary(binary)]
    };
    vec![GraphSetProjection::new(
        "value",
        GraphSetValue::Integer(GraphIntegerExpression::prepare(&code).unwrap()),
    )]
}
fn definition(quantifier: GraphSetQuantifier) -> PreparedGraphSet {
    leaf()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, leaf())
        .unwrap()
        .filter(&[positive()])
        .unwrap()
        .project(expression(Binary::Remainder), quantifier)
        .unwrap()
        .nested()
        .unwrap()
        .filter(&[compare(IntegerComparison::Equal, 1)])
        .unwrap()
}
fn expected<V: Vfs + Clone>(
    db: &Database<V>,
    quantifier: GraphSetQuantifier,
) -> (CommitSeq, QueryResult) {
    let at = db.frontier().unwrap();
    let mut count = 0;
    for row in db.vertices_at(at).unwrap() {
        if matches!(row.props.iter().find(|(key, _)| *key == P).map(|(_, v)| v),
            Some(CanonicalScalar::Int(n)) if *n > 0 && n % 2 == 1)
        {
            count += 2;
        }
    }
    if quantifier == GraphSetQuantifier::Distinct && count > 0 {
        count = 1;
    }
    (
        at,
        QueryResult::Rows {
            columns: vec!["value".into()],
            rows: (0..count)
                .map(|_| {
                    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                        1,
                    )))]
                })
                .collect(),
        },
    )
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
        .map(|query| {
            let rows = sets::rows(query).unwrap();
            let (policy, at, failure) = query.status();
            (
                std::ptr::from_ref(rows) as usize,
                policy,
                at,
                failure,
                rows.checked_clone(LIMBS, &mut |_| Ok::<_, ()>(())).unwrap(),
            )
        })
        .collect()
}

#[test]
fn native_filters_before_and_after_distinct_publish_exact_successor_deltas() {
    let ((), report) = run_async_under_lab(0x6669_6c01, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [
            (1, Some(-3)),
            (2, Some(0)),
            (3, Some(3)),
            (4, Some(3)),
            (u128::MAX, None),
        ] {
            seed.create_vertex(
                VId(id),
                vec![],
                value
                    .map(|n| vec![(P, CanonicalScalar::Int(n))])
                    .unwrap_or_default(),
            );
        }
        seed.create_vertex(
            VId(9),
            vec![],
            vec![(
                P,
                CanonicalScalar::ucs_basic_text("not an integer").unwrap(),
            )],
        );
        db.write(&commit, seed).await.unwrap();
        let all = register(&mut db, &cx, &definition(GraphSetQuantifier::All), policy()).unwrap();
        let distinct = register(
            &mut db,
            &cx,
            &definition(GraphSetQuantifier::Distinct),
            policy(),
        )
        .unwrap();
        assert!(db.standing_projection_delta(&cx, &all).unwrap().is_none());
        for tick in 0..6 {
            let mut prior = db
                .standing_projection(&cx, &all)
                .unwrap()
                .rows()
                .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            for (handle, q) in [
                (&all, GraphSetQuantifier::All),
                (&distinct, GraphSetQuantifier::Distinct),
            ] {
                assert_eq!(
                    db.standing_native_query(&cx, handle, policy()).unwrap(),
                    expected(&db, q)
                );
            }
            let mut update = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    update.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(5)));
                }
                1 => {
                    update.delete_vertex(VId(4));
                    update.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(7)));
                }
                2 => {
                    update.set_vertex_property(VId(9), P, Some(CanonicalScalar::Int(9)));
                }
                3 => {
                    update.delete_vertex(VId(3));
                    update.set_vertex_property(VId(u128::MAX), P, None);
                }
                4 => {
                    update.set_vertex_property(VId(9), P, Some(CanonicalScalar::Int(2)));
                }
                _ => {
                    update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
                }
            }
            db.write(&commit, update).await.unwrap();
            let delta = db.standing_projection_delta(&cx, &all).unwrap().unwrap();
            assert_eq!(delta.frontier(), db.frontier().unwrap());
            if tick == 0 {
                assert!(delta.rows().is_empty());
            }
            prior
                .integrate(delta.rows(), LIMBS, &mut |_| Ok::<_, ()>(()))
                .unwrap();
            assert_eq!(&prior, db.standing_projection(&cx, &all).unwrap().rows());
        }
        for (handle, q) in [
            (&all, GraphSetQuantifier::All),
            (&distinct, GraphSetQuantifier::Distinct),
        ] {
            assert_eq!(
                db.standing_native_query(&cx, handle, policy()).unwrap(),
                expected(&db, q)
            );
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_projection_delta(&cx, handle).unwrap().is_none());
        }
        let mut update = WriteBatch::new(RelationId(1));
        update.delete_vertex(VId(1));
        db.write(&commit, update).await.unwrap();
        for (handle, q) in [
            (&all, GraphSetQuantifier::All),
            (&distinct, GraphSetQuantifier::Distinct),
        ] {
            assert_eq!(
                db.standing_native_query(&cx, handle, policy()).unwrap(),
                expected(&db, q)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filter_circuit_registration_and_rebuild_roll_back_at_every_stage() {
    let ((), report) = run_async_under_lab(0x6669_6c02, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(3))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = register(&mut db, &cx, &leaf(), policy()).unwrap();
        let before = saved(&db.standing_queries);
        let query = definition(GraphSetQuantifier::Distinct);
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &query, policy(), &mut || {
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
                        .compile(&cx, &query, policy(), &mut || {
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
        let handle = register(&mut db, &cx, &query, policy()).unwrap();
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
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, update).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected(&db, GraphSetQuantifier::Distinct)
        );
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filter_scope_does_not_hide_unsupported_pages_or_errors_in_completed_children() {
    let ((), report) = run_async_under_lab(0x6669_6c03, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = register(&mut db, &cx, &leaf(), policy()).unwrap();
        let before = saved(&db.standing_queries);
        let filtered = leaf().filter(&[positive()]).unwrap();
        let (child, code) = filtered.incremental_filter().unwrap();
        assert_eq!(child.canonical_bytes(), leaf().canonical_bytes());
        assert_eq!(code, &[positive()]);
        assert!(leaf().incremental_filter().is_none());
        for query in [
            filtered
                .clone()
                .with_order_by(&[fgdb_gql::algebra::GraphValueOrder {
                    column: 0,
                    descending: true,
                    nulls_first: false,
                }])
                .unwrap(),
            filtered.with_page(1, None),
            leaf()
                .with_page(1, None)
                .filter(&[Predicate::Truth(Some(false))])
                .unwrap(),
        ] {
            assert!(register(&mut db, &cx, &query, policy()).is_err());
            assert_eq!(saved(&db.standing_queries), before);
        }
        // A filter cannot suppress an error in an already completed child.
        let invalid = leaf()
            .project(expression(Binary::Divide), GraphSetQuantifier::All)
            .unwrap()
            .filter(&[Predicate::Truth(Some(false))])
            .unwrap();
        assert!(matches!(
            register(&mut db, &cx, &invalid, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::OutputExpression { column: 0, .. }
            ))
        ));
        assert_eq!(saved(&db.standing_queries), before);
        // In the opposite scope order, no retained row evaluates the division.
        let valid = leaf()
            .filter(&[positive()])
            .unwrap()
            .project(expression(Binary::Divide), GraphSetQuantifier::All)
            .unwrap();
        let h = register(&mut db, &cx, &valid, policy()).unwrap();
        assert!(db.standing_projection(&cx, &h).unwrap().rows().is_empty());
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn public_filtered_specs_compose_fence_dependents_and_rebuild_without_rebinding() {
    let ((), report) = run_async_under_lab(0x6669_6c04, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let parent = register(&mut db, &cx, &leaf(), policy()).unwrap();
        // Empty data must not turn a declared vertex schema into scalar input.
        let wrong = RowProjectionSpec::selection(
            vec![GraphSetColumnType::Vertex],
            vec!["p".into()],
            &[positive()],
        );
        assert!(wrong.is_err());
        let wrong = RowProjectionSpec::selection(
            vec![GraphSetColumnType::Vertex],
            vec!["p".into()],
            &[Predicate::Truth(Some(true))],
        )
        .unwrap();
        let before = saved(&db.standing_queries);
        assert!(
            db.register_standing_projection_spec(&cx, &parent, wrong, policy())
                .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        let spec = RowProjectionSpec::new(
            vec![GraphSetColumnType::Scalar],
            expression(Binary::Divide),
            GraphSetQuantifier::All,
        )
        .unwrap()
        .with_filter(&[positive()])
        .unwrap();
        let mut other = Database::open_memory(&commit, keys()).await.unwrap();
        assert!(matches!(
            other.register_standing_projection_spec(&cx, &parent, spec.clone(), policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        let filtered = db
            .register_standing_projection_spec(
                &cx,
                &parent,
                spec,
                GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let dependent = db
            .register_standing_set(&cx, &filtered, &filtered, SetOperation::UnionAll, policy())
            .unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, n) in [(1, 2), (2, 0), (3, -1)] {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(n))]);
        }
        db.write(&commit, seed).await.unwrap();
        assert_eq!(
            db.standing_projection_total(&cx, &filtered).unwrap(),
            &ZWeight::ONE
        );
        assert_eq!(
            db.standing_set_total(&cx, &dependent).unwrap(),
            &ZWeight::from_i128(2)
        );
        let mut exceed = WriteBatch::new(RelationId(1));
        exceed.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(4)));
        db.write(&commit, exceed).await.unwrap();
        assert!(matches!(
            db.standing_projection(&cx, &filtered),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::ResultBudget,
                ..
            })
        ));
        assert!(matches!(
            db.standing_set(&cx, &dependent),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::DependencyUnavailable,
                ..
            })
        ));
        db.standing_native_query(&cx, &parent, policy()).unwrap();
        db.rebuild_standing_query(&cx, &filtered, policy()).unwrap();
        assert!(
            db.standing_projection_delta(&cx, &filtered)
                .unwrap()
                .is_none()
        );
        db.rebuild_standing_query(&cx, &dependent, policy())
            .unwrap();
        let mut hide = WriteBatch::new(RelationId(1));
        hide.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
        db.write(&commit, hide).await.unwrap();
        assert_eq!(
            db.standing_projection_total(&cx, &filtered).unwrap(),
            &ZWeight::ONE
        );
        assert_eq!(
            db.standing_set_total(&cx, &dependent).unwrap(),
            &ZWeight::from_i128(2)
        );
        let view = db.standing_projection(&cx, &filtered).unwrap();
        let row = view.rows().iter().next().unwrap().0;
        assert_eq!(row.values(), &[GraphValue::Scalar(CanonicalScalar::Int(3))]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rejected_tuple_updates_advance_zero_quota_views_with_changed_key_work_only() {
    let ((), report) = run_async_under_lab(0x6669_6c05, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut observations = Vec::new();
        for size in [8, 512] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 0..size {
                seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
            }
            db.write(&commit, seed).await.unwrap();
            let parent = register(&mut db, &cx, &leaf(), policy()).unwrap();
            let spec = RowProjectionSpec::selection(
                vec![GraphSetColumnType::Scalar],
                vec!["p".into()],
                &[Predicate::Truth(None)],
            )
            .unwrap();
            let h = db
                .register_standing_projection_spec(
                    &cx,
                    &parent,
                    spec,
                    GqlQueryPolicy::new(100_000, 0, 10_000_000, 10_000_000),
                )
                .unwrap();
            assert!(db.standing_projection_delta(&cx, &h).unwrap().is_none());
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(2048)));
            db.write(&commit, update).await.unwrap();
            let view = db.standing_projection(&cx, &h).unwrap();
            assert_eq!(view.frontier(), db.frontier().unwrap());
            assert!(view.rows().is_empty());
            observations.push(*view.last_maintenance());
            assert!(
                db.standing_projection_delta(&cx, &h)
                    .unwrap()
                    .unwrap()
                    .rows()
                    .is_empty()
            );
        }
        assert_eq!(observations[0], observations[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
