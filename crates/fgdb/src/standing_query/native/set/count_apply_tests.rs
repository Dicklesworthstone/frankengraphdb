//! Correlated COUNT maintains one summary per needed key, independently of outer multiplicity.
//! Literal fixture counts exercise both native COUNT bodies and returning CALL aggregates.

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
const COUNT_BODY: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "RETURN token,a,COUNT { MATCH (a)-[:R]->(b:M) } AS degree"
);
const CALL_COUNT: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[:R]->(b:M) RETURN count(*) AS degree } ",
    "RETURN token,a,degree"
);
const CALL_VALUE_COUNT: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[:R]->(b:M) RETURN count(b) AS degree } ",
    "RETURN token,a,degree"
);
const CALL_DISTINCT_NEIGHBORS: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[:R]->(b:M) RETURN count(DISTINCT b) AS degree } ",
    "RETURN token,a,degree"
);
const CALL_DISTINCT_EDGES: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "CALL (a) { MATCH (a)-[r:R]->(b:M) RETURN count(DISTINCT r) AS degree } ",
    "RETURN token,a,degree"
);
const COUNT_FILTER: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "WHERE COUNT { MATCH (a)-[:R]->(b:M) } >= 2 RETURN token,a"
);
const COUNT_EXPRESSION: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "RETURN token,a,COUNT { MATCH (a)-[:R]->(b:M) } + 1 AS next_degree"
);
const NULL_IMPORT: &str = concat!(
    "MATCH (a:L) WITH a OPTIONAL MATCH (a)-[:R]->(b:M) WITH a,b ",
    "RETURN a,b,COUNT { MATCH (b)-[:R]->(c:M) } AS degree"
);
const UNCORRELATED: &str = concat!(
    "MATCH (a:L) WITH a ",
    "RETURN a,COUNT { MATCH (b:M) } AS population"
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

fn integer(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}

fn vertex(value: Option<u128>) -> GraphValue {
    value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |id| {
        GraphValue::Vertex(VId(id))
    })
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
            _ => panic!("a native relational count retains its value cells"),
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
    policy: GqlQueryPolicy,
) {
    assert_eq!(
        occurrences(
            db.execute_graph_set_governed(cx, query, policy)
                .unwrap()
                .value
        ),
        *expected,
        "{text}"
    );
    let (at, rows) = db.standing_native_bag(cx, handle, policy).unwrap();
    assert_eq!(at, db.frontier().unwrap());
    assert_eq!(native_bag(&rows), *expected, "{text}");
    assert_eq!(
        db.standing_native_columns(cx, handle).unwrap(),
        query.columns()
    );
    for result in [
        db.query(cx, text, &GqlParameters::new(), symbols, policy)
            .unwrap(),
        db.standing_native_query(cx, handle, policy).unwrap().1,
    ] {
        let QueryResult::Rows { columns, rows } = result else {
            panic!("native correlated counts return rows");
        };
        assert_eq!(columns.as_slice(), query.columns());
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

// Counts are fixture literals. This helper only expresses the four public
// schemas and the separately specified two outer occurrences per owner.
fn expected_bags(counts: &[(u128, i64)]) -> [Bag; 4] {
    let mut expected = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
    for &(owner, count) in counts {
        let values = vec![integer(7), vertex(Some(owner))];
        let mut counted = values.clone();
        counted.push(integer(count));
        expected[0].insert(row(counted.clone()), 2);
        expected[1].insert(row(counted), 2);
        if count >= 2 {
            expected[2].insert(row(values.clone()), 2);
        }
        let mut projected = values;
        projected.push(integer(count + 1));
        expected[3].insert(row(projected), 2);
    }
    expected
}

fn assert_count_circuit(queries: &[StandingQuery]) {
    assert!(
        queries
            .iter()
            .any(|query| matches!(query, StandingQuery::Group(_)))
    );
    for expected in [RowJoinKind::Semi, RowJoinKind::Left] {
        assert!(queries.iter().any(|query| {
            matches!(query, StandingQuery::Join(join) if join.spec().kind() == expected)
        }));
    }
}

#[test]
fn native_count_and_call_summarize_parallel_witnesses_once_per_outer_occurrence() {
    let ((), report) = run_async_under_lab(0x636e_7001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe1)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [COUNT_BODY, CALL_COUNT, COUNT_FILTER, COUNT_EXPRESSION] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
            let (nodes, width) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            assert!(width > 0);
            assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
            assert_count_circuit(&db.standing_queries[first..]);
            queries.push((text, bind(text), handle));
        }
        let mut expected = expected_bags(&[(1, 2), (3, 1), (5, 0)]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..16 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(if phase == 1 { RelationId(2) } else { R });
            match phase {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(99)));
                }
                1 => {
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
                    change.delete_vertex(VId(2));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 | 1 | 3 => expected_bags(&[(1, 2), (3, 1), (5, 0)]),
                2 => expected_bags(&[(1, 3), (3, 1), (5, 0)]),
                4 => expected_bags(&[(1, 1), (3, 1), (5, 0)]),
                5 | 9 => expected_bags(&[(1, 0), (3, 0), (5, 0)]),
                6 | 8 => expected_bags(&[(1, 0), (3, 0), (5, 1)]),
                7 => expected_bags(&[(1, 1), (3, 1), (5, 1)]),
                10 => expected_bags(&[(1, 0), (3, 1), (5, 0)]),
                11 => expected_bags(&[(1, 0), (5, 0)]),
                12 | 14 | 15 => expected_bags(&[(1, 0), (5, 0), (9, 1)]),
                13 => expected_bags(&[(1, 0), (9, 1)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
            check_delta(&db, &cx, handle, db.frontier().unwrap(), rows, rows);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn expected_value_counts(counts: &[(u128, i64, i64)]) -> [Bag; 3] {
    let mut expected = [Bag::new(), Bag::new(), Bag::new()];
    for &(owner, occurrences, neighbors) in counts {
        for (output, count) in expected
            .iter_mut()
            .zip([occurrences, neighbors, occurrences])
        {
            output.insert(
                row(vec![integer(7), vertex(Some(owner)), integer(count)]),
                2,
            );
        }
    }
    expected
}

#[test]
fn native_value_counts_distinguish_parallel_edges_and_last_neighbor_support() {
    let ((), report) = run_async_under_lab(0x636e_7007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe7)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for text in [
            CALL_VALUE_COUNT,
            CALL_DISTINCT_NEIGHBORS,
            CALL_DISTINCT_EDGES,
        ] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
            assert_count_circuit(&db.standing_queries[first..]);
            queries.push((text, bind(text), handle));
        }
        let mut expected = expected_value_counts(&[(1, 2, 1), (3, 1, 1), (5, 0, 0)]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..12 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.add_edge(EId(15), VId(1), VId(4), vec![]);
                }
                2 => {
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.add_edge(EId(16), VId(1), VId(4), vec![]);
                }
                4 => {
                    change.set_vertex_label(VId(4), M, false);
                }
                5 => {
                    change.set_vertex_label(VId(4), M, true);
                }
                6 => {
                    change.delete_vertex(VId(4));
                }
                7 => {
                    change.add_edge(EId(17), VId(1), VId(2), vec![]);
                }
                8 => {
                    change.set_vertex_label(VId(1), L, false);
                }
                9 => {
                    change.set_vertex_label(VId(1), L, true);
                    change.add_edge(EId(18), VId(1), VId(2), vec![]);
                }
                10 => {
                    change.delete_vertex(VId(2));
                }
                11 => {
                    change.set_vertex_label(VId(6), M, true);
                    change.add_edge(EId(19), VId(5), VId(6), vec![]);
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 | 2 => expected_value_counts(&[(1, 1, 1), (3, 1, 1), (5, 0, 0)]),
                1 => expected_value_counts(&[(1, 2, 2), (3, 1, 1), (5, 0, 0)]),
                3 | 5 => expected_value_counts(&[(1, 2, 1), (3, 1, 1), (5, 0, 0)]),
                4 | 6 | 10 => expected_value_counts(&[(1, 0, 0), (3, 0, 0), (5, 0, 0)]),
                7 => expected_value_counts(&[(1, 1, 1), (3, 0, 0), (5, 0, 0)]),
                8 => expected_value_counts(&[(3, 0, 0), (5, 0, 0)]),
                9 => expected_value_counts(&[(1, 2, 1), (3, 0, 0), (5, 0, 0)]),
                11 => expected_value_counts(&[(1, 0, 0), (3, 0, 0), (5, 2, 1)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn nullable_row(owner: u128, imported: Option<u128>, count: i64) -> GraphValueRow {
    row(vec![vertex(Some(owner)), vertex(imported), integer(count)])
}

#[test]
fn counts_of_missing_and_null_imports_are_zero_without_scaling_shared_key_summaries() {
    let ((), report) = run_async_under_lab(0x636e_7002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe2)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, label) in [(1, L), (2, M), (3, L), (4, M)] {
            initial.create_vertex(VId(id), vec![label], vec![]);
        }
        initial.add_edge(EId(11), VId(1), VId(2), vec![]);
        initial.add_edge(EId(12), VId(1), VId(2), vec![]);
        initial.add_edge(EId(13), VId(2), VId(4), vec![]);
        initial.add_edge(EId(14), VId(2), VId(4), vec![]);
        db.write(&commit, initial).await.unwrap();
        let query = bind(NULL_IMPORT);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, NULL_IMPORT, &GqlParameters::new(), symbols, policy())
            .unwrap();
        assert_count_circuit(&db.standing_queries[first..]);
        let mut expected = bag([
            (nullable_row(1, Some(2), 2), 2),
            (nullable_row(3, None, 0), 1),
        ]);
        check(&db, &cx, NULL_IMPORT, &query, &handle, &expected, policy());
        for phase in 0..10 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.create_vertex(VId(5), vec![M], vec![]);
                    change.add_edge(EId(15), VId(3), VId(5), vec![]);
                }
                1 => {
                    change.add_edge(EId(16), VId(5), VId(4), vec![]);
                }
                2 => {
                    change.add_edge(EId(17), VId(2), VId(4), vec![]);
                }
                3 => {
                    change.delete_edge(EId(13));
                }
                4 => {
                    change.delete_vertex(VId(4));
                }
                5 => {
                    change.add_edge(EId(18), VId(5), VId(2), vec![]);
                }
                6 => {
                    change.delete_vertex(VId(5));
                }
                7 => {
                    change.create_vertex(VId(6), vec![M], vec![]);
                    change.add_edge(EId(19), VId(3), VId(2), vec![]);
                    change.add_edge(EId(20), VId(2), VId(6), vec![]);
                }
                8 => {
                    change.delete_vertex(VId(2));
                }
                9 => {
                    change.delete_vertex(VId(1));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => bag([
                    (nullable_row(1, Some(2), 2), 2),
                    (nullable_row(3, Some(5), 0), 1),
                ]),
                1 => bag([
                    (nullable_row(1, Some(2), 2), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                2 => bag([
                    (nullable_row(1, Some(2), 3), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                3 => bag([
                    (nullable_row(1, Some(2), 2), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                4 => bag([
                    (nullable_row(1, Some(2), 0), 2),
                    (nullable_row(3, Some(5), 0), 1),
                ]),
                5 => bag([
                    (nullable_row(1, Some(2), 0), 2),
                    (nullable_row(3, Some(5), 1), 1),
                ]),
                6 => bag([
                    (nullable_row(1, Some(2), 0), 2),
                    (nullable_row(3, None, 0), 1),
                ]),
                7 => bag([
                    (nullable_row(1, Some(2), 1), 2),
                    (nullable_row(3, Some(2), 1), 1),
                ]),
                8 => bag([(nullable_row(1, None, 0), 1), (nullable_row(3, None, 0), 1)]),
                9 => bag([(nullable_row(3, None, 0), 1)]),
                _ => unreachable!(),
            };
            check(&db, &cx, NULL_IMPORT, &query, &handle, &next, policy());
            check_delta(&db, &cx, &handle, from, &expected, &next);
            expected = next;
        }
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        check(&db, &cx, NULL_IMPORT, &query, &handle, &expected, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_count_witnesses_do_not_spend_the_public_result_row_quota() {
    let ((), report) = run_async_under_lab(0x636e_7003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe3)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![]);
        initial.create_vertex(VId(2), vec![M], vec![]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let two = GqlQueryPolicy::new(1_000, 2, 50_000_000, 50_000_000);
        let mut queries = Vec::new();
        for text in [COUNT_BODY, CALL_COUNT] {
            let query = bind(text);
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &GqlParameters::new(), symbols, two)
                .unwrap();
            assert_count_circuit(&db.standing_queries[first..]);
            queries.push((text, query, handle));
        }
        let mut expected = expected_bags(&[(1, 64)])[0].clone();
        for (text, query, handle) in &queries {
            check(&db, &cx, text, query, handle, &expected, two);
        }
        for phase in 0..4 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.add_edge(EId(1000), VId(1), VId(2), vec![]);
                }
                1 => {
                    change.set_vertex_label(VId(2), M, false);
                }
                2 => {
                    change.set_vertex_label(VId(2), M, true);
                }
                3 => {
                    change.delete_vertex(VId(2));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let count = match phase {
                0 | 2 => 65,
                1 | 3 => 0,
                _ => unreachable!(),
            };
            let next = expected_bags(&[(1, count)])[0].clone();
            for (text, query, handle) in &queries {
                check(&db, &cx, text, query, handle, &next, two);
                check_delta(&db, &cx, handle, from, &expected, &next);
                db.rebuild_standing_query(&cx, handle, two).unwrap();
                check(&db, &cx, text, query, handle, &next, two);
                for refused in [
                    GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000),
                    GqlQueryPolicy::new(0, 2, 50_000_000, 50_000_000),
                ] {
                    assert!(db.rebuild_standing_query(&cx, handle, refused).is_err());
                    check(&db, &cx, text, query, handle, &next, two);
                }
            }
            expected = next;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_empty_outer_bag_keeps_a_private_global_zero_group_under_zero_public_rows() {
    let ((), report) = run_async_under_lab(0x636e_7004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe4)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for id in 100..164 {
            initial.create_vertex(VId(id), vec![M], vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let query = bind(UNCORRELATED);
        let zero = GqlQueryPolicy::new(1_000, 0, 50_000_000, 50_000_000);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, UNCORRELATED, &GqlParameters::new(), symbols, zero)
            .unwrap();
        assert_count_circuit(&db.standing_queries[first..]);
        check(&db, &cx, UNCORRELATED, &query, &handle, &Bag::new(), zero);
        db.rebuild_standing_query(&cx, &handle, zero).unwrap();
        check(&db, &cx, UNCORRELATED, &query, &handle, &Bag::new(), zero);

        let mut change = WriteBatch::new(R);
        change.create_vertex(VId(1), vec![L], vec![]);
        db.write(&commit, change).await.unwrap();
        // The private exemption does not relax the public row limit.
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let one = GqlQueryPolicy::new(1_000, 1, 50_000_000, 50_000_000);
        db.rebuild_standing_query(&cx, &handle, one).unwrap();
        let expected = bag([(row(vec![vertex(Some(1)), integer(64)]), 1)]);
        check(&db, &cx, UNCORRELATED, &query, &handle, &expected, one);
        assert!(db.rebuild_standing_query(&cx, &handle, zero).is_err());
        check(&db, &cx, UNCORRELATED, &query, &handle, &expected, one);

        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(100));
        db.write(&commit, change).await.unwrap();
        let next = bag([(row(vec![vertex(Some(1)), integer(63)]), 1)]);
        check(&db, &cx, UNCORRELATED, &query, &handle, &next, one);
        check_delta(&db, &cx, &handle, from, &expected, &next);
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
fn each_count_registration_and_rebuild_checkpoint_preserves_existing_circuits() {
    let ((), report) = run_async_under_lab(0x636e_7005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe5)).await.unwrap();
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
        let query = bind(CALL_COUNT);
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
            panic!("count Apply owns one complete circuit");
        };
        let first = *first;
        let mut calls = 0;
        let private_inputs = handle.native.as_deref().unwrap().private_inputs().unwrap();
        rebuild_checked(
            &mut db,
            &cx,
            first,
            handle.index,
            private_inputs,
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
                    private_inputs,
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
        let expected = expected_bags(&[(1, 0), (3, 1), (5, 0)]);
        check(
            &db,
            &cx,
            CALL_COUNT,
            &query,
            &handle,
            &expected[1],
            policy(),
        );
        db.standing_native_query(&cx, &sibling, policy()).unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn local_count_semantics_and_fallible_discarded_values_refuse_even_under_empty_outer() {
    let ((), report) = run_async_under_lab(0x636e_7006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe6)).await.unwrap();
        let mut initial = WriteBatch::new(R);
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
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (a)-[:R]->(b) RETURN b LIMIT 1 } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (a)-[:R]->(b) RETURN DISTINCT b } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (b:M) WHERE b <> a RETURN b } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (a)-[:R]->(b) WITH b,12 / 0 AS bad RETURN b } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a RETURN COUNT { ",
                "MATCH (b:M) RETURN b.p } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a CALL (a) { ",
                "MATCH (a)-[:R]->(b) RETURN count(b.p) AS degree } RETURN a,degree"
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
