//! Per-invocation DISTINCT retains its support independently of the outer bag.
//! Literal successor bags cover last witnesses, shared/NULL imports and atomic admission.

use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText, PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const M: LabelId = LabelId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const CALL: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL { WITH a MATCH (a)-[r:R]->(b:M) RETURN DISTINCT b AS child } ",
    "RETURN token,a,child"
);
const SCOPED_CALL: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[r:R]->(b:M) WITH DISTINCT b RETURN DISTINCT b AS child } ",
    "RETURN token,a,child"
);
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
        (GraphSymbolKind::Label, "M") => Some(GraphSymbol::Label(M)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn bind(text: &str) -> PreparedGraphSet {
    PreparedGraphSetText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
}

fn leaf(text: &str) -> PreparedGraphSet {
    PreparedGraphText::prepare(text, symbols)
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap()
        .with_duplicates()
        .into()
}

fn scalar(value: CanonicalScalar) -> GraphValue {
    GraphValue::Scalar(value)
}

fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
}

fn call_bag(pairs: &[(u128, u128)]) -> Bag {
    pairs
        .iter()
        .map(|&(owner, child)| {
            (
                row(vec![
                    scalar(CanonicalScalar::Int(7)),
                    GraphValue::Vertex(VId(owner)),
                    GraphValue::Vertex(VId(child)),
                ]),
                2,
            )
        })
        .collect()
}

fn occurrences(rows: impl IntoIterator<Item = GraphValueRow>) -> Bag {
    let mut result = Bag::new();
    for row in rows {
        *result.entry(row).or_default() += 1;
    }
    result
}

fn cells(values: Vec<QueryValue>) -> GraphValueRow {
    row(values
        .into_iter()
        .map(|value| match value {
            QueryValue::Value(value) => value,
            _ => panic!("maintained relational output retains its value cells"),
        })
        .collect())
}

fn native_bag(rows: &ZSet<Vec<QueryValue>>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (cells(row.clone()), weight.to_i128().unwrap()))
        .collect()
}

fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    definition: &PreparedGraphSet,
    handle: &StandingQueryHandle,
    expected: &Bag,
    delivery: GqlQueryPolicy,
) {
    assert_eq!(
        occurrences(
            db.execute_graph_set_governed(cx, definition, policy())
                .unwrap()
                .value
        ),
        *expected
    );
    let (at, rows) = db.standing_native_bag(cx, handle, delivery).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&rows), *expected);
    assert_eq!(
        db.standing_native_columns(cx, handle).unwrap(),
        definition.columns()
    );
    let QueryResult::Rows { columns, rows } =
        db.standing_native_query(cx, handle, delivery).unwrap().1
    else {
        panic!("native DISTINCT Apply returns rows");
    };
    assert_eq!(columns.as_slice(), definition.columns());
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
    for (row, weight) in before {
        *expected.entry(row.clone()).or_default() -= weight;
    }
    expected.retain(|_, weight| *weight != 0);
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
    for (id, labels) in [
        (1, vec![L]),
        (2, vec![M]),
        (3, vec![L]),
        (4, vec![M]),
        (5, vec![L]),
        (6, vec![]),
    ] {
        batch.create_vertex(VId(id), labels, vec![]);
    }
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    batch.add_edge(EId(12), VId(1), VId(2), vec![]);
    batch.add_edge(EId(13), VId(3), VId(2), vec![]);
    batch.add_edge(EId(14), VId(5), VId(6), vec![]);
    batch
}

