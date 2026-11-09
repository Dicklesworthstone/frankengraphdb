//! Maintained correlated Apply uses the existing graph, join and projection owners.
//! Expected bags and successor differences below are literal fixture outcomes.

use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::{GraphValue, IntegerComparison};
use fgdb_gql::{
    GqlParameters, GqlScalarParameter, GraphIntegerBinary, GraphIntegerExpression, GraphIntegerOp,
    GraphSetOperand, GraphSetPredicateOp, GraphSymbol, GraphSymbolKind, PreparedGraphSetText,
    PreparedGraphText,
};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const M: LabelId = LabelId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const INNER: &str = "MATCH (a:L) UNWIND [7,7] AS token \
    CALL (a) { MATCH (a)-[:R]->(b) RETURN b AS child } RETURN token,a,child";
const OPTIONAL: &str = "MATCH (a:L) WITH a OPTIONAL MATCH (a)-[:R]->(b) RETURN a,b";
type Bag = BTreeMap<GraphValueRow, i128>;

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100_000, 10_000_000, 10_000_000)
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

fn scalar(value: Option<i64>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int))
}

fn vertex(value: Option<u128>) -> GraphValue {
    value.map_or_else(|| scalar(None), |id| GraphValue::Vertex(VId(id)))
}

fn bag(entries: impl IntoIterator<Item = (GraphValueRow, i128)>) -> Bag {
    entries.into_iter().collect()
}

fn occurrences(rows: impl IntoIterator<Item = GraphValueRow>) -> Bag {
    let mut result = Bag::new();
    for row in rows {
        *result.entry(row).or_default() += 1;
    }
    result
}

fn native_bag(rows: &ZSet<Vec<QueryValue>>) -> Bag {
    rows.iter()
        .map(|(row, count)| {
            let values = row
                .iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value.clone(),
                    _ => panic!("relational results retain their native value cells"),
                })
                .collect();
            (
                GraphValueRow::from_owned_values(values),
                count.to_i128().unwrap(),
            )
        })
        .collect()
}

fn difference(before: &Bag, after: &Bag) -> Bag {
    let mut delta = after.clone();
    for (row, count) in before {
        *delta.entry(row.clone()).or_default() -= count;
    }
    delta.retain(|_, count| *count != 0);
    delta
}

fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    definition: &PreparedGraphSet,
    handle: &StandingQueryHandle,
    expected: &Bag,
) {
    assert_eq!(
        occurrences(
            db.execute_graph_set_governed(cx, definition, policy())
                .unwrap()
                .value
        ),
        *expected
    );
    let (at, rows) = db.standing_native_bag(cx, handle, policy()).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&rows), *expected);
    assert_eq!(
        db.standing_native_columns(cx, handle).unwrap(),
        definition.columns()
    );
}

fn check_text(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    handle: &StandingQueryHandle,
    expected: &Bag,
) {
    for result in [
        db.query(cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap(),
        db.standing_native_query(cx, handle, policy()).unwrap().1,
    ] {
        let QueryResult::Rows { columns, rows } = result else {
            panic!("native correlated reads return rows");
        };
        assert_eq!(
            columns.as_slice(),
            db.standing_native_columns(cx, handle).unwrap()
        );
        let rows = rows.into_iter().map(|row| {
            GraphValueRow::from_owned_values(
                row.into_iter()
                    .map(|value| match value {
                        QueryValue::Value(value) => value,
                        _ => panic!("relational output cell"),
                    })
                    .collect(),
            )
        });
        assert_eq!(occurrences(rows), *expected);
    }
}

fn check_delta(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    handle: &StandingQueryHandle,
    from: CommitSeq,
    before: &Bag,
    after: &Bag,
) {
    let (at, actual) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&actual), difference(before, after));
    // Delivery is repeatable; observing a derivative does not consume it.
    let (_, again) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(again, actual);
}

fn inner_row(owner: u128, child: u128) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![
        scalar(Some(7)),
        vertex(Some(owner)),
        vertex(Some(child)),
    ])
}

fn optional_row(owner: u128, child: Option<u128>) -> GraphValueRow {
    GraphValueRow::from_owned_values(vec![vertex(Some(owner)), vertex(child)])
}

