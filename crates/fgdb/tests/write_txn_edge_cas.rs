use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteMismatchPolicy, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const EDGE: EId = EId(10);
const PROPERTY: PropertyKeyId = PropertyKeyId(7);
const OLD: i64 = 5;
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-write-txn-edge-cas-{}-{name}",
        std::process::id()
    ))
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

async fn seeded_edge(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut database = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    seed.add_edge(
        EDGE,
        VId(1),
        VId(2),
        vec![(PROPERTY, CanonicalScalar::Int(OLD))],
    );
    database.write(cx, seed).await.expect("seed edge commits");
    database
}

fn matching_cas(value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.compare_and_set_edge_property(
        EDGE,
        PROPERTY,
        Some(CanonicalScalar::Int(OLD)),
        CanonicalScalar::Int(value),
        WriteMismatchPolicy::AbortWrite,
    );
    batch
}

#[test]
fn staged_matching_edge_cas_is_private_and_abort_preserves_old_value() {
    under_lab(0x88_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-private-cas");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, matching_cas(11))
                .expect("stage matching edge CAS");

            assert_eq!(
                transaction
                    .edge(&database, EDGE)
                    .expect("overlay edge")
                    .expect("edge remains live")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(11))]
            );
            assert_eq!(
                database
                    .edge(EDGE)
                    .expect("base edge")
                    .expect("edge remains live")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(OLD))],
                "base view retains the old value"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            reopened
                .edge(EDGE)
                .expect("reopen edge")
                .expect("edge remains live")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(OLD))]
        );
    });
}

#[test]
fn matching_edge_cas_commits_once_and_mismatch_changes_no_edge_value() {
    under_lab(0x88_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-and-mismatch");
        let committed;
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin matching CAS");
            transaction
                .write(&mut database, matching_cas(22))
                .expect("stage matching edge CAS");
            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit matching CAS");
            assert_eq!(committed.0, before.0 + 1, "matching CAS consumes one sequence");

            let mut mismatch = database.begin(&txn_cx).expect("begin mismatch CAS");
            let mut mismatch_batch = WriteBatch::new(R);
            mismatch_batch.compare_and_set_edge_property(
                EDGE,
                PROPERTY,
                Some(CanonicalScalar::Int(OLD)),
                CanonicalScalar::Int(99),
                WriteMismatchPolicy::NoOp,
            );
            mismatch_batch.create_vertex(VId(3), vec![], vec![]);
            mismatch
                .write(&mut database, mismatch_batch)
                .expect("NoOp mismatch keeps sibling staged write");
            assert_eq!(
                mismatch
                    .edge(&database, EDGE)
                    .expect("mismatch overlay edge")
                    .expect("edge remains live")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(22))],
                "mismatching CAS leaves the overlay value unchanged"
            );
            assert_eq!(
                database
                    .edge(EDGE)
                    .expect("base edge")
                    .expect("edge remains live")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(22))],
                "mismatching CAS leaves durable value unchanged"
            );
            mismatch.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(reopened.frontier().expect("healthy reopened frontier"), committed);
        assert_eq!(
            reopened
                .edge(EDGE)
                .expect("reopen edge")
                .expect("edge remains live")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(22))]
        );
        assert!(reopened.vertex(VId(3)).expect("reopen sibling vertex").is_none());
    });
}

#[test]
fn concurrent_matching_cas_of_observed_edge_aborts_reader_with_read_01() {
    under_lab(0x88_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("cas-read-conflict");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let mut reader = database.begin(&txn_cx).expect("begin edge reader");
            let mut writer = database.begin(&txn_cx).expect("begin CAS writer");
            assert_eq!(
                reader
                    .edge(&database, EDGE)
                    .expect("transactional edge read succeeds")
                    .expect("observed edge exists")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(OLD))]
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            writer
                .write(&mut database, matching_cas(33))
                .expect("writer stages matching edge CAS");
            writer
                .commit(&mut database, &commit_cx)
                .await
                .expect("CAS writer commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "edge CAS conflict must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "edge CAS conflict must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            reopened
                .edge(EDGE)
                .expect("reopen edge")
                .expect("edge remains live")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(33))]
        );
        assert!(
            reopened.vertex(VId(3)).expect("reopen vertex").is_none(),
            "READ-01 abort leaves no disjoint write residue"
        );
    });
}
