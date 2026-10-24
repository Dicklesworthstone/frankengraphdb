//! openCypher list comprehensions and list quantifiers through `Database::query`
//! (fgdb-20foe). Every expectation below is computed by hand from the literal
//! lists and the fixture, not by a second query.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, QueryError, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x4c; 32],
        DatabaseSecurityNamespaceId([0x4d; 32]),
        [0x4e; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}
fn int(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn truth(value: Option<bool>) -> GraphValue {
    GraphValue::Scalar(value.map_or(CanonicalScalar::Null, CanonicalScalar::Bool))
}
fn list(values: impl IntoIterator<Item = GraphValue>) -> GraphValue {
    GraphValue::List(values.into_iter().collect())
}
fn ints(values: &[i64]) -> GraphValue {
    list(values.iter().copied().map(int))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}

/// Person vertices 1..=4 with p = 10, 20, 30, 40.
fn graph() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for id in 1..=4_i64 {
        batch.create_vertex(
            VId(id as u128),
            vec![PERSON],
            vec![(P, CanonicalScalar::Int(id * 10))],
        );
    }
    batch
}

fn run<T>(test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(test(&commit, &cx))
}

fn cells(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        return vec![vec![GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text("<not a row result>").unwrap(),
        )]];
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => GraphValue::Scalar(
                        CanonicalScalar::ucs_basic_text("<not a plain value>").unwrap(),
                    ),
                })
                .collect()
        })
        .collect()
}

#[test]
fn comprehensions_filter_and_project_under_three_valued_where() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let one = |text: &str| {
            cells(db.query(cx, text, &params, symbols, policy()).expect(text))
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
        };
        for (text, expected) in [
            // WHERE and | together, WHERE only, | only.
            (
                "RETURN [x IN [1, 2, 3, 4] WHERE x > 2 | x * 10] AS l",
                ints(&[30, 40]),
            ),
            ("RETURN [x IN [1, 2, 3] WHERE x <> 2] AS l", ints(&[1, 3])),
            ("RETURN [x IN [1, 2, 3] | x * x] AS l", ints(&[1, 4, 9])),
            // Only TRUE keeps an element: NULL > 1 is UNKNOWN.
            ("RETURN [x IN [1, null, 3] WHERE x > 1] AS l", ints(&[3])),
            // Elements pass through whole, lists included.
            ("RETURN [x IN [[1, 2], [3]] | size(x)] AS l", ints(&[2, 1])),
            // Nested scopes: the inner projection reads both elements.
            (
                "RETURN [x IN [1, 2] | [y IN [10, 20] | x + y]] AS l",
                list([ints(&[11, 21]), ints(&[12, 22])]),
            ),
            ("RETURN [x IN [] | x] AS l", ints(&[])),
            ("RETURN size([x IN [5, 6, 7] WHERE x >= 6]) AS n", int(2)),
            ("RETURN [x IN [1, 2, 3] | x][1] AS second", int(2)),
            // fgdb-o5o9t: size() of a list element in a predicate, and a map
            // element's entries.
            (
                "RETURN [x IN [[1, 2], [3]] WHERE size(x) > 1] AS l",
                list([ints(&[1, 2])]),
            ),
            (
                "RETURN [x IN [{p: 1}, {p: 2, q: 3}] WHERE x.p > 1 | x.q] AS l",
                ints(&[3]),
            ),
        ] {
            assert_eq!(one(text), Some(expected), "{text}");
        }
        // A NULL list is NULL, not an empty list.
        assert_eq!(one("RETURN [x IN null | x] AS l"), Some(null()));
    });
}

#[test]
fn quantifiers_are_three_valued_over_their_elements() {
    run(async |commit, cx| {
        let db = Database::open_memory(commit, keys()).await.unwrap();
        let params = GqlParameters::new();
        for (text, expected) in [
            ("RETURN any(x IN [1, 2, 3] WHERE x > 2) AS b", Some(true)),
            ("RETURN any(x IN [1, 2, 3] WHERE x > 5) AS b", Some(false)),
            ("RETURN any(x IN [1, null] WHERE x > 1) AS b", None),
            ("RETURN any(x IN [2, null] WHERE x > 1) AS b", Some(true)),
            ("RETURN all(x IN [1, 2, 3] WHERE x > 0) AS b", Some(true)),
            ("RETURN all(x IN [1, 2, 3] WHERE x > 1) AS b", Some(false)),
            ("RETURN all(x IN [1, null] WHERE x > 0) AS b", None),
            ("RETURN all(x IN [] WHERE x > 0) AS b", Some(true)),
            ("RETURN none(x IN [1, 2] WHERE x > 5) AS b", Some(true)),
            ("RETURN none(x IN [1, 7] WHERE x > 5) AS b", Some(false)),
            ("RETURN single(x IN [1, 2, 3] WHERE x = 2) AS b", Some(true)),
            (
                "RETURN single(x IN [1, 2, 2] WHERE x = 2) AS b",
                Some(false),
            ),
            ("RETURN single(x IN [2, null] WHERE x = 2) AS b", None),
            // Two TRUEs decide single() whatever the UNKNOWN element is.
            (
                "RETURN single(x IN [2, 2, null] WHERE x = 2) AS b",
                Some(false),
            ),
            ("RETURN single(x IN [] WHERE x = 2) AS b", Some(false)),
        ] {
            let rows = cells(db.query(cx, text, &params, symbols, policy()).expect(text));
            assert_eq!(rows, vec![vec![truth(expected)]], "{text}");
        }
    });
}

