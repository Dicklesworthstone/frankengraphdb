//! Finite pages preserve batch scope order through the production native circuit.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, GraphValueOrder, IntegerComparison};
use fgdb_gql::{
    GqlParameters, GqlScalarParameter, GraphIntegerExpression, GraphIntegerOp, GraphIntegerUnary,
    GraphSetOperand, GraphSetPredicateOp, GraphSetProjection, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

const P: PropertyKeyId = PropertyKeyId(1);
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 20_000_000, 20_000_000)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xf1; 32],
        DatabaseSecurityNamespaceId([0xf2; 32]),
        [0xf3; 32],
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
fn less_than_eight() -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(0),
        comparison: IntegerComparison::Less,
        right: GraphSetOperand::Literal(GqlScalarParameter::new(CanonicalScalar::Int(8)).unwrap()),
    }
}
fn order(descending: bool) -> Vec<GraphValueOrder> {
    vec![GraphValueOrder {
        column: 0,
        descending,
        nulls_first: false,
    }]
}
fn queries() -> Vec<PreparedGraphSet> {
    let ranked = leaf()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, leaf())
        .unwrap()
        .with_order_by(&order(true))
        .unwrap()
        .with_page(1, Some(7));
    let filtered = ranked.clone().filter(&[less_than_eight()]).unwrap();
    let negative = GraphIntegerExpression::prepare(&[
        GraphIntegerOp::Column(0),
        GraphIntegerOp::Unary(GraphIntegerUnary::Negate),
    ])
    .unwrap();
    vec![
        ranked.clone(),
        ranked.clone().nested().unwrap().with_page(1, Some(2)),
        filtered.clone(), // No outer page: delivery must retain inherited order.
        filtered
            .clone()
            .project(
                vec![GraphSetProjection::new(
                    "negated",
                    GraphSetValue::Integer(negative),
                )],
                GraphSetQuantifier::All,
            )
            .unwrap(), // Projection deliberately canonicalizes.
        leaf()
            .project(
                vec![GraphSetProjection::new("p", GraphSetValue::Column(0))],
                GraphSetQuantifier::Distinct,
            )
            .unwrap()
            .with_order_by(&order(true))
            .unwrap()
            .with_page(0, Some(3)),
        filtered.nested().unwrap().with_page(1, Some(2)), // Must not re-page canonical order.
        ranked
            .nested()
            .unwrap()
            .with_order_by(&order(false))
            .unwrap()
            .with_page(0, Some(2)),
        leaf().with_page(0, Some(0)),
    ]
}
fn batch_result<V: Vfs + Clone>(
    db: &Database<V>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
) -> QueryResult {
    let view = db.read_session().unwrap();
    let rows = view
        .execute_graph_set_governed_at(cx, query, view.frontier(), policy())
        .unwrap()
        .value;
    QueryResult::Rows {
        columns: query.columns().to_vec(),
        rows: rows
            .iter()
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
fn integers(values: &[i64]) -> QueryResult {
    QueryResult::Rows {
        columns: vec!["p".into()],
        rows: values
            .iter()
            .map(|n| {
                vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                    *n,
                )))]
            })
            .collect(),
    }
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [
        (1, Some(8)),
        (2, Some(3)),
        (3, Some(8)),
        (4, None),
        (5, Some(1)),
        (6, Some(6)),
    ] {
        batch.create_vertex(
            VId(id),
            vec![],
            value
                .map(|n| vec![(P, CanonicalScalar::Int(n))])
                .unwrap_or_default(),
        );
    }
    batch
}

