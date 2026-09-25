//! Native transaction search, independent BFS expectations and real commit
//! completion. No alternative storage model is used by the product path.
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, WriteBatch, WriteError, WriteTxn, WriteTxnError};
use fgdb_beacon::expansion::{ExpansionDirection as Direction, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{ReadError, ReadOptions};
use fgdb_beacon::{
    BeaconError, DistanceMetric, ExactHybridQuery, ExactRrfProfile, GraphHybridHit,
    GraphHybridQuery, HnswConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

type Options = ReadOptions<PropertyKeyId, LabelId>;
type Error = ReadError<WriteTxnError, Box<asupersync::error::Error>>;
const L: LabelId = LabelId(1);
const OTHER: LabelId = LabelId(9);
const T: PropertyKeyId = PropertyKeyId(1);
const X: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const R2: RelationId = RelationId(2);
const HIGH: VId = VId((1_u128 << 100) + 3);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xe1; 32], DatabaseSecurityNamespaceId([0xe2; 32]), [0xe3; 32])
}
fn scalar_text(value: &str) -> CanonicalScalar {
    CanonicalScalar::ucs_basic_text(value).unwrap()
}
fn props(value: i64) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    vec![(T, scalar_text("graph")), (X, CanonicalScalar::Int(value))]
}
fn options() -> Options {
    let mut options = Options::text(T);
    options.vertex_label = Some(L);
    options.projection.vector = vec![X];
    options.index.vector = Some(HnswConfig::new(1, DistanceMetric::SquaredEuclidean));
    options
}
fn query(mode: VectorSearch) -> GraphHybridQuery<'static> {
    GraphHybridQuery {
        retrieval: ExactHybridQuery {
            vector: &[0.0], text: "graph", k: 16,
            vector_candidates: 16, text_candidates: 16,
            vector_mode: mode, text_mode: TextMatch::Any,
            profile: ExactRrfProfile::default(),
        },
        graph_candidates: 16, graph_weight: 100,
    }
}
fn expansion(seeds: &[VId]) -> ExpansionSpec<'_, RelationId> {
    ExpansionSpec {
        seeds, relation: None, direction: Direction::Outgoing,
        max_hops: 3, include_seeds: false, limits: ExpansionLimits::default(),
    }
}
fn hops(rows: &[GraphHybridHit]) -> BTreeMap<VId, u32> {
    rows.iter().filter_map(|hit| hit.graph_hops.map(|distance| (hit.id, distance))).collect()
}
fn conflict(result: Result<fgdb_types::EmbeddedTxnCompletion, WriteTxnError>) {
    assert!(matches!(result,
        Err(WriteTxnError::Write(WriteError::FirstCommitterWins {
            law: "FG-LAW-FCW-READ-01", ..
        }))
    ), "expected read conflict, got {result:?}");
}

async fn small(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(R);
    for id in 1..=3 {
        batch.create_vertex(VId(id), vec![L], props(id as i64));
    }
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch.add_edge(EId(2), VId(1), VId(2), vec![]);
    batch.add_edge(EId(9), VId(2), VId(3), vec![]);
    db.write(cx, batch).await.unwrap();
    db
}

// Deliberately simple independent traversal over explicit expected incidences.
// It uses neither ExpansionGraph nor native snapshot/transaction edge readers.
fn bfs(edges: &[(VId, RelationId, VId)], spec: ExpansionSpec<'_, RelationId>) -> BTreeMap<VId, u32> {
    let vertices: BTreeSet<_> = [VId(1), VId(2), VId(4), VId(5), VId(7), HIGH].into();
    let mut result = BTreeMap::new();
    let mut queue = VecDeque::new();
    for &seed in spec.seeds {
        if vertices.contains(&seed) && result.insert(seed, 0).is_none() {
            queue.push_back(seed);
        }
    }
    while let Some(at) = queue.pop_front() {
        let distance = result[&at];
        if distance == spec.max_hops { continue; }
        for &(src, relation, dst) in edges {
            if spec.relation.is_some_and(|wanted| wanted != relation) { continue; }
            for (from, to, enabled) in [
                (src, dst, spec.direction != Direction::Incoming),
                (dst, src, spec.direction != Direction::Outgoing),
            ] {
                if enabled && from == at && vertices.contains(&to) && !result.contains_key(&to) {
                    result.insert(to, distance + 1);
                    queue.push_back(to);
                }
            }
        }
    }
    if !spec.include_seeds { result.retain(|_, distance| *distance != 0); }
    result
}

