//! **`WHERE b.k >= 1` keeps sources of greater-or-equal dests**
//! (`fgdb-w5-parsers-nje.28`).
//!
//! The non-strict comparator reaches the DESTINATION side: `>= 1` answers
//! the boundary dest's source AND the above-boundary dest's source —
//! `[1, 3]` beside the strict `[3]` on one fixture, so a renamed `>`
//! fails; the `k = 0` dest separates `>=` from `<>` (not-equal would
//! admit 5); and the keyless dest's source 7 stays out of the non-strict
//! comparator too. The strict-greater, equality, strict-less, and
//! unfiltered statements are pinned alongside, and the still-unsupported
//! dest `<=` spelling remains a typed parse error (the source `<=`
//! graduated under fgdb-w5-parsers-nje.29 into its own suite).

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const GE_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k >= 1 RETURN a";
const GT_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a";
const EQ_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a";
const LT_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k < 1 RETURN a";
const PLAIN_A: &str = "MATCH (a)-[:R]->(b) RETURN a";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-dst-ge-{}-{name}", std::process::id()))
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

fn bind_rk() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_property("k", K)
}

/// Every dest-side carrier state on its own edge: boundary, above, below,
/// keyless.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![(K, CanonicalScalar::Int(1))]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.create_vertex(VId(4), vec![], vec![(K, CanonicalScalar::Int(9))]);
    seed.create_vertex(VId(5), vec![], vec![]);
    seed.create_vertex(VId(6), vec![], vec![(K, CanonicalScalar::Int(0))]);
    seed.create_vertex(VId(7), vec![], vec![]);
    seed.create_vertex(VId(8), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(4), vec![]);
    seed.add_edge(EId(12), VId(5), VId(6), vec![]);
    seed.add_edge(EId(13), VId(7), VId(8), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Five exact answers on one fixture — the non-strict/strict split on the
/// dest side is the headline.
#[test]
fn dest_greater_or_equal_includes_the_boundary_and_strict_does_not() {
    under_lab(0x28_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("dst-ge");
        let db = seeded(cx, &dir).await;

        let ge = db.execute_gql(GE_A, &bind_rk()).expect("WHERE b.k >= 1 executes");
        assert_eq!(
            ge,
            vec![VId(1), VId(3)],
            "boundary and above-boundary dests' SOURCES answer — equal to \
             the strict answer would mean >= landed as a renamed >"
        );
        assert!(
            !ge.contains(&VId(5)),
            "the k=0 dest separates >= from <>: not-equal admits 5, \
             greater-or-equal does not"
        );
        assert!(
            !ge.contains(&VId(7)),
            "missing k is not >= anything: the keyless dest's source is out"
        );

        assert_eq!(
            db.execute_gql(GT_A, &bind_rk()).expect("WHERE b.k > 1 executes"),
            vec![VId(3)],
            "the strict sibling excludes the boundary on the same fixture"
        );
        assert_eq!(
            db.execute_gql(EQ_A, &bind_rk()).expect("WHERE b.k = 1 executes"),
            vec![VId(1)],
            "equality answers the boundary carrier's source alone"
        );
        assert_eq!(
            db.execute_gql(LT_A, &bind_rk()).expect("WHERE b.k < 1 executes"),
            vec![VId(5)],
            "strict less is unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(PLAIN_A, &bind_rk()).expect("unfiltered executes"),
            vec![VId(1), VId(3), VId(5), VId(7)],
            "without WHERE every source answers"
        );
    });
}

/// The still-unsupported dest `<=` spelling is a typed parse error — the
/// dest `>=` graduating does not legalize its mirror. (The source `<=`
/// graduated under fgdb-w5-parsers-nje.29 and lives in its own suite.)
#[test]
fn the_dest_le_spelling_is_still_a_typed_parse_error() {
    under_lab(0x28_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("le-refused");
        let db = seeded(cx, &dir).await;

        // Narrowed by fgdb-w5-parsers-nje.29: the SOURCE <= spelling
        // graduated to grammar (its positive suite is gql_where_prop_le.rs),
        // so only the dest spelling remains a planted negative here.
        for off_grammar in ["MATCH (a)-[:R]->(b) WHERE b.k <= 1 RETURN a"] {
            let err = db.execute_gql(off_grammar, &bind_rk()).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
}
