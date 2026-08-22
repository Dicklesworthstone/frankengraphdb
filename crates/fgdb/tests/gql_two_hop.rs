//! **The two-hop pattern: `(a)-[:R]->(b)-[:S]->(c)`**
//! (`fgdb-gql-two-hop-8pfw`).
//!
//! The first COMPOSED pattern: hop two expands only from vertices hop one
//! reached. The fixture plants both ways to fake it — an `:S` edge whose
//! source is not an `:R` destination (`9-[:S]->8`: a kernel that scans all
//! `:S` edges returns 8) and an `:R` edge with no `:S` continuation
//! (`1-[:R]->7`: nothing to compose, so 7 must not leak into `RETURN c`
//! while still answering in the one-hop statement). All three projections
//! are pinned, the one-hop grammar is re-pinned beside the new one, an
//! unbound RETURN variable stays a parse error, and a bind missing `S` is
//! a typed error — a two-name statement with a one-name bind must refuse,
//! never answer empty.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const TWO_HOP_C: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN c";
const TWO_HOP_A: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN a";
const TWO_HOP_B: &str = "MATCH (a)-[:R]->(b)-[:S]->(c) RETURN b";
const ONE_HOP_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-two-hop-{}-{name}", std::process::id()))
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

/// The composed fixture, decoys included:
/// `1-[:R]->2-[:S]->4`, `3-[:R]->2-[:S]->5`, plus `9-[:S]->8` (an `:S`
/// source that is no `:R` destination) and `1-[:R]->7` (an `:R` hop with
/// no continuation).
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut r_batch = WriteBatch::new(R);
    for vid in [1u128, 2, 3, 4, 5, 7, 8, 9] {
        r_batch.create_vertex(VId(vid), vec![LabelId(3)], vec![]);
    }
    r_batch.add_edge(EId(10), VId(1), VId(2), vec![]);
    r_batch.add_edge(EId(11), VId(3), VId(2), vec![]);
    r_batch.add_edge(EId(12), VId(1), VId(7), vec![]);
    db.write(cx, r_batch).await.expect("R edges commit");
    let mut s_batch = WriteBatch::new(S);
    s_batch.add_edge(EId(20), VId(2), VId(4), vec![]);
    s_batch.add_edge(EId(21), VId(2), VId(5), vec![]);
    s_batch.add_edge(EId(22), VId(9), VId(8), vec![]);
    db.write(cx, s_batch).await.expect("S edges commit");
    db
}

/// `RETURN c`: only the COMPOSED destinations — the `:S`-from-nowhere 8 is
/// out (its source is no `:R` destination) and the dangling `:R` target 7
/// is out (it has no `:S` continuation).
#[test]
fn return_c_answers_only_the_composed_destinations() {
    under_lab(0x24_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("composed");
        let db = seeded(cx, &dir).await;

        let rows = db
            .execute_gql(TWO_HOP_C, &bind_rs())
            .expect("two-hop RETURN c executes");
        assert_eq!(
            rows,
            vec![VId(4), VId(5)],
            "hop two expands only from hop one's destinations: 8 rides an \
             :S edge from nowhere and 7 has no continuation"
        );
    });
}

/// The other two projections of the same composed match: the sources that
/// complete BOTH hops, and the middle vertex.
#[test]
fn return_a_and_b_project_the_composed_endpoints() {
    under_lab(0x24_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("endpoints");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(TWO_HOP_A, &bind_rs())
                .expect("two-hop RETURN a executes"),
            vec![VId(1), VId(3)],
            "sources whose :R hop continues over :S — 1 qualifies through 2, \
             not through its dangling edge to 7"
        );
        assert_eq!(
            db.execute_gql(TWO_HOP_B, &bind_rs())
                .expect("two-hop RETURN b executes"),
            vec![VId(2)],
            "the middle vertex: an :R destination that carries :S edges — \
             7 does not, 9 was never reached"
        );
    });
}

/// The one-hop statement beside the composed one: `[2, 7]` — 7 is a real
/// `:R` destination even though it composes nothing. Growing a hop must
/// not have moved the one-hop answer.
#[test]
fn the_one_hop_statement_is_unmoved() {
    under_lab(0x24_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("one-hop");
        let db = seeded(cx, &dir).await;

        assert_eq!(
            db.execute_gql(ONE_HOP_B, &bind_rs())
                .expect("one-hop RETURN b executes"),
            vec![VId(2), VId(7)],
            "the dangling :R destination answers in one hop, only the \
             composed statement excludes it"
        );
    });
}

/// The refusal arms: an unbound RETURN variable is the typed parse error,
/// and a bind that cannot name `S` is a typed error — never empty rows,
/// because an unanswerable statement is not an answered-empty one.
#[test]
fn unbound_return_and_missing_s_bind_refuse_typed() {
    under_lab(0x24_04, |cx| async move {
        let cx = &cx;
        let dir = scratch("refusals");
        let db = seeded(cx, &dir).await;

        let err = db
            .execute_gql("MATCH (a)-[:R]->(b)-[:S]->(c) RETURN d", &bind_rs())
            .expect_err("d is bound by nothing");
        assert!(
            matches!(err, GqlError::Parse(_)),
            "an unbound RETURN variable is the typed parse arm, got {err:?}"
        );

        let r_only = RelationBind::new().with_relation("R", R);
        let err = db
            .execute_gql(TWO_HOP_C, &r_only)
            .expect_err("the bind cannot name S; empty rows would be a lie");
        assert!(
            matches!(err, GqlError::Bind(_)),
            "a missing relation name is the typed bind arm, got {err:?}"
        );
    });
}
