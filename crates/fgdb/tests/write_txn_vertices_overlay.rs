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
fn bulk_vertices_preserve_staged_labels_properties_and_historical_birth_rows() {
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::CanonicalScalar;

    under_lab(0x8c04, |contexts| async move {
        let commit = contexts.commit();
        let txn_cx = contexts.txn();
        let mut database = seeded_vertex(&commit, &scratch("batch-basis-parity")).await;
        let mut second = WriteBatch::new(R);
        second.create_vertex(VId(2), vec![], vec![]);
        database
            .write(&commit, second)
            .await
            .expect("second vertex commits");
        let mut transaction = database.begin(&txn_cx).expect("transaction begins");
        let basis = transaction.basis();
        let key = PropertyKeyId(7);
        let mut staged = WriteBatch::new(R);
        staged.ensure_vertex(VId(1), vec![LabelId(99)], vec![]);
        staged.set_vertex_label(VId(1), LabelId(8), true);
        staged.set_vertex_label(VId(1), LabelId(7), true);
        staged.set_vertex_label(VId(1), LabelId(7), false);
        staged.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(3)));
        staged.compare_and_set_vertex_property(
            VId(1),
            key,
            Some(CanonicalScalar::Int(3)),
            CanonicalScalar::Int(5),
            fgdb::WriteMismatchPolicy::AbortWrite,
        );
        staged.compare_and_set_vertex_property(
            VId(1),
            key,
            Some(CanonicalScalar::Int(-1)),
            CanonicalScalar::Int(99),
            fgdb::WriteMismatchPolicy::NoOp,
        );
        staged.delete_vertex(VId(2));
        staged.create_vertex(
            VId(3),
            vec![LabelId(9), LabelId(4)],
            vec![(key, CanonicalScalar::Int(8))],
        );
        transaction
            .write(&mut database, staged)
            .expect("ordered effects stage");
        let before = transaction.vertices(&database).expect("bulk overlay reads");
        assert_eq!(vertex_ids(&before), vec![VId(1), VId(3)]);
        assert_eq!(before[0].labels, vec![LabelId(8)]);
        assert_eq!(before[0].props, vec![(key, CanonicalScalar::Int(5))]);
        assert_eq!(before[1].labels, vec![LabelId(4), LabelId(9)]);
        assert_eq!(before[1].birth_ordinal, 9);
        assert_eq!(before[1].created_at, basis);
        let points: Vec<_> = [VId(1), VId(2), VId(3)]
            .into_iter()
            .filter_map(|vid| {
                transaction
                    .vertex(&database, vid)
                    .expect("point overlay reads")
            })
            .collect();
        assert_eq!(before, points);

        let mut advancing = WriteBatch::new(R);
        advancing.set_vertex_property(VId(1), key, Some(CanonicalScalar::Int(42)));
        advancing.create_vertex(VId(4), vec![], vec![]);
        let live = database
            .write(&commit, advancing)
            .await
            .expect("another writer advances");
        assert!(live > basis);
        let after = transaction
            .vertices(&database)
            .expect("bulk read keeps pinned basis");
        let mut expected = before;
        assert_eq!(expected[0].retired_at, None);
        expected[0].retired_at = Some(live);
        assert_eq!(after, expected, "only the known future retirement changes");
        let historical_points: Vec<_> = [VId(1), VId(2), VId(3), VId(4)]
            .into_iter()
            .filter_map(|vid| {
                transaction
                    .vertex(&database, vid)
                    .expect("historical point overlay")
            })
            .collect();
        assert_eq!(after, historical_points);
        transaction.abort();
        assert_eq!(txn_cx.outstanding_obligations(), 0);
    });
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