#[test]
fn native_windows_filters_and_projection_scopes_match_batch_order_after_commits() {
    let ((), report) = run_async_under_lab(0x574e_0301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let definitions = queries();
        let handles: Vec<_> = definitions
            .iter()
            .map(|q| register(&mut db, &cx, q, policy()).unwrap())
            .collect();
        // Independent concrete controls ensure the differential's comparator
        // cannot bless canonical ordering where descending order is required.
        assert_eq!(
            db.standing_native_query(&cx, &handles[0], policy())
                .unwrap()
                .1,
            integers(&[8, 8, 8, 6, 6, 3, 3])
        );
        assert_eq!(
            db.standing_native_query(&cx, &handles[2], policy())
                .unwrap()
                .1,
            integers(&[6, 6, 3, 3])
        );
        assert_eq!(
            db.standing_native_query(&cx, &handles[5], policy())
                .unwrap()
                .1,
            integers(&[6, 3])
        );
        for step in 0..5 {
            for (handle, definition) in handles.iter().zip(&definitions) {
                let actual = db.standing_native_query(&cx, handle, policy()).unwrap();
                assert_eq!(actual.0, db.frontier().unwrap());
                assert_eq!(actual.1, batch_result(&db, &cx, definition));
            }
            let mut change = WriteBatch::new(RelationId(1));
            match step {
                0 => {
                    change.delete_vertex(VId(1));
                }
                1 => {
                    change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(2)));
                }
                2 => {
                    change.set_vertex_property(VId(4), P, Some(CanonicalScalar::Int(7)));
                }
                3 => {
                    change.delete_vertex(VId(6));
                }
                _ => {
                    change.create_vertex(VId(99), vec![], vec![(P, CanonicalScalar::Int(10))]);
                }
            }
            db.write(&commit, change).await.unwrap();
        }
        for (handle, definition) in handles.iter().zip(&definitions) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert_eq!(
                db.standing_native_query(&cx, handle, policy()).unwrap().1,
                batch_result(&db, &cx, definition)
            );
        }
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(99));
        db.write(&commit, change).await.unwrap();
        for (handle, definition) in handles.iter().zip(&definitions) {
            assert_eq!(
                db.standing_native_query(&cx, handle, policy()).unwrap().1,
                batch_result(&db, &cx, definition)
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
        .map(|query| {
            let (policy, at, failure) = query.status();
            let rows = sets::rows(query).unwrap();
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
fn every_window_circuit_registration_and_rebuild_boundary_preserves_old_nodes() {
    let ((), report) = run_async_under_lab(0x574e_0302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let query = queries().remove(2); // Includes a final order-restoring window.
        let mut calls = 0;
        let handle = register_checked(&mut db, &cx, &query, policy(), &mut || {
            calls += 1;
            Ok(())
        })
        .unwrap();
        let before = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                register_checked(&mut db, &cx, &query, policy(), &mut || {
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
            assert_eq!(saved(&db.standing_queries), before);
        }
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
        let before = saved(&db.standing_queries);
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
            assert_eq!(saved(&db.standing_queries), before);
        }
        let expected = db.standing_native_query(&cx, &handle, policy()).unwrap();
        assert!(matches!(
            db.standing_native_query(
                &cx,
                &handle,
                GqlQueryPolicy::new(0, 0, 20_000_000, 20_000_000)
            ),
            Err(StandingQueryError::Delivery(
                StandingQueryFailure::ResultBudget
            ))
        ));
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap(),
            expected
        );
        let mut change = WriteBatch::new(RelationId(1));
        change.delete_vertex(VId(6));
        db.write(&commit, change).await.unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap().1,
            batch_result(&db, &cx, &query)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_window_never_bypasses_child_errors_or_per_node_admission() {
    let ((), report) = run_async_under_lab(0x574e_0303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let handle = register(&mut db, &cx, &leaf().with_page(0, Some(0)), policy()).unwrap();
        assert_eq!(
            db.standing_native_query(&cx, &handle, policy()).unwrap().1,
            integers(&[])
        );
        let before = saved(&db.standing_queries);
        let unsupported = leaf()
            .with_page(1, None)
            .with_order_by(&order(true))
            .unwrap();
        assert!(register(&mut db, &cx, &unsupported, policy()).is_err());
        // A finite zero at the parent cannot legitimize unsupported descendants.
        let unsupported = leaf()
            .with_page(1, None)
            .with_order_by(&order(true))
            .unwrap()
            .nested()
            .unwrap()
            .with_page(0, Some(0));
        assert!(register(&mut db, &cx, &unsupported, policy()).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        let code = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(fgdb_gql::GraphIntegerBinary::Divide),
        ])
        .unwrap();
        let broken = leaf()
            .project(
                vec![GraphSetProjection::new("p", GraphSetValue::Integer(code))],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .with_page(0, Some(0));
        assert!(matches!(
            register(&mut db, &cx, &broken, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::OutputExpression { .. }
            ))
        ));
        assert!(
            register(
                &mut db,
                &cx,
                &leaf().with_page(0, Some(0)),
                GqlQueryPolicy::new(0, 0, 20_000_000, 20_000_000)
            )
            .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        // Preserve the concurrent unwindowed Cartesian capability. Its implicit
        // left-major enumeration cannot silently become a native page's order.
        let product = leaf().cross_join(leaf()).unwrap();
        let product_handle = db
            .register_standing_relation(&cx, &product, policy())
            .unwrap();
        let accepted = saved(&db.standing_queries);
        for paged in [
            product.with_page(0, Some(0)),
            leaf().cross_join(leaf().with_page(0, Some(2))).unwrap(),
        ] {
            assert!(matches!(
                db.register_standing_relation(&cx, &paged, policy()),
                Err(StandingQueryError::Unsupported)
            ));
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        // Explicit typed ranking of that maintained BAG has no implicit-order
        // ambiguity and remains available without re-executing a graph query.
        let ranked = db
            .register_standing_window(
                &cx,
                &product_handle,
                &order(true),
                GraphSetQuantifier::All,
                0,
                2,
                policy(),
            )
            .unwrap();
        assert_eq!(
            db.standing_window_total(&cx, &ranked).unwrap(),
            &fgdb_delta_types::ZWeight::from_i128(2)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
