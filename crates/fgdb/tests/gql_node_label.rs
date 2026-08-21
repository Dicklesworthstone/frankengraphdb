//! **The labeled source: `(a:Person)-[:R]->(b)`** (`fgdb-w5-parsers-nje.5`).
//!
//! The first node predicate in the pattern: a label on `a` narrows the
//! expansion to sources CARRYING that label. The fixture pairs a labeled
//! and an unlabeled source with disjoint destinations, so the labeled
//! statement answering `[2]`-not-`[4]` cannot be the unlabeled scan in
//! disguise, and the unlabeled statement is re-pinned beside it holding
//! both. The label name resolves through the caller's bind exactly like a
//! relation name — no invented catalog — so an unbound label is the typed
//! Bind arm, and a label on the not-yet-grammatical labeled two-hop stays
//! the typed Parse arm.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bind-builder style of
//! `with_relation`): `RelationBind::with_label(name, LabelId)`. Until it
//! lands this file fails to compile — deliberately; do not weaken it.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PERSON: LabelId = LabelId(3);
const LABELED_B: &str = "MATCH (a:Person)-[:R]->(b) RETURN b";
const UNLABELED_B: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-node-label-{}-{name}", std::process::id()))
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

fn bind_r_person() -> RelationBind {
    RelationBind::new()
        .with_relation("R", R)
        .with_label("Person", PERSON)
}

/// A labeled and an unlabeled source with DISJOINT destinations: the
/// labeled answer cannot be the unlabeled one in disguise.
async fn seeded(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![PERSON], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.create_vertex(VId(3), vec![], vec![]);
    seed.create_vertex(VId(4), vec![], vec![]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    seed.add_edge(EId(11), VId(3), VId(4), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// The label narrows the expansion to labeled sources; the unlabeled
/// statement beside it still answers both destinations.
#[test]
fn the_label_excludes_unlabeled_sources() {
    under_lab(0x1b_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("labeled");
        let db = seeded(cx, &dir).await;

        let labeled = db
            .execute_gql(LABELED_B, &bind_r_person())
            .expect("the labeled MATCH executes");
        assert!(
            labeled.contains(&VId(2)),
            "the :Person source's destination answers: {labeled:?}"
        );
        assert!(
            !labeled.contains(&VId(4)),
            "the unlabeled source's destination is out — the label is a \
             filter on the SOURCE, not decoration: {labeled:?}"
        );

        let unlabeled = db
            .execute_gql(UNLABELED_B, &bind_r_person())
            .expect("the unlabeled MATCH executes");
        assert!(
            unlabeled.contains(&VId(2)) && unlabeled.contains(&VId(4)),
            "without the label both destinations answer — the labeled \
             kernel did not narrow the unlabeled statement: {unlabeled:?}"
        );
    });
}

/// A label name the bind cannot resolve is the typed Bind arm — an
/// unanswerable statement, never an empty answer; and a label on the
/// not-yet-grammatical two-hop stays the typed Parse arm.
#[test]
fn unbound_label_is_bind_and_labeled_two_hop_is_parse() {
    under_lab(0x1b_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("refusals");
        let db = seeded(cx, &dir).await;

        let err = db
            .execute_gql("MATCH (a:Missing)-[:R]->(b) RETURN b", &bind_r_person())
            .expect_err("the bind cannot name Missing");
        assert!(
            matches!(err, GqlError::Bind(_)),
            "an unbound label is the typed bind arm, got {err:?}"
        );

        let err = db
            .execute_gql(
                "MATCH (a:Person)-[:R]->(b)-[:S]->(c) RETURN c",
                &bind_r_person(),
            )
            .expect_err("the labeled two-hop is not grammar yet");
        assert!(
            matches!(err, GqlError::Parse(_)),
            "a label on the two-hop pattern is the typed parse arm, got {err:?}"
        );
    });
}
