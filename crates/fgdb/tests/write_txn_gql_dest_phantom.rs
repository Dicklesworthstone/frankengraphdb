//! **Destination-side phantoms: a new edge INTO an observed destination
//! aborts the matcher** (`fgdb-w4-g1-txn-core-qpmg.21`).
//!
//! The nastiest phantom shape: B commits `9-[:R]->2` — a new SOURCE into
//! the destination A already observed. The RETURNED row set does not even
//! change (`[2]` before, `[2]` after — destinations dedup), so a guard
//! keyed on "did my answer's rows change" sails through, and A's write-set
//! (`{VId(3)}`) is disjoint from B's (`{VId(9), EId(11)}`) so write-set FCW
//! is blind too. What died is the MATCH observation itself: A certified the
//! incidence structure around dest 2 — how the pattern binds, not merely
//! which vids came back — and B changed it. Only a predicate/expansion-
//! footprint dependency covering the OBSERVED DESTINATION's incidence can
//! deliver this abort. The regression test keeps the guard honest from the
//! other side: a bare concurrent vertex changes no incidence and must not
//! abort.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, RelationBind, WriteBatch, WriteError, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const PROP: PropertyKeyId = PropertyKeyId(7);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-dest-phantom-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(PurposeContexts) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

/// One committed `:R` edge `VId(1) -> VId(2)`: the observation whose
/// incidence B disturbs.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![(PROP, int(0))]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// B lands a new edge INTO the destination A observed — the returned rows
/// do not change, the write-sets are disjoint — and A's commit must still
/// abort typed, rendering FG-LAW-FCW-READ-01. Reopen: the answer still
/// contains VId(2) (B's edge points at the same destination) and A's
/// disjoint create is nowhere.
#[test]
fn a_new_edge_into_the_observed_destination_aborts_the_matcher() {
    under_lab(0xdf_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("into-dest");
        {
            let mut db = seeded(&commit, &dir).await;

            // Txn A matches [2] and stages a write touching ONLY VId(3).
            let mut txn_a = db.begin(&txn_cx).expect("matcher txn begins");
            let matched = txn_a
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(matched, vec![VId(2)], "the observed answer");
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![LabelId(5)], vec![(PROP, int(3))]);
            txn_a
                .write(&mut db, disjoint)
                .expect("stages the disjoint write");

            // Txn B: a new SOURCE into the observed destination. The
            // destination set stays [2] — this phantom is invisible to a
            // rows-changed guard by construction.
            let mut txn_b = db.begin(&txn_cx).expect("phantom writer begins");
            let mut into_dest = WriteBatch::new(R);
            into_dest.create_vertex(VId(9), vec![], vec![(PROP, int(9))]);
            into_dest.add_edge(EId(11), VId(9), VId(2), vec![]);
            txn_b
                .write(&mut db, into_dest)
                .expect("stages the incoming edge");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the phantom writer commits first");
            assert_eq!(
                db.execute_gql(PINNED, &bind_r())
                    .expect("base MATCH executes"),
                vec![VId(2)],
                "control: the RETURNED rows did not change — only the \
                 incidence around the observed destination did"
            );

            // A={VId(3)}, B={VId(9), EId(11)}: write-sets disjoint, returned
            // rows identical. Only the observed destination's incidence
            // footprint can abort this commit.
            let err = txn_a
                .commit(&mut db, &commit)
                .await
                .expect_err("the observed incidence is dead; the commit must abort");
            assert!(
                !matches!(err, WriteTxnError::Write(WriteError::Commit(_))),
                "the abort must be typed, not the generic Commit wrap: {err:?}"
            );
            let rendered = format!("{err:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "the abort must name the read-set law: {rendered}"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        let answer = db
            .execute_gql(PINNED, &bind_r())
            .expect("executes after reopen");
        assert!(
            answer.contains(&VId(2)),
            "the observed destination — now with two incoming edges — is \
             still the durable answer: {answer:?}"
        );
        assert!(
            db.vertex(VId(3)).expect("reads").is_none(),
            "nothing of the aborted matcher's txn is durable"
        );
        assert_eq!(
            db.vertex(VId(9)).expect("reads").expect("row").props,
            vec![(PROP, int(9))],
            "the phantom writer's source vertex is durable"
        );
    });
}

/// REGRESSION: a bare concurrent vertex — no `:R` edge, no incidence
/// change anywhere near the observation — must not abort the matcher.
/// Whatever footprint the implementation records for the dest-side phantom
/// above, it must not degrade into a serial lock.
#[test]
fn a_bare_concurrent_create_still_does_not_abort_the_matcher() {
    under_lab(0xdf_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("bare-ok");
        {
            let mut db = seeded(&commit, &dir).await;

            let mut txn_a = db.begin(&txn_cx).expect("matcher txn begins");
            let matched = txn_a
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(matched, vec![VId(2)]);
            let mut staged = WriteBatch::new(R);
            staged.create_vertex(VId(3), vec![LabelId(5)], vec![(PROP, int(3))]);
            txn_a.write(&mut db, staged).expect("stages its write");

            let mut txn_b = db.begin(&txn_cx).expect("creator txn begins");
            let mut create = WriteBatch::new(R);
            create.create_vertex(VId(9), vec![], vec![(PROP, int(9))]);
            txn_b
                .write(&mut db, create)
                .expect("stages the bare create");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the bare concurrent txn commits");

            txn_a.commit(&mut db, &commit).await.expect(
                "no incidence A observed changed — an abort here means the \
                 dest-side footprint degraded into a serial lock",
            );
        }

        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(3)).expect("reads").expect("row").props,
            vec![(PROP, int(3))],
            "the matcher's create is durable"
        );
        assert_eq!(
            db.vertex(VId(9)).expect("reads").expect("row").props,
            vec![(PROP, int(9))],
            "the concurrent bare create is durable beside it"
        );
    });
}
