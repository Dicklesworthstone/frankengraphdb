//! **The incoming pattern: `(a)<-[:R]-(b)`**
//! (`fgdb-gql-incoming-1qei`).
//!
//! The grammar's second edge direction. On the shared fixture — sources
//! `[1, 3]`, destination `[2]` — the incoming pattern binds `a` to the
//! vertex WITH in-edges and `b` to the vertices the edges come FROM, so
//! the two projections answer exactly opposite to the outbound statement's.
//! That crossing is the fixture's whole point: an implementation that
//! parses `<-` and silently expands outbound anyway gives `RETURN a = [1,3]`
//! here and is caught by both tests at once. The outbound statement is
//! re-pinned beside it, and the two malformed arrows (`-[:R]-`,
//! `<[:R]->`) stay typed parse errors — a two-direction grammar is still
//! exactly two shapes, not a lenient arrow parser.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const IN_RETURN_A: &str = "MATCH (a)<-[:R]-(b) RETURN a";
const IN_RETURN_B: &str = "MATCH (a)<-[:R]-(b) RETURN b";
const OUT_RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-incoming-{}-{name}", std::process::id()))
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

/// Two edges into one destination: `1-[:R]->2` and `3-[:R]->2`.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.add_edge(EId(10), VId(3), VId(2), vec![]);
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Incoming, `RETURN a`: the vertex with in-edges — `[2]`. An outbound
/// expansion wearing a `<-` costume answers `[1, 3]` here.
#[test]
fn incoming_return_a_projects_the_vertex_with_in_edges() {
    under_lab(0x1e_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("in-a");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(IN_RETURN_A, &bind_r())
            .expect("incoming RETURN a executes");
        assert_eq!(
            rows,
            vec![VId(2)],
            "a binds the vertex the :R edges point AT — the direction flip \
             is real, not cosmetic"
        );
    });
}

/// Incoming, `RETURN b`: where the edges come from — `[1, 3]`, CGSE-sorted.
#[test]
fn incoming_return_b_projects_the_sources_of_the_in_edges() {
    under_lab(0x1e_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("in-b");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(IN_RETURN_B, &bind_r())
            .expect("incoming RETURN b executes");
        assert_eq!(
            rows,
            vec![VId(1), VId(3)],
            "b binds the vertices the :R edges come FROM, sorted ascending"
        );
    });
}

/// The outbound statement beside it: still `[2]`. Growing a direction must
/// not have moved the one that already worked.
#[test]
fn outbound_return_b_is_unmoved() {
    under_lab(0x1e_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("out-b");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(OUT_RETURN_B, &bind_r())
            .expect("outbound RETURN b executes");
        assert_eq!(rows, vec![VId(2)], "the outbound projection is unchanged");
    });
}

/// Malformed arrows are typed parse errors. The undirected `-[:R]-` shape
/// graduated to legal grammar (fgdb-w5-parsers-nje.2 — see
/// `gql_undirected.rs`), so the contradictory `<[:R]->` carries this test
/// alone now: it is none of the three legal shapes.
#[test]
fn malformed_arrows_are_typed_parse_errors() {
    under_lab(0x1e_04, |cx| async move {
        let cx = &cx;
        let dir = scratch("bad-arrows");
        let db = seeded(cx, &dir).await;

        let off_grammar = "MATCH (a)<[:R]->(b) RETURN a";
        let err = db
            .execute_gql(off_grammar, &bind_r())
            .expect_err(off_grammar);
        assert!(
            matches!(err, GqlError::Parse(_)),
            "{off_grammar:?} must be the typed parse arm, got {err:?}"
        );
    });
}
