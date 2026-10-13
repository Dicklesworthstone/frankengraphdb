//! Differential + complexity gates for bound-endpoint indexed admission
//! (fgdb-indexed-expansion-a26r).
//!
//! The independent oracle is the *storage* merge over the admitted snapshot
//! (`Database::edges_at`), folded in the test into the exact answer an
//! unbound whole-graph scan would produce for the same predicate. The bound
//! query must return byte-identical rows in the same order at every admitted
//! sequence, in both directions and undirected, including retired edges. A
//! separate complexity gate proves degree-proportional admission: the same
//! bound query charges an identical Work/SnapshotRecord count on a 1k-edge
//! and a 50k-edge graph whose bound vertex neighbourhood is identical.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, RelationBind, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy, PreparedGqlTemplate};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};

const R: RelationId = RelationId(1);
const N: PropertyKeyId = PropertyKeyId(1);
const BOUND: i64 = 7;
const ID_TEXT: &str = "MATCH (a)-[:R]->(b) WHERE a.n = $n RETURN b";
const INCOMING_TEXT: &str = "MATCH (a)<-[:R]-(b) WHERE a.n = $n RETURN b";
const UNDIRECTED_TEXT: &str = "MATCH (a)-[:R]-(b) WHERE a.n = $n RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa2; 32],
        DatabaseSecurityNamespaceId([0xa3; 32]),
        [0xa4; 32],
    )
}

/// Deterministic LCG; the closed universe has no rand crate.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 11
    }
}

/// At least 5 commits mixing vertex creates, edge creates, edge deletes and property
/// updates. Property updates keep exactly one vertex per seed bound to the
/// property value the queries select on, so bounded answers stay stable.
async fn generated(
    seed: u64,
    db: &mut Database<MemVfs>,
    cx: &fgdb_types::CommitCx,
) -> Vec<CommitSeq> {
    let mut rng = Rng(seed | 1);
    let mut seqs = Vec::new();
    let mut next_eid = 1_000_u128;
    let mut live: Vec<EId> = Vec::new();
    for commit in 0..6_u64 {
        let mut batch = WriteBatch::new(R);
        if commit == 0 {
            batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
            for id in 1..12_u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            batch.add_edge(EId(1), VId(0), VId(11), vec![]);
            batch.add_edge(EId(2), VId(11), VId(0), vec![]);
        }
        if commit == 2 {
            batch.set_edge_property(EId(2), N, Some(CanonicalScalar::Int(42)));
        }
        if commit == 3 {
            batch.delete_edge(EId(1));
        }
        for _ in 0..10 {
            let roll = rng.next() % 5;
            match roll {
                // delete an edge
                0 if live.len() > 4 => {
                    let at = (rng.next() as usize) % live.len();
                    let eid = live.swap_remove(at);
                    batch.delete_edge(eid);
                }
                // property update on a non-bound vertex
                1 => {
                    let id = 1 + (rng.next() % 10) as u128;
                    batch.set_vertex_property(
                        VId(id),
                        N,
                        Some(CanonicalScalar::Int(commit as i64)),
                    );
                }
                _ => {
                    let eid = EId(next_eid);
                    next_eid += 1;
                    // 50% of edges touch the bound vertex, giving every commit
                    // both in- and out-degree changes on VId(0).
                    let (src, dst) = if rng.next().is_multiple_of(2) {
                        (VId(0), VId((rng.next() % 12) as u128))
                    } else {
                        (VId((rng.next() % 12) as u128), VId(0))
                    };
                    batch.add_edge(eid, src, dst, vec![]);
                    live.push(eid);
                }
            }
        }
        seqs.push(db.write(cx, batch).await.unwrap());
    }
    seqs
}

/// The independent oracle: one visible edge triple per EId at `as_of`, from
/// the snapshot's decoded blocks, ordered by EId — visit_edges' winner rule.
fn oracle(
    db: &Database<MemVfs>,
    at: CommitSeq,
    bound: VId,
    forward: Option<bool>,
) -> Vec<(VId, VId)> {
    let mut rows = Vec::new();
    for record in db.edges_at(at).unwrap() {
        let entry = &record.entry;
        if entry.relation != R {
            continue;
        }
        let incident = match forward {
            Some(true) => entry.src == bound,
            Some(false) => entry.dst == bound,
            None => entry.src == bound || entry.dst == bound,
        };
        if incident {
            rows.push((entry.src, entry.dst));
        }
    }
    rows
}