#[test]
fn native_call_and_optional_keep_parallel_bags_exact_deltas_and_bounded_footprints() {
    let ((), report) = run_async_under_lab(0x6170_7001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xc1)).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, label, value) in [(1, L, 10), (2, M, 20), (3, L, 30), (4, M, 40)] {
            seed.create_vertex(VId(id), vec![label], vec![(P, CanonicalScalar::Int(value))]);
        }
        seed.add_edge(EId(11), VId(1), VId(2), vec![]);
        seed.add_edge(EId(12), VId(1), VId(2), vec![]);
        seed.add_edge(EId(13), VId(3), VId(2), vec![]);
        db.write(&commit, seed).await.unwrap();

        let params = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [INNER, OPTIONAL] {
            let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            let (nodes, width) = prepared.subscription_footprint(&cx, &params).unwrap();
            let before = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &params, symbols, policy())
                .unwrap();
            assert!(u64::try_from(db.standing_queries.len() - before).unwrap() <= nodes);
            assert!(width > 0);
            queries.push((text, bind(text), handle));
        }
        let mut expected = [
            bag([(inner_row(1, 2), 4), (inner_row(3, 2), 2)]),
            bag([(optional_row(1, Some(2)), 2), (optional_row(3, Some(2)), 1)]),
        ];
        for ((text, definition, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, definition, handle, rows);
            check_text(&db, &cx, text, handle, rows);
        }

        for tick in 0..7 {
            let before = db.frontier().unwrap();
            let previous = expected.clone();
            let mut change = WriteBatch::new(R);
            expected = match tick {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(11)));
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(21)));
                    previous.clone()
                }
                1 => {
                    change.add_edge(EId(14), VId(1), VId(4), vec![]);
                    [
                        bag([
                            (inner_row(1, 2), 4),
                            (inner_row(1, 4), 2),
                            (inner_row(3, 2), 2),
                        ]),
                        bag([
                            (optional_row(1, Some(2)), 2),
                            (optional_row(1, Some(4)), 1),
                            (optional_row(3, Some(2)), 1),
                        ]),
                    ]
                }
                2 => {
                    change.delete_edge(EId(11));
                    [
                        bag([
                            (inner_row(1, 2), 2),
                            (inner_row(1, 4), 2),
                            (inner_row(3, 2), 2),
                        ]),
                        bag([
                            (optional_row(1, Some(2)), 1),
                            (optional_row(1, Some(4)), 1),
                            (optional_row(3, Some(2)), 1),
                        ]),
                    ]
                }
                3 => {
                    change.delete_edge(EId(12));
                    [
                        bag([(inner_row(1, 4), 2), (inner_row(3, 2), 2)]),
                        bag([(optional_row(1, Some(4)), 1), (optional_row(3, Some(2)), 1)]),
                    ]
                }
                4 => {
                    // The cascade retracts the last witness for owner 3.
                    change.delete_vertex(VId(2));
                    [
                        bag([(inner_row(1, 4), 2)]),
                        bag([(optional_row(1, Some(4)), 1), (optional_row(3, None), 1)]),
                    ]
                }
                5 => {
                    change.delete_vertex(VId(4));
                    [
                        Bag::new(),
                        bag([(optional_row(1, None), 1), (optional_row(3, None), 1)]),
                    ]
                }
                _ => {
                    change.create_vertex(VId(5), vec![L], vec![]);
                    change.create_vertex(VId(6), vec![M], vec![]);
                    change.add_edge(EId(15), VId(5), VId(6), vec![]);
                    [
                        bag([(inner_row(5, 6), 2)]),
                        bag([
                            (optional_row(1, None), 1),
                            (optional_row(3, None), 1),
                            (optional_row(5, Some(6)), 1),
                        ]),
                    ]
                }
            };
            db.write(&commit, change).await.unwrap();
            for (((text, definition, handle), old), next) in
                queries.iter().zip(&previous).zip(&expected)
            {
                check(&db, &cx, definition, handle, next);
                check_text(&db, &cx, text, handle, next);
                check_delta(&db, &cx, handle, before, old, next);
            }
        }
        for ((text, definition, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, definition, handle, rows);
            check_text(&db, &cx, text, handle, rows);
            assert!(sets::delta(&db.standing_queries[handle.index]).is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn literal(value: Option<i64>) -> GraphSetValue {
    GraphSetValue::Literal(
        GqlScalarParameter::new(value.map_or(CanonicalScalar::Null, CanonicalScalar::Int)).unwrap(),
    )
}

fn selected_children() -> PreparedGraphSet {
    let mut pattern = GraphPatternBuilder::new();
    pattern.vertex("b").unwrap();
    pattern.filter("b", VertexPredicate::HasLabel(M)).unwrap();
    pattern
        .filter(
            "b",
            VertexPredicate::IntegerProperty {
                key: P,
                comparison: IntegerComparison::Equal,
                value: 10,
            },
        )
        .unwrap();
    PreparedGraphSet::from(
        pattern
            .prepare_values(&[GraphColumn::vertex("child", "b")], 0, None)
            .unwrap()
            .with_duplicates(),
    )
    .project(
        vec![
            GraphSetProjection::new("child", GraphSetValue::Column(0)),
            GraphSetProjection::new("score", literal(Some(10))),
            GraphSetProjection::new("missing", literal(None)),
        ],
        GraphSetQuantifier::All,
    )
    .unwrap()
}

fn optional_definition() -> PreparedGraphSet {
    let left = leaf("MATCH (a:L) RETURN a AS owner,a.p AS wanted");
    let left = left
        .clone()
        .combine(GraphSetOperation::Union, GraphSetQuantifier::All, left)
        .unwrap();
    let arguments =
        PreparedGraphSet::arguments(vec!["wanted".into()], vec![GraphSetColumnType::Stored])
            .unwrap();
    let body = arguments
        .cross_join(selected_children())
        .unwrap()
        .filter(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(0),
            comparison: IntegerComparison::Equal,
            right: GraphSetOperand::Column(2),
        }])
        .unwrap()
        .project(
            vec![
                GraphSetProjection::new("child", GraphSetValue::Column(1)),
                GraphSetProjection::new("copied", GraphSetValue::Column(0)),
                GraphSetProjection::new("literal", literal(Some(7))),
                GraphSetProjection::new("missing", GraphSetValue::Column(3)),
                GraphSetProjection::new(
                    "quotient",
                    GraphSetValue::Integer(
                        GraphIntegerExpression::prepare(&[
                            GraphIntegerOp::Literal(Some(12)),
                            GraphIntegerOp::Column(0),
                            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
                        ])
                        .unwrap(),
                    ),
                ),
            ],
            GraphSetQuantifier::All,
        )
        .unwrap();
    left.optional_apply(body, vec![1]).unwrap()
}

