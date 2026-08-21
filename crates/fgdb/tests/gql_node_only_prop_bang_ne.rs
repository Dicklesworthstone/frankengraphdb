//! **Node-only `WHERE a.k != 1` aliases `<>` on the labeled scan**
//! (`fgdb-w5-parsers-nje-59-p04u`).
//!
//! The C-style spelling of `gql_node_only_prop_ne.rs`: `!=` filters the
//! edgeless labeled scan exactly like `<>`, so beside the literal `[3]`
//! pin the `!=` rows are asserted EQUAL to the `<>` rows themselves — one
//! comparator, two spellings. All three carrier states sit in one fixture
//! — `k = 1`, `k = 9`, and no `k` at all — so each statement's exact
//! answer convicts a different cheat: the inequality answering the
//! keyless 5 is the complement-of-equality reading; the equality
//! answering anything but 1 is a broken predicate; and the unfiltered
//! scan answering fewer than all three means the WHERE machinery leaked
//! into the plain statement.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const BANG_NE: &str = "MATCH (a:Person) WHERE a.k != 1 RETURN a";
const NE: &str = "MATCH (a:Person) WHERE a.k <> 1 RETURN a";
const EQ: &str = "MATCH (a:Person) WHERE a.k = 1 RETURN a";
const PLAIN: &str = "MATCH (a:Person) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-node-prop-bang-ne-{}-{name}",
        std::process::id()
    ))
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

fn bind_all() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_label("Person", PERSON)
        .with_property("k", K)
}

/// Three labeled isolates, three carrier states: `k=1`, `k=9`, keyless.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
    seed.create_vertex(VId(3), vec![PERSON], vec![(K, CanonicalScalar::Int(9))]);
    seed.create_vertex(VId(5), vec![PERSON], vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// One fixture, four exact answers: the C-style alias, its `<>` twin,
/// equality, and the unfiltered scan.
#[test]
fn the_bang_inequality_aliases_the_angle_spelling() {
    under_lab(0x9e_59, |cx| async move {
        let cx = &cx;
        let dir = scratch("three-carriers");
        let db = seeded(cx, &dir).await;

        let bang = db
            .execute_gql(BANG_NE, &bind_all())
            .expect("WHERE a.k != 1 executes");
        assert_eq!(
            bang,
            vec![VId(3)],
            "only the k=9 carrier passes: the k=1 isolate fails the \
             inequality, and the keyless 5 satisfies NEITHER predicate — \
             answering it is the complement-of-equality cheat"
        );
        assert!(
            !bang.contains(&VId(5)),
            "the keyless isolate is OUT, not trivially unequal"
        );
        assert_eq!(
            bang,
            db.execute_gql(NE, &bind_all())
                .expect("the <> sibling still executes"),
            "!= and <> are one comparator in two spellings on the scan"
        );

        assert_eq!(
            db.execute_gql(EQ, &bind_all())
                .expect("WHERE a.k = 1 executes"),
            vec![VId(1)],
            "the equality still answers exactly the k=1 isolate"
        );
        assert_eq!(
            db.execute_gql(PLAIN, &bind_all())
                .expect("unfiltered scan executes"),
            vec![VId(1), VId(3), VId(5)],
            "without WHERE all three labeled isolates answer — the predicate \
             machinery did not leak into the plain statement"
        );
    });
}
