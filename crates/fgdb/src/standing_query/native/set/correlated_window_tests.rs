//! Maintained invocation-local pages through public text binding and delivery.
//! Literal expected bags verify promotion, optional completion, and field deltas.

use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalList, CanonicalPropertyValue, CanonicalScalar, DatabaseSecurityNamespaceId, EId,
    PurposeContexts,
};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const LIMBS: LimbLimit = LimbLimit::new(4);
type Bag = BTreeMap<GraphValueRow, i128>;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 50_000_000)
}
fn keys(tag: u8) -> DatabaseKeys {
    DatabaseKeys::new([tag; 32], DatabaseSecurityNamespaceId([tag; 32]), [tag; 32])
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn text(optional: bool, distinct: bool, predicate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH a,token {}CALL (a) {{ MATCH (a)-[:R]->(b) {predicate} RETURN {}b AS candidate,b.p AS score ORDER BY score DESC,candidate ASC SKIP 1 LIMIT 1 }} RETURN a AS owner,candidate,score",
        if optional { "OPTIONAL " } else { "" },
        if distinct { "DISTINCT " } else { "" }
    )
}
fn bind(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}
fn cells(values: Vec<QueryValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(
        values
            .into_iter()
            .map(|value| match value {
                QueryValue::Value(value) => value,
                _ => panic!("native row values remain typed"),
            })
            .collect(),
    )
}
fn native_bag(rows: &ZSet<Vec<QueryValue>>) -> Bag {
    rows.iter()
        .map(|(row, n)| (cells(row.clone()), n.to_i128().unwrap()))
        .collect()
}
fn occurrences(rows: impl IntoIterator<Item = GraphValueRow>) -> Bag {
    let mut output = Bag::new();
    for row in rows {
        *output.entry(row).or_default() += 1;
    }
    output
}
fn expected(owners: &[(u128, Option<(u128, i64)>)], optional: bool) -> Bag {
    owners
        .iter()
        .filter_map(|&(owner, selected)| {
            if !optional && selected.is_none() {
                return None;
            }
            let (candidate, score) = selected.map_or(
                (
                    GraphValue::Scalar(CanonicalScalar::Null),
                    GraphValue::Scalar(CanonicalScalar::Null),
                ),
                |(candidate, score)| {
                    (
                        GraphValue::Vertex(VId(candidate)),
                        GraphValue::Scalar(CanonicalScalar::Int(score)),
                    )
                },
            );
            Some((
                GraphValueRow::from_owned_values(vec![
                    GraphValue::Vertex(VId(owner)),
                    candidate,
                    score,
                ]),
                2,
            ))
        })
        .collect()
}
fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    query: &PreparedGraphSet,
    handle: &StandingQueryHandle,
    expected: &Bag,
    delivery: GqlQueryPolicy,
) {
    assert_eq!(
        occurrences(
            db.execute_graph_set_governed(cx, query, policy())
                .unwrap()
                .value
        ),
        *expected
    );
    let (at, rows) = db.standing_native_bag(cx, handle, delivery).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&rows), *expected);
    let QueryResult::Rows { columns, rows } =
        db.standing_native_query(cx, handle, delivery).unwrap().1
    else {
        panic!("a maintained invocation returns rows");
    };
    assert_eq!(columns.as_slice(), query.columns());
    assert_eq!(occurrences(rows.into_iter().map(cells)), *expected);
}
fn check_delta(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    from: CommitSeq,
    before: &Bag,
    after: &Bag,
) {
    let mut expected = after.clone();
    for (row, n) in before {
        *expected.entry(row.clone()).or_default() -= n;
    }
    expected.retain(|_, n| *n != 0);
    let (at, delta) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&delta), expected);
    assert_eq!(
        db.standing_native_delta(cx, handle, from, policy())
            .unwrap()
            .1,
        delta
    );
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in [1, 2, 3] {
        batch.create_vertex(VId(id), vec![L], vec![]);
    }
    for (id, score) in [(10, 9), (11, 8), (12, 7)] {
        batch.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(score))]);
    }
    for (id, a, b) in [
        (101, 1, 10),
        (102, 1, 10),
        (103, 1, 11),
        (104, 1, 12),
        (201, 2, 11),
        (202, 2, 12),
    ] {
        batch.add_edge(EId(id), VId(a), VId(b), vec![]);
    }
    batch
}

