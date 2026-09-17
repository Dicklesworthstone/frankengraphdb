//! Native ANY/ALL SHORTEST WALK and bounded variable-length expansion versus
//! an independent evaluator over replayed `ReferenceGraph` state
//! (fgdb-oracle-shortest-walk-6y4q).
//!
//! The oracle never calls fgdb-gql evaluation code. Its law, read from the
//! engine's own registered semantics (fgdb-gql/src/graph_text/scoped.rs:7-10,
//! algebra/pattern.rs:476-479, shortest_walk.rs:5-6): a pair (a,b) settles at
//! `dist'` = min { k in [m,n] : some walk of length k joins a to b } — the
//! lower bound delays settlement instead of post-filtering — ANY emits one row
//! per settled pair, ALL emits one row per walk of length exactly `dist'`
//! (tied minimum-hop occurrences, parallel edges included), and a plain
//! bounded `WALK *m..n` emits every walk of every length in the interval.
use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, MemVfs, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_gql::algebra::GraphValueRow;
use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphSymbol, GraphSymbolKind, PreparedGraphText};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::{
    BranchId, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, GraphId, PurposeContexts,
    QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};

const R: RelationId = RelationId(1);
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const SOURCES: [u128; 9] = [1, 2, 3, 4, 5, 6, 7, 8, 9];

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0x6a; 32], NAMESPACE, [0x4d; 32])
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        [0x6a; 32],
        NAMESPACE,
        [0x4d; 32],
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(50_000, 50_000, 50_000_000, 50_000_000)
}

/// Deterministic structure with every required hazard: directed cycles, a
/// self-loop, parallel edges, an isolated vertex (9), and two tied 2-hop
/// routes into vertex 6 (1->5->6 and 1->7->6). The seed-varied shortcut is
/// deleted in commit 2 and one tie edge in commit 3, reshaping shortest
/// distances between the pinned and live graphs.
async fn generate(db: &mut Database<MemVfs>, cx: &CommitCx, seed: u64) -> (CommitSeq, CommitSeq) {
    let mut batch = WriteBatch::new(R);
    for id in 1u128..=9 {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    let mut eid: u128 = 1;
    let mut edge = |batch: &mut WriteBatch, src: u128, dst: u128| {
        batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
        eid += 1;
        EId(eid - 1)
    };
    // Directed cycle 1->2->3->4->1 plus the closing edge 2->1.
    edge(&mut batch, 1, 2);
    edge(&mut batch, 2, 3);
    edge(&mut batch, 3, 4);
    edge(&mut batch, 4, 1);
    edge(&mut batch, 2, 1);
    // Tied routes 1->5->6 and 1->7->6, then 6->8.
    let tie_a = edge(&mut batch, 1, 5);
    edge(&mut batch, 5, 6);
    edge(&mut batch, 1, 7);
    edge(&mut batch, 7, 6);
    edge(&mut batch, 6, 8);
    // Self-loop and a parallel pair 3->2.
    edge(&mut batch, 8, 8);
    edge(&mut batch, 3, 2);
    edge(&mut batch, 3, 2);
    // Seed-varied shortcut: three of four seeds give 1 a direct edge into 6.
    let varied = edge(&mut batch, u128::from(1 + seed % 3), 6);
    let basis1 = db.write(cx, batch).await.unwrap();

    let mut first = WriteBatch::new(R);
    first.delete_edge(varied);
    let basis2 = db.write(cx, first).await.unwrap();
    let mut second = WriteBatch::new(R);
    second.delete_edge(tie_a);
    let basis3 = db.write(cx, second).await.unwrap();
    (basis1, basis3)
}

/// Per-step adjacency multiplicity in the requested direction: 0 forward,
/// 1 reverse, 2 undirected. Walks may reuse edges, so parallel edges and a
/// self-loop each contribute separately.
fn step(graph: &ReferenceGraph, from: VId, direction: u8) -> BTreeMap<VId, u64> {
    let mut next = BTreeMap::new();
    for (_, e) in graph.iter_edges().filter(|(_, e)| e.relation == R) {
        match direction {
            0 => {
                if e.src == from {
                    *next.entry(e.dst).or_insert(0) += 1;
                }
            }
            1 => {
                if e.dst == from {
                    *next.entry(e.src).or_insert(0) += 1;
                }
            }
            _ => {
                if e.src == from {
                    *next.entry(e.dst).or_insert(0) += 1;
                }
                if e.dst == from {
                    *next.entry(e.src).or_insert(0) += 1;
                }
            }
        }
    }
    next
}

/// Walks of exactly `k` hops: the product of the previous level with the
/// adjacency. Level 0 is the empty walk (one route to the source itself).
fn level(
    graph: &ReferenceGraph,
    previous: &BTreeMap<VId, u64>,
    direction: u8,
) -> BTreeMap<VId, u64> {
    let mut current: BTreeMap<VId, u64> = BTreeMap::new();
    for (vertex, routes) in previous {
        for (head, parallel) in step(graph, *vertex, direction) {
            *current.entry(head).or_insert(0) += routes * parallel;
        }
    }
    current
}

/// The settlement law. Per source, levels k = 0..=n are enumerated; a pair
/// settles at its first level k >= m with any walk: ANY keeps one row per
/// settled pair, ALL keeps the walk count at the settled level. With
/// `accumulate_all` (plain bounded WALK) every level's walks are summed.
fn settled(
    graph: &ReferenceGraph,
    src: VId,
    m: u64,
    n: u64,
    direction: u8,
    accumulate_all: bool,
) -> BTreeMap<VId, u64> {
    let mut settled = BTreeMap::new();
    let mut done = BTreeSet::new();
    let mut current: BTreeMap<VId, u64> = BTreeMap::new();
    current.insert(src, 1);
    for k in 0..=n {
        if k >= m {
            for (dst, count) in &current {
                if accumulate_all {
                    *settled.entry(*dst).or_insert(0) += *count;
                } else if done.insert(*dst) {
                    settled.insert(*dst, *count);
                }
            }
        }
        if k < n {
            current = level(graph, &current, direction);
        }
    }
    settled
}

/// One ANY-style row per settled pair, or one ALL-style row per walk
/// occurrence at the settled level.
fn expected_rows(
    graph: &ReferenceGraph,
    m: u64,
    n: u64,
    direction: u8,
    any: bool,
) -> Vec<(VId, VId)> {
    let mut rows = Vec::new();
    for id in SOURCES {
        for (dst, count) in settled(graph, VId(id), m, n, direction, false) {
            if any {
                rows.push((VId(id), dst));
            } else {
                for _ in 0..count {
                    rows.push((VId(id), dst));
                }
            }
        }
    }
    rows
}

fn engine_pairs(rows: &[GraphValueRow]) -> Vec<(VId, VId)> {
    rows.iter()
        .map(|r| {
            (
                r.values()[0].as_vertex().unwrap(),
                r.values()[1].as_vertex().unwrap(),
            )
        })
        .collect()
}

fn compare(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    seed: u64,
    text: &str,
    mut expected: Vec<(VId, VId)>,
) {
    let prepared = PreparedGraphText::prepare(text, symbols)
        .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let mut actual = engine_pairs(
        &db.execute_graph_pattern_governed(cx, &prepared, policy())
            .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
            .value,
    );
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected, "seed={seed:#x}; query={text}");
}

