//! **MATCH phantoms: a new qualifying edge aborts the reader**
//! (`fgdb-w4-g1-txn-core-qpmg.7`).
//!
//! The read-set suites pinned overwrites of rows a MATCH RETURNED. The
//! phantom is the other hole: a concurrent committer that ADDS a `:R` edge
//! changes what the statement WOULD have returned without touching any row
//! it did return. A matcher that certified "the destinations are exactly
//! [2]" and commits after the answer became [2, 9] certified a dead
//! predicate — the MATCH read-dependency is the PATTERN, not only its rows.
//!
//! **CONSTRUCTION, same as the sibling suites.** The matcher's staged write
//! touches ONLY `VId(3)`; the phantom writer touches `VId(9)`/`EId(11)`.
//! Write-sets and returned-row sets are all disjoint, so neither the
//! write-set FCW validator nor returned-row read-set recording can fire —
//! only a predicate-level dependency (however the implementation scopes it:
//! source-vertex expansion footprint, relation-level, or finer) can deliver
//! test 1's abort. Test 2 is the over-approximation guard CARRIED FORWARD
//! from qpmg.6: a bare vertex with no `:R` edge changes no answer and must
//! still commit — a "phantom guard" that aborts on any concurrent commit is
//! a serial lock, not predicate validation.

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
    std::env::temp_dir().join(format!("fgdb-gql-phantom-{}-{name}", std::process::id()))
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

/// One committed `:R` edge `VId(1) -> VId(2)`: the MATCH answer is exactly
/// `[VId(2)]` until the phantom writer widens it.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![]);
    seed.create_vertex(VId(2), vec![], vec![(PROP, int(0))]);
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// A concurrent commit ADDS `1-[:R]->9` after txn A matched `[2]`: A's
/// commit must abort typed, rendering FG-LAW-FCW-READ-01, even though no
/// row A returned was touched. Reopen: the shared MATCH serves `[2, 9]`
/// and nothing of A's txn is durable.
#[test]
fn a_new_qualifying_edge_aborts_the_matcher_typed() {
    under_lab(0xf1_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("phantom-edge");
        {
            let mut db = seeded(&commit, &dir).await;

            // Txn A matches [2] and stages a write touching ONLY VId(3).
            let mut txn_a = db.begin(&txn_cx).expect("matcher txn begins");
            let matched = txn_a
                .execute_gql(&db, PINNED, &bind_r())
                .expect("the txn's MATCH executes");
            assert_eq!(matched, vec![VId(2)], "the pre-phantom answer");
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![LabelId(5)], vec![(PROP, int(3))]);
            txn_a
                .write(&mut db, disjoint)
                .expect("stages the disjoint write");

            // Txn B widens the ANSWER without touching any returned row:
            // a new vertex and a new qualifying edge from the same source.
            let mut txn_b = db.begin(&txn_cx).expect("phantom writer begins");
            let mut widen = WriteBatch::new(R);
            widen.create_vertex(VId(9), vec![], vec![(PROP, int(9))]);
            widen.add_edge(EId(11), VId(1), VId(9), vec![]);
            txn_b
                .write(&mut db, widen)
                .expect("stages the phantom edge");
            txn_b
                .commit(&mut db, &commit)
                .await
                .expect("the phantom writer commits first");

            // A's write-set {VId(3)}, B's {VId(9), EId(11)}: disjoint. The
            // rows A RETURNED are untouched too. Only a predicate-level
            // dependency can abort this commit — a returned-rows-only
            // read-set commits a certificate of a dead answer here.
            let err = txn_a
                .commit(&mut db, &commit)
                .await
                .expect_err("the matched predicate is dead; the commit must abort");
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
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("executes after reopen"),
            vec![VId(2), VId(9)],
            "the widened answer is durable"
        );
        assert!(
            db.vertex(VId(3)).expect("reads").is_none(),
            "nothing of the aborted matcher's txn is durable"
        );
    });
}

/// REGRESSION of qpmg.6's guard rail, restated here so the phantom fix
/// cannot land as a serial lock: a concurrent BARE vertex create — no `:R`
/// edge, no answer change — must not abort the matcher, and both effects
/// are durable side by side across reopen.
#[test]
fn a_bare_concurrent_create_still_does_not_abort_the_matcher() {
    under_lab(0xf1_02, |contexts| async move {
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
                "no answer A certified changed — an abort here means the \
                 phantom guard is a serial lock",
            );
        }

        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.execute_gql(PINNED, &bind_r())
                .expect("executes after reopen"),
            vec![VId(2)],
            "the bare vertex never joined the answer"
        );
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