#[test]
fn elements_shadow_graph_names_and_read_the_enclosing_row() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let query = |text: &str| cells(db.query(cx, text, &params, symbols, policy()).expect(text));
        // The projection reads the matched vertex's property from the row.
        assert_eq!(
            query("MATCH (n:Person) RETURN n.p AS v, [x IN [1, 2] | x + n.p] AS l ORDER BY v"),
            [10, 20, 30, 40]
                .iter()
                .map(|v| vec![int(*v), ints(&[v + 1, v + 2])])
                .collect::<Vec<_>>()
        );
        // An element named like a pattern variable shadows it inside its scope.
        assert_eq!(
            query("MATCH (n:Person) WHERE n.p = 20 RETURN [n IN [1, 2] | n * 2] AS l"),
            vec![vec![ints(&[2, 4])]]
        );
        // A WITH-carried collected list, filtered after the grouping.
        assert_eq!(
            query(
                "MATCH (n:Person) WITH collect(n.p) AS ps RETURN size([v IN ps WHERE v > 15]) AS k"
            ),
            vec![vec![int(3)]]
        );
        assert_eq!(
            query(
                "MATCH (n:Person) WITH collect(n.p) AS ps \
                 RETURN any(v IN ps WHERE v = 40) AS has, all(v IN ps WHERE v >= 10) AS every"
            ),
            vec![vec![truth(Some(true)), truth(Some(true))]]
        );
        // The same scopes now run directly over completed aggregate outputs.
        // Their local names remain separate from the graph binding and each
        // following RETURN item.
        assert_eq!(
            query(
                "MATCH (n:Person) RETURN [n IN collect(n.p) WHERE n > 15 | n + 1] AS l, reduce(total=0, x IN collect(n.p) | total+x) AS total, any(x IN collect(n.p) WHERE x=40) AS has"
            ),
            vec![vec![ints(&[21, 31, 41]), int(100), truth(Some(true))]]
        );
    });
}

#[test]
fn out_of_profile_comprehensions_refuse_typed() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        for text in [
            // Aggregates may feed an element scope; an aggregate nested
            // inside another aggregate's argument is still invalid.
            "MATCH (n:Person) RETURN [x IN collect(sum(n.p)) | x] AS l",
            // A quantifier needs its WHERE predicate.
            "RETURN any(x IN [1, 2]) AS b",
            // An element is not visible outside its brackets.
            "RETURN [x IN [1] | x] AS l, x AS y",
        ] {
            let result = db.query(cx, text, &params, symbols, policy());
            assert!(
                matches!(result, Err(QueryError::Refused { .. })),
                "{text}: {result:?}"
            );
        }
        // Only a map element has entries (fgdb-o5o9t): `x.p` over an Int is a
        // typed evaluation error, never a read of a same-named pattern
        // variable or of storage.
        let result = db.query(
            cx,
            "MATCH (n:Person) RETURN [x IN [1] | x.p] AS l",
            &params,
            symbols,
            policy(),
        );
        assert!(result.is_err(), "{result:?}");
        // A non-list is never comprehended as if it were one.
        let result = db.query(cx, "RETURN [x IN 3 | x] AS l", &params, symbols, policy());
        assert!(result.is_err(), "{result:?}");
        // A non-Boolean WHERE is an expression error, never a silent filter.
        let result = db.query(
            cx,
            "RETURN [x IN [1, 2] WHERE x + 1] AS l",
            &params,
            symbols,
            policy(),
        );
        assert!(result.is_err(), "{result:?}");
    });
}

