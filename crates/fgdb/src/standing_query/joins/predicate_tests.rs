//! Real Chronicle commits through the database-owned predicate-join lifecycle.
use super::*;
use crate::{DatabaseKeys, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, IntegerComparison};
use fgdb_gql::{
    GqlParameters, GraphSetColumnType, GraphSetOperand, GraphSetPredicateOp,
    GraphSymbol, GraphSymbolKind, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::collections::BTreeMap;

type Bag = BTreeMap<GraphValueRow, i128>;
const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
    RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
];
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
    db: &mut Database<V>, cx: &QueryCx,
) -> (StandingQueryHandle, StandingQueryHandle) {
    let mut register = |text| {
        let definition = PreparedGraphText::prepare(text, symbols).unwrap()
            .bind_parameters(&GqlParameters::new()).unwrap();
        db.register_standing_rows(cx, definition, policy()).unwrap()
    };
    (
        register("MATCH (n:L) RETURN n.k AS k, n.p AS p"),
        register("MATCH (n:R) RETURN n.k AS k, n.p AS p"),
    )
}
fn vertex(batch: &mut WriteBatch, id: u128, label: u64, key: i64, payload: i64) {
    batch.create_vertex(VId(id), vec![LabelId(label)], vec![
        (PropertyKeyId(1), CanonicalScalar::Int(key)),
        (PropertyKeyId(2), CanonicalScalar::Int(payload)),
    ]);
}
fn spec(keyed: bool, kind: RowJoinKind) -> RowJoinSpec {
    let types = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed {
        RowJoinSpec::new(&types, &types, &[(0, 0)])
    } else {
        RowJoinSpec::cross(&types, &types)
    }.unwrap();
    spec.with_kind(kind).with_predicate(&[
        GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1),
            comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(3),
        },
    ]).unwrap()
}
fn plain(rows: &ZSet<GraphValueRow>) -> Bag {
    rows.iter().map(|(row, count)| (row.clone(), count.to_i128().unwrap())).collect()
}
fn put(out: &mut Bag, values: Vec<GraphValue>, count: i128) {
    *out.entry(GraphValueRow::from_owned_values(values)).or_default() += count;
}
fn nulls(width: usize) -> Vec<GraphValue> {
    vec![GraphValue::Scalar(CanonicalScalar::Null); width]
}
// Independent nested-loop recomputation. No RowJoinSpec interpreter, selected
// derivative, witness arrangement or production comparison helper is called.
fn oracle(left: &Bag, right: &Bag, keyed: bool, kind: RowJoinKind) -> Bag {
    let matches = |l: &GraphValueRow, r: &GraphValueRow| {
        let l = l.values();
        let r = r.values();
        (!keyed || (!matches!(l[0], GraphValue::Scalar(CanonicalScalar::Null))
            && !matches!(r[0], GraphValue::Scalar(CanonicalScalar::Null)) && l[0] == r[0]))
            && matches!(
                (&l[1], &r[1]),
                (GraphValue::Scalar(CanonicalScalar::Int(a)),
                 GraphValue::Scalar(CanonicalScalar::Int(b))) if a < b
            )
    };
    let mut out = Bag::new();
    for (l, &lc) in left {
        let mut found = false;
        for (r, &rc) in right {
            if matches(l, r) {
                found = true;
                if !matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti) {
                    put(&mut out, l.values().iter().chain(r.values()).cloned().collect(), lc * rc);
                }
            }
        }
        match kind {
            RowJoinKind::Semi if found => put(&mut out, l.values().to_vec(), lc),
            RowJoinKind::Anti if !found => put(&mut out, l.values().to_vec(), lc),
            RowJoinKind::Left | RowJoinKind::Full if !found => {
                put(&mut out, [l.values().to_vec(), nulls(2)].concat(), lc);
            }
            _ => {}
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for (r, &rc) in right {
            if !left.keys().any(|l| matches(l, r)) {
                put(&mut out, [nulls(2), r.values().to_vec()].concat(), rc);
            }
        }
    }
    out
}
fn difference(after: &Bag, before: &Bag) -> Bag {
    let mut out = after.clone();
    for (row, count) in before {
        *out.entry(row.clone()).or_default() -= count;
    }
    out.retain(|_, count| *count != 0);
    out
}

