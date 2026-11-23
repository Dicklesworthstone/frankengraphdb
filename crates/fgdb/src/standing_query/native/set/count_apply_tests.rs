//! Correlated scalar aggregates retain one summary per invocation, independent of outer multiplicity.
//! Literal fixture values exercise native COUNT bodies and returning CALL aggregates.

use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, LimbLimit, PropertyKeyId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::row_join::RowJoinKind;
use fgdb_gql::{GqlParameters, GraphSymbol, GraphSymbolKind, PreparedGraphSetText};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts,
};
use std::collections::BTreeMap;

const P: PropertyKeyId = PropertyKeyId(1);
const SELECTED: PropertyKeyId = PropertyKeyId(2);
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
const COUNT_DISTINCT_BODY: &str = concat!(
    "MATCH (a:L) UNWIND [7,7] AS token WITH token,a ",
    "RETURN token,a,COUNT { MATCH (a)-[:R]->(b:M) RETURN DISTINCT b } AS degree"
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
        (GraphSymbolKind::Property, "selected") => Some(GraphSymbol::Property(SELECTED)),
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
            _ => panic!("a native relational aggregate retains its value cells"),
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
            panic!("native correlated aggregates return rows");
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

fn expected_value_counts(counts: &[(u128, i64, i64)]) -> [Bag; 4] {
    let mut expected = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
    for &(owner, occurrences, neighbors) in counts {
        for (output, count) in
            expected
                .iter_mut()
                .zip([occurrences, neighbors, occurrences, neighbors])
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
            COUNT_DISTINCT_BODY,
        ] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            assert!(u64::try_from(db.standing_queries.len() - first).unwrap() <= nodes);
            if text == COUNT_DISTINCT_BODY {
                assert!(
                    db.standing_queries[first..]
                        .iter()
                        .any(|query| matches!(query, StandingQuery::Group(_)))
                );
            } else {
                assert_count_circuit(&db.standing_queries[first..]);
            }
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
fn each_count_and_average_registration_and_rebuild_checkpoint_preserves_existing_circuits() {
    let ((), report) = run_async_under_lab(0x636e_7005, |root| async move {
        for text in [CALL_COUNT.to_owned(), scalar_call_text("avg(id(b))")] {
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
            let query = bind(&text);
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
                panic!("scalar Apply owns one complete circuit");
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
            let expected = if text == CALL_COUNT {
                expected_bags(&[(1, 0), (3, 1), (5, 0)])[1].clone()
            } else {
                scalar_call_bag([vertex(None), floating(4.0), vertex(None)])
            };
            check(&db, &cx, &text, &query, &handle, &expected, policy());
            db.standing_native_query(&cx, &sibling, policy()).unwrap();
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_count_body_sources_refuse_even_under_empty_outer() {
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
                "MATCH (b:M) RETURN b.p } AS degree"
            ),
            concat!(
                "MATCH (a:L) WITH a,a.p AS threshold RETURN COUNT { ",
                "MATCH (a)-[:R]->(b:M) WHERE b.p > threshold RETURN b } AS degree"
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

fn filtered_counts_text(aggregate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [NULL,1,2,2] AS floor \
         WITH a,a.p AS score,toInteger(floor) AS floor \
         CALL (a,score,floor) {{ \
         MATCH (a)-[:R]->(b:M {{selected:1}}) \
         WHERE score >= floor AND b <> a RETURN {aggregate} AS degree }} \
         RETURN a,floor,degree"
    )
}

type FilteredCounts = (u128, [(i64, i64); 2]);

// Literal expected (occurrence count, distinct neighbor count) for thresholds
// one and two. NULL never passes the predicate; threshold two occurs twice.
fn filtered_expected(entries: &[FilteredCounts]) -> [Bag; 3] {
    let mut bags = [Bag::new(), Bag::new(), Bag::new()];
    for &(owner, counts) in entries {
        for (floor, (count, distinct), multiplicity) in [
            (None, (0, 0), 1),
            (Some(1), counts[0], 1),
            (Some(2), counts[1], 2),
        ] {
            for (result, value) in bags.iter_mut().zip([count, count, distinct]) {
                result.insert(
                    row(vec![
                        vertex(Some(owner)),
                        floor.map_or(GraphValue::Scalar(CanonicalScalar::Null), integer),
                        integer(value),
                    ]),
                    multiplicity,
                );
            }
        }
    }
    bags
}

#[test]
fn filtered_counts_maintain_nullable_invocations_and_property_selected_witnesses() {
    let ((), report) = run_async_under_lab(0x636e_7010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf0)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, labels, props) in [
            (
                1,
                vec![L, M],
                vec![
                    (P, CanonicalScalar::Int(2)),
                    (SELECTED, CanonicalScalar::Int(1)),
                ],
            ),
            (2, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (3, vec![L], vec![(P, CanonicalScalar::Int(1))]),
            (4, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (5, vec![L], vec![]),
            (6, vec![M], vec![(SELECTED, CanonicalScalar::Int(0))]),
        ] {
            initial.create_vertex(VId(id), labels, props);
        }
        for (id, src, dst) in [
            (11, 1, 2),
            (12, 1, 2),
            (13, 1, 4),
            (14, 1, 1),
            (15, 3, 4),
            (16, 5, 2),
            (17, 1, 6),
        ] {
            initial.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for aggregate in ["count(*)", "count(b)", "count(DISTINCT b)"] {
            let text = filtered_counts_text(aggregate);
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let before = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= (db.standing_queries.len() - before) as u64);
            assert!(
                db.standing_queries[before..]
                    .iter()
                    .any(|query| matches!(query, StandingQuery::Group(_)))
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        let mut expected = filtered_expected(&[
            (1, [(3, 2), (3, 2)]),
            (3, [(1, 1), (0, 0)]),
            (5, [(0, 0), (0, 0)]),
        ]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..11 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => change.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(3))),
                1 => change.set_vertex_property(VId(2), SELECTED, None),
                2 => change.set_vertex_property(VId(5), P, Some(CanonicalScalar::Int(2))),
                3 => change.set_vertex_property(VId(2), SELECTED, Some(CanonicalScalar::Int(1))),
                4 => change.delete_edge(EId(11)),
                5 => change.delete_edge(EId(12)),
                6 => change.delete_vertex(VId(4)),
                7 => {
                    change.set_vertex_property(VId(1), P, None);
                    change.add_edge(EId(18), VId(1), VId(2), vec![])
                }
                8 => change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(2))),
                9 => change.delete_vertex(VId(5)),
                10 => change.delete_vertex(VId(2)),
                _ => unreachable!(),
            };
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => filtered_expected(&[
                    (1, [(3, 2), (3, 2)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(0, 0), (0, 0)]),
                ]),
                1 | 2 => filtered_expected(&[
                    (1, [(1, 1), (1, 1)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(0, 0), (0, 0)]),
                ]),
                3 => filtered_expected(&[
                    (1, [(3, 2), (3, 2)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                4 => filtered_expected(&[
                    (1, [(2, 2), (2, 2)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                5 => filtered_expected(&[
                    (1, [(1, 1), (1, 1)]),
                    (3, [(1, 1), (1, 1)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                6 | 7 => filtered_expected(&[
                    (1, [(0, 0), (0, 0)]),
                    (3, [(0, 0), (0, 0)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                8 => filtered_expected(&[
                    (1, [(1, 1), (1, 1)]),
                    (3, [(0, 0), (0, 0)]),
                    (5, [(1, 1), (1, 1)]),
                ]),
                9 => filtered_expected(&[(1, [(1, 1), (1, 1)]), (3, [(0, 0), (0, 0)])]),
                10 => filtered_expected(&[(1, [(0, 0), (0, 0)]), (3, [(0, 0), (0, 0)])]),
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

#[test]
fn filtered_count_private_witnesses_preserve_public_quota_and_registration_atomicity() {
    let ((), report) = run_async_under_lab(0x636e_7011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf1)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(2))]);
        initial.create_vertex(VId(2), vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let four = GqlQueryPolicy::new(1_000, 4, 50_000_000, 50_000_000);
        let text = filtered_counts_text("count(*)");
        let query = bind(&text);
        let handle = db
            .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, four)
            .unwrap();
        let expected = filtered_expected(&[(1, [(64, 1), (64, 1)])]);
        check(&db, &cx, &text, &query, &handle, &expected[0], four);

        let before = saved(&db.standing_queries);
        let refused = GqlQueryPolicy::new(1_000, 4, 0, 0);
        assert!(
            db.register_standing_native(&cx, &text, &GqlParameters::new(), symbols, refused)
                .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        check(&db, &cx, &text, &query, &handle, &expected[0], four);

        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(2), SELECTED, None);
        db.write(&commit, change).await.unwrap();
        let empty = filtered_expected(&[(1, [(0, 0), (0, 0)])]);
        check(&db, &cx, &text, &query, &handle, &empty[0], four);
        check_delta(&db, &cx, &handle, from, &expected[0], &empty[0]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_count_keeps_uninvoked_and_invoked_expression_errors_distinct() {
    let ((), report) = run_async_under_lab(0x636e_7012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf2)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(2), vec![M], vec![]);
        db.write(&commit, initial).await.unwrap();
        let parameters = GqlParameters::new();
        let theta = concat!(
            "MATCH (a:L) WITH a RETURN COUNT { ",
            "MATCH (b:M) WHERE b <> a RETURN b } AS degree"
        );
        let broken = concat!(
            "MATCH (a:L) WITH a RETURN COUNT { ",
            "MATCH (a)-[:R]->(b) WITH b,12 / 0 AS bad RETURN b } AS degree"
        );
        let mut dormant = Vec::new();
        for text in [theta, broken] {
            let handle = db
                .register_standing_native(&cx, text, &parameters, symbols, policy())
                .unwrap();
            check(&db, &cx, text, &bind(text), &handle, &Bag::new(), policy());
            dormant.push(handle);
        }
        let mut invoked = WriteBatch::new(R);
        invoked.create_vertex(VId(1), vec![L], vec![]);
        invoked.add_edge(EId(1), VId(1), VId(2), vec![]);
        db.write(&commit, invoked).await.unwrap();
        assert!(
            db.standing_native_query(&cx, &dormant[1], policy())
                .is_err()
        );
        let selected = db
            .register_standing_native(&cx, theta, &parameters, symbols, policy())
            .unwrap();
        let one = bag([(row(vec![integer(1)]), 1)]);
        check(&db, &cx, theta, &bind(theta), &selected, &one, policy());
        let saved_registry = saved(&db.standing_queries);
        for text in [
            broken.to_owned(),
            format!("{broken} ORDER BY degree LIMIT 0"),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(&text), policy())
                    .is_err()
            );
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            prepared.subscription_footprint(&cx, &parameters).unwrap();
            assert!(
                prepared
                    .register_standing(&mut db, &cx, &parameters, policy())
                    .is_err()
            );
            assert_eq!(saved(&db.standing_queries), saved_registry);
        }
        check(&db, &cx, theta, &bind(theta), &selected, &one, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_summary_calls_maintain_computed_rows_filters_and_local_distinct() {
    let ((), report) = run_async_under_lab(0x7073_7010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe7)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let prefix = "MATCH (a:L) UNWIND [7,7] AS token WITH token,a CALL (a) { ";
        let statements = [
            format!("{prefix}MATCH (a)-[:R]->(b:M) WITH count(*) AS n WHERE n > 1 RETURN n+1 AS degree }} RETURN token,a,degree"),
            format!("{prefix}MATCH (a)-[:R]->(b:M) WITH count(*) AS n,sum(id(b)) AS s WHERE n = 0 OR s >= 4 RETURN coalesce(s,7) AS total,n+1 AS degree }} RETURN token,a,total,degree"),
            format!("{prefix}MATCH (a)-[:R]->(b:M) WITH count(*) AS n WITH DISTINCT NULL AS absent RETURN absent }} RETURN token,a,absent"),
            format!("{prefix}MATCH (a)-[:R]->(b:M) RETURN count(*)+1 AS degree }} RETURN token,a,degree"),
        ];
        let mut queries = Vec::new();
        for text in statements {
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared.register_standing(&mut db, &cx, &parameters, policy()).unwrap();
            assert!(nodes >= (db.standing_queries.len() - first) as u64);
            assert!(db.standing_queries[first..].iter().any(|query| matches!(query, StandingQuery::Group(_))));
            queries.push((text.clone(), bind(&text), handle));
        }
        // Counts and sums are literal fixture answers. Formatting the returned
        // expressions here does not invoke an engine or maintain a second graph.
        let expected = |groups: &[(u128, i64, Option<i64>)]| {
            let mut rows = [Bag::new(), Bag::new(), Bag::new(), Bag::new()];
            for &(owner, count, sum) in groups {
                let prefix = vec![integer(7), vertex(Some(owner))];
                let mut computed = prefix.clone();
                computed.push(integer(count + 1));
                if count > 1 {
                    rows[0].insert(row(computed.clone()), 2);
                }
                rows[3].insert(row(computed), 2);
                if count == 0 || sum.is_some_and(|value| value >= 4) {
                    let mut tuple = prefix.clone();
                    tuple.extend([integer(sum.unwrap_or(7)), integer(count + 1)]);
                    rows[1].insert(row(tuple), 2);
                }
                let mut absent = prefix;
                absent.push(vertex(None));
                rows[2].insert(row(absent), 2);
            }
            rows
        };
        let mut previous = expected(&[(1, 2, Some(4)), (3, 1, Some(4)), (5, 0, None)]);
        for ((text, query, handle), rows) in queries.iter().zip(&previous) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..6 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => change.delete_edge(EId(11)),
                1 => change.add_edge(EId(15), VId(5), VId(2), vec![]),
                2 => {
                    change.delete_edge(EId(12));
                    change.delete_edge(EId(13));
                }
                3 => {
                    change.add_edge(EId(16), VId(3), VId(2), vec![]);
                    change.add_edge(EId(17), VId(3), VId(2), vec![]);
                    change.add_edge(EId(18), VId(1), VId(4), vec![]);
                }
                4 => change.delete_vertex(VId(2)),
                5 => change.set_vertex_label(VId(3), L, false),
                _ => unreachable!(),
            };
            db.write(&commit, change).await.unwrap();
            let next = match phase {
                0 => expected(&[(1, 1, Some(2)), (3, 1, Some(4)), (5, 0, None)]),
                1 => expected(&[(1, 1, Some(2)), (3, 1, Some(4)), (5, 1, Some(2))]),
                2 => expected(&[(1, 0, None), (3, 0, None), (5, 1, Some(2))]),
                3 => expected(&[(1, 1, Some(4)), (3, 2, Some(4)), (5, 1, Some(2))]),
                4 => expected(&[(1, 1, Some(4)), (3, 0, None), (5, 0, None)]),
                5 => expected(&[(1, 1, Some(4)), (5, 0, None)]),
                _ => unreachable!(),
            };
            for (((text, query, handle), old), rows) in queries.iter().zip(&previous).zip(&next) {
                check(&db, &cx, text, query, handle, rows, policy());
                check_delta(&db, &cx, handle, from, old, rows);
            }
            previous = next;
        }
        for ((text, query, handle), rows) in queries.iter().zip(&previous) {
            db.rebuild_standing_query(&cx, handle, policy()).unwrap();
            check(&db, &cx, text, query, handle, rows, policy());
            let before = saved(&db.standing_queries);
            for refused in [
                GqlQueryPolicy::new(100_000, 100_000, 0, 50_000_000),
                GqlQueryPolicy::new(100_000, 100_000, 50_000_000, 0),
            ] {
                assert!(db.register_standing_native(&cx, text, &parameters, symbols, refused).is_err());
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn post_summary_projection_errors_cannot_publish_or_hide_behind_final_pages() {
    let ((), report) = run_async_under_lab(0x7073_7011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xe8)).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let parameters = GqlParameters::new();
        let safe = "MATCH (a:L) WITH a CALL (a) { MATCH (a)-[:R]->(b:M) WITH count(*) AS n WHERE n > 0 RETURN 12/n AS quotient } RETURN a,quotient";
        let handle = db.register_standing_native(&cx, safe, &parameters, symbols, policy()).unwrap();
        let rows = bag([(row(vec![vertex(Some(1)), integer(6)]), 1), (row(vec![vertex(Some(3)), integer(12)]), 1)]);
        check(&db, &cx, safe, &bind(safe), &handle, &rows, policy());
        let before = saved(&db.standing_queries);
        for tail in ["", " LIMIT 0", " LIMIT 1"] {
            for body in [
                "WITH count(*) AS n RETURN 12/n AS quotient",
                "WITH count(*) AS n WITH n,12/0 AS discarded WHERE false RETURN n AS quotient",
            ] {
                let text = format!("MATCH (a:L) WITH a CALL (a) {{ MATCH (a)-[:R]->(b:M) {body} }} RETURN a,quotient{tail}");
                assert!(db.execute_graph_set_governed(&cx, &bind(&text), policy()).is_err());
                let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
                prepared.subscription_footprint(&cx, &parameters).unwrap();
                assert!(prepared.register_standing(&mut db, &cx, &parameters, policy()).is_err());
                assert_eq!(saved(&db.standing_queries), before);
            }
        }
        check(&db, &cx, safe, &bind(safe), &handle, &rows, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn floating(value: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
}

fn null_summaries() -> [GraphValue; 6] {
    std::array::from_fn(|_| GraphValue::Scalar(CanonicalScalar::Null))
}

// Fixture literals only: this helper formats the six native result domains.
fn scalar_summaries(
    sum: i64,
    distinct_sum: i64,
    average: f64,
    distinct_average: f64,
    min: i64,
    max: i64,
) -> [GraphValue; 6] {
    [
        integer(sum),
        integer(distinct_sum),
        floating(average),
        floating(distinct_average),
        integer(min),
        integer(max),
    ]
}

fn scalar_aggregates_text(aggregate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [NULL,1,3,3] AS floor \
         WITH a,a.p AS score,toInteger(floor) AS floor \
         CALL (a,score,floor) {{ \
         MATCH (a)-[:R]->(b:M {{selected:1}}) \
         WHERE (floor IS NULL OR score >= floor) AND b <> a \
         WITH score + id(b) AS value RETURN {aggregate} AS total }} \
         RETURN a,floor,total"
    )
}

// Each explicit result tuple belongs to NULL, one, or three respectively.
// The duplicate three restores two outer occurrences without scaling the body.
fn scalar_aggregate_bags(entries: &[(u128, [[GraphValue; 6]; 3])]) -> [Bag; 6] {
    let mut bags: [Bag; 6] = std::array::from_fn(|_| Bag::new());
    for (owner, results) in entries {
        for ((floor, multiplicity), values) in [(None, 1), (Some(1), 1), (Some(3), 2)]
            .into_iter()
            .zip(results)
        {
            for (bag, value) in bags.iter_mut().zip(values) {
                bag.insert(
                    row(vec![
                        vertex(Some(*owner)),
                        floor.map_or(GraphValue::Scalar(CanonicalScalar::Null), integer),
                        value.clone(),
                    ]),
                    multiplicity,
                );
            }
        }
    }
    bags
}

#[test]
fn filtered_scalar_aggregates_maintain_nullable_duplicate_invocations_and_retractions() {
    let ((), report) = run_async_under_lab(0x7361_7010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf3)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, labels, props) in [
            (
                1,
                vec![L, M],
                vec![
                    (P, CanonicalScalar::Int(2)),
                    (SELECTED, CanonicalScalar::Int(1)),
                ],
            ),
            (2, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (3, vec![L], vec![(P, CanonicalScalar::Int(1))]),
            (4, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (5, vec![L], vec![]),
            (6, vec![M], vec![(SELECTED, CanonicalScalar::Int(0))]),
        ] {
            initial.create_vertex(VId(id), labels, props);
        }
        for (id, src, dst) in [
            (11, 1, 2),
            (12, 1, 2),
            (13, 1, 4),
            (14, 1, 1),
            (15, 3, 4),
            (16, 5, 2),
            (17, 1, 6),
        ] {
            initial.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let parameters = GqlParameters::new();
        let mut queries = Vec::new();
        for aggregate in [
            "sum(value)",
            "sum(DISTINCT value)",
            "avg(value)",
            "avg(DISTINCT value)",
            "min(value)",
            "max(value)",
        ] {
            let text = scalar_aggregates_text(aggregate);
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= (db.standing_queries.len() - first) as u64);
            assert!(
                db.standing_queries[first..]
                    .iter()
                    .any(|query| matches!(query, StandingQuery::Group(_)))
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        let nulls = null_summaries();
        let one = scalar_summaries(14, 10, 14.0 / 3.0, 5.0, 4, 6);
        let three = scalar_summaries(5, 5, 5.0, 5.0, 5, 5);
        let mut expected = scalar_aggregate_bags(&[
            (1, [one.clone(), one, nulls.clone()]),
            (3, [three.clone(), three.clone(), nulls.clone()]),
            (5, [nulls.clone(), nulls.clone(), nulls.clone()]),
        ]);
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..5 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    // Simultaneous argument replacement and witness retraction.
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.delete_edge(EId(13));
                    change.set_vertex_property(VId(4), SELECTED, None);
                }
                2 => {
                    change.set_vertex_property(VId(4), SELECTED, Some(CanonicalScalar::Int(1)));
                    change.add_edge(EId(21), VId(1), VId(4), vec![]);
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.delete_edge(EId(21));
                    // All-NULL input becomes empty without changing its NULL result.
                    change.delete_edge(EId(16));
                }
                4 => {
                    change.set_vertex_property(VId(6), P, Some(CanonicalScalar::Int(99)));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let one = match phase {
                0 => scalar_summaries(14, 14, 7.0, 7.0, 6, 8),
                1 => scalar_summaries(6, 6, 6.0, 6.0, 6, 6),
                2 => scalar_summaries(8, 8, 8.0, 8.0, 8, 8),
                _ => nulls.clone(),
            };
            let three = if phase == 1 {
                nulls.clone()
            } else {
                three.clone()
            };
            let next = scalar_aggregate_bags(&[
                (1, [one.clone(), one.clone(), one]),
                (3, [three.clone(), three, nulls.clone()]),
                (5, [nulls.clone(), nulls.clone(), nulls.clone()]),
            ]);
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

fn scalar_call_text(aggregate: &str) -> String {
    format!(
        "MATCH (a:L) UNWIND [7,7] AS token WITH token,a \
         CALL (a) {{ MATCH (a)-[:R]->(b:M) RETURN {aggregate} AS degree }} \
         RETURN token,a,degree"
    )
}

fn scalar_call_bag(values: [GraphValue; 3]) -> Bag {
    bag([1, 3, 5]
        .into_iter()
        .zip(values)
        .map(|(owner, value)| (row(vec![integer(7), vertex(Some(owner)), value]), 2)))
}

#[test]
fn filtered_extrema_restore_vertex_and_text_values_without_integer_coercion() {
    let ((), report) = run_async_under_lab(0x7361_7011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf4)).await.unwrap();
        let mut initial = seed();
        initial.add_edge(EId(15), VId(1), VId(4), vec![]);
        db.write(&commit, initial).await.unwrap();
        let mut queries = Vec::new();
        for aggregate in ["min(b)", "max(b)", "min(elementId(b))", "max(elementId(b))"] {
            let text = scalar_call_text(aggregate);
            let handle = db
                .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, policy())
                .unwrap();
            queries.push((text.clone(), bind(&text), handle));
        }
        let text_value =
            |value: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(value).unwrap());
        let null = GraphValue::Scalar(CanonicalScalar::Null);
        let mut expected = [
            scalar_call_bag([vertex(Some(2)), vertex(Some(4)), null.clone()]),
            scalar_call_bag([vertex(Some(4)), vertex(Some(4)), null.clone()]),
            scalar_call_bag([text_value("2"), text_value("4"), null.clone()]),
            scalar_call_bag([text_value("4"), text_value("4"), null.clone()]),
        ];
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..4 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => change.delete_edge(EId(11)),
                1 => change.delete_edge(EId(12)),
                2 => change.delete_edge(EId(15)),
                3 => change.delete_vertex(VId(4)),
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let (min, max) = match phase {
                0 => (Some(2), Some(4)),
                1 => (Some(4), Some(4)),
                _ => (None, None),
            };
            let other = if phase == 3 { None } else { Some(4) };
            let as_text = |id: Option<u128>| {
                id.map_or_else(|| null.clone(), |id| text_value(&id.to_string()))
            };
            let next = [
                scalar_call_bag([vertex(min), vertex(other), null.clone()]),
                scalar_call_bag([vertex(max), vertex(other), null.clone()]),
                scalar_call_bag([as_text(min), as_text(other), null.clone()]),
                scalar_call_bag([as_text(max), as_text(other), null.clone()]),
            ];
            for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&next)
            {
                check(&db, &cx, text, query, handle, after, policy());
                check_delta(&db, &cx, handle, from, before, after);
            }
            expected = next;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_scalar_aggregate_witnesses_preserve_final_quota_and_budget_atomicity() {
    let ((), report) = run_async_under_lab(0x7361_7012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf5)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![L], vec![(P, CanonicalScalar::Int(2))]);
        initial.create_vertex(VId(2), vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]);
        for edge in 10..74 {
            initial.add_edge(EId(edge), VId(1), VId(2), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let four = GqlQueryPolicy::new(1_000, 4, 50_000_000, 50_000_000);
        let value = scalar_summaries(256, 4, 4.0, 4.0, 4, 4);
        let nulls = null_summaries();
        let mut expected =
            scalar_aggregate_bags(&[(1, [value.clone(), value, nulls.clone()])]).to_vec();
        let mut queries = Vec::new();
        let mut statements: Vec<_> = [
            "sum(value)",
            "sum(DISTINCT value)",
            "avg(value)",
            "avg(DISTINCT value)",
            "min(value)",
            "max(value)",
        ]
        .into_iter()
        .map(scalar_aggregates_text)
        .collect();
        for profile in 0..3 {
            let value = if profile == 2 {
                mixed_values([1, 1, 1], Some((4, 4, 4.0, 4.0)), [Some(2), Some(2)])
            } else {
                mixed_values([64, 64, 1], Some((256, 4, 4.0, 4.0)), [Some(2), Some(2)])
            };
            expected.push(mixed_bag(&[(1, [value.clone(), value, mixed_empty()])]));
            statements.push(mixed_aggregates_text(profile));
        }
        for (index, text) in statements.into_iter().enumerate() {
            let query = bind(&text);
            let handle = db
                .register_standing_native(&cx, &text, &GqlParameters::new(), symbols, four)
                .unwrap();
            check(&db, &cx, &text, &query, &handle, &expected[index], four);
            let before = saved(&db.standing_queries);
            for refused in [
                GqlQueryPolicy::new(1_000, 4, 0, 50_000_000),
                GqlQueryPolicy::new(1_000, 4, 50_000_000, 0),
                GqlQueryPolicy::new(1_000, 3, 50_000_000, 50_000_000),
            ] {
                assert!(
                    db.register_standing_native(
                        &cx,
                        &text,
                        &GqlParameters::new(),
                        symbols,
                        refused
                    )
                    .is_err()
                );
                assert_eq!(saved(&db.standing_queries), before);
            }
            let too_few = GqlQueryPolicy::new(1_000, 3, 50_000_000, 50_000_000);
            assert!(db.standing_native_bag(&cx, &handle, too_few).is_err());
            assert_eq!(saved(&db.standing_queries), before);
            check(&db, &cx, &text, &query, &handle, &expected[index], four);
            queries.push((text, query, handle));
        }
        let from = db.frontier().unwrap();
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(VId(2), SELECTED, None);
        db.write(&commit, change).await.unwrap();
        let mut empty =
            scalar_aggregate_bags(&[(1, [nulls.clone(), nulls.clone(), nulls])]).to_vec();
        for _ in 0..3 {
            empty.push(mixed_bag(&[(
                1,
                [mixed_empty(), mixed_empty(), mixed_empty()],
            )]));
        }
        for (((text, query, handle), before), after) in queries.iter().zip(&expected).zip(&empty) {
            check(&db, &cx, text, query, handle, after, four);
            check_delta(&db, &cx, handle, from, before, after);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn filtered_summary_tuple_keeps_dormant_errors_private_and_failed_conversion_retryable() {
    let ((), report) = run_async_under_lab(0x7361_7013, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf6)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        initial.create_vertex(VId(1), vec![], vec![(P, CanonicalScalar::Int(i64::MAX))]);
        initial.create_vertex(VId(2), vec![M], vec![]);
        initial.add_edge(EId(11), VId(1), VId(2), vec![]);
        initial.add_edge(EId(12), VId(1), VId(2), vec![]);
        db.write(&commit, initial).await.unwrap();
        let text = concat!(
            "MATCH (a:L) WITH a,a.p AS value ",
            "CALL (a,value) { MATCH (a)-[:R]->(b:M) RETURN count(*) AS occurrences,sum(value) AS total } ",
            "RETURN a,occurrences,total"
        );
        let query = bind(text);
        let first = db.standing_queries.len();
        let handle = db
            .register_standing_native(&cx, text, &GqlParameters::new(), symbols, policy())
            .unwrap();
        check(&db, &cx, text, &query, &handle, &Bag::new(), policy());
        let group = (first..db.standing_queries.len())
            .find(|&index| matches!(db.standing_queries[index], StandingQuery::Group(_)))
            .unwrap();
        let old_frontier = db.standing_queries[group].status().1;
        let old_rows = sets::rows(&db.standing_queries[group])
            .unwrap()
            .checked_clone(LIMBS, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        let mut activate = WriteBatch::new(R);
        activate.set_vertex_label(VId(1), L, true);
        db.write(&commit, activate).await.unwrap();
        assert!(matches!(
            db.standing_queries[group].status().2,
            Some(StandingQueryFailure::OutputExpression {
                error: fgdb_gql::GraphIntegerError {
                    kind: fgdb_gql::GraphIntegerErrorKind::Overflow,
                    ..
                },
                ..
            })
        ));
        assert_eq!(db.standing_queries[group].status().1, old_frontier);
        assert_eq!(sets::rows(&db.standing_queries[group]).unwrap(), &old_rows);
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let before = saved(&db.standing_queries);
        let hidden_sum = concat!(
            "MATCH (a:L) WITH a,a.p AS value ",
            "CALL (a,value) { MATCH (a)-[:R]->(b:M) RETURN count(*) AS occurrences,sum(value) AS total } ",
            "RETURN a,occurrences"
        );
        for candidate in [
            text.to_owned(),
            format!("{text} ORDER BY total LIMIT 0"),
            hidden_sum.to_owned(),
            format!("{hidden_sum} LIMIT 0"),
        ] {
            assert!(
                db.execute_graph_set_governed(&cx, &bind(&candidate), policy())
                    .is_err()
            );
            assert!(
                db.register_standing_native(
                    &cx,
                    &candidate,
                    &GqlParameters::new(),
                    symbols,
                    policy()
                )
                .is_err()
            );
            assert_eq!(saved(&db.standing_queries), before);
        }
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(1)));
        db.write(&commit, repair).await.unwrap();
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let before = saved(&db.standing_queries);
        assert!(
            db.rebuild_standing_query(&cx, &handle, GqlQueryPolicy::new(1_000, 4, 0, 50_000_000))
                .is_err()
        );
        assert_eq!(saved(&db.standing_queries), before);
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        let two = bag([(row(vec![vertex(Some(1)), integer(2), integer(2)]), 1)]);
        check(&db, &cx, text, &query, &handle, &two, policy());
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(
            VId(1),
            P,
            Some(CanonicalScalar::ucs_basic_text("bad").unwrap()),
        );
        db.write(&commit, invalid).await.unwrap();
        assert!(matches!(
            db.standing_queries[group].status().2,
            Some(StandingQueryFailure::NonIntegerAggregate { .. })
        ));
        assert!(db.standing_native_query(&cx, &handle, policy()).is_err());
        let before = saved(&db.standing_queries);
        assert!(db.rebuild_standing_query(&cx, &handle, policy()).is_err());
        assert_eq!(saved(&db.standing_queries), before);
        let mut repair = WriteBatch::new(R);
        repair.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(0)));
        db.write(&commit, repair).await.unwrap();
        db.rebuild_standing_query(&cx, &handle, policy()).unwrap();
        let zero = bag([(row(vec![vertex(Some(1)), integer(2), integer(0)]), 1)]);
        check(&db, &cx, text, &query, &handle, &zero, policy());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn mixed_aggregates_text(profile: usize) -> String {
    let stages = match profile {
        0 => "WITH b,score+id(b) AS value ",
        1 => "WITH DISTINCT b,r,score WITH b,score+id(b) AS value ",
        2 => {
            "WITH DISTINCT b,r,score WITH b,score+id(b) AS value WITH DISTINCT b,value WHERE value IS NULL OR value <> 8 "
        }
        _ => unreachable!(),
    };
    format!(
        "MATCH (a:L) UNWIND [NULL,1,3,3] AS floor \
         WITH a,a.p AS score,toInteger(floor) AS floor \
         CALL (a,score,floor) {{ MATCH (a)-[r:R]->(b:M {{selected:1}}) \
         WHERE (floor IS NULL OR score >= floor) AND b <> a \
         {stages}RETURN count(*) AS occurrences,count(value) AS nonnull, \
         count(DISTINCT value) AS unique_values,sum(value) AS total, \
         sum(DISTINCT value) AS unique_total,avg(value) AS mean, \
         avg(DISTINCT value) AS unique_mean,min(b) AS least,max(b) AS greatest }} \
         RETURN a,floor,occurrences,nonnull,unique_values,total,unique_total,mean,unique_mean,least,greatest"
    )
}

// Format independently stated fixture results; no production aggregate kernel
// or query execution participates in the expected values.
fn mixed_values(
    counts: [i64; 3],
    numeric: Option<(i64, i64, f64, f64)>,
    extrema: [Option<u128>; 2],
) -> Vec<GraphValue> {
    let mut values: Vec<_> = counts.into_iter().map(integer).collect();
    if let Some((sum, distinct_sum, mean, distinct_mean)) = numeric {
        values.extend([
            integer(sum),
            integer(distinct_sum),
            floating(mean),
            floating(distinct_mean),
        ]);
    } else {
        values.extend((0..4).map(|_| vertex(None)));
    }
    values.extend(extrema.into_iter().map(vertex));
    values
}

fn mixed_empty() -> Vec<GraphValue> {
    mixed_values([0, 0, 0], None, [None, None])
}

fn mixed_bag(entries: &[(u128, [Vec<GraphValue>; 3])]) -> Bag {
    let mut result = Bag::new();
    for (owner, summaries) in entries {
        for ((floor, multiplicity), summary) in [(None, 1), (Some(1), 1), (Some(3), 2)]
            .into_iter()
            .zip(summaries)
        {
            let mut cells = vec![
                vertex(Some(*owner)),
                floor.map_or_else(|| vertex(None), integer),
            ];
            cells.extend(summary.iter().cloned());
            result.insert(row(cells), multiplicity);
        }
    }
    result
}

#[test]
fn mixed_aggregate_tuples_maintain_distinct_body_support_and_complete_deltas() {
    let ((), report) = run_async_under_lab(0x7361_7014, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let commit = c.commit();
        let mut db = Database::open_memory(&commit, keys(0xf7)).await.unwrap();
        let mut initial = WriteBatch::new(R);
        for (id, labels, props) in [
            (
                1,
                vec![L, M],
                vec![
                    (P, CanonicalScalar::Int(2)),
                    (SELECTED, CanonicalScalar::Int(1)),
                ],
            ),
            (2, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (3, vec![L], vec![(P, CanonicalScalar::Int(1))]),
            (4, vec![M], vec![(SELECTED, CanonicalScalar::Int(1))]),
            (5, vec![L], vec![]),
            (6, vec![M], vec![(SELECTED, CanonicalScalar::Int(0))]),
        ] {
            initial.create_vertex(VId(id), labels, props);
        }
        for (id, src, dst) in [
            (11, 1, 2),
            (12, 1, 2),
            (13, 1, 4),
            (14, 1, 1),
            (15, 3, 4),
            (16, 5, 2),
            (17, 1, 6),
        ] {
            initial.add_edge(EId(id), VId(src), VId(dst), vec![]);
        }
        db.write(&commit, initial).await.unwrap();
        let mut queries = Vec::new();
        for profile in 0..3 {
            let text = mixed_aggregates_text(profile);
            let parameters = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(&text, &parameters, symbols).unwrap();
            let (nodes, _) = prepared.subscription_footprint(&cx, &parameters).unwrap();
            let first = db.standing_queries.len();
            let handle = prepared
                .register_standing(&mut db, &cx, &parameters, policy())
                .unwrap();
            assert!(nodes >= u64::try_from(db.standing_queries.len() - first).unwrap());
            assert_eq!(
                db.standing_queries[first..]
                    .iter()
                    .filter(|query| matches!(query, StandingQuery::Group(_)))
                    .count(),
                1
            );
            queries.push((text.clone(), bind(&text), handle));
        }
        let three = mixed_values([1, 1, 1], Some((5, 5, 5.0, 5.0)), [Some(4), Some(4)]);
        let all_null = mixed_values([1, 0, 0], None, [Some(2), Some(2)]);
        let mut expected: Vec<_> = (0..3)
            .map(|profile| {
                let one = if profile == 2 {
                    mixed_values([2, 2, 2], Some((10, 10, 5.0, 5.0)), [Some(2), Some(4)])
                } else {
                    mixed_values(
                        [3, 3, 2],
                        Some((14, 10, 14.0 / 3.0, 5.0)),
                        [Some(2), Some(4)],
                    )
                };
                mixed_bag(&[
                    (1, [one.clone(), one, mixed_empty()]),
                    (3, [three.clone(), three.clone(), mixed_empty()]),
                    (5, [all_null.clone(), mixed_empty(), mixed_empty()]),
                ])
            })
            .collect();
        for ((text, query, handle), rows) in queries.iter().zip(&expected) {
            check(&db, &cx, text, query, handle, rows, policy());
        }
        for phase in 0..7 {
            let from = db.frontier().unwrap();
            let mut change = WriteBatch::new(R);
            match phase {
                0 => {
                    change.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(4)));
                    change.delete_edge(EId(11));
                }
                1 => {
                    change.delete_edge(EId(13));
                    change.set_vertex_property(VId(4), SELECTED, None);
                }
                2 => {
                    change.set_vertex_property(VId(4), SELECTED, Some(CanonicalScalar::Int(1)));
                    change.add_edge(EId(21), VId(1), VId(4), vec![]);
                    change.delete_edge(EId(12));
                }
                3 => {
                    change.delete_edge(EId(21));
                    change.delete_edge(EId(16));
                }
                4 => {
                    change.add_edge(EId(22), VId(1), VId(2), vec![]);
                    change.add_edge(EId(23), VId(1), VId(2), vec![]);
                }
                5 => {
                    change.delete_edge(EId(22));
                }
                6 => {
                    change.delete_edge(EId(23));
                }
                _ => unreachable!(),
            }
            db.write(&commit, change).await.unwrap();
            let next: Vec<_> = (0..3)
                .map(|profile| {
                    let one = if profile == 2 {
                        match phase {
                            0 | 1 | 4 | 5 => {
                                mixed_values([1, 1, 1], Some((6, 6, 6.0, 6.0)), [Some(2), Some(2)])
                            }
                            _ => mixed_empty(),
                        }
                    } else {
                        match phase {
                            0 => mixed_values(
                                [2, 2, 2],
                                Some((14, 14, 7.0, 7.0)),
                                [Some(2), Some(4)],
                            ),
                            1 | 5 => {
                                mixed_values([1, 1, 1], Some((6, 6, 6.0, 6.0)), [Some(2), Some(2)])
                            }
                            2 => {
                                mixed_values([1, 1, 1], Some((8, 8, 8.0, 8.0)), [Some(4), Some(4)])
                            }
                            4 => {
                                mixed_values([2, 2, 1], Some((12, 6, 6.0, 6.0)), [Some(2), Some(2)])
                            }
                            _ => mixed_empty(),
                        }
                    };
                    let three = if phase == 1 {
                        mixed_empty()
                    } else {
                        three.clone()
                    };
                    let five = if phase < 3 {
                        all_null.clone()
                    } else {
                        mixed_empty()
                    };
                    mixed_bag(&[
                        (1, [one.clone(), one.clone(), one]),
                        (3, [three.clone(), three, mixed_empty()]),
                        (5, [five, mixed_empty(), mixed_empty()]),
                    ])
                })
                .collect();
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
