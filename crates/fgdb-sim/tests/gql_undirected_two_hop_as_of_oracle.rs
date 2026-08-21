//! **The undirected two-hop as-of, differentially against the oracle**
//! (`fgdb-gql-undir-2hop-7mrc`).
//!
//! The composed law re-derived from the reference in plain test code: the
//! vias are the vertices incident (either end) to an `:R` edge, and the
//! composed incidents are the OTHER endpoints of every `:S` edge touching
//! a via — sorted, deduplicated. The decoy `9-[:S]->8` sits in the S1
//! prefix so the derivation itself must filter it on both sides of the
//! comparison; the widening `2-[:S]->5` lands after S1 so the pinned
//! engine answer must equal the truncated oracle's `[4]` while the live
//! answer equals the full-stream `[4, 5]`. Concrete-value asserts ride
//! along so an empty truncation cannot agree vacuously.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_reference::ReferenceGraph;
use fgdb_sim::{replay, replay_through};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const UN_TWO_HOP_C: &str = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);
const DEK: [u8; 32] = [0x3c; 32];

fn engine_keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, DEK)
}

fn oracle_keys() -> CapsuleKeys {
    CapsuleKeys::new(K_OID, NAMESPACE, DEK, CAPSULE_OBJECT_KIND, CapsuleProfile::balanced())
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-un2h-oracle-{}-{name}", std::process::id()))
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

/// The oracle's composed incidents, derived independently: vias are the
/// `:R`-incident vertices (either end), and the answer is the other
/// endpoint of every `:S` edge touching a via — sorted, deduplicated. The
/// dual-incidence filter lives HERE, in plain code over reference edges,
/// so the decoy is excluded by derivation, not by the engine's say-so.
fn reference_composed_incidents(graph: &ReferenceGraph) -> Vec<VId> {
    let mut vias = std::collections::BTreeSet::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == R) {
        vias.insert(edge.src);
        vias.insert(edge.dst);
    }
    let mut composed = Vec::new();
    for (_, edge) in graph.iter_edges().filter(|(_, edge)| edge.relation == S) {
        if vias.contains(&edge.src) {
            composed.push(edge.dst);
        }
        if vias.contains(&edge.dst) {
            composed.push(edge.src);
        }
    }
    composed.sort_unstable();
    composed.dedup();
    composed
}

/// Engine answers before the drop; oracle prefixes after it; nothing but
/// the path and the keys crosses the line between them.
#[test]
fn the_pinned_undirected_two_hop_equals_the_truncated_oracle() {
    under_lab(0x0b_51, |cx| async move {
        let cx = &cx;
        let dir = scratch("truncated-compose");
        let s1;
        let engine_at_s1;
        let engine_live;
        {
            let mut db = Database::create(cx, &dir, engine_keys()).await.expect("creates");
            let mut r_batch = WriteBatch::new(R);
            for vid in [1u128, 2, 4, 8, 9] {
                r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
            }
            r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, r_batch).await.expect("R edge commits");
            let mut s_batch = WriteBatch::new(S);
            s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
            s_batch.add_edge(EId(21), VId(9), VId(8), vec![]);
            db.write(cx, s_batch).await.expect("S edges + decoy commit");
            s1 = db.frontier().expect("healthy frontier");

            let mut widen = WriteBatch::new(S);
            widen.create_vertex(VId(5), vec![], vec![]);
            widen.add_edge(EId(22), VId(2), VId(5), vec![]);
            db.write(cx, widen).await.expect("the widening continuation lands");

            engine_at_s1 = db
                .execute_gql_at(UN_TWO_HOP_C, &bind_rs(), s1)
                .expect("the pinned undirected two-hop executes");
            engine_live = db
                .execute_gql(UN_TWO_HOP_C, &bind_rs())
                .expect("the live undirected two-hop executes");
        }
        // NOTHING crosses this line except the path and the keys.

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("the oracle opens the durable stream");

        let truncated = replay_through(cx, &coordinator, s1)
            .await
            .expect("the S1 prefix replays");
        let truncated_graph = truncated
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate at S1");
        assert_eq!(
            engine_at_s1,
            reference_composed_incidents(truncated_graph),
            "the pinned answer equals the oracle's composed incidents over \
             the S1 prefix — decoy filtered by derivation on both sides"
        );
        assert_eq!(
            engine_at_s1,
            vec![VId(4)],
            "and concretely: 4 composed, 5 not yet committed, 8 never — an \
             empty truncation cannot agree vacuously"
        );

        let full = replay(cx, &coordinator).await.expect("the full stream replays");
        let full_graph = full
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate");
        assert_eq!(
            engine_live,
            reference_composed_incidents(full_graph),
            "the live answer equals the full-stream composed incidents"
        );
        assert_eq!(engine_live, vec![VId(4), VId(5)]);
    });
}
