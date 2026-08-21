//! **The undirected two-hop: `(a)-[:R]-(b)-[:S]-(c)`**
//! (`fgdb-gql-undir-2hop-7mrc`).
//!
//! Undirected compose is an incidence claim about the VIA: `b` must be
//! incident to an `:R` edge (either end) and to an `:S` edge (either end),
//! and `RETURN c` is the unique other endpoints of the `:S` edges at those
//! vias. The fixture plants the counterfeit: `9-[:S]->8` is an `:S` edge
//! neither of whose endpoints is `:R`-incident, so 8 can only appear if
//! the kernel collects `:S` endpoints without requiring the via's dual
//! incidence. The undirected two-hop assertions are deliberately
//! contains-shaped — 4 in, 8 out — so this suite pins the composition law
//! without over-pinning symmetric-binding decisions that belong to the
//! implementation bead; the one-hop undirected and the directed two-hop
//! answers are exact and re-pinned so the new shape cannot have loosened
//! either, and `RETURN d` stays a typed parse error.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const UN_TWO_HOP_C: &str = "MATCH (a)-[:R]-(b)-[:S]-(c) RETURN c";
const UN_ONE_HOP_B: &str = "MATCH (a)-[:R]-(b) RETURN b";
const DIR_TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-undir-2hop-{}-{name}", std::process::id()))
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

fn bind_rs() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_relation("S", S)
}

/// The composed fixture with the dual-incidence counterfeit planted:
/// `1-[:R]->2-[:S]->4`, `3-[:R]->2` (an extra `:R` at the via), and
/// `9-[:S]->8` whose endpoints carry no `:R` incidence at all.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut r_batch = WriteBatch::new(R);
    for vid in [1u128, 2, 3, 4, 8, 9] {
        r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
    }
    r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    r_batch.add_edge(EId(11), VId(3), VId(2), vec![]);
    db.write(cx, r_batch).await.expect("R edges commit");
    let mut s_batch = WriteBatch::new(S);
    s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
    s_batch.add_edge(EId(21), VId(9), VId(8), vec![]);
    db.write(cx, s_batch).await.expect("S edges commit");
    db
}

/// `RETURN c` composes through dual-incident vias only: 4 is reached
/// through via 2 (`:R`-incident and `:S`-incident), while 8 rides an `:S`
/// edge whose endpoints touch no `:R` edge and must be absent.
#[test]
fn return_c_requires_the_via_to_be_incident_to_both_relations() {
    under_lab(0x72_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("dual-incidence");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(UN_TWO_HOP_C, &bind_rs())
            .expect("undirected two-hop RETURN c executes");
        assert!(
            rows.contains(&VId(4)),
            "4 is the other endpoint of an :S edge at the dual-incident via 2: {rows:?}"
        );
        assert!(
            !rows.contains(&VId(8)),
            "8's :S edge touches no :R-incident vertex — a kernel that \
             collects :S endpoints without the via's dual incidence leaks it: {rows:?}"
        );
    });
}

/// The two established statements beside the new shape, exact and
/// unmoved: all `:R` incidents one-hop undirected, and the directed
/// composed answer.
#[test]
fn the_established_statements_are_unmoved() {
    under_lab(0x72_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("unmoved");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(UN_ONE_HOP_B, &bind_rs())
                .expect("undirected one-hop RETURN b executes"),
            vec![VId(1), VId(2), VId(3)],
            "every :R incident, both orientations — the isolates-from-R \
             (4, 8, 9) stay out"
        );
        assert_eq!(
            db.execute_gql(DIR_TWO_HOP_C, &bind_rs())
                .expect("directed two-hop RETURN c executes"),
            vec![VId(4)],
            "the directed composed answer is unchanged by the undirected shape"
        );
    });
}

/// An unbound RETURN variable on the undirected composed statement is the
/// typed parse arm.
#[test]
fn return_of_an_unbound_variable_is_a_typed_parse_error() {
    under_lab(0x72_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("unbound");
        let db = seeded(cx, &dir).await;

        let err = db
            .execute_gql("MATCH (a)-[:R]-(b)-[:S]-(c) RETURN d", &bind_rs())
            .expect_err("d is bound by nothing");
        assert!(
            matches!(err, GqlError::Parse(_)),
            "an unbound RETURN variable is the typed parse arm, got {err:?}"
        );
    });
}
