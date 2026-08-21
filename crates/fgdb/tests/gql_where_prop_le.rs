//! **`WHERE a.k <= 1` keeps dests of less-or-equal sources**
//! (`fgdb-w5-parsers-nje.29`).
//!
//! The non-strict less comparator on the source side: `<= 1` answers the
//! boundary carrier's dest AND the below-boundary carrier's dest —
//! `[2, 6]` beside the strict `<`'s `[6]` on one fixture, so a renamed
//! `<` fails; the `k = 9` carrier separates `<=` from `<>`; and the
//! keyless source's dest 8 stays out of the non-strict comparator too.
//! The strict-less, equality, strict-greater, and non-strict-greater
//! siblings are pinned alongside with the unfiltered scan, the DEST `<=`
//! spelling remains a typed parse error this slice, and the C-style `!=`
//! spelling is refused — the grammar's inequality is `<>`, not a lenient
//! alias set.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const LE_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k <= 1 RETURN b";
const LT_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k < 1 RETURN b";
const EQ_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k = 1 RETURN b";
const GT_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k > 1 RETURN b";
const GE_B: &str = "MATCH (a)-[:R]->(b) WHERE a.k >= 1 RETURN b";
const PLAIN_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-prop-le-{}-{name}", std::process::id()))
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

/// Every source-side carrier state on its own edge: boundary, above,
/// below, keyless.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(K, CanonicalScalar::Int(1))]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![(K, CanonicalScalar::Int(9))]);
    seed.create_vertex(VId(4), vec![], vec![]);
    seed.create_vertex(VId(5), vec![], vec![(K, CanonicalScalar::Int(0))]);
    seed.create_vertex(VId(6), vec![], vec![]);
    seed.create_vertex(VId(7), vec![], vec![]);
    seed.create_vertex(VId(8), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(4), vec![]);
    seed.add_edge(EId(12), VId(5), VId(6), vec![]);
    seed.add_edge(EId(13), VId(7), VId(8), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// Six exact answers on one fixture — the non-strict/strict split at the
/// lower boundary is the headline.
#[test]
fn less_or_equal_includes_the_boundary_and_strict_does_not() {
    under_lab(0x29_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("le");
        let db = seeded(cx, &dir).await;

        let le = db.execute_gql(LE_B, &bind_rk()).expect("WHERE a.k <= 1 executes");
        assert_eq!(
            le,
            vec![VId(2), VId(6)],
            "boundary AND below-boundary carriers answer — equal to the \
             strict answer would mean <= landed as a renamed <"
        );
        assert!(
            !le.contains(&VId(8)),
            "missing k is not <= anything: the keyless source's dest is out \
             of the non-strict comparator too"
        );
        assert!(
            !le.contains(&VId(4)),
            "9 <= 1 is false: the k=9 carrier separates <= from <>"
        );

        assert_eq!(
            db.execute_gql(LT_B, &bind_rk()).expect("WHERE a.k < 1 executes"),
            vec![VId(6)],
            "the strict sibling excludes the boundary on the same fixture"
        );
        assert_eq!(
            db.execute_gql(EQ_B, &bind_rk()).expect("WHERE a.k = 1 executes"),
            vec![VId(2)],
            "equality answers the boundary carrier alone — <= is its union \
             with the strict <"
        );
        assert_eq!(
            db.execute_gql(GT_B, &bind_rk()).expect("WHERE a.k > 1 executes"),
            vec![VId(4)],
            "strict greater is unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(GE_B, &bind_rk()).expect("WHERE a.k >= 1 executes"),
            vec![VId(2), VId(4)],
            "non-strict greater is unmoved too — and <= is not its alias"
        );
        assert_eq!(
            db.execute_gql(PLAIN_B, &bind_rk()).expect("unfiltered executes"),
            vec![VId(2), VId(4), VId(6), VId(8)],
            "without WHERE every dest answers"
        );
    });
}

/// The refusals: the DEST `<=` spelling is still off-grammar this slice,
/// and the C-style `!=` never was grammar — `<>` is the inequality.
#[test]
fn dest_le_and_c_style_inequality_are_typed_parse_errors() {
    under_lab(0x29_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("refusals");
        let db = seeded(cx, &dir).await;

        // Narrowed by fgdb-w5-parsers-nje.30: the dest <= spelling
        // graduated to grammar (its positive suite is
        // gql_where_dst_prop_le.rs), so the C-style alias carries this
        // planted negative alone — moved, not weakened.
        for off_grammar in ["MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b"] {
            let err = db.execute_gql(off_grammar, &bind_rk()).expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm: {err:?}"
            );
        }
    });
}
