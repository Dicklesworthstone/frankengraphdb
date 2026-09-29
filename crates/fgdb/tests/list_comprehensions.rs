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
    });
}

#[test]
fn out_of_profile_comprehensions_refuse_typed() {
    run(async |commit, cx| {
        let mut db = Database::open_memory(commit, keys()).await.unwrap();
        db.write(commit, graph()).await.unwrap();
        let params = GqlParameters::new();
        for text in [
            // A property read on an element needs storage access in the scope.
            "MATCH (n:Person) RETURN [x IN [1] | x.p] AS l",
            // The aggregate-output evaluator binds no element scope.
            "MATCH (n:Person) RETURN [x IN collect(n.p) WHERE x > 10] AS l",
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
        // Predicates run on the scalar expression VM: size() there is text
        // CHAR_LENGTH, so a list element in a predicate is a typed error.
        let result = db.query(
            cx,
            "RETURN [x IN [[1, 2], [3]] WHERE size(x) > 1] AS l",
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
            // size([3]). (Inside arithmetic, size() is the scalar VM's text
            // length, so `s + size(x)` over lists is a typed error.)
            (
                "RETURN reduce(s = 0, x IN [[1, 2], [3]] | size(x)) AS v",
                int(1),
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