#[test]
fn native_correlated_pages_refill_and_rerank_independently_with_outer_bags() {
    let ((), report) = run_async_under_lab(0x7061_6701, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0xe1)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut queries = Vec::new();
        for optional in [false, true] {
            for distinct in [false, true] {
                let source = text(optional, distinct, "WHERE b.p >= 0");
                let prepared = PreparedNativeRead::prepare(&source, &params, symbols).unwrap();
                let (nodes, width) = prepared.subscription_footprint(&cx, &params).unwrap();
                let first = db.standing_queries.len();
                let handle = prepared
                    .register_standing(&mut db, &cx, &params, policy())
                    .unwrap();
                assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
                assert!(width > 0);
                assert!(
                    db.standing_queries[first..]
                        .iter()
                        .any(|node| matches!(node, StandingQuery::Window(_)))
                );
                let query = bind(&source);
                let selected = if distinct { (11, 8) } else { (10, 9) };
                let initial = expected(
                    &[(1, Some(selected)), (2, Some((12, 7))), (3, None)],
                    optional,
                );
                check(&db, &cx, &query, &handle, &initial, policy());
                queries.push((optional, query, handle, initial));
            }
        }
        let frozen = db
            .standing_native_bag(&cx, &queries[0].2, policy())
            .unwrap();
        for phase in 0..8 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_edge(EId(101));
                }
                1 => {
                    change.delete_edge(EId(102));
                }
                2 => {
                    change.set_vertex_property(VId(12), P, Some(CanonicalScalar::Int(10)));
                }
                3 => {
                    change.add_edge(EId(301), VId(3), VId(10), vec![]);
                    change.add_edge(EId(302), VId(3), VId(11), vec![]);
                }
                4 => {
                    change.delete_vertex(VId(1));
                }
                5 => {
                    change.set_vertex_property(VId(11), P, Some(CanonicalScalar::Int(-1)));
                }
                6 => {
                    change.add_edge(EId(203), VId(2), VId(10), vec![]);
                    change.add_edge(EId(303), VId(3), VId(12), vec![]);
                }
                7 => {
                    change.set_vertex_label(VId(3), L, false);
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let selected = match phase {
                0 => vec![(1, Some((11, 8))), (2, Some((12, 7))), (3, None)],
                1 => vec![(1, Some((12, 7))), (2, Some((12, 7))), (3, None)],
                2 => vec![(1, Some((11, 8))), (2, Some((11, 8))), (3, None)],
                3 => vec![(1, Some((11, 8))), (2, Some((11, 8))), (3, Some((11, 8)))],
                4 => vec![(2, Some((11, 8))), (3, Some((11, 8)))],
                5 => vec![(2, None), (3, None)],
                6 => vec![(2, Some((10, 9))), (3, Some((10, 9)))],
                7 => vec![(2, Some((10, 9)))],
                _ => unreachable!(),
            };
            for (optional, query, handle, previous) in &mut queries {
                let next = expected(&selected, *optional);
                check(&db, &cx, query, handle, &next, policy());
                check_delta(&db, &cx, handle, from, previous, &next);
                if phase == 2 || phase == 7 {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    check(&db, &cx, query, handle, &next, policy());
                }
                *previous = next;
            }
        }
        assert_eq!(
            native_bag(&frozen.1),
            expected(&[(1, Some((10, 9))), (2, Some((12, 7))), (3, None)], false)
        );
        assert!(frozen.0 < db.frontier().unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bounded_property_pages_exclude_inactive_huge_collections_and_refuse_unsound_guards() {
    let ((), report) = run_async_under_lab(0x7061_6702, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0xe2)).await.unwrap();
        let huge = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![
                CanonicalScalar::Int(0).into();
                GraphValue::MAX_LIST_NODES
            ])
            .unwrap(),
        );
        assert!(GraphValue::from_property_value(&huge).is_none());
        let mut initial = WriteBatch::new(R);
        for id in [1, 90, 99] {
            initial.create_vertex(VId(id), vec![], vec![]);
        }
        initial.create_vertex(VId(10), vec![], vec![(P, CanonicalScalar::Int(9))]);
        initial.create_vertex(VId(11), vec![], vec![(P, CanonicalScalar::Int(8))]);
        initial.set_vertex_property_value(VId(99), P, Some(huge));
        for (id, a, b) in [(1, 1, 10), (2, 1, 11), (3, 90, 99), (4, 90, 10)] {
            initial.add_edge(EId(id), VId(a), VId(b), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let query = bind(&text(true, false, "WHERE b.p >= 0"));
        let handle = db
            .register_standing_relation(&cx, &query, policy())
            .unwrap();
        check(&db, &cx, &query, &handle, &Bag::new(), policy());
        let before = saved(&db.standing_queries);
        for predicate in [
            "",
            "WHERE b.p <> 0",
            "WHERE a.p >= 0",
            "WHERE b.q >= 0",
            "WHERE b.p >= 0 OR b.q >= 0",
            "WHERE NOT (b.p >= 0)",
        ] {
            let refused = bind(&text(true, false, predicate));
            // No owner invokes this body. A global RHS conversion must never
            // turn an ordinary empty result into StoredValueBounds.
            assert!(
                db.execute_graph_set_governed(&cx, &refused, policy())
                    .unwrap()
                    .value
                    .is_empty()
            );
            assert!(matches!(
                db.register_standing_relation(&cx, &refused, policy()),
                Err(StandingQueryError::Unsupported)
            ));
            assert_eq!(saved(&db.standing_queries), before);
        }
        let mut activate = WriteBatch::new(R);
        activate.set_vertex_label(VId(1), L, true);
        db.write(&commit, activate).await.unwrap();
        check(
            &db,
            &cx,
            &query,
            &handle,
            &expected(&[(1, Some((11, 8)))], true),
            policy(),
        );
        let mut activate = WriteBatch::new(R);
        activate.set_vertex_property(VId(99), P, Some(CanonicalScalar::Int(7)));
        activate.set_vertex_label(VId(90), L, true);
        db.write(&commit, activate).await.unwrap();
        let expected = expected(&[(1, Some((11, 8))), (90, Some((99, 7)))], true);
        check(&db, &cx, &query, &handle, &expected, policy());
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, &query, &handle, &expected, policy());
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
fn correlated_page_admission_and_rebuild_keep_prior_owners_atomic_on_every_checkpoint() {
    let ((), report) = run_async_under_lab(0x7061_6703, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys(0xe3)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        db.register_standing_native(
            &cx,
            "MATCH (n) RETURN n",
            &GqlParameters::new(),
            symbols,
            policy(),
        )
        .unwrap();
        let query = bind(&text(true, false, "WHERE b.p >= 0"));
        let delivery = GqlQueryPolicy::new(1_000, 6, 50_000_000, 50_000_000);
        let before = saved(&db.standing_queries);
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &query, delivery, &mut || {
                    calls += 1;
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(saved(&db.standing_queries), before);
        for stop in 1..=calls {
            let mut at = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(
                    staged
                        .compile(&cx, &query, delivery, &mut || {
                            at += 1;
                            if at == stop {
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
            assert_eq!(at, stop);
            assert_eq!(saved(&db.standing_queries), before);
        }
        for refused in [
            GqlQueryPolicy::new(0, 6, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 5, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 6, 0, 50_000_000),
            GqlQueryPolicy::new(1_000, 6, 50_000_000, 0),
        ] {
            assert!(db.register_standing_relation(&cx, &query, refused).is_err());
            assert_eq!(saved(&db.standing_queries), before);
        }
        let handle = db
            .register_standing_relation(&cx, &query, delivery)
            .unwrap();
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("a page retains the complete invocation circuit");
        };
        let first = *first;
        let mut calls = 0;
        rebuild_checked(
            &mut db,
            &cx,
            first,
            handle.index,
            handle.native.as_deref().unwrap().private_inputs().unwrap(),
            delivery,
            &mut || {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
        let accepted = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut at = 0;
            assert!(
                rebuild_checked(
                    &mut db,
                    &cx,
                    first,
                    handle.index,
                    handle.native.as_deref().unwrap().private_inputs().unwrap(),
                    delivery,
                    &mut || {
                        at += 1;
                        if at == stop {
                            Err(StandingQueryError::Maintenance(
                                StandingQueryFailure::Interrupted,
                            ))
                        } else {
                            Ok(())
                        }
                    }
                )
                .is_err()
            );
            assert_eq!(at, stop);
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        check(
            &db,
            &cx,
            &query,
            &handle,
            &expected(&[(1, Some((10, 9))), (2, Some((12, 7))), (3, None)], true),
            delivery,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
