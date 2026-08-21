//! **The undirected as-of MATCH, differentially against the oracle**
//! (`fgdb-w5-parsers-nje.2`).
//!
//! The product suites prove the undirected scan against the engine's own
//! reads; this one asks the §15.2 question — does the pinned undirected
//! answer equal what the durable stream MEANS at that sequence? The
//! oracle's incidents are derived from `fgdb-reference`'s edge iterator
//! over the `replay_through(S1)` prefix, with the derivation done in plain
//! test code (src+dst of every `:R` edge, sorted, deduplicated) so the two
//! sides share nothing but bytes on disk. A later commit widens the live
//! answer; the pinned one must keep matching the truncated oracle, and the
//! directed as-of statement rides along so direction erasure cannot have
//! leaked into the directed scan.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_chronicle::capsule::{CapsuleKeys, CapsuleProfile};
use fgdb_chronicle::commit::CommitCoordinator;
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{BranchId, CommitSeq, EId, GraphId, VId};
use std::path::PathBuf;

const GRAPH: GraphId = GraphId(1);
const BRANCH: BranchId = BranchId(1);
const R: RelationId = RelationId(1);
const UN_RETURN_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const OUT_RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
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
        fgdb::CAPSULE_OBJECT_KIND,
        CapsuleProfile::balanced(),
    )
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-un-asof-oracle-{}-{name}", std::process::id()))
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

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// The oracle's undirected incidents: src+dst of every `:R` edge the
/// reference holds, sorted and deduplicated — plain code over the
/// replayed prefix, no engine type anywhere in the derivation.
fn reference_incidents(graph: &fgdb_reference::ReferenceGraph) -> Vec<VId> {
    let mut incidents: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == R)
        .flat_map(|(_, edge)| [edge.src, edge.dst])
        .collect();
    incidents.sort_unstable();
    incidents.dedup();
    incidents
}

/// Seed one edge, capture S1, widen with a second edge; the pinned
/// undirected answer equals the truncated oracle, the live one the full
/// oracle, and the directed as-of statement never widened.
#[test]
fn the_pinned_undirected_answer_equals_the_truncated_oracle() {
    under_lab(0x0a_51, |cx| async move {
        let cx = &cx;
        let dir = scratch("truncated-oracle");
        let s1;
        let engine_at_s1;
        let engine_live;
        let engine_directed_at_s1;
        {
            let mut db = Database::create(cx, &dir, engine_keys()).await.expect("creates");
            let mut seed = WriteBatch::new(R);
            seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
            seed.create_vertex(VId(2), vec![], vec![]);
            seed.create_vertex(VId(3), vec![], vec![]);
            seed.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, seed).await.expect("seed commits");
            s1 = db.frontier().expect("healthy frontier");

            let mut widen = WriteBatch::new(R);
            widen.add_edge(EId(11), VId(3), VId(2), vec![]);
            db.write(cx, widen).await.expect("the widening commit lands");

            engine_at_s1 = db
                .execute_gql_at(UN_RETURN_B, &bind_r(), s1)
                .expect("the pinned undirected MATCH executes");
            engine_live = db
                .execute_gql(UN_RETURN_B, &bind_r())
                .expect("the live undirected MATCH executes");
            engine_directed_at_s1 = db
                .execute_gql_at(OUT_RETURN_B, &bind_r(), s1)
                .expect("the pinned directed MATCH executes");
        }
        // NOTHING crosses this line except the path and the keys.

        let coordinator = CommitCoordinator::open(cx, &dir, oracle_keys())
            .await
            .expect("the oracle opens the durable stream");

        let truncated = fgdb_sim::replay_through(cx, &coordinator, s1)
            .await
            .expect("the S1 prefix replays");
        let truncated_graph = truncated
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate at S1");
        assert_eq!(
            engine_at_s1,
            reference_incidents(truncated_graph),
            "the pinned undirected answer equals the oracle's incidents \
             over the S1 prefix alone"
        );
        assert_eq!(
            engine_at_s1,
            vec![VId(1), VId(2)],
            "and that oracle answer is the expected one — a truncation that \
             replayed nothing would agree vacuously"
        );

        let full = fgdb_sim::replay(cx, &coordinator)
            .await
            .expect("the full stream replays");
        let full_graph = full
            .database
            .graph(GRAPH, BRANCH)
            .expect("the oracle materialized the coordinate");
        assert_eq!(
            engine_live,
            reference_incidents(full_graph),
            "the live undirected answer equals the full-stream incidents"
        );
        assert_eq!(engine_live, vec![VId(1), VId(2), VId(3)]);

        assert_eq!(
            engine_directed_at_s1,
            vec![VId(2)],
            "the directed as-of statement never widened: direction erasure \
             lives in the undirected plan, not in the scan it shares"
        );
    });
}
