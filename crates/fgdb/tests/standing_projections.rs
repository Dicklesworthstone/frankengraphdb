//! Real committed updates, maintained projection circuits, and storage oracles.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, StandingQueryError, StandingQueryFailure, StandingQueryHandle,
    WriteBatch,
};
use fgdb_delta_types::zset::set::SetOperation;
use fgdb_delta_types::{LimbLimit, PropertyKeyId, RelationId, ZWeight};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphIntegerBinary as Binary, GraphIntegerExpression,
    GraphIntegerOp as Op, GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn definition(
    text: &str,
) -> fgdb_gql::algebra::PreparedGraphPattern<fgdb_gql::algebra::GraphValueRow> {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn parity() -> Vec<GraphSetProjection> {
    vec![GraphSetProjection::new(
        "parity",
        GraphSetValue::Integer(
            GraphIntegerExpression::prepare(&[
                Op::Column(0),
                Op::Literal(Some(2)),
                Op::Binary(Binary::Remainder),
            ])
            .unwrap(),
        ),
    )]
}
fn actual<V: asupersync::fs::Vfs + Clone>(
    db: &Database<V>,
    cx: &fgdb_types::QueryCx,
    h: &StandingQueryHandle,
) -> BTreeMap<Vec<GraphValue>, i128> {
    db.standing_projection(cx, h)
        .unwrap()
        .rows()
        .iter()
        .map(|(r, w)| (r.values().to_vec(), w.to_i128().unwrap()))
        .collect()
}
fn oracle<V: asupersync::fs::Vfs + Clone>(
    db: &Database<V>,
    q: GraphSetQuantifier,
) -> BTreeMap<Vec<GraphValue>, i128> {
    let mut out = BTreeMap::new();
    for row in db.vertices_at(db.frontier().unwrap()).unwrap() {
        let value = match row.props.iter().find(|(key, _)| *key == P).map(|(_, v)| v) {
            Some(CanonicalScalar::Int(n)) => CanonicalScalar::Int(n % 2),
            None | Some(CanonicalScalar::Null) => CanonicalScalar::Null,
            _ => unreachable!("oracle fixture domain"),
        };
        *out.entry(vec![GraphValue::Scalar(value)]).or_insert(0) += 1;
    }
    if q == GraphSetQuantifier::Distinct {
        for count in out.values_mut() {
            *count = 1;
        }
    }
    out
}

#[test]
fn committed_projection_deltas_feed_shared_sets_and_joins_without_rescanning() {
    let ((), report) = run_async_under_lab(0x7072_6401, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [
            (1, Some(0)),
            (2, Some(2)),
            (3, Some(2)),
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
        db.write(&commit, seed).await.unwrap();
        let parent = db
            .register_standing_rows(&cx, definition("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        let all = db
            .register_standing_projection(&cx, &parent, parity(), GraphSetQuantifier::All, policy())
            .unwrap();
        let distinct = db
            .register_standing_projection(
                &cx,
                &parent,
                parity(),
                GraphSetQuantifier::Distinct,
                policy(),
            )
            .unwrap();
        let doubled = db
            .register_standing_set(&cx, &all, &all, SetOperation::UnionAll, policy())
            .unwrap();
        let joined = db
            .register_standing_join(&cx, &distinct, &distinct, &[(0, 0)], policy())
            .unwrap();
        assert!(db.standing_projection_delta(&cx, &all).unwrap().is_none());
        assert_eq!(
            db.standing_projection_columns(&cx, &all).unwrap(),
            &["parity"]
        );
        for tick in 0..5 {
            let mut before = db
                .standing_projection(&cx, &all)
                .unwrap()
                .rows()
                .checked_clone(LimbLimit::new(4), &mut |_| Ok::<_, ()>(()))
                .unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
                }
                1 => {
                    update.delete_vertex(VId(2));
                    update.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(5)));
                }
                2 => {
                    update.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(8)));
                }
                3 => {
                    update.delete_vertex(VId(1));
                    update.delete_vertex(VId(3));
                }
                _ => {
                    update.create_vertex(VId(77), vec![], vec![(P, CanonicalScalar::Int(6))]);
                }
            }
            db.write(&commit, update).await.unwrap();
            for (h, q) in [
                (&all, GraphSetQuantifier::All),
                (&distinct, GraphSetQuantifier::Distinct),
            ] {
                assert_eq!(actual(&db, &cx, h), oracle(&db, q));
                assert_eq!(
                    db.standing_projection(&cx, h).unwrap().frontier(),
                    db.frontier().unwrap()
                );
            }
            before
                .integrate(
                    db.standing_projection_delta(&cx, &all)
                        .unwrap()
                        .unwrap()
                        .rows(),
                    LimbLimit::new(4),
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
            assert_eq!(&before, db.standing_projection(&cx, &all).unwrap().rows());
            let sum: i128 = oracle(&db, GraphSetQuantifier::All).values().sum();
            assert_eq!(
                db.standing_set_total(&cx, &doubled).unwrap(),
                &ZWeight::from_i128(sum * 2)
            );
            let nonnull = oracle(&db, GraphSetQuantifier::Distinct)
                .keys()
                .filter(|r| !r[0].is_null())
                .count();
            assert_eq!(
                db.standing_join_total(&cx, &joined).unwrap(),
                &ZWeight::from_i128(nonnull as i128)
            );
            if tick == 0 {
                assert!(
                    db.standing_projection_delta(&cx, &all)
                        .unwrap()
                        .unwrap()
                        .rows()
                        .is_empty()
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_expression_and_result_failures_fence_only_dependents_and_rebuild_exactly() {
    let ((), report) = run_async_under_lab(0x7072_6402, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 1..=2 {
            seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(2))]);
        }
        db.write(&commit, seed).await.unwrap();
        let parent = db
            .register_standing_rows(&cx, definition("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        let q = db
            .register_standing_projection(
                &cx,
                &parent,
                parity(),
                GraphSetQuantifier::Distinct,
                GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000),
            )
            .unwrap();
        let child = db
            .register_standing_set(&cx, &q, &q, SetOperation::UnionDistinct, policy())
            .unwrap();
        let basis = db.frontier().unwrap();
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(3)));
        let at = db.write(&commit, update).await.unwrap();
        assert!(at > basis);
        assert!(matches!(
            db.standing_projection(&cx, &q),
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
        assert!(
            db.rebuild_standing_query(&cx, &q, GqlQueryPolicy::new(100, 100, 0, 0))
                .is_err()
        );
        db.rebuild_standing_query(&cx, &q, policy()).unwrap();
        assert!(db.standing_projection_delta(&cx, &q).unwrap().is_none());
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        let mut bad = WriteBatch::new(RelationId(1));
        bad.set_vertex_property(
            VId(2),
            P,
            Some(CanonicalScalar::ucs_basic_text("wrong kind").unwrap()),
        );
        db.write(&commit, bad).await.unwrap();
        assert!(matches!(
            db.standing_projection(&cx, &q),
            Err(StandingQueryError::Unavailable {
                reason: StandingQueryFailure::OutputExpression { column: 0, .. },
                ..
            })
        ));
        db.standing_rows(&cx, &parent).unwrap();
        let mut fix = WriteBatch::new(RelationId(1));
        fix.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(8)));
        db.write(&commit, fix).await.unwrap();
        db.rebuild_standing_query(&cx, &q, policy()).unwrap();
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert_eq!(
            actual(&db, &cx, &q),
            oracle(&db, GraphSetQuantifier::Distinct)
        );
        let mut next = WriteBatch::new(RelationId(1));
        next.delete_vertex(VId(1));
        db.write(&commit, next).await.unwrap();
        assert_eq!(db.standing_set_total(&cx, &child).unwrap(), &ZWeight::ONE);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn schema_ownership_and_compaction_reopen_keep_projection_contracts() {
    let ((), report) = run_async_under_lab(0x7072_6403, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let parent = db
            .register_standing_rows(&cx, definition("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        assert!(matches!(
            db.register_standing_projection(
                &cx,
                &parent,
                vec![GraphSetProjection::new("bad", GraphSetValue::Column(2))],
                GraphSetQuantifier::All,
                policy()
            ),
            Err(StandingQueryError::ProjectionSchema(_))
        ));
        let q = db
            .register_standing_projection(&cx, &parent, parity(), GraphSetQuantifier::All, policy())
            .unwrap();
        assert!(matches!(
            db.standing_projection(&cx, &parent),
            Err(StandingQueryError::Unsupported)
        ));
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(u128::MAX), vec![], vec![(P, CanonicalScalar::Int(9))]);
        db.write(&commit, seed).await.unwrap();
        let expected = actual(&db, &cx, &q);
        db.compact(&commit).await.unwrap();
        assert_eq!(actual(&db, &cx, &q), expected);
        db.rebuild_standing_query(&cx, &q, policy()).unwrap();
        drop(db);
        let mut db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert!(matches!(
            db.standing_projection(&cx, &q),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(matches!(
            db.register_standing_projection(
                &cx,
                &parent,
                parity(),
                GraphSetQuantifier::All,
                policy()
            ),
            Err(StandingQueryError::ForeignHandle)
        ));
        let parent = db
            .register_standing_rows(&cx, definition("MATCH (n) RETURN n.p AS p"), policy())
            .unwrap();
        let q = db
            .register_standing_projection(&cx, &parent, parity(), GraphSetQuantifier::All, policy())
            .unwrap();
        assert_eq!(actual(&db, &cx, &q), expected);
        assert!(matches!(
            db.register_standing_projection(
                &cx,
                &parent,
                parity(),
                GraphSetQuantifier::All,
                GqlQueryPolicy::new(0, 100, 10_000, 10_000)
            ),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::SnapshotBudget
            ))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn changed_tuple_work_does_not_visit_unrelated_parent_rows() {
    let ((), report) = run_async_under_lab(0x7072_6404, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut observed = Vec::new();
        for size in [8, 1024] {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut seed = WriteBatch::new(RelationId(1));
            for id in 0..size {
                seed.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
            }
            db.write(&commit, seed).await.unwrap();
            let parent = db
                .register_standing_rows(&cx, definition("MATCH (n) RETURN n.p AS p"), policy())
                .unwrap();
            let q = db
                .register_standing_projection(
                    &cx,
                    &parent,
                    parity(),
                    GraphSetQuantifier::All,
                    policy(),
                )
                .unwrap();
            let mut update = WriteBatch::new(RelationId(1));
            update.set_vertex_property(VId(0), P, Some(CanonicalScalar::Int(2048)));
            db.write(&commit, update).await.unwrap();
            let view = db.standing_projection(&cx, &q).unwrap();
            observed.push(*view.last_maintenance());
            assert!(
                db.standing_projection_delta(&cx, &q)
                    .unwrap()
                    .unwrap()
                    .rows()
                    .is_empty()
            );
            assert_eq!(view.frontier(), CommitSeq(2));
        }
        assert_eq!(observed[0], observed[1]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