fn compare_at(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    seed: u64,
    text: &str,
    basis: CommitSeq,
    mut expected: Vec<(VId, VId)>,
) {
    let prepared = PreparedGraphText::prepare(text, symbols)
        .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: {e:?}"))
        .bind_parameters(&GqlParameters::new())
        .unwrap();
    let mut actual = engine_pairs(
        &db.execute_graph_pattern_governed_at(cx, &prepared, basis, policy())
            .unwrap_or_else(|e| panic!("seed={seed:#x} {text}: as-of {basis:?}: {e:?}"))
            .value,
    );
    actual.sort();
    expected.sort();
    assert_eq!(
        actual, expected,
        "seed={seed:#x}; as-of {basis:?}; query={text}"
    );
}

fn check_families(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    live: &ReferenceGraph,
    pinned: &ReferenceGraph,
    pinned_basis: CommitSeq,
    seed: u64,
) {
    // ANY SHORTEST, forward, m=1..n=3: one row per reachable pair within
    // bounds. Vertex 9 is isolated, so it appears in neither column.
    let any = expected_rows(live, 1, 3, 0, true);
    assert!(!any.iter().any(|(s, d)| *s == VId(9) || *d == VId(9)));
    assert!(
        any.iter()
            .any(|(s, d)| settled(live, *s, 1, 3, 0, false)[d] >= 2),
        "some settled pair must have >= 2 tied routes"
    );
    compare(
        db,
        cx,
        seed,
        "MATCH ANY SHORTEST WALK (a)-[:R*1..3]->(b) RETURN a,b",
        any,
    );
    // ALL SHORTEST with m = 0: empty walks settle self-pairs (so (8,8) and
    // every isolated pair appears), and the parallel pair into 2 makes some
    // pair's settled multiplicity >= 2.
    let all = expected_rows(live, 0, 3, 0, false);
    assert!(all.contains(&(VId(8), VId(8))), "empty walk settles (8,8)");
    assert!(
        all.iter()
            .any(|(s, d)| s != d && settled(live, *s, 0, 3, 0, false)[d] >= 2),
        "ALL multiplicity >= 2 for a tied pair"
    );
    compare(
        db,
        cx,
        seed,
        "MATCH ALL SHORTEST WALK (a)-[:R*0..3]->(b) RETURN a,b",
        all,
    );
    // Reverse direction: (a)<-[:R*1..2]-(b) joins a to b by backwards walks.
    let reverse = expected_rows(live, 1, 2, 1, false);
    assert!(!reverse.is_empty());
    compare(
        db,
        cx,
        seed,
        "MATCH ALL SHORTEST WALK (a)<-[:R*1..2]-(b) RETURN a,b",
        reverse,
    );
    // Undirected ANY over the same bounds.
    let undirected = expected_rows(live, 1, 2, 2, true);
    assert!(!undirected.is_empty());
    compare(
        db,
        cx,
        seed,
        "MATCH ANY SHORTEST WALK (a)-[:R*1..2]-(b) RETURN a,b",
        undirected,
    );
    // Plain bounded WALK *1..2: every walk of length 1 and 2, no settlement.
    // The cycle closes within the bound, so (1,1) appears.
    let bounded: Vec<(VId, VId)> = {
        let mut rows = Vec::new();
        for id in SOURCES {
            for (dst, count) in settled(live, VId(id), 1, 2, 0, true) {
                for _ in 0..count {
                    rows.push((VId(id), dst));
                }
            }
        }
        rows
    };
    assert!(
        bounded.contains(&(VId(1), VId(1))),
        "cycle closes in 2 hops"
    );
    compare(
        db,
        cx,
        seed,
        "MATCH WALK (a)-[:R*1..2]->(b) RETURN a,b",
        bounded,
    );
    // A lower bound above the only route's length excludes the pair: with
    // *3..3 the one-hop route into 5 no longer settles it.
    let tightened = expected_rows(live, 3, 3, 0, true);
    assert!(!tightened.contains(&(VId(1), VId(5))));
    compare(
        db,
        cx,
        seed,
        "MATCH ANY SHORTEST WALK (a)-[:R*3..3]->(b) RETURN a,b",
        tightened,
    );
    // Temporal family: pinned before the tie-edge delete, the ALL answer
    // differs from the live answer, and both pinned and live match their own
    // graph under the registered settlement semantics.
    let pinned_rows = expected_rows(pinned, 1, 3, 0, false);
    let live_rows = expected_rows(live, 1, 3, 0, false);
    assert_ne!(pinned_rows, live_rows, "the delete must move the answer");
    compare_at(
        db,
        cx,
        seed,
        "MATCH ALL SHORTEST WALK (a)-[:R*1..3]->(b) RETURN a,b",
        pinned_basis,
        pinned_rows,
    );
}

