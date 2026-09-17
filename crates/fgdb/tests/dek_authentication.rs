//! DEK authentication at open (fgdb-hkiy).
//!
//! `Database::open` must refuse a wrong data-encryption key on an EMPTY
//! database — the PLAIN root slot bound no DEK commitment, so the first
//! capsule decryption was the only key authentication. These tests pin the
//! create-time DEK binding: wrong DEK refused before any write, on empty and
//! non-empty databases alike, while the correct keys keep working.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, OpenError, WriteBatch};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};

const R: fgdb_delta_types::RelationId = fgdb_delta_types::RelationId(1);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa3; 32],
    )
}

/// Same object-identity key and namespace, different data-encryption key.
fn wrong_dek() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xa1; 32],
        DatabaseSecurityNamespaceId([0xa2; 32]),
        [0xa4; 32],
    )
}

fn scratch(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("fgdb-dek-auth-{}-{name}", std::process::id()))
}

#[test]
fn empty_database_open_with_wrong_dek_is_refused() {
    let ((), report) = run_async_under_lab(0x1a01_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = scratch("empty-wrong-dek");
        {
            let _db = Database::create(&cx, &path, keys()).await.unwrap();
        }
        let opened = Database::open(&cx, &path, wrong_dek()).await;
        assert!(
            matches!(opened, Err(OpenError::WrongDek { .. })),
            "{opened:?}"
        );
        assert!(
            Database::open(&cx, &path, keys()).await.is_ok(),
            "the correct keys must still open the empty database"
        );
    });
    assert!(report.lab_test_passed(), "lab failed");
}

#[test]
fn non_empty_database_open_with_wrong_dek_is_refused() {
    let ((), report) = run_async_under_lab(0x1a02_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = scratch("nonempty-wrong-dek");
        {
            let mut db = Database::create(&cx, &path, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, batch).await.unwrap();
        }
        assert!(matches!(
            Database::open(&cx, &path, wrong_dek()).await,
            Err(OpenError::WrongDek { .. })
        ));
        assert!(matches!(
            Database::open_rebuilding(&cx, &path, wrong_dek()).await,
            Err(OpenError::WrongDek { .. })
        ));
        let reopened = Database::open(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(1));
    });
    assert!(report.lab_test_passed(), "lab failed");
}

#[test]
fn correct_keys_still_open_and_read() {
    let ((), report) = run_async_under_lab(0x1a03_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = scratch("correct-keys");
        {
            let mut db = Database::create(&cx, &path, keys()).await.unwrap();
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![], vec![]);
            db.write(&cx, batch).await.unwrap();
        }
        let reopened = Database::open(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(1));
        assert!(reopened.vertex(VId(1)).unwrap().is_some());
    });
    assert!(report.lab_test_passed(), "lab failed");
}

#[test]
fn refused_wrong_key_leaves_database_writable_with_correct_key() {
    let ((), report) = run_async_under_lab(0x1a04_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.commit();
        let path = scratch("wrong-dek-write");
        drop(Database::create(&cx, &path, keys()).await.unwrap());
        assert!(matches!(
            Database::open(&cx, &path, wrong_dek()).await,
            Err(OpenError::WrongDek { .. })
        ));
        let mut db = Database::open(&cx, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), CommitSeq(0));
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(9), vec![], vec![]);
        assert_eq!(db.write(&cx, batch).await.unwrap(), CommitSeq(1));
        drop(db);
        let reopened = Database::open(&cx, &path, keys()).await.unwrap();
        assert_eq!(reopened.frontier().unwrap(), CommitSeq(1));
        assert!(reopened.vertex(VId(9)).unwrap().is_some());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