/// Indexed answers == scan answers, both directions + undirected, at every
/// cut including before/after deletes; anti-vacuity asserts non-empty rows
/// and at least one retired edge filtered at the post-delete cut.
#[test]
fn indexed_bound_answers_equal_scan_answers_across_history_and_directions() {
    for (lab_seed, graph_seed) in [
        (0xa26_0001, 1_u64),
        (0xa26_0002, 2),
        (0xa26_0003, 0xdead_beef),
    ] {
        let ((), report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let seqs = generated(graph_seed, &mut db, &commit).await;
            let names = RelationBind::new()
                .with_relation("R", R)
                .with_property("n", N);
            let bound_template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
            let incoming_template = PreparedGqlTemplate::prepare(INCOMING_TEXT, &names).unwrap();
            let undirected_template =
                PreparedGqlTemplate::prepare(UNDIRECTED_TEXT, &names).unwrap();
            let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
            let mut saw_nonempty = false;
            for at in &seqs {
                for (template, forward) in [
                    (&bound_template, Some(true)),
                    (&incoming_template, Some(false)),
                    (&undirected_template, None),
                ] {
                    let query = template.bind_parameters(&args).unwrap();
                    // Project{b} + Distinct + OrderByVertexId mirror: sorted
                    // distinct single-column rows.
                    let expected: Vec<VId> = oracle(&db, *at, VId(0), forward)
                        .into_iter()
                        .map(|(src, dst)| {
                            if forward == Some(false) || (forward.is_none() && dst == VId(0)) {
                                src
                            } else {
                                dst
                            }
                        })
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    let rows = db
                        .execute_prepared_query_governed_at(
                            &contexts.query(),
                            &query,
                            *at,
                            GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                        )
                        .unwrap();
                    assert_eq!(rows.value, expected, "at={:?} forward={forward:?}", at);
                    saw_nonempty |= !rows.value.is_empty();
                }
            }
            assert!(db.edge_at(EId(1), seqs[2]).unwrap().is_some());
            assert!(db.edge_at(EId(1), seqs[3]).unwrap().is_none());
            assert!(db.edge_at(EId(1), seqs[5]).unwrap().is_none());
            assert!(
                saw_nonempty,
                "seed {graph_seed}: differential produced no rows"
            );
        });
        assert!(report.lab_test_passed(), "seed {graph_seed}: {report:?}");
    }
}

/// Identical neighbourhoods on 1k and 50k edge snapshots must charge the same work.
#[test]
fn bound_degree_charges_stay_constant_as_unrelated_edges_grow() {
    // Two sequential single-database lab runs: (edges, expected records).
    let mut charged = Vec::new();
    for (lab_seed, total) in [(0xa26_0042_u64, 1_000_usize), (0xa26_0043, 50_000)] {
        let (result, report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let query_cx = contexts.query();
            let names = RelationBind::new()
                .with_relation("R", R)
                .with_property("n", N);
            let template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
            let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
            let query = template.bind_parameters(&args).unwrap();
            let wide = GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000);
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let at = graph_of_degree(&mut db, &commit, total, 20).await;
            let run = db
                .execute_prepared_query_governed_at(&query_cx, &query, at, wide)
                .unwrap();
            (
                run.rows.snapshot_records,
                run.evaluator.work_units,
                run.value,
            )
        });
        assert!(report.lab_test_passed(), "total {total}: {report:?}");
        charged.push((total, result));
    }
    // Identical degree-20 bound neighbourhood: source charges must match.
    assert_eq!(charged[0].1.0, charged[1].1.0, "snapshot records");
    assert_eq!(charged[0].1.1, charged[1].1.1, "work units");
    assert_eq!(charged[0].1.2, charged[1].1.2, "result rows");
    assert_eq!(
        charged[0].1.0, 10,
        "forward bound lookup charges out-degree"
    );
    assert!(charged[0].1.1 > 0);
}

