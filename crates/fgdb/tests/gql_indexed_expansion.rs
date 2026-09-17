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
const INCOMING_TEXT: &str = "MATCH (b)<-[:R]-(a) WHERE a.n = $n RETURN b";
const UNDIRECTED_TEXT: &str = "MATCH (a)-[:R]-(b) WHERE a.n = $n RETURN b";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa2; 32], DatabaseSecurityNamespaceId([0xa3; 32]), [0xa4; 32])
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

/// >=5 commits mixing vertex creates, edge creates, edge deletes and property
/// updates. Property updates keep exactly one vertex per seed bound to the
/// property value the queries select on, so bounded answers stay stable.
async fn generated(seed: u64, db: &mut Database<MemVfs>, cx: &fgdb_types::CommitCx) -> Vec<CommitSeq> {
    let mut rng = Rng(seed | 1);
    let mut seqs = Vec::new();
    let mut next_eid = 1_000_u128;
    let mut live: Vec<EId> = Vec::new();
    for commit in 0..6_u64 {
        let mut batch = WriteBatch::new(R);
        if commit == 0 {
            batch.create_vertex(
                VId(0),
                vec![],
                vec![(N, CanonicalScalar::Int(BOUND))],
            );
            for id in 1..12_u128 {
                batch.create_vertex(VId(id), vec![], vec![]);
            }
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
                    batch.set_vertex_property(VId(id), N, Some(CanonicalScalar::Int(commit as i64)));
                }
                _ => {
                    let eid = EId(next_eid);
                    next_eid += 1;
                    // 50% of edges touch the bound vertex, giving every commit
                    // both in- and out-degree changes on VId(0).
                    let (src, dst) = if rng.next() % 2 == 0 {
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
fn oracle(db: &Database<MemVfs>, at: CommitSeq, bound: VId, forward: Option<bool>) -> Vec<(VId, VId)> {
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
    for (lab_seed, graph_seed) in [(0xa26_0001, 1_u64), (0xa26_0002, 2), (0xa26_0003, 0xdead_beef)] {
        let ((), report) = run_async_under_lab(lab_seed, move |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let seqs = generated(graph_seed, &mut db, &commit).await;
            let names = RelationBind::new().with_relation("R", R).with_property("n", N);
            let bound_template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
            let incoming_template = PreparedGqlTemplate::prepare(INCOMING_TEXT, &names).unwrap();
            let undirected_template = PreparedGqlTemplate::prepare(UNDIRECTED_TEXT, &names).unwrap();
            let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
            let mut saw_nonempty = false;
            let mut saw_deleted_filtered = false;
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
                        .map(|(_, other)| other)
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    let rows = db.execute_prepared_query_governed_at(
                        &contexts.query(),
                        &query,
                        *at,
                        GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000),
                    ).unwrap();
                    assert_eq!(rows.value, expected, "at={:?} forward={forward:?}", at);
                    saw_nonempty |= !rows.value.is_empty();
                }
            }
            // Deleted edges must actually filter: every commit after the
            // first deletes edges, so bound-incident triples ever created
            // must exceed what remains visible at the final cut.
            let final_at = *seqs.last().unwrap();
            let mut ever_incident = 0_u64;
            let mut visible_incident = 0_u64;
            let mut seen_eids = std::collections::BTreeSet::new();
            for record in db.edges_at(final_at).unwrap() {
                let entry = &record.entry;
                if entry.relation == R && (entry.src == VId(0) || entry.dst == VId(0)) {
                    seen_eids.insert(entry.eid);
                }
            }
            for eid in seen_eids {
                ever_incident += 1;
                if db.edge_at(eid, final_at).unwrap().is_some() {
                    visible_incident += 1;
                }
            }
            saw_deleted_filtered = ever_incident > visible_incident;
            assert!(
                saw_deleted_filtered,
                "seed {graph_seed}: no deleted edge was ever filtered"
            );
            assert!(
                saw_nonempty,
                "seed {graph_seed}: differential produced no rows"
            );
        });
        assert!(report.lab_test_passed(), "seed {graph_seed}: {report:?}");
    }
}

/// Expanding from the bound vertex charges Work/SnapshotRecord events
/// proportional to its degree d: identical neighbourhoods on a 1k-edge and a
/// 50k-edge graph produce identical charged counts for the same bound query.
#[test]
fn bound_degree_charges_stay_constant_as_unrelated_edges_grow() {
    let ((), report) = run_async_under_lab(0xa26_0042, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let query_cx = contexts.query();
        let names = RelationBind::new().with_relation("R", R).with_property("n", N);
        let template = PreparedGqlTemplate::prepare(ID_TEXT, &names).unwrap();
        let args = GqlParameters::new().with_int64("n", BOUND).unwrap();
        let query = template.bind_parameters(&args).unwrap();
        let wide = GqlQueryPolicy::new(1_000_000, 1_000_000, 10_000_000, 10_000_000);
        // Identical degree-20 bound neighbourhood on both sizes.
        let mut small = Database::open_memory(&commit, keys()).await.unwrap();
        let at_small = graph_of_degree(&mut small, &commit, 1_000, 20).await;
        let mut large = Database::open_memory(&commit, keys()).await.unwrap();
        let at_large = graph_of_degree(&mut large, &commit, 50_000, 20).await;
        assert_eq!(at_small, at_large);
        let one = small
            .execute_prepared_query_governed_at(&query_cx, &query, at_small, wide)
            .unwrap();
        let two = large
            .execute_prepared_query_governed_at(&query_cx, &query, at_large, wide)
            .unwrap();
        // Admission is degree-proportional: the source Work and SnapshotRecord
        // charges must be identical; the scan path would charge |E| records.
        assert_eq!(one.rows.snapshot_records, two.rows.snapshot_records);
        assert_eq!(one.evaluator.work_units, two.evaluator.work_units);
        assert_eq!(one.value, two.value);
        assert_eq!(one.rows.snapshot_records, 10, "forward bound lookup charges out-degree");
        assert!(one.evaluator.work_units > 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

/// One commit building `total` edges where the bound vertex owns exactly
/// `degree` of them (alternating in/out), everything else fanned from other
/// vertices. Returns the commit sequence.
async fn graph_of_degree(
    db: &mut Database<MemVfs>,
    cx: &fgdb_types::CommitCx,
    total: usize,
    degree: usize,
) -> CommitSeq {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(0), vec![], vec![(N, CanonicalScalar::Int(BOUND))]);
    let others = (total / 8).max(4);
    for id in 1..=others as u128 {
        batch.create_vertex(VId(id), vec![], vec![]);
    }
    for at in 0..total {
        let eid = EId(50_000_000 + at as u128);
        let (src, dst) = if at < degree {
            if at % 2 == 0 {
                (VId(0), VId(1 + (at as u128) % (others as u128)))
            } else {
                (VId(1 + (at as u128) % (others as u128)), VId(0))
            }
        } else {
            let a = 1 + (at as u128) % (others as u128);
            let b = 1 + ((at * 7 + 3) as u128) % (others as u128);
            (VId(a), VId(b))
        };
        batch.add_edge(eid, src, dst, vec![]);
    }
    db.write(cx, batch).await.unwrap()
}