#[test]
fn native_distinct_calls_preserve_duplicate_outers_and_retract_only_last_witnesses() {
    let ((), report) = run_async_under_lab(0x6473_7001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd1)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let params = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [CALL, SCOPED_CALL] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
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
                    .filter(|query| {
                        matches!(query, StandingQuery::Projection(state)
                    if state.spec().quantifier() == GraphSetQuantifier::Distinct)
                    })
                    .count()
                    >= 2
            );
            queries.push((text, bind(text), handle));
        }
        let mut expected = call_bag(&[(1, 2), (3, 2)]);
        let frozen = db
            .standing_native_bag(&cx, &queries[0].2, policy())
            .unwrap();
        for (text, query, handle) in &queries {
            check(&db, &cx, query, handle, &expected, policy());
            let QueryResult::Rows { rows, .. } =
                db.query(&cx, text, &params, symbols, policy()).unwrap()
            else {
                panic!("CALL returns rows");
            };
            assert_eq!(occurrences(rows.into_iter().map(cells)), expected);
        }
        for phase in 0..9 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(9)));
                }
                1 => {
                    change.delete_edge(EId(11));
                }
                2 => {
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.add_edge(EId(15), VId(1), VId(4), vec![]);
                    change.add_edge(EId(16), VId(1), VId(4), vec![]);
                }
                4 => {
                    change.set_vertex_label(VId(2), M, false);
                    change.add_edge(EId(17), VId(3), VId(4), vec![]);
                }
                5 => {
                    change.delete_vertex(VId(4));
                }
                6 => {
                    change.set_vertex_label(VId(1), L, false);
                    change.set_vertex_label(VId(6), M, true);
                }
                7 => {
                    change.set_vertex_label(VId(1), L, true);
                    change.add_edge(EId(18), VId(1), VId(6), vec![]);
                }
                8 => {
                    change.delete_vertex(VId(5));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 | 1 => call_bag(&[(1, 2), (3, 2)]),
                2 => call_bag(&[(3, 2)]),
                3 => call_bag(&[(1, 4), (3, 2)]),
                4 => call_bag(&[(1, 4), (3, 4)]),
                5 => Bag::new(),
                6 => call_bag(&[(5, 6)]),
                7 => call_bag(&[(1, 6), (5, 6)]),
                8 => call_bag(&[(1, 6)]),
                _ => unreachable!(),
            };
            for (_, query, handle) in &queries {
                check(&db, &cx, query, handle, &next, policy());
                check_delta(&db, &cx, handle, from, &expected, &next);
                if phase == 3 || phase == 8 {
                    db.rebuild_standing_query(&cx, handle, policy()).unwrap();
                    check(&db, &cx, query, handle, &next, policy());
                }
            }
            expected = next;
        }
        assert_eq!(native_bag(&frozen.1), call_bag(&[(1, 2), (3, 2)]));
        assert!(frozen.0 < db.frontier().unwrap());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn optional_definition() -> PreparedGraphSet {
    let left = leaf("MATCH (a:L) RETURN a AS owner,a.p AS wanted");
    let left = left
        .clone()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, left)
        .unwrap();
    let arguments = PreparedGraphSet::arguments(
        vec!["first".into(), "second".into()],
        vec![GraphSetColumnType::Stored, GraphSetColumnType::Stored],
    )
    .unwrap()
    .project(
        vec![
            GraphSetProjection::new("second", GraphSetValue::Column(1)),
            GraphSetProjection::new("first", GraphSetValue::Column(0)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap();
    let body = arguments
        .cross_join(leaf("MATCH (b:M) RETURN b"))
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("copied", GraphSetValue::Column(0)),
                GraphSetProjection::new(
                    "literal",
                    GraphSetValue::Value(scalar(CanonicalScalar::Int(7))),
                ),
                GraphSetProjection::new(
                    "actual_null",
                    GraphSetValue::Value(scalar(CanonicalScalar::Null)),
                ),
            ],
            GraphSetQuantifier::Distinct,
        )
        .unwrap();
    left.optional_apply(body, vec![1, 1]).unwrap()
}

fn optional_bag(owners: &[(u128, CanonicalScalar)], matched: bool) -> Bag {
    owners
        .iter()
        .map(|(owner, imported)| {
            (
                row(vec![
                    GraphValue::Vertex(VId(*owner)),
                    scalar(imported.clone()),
                    scalar(if matched {
                        imported.clone()
                    } else {
                        CanonicalScalar::Null
                    }),
                    scalar(if matched {
                        CanonicalScalar::Int(7)
                    } else {
                        CanonicalScalar::Null
                    }),
                    scalar(CanonicalScalar::Null),
                ]),
                2,
            )
        })
        .collect()
}

