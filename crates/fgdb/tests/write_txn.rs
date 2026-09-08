//! **WriteTxn: the pinned-snapshot product transaction**
//! (`fgdb-writetxn-pin-l8wb`).
//!
//! Gap-2 remainder after FCW: `Database::begin` acquires the `TxnCx`
//! `pin_snapshot` obligation and records the frontier as the txn's basis;
//! `WriteTxn::write` prepares against that PINNED basis; `commit` goes
//! through the two-fsync path and releases the pin; `abort` releases the pin
//! with nothing durable. Not Graph-SSI, not the merge ladder, not
//! WriteCoordinator.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (landed at 9048fc5):
//! - `Database::begin(&mut self, txn: &TxnCx) -> Result<WriteTxn, WriteError>`
//! - `WriteTxn::write(&mut self, &mut Database<V>, WriteBatch)` (one batch
//!   per txn this slice)
//! - `WriteTxn::commit(&mut self, &mut Database<V>, &CommitCx)` (async),
//!   returning `WriteTxnError`; the FCW loser surfaces as
//!   `WriteTxnError::Write(WriteError::FirstCommitterWins { .. })`
//! - `WriteTxn::abort(self)`
//!
//! **THE PLANTED NEGATIVE (test 3).** The cheap counterfeit is a `begin`
//! that stores a basis number and never touches `TxnCx::pin_snapshot`: every
//! commit/abort test still passes, but the pin is fiction — nothing in the
//! runtime knows a snapshot is held, so nothing can hold compaction or lab
//! oracles to it. The obligation ledger is the observable:
//! `outstanding_obligations()` on the very `TxnCx` handed to `begin` MUST
//! rise while the txn is open and return to its baseline after commit AND
//! after abort. A pin that was never acquired cannot raise it; a pin that is
//! never released cannot lower it back.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteError, WriteTxn, WriteTxnError};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
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
    std::env::temp_dir().join(format!("fgdb-write-txn-{}-{name}", std::process::id()))
}

/// Unlike the sibling suites this harness hands the test the WHOLE
/// `PurposeContexts`: a txn test needs the `TxnCx` (to begin and to read the
/// obligation ledger) and the `CommitCx` (to commit) as two separately
/// narrowed capabilities, exactly as a session would hold them.
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

fn int(value: i64) -> CanonicalScalar {
    CanonicalScalar::Int(value)
}

/// Seed one live vertex carrying `PROP = 0`, so every conflict below is a
/// pure property-family update — the shape only FCW can refuse.
async fn seeded(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut db = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(KNOWS);
    seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
    db.write(cx, seed).await.expect("seed commits");
    db
}

fn wrong_owner<T>(result: Result<T, WriteTxnError>) {
    let error = result.err().expect("a foreign database must be refused");
    assert!(
        matches!(&error, WriteTxnError::WrongDatabase),
        "expected WrongDatabase, got {error:?}"
    );
}

