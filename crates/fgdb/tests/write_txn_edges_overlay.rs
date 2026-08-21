use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const PROPERTY: PropertyKeyId = PropertyKeyId(7);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-write-txn-edges-overlay-{}-{name}",
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
    for vid in [VId(1), VId(2), VId(3)] {
        seed.create_vertex(vid, vec![], vec![]);
    }
    seed.add_edge(EId(10), VId(1), VId(2), vec![]);
    database.write(cx, seed).await.expect("seed edge commits");
    database
}

fn edge_ids(edges: &[fgdb::EdgeRecord]) -> Vec<EId> {
    edges.iter().map(|edge| edge.entry.eid).collect()
}

#[test]
fn staged_addition_appears_only_in_transaction_edges_and_abort_discards_it() {
    under_lab(0x8a_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-added-edge");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            let mut add = WriteBatch::new(R);
            add.add_edge(EId(11), VId(1), VId(3), vec![]);
            transaction
                .write(&mut database, add)
                .expect("stage second edge");

            assert_eq!(
                edge_ids(&transaction.edges(&database).expect("overlay edges read succeeds")),
                vec![EId(10), EId(11)]
            );
            assert_eq!(
                edge_ids(&database.edges().expect("base edges read succeeds")),
                vec![EId(10)],
                "base view cannot see the staged edge"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(database.frontier().expect("abort leaves handle healthy"), frontier_before);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(edge_ids(&reopened.edges().expect("reopen edges")), vec![EId(10)]);
        assert!(reopened.edge(EId(11)).expect("reopen staged edge").is_none());
    });
}

#[test]
fn staged_deletion_empties_transaction_edges_and_commits_once() {
    under_lab(0x8a_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-deleted-edge");
        let committed;
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            let mut delete = WriteBatch::new(R);
            delete.delete_edge(EId(10));
            transaction
                .write(&mut database, delete)
                .expect("stage edge deletion");

            assert!(transaction.edges(&database).expect("overlay edges").is_empty());
            assert_eq!(edge_ids(&database.edges().expect("base edges")), vec![EId(10)]);
            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit staged deletion");
            assert_eq!(committed.0, before.0 + 1, "one transaction consumes one sequence");
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(reopened.frontier().expect("healthy reopened frontier"), committed);
        assert!(reopened.edges().expect("reopen edges").is_empty());
        assert!(reopened.edge(EId(10)).expect("reopen deleted edge").is_none());
    });
}

#[test]
fn concurrent_mutation_of_edge_observed_by_edges_aborts_reader_with_read_01() {
    under_lab(0x8a_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("edges-read-conflict");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let mut reader = database.begin(&txn_cx).expect("begin edges reader");
            let mut writer = database.begin(&txn_cx).expect("begin edge writer");
            assert_eq!(
                edge_ids(&reader.edges(&database).expect("transactional edges read succeeds")),
                vec![EId(10)]
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(4), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            let mut property = WriteBatch::new(R);
            property.set_edge_property(
                EId(10),
                PROPERTY,
                Some(CanonicalScalar::Int(33)),
            );
            writer
                .write(&mut database, property)
                .expect("writer stages observed edge mutation");
            writer
                .commit(&mut database, &commit_cx)
                .await
                .expect("edge writer commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "edges read conflict must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "edges read conflict must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys()).await.expect("reopens");
        assert_eq!(
            reopened
                .edge(EId(10))
                .expect("reopen edge")
                .expect("edge remains live")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(33))]
        );
        assert!(
            reopened.vertex(VId(4)).expect("reopen vertex").is_none(),
            "READ-01 abort leaves no disjoint write residue"
        );
    });
}