fn run_seed(seed: u64) {
    let ((), report) = run_async_under_lab(seed, move |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let (basis1, live_basis) = generate(&mut db, &commit, seed).await;
        drop(db);
        let coordinator =
            CommitCoordinator::open_with_vfs(&commit, vfs.clone(), &path, oracle_keys())
                .await
                .unwrap();
        let live = replay(&commit, &coordinator).await.unwrap().database;
        let pinned = replay_through(&commit, &coordinator, basis1)
            .await
            .unwrap()
            .database;
        drop(coordinator);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(db.frontier().unwrap(), live_basis);
        check_families(
            &db,
            &query,
            live.graph(GRAPH, BRANCH).unwrap(),
            pinned.graph(GRAPH, BRANCH).unwrap(),
            basis1,
            seed,
        );
    });
    assert!(report.lab_test_passed(), "seed={seed:#x}: {report:?}");
}

#[test]
fn native_shortest_walk_seed_6a70() {
    run_seed(0x6a70);
}

#[test]
fn native_shortest_walk_seed_beef() {
    run_seed(0xbeef);
}

#[test]
fn native_shortest_walk_seed_fa11() {
    run_seed(0xfa11);
}

#[test]
fn native_shortest_walk_seed_c0de() {
    run_seed(0xc0de);
}
