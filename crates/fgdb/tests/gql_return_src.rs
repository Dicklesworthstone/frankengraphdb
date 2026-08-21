//! **`RETURN a` projects sources** (`fgdb-gql-return-src-g7g5`).
//!
//! The pinned grammar grows its second projection: `MATCH (a)-[:R]->(b)
//! RETURN a` answers the SOURCES of the matched edges, under the same CGSE
//! row contract as `RETURN b` (ascending, deduplicated). The fixture is
//! shaped so the two projections give DIFFERENT answers — two edges into
//! one destination — killing an implementation that parses `RETURN a` and
//! still projects destinations: `[1, 3]` vs `[2]` cannot be confused. A
//! variable the pattern never bound (`RETURN c`) stays a typed parse
//! error, and `RETURN b` is re-pinned beside the new projection so the
//! grammar growth cannot regress it.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
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
    std::env::temp_dir().join(format!("fgdb-return-src-{}-{name}", std::process::id()))
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

/// Two edges into ONE destination, sources written descending: the source
/// and destination projections must give different, correctly sorted
/// answers from the same graph.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![]);
    // Source 3 first: ascending output must be a sort, not write order.
    seed.add_edge(EId(10), VId(3), VId(2), vec![]);
    seed.add_edge(EId(11), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// `RETURN a` answers the sources — ascending, deduplicated — and cannot
/// be the destination set in disguise, because the fixture makes the two
/// sets disjoint.
#[test]
fn return_a_projects_the_sources_sorted() {
    under_lab(0x6a_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("sources");
        let db = seeded(cx, &dir).await;

        let rows = db.execute_gql(RETURN_A, &bind_r()).expect("RETURN a executes");
        assert_eq!(
            rows,
            vec![VId(1), VId(3)],
            "the matched SOURCES, CGSE-sorted ascending — a destination \
             projection would answer [2] here and cannot be confused"
        );
    });
}

/// `RETURN b` beside it: still the destinations, still deduplicated — two
/// matched edges, one destination row. The grammar growing a second
/// projection must not have moved the first.
#[test]
fn return_b_still_projects_the_destinations() {
    under_lab(0x6a_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("dests");
        let db = seeded(cx, &dir).await;

        let rows = db.execute_gql(RETURN_B, &bind_r()).expect("RETURN b executes");
        assert_eq!(
            rows,
            vec![VId(2)],
            "two edges, one destination, one row — dedup intact"
        );
    });
}

/// A variable the pattern never bound is off-grammar: `RETURN c` (and its
/// neighbours) must be the typed parse arm, not an empty answer and not a
/// silent fallback to either bound variable.
#[test]
fn return_of_an_unbound_variable_is_a_typed_parse_error() {
    under_lab(0x6a_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("unbound");
        let db = seeded(cx, &dir).await;

        for off_grammar in [
            "MATCH (a)-[:R]->(b) RETURN c",
            "MATCH (a)-[:R]->(b) RETURN ab",
            "MATCH (a)-[:R]->(b) RETURN",
        ] {
            let err = db.execute_gql(off_grammar, &bind_r()).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}
