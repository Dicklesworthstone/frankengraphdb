//! **First-committer-wins on the product write path** (`fgdb-fcw-writebatch-6cxf`).
//!
//! Gate G1's Genesis slice requires "commit validation is not
//! PassThroughValidator", and this file is the product-level witness: two
//! `WriteBatch`es prepared against ONE snapshot, where the first
//! `commit_prepared` wins and the overlapping second receives the typed FCW
//! abort — while disjoint write-sets against the same basis both commit, and
//! sequential `write()` (basis advanced between commits) never false-aborts.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the bead's names; the
//! validator landed in `crates/fgdb/src/fcw.rs`, the product wiring rides the
//! same bead):
//! - `Database::prepare_write(WriteBatch) -> Result<_, WriteError>` — encode a
//!   batch against the handle's live snapshot without committing, so two
//!   batches can share a basis.
//! - `Database::commit_prepared(&CommitCx, _) -> Result<CommitSeq, WriteError>`
//!   (async) — submit a prepared batch through the two-fsync path.
//! - The FCW refusal is a TYPED `WriteError` arm, not a generic
//!   `WriteError::Commit(_)` wrap, and its `Debug` carries the law ID
//!   `FG-LAW-FCW-01` (`crates/fgdb/src/fcw.rs::FCW_LAW`).
//! Until that wiring lands, this file fails to compile — deliberately. Per the
//! code-first wave it is committed anyway as the executable acceptance
//! criteria; do not weaken it to make it compile.
//!
//! **WHY THE LAW-STRING ASSERTIONS ARE LOAD-BEARING (the planted negative).**
//! The live fold already refuses create/create (`AlreadyLive`) — a rename of
//! `PassThroughValidator` plus the fold would still abort SOME second writes.
//! Every conflict in this file is a property-family update on an element both
//! batches saw as live: the fold has no refusal for that shape, so the ONLY
//! component that can abort it is the installed FCW validator, and only the
//! validator emits `FG-LAW-FCW-01`. A test that merely asserted "some error"
//! would pass under a fold-only abort; asserting the law kills that cheat.
//! Swapping `PassThroughValidator` back in cannot satisfy these tests either:
//! pass-through never rejects, so the expected abort simply does not arrive.
//! (No test-only validator-swap hook exists on `Database`, and adding one is
//! the implementation slice's call, not this file's.)
//!
//! **NO ORACLE REPLAY HERE, ON PURPOSE.** `fgdb-reference` carries a
//! registered dependency allowlist (§15.2) that forbids `fgdb` from depending
//! on the oracle even as a dev-dependency; the differential lives in
//! `crates/fgdb-sim/tests/`. This file proves the engine against itself
//! across a reopen, exactly like `spine.rs`.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, VId};
use std::path::PathBuf;

const KNOWS: RelationId = RelationId(1);
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
    std::env::temp_dir().join(format!("fgdb-fcw-{}-{name}", std::process::id()))
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

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value.into())
}