#[test]
fn foreign_handles_refuse_every_read_and_write_without_consuming_the_owner() {
    under_lab(0x7a11, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        for same_keys in [true, false] {
            let mut owner = seeded(&commit, &scratch(&format!("owner-{same_keys}"))).await;
            let foreign_keys = if same_keys {
                keys()
            } else {
                DatabaseKeys::new([9; 32], DatabaseSecurityNamespaceId([8; 32]), [7; 32])
            };
            let mut foreign = Database::create(
                &commit,
                &scratch(&format!("foreign-{same_keys}")),
                foreign_keys,
            )
            .await
            .expect("foreign creates");
            let mut seed = WriteBatch::new(KNOWS);
            seed.create_vertex(VId(1), vec![LabelId(3)], vec![(PROP, int(0))]);
            foreign.write(&commit, seed).await.expect("foreign seeds");
            assert_eq!(
                owner.frontier().expect("owner frontier"),
                foreign.frontier().expect("foreign frontier")
            );

            let mut direct = WriteBatch::new(KNOWS);
            direct.create_vertex(VId(8), vec![], vec![]);
            let prepared_write = owner.prepare_write(direct).expect("owner prepares");
            assert!(matches!(
                foreign
                    .commit_prepared(&commit, prepared_write.clone())
                    .await,
                Err(WriteError::ForeignPreparedWrite)
            ));
            assert!(matches!(
                foreign
                    .commit_prepared_with_crash(&commit, prepared_write.clone(), None)
                    .await,
                Err(WriteError::ForeignPreparedWrite)
            ));

            let mut txn = owner.begin(&txn_cx).expect("owner begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.set_vertex_property(VId(1), PROP, Some(int(5)));
            batch.create_vertex(VId(2), vec![LabelId(3)], vec![]);
            txn.write(&mut owner, batch).expect("owner stages");
            let bind = fgdb::RelationBind::new().with_label("Person", LabelId(3));
            let query = txn
                .prepare_gql_query("MATCH (n:Person) RETURN n", &bind)
                .expect("prepares query");
            let artifact = txn
                .execute_prepared_query_overlay_artifact(&owner, &query)
                .expect("owner issues artifact");
            let bytes = artifact.to_bytes();
            assert_eq!(artifact.rows(), &[VId(1), VId(2)]);
            let mut cursor = txn
                .open_untrusted_prepared_query_overlay_artifact_cursor(&owner, &query, &bytes)
                .expect("owner opens an audited cursor");
            assert_eq!(cursor.next_page(1).expect("first page").rows(), &[VId(1)]);
            let checkpoint = cursor.checkpoint_token().expect("row remains").to_bytes();
            let mut resumed = txn
                .resume_untrusted_prepared_query_overlay_artifact_cursor(
                    &owner,
                    &query,
                    &bytes,
                    &checkpoint,
                )
                .expect("owner resumes the same checkpoint");
            assert_eq!(resumed.next_page(1).expect("last page").rows(), &[VId(2)]);
            let before = (
                format!("{txn:?}"),
                txn.staged_effect_digest().expect("staged digest"),
            );
            let owner_rows = owner.vertices().expect("owner rows");
            let foreign_rows = foreign.vertices().expect("foreign rows");
            let owner_frontier = owner.frontier().expect("owner frontier");
            let foreign_frontier = foreign.frontier().expect("foreign frontier");
            wrong_owner(txn.vertex(&foreign, VId(1)));
            wrong_owner(txn.vertices(&foreign));
            wrong_owner(txn.edge(&foreign, EId(1)));
            wrong_owner(txn.edges(&foreign));
            wrong_owner(txn.neighbours(&foreign, VId(1), KNOWS));
            wrong_owner(txn.in_neighbours(&foreign, VId(1), KNOWS));
            wrong_owner(txn.execute_gql(&foreign, "MATCH (n:Person) RETURN n", &bind));
            wrong_owner(txn.execute_prepared_gql(&foreign, query.plan()));
            wrong_owner(txn.execute_prepared_query(&foreign, &query));
            wrong_owner(txn.execute_prepared_query_certified(&foreign, &query));
            wrong_owner(txn.execute_prepared_query_overlay_artifact(&foreign, &query));
            assert!(matches!(
                txn.execute_prepared_query_budgeted(
                    &foreign,
                    &query,
                    fgdb_gql::GqlExecutionBudget::new(10, 10)
                ),
                Err(fgdb_gql::BudgetedGqlError::Execution(
                    WriteTxnError::WrongDatabase
                ))
            ));
            assert!(matches!(
                txn.audit_prepared_query_overlay_artifact(&foreign, &query, &bytes),
                Err(fgdb_gql::GqlEvidenceAuditError::Execution(
                    WriteTxnError::WrongDatabase
                ))
            ));
            assert!(matches!(
                txn.open_untrusted_prepared_query_overlay_artifact_cursor(&foreign, &query, &bytes),
                Err(fgdb_gql::GqlEvidenceLimitedAuditError::Audit(
                    fgdb_gql::GqlEvidenceAuditError::Execution(WriteTxnError::WrongDatabase)
                ))
            ));
            assert!(matches!(
                txn.resume_untrusted_prepared_query_overlay_artifact_cursor(
                    &foreign,
                    &query,
                    &bytes,
                    &checkpoint,
                ),
                Err(fgdb_gql::GqlEvidencePageAuditError::Audit(
                    fgdb_gql::GqlEvidenceLimitedAuditError::Audit(
                        fgdb_gql::GqlEvidenceAuditError::Execution(WriteTxnError::WrongDatabase)
                    )
                ))
            ));
            let mut rejected = WriteBatch::new(KNOWS);
            rejected.set_vertex_property(VId(1), PROP, Some(int(99)));
            wrong_owner(txn.write(&mut foreign, rejected));
            wrong_owner(txn.commit(&mut foreign, &commit).await);
            wrong_owner(txn.commit_with_crash(&mut foreign, &commit, None).await);
            assert_eq!(
                before,
                (
                    format!("{txn:?}"),
                    txn.staged_effect_digest().expect("unchanged staged digest")
                )
            );
            assert_eq!(owner.vertices().expect("owner unchanged"), owner_rows);
            assert_eq!(foreign.vertices().expect("foreign unchanged"), foreign_rows);
            assert_eq!(
                owner.frontier().expect("owner frontier unchanged"),
                owner_frontier
            );
            assert_eq!(
                foreign.frontier().expect("foreign frontier unchanged"),
                foreign_frontier
            );
            assert_eq!(txn_cx.outstanding_obligations(), 1);

            // Moving the Rust value and publishing a disjoint row retain the
            // opened writer identity while the transaction keeps its old basis.
            let mut owner = Box::new(owner);
            let mut advancing = WriteBatch::new(KNOWS);
            advancing.create_vertex(VId(7), vec![], vec![]);
            owner
                .write(&commit, advancing)
                .await
                .expect("owner advances");
            assert_eq!(
                txn.vertex(&owner, VId(1))
                    .expect("pinned owner read")
                    .expect("row")
                    .props,
                vec![(PROP, int(5))]
            );
            txn.commit(&mut owner, &commit)
                .await
                .expect("actual owner still commits");
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            owner
                .commit_prepared(&commit, prepared_write.clone())
                .await
                .expect("original prepared owner still commits");
            assert!(owner.vertex(VId(8)).expect("prepared row").is_some());
            assert!(foreign.vertex(VId(8)).expect("foreign row").is_none());
            assert_eq!(
                foreign.frontier().expect("foreign stays unchanged"),
                foreign_frontier
            );
        }
    });
}