#[test]
fn optional_distinct_keys_null_and_false_imports_and_shares_repeated_arguments() {
    let ((), report) = run_async_under_lab(0x6473_7002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd2)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        initial.create_vertex(VId(2), vec![L], vec![]);
        initial.create_vertex(VId(3), vec![L], vec![(P, CanonicalScalar::Bool(false))]);
        initial.create_vertex(VId(10), vec![M], vec![]);
        initial.create_vertex(VId(11), vec![M], vec![]);
        db.write(&commit, initial).await.unwrap();
        let query = optional_definition();
        let handle = db
            .register_standing_relation(&cx, &query, policy())
            .unwrap();
        let mut expected = optional_bag(
            &[
                (1, CanonicalScalar::Null),
                (2, CanonicalScalar::Null),
                (3, CanonicalScalar::Bool(false)),
            ],
            true,
        );
        check(&db, &cx, &query, &handle, &expected, policy());
        for phase in 0..6 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_vertex(VId(10));
                }
                1 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Bool(false)));
                }
                2 => {
                    change.delete_vertex(VId(1));
                    change.delete_vertex(VId(11));
                }
                3 => {
                    change.create_vertex(VId(12), vec![M], vec![]);
                    change.create_vertex(VId(4), vec![L], vec![]);
                }
                4 => {
                    change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(0)));
                }
                5 => {
                    change.delete_vertex(VId(2));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => expected.clone(),
                1 => optional_bag(
                    &[
                        (1, CanonicalScalar::Null),
                        (2, CanonicalScalar::Bool(false)),
                        (3, CanonicalScalar::Bool(false)),
                    ],
                    true,
                ),
                2 => optional_bag(
                    &[
                        (2, CanonicalScalar::Bool(false)),
                        (3, CanonicalScalar::Bool(false)),
                    ],
                    false,
                ),
                3 => optional_bag(
                    &[
                        (2, CanonicalScalar::Bool(false)),
                        (3, CanonicalScalar::Bool(false)),
                        (4, CanonicalScalar::Null),
                    ],
                    true,
                ),
                4 => optional_bag(
                    &[
                        (2, CanonicalScalar::Bool(false)),
                        (3, CanonicalScalar::Int(0)),
                        (4, CanonicalScalar::Null),
                    ],
                    true,
                ),
                5 => optional_bag(
                    &[(3, CanonicalScalar::Int(0)), (4, CanonicalScalar::Null)],
                    true,
                ),
                _ => unreachable!(),
            };
            check(&db, &cx, &query, &handle, &next, policy());
            check_delta(&db, &cx, &handle, from, &expected, &next);
            expected = next;
        }
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
fn distinct_private_witnesses_respect_delivery_limits_and_all_failed_admissions_are_atomic() {
    let ((), report) = run_async_under_lab(0x6473_7003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd3)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        initial.create_vertex(VId(2), vec![M], vec![]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let query = bind(CALL);
        let two = GqlQueryPolicy::new(1_000, 2, 50_000_000, 50_000_000);
        let before = saved(&db.standing_queries);
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile(&cx, &query, two, &mut || {
                    calls += 1;
                    Ok(())
                })
                .unwrap();
        }
        assert!(calls > 0);
        assert_eq!(saved(&db.standing_queries), before);
        for stop in 1..=calls {
            let mut seen = 0;
            {
                let mut staged = Staging::new(&mut db);
                assert!(
                    staged
                        .compile(&cx, &query, two, &mut || {
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
        for refused in [
            GqlQueryPolicy::new(0, 2, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 0, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 50_000_000, 0),
        ] {
            assert!(db.register_standing_relation(&cx, &query, refused).is_err());
            assert_eq!(saved(&db.standing_queries), before);
        }
        // Structural roles must survive changing an initially unbounded
        // allowance; a numeric max_rows=None cannot identify private nodes.
        let unlimited = GqlQueryPolicy {
            rows: fgdb_gql::GqlExecutionBudget::snapshot_records(1_000),
            evaluator: two.evaluator,
        };
        let handle = db
            .register_standing_relation(&cx, &query, unlimited)
            .unwrap();
        db.rebuild_standing_query(&cx, &handle, two).unwrap();
        let expected = call_bag(&[(1, 2)]);
        check(&db, &cx, &query, &handle, &expected, two);
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("DISTINCT Apply owns a complete circuit");
        };
        let first = *first;
        let mut calls = 0;
        rebuild_checked(
            &mut db,
            &cx,
            first,
            handle.index,
            handle.native.as_deref().unwrap().private_inputs().unwrap(),
            two,
            &mut || {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
        let accepted = saved(&db.standing_queries);
        for stop in 1..=calls {
            let mut seen = 0;
            assert!(
                rebuild_checked(
                    &mut db,
                    &cx,
                    first,
                    handle.index,
                    handle.native.as_deref().unwrap().private_inputs().unwrap(),
                    two,
                    &mut || {
                        seen += 1;
                        if seen == stop {
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
            assert_eq!(seen, stop);
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        for refused in [
            GqlQueryPolicy::new(0, 2, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 0, 50_000_000),
            GqlQueryPolicy::new(1_000, 2, 50_000_000, 0),
        ] {
            assert!(db.rebuild_standing_query(&cx, &handle, refused).is_err());
            assert_eq!(saved(&db.standing_queries), accepted);
        }
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(1000), VId(1), VId(2), vec![]);
        db.write(&commit, change).await.unwrap();
        check(&db, &cx, &query, &handle, &expected, two);
        check_delta(&db, &cx, &handle, from, &expected, &expected);
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(2));
        db.write(&commit, change).await.unwrap();
        check(&db, &cx, &query, &handle, &Bag::new(), two);
        check_delta(&db, &cx, &handle, from, &expected, &Bag::new());
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
