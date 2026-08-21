//! **`WriteTxn::in_neighbours` overlay vs the reference oracle**
//! (`fgdb-w4-g1-txn-core-qpmg.15`).
//!
//! The incoming-expansion twin of `writetxn_vertices_overlay.rs`: a pinned
//! transaction's `in_neighbours` must see its own staged incoming edges and
//! deletions before commit, an abort must leave the durable stream exactly at
//! the seed, a committed deletion must ride ONE sequence, and an observed
//! incoming edge deleted by a concurrent first committer must abort the
//! reader under `FG-LAW-FCW-READ-01` — with `fgdb-reference`'s replay of the
//! independent durable stream as the judge every time.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::RelationId;
use fgdb_sim::replay;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(
        K_OID,
        NAMESPACE,
        DEK,
        CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-writetxn-in-neighbours-overlay-{}-{name}",
        std::process::id()
    ))
}

/// The seed every test replays against: `VId(1) -[R, EId(10)]-> VId(2)`, so
/// the pre-txn incoming set of `VId(2)` is exactly `[VId(1)]`.
fn seed_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch
}

/// The reference oracle has no `in_neighbours` accessor, so the incoming set
/// is derived from the edge table itself — which is also the stronger
/// statement: the sources are read off the durable edges, not off any
/// adjacency index that could drift from them.
fn reference_in_neighbours(
    graph: &fgdb_reference::ReferenceGraph,
    dst: VId,
    relation: RelationId,
) -> Vec<VId> {
    let mut sources: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.dst == dst && edge.relation == relation)
        .map(|(_, edge)| edge.src)
        .collect();
    sources.sort_unstable();
    sources.dedup();
    sources
}

#[test]
fn aborted_incoming_create_replays_only_the_seed() {
    let dir = scratch("abort-incoming-create");
    let ((), report) = run_async_under_lab(0x9e_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut incoming = WriteBatch::new(R);
        incoming.create_vertex(VId(3), vec![], vec![]);
        incoming.add_edge(EId(11), VId(3), VId(2), vec![]);
        transaction
            .write(&mut database, incoming)
            .expect("stage second incoming edge");
        assert_eq!(
            transaction
                .in_neighbours(&database, VId(2), R)
                .expect("overlay incoming set"),
            vec![VId(1), VId(3)],
            "the staged incoming edge is visible before commit"
        );
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            database.frontier().expect("abort leaves handle healthy"),
            frontier_before
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 1, "only the seed is durable");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            reference_in_neighbours(graph, VId(2), R),
            vec![VId(1)],
            "the incoming set matches pre-txn: the aborted edge never landed"
        );
        assert!(
            graph.vertex(VId(3)).is_none(),
            "the aborted source vertex is not durable"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn committed_incoming_deletion_replays_an_empty_incoming_set() {
    let dir = scratch("commit-incoming-delete");
    let ((), report) = run_async_under_lab(0x9e_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable edge");
        let frontier_before = database.frontier().expect("healthy seed frontier");

        let mut transaction = database.begin(&txn_cx).expect("begin transaction");
        let mut delete = WriteBatch::new(R);
        delete.delete_edge(EId(10));
        transaction
            .write(&mut database, delete)
            .expect("stage incoming-edge deletion");
        assert!(
            transaction
                .in_neighbours(&database, VId(2), R)
                .expect("overlay incoming set")
                .is_empty(),
            "the staged deletion empties the overlay incoming set before commit"
        );
        let committed = transaction
            .commit(&mut database, &commit_cx)
            .await
            .expect("commit staged deletion");
        assert_eq!(committed.0, frontier_before.0 + 1, "one new sequence");
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), committed.0 as usize);
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(
            reference_in_neighbours(graph, VId(2), R).is_empty(),
            "the committed deletion empties the durable incoming set"
        );
        assert!(
            graph.vertex(VId(1)).is_some() && graph.vertex(VId(2)).is_some(),
            "only the edge was deleted; both endpoints survive"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn concurrent_deletion_of_observed_incoming_edge_aborts_and_replays_only_deleter() {
    let dir = scratch("in-neighbours-read-conflict");
    let ((), report) = run_async_under_lab(0x9e_03, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let txn_cx = contexts.txn();
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_edge())
            .await
            .expect("seed durable edge");

        let mut reader = database.begin(&txn_cx).expect("begin incoming reader");
        let mut deleter = database.begin(&txn_cx).expect("begin edge deleter");
        assert_eq!(
            reader
                .in_neighbours(&database, VId(2), R)
                .expect("transactional incoming read"),
            vec![VId(1)],
            "the reader observes the seed's incoming edge"
        );
        let mut disjoint = WriteBatch::new(R);
        disjoint.create_vertex(VId(4), vec![], vec![]);
        reader
            .write(&mut database, disjoint)
            .expect("reader stages disjoint vertex");
        let mut delete = WriteBatch::new(R);
        delete.delete_edge(EId(10));
        deleter
            .write(&mut database, delete)
            .expect("deleter stages observed incoming-edge deletion");
        deleter
            .commit(&mut database, &commit_cx)
            .await
            .expect("edge deleter commits first");

        let refusal = reader.commit(&mut database, &commit_cx).await;
        assert!(
            matches!(&refusal, Err(WriteTxnError::Write(_))),
            "incoming read conflict must be a typed Write abort: {refusal:?}"
        );
        let rendered = format!("{refusal:?}");
        assert!(
            rendered.contains("FG-LAW-FCW-READ-01"),
            "incoming read conflict must name READ-01: {rendered}"
        );
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        assert_eq!(coordinator.chain().len(), 2, "seed and edge deleter only");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert!(
            reference_in_neighbours(graph, VId(2), R).is_empty(),
            "B's deletion of the observed incoming edge is durable"
        );
        assert!(
            graph.vertex(VId(4)).is_none(),
            "READ-01 abort leaves none of A's disjoint write"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
