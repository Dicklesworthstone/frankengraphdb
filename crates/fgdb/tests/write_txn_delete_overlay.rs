use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const EDGE: EId = EId(10);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-write-txn-delete-overlay-{}-{name}",
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
    seed.add_edge(EDGE, VId(1), VId(2), vec![]);
    database.write(cx, seed).await.expect("seed edge commits");
    database
}

fn delete_edge() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.delete_edge(EDGE);
    batch
}

#[test]
fn staged_delete_is_private_and_abort_restores_the_durable_edge() {
    under_lab(0x82_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-private-delete");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, delete_edge())
                .expect("stage edge deletion");

            assert!(
                transaction
                    .edge(&database, EDGE)
                    .expect("overlay edge read succeeds")
                    .is_none(),
                "transaction overlay hides the deleted edge"
            );
            assert!(
                transaction
                    .neighbours(&database, VId(1), R)
                    .expect("overlay neighbours read succeeds")
                    .is_empty(),
                "transaction overlay omits the deleted destination"
            );
            assert!(
                database.edge(EDGE).expect("base edge read succeeds").is_some(),
                "base view still contains the durable edge"
            );
            assert_eq!(
                database.neighbours(VId(1), R).expect("base neighbours read succeeds"),
                vec![VId(2)]
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
            assert!(database.edge(EDGE).expect("edge after abort").is_some());
            assert_eq!(
                database.neighbours(VId(1), R).expect("neighbours after abort"),
                vec![VId(2)]
            );
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert!(reopened.edge(EDGE).expect("reopen edge").is_some());
        assert_eq!(
            reopened.neighbours(VId(1), R).expect("reopen neighbours"),
            vec![VId(2)]
        );
    });
}

#[test]
fn committed_delete_consumes_one_sequence_and_survives_reopen() {
    under_lab(0x82_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-delete");
        let committed;
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, delete_edge())
                .expect("stage edge deletion");
            assert!(transaction.edge(&database, EDGE).expect("overlay edge").is_none());
            assert!(
                transaction
                    .neighbours(&database, VId(1), R)
                    .expect("overlay neighbours")
                    .is_empty()
            );

            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit staged deletion");
            assert_eq!(committed.0, before.0 + 1, "one transaction consumes one sequence");
            assert_eq!(database.frontier().expect("healthy committed frontier"), committed);
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(reopened.frontier().expect("healthy reopened frontier"), committed);
        assert!(reopened.edge(EDGE).expect("reopen edge").is_none());
        assert!(reopened.neighbours(VId(1), R).expect("reopen neighbours").is_empty());
    });
}

#[test]
fn concurrent_delete_of_observed_adjacency_aborts_reader_with_read_01() {
    under_lab(0x82_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("delete-read-conflict");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let mut reader = database.begin(&txn_cx).expect("begin adjacency reader");
            let mut deleter = database.begin(&txn_cx).expect("begin edge deleter");

            assert_eq!(
                reader
                    .neighbours(&database, VId(1), R)
                    .expect("transactional neighbours read succeeds"),
                vec![VId(2)]
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            deleter
                .write(&mut database, delete_edge())
                .expect("deleter stages observed edge deletion");
            deleter
                .commit(&mut database, &commit_cx)
                .await
                .expect("edge deleter commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "adjacency deletion must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "delete conflict must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert!(reopened.edge(EDGE).expect("reopen edge").is_none());
        assert!(reopened.neighbours(VId(1), R).expect("reopen neighbours").is_empty());
        assert!(
            reopened.vertex(VId(3)).expect("reopen vertex").is_none(),
            "READ-01 abort leaves no disjoint write residue"
        );
    });
}
