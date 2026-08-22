use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, WriteBatch, WriteTxnError};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_types::context::PurposeContexts;
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{CanonicalScalar, EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const EDGE: EId = EId(10);
const PROPERTY: PropertyKeyId = PropertyKeyId(7);
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "fgdb-write-txn-edge-props-{}-{name}",
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

fn set_edge_property(value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_edge_property(EDGE, PROPERTY, Some(CanonicalScalar::Int(value)));
    batch
}

#[test]
fn staged_edge_property_is_private_and_abort_discards_it() {
    under_lab(0x84_01, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("abort-private-property");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let frontier_before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, set_edge_property(11))
                .expect("stage edge property");

            assert_eq!(
                transaction
                    .edge(&database, EDGE)
                    .expect("overlay edge read succeeds")
                    .expect("overlay edge remains live")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(11))]
            );
            assert!(
                database
                    .edge(EDGE)
                    .expect("base edge read succeeds")
                    .expect("base edge remains live")
                    .props
                    .is_empty(),
                "base view cannot see the private property"
            );

            transaction.abort();
            assert_eq!(txn_cx.outstanding_obligations(), 0);
            assert_eq!(
                database.frontier().expect("abort leaves handle healthy"),
                frontier_before
            );
            assert!(
                database
                    .edge(EDGE)
                    .expect("edge after abort")
                    .expect("edge survives abort")
                    .props
                    .is_empty()
            );
        }

        let reopened = Database::open(&commit_cx, &dir, keys())
            .await
            .expect("reopens");
        assert!(
            reopened
                .edge(EDGE)
                .expect("reopen edge")
                .expect("edge survives reopen")
                .props
                .is_empty(),
            "aborted property is not durable"
        );
    });
}

#[test]
fn committed_edge_property_consumes_one_sequence_and_survives_reopen() {
    under_lab(0x84_02, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("commit-property");
        let committed;
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let before = database.frontier().expect("healthy seed frontier");
            let mut transaction = database.begin(&txn_cx).expect("begin transaction");
            transaction
                .write(&mut database, set_edge_property(22))
                .expect("stage edge property");
            assert_eq!(
                transaction
                    .edge(&database, EDGE)
                    .expect("overlay edge")
                    .expect("overlay edge remains live")
                    .props,
                vec![(PROPERTY, CanonicalScalar::Int(22))]
            );

            committed = transaction
                .commit(&mut database, &commit_cx)
                .await
                .expect("commit staged property");
            assert_eq!(
                committed.0,
                before.0 + 1,
                "one transaction consumes one sequence"
            );
            assert_eq!(
                database.frontier().expect("healthy committed frontier"),
                committed
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
        assert_eq!(
            reopened
                .edge(EDGE)
                .expect("reopen edge")
                .expect("edge remains live")
                .props,
            vec![(PROPERTY, CanonicalScalar::Int(22))]
        );
    });
}

#[test]
fn concurrent_property_change_of_observed_edge_aborts_reader_with_read_01() {
    under_lab(0x84_03, |contexts| async move {
        let commit_cx = contexts.commit();
        let txn_cx = contexts.txn();
        let dir = scratch("property-read-conflict");
        {
            let mut database = seeded_edge(&commit_cx, &dir).await;
            let mut reader = database.begin(&txn_cx).expect("begin edge reader");
            let mut writer = database.begin(&txn_cx).expect("begin property writer");

            assert!(
                reader
                    .edge(&database, EDGE)
                    .expect("transactional edge read succeeds")
                    .expect("observed edge exists")
                    .props
                    .is_empty()
            );
            let mut disjoint = WriteBatch::new(R);
            disjoint.create_vertex(VId(3), vec![], vec![]);
            reader
                .write(&mut database, disjoint)
                .expect("reader stages disjoint vertex");
            writer
                .write(&mut database, set_edge_property(33))
                .expect("writer stages observed edge property");
            writer
                .commit(&mut database, &commit_cx)
                .await
                .expect("property writer commits first");

            let refusal = reader.commit(&mut database, &commit_cx).await;
            assert!(
                matches!(&refusal, Err(WriteTxnError::Write(_))),
                "edge property conflict must be a typed Write abort: {refusal:?}"
            );
            let rendered = format!("{refusal:?}");
            assert!(
                rendered.contains("FG-LAW-FCW-READ-01"),
                "edge property conflict must name READ-01: {rendered}"
            );
            assert_eq!(txn_cx.outstanding_obligations(), 0);
        }

        let reopened = Database::open(&commit_cx, &dir, keys())
            .await
            .expect("reopens");
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