#[test]
fn list_functions_slices_ranges_and_reduce_follow_opencypher() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        let one = |text: &str| {
            cells(db.query(cx, text, &params, symbols, policy()).expect(text))
                .into_iter()
                .next()
                .and_then(|row| row.into_iter().next())
        };
        for (text, expected) in [
            // head/last index the ends; tail drops the first member.
            ("RETURN head([1, 2, 3]) AS v", int(1)),
            ("RETURN head([]) AS v", null()),
            ("RETURN last([1, 2, 3]) AS v", int(3)),
            ("RETURN last([]) AS v", null()),
            ("RETURN tail([1, 2, 3]) AS v", ints(&[2, 3])),
            ("RETURN tail([]) AS v", ints(&[])),
            // Slices: bounds clamp, negatives count from the end.
            ("RETURN [1, 2, 3, 4][1..3] AS v", ints(&[2, 3])),
            ("RETURN [1, 2, 3, 4][..2] AS v", ints(&[1, 2])),
            ("RETURN [1, 2, 3, 4][2..] AS v", ints(&[3, 4])),
            ("RETURN [1, 2, 3, 4][-2..] AS v", ints(&[3, 4])),
            ("RETURN [1, 2, 3][..-1] AS v", ints(&[1, 2])),
            ("RETURN [1, 2, 3][5..9] AS v", ints(&[])),
            ("RETURN [1, 2, 3][2..1] AS v", ints(&[])),
            // Subscripts compose with scalar expressions through the normal
            // Database query path, including carried rows and element locals.
            ("WITH [1, 2, 3] AS items RETURN items[0] + 10 AS v", int(11)),
            (
                "MATCH (n:Person) WHERE n.p = 20 WITH [n.p, n.p + 1] AS items \
                 RETURN items[0] + items[1] AS v",
                int(41),
            ),
            (
                "RETURN [items IN [[1, 2], [3, 4]] | items[0] + items[1]] AS v",
                ints(&[3, 7]),
            ),
            (
                "WITH [1, 2, 3] AS items \
                 RETURN CASE WHEN true THEN items[1] + 10 ELSE 0 END AS v",
                int(12),
            ),
            // A NULL bound is NULL. (A bare `null..` reads as a property
            // access in the shared operand parser, so the bound is wrapped.)
            ("RETURN [1, 2, 3][(null)..2] AS v", null()),
            // range is end-inclusive in its step's direction.
            ("RETURN range(1, 4) AS v", ints(&[1, 2, 3, 4])),
            ("RETURN range(0, 10, 3) AS v", ints(&[0, 3, 6, 9])),
            ("RETURN range(5, 1, -2) AS v", ints(&[5, 3, 1])),
            ("RETURN range(1, 0) AS v", ints(&[])),
            ("RETURN range(3, 3, -1) AS v", ints(&[3])),
            // reduce folds left from init; NULL list is NULL, empty is init.
            ("RETURN reduce(s = 0, x IN [1, 2, 3] | s + x) AS v", int(6)),
            ("RETURN reduce(s = 1, x IN [2, 3, 4] | s * x) AS v", int(24)),
            ("RETURN reduce(s = 7, x IN [] | s + x) AS v", int(7)),
            ("RETURN reduce(s = 0, x IN null | s + x) AS v", null()),
            // A list element is whole at value level: the last step is
            // size([3]). Inside arithmetic, size() of a list element is a let
            // slot (fgdb-o5o9t), and the accumulator stays in reach: 2 + 1.
            (
                "RETURN reduce(s = 0, x IN [[1, 2], [3]] | size(x)) AS v",
                int(1),
            ),
            (
                "RETURN reduce(s = 0, x IN [[1, 2], [3]] | s + size(x)) AS v",
                int(3),
            ),
            // Order matters: 10*(10*0+1)+2 = 12, not 21.
            (
                "RETURN reduce(s = 0, x IN [1, 2] | s * 10 + x) AS v",
                int(12),
            ),
        ] {
            assert_eq!(one(text), Some(expected), "{text}");
        }
        // Over rows and aggregate outputs.
        assert_eq!(
            one("MATCH (n:Person) WHERE n.p = 20 RETURN range(n.p, n.p + 2) AS v"),
            Some(ints(&[20, 21, 22]))
        );
        assert_eq!(
            one("MATCH (n:Person) RETURN size(collect(n.p)[1..]) AS v"),
            Some(int(3))
        );
        assert_eq!(
            one("MATCH (n:Person) RETURN range(1, count(*)) AS v"),
            Some(ints(&[1, 2, 3, 4]))
        );
        // A zero step and a non-integer bound are typed errors.
        for text in ["RETURN range(1, 3, 0) AS v", "RETURN [1, 2][1..'x'] AS v"] {
            let result = db.query(cx, text, &params, symbols, policy());
            assert!(result.is_err(), "{text}: {result:?}");
        }
    });
}

