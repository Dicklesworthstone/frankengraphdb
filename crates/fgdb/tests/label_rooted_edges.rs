//! A label-rooted single-edge read such as `(:A)-[r:R]->(:B)` admits its
//! edges through the bound root lookup (fgdb-61p1n). The answers are held to
//! an oracle this file computes from what it wrote: forward and reversed
//! lookups, the undirected and multi-hop shapes that still dedup, parallel
//! edges, unlabelled endpoints, a second relation and a deleted edge.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const A: LabelId = LabelId(1);
const B: LabelId = LabelId(2);
const W: PropertyKeyId = PropertyKeyId(1);

/// Every R edge's weight, read back through the admitted edge table.
fn weight(eid: u128) -> CanonicalScalar {
    CanonicalScalar::Int(10 * eid as i64)
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x5a; 32],
        DatabaseSecurityNamespaceId([0x5b; 32]),
        [0x5c; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "A") => Some(GraphSymbol::Label(A)),
        (GraphSymbolKind::Label, "B") => Some(GraphSymbol::Label(B)),
        (GraphSymbolKind::Property, "w") => Some(GraphSymbol::Property(W)),
        _ => None,
    }
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 100_000, 100_000_000, 10_000_000)
}

fn run<T>(test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(test(&commit, &cx))
}

/// Vertices 1-4 are A, 3-8 are B (3 and 4 are both), 9-12 have no label.
fn labels(vid: u128) -> Vec<LabelId> {
    let mut labels = Vec::new();
    if (1..=4).contains(&vid) {
        labels.push(A);
    }
    if (3..=8).contains(&vid) {
        labels.push(B);
    }
    labels
}

/// R edges, live at the end. Parallel 1->5 twice, A->A, B->A, unlabelled
/// endpoints on either side, and a pair between two vertices that are both A
/// and B. Edge 99 is written and then deleted.
const R_EDGES: &[(u128, u128, u128)] = &[
    (1, 1, 5),
    (2, 1, 5),
    (3, 1, 2),
    (4, 2, 6),
    (5, 3, 4),
    (6, 4, 3),
    (7, 5, 1),
    (8, 9, 5),
    (9, 1, 10),
    (10, 4, 8),
    (11, 6, 7),
    (12, 2, 3),
];
/// S edges share endpoints with R, so a relation filter is what keeps them
/// out of every R answer; vertex 5 has a self loop.
const S_EDGES: &[(u128, u128, u128)] =
    &[(20, 1, 5), (21, 5, 5), (22, 5, 6), (23, 3, 8), (24, 6, 1)];

fn is(label: LabelId, vid: u128) -> bool {
    labels(vid).contains(&label)
}

async fn open(commit: &fgdb_types::CommitCx) -> Database<MemVfs> {
    open_with(commit, true).await
}

async fn open_with(commit: &fgdb_types::CommitCx, with_s: bool) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(commit, keys())
        .await
        .unwrap();
    let mut vertices = WriteBatch::new(R);
    for vid in 1..=12u128 {
        vertices.create_vertex(VId(vid), labels(vid), vec![]);
    }
    db.write(commit, vertices).await.unwrap();
    let mut r = WriteBatch::new(R);
    for &(eid, src, dst) in R_EDGES {
        r.add_edge(EId(eid), VId(src), VId(dst), vec![(W, weight(eid))]);
    }
    r.add_edge(EId(99), VId(2), VId(7), vec![]);
    db.write(commit, r).await.unwrap();
    if with_s {
        let mut s = WriteBatch::new(S);
        for &(eid, src, dst) in S_EDGES {
            s.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        }
        db.write(commit, s).await.unwrap();
    }
    let mut retire = WriteBatch::new(R);
    retire.delete_edge(EId(99));
    db.write(commit, retire).await.unwrap();
    db
}

fn cells(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<QueryValue>> {
    let QueryResult::Rows { rows, .. } = db
        .query(cx, text, &GqlParameters::new(), symbols, policy())
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
    else {
        panic!("{text}: not a row result");
    };
    rows
}

fn query(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> Vec<Vec<GraphValue>> {
    cells(db, cx, text)
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|cell| match cell {
                    QueryValue::Value(value) => value,
                    _ => panic!("{text}: a cell that is not a plain value"),
                })
                .collect()
        })
        .collect()
}