/// Builds `total` edges across chunked commits (bounded per-commit cost; a
/// single 50k-edge batch makes lab setup the bottleneck, not the query) where
/// the bound vertex owns exactly `degree` of them (alternating in/out),
/// everything else fanned from other vertices. Returns the final commit.
async fn graph_of_degree(
    db: &mut Database<MemVfs>,
    cx: &fgdb_types::CommitCx,
    total: usize,
    degree: usize,
) -> CommitSeq {
    let others = 32;
    let mut seq = CommitSeq(0);
    let mut at = 0_usize;
    while at < total {
        let mut batch = WriteBatch::new(R);
        if seq.0 == 0 {
            batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
            for id in 1..=others as u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
        }
        let end = (at + 1_000).min(total);
        for index in at..end {
            let eid = EId(50_000_000 + index as u128);
            let (src, dst) = if index < degree {
                if index % 2 == 0 {
                    (VId(0), VId(1 + (index as u128) % (others as u128)))
                } else {
                    (VId(1 + (index as u128) % (others as u128)), VId(0))
                }
            } else {
                let a = 1 + (index as u128) % (others as u128);
                let b = 1 + ((index * 7 + 3) as u128) % (others as u128);
                (VId(a), VId(b))
            };
            batch.add_edge(eid, src, dst, vec![]);
        }
        at = end;
        seq = db.write(cx, batch).await.unwrap();
    }
    seq
}

