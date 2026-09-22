//! Public maintained-join composition, driven by real committed graph writes.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 1_000_000, 1_000_000)
}
fn keys(tag: u8) -> DatabaseKeys {
    DatabaseKeys::new([tag; 32], DatabaseSecurityNamespaceId([tag; 32]), [tag; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Label, "R") => Some(GraphSymbol::Label(LabelId(2))),
        (GraphSymbolKind::Property, "k") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn sources<V: Vfs + Clone>(
    db: &mut Database<V>,
    cx: &QueryCx,
    filter_right: bool,
) -> (StandingQueryHandle, StandingQueryHandle) {
    let definition = |text| {
        PreparedGraphText::prepare(text, symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    };
    let left = db
        .register_standing_rows(
            cx,
            definition("MATCH (n:L) RETURN n.k AS k, n.p AS p"),
            policy(),
        )
        .unwrap();
    let right_text = if filter_right {
        "MATCH (n:R) WHERE n.k = 2 RETURN n.k AS k, n.p AS p"
    } else {
        "MATCH (n:R) RETURN n.k AS k, n.p AS p"
    };
    let right = db
        .register_standing_rows(cx, definition(right_text), policy())
        .unwrap();
    (left, right)
}
fn vertex(batch: &mut WriteBatch, id: u128, label: u64, key: i64, payload: i64) {
    batch.create_vertex(
        VId(id),
        vec![LabelId(label)],
        vec![
            (PropertyKeyId(1), CanonicalScalar::Int(key)),
            (PropertyKeyId(2), CanonicalScalar::Int(payload)),
        ],
    );
}
fn bag<const N: usize>(entries: &[([Option<i64>; N], i128)]) -> ZSet<GraphValueRow> {
    ZSet::from_updates(
        entries.iter().map(|(values, count)| {
            let values = values
                .iter()
                .map(|value| {
                    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
                })
                .collect();
            (GraphValueRow::from_owned_values(values), ZWeight::from_i128(*count))
        }),
        LIMBS,
        &mut |_| Ok::<_, StandingQueryFailure>(()),
    )
    .unwrap()
}
fn difference(after: &ZSet<GraphValueRow>, before: &ZSet<GraphValueRow>) -> ZSet<GraphValueRow> {
    after
        .minus(before, LIMBS, &mut |_| Ok::<_, StandingQueryFailure>(()))
        .unwrap()
}

#[test]
fn right_and_full_equijoins_publish_exact_deltas_to_downstream_joins_and_rebuild() {
    let ((), report) = run_async_under_lab(0x726a_0301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0x81)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        vertex(&mut seed, 1, 1, 1, 10);
        vertex(&mut seed, 2, 2, 2, 20);
        db.write(&commit, seed).await.unwrap();
        let (left, right) = sources(&mut db, &cx, false);
        let right_join = db
            .register_standing_join_with_kind(
                &cx, &left, &right, &[(0, 0)], RowJoinKind::Right, policy(),
            )
            .unwrap();
        let full = db
            .register_standing_join_with_kind(
                &cx, &left, &right, &[(0, 0)], RowJoinKind::Full, policy(),
            )
            .unwrap();
        let child = db
            .register_standing_join(&cx, &full, &right, &[(2, 0)], policy())
            .unwrap();
        let before_right = bag(&[([None, None, Some(2), Some(20)], 1)]);
        let before_full = bag(&[
            ([Some(1), Some(10), None, None], 1),
            ([None, None, Some(2), Some(20)], 1),
        ]);
        let before_child = bag(&[([None, None, Some(2), Some(20), Some(2), Some(20)], 1)]);
        for (handle, expected) in [
            (&right_join, &before_right), (&full, &before_full), (&child, &before_child),
        ] {
            assert_eq!(db.standing_join(&cx, handle).unwrap().rows(), expected);
            assert!(db.standing_join_delta(&cx, handle).unwrap().is_none());
        }
        assert_eq!(
            db.standing_join_columns(&cx, &full).unwrap(),
            &["left.k", "left.p", "right.k", "right.p"]
        );
        // Both parent derivatives belong to this one committed tick.
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(2)));
        change.set_vertex_property(VId(2), PropertyKeyId(2), Some(CanonicalScalar::Int(21)));
        let at = db.write(&commit, change).await.unwrap();
        let after = bag(&[([Some(2), Some(10), Some(2), Some(21)], 1)]);
        let after_child = bag(&[([Some(2), Some(10), Some(2), Some(21), Some(2), Some(21)], 1)]);
        for (handle, before, expected) in [
            (&right_join, &before_right, &after),
            (&full, &before_full, &after),
            (&child, &before_child, &after_child),
        ] {
            let view = db.standing_join(&cx, handle).unwrap();
            assert_eq!(view.frontier, at);
            assert_eq!(view.rows(), expected);
            assert_eq!(
                db.standing_join_delta(&cx, handle).unwrap().unwrap().rows(),
                &difference(expected, before)
            );
            assert_eq!(db.standing_join_total(&cx, handle).unwrap(), &ZWeight::from_i128(1));
        }
        for handle in [&right_join, &full, &child] {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert!(db.standing_join_delta(&cx, handle).unwrap().is_none());
        }
        assert_eq!(db.standing_join_kind(&cx, &right_join).unwrap(), RowJoinKind::Right);
        assert_eq!(db.standing_join_kind(&cx, &full).unwrap(), RowJoinKind::Full);
        assert_eq!(db.standing_join(&cx, &child).unwrap().rows(), &after_child);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_cross_modes_follow_empty_witness_transitions_without_losing_bag_counts() {
    let ((), report) = run_async_under_lab(0x726a_0302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0x82)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        vertex(&mut seed, 1, 1, 1, 10);
        vertex(&mut seed, 2, 1, 1, 10);
        db.write(&commit, seed).await.unwrap();
        let (left, right) = sources(&mut db, &cx, true);
        let kinds = [
            RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
            RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
        ];
        let mut handles = Vec::new();
        let mut baselines = Vec::new();
        for kind in kinds {
            let handle = db
                .register_standing_cross_join_with_kind(&cx, &left, &right, kind, policy())
                .unwrap();
            let baseline = match kind {
                RowJoinKind::Inner | RowJoinKind::Right | RowJoinKind::Semi => ZSet::new(),
                RowJoinKind::Left | RowJoinKind::Full => {
                    bag(&[([Some(1), Some(10), None, None], 2)])
                }
                RowJoinKind::Anti => bag(&[([Some(1), Some(10)], 2)]),
            };
            assert_eq!(db.standing_join(&cx, &handle).unwrap().rows(), &baseline);
            let width = if matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti) { 2 } else { 4 };
            assert_eq!(db.standing_join_columns(&cx, &handle).unwrap().len(), width);
            handles.push(handle);
            baselines.push(baseline);
        }
        let default = db.register_standing_cross_join(&cx, &left, &right, policy()).unwrap();
        assert_eq!(db.standing_join_kind(&cx, &default).unwrap(), RowJoinKind::Inner);
        assert!(matches!(
            db.register_standing_join_with_kind(&cx, &left, &right, &[], RowJoinKind::Full, policy()),
            Err(StandingQueryError::JoinSchema(RowJoinBuildError::EmptyKeys))
        ));
        let mut other = Database::open_memory(&commit, keys(0x83)).await.unwrap();
        assert!(matches!(
            other.register_standing_cross_join_with_kind(&cx, &left, &right, RowJoinKind::Full, policy()),
            Err(StandingQueryError::ForeignHandle)
        ));
        assert!(other.standing_queries.is_empty());
        let mut insert = WriteBatch::new(RelationId(1));
        vertex(&mut insert, 3, 2, 2, 20);
        vertex(&mut insert, 4, 2, 2, 20);
        let at = db.write(&commit, insert).await.unwrap();
        let mut populated = Vec::new();
        for (index, kind) in kinds.into_iter().enumerate() {
            let expected = match kind {
                RowJoinKind::Semi => bag(&[([Some(1), Some(10)], 2)]),
                RowJoinKind::Anti => ZSet::new(),
                _ => bag(&[([Some(1), Some(10), Some(2), Some(20)], 4)]),
            };
            let handle = &handles[index];
            let view = db.standing_join(&cx, handle).unwrap();
            assert_eq!(view.frontier, at);
            assert_eq!(view.rows(), &expected);
            assert_eq!(
                db.standing_join_delta(&cx, handle).unwrap().unwrap().rows(),
                &difference(&expected, &baselines[index])
            );
            populated.push(expected);
        }
        // Remove both right witnesses through the base query's predicate.
        let mut remove = WriteBatch::new(RelationId(1));
        for id in [3, 4] {
            remove.set_vertex_property(VId(id), PropertyKeyId(1), Some(CanonicalScalar::Int(3)));
        }
        db.write(&commit, remove).await.unwrap();
        for (index, handle) in handles.iter().enumerate() {
            assert_eq!(db.standing_join(&cx, handle).unwrap().rows(), &baselines[index]);
            assert_eq!(
                db.standing_join_delta(&cx, handle).unwrap().unwrap().rows(),
                &difference(&baselines[index], &populated[index])
            );
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            assert_eq!(db.standing_join_kind(&cx, handle).unwrap(), kinds[index]);
            assert_eq!(db.standing_join(&cx, handle).unwrap().rows(), &baselines[index]);
            assert!(db.standing_join_delta(&cx, handle).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn full_join_quota_failure_fences_only_the_view_and_rebuild_restores_current_state() {
    let ((), report) = run_async_under_lab(0x726a_0303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0x84)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        vertex(&mut seed, 1, 1, 1, 10);
        db.write(&commit, seed).await.unwrap();
        let (left, right) = sources(&mut db, &cx, false);
        let limited = db
            .register_standing_join_with_kind(
                &cx, &left, &right, &[(0, 0)], RowJoinKind::Full,
                GqlQueryPolicy::new(1000, 1, 1_000_000, 1_000_000),
            )
            .unwrap();
        let healthy = db
            .register_standing_join_with_kind(
                &cx, &left, &right, &[(0, 0)], RowJoinKind::Full, policy(),
            )
            .unwrap();
        let mut insert = WriteBatch::new(RelationId(1));
        vertex(&mut insert, 2, 2, 1, 20);
        vertex(&mut insert, 3, 2, 1, 21);
        let at = db.write(&commit, insert).await.unwrap();
        assert_eq!(db.frontier().unwrap(), at);
        assert!(db.standing_join(&cx, &limited).is_err());
        assert!(db.standing_join_delta(&cx, &limited).is_err());
        let StandingQuery::Join(state) = &db.standing_queries[limited.index] else {
            panic!("registered join changed kind");
        };
        assert_eq!(state.failure, Some(StandingQueryFailure::ResultBudget));
        let expected = bag(&[
            ([Some(1), Some(10), Some(1), Some(20)], 1),
            ([Some(1), Some(10), Some(1), Some(21)], 1),
        ]);
        assert_eq!(db.standing_join(&cx, &healthy).unwrap().rows(), &expected);
        db.rebuild_standing_query(&cx, &limited, policy()).unwrap();
        assert_eq!(db.standing_join_kind(&cx, &limited).unwrap(), RowJoinKind::Full);
        assert_eq!(db.standing_join(&cx, &limited).unwrap().rows(), &expected);
        assert!(db.standing_join_delta(&cx, &limited).unwrap().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