#[test]
fn reopened_handle_refuses_old_transactions_and_prepared_clones() {
    under_lab(0x7a12, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("reopened-owner");
        let mut owner = seeded(&commit, &dir).await;
        let mut txn = owner.begin(&txn_cx).expect("begins");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(2), vec![], vec![]);
        txn.write(&mut owner, batch.clone()).expect("stages");
        let prepared = owner.prepare_write(batch).expect("prepares");
        let frontier = owner.frontier().expect("frontier");
        drop(owner);
        let mut reopened = Database::open(&commit, &dir, keys())
            .await
            .expect("same database reopens");
        assert_eq!(reopened.frontier().expect("same frontier"), frontier);
        wrong_owner(txn.vertices(&reopened));
        wrong_owner(txn.commit(&mut reopened, &commit).await);
        assert!(matches!(
            reopened.commit_prepared(&commit, prepared.clone()).await,
            Err(WriteError::ForeignPreparedWrite)
        ));
        assert_eq!(txn_cx.outstanding_obligations(), 1);
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
        assert_eq!(
            reopened.frontier().expect("refusal consumed no sequence"),
            frontier
        );
        assert!(reopened.vertex(VId(2)).expect("old write absent").is_none());
        let mut fresh = reopened.begin(&txn_cx).expect("new owner begins");
        let mut batch = WriteBatch::new(KNOWS);
        batch.create_vertex(VId(2), vec![], vec![]);
        fresh.write(&mut reopened, batch).expect("new owner stages");
        fresh
            .commit(&mut reopened, &commit)
            .await
            .expect("new owner commits");
        assert!(reopened.vertex(VId(2)).expect("new owner row").is_some());
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
}