#[test]
fn indexed_admission_still_refuses_at_snapshot_record_budget() {
    let (result, report) = run_async_under_lab(0xa26_0070, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=4 {
            batch.create_vertex(VId(id), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
        }
        for id in 1..=3 {
            batch.add_edge(EId(id), VId(id), VId(id + 1), vec![]);
        }
        let at = db.write(&commit, batch).await.unwrap();
        let names = RelationBind::new()
            .with_relation("R", R)
            .with_property("n", N);
        let template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
        let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
        let query = template.bind_parameters(&args).unwrap();
        let pinned = GqlQueryPolicy::new(2, 1_000_000, 100_000, 100_000);
        db.execute_prepared_query_governed_at(&query_cx, &query, at, pinned)
    });
    assert!(
        matches!(
            &result,
            Err(fgdb_gql::GqlQueryError::Rows(fgdb_gql::GqlBudgetExceeded {
                dimension: fgdb_gql::GqlBudgetDimension::SnapshotRecords,
                ..
            }))
        ),
        "expected snapshot-record refusal, got {result:?}"
    );
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bound_fixed_edge_keeps_topology_for_later_walk() {
    let ((), report) = run_async_under_lab(0xa26_0050, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 1..=4 {
            batch.create_vertex(VId(id), vec![], vec![(N, CanonicalScalar::Int(id as i64))]);
        }
        for id in 1..=3 {
            batch.add_edge(EId(id), VId(id), VId(id + 1), vec![]);
        }
        let at = db.write(&commit, batch).await.unwrap();
        let query = fgdb_gql::PreparedGraphText::prepare(
            "MATCH WALK (a)-[:R]->(b)-[:R*2..2]->(c) WHERE a.n = 1 RETURN ALL c",
            |kind: fgdb_gql::GraphSymbolKind, name: &str| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                    Some(fgdb_gql::GraphSymbol::Relation(R))
                }
                (fgdb_gql::GraphSymbolKind::Property, "n") => {
                    Some(fgdb_gql::GraphSymbol::Property(N))
                }
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let run = db
            .execute_graph_pattern_governed_at(
                &contexts.query(),
                &query,
                at,
                GqlQueryPolicy::new(1000, 1000, 100_000, 100_000),
            )
            .unwrap();
        let ids: Vec<_> = run
            .value
            .iter()
            .map(|row| row.values()[0].as_vertex().unwrap())
            .collect();
        assert_eq!(ids, vec![VId(4)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_snapshot_traversal_survives_successor_publication() {
    let ((), report) = run_async_under_lab(0xa26_0060, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut first = WriteBatch::new(R);
        for id in 1..=3 {
            first.create_vertex(VId(id), vec![], vec![(N, CanonicalScalar::Int(id as i64))]);
        }
        first.add_edge(EId(1), VId(1), VId(2), vec![]);
        let at = db.write(&commit, first).await.unwrap();
        let pinned = db.read_session().unwrap();
        let names = RelationBind::new()
            .with_relation("R", R)
            .with_property("n", N);
        let text = "MATCH (a)-[:R]->(b) WHERE a.n = 1 RETURN b";
        assert_eq!(pinned.execute_gql(text, &names).unwrap(), vec![VId(2)]);
        let mut successor = WriteBatch::new(R);
        successor.add_edge(EId(2), VId(1), VId(3), vec![]);
        db.write(&commit, successor).await.unwrap();
        assert_eq!(db.execute_gql(text, &names).unwrap(), vec![VId(2), VId(3)]);
        assert_eq!(pinned.execute_gql(text, &names).unwrap(), vec![VId(2)]);
        assert_eq!(db.execute_gql_at(text, &names, at).unwrap(), vec![VId(2)]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

const S: RelationId = RelationId(2);

/// A bound multi-hop shape: its text, then hops `(from, to, relation, kind)`
/// in binding order from the bound `a` (slot 0 here), where kind 0 is
/// from->to, 1 is from<-to and 2 is undirected; then the returned variable.
type Shape = (
    &'static str,
    &'static [(usize, usize, RelationId, u8)],
    usize,
);

const SHAPES: &[Shape] = &[
    (
        "MATCH (a)-[:R]->(b)-[:R]->(c) WHERE a.n = $n RETURN c",
        &[(0, 1, R, 0), (1, 2, R, 0)],
        2,
    ),
    (
        "MATCH (a)-[:R]->(b)<-[:S]-(c) WHERE a.n = $n RETURN c",
        &[(0, 1, R, 0), (1, 2, S, 1)],
        2,
    ),
    (
        "MATCH (a)<-[:R]-(b)-[:S]-(c) WHERE a.n = $n RETURN c",
        &[(0, 1, R, 1), (1, 2, S, 2)],
        2,
    ),
    (
        "MATCH (b)<-[:R]-(a)-[:S]->(c) WHERE a.n = $n RETURN c",
        &[(0, 1, R, 0), (0, 2, S, 0)],
        2,
    ),
    (
        "MATCH (a)-[:R]->(b)-[:R]->(c)-[:R]->(a) WHERE a.n = $n RETURN b",
        &[(0, 1, R, 0), (1, 2, R, 0), (2, 0, R, 0)],
        1,
    ),
    (
        "MATCH (a)-[:R]->(b)-[:S]->(c)-[:R]-(d) WHERE a.n = $n RETURN d",
        &[(0, 1, R, 0), (1, 2, S, 0), (2, 3, R, 2)],
        3,
    ),
];

/// Brute-force DIFFERENT EDGES bindings over the storage merge at `at`, as
/// the sorted bag of returned vertices: every hop picks an edge incident to
/// its already-bound vertex, all chosen EIds are distinct, and a hop into a
/// bound variable must close on it.
fn shape_oracle(db: &Database<MemVfs>, at: CommitSeq, shape: &Shape) -> Vec<VId> {
    let edges: Vec<(EId, VId, RelationId, VId)> = db
        .edges_at(at)
        .unwrap()
        .into_iter()
        .map(|record| {
            let entry = record.entry;
            (entry.eid, entry.src, entry.relation, entry.dst)
        })
        .collect();
    fn walk(
        edges: &[(EId, VId, RelationId, VId)],
        hops: &[(usize, usize, RelationId, u8)],
        bound: &mut Vec<Option<VId>>,
        used: &mut Vec<EId>,
        ret: usize,
        out: &mut Vec<VId>,
    ) {
        let Some(&(from, to, relation, kind)) = hops.first() else {
            out.push(bound[ret].unwrap());
            return;
        };
        let near = bound[from].unwrap();
        for &(eid, src, rel, dst) in edges {
            if rel != relation || used.contains(&eid) {
                continue;
            }
            let mut fars = Vec::new();
            if (kind == 0 || kind == 2) && src == near {
                fars.push(dst);
            }
            if (kind == 1 || kind == 2) && dst == near && !(kind == 2 && src == near) {
                fars.push(src);
            }
            for far in fars {
                if bound[to].is_some_and(|existing| existing != far) {
                    continue;
                }
                let previous = bound[to];
                bound[to] = Some(far);
                used.push(eid);
                walk(edges, &hops[1..], bound, used, ret, out);
                used.pop();
                bound[to] = previous;
            }
        }
    }
    let (_, hops, ret) = *shape;
    let width = 1 + hops
        .iter()
        .map(|&(from, to, ..)| from.max(to))
        .max()
        .unwrap();
    let mut out = Vec::new();
    let mut bound = vec![None; width];
    bound[0] = Some(VId(0));
    walk(&edges, hops, &mut bound, &mut Vec::new(), ret, &mut out);
    out.sort_unstable();
    out
}

/// Bound multi-hop answers equal brute-force bindings for chains that turn
/// around, change relation, branch from the root, go undirected and close
/// a cycle, over random graphs with parallel edges, self-loops and deletes.
#[test]
fn bound_multi_hop_closures_answer_every_direction_relation_and_cycle_shape() {
    for (lab_seed, graph_seed) in [(0xa26_0101, 5_u64), (0xa26_0102, 0x5eed), (0xa26_0103, 77)] {
        let ((), report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut rng = Rng(graph_seed | 1);
            let mut seqs = Vec::new();
            let mut next_eid = 1_u128;
            for round in 0..3 {
                for relation in [R, S] {
                    let mut batch = WriteBatch::new(relation);
                    if round == 0 && relation == R {
                        batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
                        for id in 1..10_u128 {
                            batch.create_vertex(VId(id), vec![], vec![]);
                        }
                    }
                    if round == 2 && relation == R {
                        batch.delete_edge(EId(1));
                        batch.delete_edge(EId(4));
                    }
                    for _ in 0..14 {
                        let src = VId(u128::from(rng.next() % 10));
                        let dst = VId(u128::from(rng.next() % 10));
                        // Keep the bound vertex busy in both directions.
                        let (src, dst) = match rng.next() % 4 {
                            0 => (VId(0), dst),
                            1 => (src, VId(0)),
                            _ => (src, dst),
                        };
                        batch.add_edge(EId(next_eid), src, dst, vec![]);
                        next_eid += 1;
                    }
                    seqs.push(db.write(&commit, batch).await.unwrap());
                }
            }
            let symbols = |kind: fgdb_gql::GraphSymbolKind, name: &str| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                    Some(fgdb_gql::GraphSymbol::Relation(R))
                }
                (fgdb_gql::GraphSymbolKind::Relation, "S") => {
                    Some(fgdb_gql::GraphSymbol::Relation(S))
                }
                (fgdb_gql::GraphSymbolKind::Property, "n") => {
                    Some(fgdb_gql::GraphSymbol::Property(N))
                }
                _ => None,
            };
            let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
            let mut nonempty = vec![false; SHAPES.len()];
            for at in &seqs {
                for (index, shape) in SHAPES.iter().enumerate() {
                    let query = fgdb_gql::PreparedGraphText::prepare(shape.0, symbols)
                        .unwrap()
                        .bind_parameters(&args)
                        .unwrap();
                    let run = db
                        .execute_graph_pattern_governed_at(
                            &contexts.query(),
                            &query,
                            *at,
                            GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                        )
                        .unwrap();
                    let mut bag: Vec<VId> = run
                        .value
                        .iter()
                        .map(|row| row.values()[0].as_vertex().unwrap())
                        .collect();
                    bag.sort_unstable();
                    assert_eq!(bag, shape_oracle(&db, *at, shape), "{} at {at:?}", shape.0);
                    nonempty[index] |= !bag.is_empty();
                }
            }
            assert!(
                nonempty.iter().all(|seen| *seen),
                "seed {graph_seed}: a shape never produced rows: {nonempty:?}"
            );
        });
        assert!(report.lab_test_passed(), "seed {graph_seed}: {report:?}");
    }
}

/// A bound two-hop admits the root's out-edges and its neighbours' out-edges
/// of the hop relation, not every edge incident to a neighbour: in-edges into
/// the neighbours and edges past the last hop leave the charge unchanged.
#[test]
fn bound_two_hop_charges_ignore_edges_off_each_steps_direction() {
    let mut charged = Vec::new();
    for (lab_seed, noise) in [(0xa26_0111_u64, 30_u128), (0xa26_0112, 3_000)] {
        let (result, report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
            // Root -> b1..b4; each b -> two c; pool vertices 100.. feed noise.
            for id in 1..=12_u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            for id in 100..120_u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            let mut eid = 1_u128;
            for b in 1..=4_u128 {
                batch.add_edge(EId(eid), VId(0), VId(b), vec![]);
                eid += 1;
                for c in [4 + 2 * b - 1, 4 + 2 * b] {
                    batch.add_edge(EId(eid), VId(b), VId(c), vec![]);
                    eid += 1;
                }
            }
            for index in 0..noise {
                let pool = VId(100 + index % 20);
                // Into a neighbour (off its Forward step), or out of a last-hop c.
                let (src, dst) = if index % 2 == 0 {
                    (pool, VId(1 + index % 4))
                } else {
                    (VId(5 + index % 8), pool)
                };
                batch.add_edge(EId(eid), src, dst, vec![]);
                eid += 1;
            }
            let at = db.write(&commit, batch).await.unwrap();
            let names = RelationBind::new()
                .with_relation("R", R)
                .with_property("n", N);
            let query = PreparedGqlTemplate::prepare(SHAPES[0].0, &names)
                .unwrap()
                .bind_parameters(&GqlParameters::new().with_int64("n", BOUND).unwrap())
                .unwrap();
            let run = db
                .execute_prepared_query_governed_at(
                    &contexts.query(),
                    &query,
                    at,
                    GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                )
                .unwrap();
            (
                run.rows.snapshot_records,
                run.evaluator.work_units,
                run.value,
            )
        });
        assert!(report.lab_test_passed(), "noise {noise}: {report:?}");
        charged.push(result);
    }
    assert_eq!(charged[0].2, (5..=12).map(VId).collect::<Vec<_>>());
    assert_eq!(
        charged[0], charged[1],
        "records, work and rows must not see the noise"
    );
}

/// A bound shape with quantified hops: text, then hops
/// `(from, to, relation, kind, minimum, maximum)` (kind as in [`Shape`]; a
/// plain edge is `1..=1`), then the returned variable.
type WalkShape = (
    &'static str,
    &'static [(usize, usize, RelationId, u8, u32, u32)],
    usize,
);

const WALK_SHAPES: &[WalkShape] = &[
    (
        "MATCH (a {n: $n})-[:R]->{1,3}(b) RETURN b",
        &[(0, 1, R, 0, 1, 3)],
        1,
    ),
    (
        "MATCH (a {n: $n})<-[:S]-{0,2}(b) RETURN b",
        &[(0, 1, S, 1, 0, 2)],
        1,
    ),
    (
        "MATCH (a {n: $n})-[:R]-{2,3}(b) RETURN b",
        &[(0, 1, R, 2, 2, 3)],
        1,
    ),
    (
        "MATCH (a {n: $n})-[:R]->{1,2}(b)-[:S]->(c) RETURN c",
        &[(0, 1, R, 0, 1, 2), (1, 2, S, 0, 1, 1)],
        2,
    ),
    (
        "MATCH (a {n: $n})-[:R]->(b)-[:S]-{1,2}(c) RETURN c",
        &[(0, 1, R, 0, 1, 1), (1, 2, S, 2, 1, 2)],
        2,
    ),
];

/// Brute-force bindings with DIFFERENT EDGES across the whole pattern,
/// including repetitions inside a quantified hop, as the sorted bag of the
/// returned vertex: one binding per distinct sequence of edges.
fn walk_oracle(db: &Database<MemVfs>, at: CommitSeq, shape: &WalkShape) -> Vec<VId> {
    type Edge = (EId, VId, RelationId, VId);
    type Hop = (usize, usize, RelationId, u8, u32, u32);
    let edges: Vec<Edge> = db
        .edges_at(at)
        .unwrap()
        .into_iter()
        .map(|record| {
            let entry = record.entry;
            (entry.eid, entry.src, entry.relation, entry.dst)
        })
        .collect();
    fn steps(edges: &[Edge], near: VId, relation: RelationId, kind: u8) -> Vec<(EId, VId)> {
        let mut out = Vec::new();
        for &(eid, src, rel, dst) in edges {
            if rel != relation {
                continue;
            }
            if (kind == 0 || kind == 2) && src == near {
                out.push((eid, dst));
            } else if (kind == 1 || kind == 2) && dst == near {
                out.push((eid, src));
            }
        }
        out
    }
    #[allow(clippy::too_many_arguments)]
    fn hop(
        edges: &[Edge],
        hops: &[Hop],
        at: VId,
        length: u32,
        bound: &mut Vec<Option<VId>>,
        used: &mut Vec<EId>,
        ret: usize,
        out: &mut Vec<VId>,
    ) {
        let (_, to, relation, kind, minimum, maximum) = hops[0];
        if length >= minimum && bound[to].is_none_or(|existing| existing == at) {
            let previous = bound[to];
            bound[to] = Some(at);
            pattern(edges, &hops[1..], bound, used, ret, out);
            bound[to] = previous;
        }
        if length == maximum {
            return;
        }
        for (eid, far) in steps(edges, at, relation, kind) {
            if used.contains(&eid) {
                continue;
            }
            used.push(eid);
            hop(edges, hops, far, length + 1, bound, used, ret, out);
            used.pop();
        }
    }
    fn pattern(
        edges: &[Edge],
        hops: &[Hop],
        bound: &mut Vec<Option<VId>>,
        used: &mut Vec<EId>,
        ret: usize,
        out: &mut Vec<VId>,
    ) {
        let Some(&(from, ..)) = hops.first() else {
            out.push(bound[ret].unwrap());
            return;
        };
        let near = bound[from].unwrap();
        hop(edges, hops, near, 0, bound, used, ret, out);
    }
    let (_, hops, ret) = *shape;
    let width = 1 + hops
        .iter()
        .map(|&(from, to, ..)| from.max(to))
        .max()
        .unwrap();
    let mut out = Vec::new();
    let mut bound = vec![None; width];
    bound[0] = Some(VId(0));
    pattern(&edges, hops, &mut bound, &mut Vec::new(), ret, &mut out);
    out.sort_unstable();
    out
}

/// Quantified reaches from a bound vertex (and a quantified hop after a bound
/// edge) admit only their per-step walk and return exactly the brute-force
/// bindings: forward, incoming, undirected, zero-length and turn-around walks
/// over random graphs with parallel edges, self-loops and deletes.
#[test]
fn bound_quantified_reaches_answer_every_direction_and_length() {
    for (lab_seed, graph_seed) in [(0xa26_0121, 9_u64), (0xa26_0122, 0xfeed), (0xa26_0123, 31)] {
        let ((), report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut rng = Rng(graph_seed | 1);
            let mut seqs = Vec::new();
            let mut next_eid = 1_u128;
            for round in 0..3 {
                for relation in [R, S] {
                    let mut batch = WriteBatch::new(relation);
                    if round == 0 && relation == R {
                        batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
                        for id in 1..9_u128 {
                            batch.create_vertex(VId(id), vec![], vec![]);
                        }
                    }
                    if round == 2 && relation == R {
                        batch.delete_edge(EId(2));
                        batch.delete_edge(EId(5));
                    }
                    for _ in 0..9 {
                        let src = VId(u128::from(rng.next() % 9));
                        let dst = VId(u128::from(rng.next() % 9));
                        let (src, dst) = match rng.next() % 4 {
                            0 => (VId(0), dst),
                            1 => (src, VId(0)),
                            _ => (src, dst),
                        };
                        batch.add_edge(EId(next_eid), src, dst, vec![]);
                        next_eid += 1;
                    }
                    seqs.push(db.write(&commit, batch).await.unwrap());
                }
            }
            let symbols = |kind: fgdb_gql::GraphSymbolKind, name: &str| match (kind, name) {
                (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                    Some(fgdb_gql::GraphSymbol::Relation(R))
                }
                (fgdb_gql::GraphSymbolKind::Relation, "S") => {
                    Some(fgdb_gql::GraphSymbol::Relation(S))
                }
                (fgdb_gql::GraphSymbolKind::Property, "n") => {
                    Some(fgdb_gql::GraphSymbol::Property(N))
                }
                _ => None,
            };
            let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
            let mut nonempty = vec![false; WALK_SHAPES.len()];
            for at in &seqs {
                for (index, shape) in WALK_SHAPES.iter().enumerate() {
                    let query = fgdb_gql::PreparedGraphText::prepare(shape.0, symbols)
                        .unwrap()
                        .bind_parameters(&args)
                        .unwrap();
                    let run = db
                        .execute_graph_pattern_governed_at(
                            &contexts.query(),
                            &query,
                            *at,
                            GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                        )
                        .unwrap();
                    let mut bag: Vec<VId> = run
                        .value
                        .iter()
                        .map(|row| row.values()[0].as_vertex().unwrap())
                        .collect();
                    bag.sort_unstable();
                    assert_eq!(bag, walk_oracle(&db, *at, shape), "{} at {at:?}", shape.0);
                    nonempty[index] |= !bag.is_empty();
                }
            }
            assert!(
                nonempty.iter().all(|seen| *seen),
                "seed {graph_seed}: a shape never produced rows: {nonempty:?}"
            );
        });
        assert!(report.lab_test_passed(), "seed {graph_seed}: {report:?}");
    }
}

/// A bound three-hop reach admits the root's walk, not the graph: records,
/// work and rows stay constant while an unreachable component grows.
#[test]
fn bound_reach_charges_ignore_an_unreachable_component() {
    let mut charged = Vec::new();
    for (lab_seed, noise) in [(0xa26_0131_u64, 40_u128), (0xa26_0132, 4_000)] {
        let (result, report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
            for id in 1..=6_u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            // 0 -> 1 -> 2 -> 3 -> 4 (the fourth hop is past the bound), 1 -> 5.
            for (eid, src, dst) in [(1, 0, 1), (2, 1, 2), (3, 2, 3), (4, 3, 4), (5, 1, 5)] {
                batch.add_edge(EId(eid), VId(src), VId(dst), vec![]);
            }
            for id in 100..140_u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
            for index in 0..noise {
                let src = VId(100 + index % 40);
                let dst = VId(100 + (index * 7 + 3) % 40);
                batch.add_edge(EId(1_000 + index), src, dst, vec![]);
            }
            let at = db.write(&commit, batch).await.unwrap();
            let query = fgdb_gql::PreparedGraphText::prepare(
                "MATCH (a {n: $n})-[:R]->{1,3}(b) RETURN b",
                |kind: fgdb_gql::GraphSymbolKind, name: &str| match (kind, name) {
                    (fgdb_gql::GraphSymbolKind::Relation, "R") => {
                        Some(fgdb_gql::GraphSymbol::Relation(R))
                    }
                    (fgdb_gql::GraphSymbolKind::Property, "n") => {
                        Some(fgdb_gql::GraphSymbol::Property(N))
                    }
                    _ => None,
                },
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new().with_int64("n", BOUND).unwrap())
            .unwrap();
            let run = db
                .execute_graph_pattern_governed_at(
                    &contexts.query(),
                    &query,
                    at,
                    GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                )
                .unwrap();
            let mut ids: Vec<VId> = run
                .value
                .iter()
                .map(|row| row.values()[0].as_vertex().unwrap())
                .collect();
            ids.sort_unstable();
            (run.rows.snapshot_records, run.evaluator.work_units, ids)
        });
        assert!(report.lab_test_passed(), "noise {noise}: {report:?}");
        charged.push(result);
    }
    assert_eq!(charged[0].2, [1, 2, 3, 5].map(VId).to_vec());
    assert_eq!(
        charged[0], charged[1],
        "records, work and rows must not see the noise"
    );
}
