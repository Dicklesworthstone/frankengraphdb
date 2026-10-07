//! A predicate-bound root reads one vertex row, not one per neighbour
//! (fgdb-2n2a2).
//!
//! `MATCH (a:Person {p: 0})-[:R]->(b) RETURN count(b)` is an edge-rooted plan.
//! Its edge closure was already local (only the root's edges are admitted),
//! but admission then hydrated a vertex row for every endpoint of every
//! admitted edge. Each row costs a binary search in every vertex patch, so at
//! a 25k-edge hub in a 15-commit database the count was refused for
//! exceeding 10M work units, although only the root's row is ever read.
//!
//! The laws:
//! - exact answers in every direction, against expectations computed here,
//!   including projections that DO read neighbour rows;
//! - the work a one-hop count from a hub needs does not grow with
//!   (degree x vertex patches): two databases that differ only in how many
//!   commits wrote the vertices need nearly the same minimal work budget;
//! - nor with the number of vertices: the root comes from the property index,
//!   not from testing the root predicate on every vertex.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, QueryError, QueryResult, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::context::{CommitCx, QueryCx};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const PERSON: LabelId = LabelId(1);
const P: PropertyKeyId = PropertyKeyId(1);
/// The hub (vertex 0, p = 0) points at 1..=DEGREE.
const DEGREE: u128 = 2_000;
/// Vertex 2_001 points at the hub, so incoming and undirected answers differ.
const INBOUND: u128 = DEGREE + 1;
/// First VId of the unrelated vertices `hub_with` adds.
const UNRELATED: u128 = 1_000_000_000;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x61; 32],
        DatabaseSecurityNamespaceId([0x62; 32]),
        [0x63; 32],
    )
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}

fn generous() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, 1_000_000_000, 1_000_000_000)
}

fn with_work(work: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(1_000_000, 1_000_000, work, 1_000_000_000)
}

/// The hub graph, with its vertices written over `commits` commits (one
/// vertex patch each), then every edge in one more commit. A second relation
/// at the hub must never be counted by an R pattern.
async fn hub(commit: &CommitCx, commits: u128) -> Database<MemVfs> {
    hub_with(commit, commits, 0).await
}

/// [`hub`] plus `unrelated` isolated Person vertices, spread over the same
/// commits so the vertex patch count is unchanged.
async fn hub_with(commit: &CommitCx, commits: u128, unrelated: u128) -> Database<MemVfs> {
    let mut db = Database::open_memory(commit, keys()).await.unwrap();
    let vids: Vec<u128> = (0..=INBOUND)
        .chain((0..unrelated).map(|at| UNRELATED + at))
        .collect();
    let per = (vids.len() as u128).div_ceil(commits) as usize;
    for chunk in vids.chunks(per) {
        let mut batch = WriteBatch::new(R);
        for &v in chunk {
            batch.create_vertex(
                VId(v),
                vec![PERSON],
                vec![(P, CanonicalScalar::Int(v as i64))],
            );
        }
        db.write(commit, batch).await.unwrap();
    }
    let mut edges = WriteBatch::new(R);
    for b in 1..=DEGREE {
        edges.add_edge(EId(b), VId(0), VId(b), vec![]);
    }
    edges.add_edge(EId(INBOUND), VId(INBOUND), VId(0), vec![]);
    db.write(commit, edges).await.unwrap();
    let mut other = WriteBatch::new(S);
    for b in 1..=10 {
        other.add_edge(EId(1_000_000 + b), VId(0), VId(b), vec![]);
    }
    db.write(commit, other).await.unwrap();
    db
}

fn count(result: &QueryResult) -> u64 {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("a read returns rows, got {result:?}");
    };
    assert_eq!(rows.len(), 1, "one aggregate row");
    rows[0][0].as_count().expect("count(...) is a count cell")
}

fn ints(result: &QueryResult) -> Vec<i64> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("a read returns rows, got {result:?}");
    };
    rows.iter()
        .map(|row| match &row[0] {
            fgdb_gql::GraphAggregateValue::Value(fgdb_gql::algebra::GraphValue::Scalar(
                CanonicalScalar::Int(value),
            )) => *value,
            other => panic!("expected an integer cell, got {other:?}"),
        })
        .collect()
}

/// The smallest work budget under which `text` succeeds; every smaller one
/// must be refused for work, never for anything else.
fn minimal_work(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> u64 {
    let params = GqlParameters::new();
    let fits = |work| match db.query(cx, text, &params, symbols, with_work(work)) {
        Ok(_) => true,
        Err(error) => {
            assert!(
                format!("{error:?}").contains("WorkUnits"),
                "{text}: refused for something other than work: {error:?}"
            );
            false
        }
    };
    let (mut low, mut high) = (0u64, 1_000_000_000u64);
    assert!(fits(high), "{text} fits a generous work budget");
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        if fits(middle) {
            high = middle;
        } else {
            low = middle;
        }
    }
    high
}

