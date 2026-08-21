//! **Node-only `WHERE a.k >= 1` keeps greater-or-equal Persons**
//! (`fgdb-w5-parsers-nje.31`).
//!
//! The non-strict comparator reaches the edgeless labeled scan: `>= 1`
//! answers the boundary carrier AND the above-boundary carrier — `[1, 2]`
//! beside the strict `>`'s `[2]`, so a renamed `>` fails on the vertex
//! scan too. The below-boundary and keyless Persons stay out (asserted by
//! name), the unlabeled `k = 1` carrier stays out of the LABELED scan
//! (the predicate does not widen the label), the strict and equality
//! siblings plus the unfiltered scan are pinned alongside, and two
//! refusals hold the grammar's edges: node-only `<=` is not grammar this
//! slice, and a predicate does not legalize the bare vertex scan.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const K: PropertyKeyId = PropertyKeyId(7);
const GE_A: &str = "MATCH (a:Person) WHERE a.k >= 1 RETURN a";
const GT_A: &str = "MATCH (a:Person) WHERE a.k > 1 RETURN a";
const EQ_A: &str = "MATCH (a:Person) WHERE a.k = 1 RETURN a";
const LT_A: &str = "MATCH (a:Person) WHERE a.k < 1 RETURN a";
const PLAIN_A: &str = "MATCH (a:Person) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-ge-{}-{name}", std::process::id()))
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

/// Four Person carrier states plus an unlabeled carrier: the labeled scan
/// and the comparator are tested against each other.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![PERSON], vec![(K, CanonicalScalar::Int(1))]);
    seed.create_vertex(VId(2), vec![PERSON], vec![(K, CanonicalScalar::Int(9))]);
    seed.create_vertex(VId(6), vec![PERSON], vec![(K, CanonicalScalar::Int(0))]);
    seed.create_vertex(VId(4), vec![PERSON], vec![]);
    seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(1))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Five exact answers on one fixture — the non-strict/strict split on the
/// vertex scan is the headline.
#[test]
fn node_only_greater_or_equal_includes_the_boundary_person() {
    under_lab(0x31_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("node-ge");
        let db = seeded(cx, &dir).await;

        let ge = db.execute_gql(GE_A, &bind_all()).expect("WHERE a.k >= 1 executes");
        assert_eq!(
            ge,
            vec![VId(1), VId(2)],
            "boundary AND above-boundary Persons answer — equal to the \
             strict answer would mean >= landed as a renamed >"
        );
        assert!(
            !ge.contains(&VId(4)),
            "missing k is not >= anything: the keyless Person is out"
        );
        assert!(
            !ge.contains(&VId(3)),
            "the unlabeled k=1 carrier is out of the LABELED scan — the \
             predicate does not widen the label"
        );
        assert!(!ge.contains(&VId(6)), "0 >= 1 is false");

        assert_eq!(
            db.execute_gql(GT_A, &bind_all()).expect("WHERE a.k > 1 executes"),
            vec![VId(2)],
            "the strict sibling excludes the boundary on the same fixture"
        );
        assert_eq!(
            db.execute_gql(EQ_A, &bind_all()).expect("WHERE a.k = 1 executes"),
            vec![VId(1)],
            "equality answers the boundary Person alone"
        );
        assert_eq!(
            db.execute_gql(LT_A, &bind_all()).expect("WHERE a.k < 1 executes"),
            vec![VId(6)],
            "strict less is unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(PLAIN_A, &bind_all()).expect("unfiltered executes"),
            vec![VId(1), VId(2), VId(4), VId(6)],
            "without WHERE every Person answers — and only Persons"
        );
    });
}

/// Two refusals hold the grammar's edges: node-only `<=` is not grammar
/// this slice, and a predicate does not legalize the bare vertex scan.
#[test]
fn node_only_le_and_the_bare_scan_are_typed_parse_errors() {
    under_lab(0x31_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("refusals");
        let db = seeded(cx, &dir).await;

        for off_grammar in [
            "MATCH (a:Person) WHERE a.k <= 1 RETURN a",
            "MATCH (a) WHERE a.k >= 1 RETURN a",
        ] {
            let err = db.execute_gql(off_grammar, &bind_all()).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
}
