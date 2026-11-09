//! Native existential continuations maintain complete outer bags with semi/anti witnesses.
//! Expected truth values are literal fixture outcomes, independent of the maintenance recipe.

use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);
const M: LabelId = LabelId(2);
const LIMBS: LimbLimit = LimbLimit::new(4);
const POSITIVE: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH a,token ",
    "WHERE EXISTS { MATCH (a)-[:R]->(b:M) } RETURN token,a"
);
const NEGATIVE: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH a,token ",
    "WHERE NOT EXISTS { MATCH (a)-[:R]->(b:M) } RETURN token,a"
);
const PRESENT: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH a,token ",
    "RETURN token,a,EXISTS { MATCH (a)-[:R]->(b:M) } AS present"
);
const ABSENT: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH a,token ",
    "RETURN token,a,NOT EXISTS { MATCH (a)-[:R]->(b:M) } AS absent"
);
const NULL_IMPORT: &str = concat!(
    "MATCH (a:L) WITH a OPTIONAL MATCH (a)-[:R]->(b:M) WITH a,b ",
    "RETURN a,b,EXISTS { MATCH (b)-[:R]->(c:M) } AS present"
);
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

fn vertex(value: Option<u128>) -> GraphValue {
    value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
        GraphValue::Vertex(VId(id))
    })
}

fn boolean(value: bool) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Bool(value))
}

fn row(values: Vec<GraphValue>) -> GraphValueRow {
    GraphValueRow::from_owned_values(values)
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

fn cells(values: Vec<QueryValue>) -> GraphValueRow {
    row(values
        .into_iter()
        .map(|value| match value {
            QueryValue::Value(value) => value,
            _ => panic!("a native relational result retains its value cells"),
        })
        .collect())
}

fn native_bag(rows: &ZSet<Vec<QueryValue>>) -> Bag {
    rows.iter()
        .map(|(row, weight)| (cells(row.clone()), weight.to_i128().unwrap()))
        .collect()
}

fn difference(before: &Bag, after: &Bag) -> Bag {
    let mut delta = after.clone();
    for (row, weight) in before {
        *delta.entry(row.clone()).or_default() -= weight;
    }
    delta.retain(|_, weight| *weight != 0);
    delta
}

fn check(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    text: &str,
    query: &PreparedGraphSet,
    handle: &StandingQueryHandle,
    expected: &Bag,
) {
    assert_eq!(
        occurrences(
            db.execute_graph_set_governed(cx, query, policy())
                .unwrap()
                .value
        ),
        *expected,
        "{text}"
    );
    let (at, rows) = db.standing_native_bag(cx, handle, policy()).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&rows), *expected, "{text}");
    assert_eq!(
        db.standing_native_columns(cx, handle).unwrap(),
        query.columns()
    );
    for result in [
        db.query(cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap(),
        db.standing_native_query(cx, handle, policy()).unwrap().1,
    ] {
        let QueryResult::Rows { columns, rows } = result else {
            panic!("native existence queries return rows");
        };
        assert_eq!(columns, query.columns());
        assert_eq!(
            occurrences(rows.into_iter().map(cells)),
            *expected,
            "{text}"
        );
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
    let (at, delta) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&delta), difference(before, after));
    let (_, repeated) = db
        .standing_native_delta(cx, handle, from, policy())
        .unwrap();
    assert_eq!(repeated, delta);
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
        batch.create_vertex(VId(id), labels, vec![(P, CanonicalScalar::Int(10))]);
    }
    batch.add_edge(EId(11), VId(1), VId(2), vec![]);
    batch.add_edge(EId(12), VId(1), VId(2), vec![]);
    batch.add_edge(EId(13), VId(3), VId(4), vec![]);
    batch.add_edge(EId(14), VId(5), VId(6), vec![]);
    batch
}

// The caller supplies the fixture's literal truth table. This helper only
// projects those known answers into the four public result schemas.
fn expected_bags(truth: &[(u128, bool)]) -> [Bag; 4] {
    let mut expected = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
    for &(owner, present) in truth {
        let values = vec![
            GraphValue::Scalar(CanonicalScalar::Int(7)),
            vertex(Some(owner)),
        ];
        expected[usize::from(!present)].insert(row(values.clone()), 2);
        let mut positive = values.clone();
        positive.push(boolean(present));
        expected[2].insert(row(positive), 2);
        let mut negative = values;
        negative.push(boolean(!present));
        expected[3].insert(row(negative), 2);
    }
    expected
}

