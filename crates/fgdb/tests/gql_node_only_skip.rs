//! **`SKIP`/`LIMIT` compose with the node-only scan**
//! (`fgdb-w5-parsers-nje.14`).
//!
//! Three labeled ISOLATES — no edges at all — so the offset provably applies
//! to the vertex scan's CGSE row set and nothing else: `SKIP 1` drops the
//! smallest labeled vid, `SKIP 1 LIMIT 1` is offset-then-truncate, and the
//! clause-free scan answers all three. The BARE node pattern stays a typed
//! parse error even with `SKIP` — the grammar grew a labeled vertex scan,
//! not an unconstrained one, and a paging clause does not license it.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::VId;
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const NODE_SKIP: &str = "MATCH (a:Person) RETURN a SKIP 1";
const NODE_SKIP_LIMIT: &str = "MATCH (a:Person) RETURN a SKIP 1 LIMIT 1";
const NODE_PLAIN: &str = "MATCH (a:Person) RETURN a";
const BARE_SKIP: &str = "MATCH (a) RETURN a SKIP 1";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-only-skip-{}-{name}", std::process::id()))
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

fn bind_person() -> RelationBind {
    RelationBind::new()
        .with_label("Person", PERSON)
        .with_relation("R", R)
}

#[test]
fn skip_and_limit_compose_with_the_labeled_vertex_scan() {
    under_lab(0x48_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("isolates");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut seed = WriteBatch::new(R);
        for vid in [1u128, 2, 4] {
            seed.create_vertex(VId(vid), vec![PERSON], vec![]);
        }
        db.write(cx, seed).await.expect("labeled isolates commit");
        let bind = bind_person();

        assert_eq!(
            db.execute_gql(NODE_SKIP, &bind)
                .expect("SKIP 1 node scan executes"),
            vec![VId(2), VId(4)],
            "SKIP 1 drops the smallest labeled vid"
        );
        assert_eq!(
            db.execute_gql(NODE_SKIP_LIMIT, &bind)
                .expect("SKIP 1 LIMIT 1 node scan executes"),
            vec![VId(2)],
            "SKIP then LIMIT is offset-then-truncate over the scan's ordering"
        );
        assert_eq!(
            db.execute_gql(NODE_PLAIN, &bind)
                .expect("clause-free node scan executes"),
            vec![VId(1), VId(2), VId(4)],
            "without SKIP/LIMIT all three isolates answer"
        );

        let bare = db
            .execute_gql(BARE_SKIP, &bind)
            .expect_err("the unlabeled node pattern stays off-grammar, SKIP or not");
        assert!(
            matches!(bare, GqlError::Parse(_)),
            "expected the typed Parse refusal, got {bare:?}"
        );
    });
}