#[test]
fn a_bound_hub_answers_exactly_in_every_direction() {
    let ((), report) = run_async_under_lab(0x6875_6201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let db = hub(&commit, 7).await;
        let params = GqlParameters::new();
        let run = |text: &str| -> Result<QueryResult, QueryError> {
            db.query(&cx, text, &params, symbols, generous())
        };
        let degree = DEGREE as u64;
        assert_eq!(
            count(&run("MATCH (a:Person {p: 0})-[:R]->(b) RETURN count(b)").unwrap()),
            degree
        );
        assert_eq!(
            count(&run("MATCH (b)<-[:R]-(a:Person {p: 0}) RETURN count(b)").unwrap()),
            degree
        );
        assert_eq!(
            count(&run("MATCH (a:Person {p: 0})<-[:R]-(b) RETURN count(b)").unwrap()),
            1
        );
        assert_eq!(
            count(&run("MATCH (a:Person {p: 0})-[:R]-(b) RETURN count(b)").unwrap()),
            degree + 1
        );
        assert_eq!(
            count(&run("MATCH (a:Person {p: 0})-[:S]->(b) RETURN count(b)").unwrap()),
            10
        );
        // These read the neighbours' rows, so their endpoints must be
        // hydrated: forward, reversed and filtered on the far side.
        assert_eq!(
            ints(&run("MATCH (a:Person {p: 0})-[:R]->(b) RETURN b.p ORDER BY b.p").unwrap()),
            (1..=DEGREE as i64).collect::<Vec<_>>()
        );
        assert_eq!(
            ints(&run("MATCH (a:Person {p: 0})<-[:R]-(b) RETURN b.p").unwrap()),
            vec![INBOUND as i64]
        );
        assert_eq!(
            count(&run("MATCH (a:Person {p: 0})-[:R]->(b:Person {p: 7}) RETURN count(b)").unwrap()),
            1
        );
        assert_eq!(
            ints(&run("MATCH (a:Person)-[:R]->(b:Person {p: 0}) RETURN a.p").unwrap()),
            vec![INBOUND as i64]
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_one_hop_count_from_a_hub_costs_no_row_per_neighbour() {
    let ((), report) = run_async_under_lab(0x6875_6202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let one_patch = hub(&commit, 1).await;
        let many_patches = hub(&commit, 25).await;
        // Directed forms only: an undirected root edge can bind either
        // endpoint to the root slot, so the root predicate is evaluated on
        // every neighbour and every neighbour's row is read.
        for text in [
            "MATCH (a:Person {p: 0})-[:R]->(b) RETURN count(b)",
            "MATCH (b)<-[:R]-(a:Person {p: 0}) RETURN count(b)",
        ] {
            let few = minimal_work(&one_patch, &cx, text);
            let many = minimal_work(&many_patches, &cx, text);
            // Hydrating a row per neighbour costs about DEGREE x 24 more
            // patch searches here (~10 work units each): ~480,000 units. Only
            // the root's row costs ~24 more, and the vertex scan's merge over
            // 25 patches a few units per vertex.
            assert!(
                many.saturating_sub(few) < 100_000,
                "{text}: {few} work units with one vertex patch but {many} with 25"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_one_hop_count_from_a_hub_costs_no_work_per_unrelated_vertex() {
    let ((), report) = run_async_under_lab(0x6875_6203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let small = hub_with(&commit, 1, 0).await;
        let large = hub_with(&commit, 1, 20_000).await;
        for text in [
            "MATCH (a:Person {p: 0})-[:R]->(b) RETURN count(b)",
            "MATCH (b)<-[:R]-(a:Person {p: 0}) RETURN count(b)",
        ] {
            let few = minimal_work(&small, &cx, text);
            let many = minimal_work(&large, &cx, text);
            // Measured: +847 units for 20,000 more vertices when the root is
            // served from the property index, +41,035 when the root
            // predicate is tested on every vertex (about 2 units each).
            assert!(
                many.saturating_sub(few) < 4_000,
                "{text}: {few} work units over the hub alone but {many} with 20,000 unrelated vertices"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_neighbour_properties_skip_unrelated_vertex_update_history() {
    let ((), report) = run_async_under_lab(0x6875_6204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let mut db = hub_with(&commit, 1, 1).await;
        let text = "MATCH (a:Person {p: 0})-[:R]->(b) RETURN b.p ORDER BY b.p";
        let params = GqlParameters::new();
        let expected: Vec<_> = (1..=DEGREE as i64).collect();
        assert_eq!(
            ints(&db.query(&cx, text, &params, symbols, generous()).unwrap()),
            expected
        );
        let before = minimal_work(&db, &cx, text);
        // These patches belong entirely to an isolated vertex. A projection
        // reads every neighbour's row, but none of these historical versions.
        for step in 1..=64 {
            let mut update = WriteBatch::new(R);
            update.set_vertex_property(VId(UNRELATED), P, Some(CanonicalScalar::Int(-step)));
            db.write(&commit, update).await.unwrap();
        }
        let after = minimal_work(&db, &cx, text);
        assert_eq!(
            ints(&db.query(&cx, text, &params, symbols, generous()).unwrap()),
            expected
        );
        // This permits several extra directory comparisons per result while
        // refusing one search of every new patch per hydrated endpoint.
        assert!(
            after.saturating_sub(before) < DEGREE as u64 * 4,
            "projection needed {before} work units before unrelated updates, {after} after"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
