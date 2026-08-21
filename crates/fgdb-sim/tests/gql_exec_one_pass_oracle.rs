//! **One-pass MATCH projections vs the reference's bound-relation edge table**
//! (`fgdb-gql-one-pass-pbwl`).
//!
//! Both projections of the pinned statement must equal a projection of the
//! BOUND relation's edge table as the reference replays it — never a
//! `vertices()` sweep, never another relation's edges. The shared-destination
//! fixture keeps the two projections genuinely different (`RETURN b`
//! collapses to one dest, `RETURN a` keeps two sources), the off-relation
//! edge is the leak control, and the epoch law pins the as-of face: the
//! first sequence's answer survives a later edge that the live answer must
//! include.

use asupersync::lab::run_async_under_lab;
use fgdb::{CAPSULE_OBJECT_KIND, Database, DatabaseKeys, RelationBind, WriteBatch};
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
const OTHER: RelationId = RelationId(2);
const RETURN_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
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
        "fgdb-gql-one-pass-oracle-{}-{name}",
        std::process::id()
    ))
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// Two sources into ONE shared destination — `1-[:R]->2`, `3-[:R]->2` — so
/// the projections differ: `RETURN b` collapses to `[2]`, `RETURN a` keeps
/// `[1, 3]`.
fn seed_shared_dest() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(1), vec![], vec![]);
    batch.create_vertex(VId(2), vec![], vec![]);
    batch.create_vertex(VId(3), vec![], vec![]);
    batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    batch.add_edge(EId(11), VId(3), VId(2), vec![]);
    batch
}

/// The off-relation leak control: `3-[:OTHER]->7` must appear in neither
/// projection of the bound relation.
fn seed_other() -> WriteBatch {
    let mut batch = WriteBatch::new(OTHER);
    batch.create_vertex(VId(7), vec![], vec![]);
    batch.add_edge(EId(12), VId(3), VId(7), vec![]);
    batch
}

/// The later epoch: `9-[:R]->5`, adding a new source and dest past `S1`.
fn later_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.create_vertex(VId(9), vec![], vec![]);
    batch.create_vertex(VId(5), vec![], vec![]);
    batch.add_edge(EId(13), VId(9), VId(5), vec![]);
    batch
}

/// One projection of the BOUND relation's edge table, read off the reference
/// oracle's own edges: unique ascending sources or destinations.
fn reference_projection(
    graph: &fgdb_reference::ReferenceGraph,
    relation: RelationId,
    project_src: bool,
) -> Vec<VId> {
    let mut projected: Vec<VId> = graph
        .iter_edges()
        .filter(|(_, edge)| edge.relation == relation)
        .map(|(_, edge)| if project_src { edge.src } else { edge.dst })
        .collect();
    projected.sort_unstable();
    projected.dedup();
    projected
}

#[test]
fn both_projections_equal_the_reference_bound_relation_edge_table() {
    let dir = scratch("projections");
    let ((), report) = run_async_under_lab(0xaa_01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_shared_dest())
            .await
            .expect("seed shared-destination edges");
        database
            .write(&commit_cx, seed_other())
            .await
            .expect("seed off-relation control");

        let dests = database
            .execute_gql(RETURN_B, &bind_r())
            .expect("RETURN b executes");
        let sources = database
            .execute_gql(RETURN_A, &bind_r())
            .expect("RETURN a executes");
        assert_eq!(dests, vec![VId(2)], "the shared dest collapses to [2]");
        assert_eq!(sources, vec![VId(1), VId(3)], "both sources, sorted");
        assert!(
            !sources.contains(&VId(7)) && !dests.contains(&VId(7)),
            "the off-relation edge leaks into neither projection"
        );
        drop(database);

        let coordinator = CommitCoordinator::open(&commit_cx, &dir, oracle_keys())
            .await
            .expect("independent oracle coordinator opens durable stream");
        let reference = replay(&commit_cx, &coordinator)
            .await
            .expect("durable stream replays into ReferenceDatabase")
            .database;
        let graph = reference
            .graph(GRAPH, BRANCH)
            .expect("reference coordinate exists");
        assert_eq!(
            sources,
            reference_projection(graph, R, true),
            "RETURN a equals the source projection of the bound relation's edges"
        );
        assert_eq!(
            dests,
            reference_projection(graph, R, false),
            "RETURN b equals the dest projection of the bound relation's edges"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}

#[test]
fn as_of_return_a_survives_a_later_edge_the_live_answer_includes() {
    let dir = scratch("epoch");
    let ((), report) = run_async_under_lab(0xaa_02, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit_cx = contexts.commit();
        let mut database = Database::create(&commit_cx, &dir, engine_keys())
            .await
            .expect("create product database");
        database
            .write(&commit_cx, seed_shared_dest())
            .await
            .expect("seed shared-destination edges");
        let s1 = database.frontier().expect("healthy S1 frontier");

        assert_eq!(
            database
                .execute_gql_at(RETURN_A, &bind_r(), s1)
                .expect("RETURN a executes at S1"),
            database
                .execute_gql(RETURN_A, &bind_r())
                .expect("RETURN a executes live"),
            "before any later edge, as-of at the frontier IS the live answer"
        );

        database
            .write(&commit_cx, later_edge())
            .await
            .expect("seed the later source");
        assert_eq!(
            database
                .execute_gql_at(RETURN_A, &bind_r(), s1)
                .expect("RETURN a executes at S1 after the later edge"),
            vec![VId(1), VId(3)],
            "the first sequence's sources are unchanged by the later edge"
        );
        assert_eq!(
            database
                .execute_gql(RETURN_A, &bind_r())
                .expect("RETURN a executes live after the later edge"),
            vec![VId(1), VId(3), VId(9)],
            "the live answer includes the later source"
        );
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
}
