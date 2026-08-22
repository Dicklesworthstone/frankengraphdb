//! **The undirected pattern: `(a)-[:R]-(b)`** (`fgdb-w5-parsers-nje.2`).
//!
//! The third edge shape: no arrow, both endpoints. On the shared fixture
//! (`1-[:R]->2`, `3-[:R]->2`) every incident vertex answers BOTH
//! projections — `[1, 2, 3]` — because an undirected match binds each edge
//! twice, once per orientation. The isolate 9 stays out (incidence, not
//! existence), the two directed statements are re-pinned beside the new
//! shape so it cannot have been implemented by loosening them, and the
//! grammar now includes an undirected two-hop: an R-only bind refuses its
//! missing `S`, while the contradictory arrow remains a typed parse error.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const UN_RETURN_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const UN_RETURN_A: &str = "MATCH (a)-[:R]-(b) RETURN a";
const OUT_RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const IN_RETURN_A: &str = "MATCH (a)<-[:R]-(b) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undirected-{}-{name}", std::process::id()))
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

/// Two edges into one destination plus an isolate: incidence is `{1, 2, 3}`
/// and the isolate 9 has none.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.create_vertex(VId(9), vec![], vec![]);
    seed.add_edge(EId(10), VId(3), VId(2), vec![]);
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Both projections of the undirected match answer every incident vertex —
/// and only incident vertices: the dest-only 2 is in BOTH answers (the
/// direction really is erased) and the isolate 9 is in neither.
#[test]
fn both_projections_answer_the_incident_vertices() {
    under_lab(0x3d_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("incident");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(UN_RETURN_B, &bind_r())
                .expect("undirected RETURN b executes"),
            vec![VId(1), VId(2), VId(3)],
            "each edge binds twice, once per orientation; the isolate 9 is \
             incident to nothing"
        );
        assert_eq!(
            db.execute_gql(UN_RETURN_A, &bind_r())
                .expect("undirected RETURN a executes"),
            vec![VId(1), VId(2), VId(3)],
            "the projections are symmetric when the direction is erased — \
             the dest-only vertex 2 answers as a too"
        );
    });
}

/// The two directed statements beside the new shape: unmoved. An
/// undirected kernel implemented by loosening the directed scan would
/// widen these and is caught here.
#[test]
fn the_directed_statements_are_unmoved() {
    under_lab(0x3d_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("directed-pinned");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(OUT_RETURN_B, &bind_r())
                .expect("outgoing RETURN b executes"),
            vec![VId(2)],
            "outgoing destinations unchanged"
        );
        assert_eq!(
            db.execute_gql(IN_RETURN_A, &bind_r())
                .expect("incoming RETURN a executes"),
            vec![VId(2)],
            "incoming in-edge-holders unchanged"
        );
    });
}

/// Two-hop undirected is grammar now: without `S` it reaches binding and
/// refuses there. The contradictory arrow remains off-grammar.
#[test]
fn undirected_two_hop_missing_s_is_bind_and_contradictory_arrow_is_parse() {
    under_lab(0x3d_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("still-off-grammar");
        let db = seeded(cx, &dir).await;

        let two_hop = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
        let err = db.execute_gql(two_hop, &bind_r()).expect_err(two_hop);
        assert!(
            matches!(err, GqlError::Bind(_)),
            "two-hop is legal grammar; missing S must be the bind arm, got {err:?}"
        );

        let contradictory = "MATCH (a)<[:R]->(b) RETURN a";
        let err = db
            .execute_gql(contradictory, &bind_r())
            .expect_err(contradictory);
        assert!(
            matches!(err, GqlError::Parse(_)),
            "contradictory arrow must be the parse arm, got {err:?}"
        );
    });
}
