use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_types::VId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-write-txn-vertices-overlay-{}-{name}",
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

async fn seeded_vertex(cx: &fgdb_types::context::CommitCx, dir: &PathBuf) -> Database {
    let mut database = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    seed.create_vertex(VId(1), vec![], vec![]);
    database.write(cx, seed).await.expect("seed vertex commits");
    database
}

fn vertex_ids(vertices: &[fgdb::VertexRow]) -> Vec<VId> {
    vertices.iter().map(|vertex| vertex.vid).collect()
}

#[test]
fn staged_creation_appears_only_in_transaction_vertices_and_abort_discards_it() {
    under_lab(0x8c_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-created-vertex");
        {
            let mut database = seeded_vertex(&commit_cx, &dir).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            let mut create = WriteBatch::new(R);
            create.create_vertex(VId(2), vec![], vec![]);
            transaction
                .write(&mut database, create)
                .expect("stage second vertex");

            assert_eq!(
                vertex_ids(&transaction.vertices(&database).expect("overlay vertices")),
                vec![VId(1), VId(2)]
            );
            assert_eq!(
                vertex_ids(&database.vertices().expect("base vertices")),
                vec![VId(1)],
                "base view cannot see the staged vertex"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(
                database.frontier().expect("abort leaves handle healthy"),
                frontier_before
            );
        }

        let reopened = Database::open(&commit_cx, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            vertex_ids(&reopened.vertices().expect("reopen vertices")),
            vec![VId(1)]
        );
        assert!(
            reopened
                .vertex(VId(2))
                .expect("reopen staged vertex")
                .is_none()
        );
    });
}

#[test]
fn staged_deletion_empties_transaction_vertices_and_commits_once() {
    under_lab(0x8c_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-deleted-vertex");
        let committed;
        {
            let mut database = seeded_vertex(&commit_cx, &dir).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            let mut delete = WriteBatch::new(R);
            delete.delete_vertex(VId(1));
            transaction
                .write(&mut database, delete)
                .expect("stage vertex deletion");

            assert!(
                transaction
                    .vertices(&database)
                    .expect("overlay vertices")
                    .is_empty()
            );
            assert_eq!(
                vertex_ids(&database.vertices().expect("base vertices")),
                vec![VId(1)]
            );
            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit staged deletion");
            assert_eq!(
                committed.0,
                before.0 + 1,
                "one transaction consumes one sequence"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys())
            .await
            .expect("reopens");
        assert_eq!(
            reopened.frontier().expect("healthy reopened frontier"),
            committed
        );
        assert!(reopened.vertices().expect("reopen vertices").is_empty());
        assert!(
            reopened
                .vertex(VId(1))
                .expect("reopen deleted vertex")
                .is_none()
        );
    });
}

#[test]
fn concurrent_deletion_of_vertex_observed_by_vertices_aborts_reader_with_read_01() {
    under_lab(0x8c_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("vertices-read-conflict");
        {
            let mut database = seeded_vertex(&commit_cx, &dir).await;
            let mut reader = database.begin(&txn_cx).expect("begin vertices reader");
            let mut deleter = database.begin(&txn_cx).expect("begin vertex deleter");
            assert_eq!(
                vertex_ids(
                    &reader
                        .vertices(&database)
                        .expect("transactional vertices read")
                ),
                vec![VId(1)]
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            let mut delete = WriteBatch::new(R);
            delete.delete_vertex(VId(1));
            deleter
                .write(&mut database, delete)
                .expect("deleter stages observed vertex deletion");
            deleter
                .commit(&mut database, &commit_cx)
                .await
                .expect("vertex deleter commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "vertices read conflict must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "vertices read conflict must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys())
            .await
            .expect("reopens");
        assert!(reopened.vertices().expect("reopen vertices").is_empty());
        assert!(
            reopened
                .vertex(VId(1))
                .expect("reopen deleted vertex")
                .is_none()
        );
        assert!(
            reopened.vertex(VId(3)).expect("reopen vertex").is_none(),
            "READ-01 abort leaves no disjoint write residue"
        );
    });
}