/// Seed two live vertices so every conflicting batch below is a pure
/// property-family UPDATE — the shape the fold cannot refuse on its own.
async fn seeded_database(cx: &CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    seed.create_vertex(VId(2), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

/// The typed-abort discipline in one place: the loser of first-committer-wins
/// must NOT surface as the generic `WriteError::Commit` wrap, must not be any
/// of the fold's own refusals, and must name the FCW law in its rendering.
fn assert_typed_fcw_abort(err: &WriteError) {
    assert!(
        !matches!(err, WriteError::Commit(_)),
        "the FCW loser must be a typed WriteError arm, not the generic \
         Commit wrap: {err:?}"
    );
    assert!(
        !matches!(
            err,
            WriteError::AlreadyLive { .. }
                | WriteError::IdentitySpent { .. }
                | WriteError::UnknownVertex { .. }
                | WriteError::UnknownEdge { .. }
                | WriteError::DanglingEndpoint { .. }
        ),
        "a property-update conflict is invisible to the fold; a fold arm here \
         means FCW never ran: {err:?}"
    );
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("FG-LAW-FCW-01"),
        "the abort must be attributable to the FCW law — pass-through cannot \
         emit it and the fold does not know it: {rendered}"
    );
}

/// Two batches prepared against ONE snapshot, overlapping on `VId(1)`: the
/// first `commit_prepared` wins, the second receives the typed FCW abort,
/// nothing of the loser is durable, and the handle is not poisoned.
#[test]
fn overlapping_prepared_batches_abort_the_second_committer() {
    under_lab(0xfc_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("overlap");
        {
            let mut db = seeded_database(cx, &dir).await;
            let basis = db.frontier().expect("healthy frontier");

            let mut first = WriteBatch::new(KNOWS);
            first.set_vertex_property(VId(1), PROP, Some(int(1)));
            let mut second = WriteBatch::new(KNOWS);
            second.set_vertex_property(VId(1), PROP, Some(int(2)));

            let first = db.prepare_write(first).expect("first prepares");
            let second = db
                .prepare_write(second)
                .expect("second prepares against the SAME snapshot");
            assert_eq!(
                db.frontier().expect("healthy frontier"),
                basis,
                "prepare_write must not advance the basis, or the two batches \
                 never shared a snapshot and this test proves nothing"
            );

            db.commit_prepared(cx, first)
                .await
                .expect("the first committer wins");
            let err = db
                .commit_prepared(cx, second)
                .await
                .expect_err("the overlapping second committer must lose");
            assert_typed_fcw_abort(&err);

            // The abort is a verdict, not damage: the same handle still
            // serves reads and accepts a fresh write.
            let row = db.vertex(VId(1)).expect("reads").expect("live row");
            assert_eq!(row.props, vec![(PROP, int(1))]);
            let mut fresh = WriteBatch::new(KNOWS);
            fresh.set_vertex_property(VId(2), PROP, Some(int(9)));
            db.write(cx, fresh)
                .await
                .expect("a fresh write after the abort commits");
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        let row = db.vertex(VId(1)).expect("reads").expect("durable row");
        assert_eq!(
            row.props,
            vec![(PROP, int(1))],
            "only the first committer's property survives the reopen; the \
             loser left no durable trace"
        );
        let bystander = db.vertex(VId(2)).expect("reads").expect("durable row");
        assert_eq!(bystander.props, vec![(PROP, int(9))]);
    });
}

/// Same two-prepares-one-snapshot shape, DISJOINT vids: FCW must judge the
/// write-set, not the basis alone, so both commit and both survive reopen.
#[test]
fn disjoint_prepared_batches_both_commit() {
    under_lab(0xfc_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("disjoint");
        {
            let mut db = seeded_database(cx, &dir).await;

            let mut first = WriteBatch::new(KNOWS);
            first.set_vertex_property(VId(1), PROP, Some(int(1)));
            let mut second = WriteBatch::new(KNOWS);
            second.set_vertex_property(VId(2), PROP, Some(int(2)));

            let first = db.prepare_write(first).expect("first prepares");
            let second = db
                .prepare_write(second)
                .expect("second prepares against the same snapshot");

            db.commit_prepared(cx, first)
                .await
                .expect("disjoint: first commits");
            db.commit_prepared(cx, second)
                .await
                .expect("disjoint write-sets against one basis must BOTH commit");
        }

        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(1))]
        );
        assert_eq!(
            db.vertex(VId(2)).expect("reads").expect("row").props,
            vec![(PROP, int(2))]
        );
    });
}

/// Sequential `write()` of two overlapping updates — the second is prepared
/// AFTER the first committed, so its basis is current and FCW must not
/// false-abort. This is the test that goes red if the installed validator
/// remembers elements forever instead of judging against the batch's basis:
/// "first committer wins" is a verdict between RIVALS for one basis, not a
/// permanent write-once lock on every element.
#[test]
fn sequential_overlapping_writes_do_not_false_abort() {
    under_lab(0xfc_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("sequential");
        {
            let mut db = seeded_database(cx, &dir).await;

            let mut first = WriteBatch::new(KNOWS);
            first.set_vertex_property(VId(1), PROP, Some(int(1)));
            db.write(cx, first)
                .await
                .expect("first sequential update commits");

            let mut second = WriteBatch::new(KNOWS);
            second.set_vertex_property(VId(1), PROP, Some(int(2)));
            db.write(cx, second).await.expect(
                "the second update's basis is current — an abort here is a \
                 false positive, not first-committer-wins",
            );

            let row = db.vertex(VId(1)).expect("reads").expect("live row");
            assert_eq!(row.props, vec![(PROP, int(2))]);
        }

        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(2))]
        );
    });
}

