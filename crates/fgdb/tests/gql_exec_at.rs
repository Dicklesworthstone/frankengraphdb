//! **The pinned MATCH answers at a pinned sequence**
//! (`fgdb-w4-g1-txn-core-qpmg.19`).
//!
//! Time travel is a corollary of MVCC, and the language surface owes it the
//! same way the typed reads do: `execute_gql_at` answers
//! `MATCH (a)-[:R]->(b) RETURN b` as of an older frontier, unmoved by every
//! commit after it. The seed answer is captured as a REAL frontier value
//! (not a constant), a later commit widens the live answer, and the pinned
//! query must keep answering the narrow one — a kernel that scans the live
//! fold and filters nothing fails on the widened row.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bead's name, trailing
//! `as_of` like `vertex_at`/`edge_at`):
//! - `Database::execute_gql_at(&self, src, &RelationBind, CommitSeq)
//!    -> Result<Vec<VId>, GqlError>`
//! Until it lands this file fails to compile — deliberately; do not weaken
//! it to make it compile.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-gql-at-{}-{name}", std::process::id()))
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

/// A commit AFTER the captured frontier widens the live answer and must not
/// touch the pinned one: live `[2, 5]`, as-of-S1 `[2]` — from the same
/// handle, in the same breath, before and after the pinned call so an
/// answer cached from the live scan cannot fake it.
#[test]
fn the_pinned_seq_answer_is_unmoved_by_later_commits() {
    under_lab(0xa7_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("pinned-seq");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
        seed.create_vertex(VId(2), vec![], vec![]);
        seed.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, seed).await.expect("seed commits");
        let s1 = db.frontier().expect("healthy frontier");

        let mut widen = WriteBatch::new(R);
        widen.create_vertex(VId(3), vec![], vec![]);
        widen.create_vertex(VId(5), vec![], vec![]);
        widen.add_edge(EId(11), VId(3), VId(5), vec![]);
        db.write(cx, widen).await.expect("the widening commit lands");

        assert_eq!(
            db.execute_gql(PINNED, &bind_r()).expect("live MATCH executes"),
            vec![VId(2), VId(5)],
            "the live answer holds both destinations, CGSE-sorted"
        );
        assert_eq!(
            db.execute_gql_at(PINNED, &bind_r(), s1)
                .expect("the pinned MATCH executes"),
            vec![VId(2)],
            "as of the captured frontier the second edge does not exist yet"
        );
        assert_eq!(
            db.execute_gql(PINNED, &bind_r()).expect("live MATCH re-executes"),
            vec![VId(2), VId(5)],
            "the pinned query did not disturb the live answer"
        );
    });
}

/// Off-grammar text through the time-travel surface is the same typed
/// `GqlError::Parse` refusal — the statement dies before any scan, at any
/// sequence, including one where the graph was empty.
#[test]
fn off_grammar_at_a_pinned_seq_is_a_typed_parse_error() {
    under_lab(0xa7_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("at-off-grammar");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let genesis = db.frontier().expect("healthy frontier");
        let mut seed = WriteBatch::new(R);
        seed.create_vertex(VId(1), vec![], vec![]);
        db.write(cx, seed).await.expect("seed commits");

        for off_grammar in ["MATCH (a) RETURN a", "MATCH (a)-[:R]->(b) RETURN b EXTRA", ""] {
            let err = db
                .execute_gql_at(off_grammar, &bind_r(), genesis)
                .expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}
