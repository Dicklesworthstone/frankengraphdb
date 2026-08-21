//! **One pass, bound relation only, both projections**
//! (`fgdb-gql-one-pass-pbwl`).
//!
//! With a second relation live in the graph, BOTH projections of the
//! pinned MATCH must stay inside the bound relation: `RETURN a` answers
//! the `:R` sources and not the `:S` source, `RETURN b` the `:R`
//! destination and not the `:S` destination. The same answers must come
//! back through `execute_gql_at` at the captured frontier — one scan
//! discipline, live and pinned. The regressions keep the suite honest
//! from both ends: dedup (two `:R` edges, one destination row) and the
//! empty graph (`Ok(vec![])`, no rows, no error).

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const RETURN_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const RETURN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-one-pass-{}-{name}", std::process::id()))
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

/// Both projections honour the bound relation — live AND at the captured
/// frontier: `[1, 3]` sources (never `7`), `[2]` destination (never `8`),
/// with the `:S` edge live in the same graph the whole time.
#[test]
fn both_projections_stay_inside_the_bound_relation_live_and_at() {
    under_lab(0x19_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("bound-relation");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.create_vertex(VId(7), vec![], vec![]);
        batch.create_vertex(VId(8), vec![], vec![]);
        batch.add_edge(EId(10), VId(3), VId(2), vec![]);
        batch.add_edge(EId(11), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.expect("R edges commit");
        let mut other = WriteBatch::new(S);
        other.add_edge(EId(12), VId(7), VId(8), vec![]);
        db.write(cx, other).await.expect("S edge commits");
        let frontier = db.frontier().expect("healthy frontier");

        assert_eq!(
            db.execute_gql(RETURN_A, &bind_r()).expect("RETURN a executes"),
            vec![VId(1), VId(3)],
            "sources of :R only — the :S source 7 is not a row"
        );
        assert_eq!(
            db.execute_gql(RETURN_B, &bind_r()).expect("RETURN b executes"),
            vec![VId(2)],
            "destination of :R only — the :S destination 8 is not a row"
        );
        assert_eq!(
            db.execute_gql_at(RETURN_A, &bind_r(), frontier)
                .expect("RETURN a executes at the frontier"),
            vec![VId(1), VId(3)],
            "one scan discipline: the pinned pass answers like the live one"
        );
        assert_eq!(
            db.execute_gql_at(RETURN_B, &bind_r(), frontier)
                .expect("RETURN b executes at the frontier"),
            vec![VId(2)]
        );
    });
}

/// Regressions from both ends: two `:R` edges into one destination stay
/// ONE row, and the empty graph answers `Ok(vec![])` for both projections.
#[test]
fn dedup_and_the_empty_graph_hold() {
    under_lab(0x19_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("regressions");
        let db = Database::create(cx, &dir, keys()).await.expect("creates");
        assert!(
            db.execute_gql(RETURN_A, &bind_r())
                .expect("RETURN a on the empty graph is a result")
                .is_empty()
        );
        assert!(
            db.execute_gql(RETURN_B, &bind_r())
                .expect("RETURN b on the empty graph is a result")
                .is_empty()
        );
        drop(db);

        let dir = scratch("regressions-dedup");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        batch.add_edge(EId(11), VId(3), VId(2), vec![]);
        db.write(cx, batch).await.expect("commits");
        assert_eq!(
            db.execute_gql(RETURN_B, &bind_r()).expect("RETURN b executes"),
            vec![VId(2)],
            "two matched edges, one destination, one row — dedup intact"
        );
    });
}