fn count(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> u64 {
    match cells(db, cx, text).as_slice() {
        [row] => match row.as_slice() {
            [cell] => cell
                .as_count()
                .unwrap_or_else(|| panic!("{text}: not a count")),
            _ => panic!("{text}: not one column"),
        },
        _ => panic!("{text}: not one row"),
    }
}

#[test]
fn label_rooted_edge_reads_answer_exactly_what_was_written() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let forward: Vec<(u128, u128, u128)> = R_EDGES
            .iter()
            .filter(|&&(_, src, dst)| is(A, src) && is(B, dst))
            .copied()
            .collect();
        assert!(forward.len() >= 6, "the fixture exercises several roots");
        assert_eq!(
            count(&db, cx, "MATCH (:A)-[r:R]->(:B) RETURN count(*) AS c"),
            forward.len() as u64
        );
        assert_eq!(
            count(&db, cx, "MATCH (:B)<-[r:R]-(:A) RETURN count(*) AS c"),
            forward.len() as u64
        );
        // Every occurrence, parallel edges included, with each edge's own
        // weight: a property read finds its edge in the admitted table.
        let mut expected: Vec<Vec<GraphValue>> = forward
            .iter()
            .map(|&(eid, src, dst)| {
                vec![
                    GraphValue::Vertex(VId(src)),
                    GraphValue::Vertex(VId(dst)),
                    GraphValue::Scalar(weight(eid)),
                ]
            })
            .collect();
        expected.sort();
        assert_eq!(
            query(
                &db,
                cx,
                "MATCH (a:A)-[r:R]->(b:B) RETURN a, b, r.w AS w ORDER BY a, b, w"
            ),
            expected
        );
        // Undirected: each orientation whose ends carry the labels, and the
        // edges between two vertices that are both A and B match both ways.
        let undirected = R_EDGES
            .iter()
            .map(|&(_, src, dst)| {
                usize::from(is(A, src) && is(B, dst)) + usize::from(is(A, dst) && is(B, src))
            })
            .sum::<usize>();
        assert_eq!(
            count(&db, cx, "MATCH (:A)-[r:R]-(:B) RETURN count(*) AS c"),
            undirected as u64
        );
        // One hop further keeps the root map and its frontier.
        let hop = R_EDGES
            .iter()
            .filter(|&&(_, src, _)| is(A, src))
            .map(|&(_, _, mid)| S_EDGES.iter().filter(|&&(_, from, _)| from == mid).count())
            .sum::<usize>();
        assert_eq!(
            count(
                &db,
                cx,
                "MATCH (:A)-[:R]->(b)-[:S]->(c) RETURN count(*) AS c"
            ),
            hop as u64
        );
    });
}