/// fgdb-1c29d / fgdb-klah2: a MATCH WHERE over list values before an
/// aggregating RETURN is the implicit `WITH *` then that WHERE, as before a
/// plain RETURN, so it filters the rows the aggregates read.
#[test]
fn a_where_over_values_filters_before_an_aggregating_return() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        for (text, expected) in [
            (
                "MATCH (n:Person) WHERE any(x IN [n.p] WHERE x > 15) RETURN count(*) AS v",
                QueryValue::Count(3),
            ),
            (
                "MATCH (n:Person) WHERE any(x IN [n.p] WHERE x > 15) RETURN sum(n.p) AS v",
                QueryValue::Integer(90),
            ),
            (
                "MATCH (n:Person) WITH * WHERE any(x IN [n.p] WHERE x > 15) \
                 RETURN sum(n.p) AS v",
                QueryValue::Integer(90),
            ),
        ] {
            let result = db.query(cx, text, &params, symbols, policy()).expect(text);
            let QueryResult::Rows { rows, .. } = result else {
                unreachable!("an aggregating RETURN answers rows")
            };
            assert_eq!(rows, vec![vec![expected]], "{text}");
        }
    });
}

/// fgdb-7nwox (TCK Pattern2, List6 [7]-[10]): a pattern comprehension is,
/// per outer row, the list of its projection over every witness of its
/// pattern, which correlates on the row's bindings. No witness is the empty
/// list, never NULL; an inner WHERE filters; duplicate outer rows each keep
/// their own list. Edges 1->2, 1->3, 2->3 over the Person fixture.
#[test]
fn pattern_comprehensions_collect_each_rows_witnesses() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        let mut batch = graph();
        for (eid, from, to) in [(10, 1, 2), (11, 1, 3), (12, 2, 3)] {
            batch.add_edge(fgdb_types::EId(eid), VId(from), VId(to), vec![]);
        }
        db.write(commit, batch).await.unwrap();
        let params = GqlParameters::new();
        let rows = |text: &str| cells(db.query(cx, text, &params, symbols, policy()).expect(text));
        assert_eq!(
            rows("MATCH (n:Person) RETURN n.p AS p, [(n)-[:R]->(m) | m.p] AS out ORDER BY p"),
            vec![
                vec![int(10), ints(&[20, 30])],
                vec![int(20), ints(&[30])],
                vec![int(30), ints(&[])],
                vec![int(40), ints(&[])],
            ]
        );
        // A literal projection collects one value per witness: the degree.
        assert_eq!(
            rows("MATCH (n:Person) RETURN n.p AS p, size([(n)-[:R]->() | 1]) AS d ORDER BY p"),
            vec![
                vec![int(10), int(2)],
                vec![int(20), int(1)],
                vec![int(30), int(0)],
                vec![int(40), int(0)],
            ]
        );
        for (text, expected) in [
            // An inner WHERE filters the witnesses.
            (
                "MATCH (n:Person {p: 10}) RETURN [(n)-[:R]->(m) WHERE m.p > 25 | m.p] AS v",
                ints(&[30]),
            ),
            // The path and the relationship are elements too.
            (
                "MATCH (n:Person {p: 10}) RETURN size([q = (n)-[:R]->() | q]) AS v",
                int(2),
            ),
            (
                "MATCH (n:Person {p: 10}) RETURN size([(n)-[r:R]->() | r]) AS v",
                int(2),
            ),
            // The projection may also read the outer row.
            (
                "MATCH (n:Person {p: 20}) RETURN [(n)-[:R]->(m) | m.p - n.p] AS v",
                ints(&[10]),
            ),
        ] {
            assert_eq!(rows(text), vec![vec![expected]], "{text}");
        }
        // Each of the two outer rows per vertex keeps its own list.
        assert_eq!(
            rows(
                "MATCH (n:Person), (k:Person) WHERE n.p = 20 AND k.p <= 20 \
                 RETURN k.p AS k, [(n)-[:R]->(m) | m.p] AS out ORDER BY k"
            ),
            vec![vec![int(10), ints(&[30])], vec![int(20), ints(&[30])],]
        );
        // Two pattern locals in one projection, an aggregate argument, a row
        // scope, and a pattern over an enclosing list-comprehension local
        // (gathered once per row, so it could not follow the local; TCK
        // Pattern2 [7]) refuse typed.
        for text in [
            "MATCH (n:Person) RETURN [(n)-[:R]->(m)-[:R]->(o) | m.p + o.p] AS v",
            "MATCH (n:Person) RETURN count([(n)-[:R]->() | 1]) AS v",
            "WITH 1 AS x RETURN [(x)-[:R]->() | 1] AS v",
            "MATCH p = (n:Person)-[:R]->() RETURN [x IN nodes(p) | size([(x)-[:R]->() | 1])] AS v",
        ] {
            let result = db.query(cx, text, &params, symbols, policy());
            assert!(result.is_err(), "{text}: {result:?}");
        }
    });
}
