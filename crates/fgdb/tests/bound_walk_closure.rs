//! A bound root's quantified walk admits its closure when what the walk
//! actually reaches is small, even when the closure's projection from the
//! root's degree and the snapshot's mean degree runs past the cap
//! (fgdb-61p1n: a citation graph's 13-hop shortest path from a 770-degree hub
//! projected past the whole graph but reaches 1.3% of it).
//!
//! The fixture: a hub of degree 40 whose neighbours are leaves except one, the
//! head of a 10-vertex chain, beside an unrelated dense component that sets
//! the snapshot's mean degree to ~20. Every walk from the hub reaches at most
//! 50 edges, while the projection is 40 x 20^9.

use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys, MemVfs, QueryResult, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};

const R: RelationId = RelationId(1);
const P: PropertyKeyId = PropertyKeyId(1);
const HUB_DEGREE: u128 = 40;
/// The chain hangs off neighbour 1: 1 -> 41 -> 42 -> ... -> 50.
const CHAIN_END: u128 = 50;
/// The dense component: DENSE vertices, each pointing at the next FANOUT.
const DENSE_FIRST: u128 = 1_000;
const DENSE: u128 = 100;
const FANOUT: u128 = 30;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn policy(records: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(records, 100_000, 100_000_000, 10_000_000)
}

fn run<T>(test: impl AsyncFnOnce(&fgdb_types::CommitCx, &QueryCx) -> T) -> T {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let commit = contexts.commit();
    let cx = contexts.query();
    runtime.block_on(test(&commit, &cx))
}

/// The R edges, as (source, destination).
fn edges() -> Vec<(u128, u128)> {
    let mut edges: Vec<(u128, u128)> = (1..=HUB_DEGREE).map(|b| (0, b)).collect();
    edges.push((1, HUB_DEGREE + 1));
    for at in HUB_DEGREE + 1..CHAIN_END {
        edges.push((at, at + 1));
    }
    for at in 0..DENSE {
        for step in 1..=FANOUT {
            edges.push((DENSE_FIRST + at, DENSE_FIRST + (at + step) % DENSE));
        }
    }
    edges
}

async fn database(commit: &fgdb_types::CommitCx) -> Database<MemVfs> {
    let mut db = Database::<MemVfs>::open_memory(commit, keys())
        .await
        .unwrap();
    let mut vertices = WriteBatch::new(R);
    for vid in (0..=CHAIN_END).chain(DENSE_FIRST..DENSE_FIRST + DENSE) {
        vertices.create_vertex(
            VId(vid),
            vec![],
            vec![(P, CanonicalScalar::Int(vid as i64))],
        );
    }
    db.write(commit, vertices).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for (at, &(src, dst)) in edges().iter().enumerate() {
        batch.add_edge(EId(at as u128 + 1), VId(src), VId(dst), vec![]);
    }
    db.write(commit, batch).await.unwrap();
    db
}

fn count(db: &Database<MemVfs>, cx: &QueryCx, text: &str, records: u64) -> Option<u64> {
    match db.query(cx, text, &GqlParameters::new(), symbols, policy(records)) {
        Ok(QueryResult::Rows { rows, .. }) => rows[0][0].as_count(),
        _ => None,
    }
}

fn admitted(db: &Database<MemVfs>, cx: &QueryCx, text: &str, records: u64) -> bool {
    db.query(cx, text, &GqlParameters::new(), symbols, policy(records))
        .is_ok()
}

/// The smallest snapshot-record budget `text` is admitted under.
fn least_records(db: &Database<MemVfs>, cx: &QueryCx, text: &str) -> u64 {
    let (mut low, mut high) = (0, 100_000);
    assert!(admitted(db, cx, text, high), "{text} under a wide budget");
    while low < high {
        let middle = low + (high - low) / 2;
        if admitted(db, cx, text, middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

#[test]
fn a_bound_walk_whose_reach_is_small_admits_its_closure_not_the_graph() {
    run(async |commit, cx| {
        let db = database(commit).await;
        let total = edges().len() as u64;
        // Every vertex a walk of 1..=10 hops from the hub reaches, by a
        // breadth-first walk of the fixture's own edges.
        let mut reached = std::collections::BTreeSet::new();
        let mut frontier = vec![0_u128];
        for _ in 0..10 {
            frontier = edges()
                .into_iter()
                .filter(|(src, _)| frontier.contains(src))
                .map(|(_, dst)| dst)
                .filter(|dst| reached.insert(*dst))
                .collect();
        }
        assert!(
            reached.len() < CHAIN_END as usize,
            "the chain outruns 10 hops"
        );
        let text = "MATCH (a {p: 0})-[:R]->{1,10}(b) RETURN count(DISTINCT b) AS c";
        assert_eq!(count(&db, cx, text, 100_000), Some(reached.len() as u64));
        let least = least_records(&db, cx, text);
        // The closure: about 50 edges and the rows of their endpoints,
        // against more than 3,000 edges in the graph. The whole-graph
        // admission the projection alone chose needs over 3,000 records.
        assert!(least <= 4 * CHAIN_END as u64, "least {least} of {total}");
        assert!(total > 50 * CHAIN_END as u64);
        // A shortest path to the farthest vertex within 10 hops, 49, at the
        // tenth hop: the shape the gauntlet asks.
        let text = "MATCH p = SHORTEST (a {p: 0})-[:R]->{1,10}(b {p: 49}) \
                    RETURN path_length(p) AS l";
        let QueryResult::Rows { rows, .. } = db
            .query(cx, text, &GqlParameters::new(), symbols, policy(100_000))
            .unwrap()
        else {
            panic!("not rows");
        };
        assert_eq!(rows.len(), 1);
        assert!(matches!(
            &rows[0][0],
            fgdb::QueryValue::Value(fgdb_gql::algebra::GraphValue::Scalar(CanonicalScalar::Int(
                10
            )))
        ));
        let least = least_records(&db, cx, text);
        assert!(least <= 4 * CHAIN_END as u64, "least {least} of {total}");
    });
}
