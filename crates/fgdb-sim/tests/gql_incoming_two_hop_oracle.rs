//! **The incoming two-hop, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.4`).
//!
//! The reversed composed law re-derived from `fgdb-reference` in plain
//! test code: `RETURN c` of `(a)<-[:R]-(b)<-[:S]-(c)` is the unique `:S`
//! SOURCES whose destination is an `:R` SOURCE — derived from the
//! reference edge table, so the engine's answer is checked against what
//! the durable stream means, not against the engine's own reads. The live
//! and frontier-pinned passes must both equal the derivation (and each
//! other), and the incoming one-hop rides along against its own
//! derivation — the unique `:R` edge sources, which is what `RETURN b`
//! binds on the incoming statement (the wave's "all :R dests" phrasing
//! reads as the flow's origin side; the landed semantics pinned in
//! `gql_incoming.rs` bind `b` to where the edges come FROM, and the
//! derivation here says so in code).

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::replay;
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const IN_TWO_HOP_C: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const IN_ONE_HOP_B: &str = "MATCH (a)<-[:R]-(b) RETURN b";
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

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-in2h-oracle-{}-{name}", std::process::id()))
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

fn bind_rs() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

/// The oracle's reversed composed sources: unique `:S` sources whose
/// destination sources an `:R` edge. The composition filter lives here, in
/// plain code over reference edges.
fn reference_composed_sources(graph: &ReferenceGraph) -> Vec<VId> {
    let mut r_sources = std::collections::BTreeSet::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        r_sources.insert(edge.src);
    }
    let mut composed: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == S && r_sources.contains(&edge.dst))
        .map(|(_, edge)| edge.src)
        .collect();
    composed.sort_unstable();
    composed.dedup();
    composed
}

/// The oracle's incoming one-hop projection: the unique `:R` edge sources
/// — where the matched edges come FROM, which is what `b` binds.
fn reference_r_sources(graph: &ReferenceGraph) -> Vec<VId> {
    let mut sources: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .map(|(_, edge)| edge.src)
        .collect();
    sources.sort_unstable();
    sources.dedup();
    sources
}

/// Engine answers (live and frontier-pinned) before the drop; the oracle
/// replays after it; nothing but path and keys crosses the line.
#[test]
fn the_incoming_two_hop_equals_the_reference_composed_sources() {
    under_lab(0x0d_51, |cx| async move {
        let cx = &cx;
        let dir = scratch("reversed-oracle");
        let engine_live;
        let engine_at_frontier;
        let engine_one_hop;
        {
            let mut db = Database::create(cx, &dir, engine_keys())
                .await
                .expect("creates");
            let mut r_batch = WriteBatch::new(R);
            for vid in [1u128, 2, 4, 7, 8, 9] {
                r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
            }
            r_batch.add_edge(EId(10), VId(2), VId(1), vec![]);
            r_batch.add_edge(EId(11), VId(7), VId(1), vec![]);
            db.write(cx, r_batch).await.expect("R edges commit");
            let mut s_batch = WriteBatch::new(S);
            s_batch.add_edge(EId(20), VId(4), VId(2), vec![]);
            s_batch.add_edge(EId(21), VId(8), VId(9), vec![]);
            db.write(cx, s_batch).await.expect("S edges commit");
            let frontier = db.frontier().expect("healthy frontier");

            engine_live = db
                .execute_gql(IN_TWO_HOP_C, &bind_rs())
                .expect("the live incoming two-hop executes");
            engine_at_frontier = db
                .execute_gql_at(IN_TWO_HOP_C, &bind_rs(), frontier)
                .expect("the frontier-pinned incoming two-hop executes");
            engine_one_hop = db
                .execute_gql(IN_ONE_HOP_B, &bind_rs())
                .expect("the incoming one-hop executes");
        }
        // NOTHING crosses this line except the path and the keys.

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("the oracle opens the durable stream");
        let replayed = replay(cx, &coordinator).await.expect("the stream replays");
        let graph = replayed
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate");

        assert_eq!(
            engine_live,
            reference_composed_sources(graph),
            "the live reversed compose equals the oracle's derivation"
        );
        assert_eq!(
            engine_live,
            vec![VId(4)],
            "and concretely: 4 composes, the decoy 8 and the feedless 7 do \
             not — an empty derivation cannot agree vacuously"
        );
        assert_eq!(
            engine_at_frontier, engine_live,
            "the frontier-pinned pass agrees with the live one on an \
             unmoved stream — one kernel, two moods"
        );
        assert_eq!(
            engine_one_hop,
            reference_r_sources(graph),
            "the incoming one-hop equals its own derivation — unmoved"
        );
        assert_eq!(engine_one_hop, vec![VId(2), VId(7)]);
    });
}