fn assert_existence_circuit(queries: &[StandingQuery]) {
    for expected in [RowJoinKind::Semi, RowJoinKind::Anti] {
        assert!(queries.iter().any(|query| {
            matches!(query, StandingQuery::Join(join) if join.spec().kind() == expected)
        }));
    }
}

#[test]
fn native_exists_filters_and_values_keep_outer_bags_through_first_and_last_witnesses() {
    let ((), report) = run_async_under_lab(0x6578_7001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd1)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [POSITIVE, NEGATIVE, PRESENT, ABSENT] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
            let (nodes, width) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            assert!(width > 0);
            assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
            // These WITH/UNWIND continuations must engage the new relational
            // existence circuit, not an older graph-pattern probe source.
            assert_existence_circuit(&db.standing_queries[first..]);
            queries.push((text, bind(text), handle));
        }
        let mut expected = expected_bags(&[(1, true), (3, true), (5, false)]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows);
        }
        for phase in 0..16 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(if phase == 1 { RelationId(2) } else { R });
            match phase {
                0 => {
                    // A property no source reads cannot alter the answer.
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                }
                1 => {
                    // Same endpoints and destination label, wrong relation.
                    change.add_edge(EId(90), VId(5), VId(2), vec![]);
                }
                2 => {
                    change.add_edge(EId(15), VId(1), VId(4), vec![]);
                }
                3 => {
                    change.delete_edge(EId(11));
                }
                4 => {
                    change.delete_edge(EId(12));
                }
                5 => {
                    change.set_vertex_label(VId(4), M, false);
                }
                6 => {
                    change.set_vertex_label(VId(6), M, true);
                }
                7 => {
                    change.set_vertex_label(VId(4), M, true);
                }
                8 => {
                    change.delete_vertex(VId(4));
                }
                9 => {
                    change.delete_edge(EId(14));
                }
                10 => {
                    change.add_edge(EId(16), VId(3), VId(2), vec![]);
                }
                11 => {
                    change.delete_vertex(VId(3));
                }
                12 => {
                    // Both the outer bag and its witness change in one tick.
                    change.create_vertex(VId(9), vec![L], vec![]);
                    change.create_vertex(VId(10), vec![M], vec![]);
                    change.add_edge(EId(17), VId(9), VId(10), vec![]);
                }
                13 => {
                    change.set_vertex_label(VId(5), L, false);
                }
                14 => {
                    change.set_vertex_label(VId(5), L, true);
                }
                15 => {
                    // Only the foreign-relation edge remains incident here.
                    change.delete_vertex(VId(2));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0..=4 => expected_bags(&[(1, true), (3, true), (5, false)]),
                5 | 9 => expected_bags(&[(1, false), (3, false), (5, false)]),
                6 | 8 => expected_bags(&[(1, false), (3, false), (5, true)]),
                7 => expected_bags(&[(1, true), (3, true), (5, true)]),
                10 => expected_bags(&[(1, false), (3, true), (5, false)]),
                11 => expected_bags(&[(1, false), (5, false)]),
                12 | 14 | 15 => expected_bags(&[(1, false), (5, false), (9, true)]),
                13 => expected_bags(&[(1, false), (9, true)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after);
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows);
            check_delta(&db, &cx, handle, db.frontier().unwrap(), rows, rows);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn nullable_row(owner: u128, imported: Option<u128>, present: bool) -> GraphValueRow {
    row(vec![
        vertex(Some(owner)),
        vertex(imported),
        boolean(present),
    ])
}

#[test]
fn exists_over_optional_imports_preserves_null_outer_rows_and_parallel_multiplicity() {
    let ((), report) = run_async_under_lab(0x6578_7002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd2)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, label) in [(1, L), (2, M), (3, L), (4, M)] {
            initial.create_vertex(VId(id), vec![label], vec![]);
        }
        initial.add_edge(EId(11), VId(1), VId(2), vec![]);
        initial.add_edge(EId(12), VId(1), VId(2), vec![]);
        initial.add_edge(EId(13), VId(2), VId(4), vec![]);
        db.write(&commit, initial).await.unwrap();
        let query = bind(NULL_IMPORT);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, NULL_IMPORT, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_existence_circuit(&db.standing_queries[first..]);
        let mut expected = bag([
            (nullable_row(1, Some(2), true), 2),
            (nullable_row(3, None, false), 1),
        ]);
        check(&db, &cx, NULL_IMPORT, &query, &handle, &expected);
        for phase in 0..7 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.create_vertex(VId(5), vec![M], vec![]);
                    change.add_edge(EId(14), VId(3), VId(5), vec![]);
                }
                1 => {
                    change.add_edge(EId(15), VId(5), VId(4), vec![]);
                }
                2 => {
                    change.delete_edge(EId(13));
                }
                3 => {
                    change.delete_vertex(VId(5));
                }
                4 => {
                    change.add_edge(EId(16), VId(3), VId(2), vec![]);
                }
                5 => {
                    change.delete_vertex(VId(2));
                }
                6 => {
                    change.delete_vertex(VId(1));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => bag([
                    (nullable_row(1, Some(2), true), 2),
                    (nullable_row(3, Some(5), false), 1),
                ]),
                1 => bag([
                    (nullable_row(1, Some(2), true), 2),
                    (nullable_row(3, Some(5), true), 1),
                ]),
                2 => bag([
                    (nullable_row(1, Some(2), false), 2),
                    (nullable_row(3, Some(5), true), 1),
                ]),
                3 => bag([
                    (nullable_row(1, Some(2), false), 2),
                    (nullable_row(3, None, false), 1),
                ]),
                4 => bag([
                    (nullable_row(1, Some(2), false), 2),
                    (nullable_row(3, Some(2), false), 1),
                ]),
                5 => bag([
                    (nullable_row(1, None, false), 1),
                    (nullable_row(3, None, false), 1),
                ]),
                6 => bag([(nullable_row(3, None, false), 1)]),
                _ => unreachable!(),
            };
            check(&db, &cx, NULL_IMPORT, &query, &handle, &next);
            check_delta(&db, &cx, &handle, from, &expected, &next);
            expected = next;
        }
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, NULL_IMPORT, &query, &handle, &expected);
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
fn each_existential_registration_and_rebuild_checkpoint_preserves_existing_circuits() {
    let ((), report) = run_async_under_lab(0x6578_7003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd3)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let sibling = db
            .register_standing_native(
                &cx,
                "MATCH (n) RETURN n",
                &GqlParameters::new(),
                symbols,
                policy(),
            )
            .unwrap();
        let query = bind(PRESENT);
        let before = saved(&db.standing_queries);
        let mut calls = 0;
        {
            let mut staged = Staging::new(&mut db);
            staged
                .compile_root(&cx, &query, policy(), &mut || {
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
                        .compile_root(&cx, &query, policy(), &mut || {
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
            panic!("existential Apply owns one complete circuit");
        };
        let first = *first;
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
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(2));
        db.write(&commit, change).await.unwrap();
        let expected = expected_bags(&[(1, false), (3, true), (5, false)]);
        check(&db, &cx, PRESENT, &query, &handle, &expected[2]);
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn existential_source_effects_and_local_pages_refuse_under_empty_outer_and_limit_zero() {
    let ((), report) = run_async_under_lab(0x6578_7004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xd4)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        // No L vertex: the original conditional bodies are never invoked.
        initial.create_vertex(VId(2), vec![M], vec![(P, CanonicalScalar::Int(0))]);
        db.write(&commit, initial).await.unwrap();
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
        let parameters = GqlParameters::new();
        for text in [
            concat!(
                "MATCH (a:L) WITH a RETURN EXISTS { ",
                "MATCH (a)-[:R]->(b) RETURN b LIMIT 2 } AS present"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN EXISTS { ",
                "MATCH (a)-[:R]->(b) RETURN DISTINCT b } AS present"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN EXISTS { ",
                "MATCH (a)-[:R]->(b) RETURN count(b) AS total } AS present"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN EXISTS { ",
                "MATCH (b:M) RETURN 12 / b.p AS quotient } AS present"
            ),
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
                let prepared =
                    PreparedNativeRead::prepare(&statement, &parameters, symbols).unwrap();
                assert!(matches!(
                    prepared.subscription_footprint(&cx, &parameters),
                    Err(StandingQueryError::Unsupported)
                ));
                assert!(matches!(
                    prepared.register_standing(&mut db, &cx, &parameters, policy()),
                    Err(StandingQueryError::Unsupported)
                ));
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