/// The product open path installs first-committer-wins, not pass-through.
///
/// `Database` exposes no validator accessor today, so the identity witness is
/// behavioral, and it is decisive: `PassThroughValidator::validate` is
/// `Ok(())` for every draft, so no configuration of pass-through can deliver
/// the abort demanded here — and no fold arm can either, because a
/// property-family update on a live element is not a fold refusal. An abort
/// carrying `FG-LAW-FCW-01` can only come from `FirstCommitterWinsValidator`
/// installed on the product coordinator. If a Debug/type-name accessor lands
/// later, add the direct identity assertion beside this one; do not replace
/// it — the behavioral form is the one a renamed pass-through cannot fake.
#[test]
fn product_open_installs_fcw_not_pass_through() {
    under_lab(0xfc_04, |cx| async move {
        let cx = &cx;
        let dir = scratch("not-pass-through");
        let mut db = seeded_database(cx, &dir).await;

        let mut first = WriteBatch::new(KNOWS);
        first.set_vertex_property(VId(1), PROP, Some(int(1)));
        let mut second = WriteBatch::new(KNOWS);
        second.set_vertex_property(VId(1), PROP, Some(int(2)));
        let first = db.prepare_write(first).expect("first prepares");
        let second = db.prepare_write(second).expect("second prepares");

        db.commit_prepared(cx, first).await.expect("first commits");
        let err = db
            .commit_prepared(cx, second)
            .await
            .expect_err("pass-through would have committed this loser");
        assert_typed_fcw_abort(&err);
    });
}

/// The planted negative, stated as its own test so the cheat it kills is
/// documented where a reviewer will look for it.
///
/// THE CHEAT: rename or wrap `PassThroughValidator`, let the live fold's
/// `AlreadyLive` refusal absorb create/create conflicts, and call the result
/// "first-committer-wins". Under that cheat every assertion here fails in a
/// specific, legible way:
/// - the conflict below is update/update on one live element — the fold has
///   no arm for it, so the expected abort never arrives (`expect_err` panics);
/// - even if some other component manufactured an error, it could not render
///   `FG-LAW-FCW-01`, which only `fcw.rs` emits (`assert_typed_fcw_abort`).
/// A validator-swap hook would make this a direct experiment (install
/// pass-through, watch this test fail); `Database` has no such hook and this
/// file does not add one — the law-string is the substitute the wave
/// specified, and it is sufficient: a fold-only abort cannot satisfy it.
#[test]
fn overlapping_abort_is_attributable_to_fcw_not_the_fold() {
    under_lab(0xfc_05, |cx| async move {
        let cx = &cx;
        let dir = scratch("attributable");
        let mut db = seeded_database(cx, &dir).await;

        // Both rivals UPDATE a property on a vertex both snapshots hold as
        // live. No create rows: `AlreadyLive` / `IdentitySpent` cannot fire.
        let mut winner = WriteBatch::new(KNOWS);
        winner.set_vertex_property(VId(1), PROP, Some(int(41)));
        let mut loser = WriteBatch::new(KNOWS);
        loser.set_vertex_property(VId(1), PROP, Some(int(42)));

        let winner = db.prepare_write(winner).expect("winner prepares");
        let loser = db.prepare_write(loser).expect("loser prepares");
        db.commit_prepared(cx, winner)
            .await
            .expect("winner commits");
        let err = db
            .commit_prepared(cx, loser)
            .await
            .expect_err("the loser must abort — and only FCW can be the aborter");
        assert_typed_fcw_abort(&err);

        // The loser's value must be nowhere: not in the live fold now, and
        // (per the overlap test's reopen) never durable.
        let row = db.vertex(VId(1)).expect("reads").expect("live row");
        assert_eq!(row.props, vec![(PROP, int(41))]);
    });
}
