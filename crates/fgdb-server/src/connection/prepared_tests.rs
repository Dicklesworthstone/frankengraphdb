//! Real native preparation and execution; the cache owns no database pin.

use super::*;
use crate::recovery::tests::{fail_after_marker, served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb_types::{CommitSeq, PurposeContexts};

fn ok<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

#[test]
fn a_prepared_read_rebinds_operands_and_observes_commits_after_preparation() {
    let ((), report) = run_async_under_lab(0x79a0_0401, |root| async move {
        let (server, token, _) = served(&root, "prepared-live-frontier").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut cache = PreparedReads::default();
        let definition = ok(prepare_read(
            &root,
            db,
            &token,
            &Prepare {
                statement: "MATCH (n) RETURN $value AS value".into(),
                parameters: vec![("value".into(), WireValue::Int(7))],
            },
        )
        .await);
        let (handle, _) = ok(cache.insert(&root, definition));
        let definition = ok(cache.get(handle));
        let first = ok(read_prepared(
            &root,
            db,
            &token,
            &definition,
            &[("value".into(), WireValue::Int(11))],
        )
        .await);
        assert_eq!(first.outcome, Outcome::Rows { seq: 0 });
        assert!(first.rows.is_empty());
        let mut batch = vertex(1);
        batch.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        assert_eq!(
            db.db
                .write(&root)
                .await
                .unwrap()
                .write(&contexts.commit(), batch)
                .await
                .unwrap(),
            CommitSeq(1)
        );
        let second = ok(read_prepared(
            &root,
            db,
            &token,
            &definition,
            &[("value".into(), WireValue::Int(19))],
        )
        .await);
        assert_eq!(second.columns, ["value"]);
        assert_eq!(
            second.rows,
            [vec![WireValue::Int(19)], vec![WireValue::Int(19)]]
        );
        assert_eq!(second.outcome, Outcome::Rows { seq: 1 });
        assert_eq!(
            read_prepared(&root, db, &token, &definition, &[])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement,
            "representative values are not defaults"
        );
        let after_refusal = ok(read_prepared(
            &root,
            db,
            &token,
            &definition,
            &[("value".into(), WireValue::Int(23))],
        )
        .await);
        assert_eq!(
            after_refusal.rows,
            [vec![WireValue::Int(23)], vec![WireValue::Int(23)]]
        );
        assert!(
            prepare_read(
                &root,
                db,
                &token,
                &Prepare {
                    statement: "CREATE ()".into(),
                    parameters: vec![]
                },
            )
            .await
            .is_err()
        );
        assert_eq!(
            db.db.read(&root).await.unwrap().frontier().unwrap(),
            CommitSeq(1)
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_cache_is_bounded_and_foreign_release_cannot_remove_another_owner() {
    let ((), report) = run_async_under_lab(0x79a0_0402, |root| async move {
        let (server, token, _) = served(&root, "prepared-cache-budget").await;
        let db = &server.databases["test"];
        let request = Prepare {
            statement: "MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        let mut cache = PreparedReads::default();
        let mut handles = Vec::new();
        for _ in 0..MAX_PREPARED_READS {
            let definition = ok(prepare_read(&root, db, &token, &request).await);
            handles.push(ok(cache.insert(&root, definition)).0);
        }
        assert_eq!(cache.entries.len(), MAX_PREPARED_READS);
        assert_eq!(
            cache.source_bytes,
            MAX_PREPARED_READS * request.statement.len()
        );
        let overflow = ok(prepare_read(&root, db, &token, &request).await);
        assert_eq!(
            cache.insert(&root, overflow).err().unwrap().code,
            ErrorCode::Budget
        );
        let mut foreign = PreparedReads::default();
        let foreign_error = foreign.get(handles[0]).err().unwrap();
        foreign.release(handles[0]);
        assert!(cache.get(handles[0]).is_ok());
        cache.release(handles[0]);
        cache.release(handles[0]);
        let released_error = cache.get(handles[0]).err().unwrap();
        let unknown_error = cache.get(PreparedHandle([0x7f; 16])).err().unwrap();
        assert_eq!(foreign_error.code, released_error.code);
        assert_eq!(foreign_error.message, released_error.message);
        assert_eq!(unknown_error.message, released_error.message);
        assert_eq!(cache.entries.len(), MAX_PREPARED_READS - 1);
        assert_eq!(
            cache.source_bytes,
            (MAX_PREPARED_READS - 1) * request.statement.len()
        );
        let replacement = ok(prepare_read(&root, db, &token, &request).await);
        let replacement = ok(cache.insert(&root, replacement)).0;
        assert_ne!(replacement, handles[0]);
        assert_eq!(cache.entries.len(), MAX_PREPARED_READS);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_handles_and_borrowed_templates_never_revive_after_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0403, |root| async move {
        let (server, token, _) = served(&root, "prepared-recovery-fence").await;
        let db = &server.databases["test"];
        let request = Prepare {
            statement: "MATCH (n) RETURN n".into(),
            parameters: vec![],
        };
        let mut cache = PreparedReads::default();
        let definition = ok(prepare_read(&root, db, &token, &request).await);
        let (old_handle, _) = ok(cache.insert(&root, definition));
        let retained = ok(cache.get(old_handle));
        fail_after_marker(&root, db, 99).await;
        assert_eq!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        );
        assert_eq!(
            read_prepared(&root, db, &token, &retained, &[])
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        assert!(cache.get(old_handle).is_err());
        assert!(cache.entries.is_empty());
        assert_eq!(cache.source_bytes, 0);
        let definition = ok(prepare_read(&root, db, &token, &request).await);
        let (new_handle, _) = ok(cache.insert(&root, definition));
        let definition = ok(cache.get(new_handle));
        let answer = ok(read_prepared(&root, db, &token, &definition, &[]).await);
        assert_eq!(answer.rows, [vec![WireValue::Vertex(99)]]);
        assert!(cache.get(old_handle).is_err());
        assert!(db.authority.retire());
        assert!(
            read_prepared(&root, db, &token, &definition, &[])
                .await
                .is_err()
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
