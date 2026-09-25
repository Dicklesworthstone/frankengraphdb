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