#[test]
fn authoritative_recovery_replaces_ownership_but_healthy_recovery_keeps_it() {
    under_lab(0x7a14, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut owner = seeded(&commit, &scratch("recovery-owner")).await;
        let mut txn = owner.begin(&txn_cx).expect("begin");
        let mut staged = WriteBatch::new(KNOWS);
        staged.create_vertex(VId(2), vec![], vec![]);
        txn.write(&mut owner, staged.clone()).expect("stage");
        let prepared = owner.prepare_write(staged).expect("prepare");
        let mut owner = owner
            .recover_authoritatively(&commit)
            .await
            .expect("healthy no-op recovery");
        assert!(
            txn.vertex(&owner, VId(2))
                .expect("same owner overlay")
                .is_some()
        );
        let mut advancing = WriteBatch::new(KNOWS);
        advancing.create_vertex(VId(3), vec![], vec![]);
        assert!(matches!(
            owner
                .write_with_publication_failure(
                    &commit,
                    advancing,
                    fgdb::DerivedPublicationStage::SealPartition
                )
                .await,
            Err(WriteError::CommittedNeedsRecovery { .. })
        ));
        let mut recovered = owner
            .recover_authoritatively(&commit)
            .await
            .expect("authoritative reopen");
        wrong_owner(txn.vertex(&recovered, VId(1)));
        wrong_owner(txn.commit(&mut recovered, &commit).await);
        assert!(matches!(
            recovered.commit_prepared(&commit, prepared).await,
            Err(WriteError::ForeignPreparedWrite)
        ));
        assert!(
            recovered
                .vertex(VId(3))
                .expect("durable write recovered")
                .is_some()
        );
        assert!(
            recovered
                .vertex(VId(2))
                .expect("old staged row absent")
                .is_none()
        );
        txn.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
}

#[test]
fn foreign_prepared_refusal_preserves_the_receivers_fcw_history() {
    under_lab(0x7a15, |contexts| async move {
        let commit = contexts.commit();
        let mut owner = seeded(&commit, &scratch("fcw-owner")).await;
        let mut receiver = seeded(&commit, &scratch("fcw-receiver")).await;
        let mut winner = WriteBatch::new(KNOWS);
        winner.set_vertex_property(VId(1), PROP, Some(int(1)));
        let winner = receiver.prepare_write(winner).expect("winner prepares");
        let mut loser = WriteBatch::new(KNOWS);
        loser.set_vertex_property(VId(1), PROP, Some(int(2)));
        let loser = receiver
            .prepare_write(loser)
            .expect("loser prepares at same basis");
        receiver
            .commit_prepared(&commit, winner)
            .await
            .expect("winner commits");

        let mut advance = WriteBatch::new(KNOWS);
        advance.create_vertex(VId(7), vec![], vec![]);
        owner
            .write(&commit, advance)
            .await
            .expect("owner reaches receiver frontier");
        let mut foreign = WriteBatch::new(KNOWS);
        foreign.create_vertex(VId(8), vec![], vec![]);
        let foreign = owner.prepare_write(foreign).expect("foreign prepares");
        assert_eq!(
            foreign.basis(),
            receiver.frontier().expect("receiver frontier")
        );
        // A guard placed after the basis-current validator reset would appear
        // to refuse correctly but erase the winner needed to reject the loser.
        assert!(matches!(
            receiver.commit_prepared(&commit, foreign).await,
            Err(WriteError::ForeignPreparedWrite)
        ));
        assert!(matches!(
            receiver.commit_prepared(&commit, loser).await,
            Err(WriteError::FirstCommitterWins { .. })
        ));
        assert_eq!(
            receiver
                .vertex(VId(1))
                .expect("receiver row")
                .expect("present")
                .props,
            vec![(PROP, int(1))]
        );
        assert!(
            receiver
                .vertex(VId(8))
                .expect("foreign row absent")
                .is_none()
        );
    });
}

/// Two txns begun against one basis, overlapping property updates on
/// `VId(1)`: the first commit wins, the second receives the typed
/// `WriteError::FirstCommitterWins` abort, and a cold reopen serves only the
/// winner. The obligation ledger returns to baseline after both outcomes —
/// the loser's pin is released by its failed commit, not leaked.
#[test]
fn overlapping_txns_first_commit_wins_second_aborts_typed() {
    under_lab(0x7a_01, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("overlap");
        {
            let mut db = seeded(&commit, &dir).await;
            let baseline = txn_cx.outstanding_obligations();

            let mut txn_first = db.begin(&txn_cx).expect("first txn begins");
            let mut txn_second = db
                .begin(&txn_cx)
                .expect("second txn begins at the same basis");

            let mut winner = WriteBatch::new(KNOWS);
            winner.set_vertex_property(VId(1), PROP, Some(int(1)));
            txn_first
                .write(&mut db, winner)
                .expect("winner stages its batch");
            let mut loser = WriteBatch::new(KNOWS);
            loser.set_vertex_property(VId(1), PROP, Some(int(2)));
            txn_second
                .write(&mut db, loser)
                .expect("loser stages its batch");

            txn_first
                .commit(&mut db, &commit)
                .await
                .expect("first committer wins");
            let err = txn_second
                .commit(&mut db, &commit)
                .await
                .expect_err("the overlapping second txn must lose");
            assert!(
                matches!(
                    err,
                    WriteTxnError::Write(WriteError::FirstCommitterWins { .. })
                ),
                "the loser must be the typed FCW arm, got {err:?}"
            );
            let rendered = format!("{err:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-01"),
                "the abort must name the FCW law: {rendered}"
            );
            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline,
                "both pins are released: the winner's by commit, the loser's \
                 by its failed commit — a leaked pin here outlives its txn"
            );
        }

        // NOTHING crosses this line except the path and the keys.
        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(1))],
            "only the winning txn's property survives the reopen"
        );
    });
}