#[derive(Clone)]
struct Case {
    direction: Direction,
    relation: Option<RelationId>,
    seeds: Vec<VId>,
    max_hops: u32,
    include_seeds: bool,
    mode: VectorSearch,
}
impl Case {
    fn spec(&self) -> ExpansionSpec<'_, RelationId> {
        ExpansionSpec {
            seeds: &self.seeds, relation: self.relation, direction: self.direction,
            max_hops: self.max_hops, include_seeds: self.include_seeds,
            limits: ExpansionLimits::default(),
        }
    }
}
fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for mode in [VectorSearch::Exact, VectorSearch::Approximate { ef_search: 32 }] {
        for direction in [Direction::Outgoing, Direction::Incoming, Direction::Undirected] {
            for relation in [None, Some(R), Some(R2)] {
                for (seeds, max_hops, include_seeds) in [
                    (vec![VId(1)], 3, false),
                    (vec![HIGH, HIGH, VId(999)], 3, true),
                    (vec![VId(1), VId(5)], 0, true),
                    (vec![], 3, false),
                ] {
                    cases.push(Case { direction, relation, seeds, max_hops, include_seeds, mode });
                }
            }
        }
    }
    cases
}

#[test]
fn three_seed_canonical_overlays_match_independent_bfs_and_committed_fusion() {
    for seed in [3, 17, 101] {
        let ((), report) = run_async_under_lab(0xbeac_3200 + seed, move |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let commit = c.commit();
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut base = WriteBatch::new(R);
            for id in 1..=4 {
                base.create_vertex(VId(id), if id == 4 { vec![OTHER] } else { vec![L] },
                    props(id as i64 + seed as i64));
            }
            base.create_vertex(VId(5), vec![L], vec![]);
            base.create_vertex(HIGH, vec![L], props(100));
            for (eid, src, dst) in [(10, VId(1), VId(2)), (11, VId(1), VId(2)),
                (12, VId(2), VId(3)), (13, VId(3), HIGH), (14, HIGH, VId(1)),
                (15, VId(1), VId(5)), (16, VId(5), VId(3))] {
                base.add_edge(EId(eid), src, dst, vec![]);
            }
            db.write(&commit, base).await.unwrap();
            let mut other = WriteBatch::new(R2);
            other.add_edge(EId(90), VId(1), HIGH, vec![]);
            db.write(&commit, other).await.unwrap();
            let mut txn = db.begin(&c.txn()).unwrap();
            let mut change = WriteBatch::new(R);
            change.ensure_edge_by_triple(EId(500), VId(1), VId(2), vec![]);
            change.delete_edge(EId(10));
            change.delete_vertex(VId(3));
            change.set_vertex_label(VId(4), L, true);
            change.set_vertex_property(VId(1), T, Some(scalar_text("graph graph updated")));
            change.set_vertex_property(HIGH, X, Some(CanonicalScalar::Int(0)));
            change.create_vertex(VId(7), vec![L], vec![]);
            for (eid, src, dst) in [(20, VId(2), VId(4)), (21, VId(4), HIGH),
                (22, VId(5), VId(7)), (23, VId(7), HIGH)] {
                change.add_edge(EId(eid), src, dst, vec![]);
            }
            let extra = match seed % 3 { 0 => (HIGH, VId(5)), 1 => (VId(4), VId(1)), _ => (VId(5), VId(2)) };
            change.add_edge(EId(24), extra.0, extra.1, vec![]);
            change.add_edge(EId(600), VId(1), HIGH, vec![]);
            change.delete_edge(EId(600));
            let mut other = WriteBatch::new(R2);
            other.delete_edge(EId(90));
            other.add_edge(EId(91), VId(2), HIGH, vec![]);
            txn.write_ordered(&mut db, vec![change, other]).unwrap();
            let basis = db.frontier().unwrap();
            let edges = [
                (VId(1), R, VId(2)), (HIGH, R, VId(1)), (VId(1), R, VId(5)),
                (VId(2), R, VId(4)), (VId(4), R, HIGH), (VId(5), R, VId(7)),
                (VId(7), R, HIGH), (extra.0, R, extra.1), (VId(2), R2, HIGH),
            ];
            let cases = cases();
            let mut answers = Vec::new();
            for case in &cases {
                let rows = txn.beacon_search_graph(&db, &c.query(), &options(), query(case.mode), case.spec()).unwrap();
                assert_eq!(hops(&rows), bfs(&edges, case.spec()), "seed={seed}");
                assert!(rows.iter().all(|hit| hit.id != VId(3)));
                answers.push(rows);
            }
            assert_eq!(db.frontier().unwrap(), basis, "reads must not publish");
            txn.commit(&mut db, &commit).await.unwrap();
            for (case, expected) in cases.iter().zip(answers) {
                assert_eq!(db.beacon_search_graph(&c.query(), &options(), query(case.mode), case.spec()).unwrap(),
                    expected, "complete score/rank agreement at seed={seed}");
            }
            assert_eq!(c.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[test]
fn unused_ensure_identity_never_overwrites_an_existing_edge_and_cascades_are_exact() {
    let ((), report) = run_async_under_lab(0xbeac_3301, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = small(&c.commit()).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut change = WriteBatch::new(R);
        // EId(9) already denotes 2->3. An ensure of the EXISTING 1->2 triple
        // must ignore that requested identity, not replace 2->3 with 1->2.
        change.ensure_edge_by_triple(EId(9), VId(1), VId(2), vec![]);
        change.delete_edge(EId(1));
        change.ensure_edge_by_triple(EId(100), VId(1), VId(2), vec![]);
        txn.write(&mut db, change).unwrap();
        txn.savepoint(&db, "one-parallel-left").unwrap();
        let q = query(VectorSearch::Exact);
        let mut e = expansion(&[VId(1)]);
        e.limits.max_input_edges = 2;
        let original = txn.beacon_search_graph(&db, &c.query(), &options(), q, e).unwrap();
        assert_eq!(hops(&original), [(VId(2), 1), (VId(3), 2)].into());
        let mut too_small = e;
        too_small.limits.max_input_edges = 1;
        assert!(matches!(txn.beacon_search_graph(&db, &c.query(), &options(), q, too_small),
            Err(ReadError::Index(BeaconError::ResourceLimit { resource: "expansion input edges", limit: 1 }))));
        let mut last = WriteBatch::new(R);
        last.delete_edge(EId(2));
        txn.write(&mut db, last).unwrap();
        assert!(hops(&txn.beacon_search_graph(&db, &c.query(), &options(), q, e).unwrap()).is_empty());
        txn.rollback_to_savepoint(&db, "one-parallel-left").unwrap();
        assert_eq!(txn.beacon_search_graph(&db, &c.query(), &options(), q, e).unwrap(), original);
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(2));
        txn.write(&mut db, cascade).unwrap();
        e.limits.max_input_edges = 0;
        let rows = txn.beacon_search_graph(&db, &c.query(), &options(), q, e).unwrap();
        assert!(hops(&rows).is_empty());
        assert!(rows.iter().all(|hit| hit.id != VId(2)));
        txn.commit(&mut db, &c.commit()).await.unwrap();
        assert_eq!(db.beacon_search_graph(&c.query(), &options(), q, e).unwrap(), rows);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn later_commits_cannot_change_pinned_scores_or_topology_but_invalidate_completion() {
    let ((), report) = run_async_under_lab(0xbeac_3302, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = small(&c.commit()).await;
        let mut txn = db.begin(&c.txn()).unwrap();
        let mut o = options();
        o.as_of = Some(txn.basis());
        let q = query(VectorSearch::Exact);
        let e = expansion(&[VId(1)]);
        let before = txn.beacon_search_graph(&db, &c.query(), &o, q, e).unwrap();
        let mut winner = WriteBatch::new(R);
        winner.delete_edge(EId(9));
        winner.add_edge(EId(30), VId(3), VId(1), vec![]);
        winner.set_vertex_property(VId(3), T, Some(scalar_text("different corpus")));
        db.write(&c.commit(), winner).await.unwrap();
        assert_eq!(txn.beacon_search_graph(&db, &c.query(), &o, q, e).unwrap(), before);
        assert_ne!(db.beacon_search_graph(&c.query(), &options(), q, e).unwrap(), before);
        conflict(txn.finish(&mut db, &c.commit()).await);
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_and_failed_scans_keep_edge_phantoms_after_rollback_but_unused_lanes_do_not() {
    for case in 0..5 {
        let ((), report) = run_async_under_lab(0xbeac_3310 + case, move |root| async move {
            let c = PurposeContexts::narrow_runtime_root(&root);
            let mut db = if case == 1 { small(&c.commit()).await }
                else { Database::open_memory(&c.commit(), keys()).await.unwrap() };
            let mut txn = db.begin(&c.txn()).unwrap();
            txn.savepoint(&db, "before-read").unwrap();
            let mut q = query(VectorSearch::Exact);
            let mut e = expansion(&[VId(1)]);
            if case == 1 { e.limits.max_input_edges = 0; }
            if case == 2 { q.graph_weight = 0; }
            if case == 3 { e.max_hops = 0; }
            if case == 4 { e.seeds = &[]; }
            let result = txn.beacon_search_graph(&db, &c.query(), &options(), q, e);
            if case == 1 {
                assert!(matches!(result, Err(ReadError::Index(BeaconError::ResourceLimit {
                    resource: "expansion input edges", limit: 0,
                }))));
            } else { assert!(result.unwrap().is_empty()); }
            txn.rollback_to_savepoint(&db, "before-read").unwrap();
            // New vertices have an unrelated label and were never observed.
            // Only the conservative edge-scan witness can reject this commit.
            let mut winner = WriteBatch::new(R2);
            winner.create_vertex(VId(80), vec![OTHER], vec![]);
            winner.create_vertex(VId(81), vec![OTHER], vec![]);
            winner.add_edge(EId(800), VId(80), VId(81), vec![]);
            db.write(&c.commit(), winner).await.unwrap();
            let completion = txn.finish(&mut db, &c.commit()).await;
            if case <= 1 { conflict(completion); }
            else { assert!(completion.is_ok(), "unused graph lane gained a dependency: {completion:?}"); }
            assert_eq!(c.outstanding_obligations(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

#[test]
fn every_capacity_and_context_refusal_leaves_search_retryable_and_unpublished() {
    let ((), report) = run_async_under_lab(0xbeac_3303, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = small(&c.commit()).await;
        let other = Database::open_memory(&c.commit(), keys()).await.unwrap();
        let mut txn = db.begin(&c.txn()).unwrap();
        let q = query(VectorSearch::Exact);
        let e = expansion(&[VId(1)]);
        let expected = txn.beacon_search_graph(&db, &c.query(), &options(), q, e).unwrap();
        let basis = db.frontier().unwrap();
        for case in 0..9 {
            let mut o = options();
            let mut limited = e;
            match case {
                0 => o.policy.max_work_units = 0,
                1 => o.policy.max_source_scratch = 0,
                2 => o.policy.max_staging_rows = 0,
                3 => o.policy.max_result_rows = 0,
                4 => limited.limits.max_vertices = 0,
                5 => limited.limits.max_input_edges = 0,
                6 => limited.limits.max_visited_vertices = 0,
                7 => limited.limits.max_seed_ids = 0,
                _ => limited.limits.max_source_scratch = 0,
            }
            let result = txn.beacon_search_graph(&db, &c.query(), &o, q, limited);
            assert!(matches!(result, Err(ReadError::Index(BeaconError::WorkBudgetExceeded))
                | Err(ReadError::Index(BeaconError::ResourceLimit { .. }))), "case={case}: {result:?}");
            assert_eq!(txn.beacon_search_graph(&db, &c.query(), &options(), q, e).unwrap(), expected);
            assert_eq!(db.frontier().unwrap(), basis);
        }
        let mut invalid = options();
        invalid.as_of = Some(CommitSeq(basis.0 + 1));
        assert!(matches!(txn.beacon_search_graph(&db, &c.query(), &invalid, q, e),
            Err(ReadError::Index(BeaconError::InvalidQuery(_)))));
        assert!(matches!(txn.beacon_search_graph(&other, &c.query(), &options(), q, e),
            Err(ReadError::Read(WriteTxnError::WrongDatabase))));
        txn.abort();
        assert!(matches!(txn.beacon_search_graph(&db, &c.query(), &options(), q, e),
            Err(ReadError::Read(WriteTxnError::Finished))));
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn threshold(mut high: usize, mut admits: impl FnMut(usize) -> bool) -> usize {
    assert!(admits(high));
    let mut low = 0;
    while low < high {
        let middle = low + (high - low) / 2;
        if admits(middle) { high = middle; } else { low = middle + 1; }
    }
    low
}

#[test]
fn graph_work_shares_the_whole_query_allowance_with_an_exact_refusal_boundary() {
    let ((), report) = run_async_under_lab(0xbeac_3304, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = small(&c.commit()).await;
        let mut minima = Vec::new();
        for enabled in [false, true] {
            let mut q = query(VectorSearch::Exact);
            if !enabled { q.graph_weight = 0; }
            let mut attempt = |limit| -> Result<Vec<GraphHybridHit>, Error> {
                // Fresh observations at EVERY trial: a previous successful
                // read must not subsidize this trial's read-witness allocations.
                let mut txn = db.begin(&c.txn()).unwrap();
                let mut o = options();
                o.policy.max_work_units = limit;
                let result = txn.beacon_search_graph(&db, &c.query(), &o, q, expansion(&[VId(1)]));
                txn.abort();
                result
            };
            let minimum = threshold(1_000_000, |limit| {
                match attempt(limit) {
                    Ok(_) => true,
                    Err(ReadError::Index(BeaconError::WorkBudgetExceeded)) => false,
                    other => panic!("unexpected capacity refusal: {other:?}"),
                }
            });
            assert!(minimum > 1);
            assert!(attempt(minimum).is_ok());
            assert!(matches!(attempt(minimum - 1), Err(ReadError::Index(BeaconError::WorkBudgetExceeded))));
            minima.push(minimum);
        }
        assert!(minima[1] > minima[0], "graph work escaped the shared allowance");
        assert_eq!(c.outstanding_obligations(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_checkpoint_interrupts_without_publication_and_the_same_transaction_can_retry() {
    let ((), report) = run_async_under_lab(0xbeac_3305, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let mut db = small(&c.commit()).await;
        let basis = db.frontier().unwrap();
        let q = query(VectorSearch::Exact);
        let o = options();
        let e = expansion(&[VId(1)]);
        let stage = |txn: &mut WriteTxn, db: &mut Database<MemVfs>| {
            let mut changes = WriteBatch::new(R);
            changes.delete_edge(EId(1));
            changes.create_vertex(VId(4), vec![L], vec![]);
            changes.add_edge(EId(30), VId(3), VId(4), vec![]);
            txn.write(db, changes).unwrap();
        };
        let mut baseline = db.begin(&c.txn()).unwrap();
        stage(&mut baseline, &mut db);
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let expected = baseline.beacon_search_graph(&db, &c.query().with_checkpoint_probe(probe.clone()), &o, q, e).unwrap();
        let calls = probe.calls();
        baseline.abort();
        assert!(calls > 20);
        for cut in 1..=calls {
            let mut txn = db.begin(&c.txn()).unwrap();
            stage(&mut txn, &mut db);
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(cut)));
            let result = txn.beacon_search_graph(&db, &c.query().with_checkpoint_probe(probe.clone()), &o, q, e);
            assert!(matches!(result, Err(ReadError::Interrupted(_))), "cut={cut}: {result:?}");
            assert_eq!(probe.calls(), cut, "continued after interruption");
            assert_eq!(db.frontier().unwrap(), basis);
            assert_eq!(txn.beacon_search_graph(&db, &c.query(), &o, q, e).unwrap(), expected);
            txn.abort();
            assert_eq!(c.outstanding_obligations(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