#[test]
fn all_six_keyed_and_theta_views_follow_real_commits_and_rebuild_exact_definitions() {
    let ((), report) = run_async_under_lab(0x6f6e_0401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0x91)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, label, key, payload) in [
            (1, 1, 1, 10), (2, 1, 1, 10), (3, 1, 1, 30),
            (4, 2, 1, 20), (5, 2, 1, 20), (6, 2, 2, 40),
        ] {
            vertex(&mut seed, id, label, key, payload);
        }
        db.write(&commit, seed).await.unwrap();
        let (left, right) = sources(&mut db, &cx);
        let mut views = Vec::new();
        for keyed in [true, false] {
            for kind in KINDS {
                let definition = spec(keyed, kind);
                let handle = db.register_standing_join_spec(
                    &cx, &left, &right, &definition, policy(),
                ).unwrap();
                let expected = oracle(
                    &plain(db.standing_rows(&cx, &left).unwrap().rows()),
                    &plain(db.standing_rows(&cx, &right).unwrap().rows()), keyed, kind,
                );
                assert_eq!(plain(db.standing_join(&cx, &handle).unwrap().rows()), expected);
                assert_eq!(db.standing_join_spec(&cx, &handle).unwrap(), &definition);
                views.push((handle, keyed, kind, definition, expected));
            }
        }
        for tick in 0..5 {
            let mut change = WriteBatch::new(RelationId(1));
            match tick {
                0 => {
                    change.set_vertex_property(VId(1), PropertyKeyId(2), Some(CanonicalScalar::Int(35)));
                    change.set_vertex_property(VId(4), PropertyKeyId(2), Some(CanonicalScalar::Int(40)));
                }
                1 => { change.set_vertex_property(VId(5), PropertyKeyId(2), None); }
                2 => { change.delete_vertex(VId(2)); change.delete_vertex(VId(6)); }
                3 => { vertex(&mut change, 7, 1, 2, 5); vertex(&mut change, 8, 2, 2, 15); }
                _ => { change.set_vertex_property(VId(4), PropertyKeyId(1), None); }
            }
            let at = db.write(&commit, change).await.unwrap();
            let l = plain(db.standing_rows(&cx, &left).unwrap().rows());
            let r = plain(db.standing_rows(&cx, &right).unwrap().rows());
            for (handle, keyed, kind, _, before) in &mut views {
                let expected = oracle(&l, &r, *keyed, *kind);
                let view = db.standing_join(&cx, handle).unwrap();
                assert_eq!(view.frontier(), at);
                assert_eq!(plain(view.rows()), expected);
                assert_eq!(
                    plain(db.standing_join_delta(&cx, handle).unwrap().unwrap().rows()),
                    difference(&expected, before),
                );
                *before = expected;
            }
        }
        for (handle, _, _, definition, expected) in views {
            db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
            assert_eq!(db.standing_join_spec(&cx, &handle).unwrap(), &definition);
            assert_eq!(plain(db.standing_join(&cx, &handle).unwrap().rows()), expected);
            assert!(db.standing_join_delta(&cx, &handle).unwrap().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn quota_failure_fences_dependents_but_rebuild_preserves_on_not_just_equality_keys() {
    let ((), report) = run_async_under_lab(0x6f6e_0402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0x92)).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        vertex(&mut seed, 1, 1, 1, 10);
        vertex(&mut seed, 2, 1, 1, 30);
        vertex(&mut seed, 3, 2, 1, 20);
        db.write(&commit, seed).await.unwrap();
        let (left, right) = sources(&mut db, &cx);
        let definition = spec(true, RowJoinKind::Full);
        let limited = db.register_standing_join_spec(
            &cx, &left, &right, &definition, GqlQueryPolicy::new(1000, 2, 1_000_000, 1_000_000),
        ).unwrap();
        let sibling = db.register_standing_join_spec(&cx, &left, &right, &definition, policy()).unwrap();
        let child = db.register_standing_cross_join(&cx, &limited, &right, policy()).unwrap();
        let old = plain(db.standing_join(&cx, &limited).unwrap().rows());
        let mut change = WriteBatch::new(RelationId(1));
        vertex(&mut change, 4, 2, 1, 40);
        let at = db.write(&commit, change).await.unwrap();
        assert!(matches!(db.standing_join(&cx, &limited), Err(StandingQueryError::Unavailable {
            reason: StandingQueryFailure::ResultBudget, ..
        })));
        assert!(matches!(db.standing_join(&cx, &child), Err(StandingQueryError::Unavailable {
            reason: StandingQueryFailure::DependencyUnavailable, ..
        })));
        let StandingQuery::Join(retained) = &db.standing_queries[limited.index] else { panic!() };
        assert_eq!(plain(retained.rows()), old);
        assert_eq!(retained.spec(), &definition);
        assert!(db.rebuild_standing_query(
            &cx, &limited, GqlQueryPolicy::new(1000, 2, 1_000_000, 1_000_000),
        ).is_err());
        assert_eq!(db.standing_join(&cx, &sibling).unwrap().frontier(), at);
        db.rebuild_standing_query(&cx, &limited, policy()).unwrap();
        db.rebuild_standing_query(&cx, &child, policy()).unwrap();
        assert_eq!(db.standing_join_spec(&cx, &limited).unwrap(), &definition);
        assert_eq!(db.standing_join_total(&cx, &limited).unwrap(), &ZWeight::from_i128(3));
        assert_eq!(db.standing_join_total(&cx, &child).unwrap(), &ZWeight::from_i128(6));
        assert_eq!(
            plain(db.standing_join(&cx, &limited).unwrap().rows()),
            plain(db.standing_join(&cx, &sibling).unwrap().rows()),
        );
        // Continue ordinary maintenance after repair, not only a one-off snapshot.
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(4), PropertyKeyId(2), Some(CanonicalScalar::Int(5)));
        db.write(&commit, change).await.unwrap();
        assert_eq!(db.standing_join_total(&cx, &limited).unwrap(), &ZWeight::from_i128(3));
        assert_eq!(db.standing_join_total(&cx, &child).unwrap(), &ZWeight::from_i128(6));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_schema_and_owner_checks_precede_empty_results_and_never_append_failed_views() {
    let ((), report) = run_async_under_lab(0x6f6e_0403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0x93)).await.unwrap();
        let (left, right) = sources(&mut db, &cx);
        let before = db.standing_queries.len();
        for side in 0..2 {
            let mut types = [[GraphSetColumnType::Scalar; 2]; 2];
            types[side][1] = GraphSetColumnType::Any;
            let definition = RowJoinSpec::cross(&types[0], &types[1]).unwrap()
                .with_kind(RowJoinKind::Semi)
                .with_predicate(&[GraphSetPredicateOp::Truth(Some(false))]).unwrap();
            assert!(matches!(
                db.register_standing_join_spec(&cx, &left, &right, &definition, policy()),
                Err(StandingQueryError::JoinInputSchema { side: actual }) if actual == side
            ));
            assert_eq!(db.standing_queries.len(), before);
            let mut other = Database::open_memory(&commit, keys(0x94)).await.unwrap();
            assert!(matches!(
                other.register_standing_join_spec(&cx, &left, &right, &definition, policy()),
                Err(StandingQueryError::ForeignHandle)
            ));
            assert!(other.standing_queries.is_empty());
        }
        let definition = spec(false, RowJoinKind::Anti);
        for denied in [GqlQueryPolicy::new(1000, 1000, 0, 1000), GqlQueryPolicy::new(1000, 1000, 1000, 0)] {
            assert!(db.register_standing_join_spec(&cx, &left, &right, &definition, denied).is_err());
            assert_eq!(db.standing_queries.len(), before);
        }
        let good = db.register_standing_join_spec(&cx, &left, &right, &definition, policy()).unwrap();
        assert!(db.standing_join(&cx, &good).unwrap().rows().is_empty());
        assert_eq!(db.standing_join_spec(&cx, &good).unwrap(), &definition);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