/// begin → write → abort: the obligation ledger returns to its baseline,
/// nothing of the aborted write is durable, and the handle keeps working —
/// an autocommit write after the abort commits and survives reopen.
#[test]
fn abort_releases_the_pin_and_leaves_nothing_durable() {
    under_lab(0x7a_02, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort");
        {
            let mut db = seeded(&commit, &dir).await;
            let baseline = txn_cx.outstanding_obligations();
            let frontier_before = db.frontier().expect("healthy frontier");

            let mut txn = db.begin(&txn_cx).expect("txn begins");
            let mut batch = WriteBatch::new(KNOWS);
            batch.set_vertex_property(VId(1), PROP, Some(int(99)));
            txn.write(&mut db, batch).expect("stages its batch");
            txn.abort();

            assert_eq!(
                txn_cx.outstanding_obligations(),
                baseline,
                "abort must release the pin"
            );
            assert_eq!(
                db.frontier().expect("healthy frontier"),
                frontier_before,
                "an aborted txn consumes no sequence"
            );
            assert_eq!(
                db.vertex(VId(1)).expect("reads").expect("row").props,
                vec![(PROP, int(0))],
                "the aborted write is invisible to the live fold"
            );

            let mut after = WriteBatch::new(KNOWS);
            after.set_vertex_property(VId(1), PROP, Some(int(5)));
            db.write(&commit, after)
                .await
                .expect("autocommit after abort works");
        }

        let db = Database::open(&commit, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            db.vertex(VId(1)).expect("reads").expect("row").props,
            vec![(PROP, int(5))],
            "reopen holds the seed and the post-abort write; the aborted \
             txn's 99 is nowhere"
        );
    });
}

/// THE PLANTED NEGATIVE, live: while a txn is open — after `begin`, before
/// commit or abort — the `TxnCx` obligation ledger is ABOVE its baseline. A
/// `begin` that skipped `TxnCx::pin_snapshot` (storing a bare basis number
/// instead) passes every other test in this file and fails this one, because
/// no bookkeeping it invents can raise the runtime's own ledger.
#[test]
fn an_open_txn_holds_a_live_pin_obligation() {
    under_lab(0x7a_03, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("live-pin");
        let mut db = seeded(&commit, &dir).await;

        let baseline = txn_cx.outstanding_obligations();
        let txn = db.begin(&txn_cx).expect("txn begins");
        assert!(
            txn_cx.outstanding_obligations() > baseline,
            "begin must ACQUIRE the pin_snapshot obligation: ledger stayed at \
             {baseline}, so the \"pinned\" snapshot is fiction"
        );
        let held: &WriteTxn = &txn;
        let _ = held; // the obligation belongs to this txn value, still alive here
        txn.abort();
        assert_eq!(
            txn_cx.outstanding_obligations(),
            baseline,
            "the ledger returns to baseline once the txn ends"
        );
    });
}