fn computed_optional_row(owner: u128, wanted: Option<i64>, child: Option<u128>) -> GraphValueRow {
    let matched = child.is_some();
    GraphValueRow::from_owned_values(vec![
        vertex(Some(owner)),
        scalar(wanted),
        vertex(child),
        scalar(if matched { Some(10) } else { None }),
        scalar(if matched { Some(7) } else { None }),
        scalar(None),
        scalar(if matched { Some(1) } else { None }),
    ])
}

#[test]
fn scalar_imports_keep_optional_null_transitions_duplicates_and_lazy_exports() {
    let ((), report) = run_async_under_lab(0x6170_7002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xc2)).await.unwrap();
        let mut seed = WriteBatch::new(R);
        for (id, label, value) in [
            (1, L, Some(10)),
            (2, L, Some(0)),
            (3, L, None),
            (10, M, Some(10)),
            (11, M, Some(10)),
        ] {
            seed.create_vertex(
                VId(id),
                vec![label],
                value
                    .map(|value| vec![(P, CanonicalScalar::Int(value))])
                    .unwrap_or_default(),
            );
        }
        db.write(&commit, seed).await.unwrap();
        let definition = optional_definition();
        let handle = db
            .register_standing_relation(&cx, &definition, policy())
            .unwrap();
        // wanted=0 has no surviving child: its exported 12/wanted is never
        // evaluated. NULL exports cover the imported value and literal too.
        let mut expected = bag([
            (computed_optional_row(1, Some(10), Some(10)), 2),
            (computed_optional_row(1, Some(10), Some(11)), 2),
            (computed_optional_row(2, Some(0), None), 2),
            (computed_optional_row(3, None, None), 2),
        ]);
        check(&db, &cx, &definition, &handle, &expected);
        for tick in 0..6 {
            let from = db.frontier().unwrap();
            let before = expected.clone();
            let mut change = WriteBatch::new(R);
            expected = match tick {
                0 => {
                    change.set_vertex_property(VId(10), P, Some(CanonicalScalar::Int(20)));
                    bag([
                        (computed_optional_row(1, Some(10), Some(11)), 2),
                        (computed_optional_row(2, Some(0), None), 2),
                        (computed_optional_row(3, None, None), 2),
                    ])
                }
                1 => {
                    change.delete_vertex(VId(11));
                    bag([
                        (computed_optional_row(1, Some(10), None), 2),
                        (computed_optional_row(2, Some(0), None), 2),
                        (computed_optional_row(3, None, None), 2),
                    ])
                }
                2 => {
                    change.set_vertex_property(VId(10), P, Some(CanonicalScalar::Int(10)));
                    bag([
                        (computed_optional_row(1, Some(10), Some(10)), 2),
                        (computed_optional_row(2, Some(0), None), 2),
                        (computed_optional_row(3, None, None), 2),
                    ])
                }
                3 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(20)));
                    bag([
                        (computed_optional_row(1, Some(20), None), 2),
                        (computed_optional_row(2, Some(0), None), 2),
                        (computed_optional_row(3, None, None), 2),
                    ])
                }
                4 => {
                    change.set_vertex_property(VId(2), P, Some(CanonicalScalar::Int(10)));
                    bag([
                        (computed_optional_row(1, Some(20), None), 2),
                        (computed_optional_row(2, Some(10), Some(10)), 2),
                        (computed_optional_row(3, None, None), 2),
                    ])
                }
                _ => {
                    change.delete_vertex(VId(10));
                    bag([
                        (computed_optional_row(1, Some(20), None), 2),
                        (computed_optional_row(2, Some(10), None), 2),
                        (computed_optional_row(3, None, None), 2),
                    ])
                }
            };
            db.write(&commit, change).await.unwrap();
            check(&db, &cx, &definition, &handle, &expected);
            check_delta(&db, &cx, &handle, from, &before, &expected);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_real_null_child_is_a_witness_and_only_absence_nulls_imported_and_literal_exports() {
    let ((), report) = run_async_under_lab(0x6170_7003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xc3)).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
        db.write(&commit, seed).await.unwrap();

        let left = leaf("MATCH (a:L) RETURN a AS owner,a.p AS wanted");
        let arguments =
            PreparedGraphSet::arguments(vec!["wanted".into()], vec![GraphSetColumnType::Stored])
                .unwrap();
        let right = leaf("MATCH (b:M) RETURN b")
            .project(
                vec![GraphSetProjection::new("actual_null", literal(None))],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let body = arguments
            .cross_join(right)
            .unwrap()
            .project(
                vec![
                    GraphSetProjection::new("copied", GraphSetValue::Column(0)),
                    GraphSetProjection::new("literal", literal(Some(7))),
                    GraphSetProjection::new("actual_null", GraphSetValue::Column(1)),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap();
        let definition = left.optional_apply(body, vec![1]).unwrap();
        let handle = db
            .register_standing_relation(&cx, &definition, policy())
            .unwrap();
        let absent = bag([(
            GraphValueRow::from_owned_values(vec![
                vertex(Some(1)),
                scalar(Some(10)),
                scalar(None),
                scalar(None),
                scalar(None),
            ]),
            1,
        )]);
        let present = bag([(
            GraphValueRow::from_owned_values(vec![
                vertex(Some(1)),
                scalar(Some(10)),
                scalar(Some(10)),
                scalar(Some(7)),
                scalar(None),
            ]),
            1,
        )]);
        check(&db, &cx, &definition, &handle, &absent);
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.create_vertex(VId(2), vec![M], vec![]);
        db.write(&commit, change).await.unwrap();
        check(&db, &cx, &definition, &handle, &present);
        check_delta(&db, &cx, &handle, from, &absent, &present);
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(2));
        db.write(&commit, change).await.unwrap();
        check(&db, &cx, &definition, &handle, &absent);
        check_delta(&db, &cx, &handle, from, &present, &absent);
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
fn every_apply_registration_and_rebuild_checkpoint_keeps_old_circuits_private() {
    let ((), report) = run_async_under_lab(0x6170_7004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xc4)).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(10))]);
        seed.create_vertex(VId(2), vec![M], vec![(P, CanonicalScalar::Int(10))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let query = optional_definition();
        let before = saved(&db.standing_queries);
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
        assert!(calls > 0);
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
        let handle = db
            .register_standing_relation(&cx, &query, policy())
            .unwrap();
        let Layout::Circuit { first, .. } = handle.native.as_deref().unwrap() else {
            panic!("Apply owns one complete circuit");
        };
        let first = *first;
        let mut calls = 0;
        rebuild_checked(
            &mut db,
            &cx,
            first,
            handle.index,
            handle.native.as_deref().unwrap().private_inputs().unwrap(),
            policy(),
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
                    policy(),
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
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(2));
        db.write(&commit, change).await.unwrap();
        let expected = bag([(computed_optional_row(1, Some(10), None), 2)]);
        check(&db, &cx, &query, &handle, &expected);
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn per_invocation_scopes_and_fallible_rhs_refuse_even_beneath_empty_outer_and_limit_zero() {
    let ((), report) = run_async_under_lab(0x6170_7005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xc5)).await.unwrap();
        let mut seed = WriteBatch::new(R);
        // No L vertices: ordinary Apply never evaluates its conditional body.
        seed.create_vertex(VId(2), vec![M], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, seed).await.unwrap();
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (b:M) RETURN b",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let before = saved(&db.standing_queries);
        let params = GqlParameters::new();
        for text in [
            "MATCH (a:L) CALL (a) { MATCH (a)-[:R]->(b) RETURN b AS child LIMIT 1 } \
             RETURN a,child",
            "MATCH (a:L) CALL (a) { MATCH (a)-[:R]->(b) RETURN DISTINCT b AS child LIMIT 1 } \
             RETURN a,child",
            "MATCH (a:L) CALL (a) { MATCH (a)-[:R]->(b) RETURN count(b.p) AS total } \
             RETURN a,total",
            "MATCH (a:L) CALL (a) { MATCH (b:M) RETURN 12 / b.p AS quotient } \
             RETURN a,quotient",
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(text), policy())
                    .unwrap()
                    .value
                    .is_empty()
            );
            for zero in [false, true] {
                let statement = if zero {
                    format!("{text} LIMIT 0")
                } else {
                    text.to_owned()
                };
                let prepared = PreparedNativeRead::prepare(&statement, &params, symbols).unwrap();
                assert!(matches!(
                    prepared.subscription_footprint(&cx, &params),
                    Err(StandingQueryError::Unsupported)
                ));
                assert!(matches!(
                    prepared.register_standing(&mut db, &cx, &params, policy()),
                    Err(StandingQueryError::Unsupported)
                ));
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn inner_exports_fail_before_later_filters_and_optional_refuses_that_stage_order() {
    let ((), report) = run_async_under_lab(0x6170_7006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xc6)).await.unwrap();
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![L], vec![]);
        seed.create_vertex(VId(2), vec![M], vec![]);
        db.write(&commit, seed).await.unwrap();
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let before = saved(&db.standing_queries);
        let left = leaf("MATCH (a:L) RETURN a");
        let body = PreparedGraphSet::arguments(vec!["a".into()], vec![GraphSetColumnType::Vertex])
            .unwrap()
            .cross_join(leaf("MATCH (b:M) RETURN b"))
            .unwrap()
            .project(
                vec![GraphSetProjection::new(
                    "bad",
                    GraphSetValue::Integer(
                        GraphIntegerExpression::prepare(&[
                            GraphIntegerOp::Literal(Some(12)),
                            GraphIntegerOp::Literal(Some(0)),
                            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
                        ])
                        .unwrap(),
                    ),
                )],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .filter(&[GraphSetPredicateOp::Truth(Some(false))])
            .unwrap();
        let inner = left.clone().apply(body.clone(), vec![0]).unwrap();
        assert!(
            db.execute_graph_set_governed(&cx, &inner, policy())
                .is_err()
        );
        assert!(matches!(
            db.register_standing_relation(&cx, &inner, policy()),
            Err(StandingQueryError::Maintenance(
                StandingQueryFailure::OutputExpression { column: 0, .. }
            ))
        ));
        assert_eq!(saved(&db.standing_queries), before);
        let optional = left.optional_apply(body, vec![0]).unwrap();
        assert!(matches!(
            db.register_standing_relation(&cx, &optional, policy()),
            Err(StandingQueryError::Unsupported)
        ));
        assert_eq!(saved(&db.standing_queries), before);
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
