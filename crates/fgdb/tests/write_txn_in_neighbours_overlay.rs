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
        "fgdb-write-txn-in-neighbours-overlay-{}-{name}",
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

async fn seeded_vertices(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut database = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![], vec![]);
    seed.create_vertex(VId(2), vec![], vec![]);
    database.write(cx, seed).await.expect("seed vertices commit");
    database
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

#[test]
fn staged_edge_appears_only_in_transaction_in_neighbours_and_abort_discards_it() {
    under_lab(0x8e_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-added-incoming-edge");
        {
            let mut database = seeded_vertices(&commit_cx, &dir).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            let mut add = WriteBatch::new(R);
            add.add_edge(EDGE, VId(1), VId(2), vec![]);
            transaction
                .write(&mut database, add)
                .expect("stage incoming edge");

            assert_eq!(
                transaction
                    .in_neighbours(&database, VId(2), R)
                    .expect("overlay incoming read succeeds"),
                vec![VId(1)]
            );
            assert!(
                database
                    .in_neighbours(VId(2), R)
                    .expect("base incoming read succeeds")
                    .is_empty(),
                "base view cannot see the staged incoming edge"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert!(reopened.in_neighbours(VId(2), R).expect("reopen incoming").is_empty());
        assert!(reopened.edge(EDGE).expect("reopen staged edge").is_none());
    });
}

#[test]
fn staged_edge_deletion_empties_in_neighbours_and_commits_once() {
    under_lab(0x8e_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-deleted-incoming-edge");
        let committed;
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            let mut delete = WriteBatch::new(R);
            delete.delete_edge(EDGE);
            transaction
                .write(&mut database, delete)
                .expect("stage incoming edge deletion");

            assert!(
                transaction
                    .in_neighbours(&database, VId(2), R)
                    .expect("overlay incoming")
                    .is_empty()
            );
            assert_eq!(
                database.in_neighbours(VId(2), R).expect("base incoming"),
                vec![VId(1)]
            );
            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit staged deletion");
            assert_eq!(committed.0, before.0 + 1, "one transaction consumes one sequence");
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(reopened.frontier().expect("healthy reopened frontier"), committed);
        assert!(reopened.in_neighbours(VId(2), R).expect("reopen incoming").is_empty());
        assert!(reopened.edge(EDGE).expect("reopen deleted edge").is_none());
    });
}

#[test]
fn concurrent_deletion_of_observed_incoming_edge_aborts_reader_with_read_01() {
    under_lab(0x8e_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("incoming-read-conflict");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let mut reader = database.begin(&txn_cx).expect("begin incoming reader");
            let mut deleter = database.begin(&txn_cx).expect("begin edge deleter");
            assert_eq!(
                reader
                    .in_neighbours(&database, VId(2), R)
                    .expect("transactional incoming read succeeds"),
                vec![VId(1)]
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            let mut delete = WriteBatch::new(R);
            delete.delete_edge(EDGE);
            deleter
                .write(&mut database, delete)
                .expect("deleter stages observed incoming edge deletion");
            deleter
                .commit(&mut database, &commit_cx)
                .await
                .expect("edge deleter commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "incoming read conflict must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "incoming read conflict must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert!(reopened.in_neighbours(VId(2), R).expect("reopen incoming").is_empty());
        assert!(reopened.edge(EDGE).expect("reopen deleted edge").is_none());
        assert!(
            reopened.vertex(VId(3)).expect("reopen vertex").is_none(),
            "READ-01 abort leaves no disjoint write residue"
        );
    });
}