/// fgdb-qo3jv: a label test under OR, NOT or parentheses in a MATCH WHERE is
/// the Boolean `'L' IN labels(v)`, and a relationship's `r:T` is
/// `type(r) = 'T'`, both read by the row filter after the match. Their rows
/// are the ones a three-valued evaluation of what was written keeps, the
/// rows the spelled-out forms return, and an OR of two labels is the union of
/// the two inline labels. A label no catalog names is simply absent. A NULL
/// element from OPTIONAL MATCH makes the test NULL, so neither it nor its
/// negation keeps the row. A WHERE so deferred before a later OPTIONAL MATCH
/// filters that clause's input (TCK Match7 [25]).
#[test]
fn label_tests_under_or_and_not_are_three_valued_label_membership() {
    run(async |commit, cx| {
        let db = open(commit).await;
        let vertex = |value: &GraphValue| match value {
            GraphValue::Vertex(VId(vid)) => *vid,
            other => panic!("not a vertex: {other:?}"),
        };
        let vertices = |text: &str| {
            let mut found: Vec<u128> = query(&db, cx, text)
                .iter()
                .map(|row| vertex(&row[0]))
                .collect();
            found.sort_unstable();
            found
        };
        // Each case: the label test, its spelled-out form, and which written
        // vertices (or relationships) it keeps.
        type KeepVertex = fn(u128) -> bool;
        type KeepEdge = fn(RelationId, u128) -> bool;
        let cases: [(&str, &str, KeepVertex); 6] = [
            ("a:A OR a:B", "'A' IN labels(a) OR 'B' IN labels(a)", |v| {
                is(A, v) || is(B, v)
            }),
            ("NOT a:B", "NOT 'B' IN labels(a)", |v| !is(B, v)),
            ("NOT (a:A)", "NOT ('A' IN labels(a))", |v| !is(A, v)),
            (
                "(a:A) AND NOT a:B",
                "'A' IN labels(a) AND NOT 'B' IN labels(a)",
                |v| is(A, v) && !is(B, v),
            ),
            (
                "a:A:B OR NOT (a:A OR a:B)",
                "'A' IN labels(a) AND 'B' IN labels(a) \
                 OR NOT ('A' IN labels(a) OR 'B' IN labels(a))",
                |v| (is(A, v) && is(B, v)) || !(is(A, v) || is(B, v)),
            ),
            (
                "a:Z OR NOT a:Z",
                "'Z' IN labels(a) OR NOT 'Z' IN labels(a)",
                |_| true,
            ),
        ];
        for (test, spelled, keep) in cases {
            let expected: Vec<u128> = (1..=12).filter(|&vid| keep(vid)).collect();
            let text = format!("MATCH (a) WHERE {test} RETURN a");
            assert_eq!(vertices(&text), expected, "{text}");
            let text = format!("MATCH (a) WHERE {spelled} RETURN a");
            assert_eq!(vertices(&text), expected, "{text}");
            let text = format!("MATCH (a) WHERE {test} RETURN count(*) AS c");
            assert_eq!(count(&db, cx, &text), expected.len() as u64, "{text}");
        }
        let mut union = vertices("MATCH (a:A) RETURN a");
        union.extend(vertices("MATCH (a:B) RETURN a"));
        union.sort_unstable();
        union.dedup();
        assert_eq!(vertices("MATCH (a) WHERE a:A OR a:B RETURN a"), union);
        // Relationship tests, over both relations' live edges.
        let edges: Vec<(RelationId, u128, u128)> = (R_EDGES.iter().map(|&(_, s, d)| (R, s, d)))
            .chain(S_EDGES.iter().map(|&(_, s, d)| (S, s, d)))
            .collect();
        let relationship_cases: [(&str, &str, KeepEdge); 2] = [
            (
                "r:S OR b:A",
                "type(r) = 'S' OR 'A' IN labels(b)",
                |relation, dst| relation == S || is(A, dst),
            ),
            (
                "NOT r:R AND NOT (b:B)",
                "NOT type(r) = 'R' AND NOT ('B' IN labels(b))",
                |relation, dst| relation != R && !is(B, dst),
            ),
        ];
        for (test, spelled, keep) in relationship_cases {
            let mut expected: Vec<(u128, u128)> = (edges.iter())
                .filter(|&&(relation, _, dst)| keep(relation, dst))
                .map(|&(_, src, dst)| (src, dst))
                .collect();
            expected.sort_unstable();
            for predicate in [test, spelled] {
                let text = format!("MATCH (a)-[r]->(b) WHERE {predicate} RETURN a, b");
                let mut found: Vec<(u128, u128)> = query(&db, cx, &text)
                    .iter()
                    .map(|row| (vertex(&row[0]), vertex(&row[1])))
                    .collect();
                found.sort_unstable();
                assert_eq!(found, expected, "{text}");
            }
        }
        // TCK Match7 [25]: the filter runs before the OPTIONAL MATCH, which
        // finds the one self-loop (S 5->5) among the vertices that are not A.
        for predicate in ["NOT (a:A)", "NOT ('A' IN labels(a))"] {
            let text = format!(
                "MATCH (a) WHERE {predicate} OPTIONAL MATCH (a)-[r]->(a) \
                 RETURN a, r IS NULL AS missing"
            );
            let mut found: Vec<(u128, bool)> = query(&db, cx, &text)
                .iter()
                .map(|row| match &row[1] {
                    GraphValue::Scalar(CanonicalScalar::Bool(missing)) => {
                        (vertex(&row[0]), *missing)
                    }
                    other => panic!("{text}: {other:?}"),
                })
                .collect();
            found.sort_unstable();
            let expected: Vec<(u128, bool)> = (5..=12).map(|vid| (vid, vid != 5)).collect();
            assert_eq!(found, expected, "{text}");
        }
        // A NULL element: every A vertex with its S successor, if any (1->5,
        // 3->8; 2 and 4 have none), filtered three-valued. Each case reads
        // (a, whether b is B, whether b is A), NULL for a NULL b.
        fn or(x: Option<bool>, y: Option<bool>) -> Option<bool> {
            match (x, y) {
                (Some(true), _) | (_, Some(true)) => Some(true),
                (Some(false), Some(false)) => Some(false),
                _ => None,
            }
        }
        fn not(x: Option<bool>) -> Option<bool> {
            x.map(|truth| !truth)
        }
        let successors: [(u128, Option<u128>); 4] =
            [(1, Some(5)), (2, None), (3, Some(8)), (4, None)];
        type Keep = fn(u128, Option<bool>, Option<bool>) -> Option<bool>;
        let null_cases: [(&str, &str, Keep); 3] = [
            (
                "b:B OR NOT b:A",
                "'B' IN labels(b) OR NOT 'A' IN labels(b)",
                |_, b, a| or(b, not(a)),
            ),
            ("NOT b:B", "NOT 'B' IN labels(b)", |_, b, _| not(b)),
            (
                "b:B OR a:A",
                "'B' IN labels(b) OR 'A' IN labels(a)",
                |a, b, _| or(b, Some(is(A, a))),
            ),
        ];
        for (test, spelled, keep) in null_cases {
            let mut expected: Vec<(u128, bool)> = (successors.iter())
                .filter(|&&(a, b)| {
                    let label = |of| b.map(|b| is(of, b));
                    keep(a, label(B), label(A)) == Some(true)
                })
                .map(|&(a, b)| (a, b.is_none()))
                .collect();
            expected.sort_unstable();
            for predicate in [test, spelled] {
                let text = format!(
                    "MATCH (a:A) OPTIONAL MATCH (a)-[:S]->(b) WITH a, b \
                     WHERE {predicate} RETURN a, b IS NULL AS missing"
                );
                let mut found: Vec<(u128, bool)> = query(&db, cx, &text)
                    .iter()
                    .map(|row| match &row[1] {
                        GraphValue::Scalar(CanonicalScalar::Bool(missing)) => {
                            (vertex(&row[0]), *missing)
                        }
                        other => panic!("{text}: {other:?}"),
                    })
                    .collect();
                found.sort_unstable();
                assert_eq!(found, expected, "{text}");
            }
        }
    });
}

/// The smallest snapshot-record budget `text` is admitted under.
fn least_records(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> u64 {
    let admitted = |records| {
        db.query(
            cx,
            text,
            &GqlParameters::new(),
            symbols,
            GqlQueryPolicy::new(records, 100_000, 100_000_000, 10_000_000),
        )
        .is_ok()
    };
    let (mut low, mut high) = (0, 10_000);
    assert!(admitted(high), "{text} is admitted under a wide budget");
    while low < high {
        let middle = low + (high - low) / 2;
        if admitted(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

/// A label-rooted read admits only its own relation's edges: another
/// relation's edges between the same vertices cost it no snapshot record.
#[test]
fn another_relation_between_the_same_vertices_costs_a_label_rooted_read_nothing() {
    run(async |commit, cx| {
        let text = "MATCH (:A)-[r:R]->(:B) RETURN count(*) AS c";
        let with = open_with(commit, true).await;
        let without = open_with(commit, false).await;
        assert_eq!(count(&with, cx, text), count(&without, cx, text));
        let least = least_records(&without, cx, text);
        assert!(least > 0);
        assert_eq!(least_records(&with, cx, text), least);
    });
}
