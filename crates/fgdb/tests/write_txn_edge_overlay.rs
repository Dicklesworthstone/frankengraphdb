use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::RelationId;
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-write-txn-edge-overlay-{}-{name}",
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

async fn seeded_vertices(
    cx: &fgdb_types::context::CommitCx,
    dir: &PathBuf,
    vertices: &[VId],
) -> Database {
    let mut database = Database::create(cx, dir, keys()).await.expect("creates");
    let mut seed = WriteBatch::new(R);
    for vid in vertices {
        seed.create_vertex(*vid, vec![], vec![]);
    }
    database.write(cx, seed).await.expect("seed vertices commit");
    database
}

fn add_edge(eid: EId, source: VId, destination: VId) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.add_edge(eid, source, destination, vec![]);
    batch
}

#[test]
fn staged_edge_is_visible_only_through_the_transaction_and_abort_discards_it() {
    under_lab(0x80_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-private");
        {
            let mut database = seeded_vertices(&commit_cx, &dir, &[VId(1), VId(2)]).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, add_edge(EId(10), VId(1), VId(2)))
                .expect("stage R edge");

            assert_eq!(
                transaction
                    .neighbours(&database, VId(1), R)
                    .expect("overlay neighbours read succeeds"),
                vec![VId(2)]
            );
            assert!(
                transaction
                    .edge(&database, EId(10))
                    .expect("overlay edge read succeeds")
                    .is_some(),
                "transaction sees its staged edge"
            );
            assert!(
                database.neighbours(VId(1), R).expect("base neighbours read succeeds").is_empty(),
                "base view cannot see the private adjacency"
            );
            assert!(
                database.edge(EId(10)).expect("base edge read succeeds").is_none(),
                "base view cannot see the private edge row"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
            assert!(database.neighbours(VId(1), R).expect("reads after abort").is_empty());
            assert!(database.edge(EId(10)).expect("reads after abort").is_none());
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert!(reopened.neighbours(VId(1), R).expect("reopen neighbours").is_empty());
        assert!(reopened.edge(EId(10)).expect("reopen edge").is_none());
    });
}

#[test]
fn committed_staged_edge_consumes_one_sequence_and_survives_reopen() {
    under_lab(0x80_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-one-sequence");
        let committed;
        {
            let mut database = seeded_vertices(&commit_cx, &dir, &[VId(1), VId(2)]).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, add_edge(EId(10), VId(1), VId(2)))
                .expect("stage R edge");
            assert_eq!(
                transaction
                    .neighbours(&database, VId(1), R)
                    .expect("overlay neighbours read succeeds"),
                vec![VId(2)]
            );
            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit staged edge");
            assert_eq!(committed.0, before.0 + 1, "one transaction consumes one sequence");
            assert_eq!(database.frontier().expect("healthy committed frontier"), committed);
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(reopened.frontier().expect("healthy reopened frontier"), committed);
        assert_eq!(
            reopened.neighbours(VId(1), R).expect("reopen neighbours"),
            vec![VId(2)]
        );
        assert!(reopened.edge(EId(10)).expect("reopen edge").is_some());
    });
}

#[test]
fn concurrent_edge_from_observed_source_aborts_reader_with_read_01() {
    under_lab(0x80_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("adjacency-read-conflict");
        {
            let mut database =
                seeded_vertices(&commit_cx, &dir, &[VId(1), VId(2)]).await;
            let mut reader = database.begin(&txn_cx).expect("begin adjacency reader");
            let mut writer = database.begin(&txn_cx).expect("begin edge writer");

            assert!(
                reader
                    .neighbours(&database, VId(1), R)
                    .expect("transactional neighbours read succeeds")
                    .is_empty()
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            writer
                .write(&mut database, add_edge(EId(11), VId(1), VId(2)))
                .expect("writer stages edge from observed source");
            writer
                .commit(&mut database, &commit_cx)
                .await
                .expect("edge writer commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "adjacency conflict must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "adjacency abort must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            reopened.neighbours(VId(1), R).expect("reopen neighbours"),
            vec![VId(2)]
        );
        assert!(reopened.edge(EId(11)).expect("reopen edge").is_some());
        assert!(
            reopened.vertex(VId(3)).expect("reopen vertex").is_none(),
            "READ-01 abort leaves no disjoint write residue"
        );
    });
}
