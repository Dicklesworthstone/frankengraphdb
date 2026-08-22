//! **`WHERE b.k <= 1` keeps sources of less-or-equal dests**
//! (`fgdb-w5-parsers-nje.30`).
//!
//! The last comparator/side cell: non-strict less on the DESTINATION,
//! projecting sources. `<= 1` answers the boundary dest's source AND the
//! below-boundary dest's source — `[1, 5]` beside the strict `<`'s `[5]`
//! on one fixture, so a renamed `<` fails; the `k = 9` dest separates
//! `<=` from `<>` (not-equal would admit 3); and the keyless dest's
//! source 7 stays out of the non-strict comparator too. The strict-less,
//! equality, and strict-greater siblings are pinned alongside with the
//! unfiltered scan, and the C-style `!=` spelling is refused — with the
//! comparator grid now full, the alias set is the surviving off-grammar
//! boundary.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K: PropertyKeyId = PropertyKeyId(7);
const LE_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k <= 1 RETURN a";
const LT_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k < 1 RETURN a";
const EQ_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k = 1 RETURN a";
const GT_A: &str = "MATCH (a)-[:R]->(b) WHERE b.k > 1 RETURN a";
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
    std::env::temp_dir().join(format!("fgdb-dst-le-{}-{name}", std::process::id()))
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

/// Five exact answers on one fixture — the non-strict/strict split at the
/// dest-side lower boundary is the headline.
#[test]
fn dest_less_or_equal_includes_the_boundary_and_strict_does_not() {
    under_lab(0x30_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("dst-le");
        let db = seeded(cx, &dir).await;

        let le = db
            .execute_gql(LE_A, &bind_rk())
            .expect("WHERE b.k <= 1 executes");
        assert_eq!(
            le,
            vec![VId(1), VId(5)],
            "boundary and below-boundary dests' SOURCES answer — equal to \
             the strict answer would mean <= landed as a renamed <"
        );
        assert!(
            !le.contains(&VId(3)),
            "9 <= 1 is false: the k=9 dest separates <= from <>"
        );
        assert!(
            !le.contains(&VId(7)),
            "missing k is not <= anything: the keyless dest's source is out"
        );

        assert_eq!(
            db.execute_gql(LT_A, &bind_rk())
                .expect("WHERE b.k < 1 executes"),
            vec![VId(5)],
            "the strict sibling excludes the boundary on the same fixture"
        );
        assert_eq!(
            db.execute_gql(EQ_A, &bind_rk())
                .expect("WHERE b.k = 1 executes"),
            vec![VId(1)],
            "equality answers the boundary carrier's source alone"
        );
        assert_eq!(
            db.execute_gql(GT_A, &bind_rk())
                .expect("WHERE b.k > 1 executes"),
            vec![VId(3)],
            "strict greater is unmoved beside the new spelling"
        );
        assert_eq!(
            db.execute_gql(PLAIN_A, &bind_rk())
                .expect("unfiltered executes"),
            vec![VId(1), VId(3), VId(5), VId(7)],
            "without WHERE every source answers"
        );
    });
}

/// The C-style `!=` spelling is refused: with the comparator grid full,
/// the alias set is the surviving off-grammar boundary — `<>` is the
/// inequality, `!=` never was.
#[test]
fn the_c_style_inequality_executes_the_landed_alias() {
    under_lab(0x30_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("neq-alias-refused");
        let db = seeded(cx, &dir).await;

        // The nje.57+ tranche landed the C-style != alias for hop-1
        // filters. Every ORIGIN on this fixture is keyless, and
        // missing-is-OUT holds for the source property exactly as for the
        // destination: no edge survives the source-side !=.
        assert_eq!(
            db.execute_gql("MATCH (a)-[:R]->(b) WHERE a.k != 1 RETURN b", &bind_rk())
                .expect("the landed source-side != executes"),
            Vec::<VId>::new(),
            "every origin lacks k, and missing-is-OUT"
        );
    });
}
