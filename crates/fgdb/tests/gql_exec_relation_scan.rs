//! **The MATCH scan is a relation-edge scan, not a vertex sweep**
//! (`fgdb-w4-g1-txn-core-qpmg.16`).
//!
//! Three shapes the pinned `MATCH (a)-[:R]->(b) RETURN b` scan must get
//! right, each aimed at a different lazy kernel: an isolate vertex must
//! contribute nothing (a kernel that emits every vertex it visits fails);
//! edges in ANOTHER relation must contribute nothing even when the bound
//! relation has answers elsewhere (a kernel that matches any edge shape
//! fails — this is the in-statement sibling of `min_gql_match`'s
//! off-relation bind test, here with both relations live in one graph); and
//! an empty graph is `Ok(vec![])`, never an error and never a phantom row.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-rel-scan-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// An isolate vertex contributes nothing: one `:R` edge and one edgeless
/// vertex yield exactly the edge's destination — a kernel that emits
/// visited vertices instead of edge destinations returns the isolate too
/// and fails.
#[test]
fn an_isolate_vertex_is_not_a_destination() {
    under_lab(0x5c_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("isolate");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(9), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.expect("commits");

        let rows = db.execute_gql(PINNED, &bind_r()).expect("executes");
        assert_eq!(
            rows,
            vec![VId(2)],
            "only the edge's destination answers; the isolate VId(9) is no \
             destination of anything"
        );
    });
}

/// Both relations live in one graph: the `:R` matches come back complete
/// and CGSE-sorted (written descending so ascending output is a sort), and
/// the `:OTHER` edge's destination is absent even though its SOURCE also
/// carries an `:R` edge — the scan filters per edge, not per vertex.
#[test]
fn another_relations_edges_are_not_matched() {
    under_lab(0x5c_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("two-relations");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.create_vertex(VId(5), vec![], vec![]);
        batch.create_vertex(VId(9), vec![], vec![]);
        // Descending destination order: 5 before 2. Ascending output must
        // be a sort, not an accident of write order.
        batch.add_edge(EId(10), VId(1), VId(5), vec![]);
        batch.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(cx, batch).await.expect("R edges commit");
        // The :OTHER edge leaves VId(1) — the same source as an :R edge —
        // so a per-vertex (rather than per-edge) relation filter still
        // wrongly emits VId(9).
        let mut other = WriteBatch::new(OTHER);
        other.add_edge(EId(12), VId(1), VId(9), vec![]);
        db.write(cx, other).await.expect("OTHER edge commits");

        let rows = db.execute_gql(PINNED, &bind_r()).expect("executes");
        assert_eq!(
            rows,
            vec![VId(2), VId(5)],
            "complete over :R, CGSE-sorted ascending, and VId(9) — reachable \
             only through :OTHER — is absent"
        );
    });
}

/// An empty graph answers `Ok(vec![])`: no rows, no error, no phantom.
#[test]
fn an_empty_graph_matches_nothing() {
    under_lab(0x5c_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("empty");
        let db = Database::create(cx, &dir, keys()).await.expect("creates");

        let rows = db
            .execute_gql(PINNED, &bind_r())
            .expect("an empty graph is a result, not a failure");
        assert!(rows.is_empty(), "no edges, no destinations: {rows:?}");
    });
}

/// A type alternation `-[:R|OTHER]->` scans exactly its members (fgdb-wpteg):
/// the union of the `:R` and `:OTHER` matches, never a third relation's,
/// through the pinned scan and the general query surface, counted per edge.
#[test]
fn an_alternation_scans_exactly_its_member_relations() {
    use fgdb::{QueryResult, QueryValue};
    use fgdb_gql::algebra::GraphValue;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
    use fgdb_types::CanonicalScalar;
    const THIRD: RelationId = RelationId(3);
    let ((), report) = run_async_under_lab(0x5c_04, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let cx = &commit;
        let mut db = Database::open_memory(cx, keys()).await.expect("opens");
        let mut batch = WriteBatch::new(R);
        for vid in [1, 2, 3, 5, 7, 9] {
            batch.create_vertex(VId(vid), vec![], vec![]);
        }
        batch.add_edge(EId(10), VId(1), VId(5), vec![]);
        batch.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(cx, batch).await.expect("R edges commit");
        let mut other = WriteBatch::new(OTHER);
        other.add_edge(EId(12), VId(1), VId(9), vec![]);
        db.write(cx, other).await.expect("OTHER edge commits");
        let mut third = WriteBatch::new(THIRD);
        third.add_edge(EId(13), VId(3), VId(7), vec![]);
        db.write(cx, third).await.expect("THIRD edge commits");

        let symbols = |kind: GraphSymbolKind, name: &str| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Relation, "OTHER") => Some(GraphSymbol::Relation(OTHER)),
            (GraphSymbolKind::Relation, "THIRD") => Some(GraphSymbol::Relation(THIRD)),
            _ => None,
        };
        // The GQL text surface reads alternation (6405889e). The legacy
        // RelationBind slice behind execute_gql has one relation per hop and
        // never read it, so these targets go through the query surface.
        let targets = |text: &str| -> Vec<VId> {
            let QueryResult::Rows { rows, .. } = db
                .query(
                    &query,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000),
                )
                .expect(text)
            else {
                panic!("{text} is a row query");
            };
            let mut targets: Vec<_> = rows
                .iter()
                .map(|row| match &row[..] {
                    [QueryValue::Value(GraphValue::Vertex(vid))] => *vid,
                    _ => panic!("{text}: not one vertex cell"),
                })
                .collect();
            targets.sort();
            targets
        };
        assert_eq!(
            targets("MATCH (a)-[:R|OTHER]->(b) RETURN b"),
            vec![VId(2), VId(5), VId(9)]
        );
        assert_eq!(
            targets("MATCH (a)-[:OTHER|:R|R]->(b) RETURN b"),
            vec![VId(2), VId(5), VId(9)]
        );
        assert_eq!(
            targets("MATCH (a)-[:R|OTHER|THIRD]->(b) RETURN b"),
            targets("MATCH (a)-->(b) RETURN b")
        );
        // A count cell is Count, Integer or a scalar Int by executor.
        let count = |text: &str| -> i128 {
            let QueryResult::Rows { rows, .. } = db
                .query(
                    &query,
                    text,
                    &GqlParameters::new(),
                    symbols,
                    GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000),
                )
                .expect(text)
            else {
                panic!("{text} is a row query");
            };
            assert_eq!(rows.len(), 1, "{text}");
            match &rows[0][..] {
                [QueryValue::Count(count)] => i128::from(*count),
                [QueryValue::Integer(count)] => *count,
                [QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(count)))] => {
                    i128::from(*count)
                }
                _ => panic!("{text}: not one count cell"),
            }
        };
        assert_eq!(count("MATCH (a)-[r:R|OTHER]->(b) RETURN count(r) AS c"), 3);
        assert_eq!(count("MATCH (a)<-[r:R|OTHER]-(b) RETURN count(*) AS c"), 3);
        assert_eq!(count("MATCH (a)-[:R|THIRD]->(b) RETURN count(*) AS c"), 3);
        assert_eq!(
            count("MATCH (a)-[:R|THIRD]->(b) RETURN count(DISTINCT a) AS c"),
            2
        );
    });
    assert!(report.lab_test_passed(), "lab run failed: {report:?}");
}
