//! **`WHERE a <> b` drops the self-loop** (`fgdb-gql-where-neq-v476`).
//!
//! The first predicate: bound-variable inequality. A self-loop
//! (`5-[:R]->5`) is a real matched edge whose two bindings coincide, so it
//! is the exact row `WHERE a <> b` exists to drop — and the only row it
//! may drop. Both projections are pinned with and without the predicate,
//! the flipped spelling `WHERE b <> a` must filter identically (inequality
//! is symmetric; a filter keyed to "left operand is the source" is not an
//! inequality), and everything beyond the one predicate — equality, an
//! unbound operand, a property path — stays a typed parse error.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PLAIN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const NEQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a <> b RETURN b";
const NEQ_A: &str = "MATCH (a)-[:R]->(b) WHERE a <> b RETURN a";
const NEQ_FLIPPED_B: &str = "MATCH (a)-[:R]->(b) WHERE b <> a RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-where-neq-{}-{name}", std::process::id()))
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

/// Two ordinary edges into one destination plus the self-loop: the loop's
/// endpoint 5 answers both projections unfiltered and neither filtered.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    for vid in [1u128, 2, 3, 5] {
        seed.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(2), vec![]);
    seed.add_edge(EId(12), VId(5), VId(5), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Unfiltered, the loop destination answers; under `a <> b` — in either
/// spelling — exactly the loop's rows vanish from BOTH projections and
/// nothing else moves.
#[test]
fn the_inequality_drops_exactly_the_self_loop_rows() {
    under_lab(0x4e_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("drop-loop");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(PLAIN_B, &bind_r()).expect("unfiltered RETURN b executes"),
            vec![VId(2), VId(5)],
            "the self-loop's destination is a real row until a predicate \
             says otherwise"
        );
        assert_eq!(
            db.execute_gql(NEQ_B, &bind_r()).expect("filtered RETURN b executes"),
            vec![VId(2)],
            "a <> b drops the row whose bindings coincide — and only it"
        );
        assert_eq!(
            db.execute_gql(NEQ_A, &bind_r()).expect("filtered RETURN a executes"),
            vec![VId(1), VId(3)],
            "the loop's source vanishes from the other projection too"
        );
        assert_eq!(
            db.execute_gql(NEQ_FLIPPED_B, &bind_r()).expect("flipped spelling executes"),
            vec![VId(2)],
            "b <> a filters identically: inequality is symmetric, not a \
             claim about which operand is the source"
        );
    });
}

/// One predicate, exactly: equality, an unbound operand, and a property
/// path are all off-grammar — typed parse errors, never silently-true
/// filters.
#[test]
fn anything_beyond_the_one_predicate_is_a_typed_parse_error() {
    under_lab(0x4e_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("off-grammar");
        let db = seeded(cx, &dir).await;

        for off_grammar in [
            "MATCH (a)-[:R]->(b) WHERE a = b RETURN b",
            "MATCH (a)-[:R]->(b) WHERE a <> c RETURN b",
            "MATCH (a)-[:R]->(b) WHERE a.x <> b RETURN b",
        ] {
            let err = db.execute_gql(off_grammar, &bind_r()).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}
