//! **The node-only pattern: `MATCH (a:Person) RETURN a`**
//! (`fgdb-w5-parsers-nje.7`).
//!
//! The first edgeless pattern: a labeled vertex scan. The fixture spreads
//! the label across an ISOLATE (1) and an edge SOURCE (3), so the answer
//! `[1, 3]` proves the scan is over vertices, not over edge endpoints — a
//! kernel that walks the edge table finds 3 and misses the isolate 1,
//! while one that returns every vertex leaks the unlabeled 2 and 4. The
//! edge statement is re-pinned beside it, the BARE node pattern
//! `MATCH (a) RETURN a` stays a typed parse error (the grammar grew a
//! labeled vertex scan, not an unconstrained one), and an unresolvable
//! label is the typed bind arm.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const NODE_ONLY: &str = "MATCH (a:Person) RETURN a";
const EDGE_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-only-{}-{name}", std::process::id()))
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

fn bind_r_person() -> RelationBind {
    RelationBind::new()
        .with_label("Person", PERSON)
        .with_relation("R", R)
}

/// A labeled isolate, an unlabeled isolate, and a labeled edge source:
/// vertex-scan and edge-endpoint kernels answer differently on every row.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![PERSON], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![PERSON], vec![]);
    seed.create_vertex(VId(4), vec![], vec![]);
    seed.add_edge(EId(10), VId(3), VId(4), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// The labeled vertex scan answers the labeled ISOLATE and the labeled
/// edge source — and neither unlabeled vertex; the edge statement beside
/// it is unmoved.
#[test]
fn the_labeled_vertex_scan_answers_isolates_and_sources_alike() {
    under_lab(0x40_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("node-only");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(NODE_ONLY, &bind_r_person())
            .expect("the node-only MATCH executes");
        assert!(
            rows.contains(&VId(1)) && rows.contains(&VId(3)),
            "the labeled isolate AND the labeled source both answer — an \
             edge-table walk misses the isolate: {rows:?}"
        );
        assert!(
            !rows.contains(&VId(2)) && !rows.contains(&VId(4)),
            "the unlabeled vertices are out — an every-vertex scan leaks \
             them: {rows:?}"
        );

        assert_eq!(
            db.execute_gql(EDGE_B, &bind_r_person())
                .expect("the edge MATCH executes"),
            vec![VId(4)],
            "the edge statement is unmoved by the node-only grammar"
        );
    });
}

/// The bare node pattern stays off-grammar, and an unresolvable label is
/// the typed bind arm — never an empty answer.
#[test]
fn bare_node_is_parse_and_missing_label_is_bind() {
    under_lab(0x40_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("refusals");
        let db = seeded(cx, &dir).await;

        let err = db
            .execute_gql("MATCH (a) RETURN a", &bind_r_person())
            .expect_err("the unconstrained vertex scan is not grammar");
        assert!(
            matches!(err, GqlError::Parse(_)),
            "the bare node pattern is the typed parse arm, got {err:?}"
        );

        let err = db
            .execute_gql("MATCH (a:Missing) RETURN a", &bind_r_person())
            .expect_err("the bind cannot name Missing");
        assert!(
            matches!(err, GqlError::Bind(_)),
            "an unresolvable label is the typed bind arm, got {err:?}"
        );
    });
}
