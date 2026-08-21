//! **The incoming two-hop: `(a)<-[:R]-(b)<-[:S]-(c)`**
//! (`fgdb-w5-parsers-nje.4`).
//!
//! Both arrows flipped: the edge flow is `c-[:S]->b-[:R]->a`, so hop two
//! must expand only from vertices that are `:R` SOURCES — the incoming
//! mirror of the outgoing composed law. The fixture plants both fakes: an
//! `:S` edge whose destination is not an `:R` source (`8-[:S]->9`: a
//! kernel that collects all `:S` sources returns 8) and an `:R` edge with
//! no `:S` feed (`7-[:R]->1`: `b = 7` completes one hop and composes
//! nothing, so 7 must not leak into `RETURN c`). The incoming one-hop and
//! the outgoing two-hop are re-pinned beside the new shape — the latter is
//! EMPTY on this reversed fixture, which is itself the direction control:
//! a kernel that ignores the arrows answers `[4]` for both composed
//! statements and fails the outgoing one.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const IN_TWO_HOP_C: &str = "MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN c";
const IN_ONE_HOP_B: &str = "MATCH (a)<-[:R]-(b) RETURN b";
const OUT_TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-in-2hop-{}-{name}", std::process::id()))
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

/// The reversed composed fixture: `4-[:S]->2-[:R]->1` (c=4, b=2, a=1),
/// the `:S` decoy `8-[:S]->9`, and the feedless `7-[:R]->1`.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut r_batch = WriteBatch::new(R);
    for vid in [1u128, 2, 4, 7, 8, 9] {
        r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
    }
    r_batch.add_edge(EId(10), VId(2), VId(1), vec![]);
    r_batch.add_edge(EId(11), VId(7), VId(1), vec![]);
    db.write(cx, r_batch).await.expect("R edges commit");
    let mut s_batch = WriteBatch::new(S);
    s_batch.add_edge(EId(20), VId(4), VId(2), vec![]);
    s_batch.add_edge(EId(21), VId(8), VId(9), vec![]);
    db.write(cx, s_batch).await.expect("S edges commit");
    db
}

/// `RETURN c` composes backwards through `:R` sources only: 4 feeds the
/// `:R` source 2; 8 feeds 9, which sources no `:R` edge; and the feedless
/// 7 never reaches `c` at all.
#[test]
fn return_c_answers_only_the_sources_that_feed_an_r_source() {
    under_lab(0x12_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("reversed-compose");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(IN_TWO_HOP_C, &bind_rs())
            .expect("incoming two-hop RETURN c executes");
        assert!(
            rows.contains(&VId(4)),
            "4 feeds the :R source 2 over :S — the composed row: {rows:?}"
        );
        assert!(
            !rows.contains(&VId(8)),
            "8's :S edge lands on 9, which sources no :R edge — a kernel \
             that collects all :S sources leaks it: {rows:?}"
        );
        assert!(
            !rows.contains(&VId(7)),
            "7 completes one hop and composes nothing; it is a b, never a c: {rows:?}"
        );
    });
}

/// The established statements beside the new shape: the incoming one-hop
/// still answers the `:R` sources, and the OUTGOING two-hop is empty on
/// this reversed fixture — the direction control an arrow-blind kernel
/// fails.
#[test]
fn the_established_statements_are_unmoved() {
    under_lab(0x12_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("unmoved");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(IN_ONE_HOP_B, &bind_rs())
                .expect("incoming one-hop RETURN b executes"),
            vec![VId(2), VId(7)],
            "the :R sources, CGSE-sorted — one hop is untouched by the compose"
        );
        assert!(
            db.execute_gql(OUT_TWO_HOP_C, &bind_rs())
                .expect("outgoing two-hop RETURN c executes")
                .is_empty(),
            "no :S edge leaves an :R destination on this reversed fixture — \
             an arrow-blind kernel answers [4] here and is caught"
        );
    });
}

/// An unbound RETURN variable on the incoming composed statement is the
/// typed parse arm.
#[test]
fn return_of_an_unbound_variable_is_a_typed_parse_error() {
    under_lab(0x12_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("unbound");
        let db = seeded(cx, &dir).await;

        let err = db
            .execute_gql("MATCH (a)<-[:R]-(b)<-[:S]-(c) RETURN d", &bind_rs())
            .expect_err("d is bound by nothing");
        assert!(
            matches!(err, GqlError::Parse(_)),
            "an unbound RETURN variable is the typed parse arm, got {err:?}"
        );
    });
}
