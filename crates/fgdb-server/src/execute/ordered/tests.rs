use super::*;
use crate::recovery::tests::{fail_after_marker, served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb_warden::{Grant, QueryLimits};

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

#[test]
fn ordered_admission_is_authenticated_bounded_and_old_cursors_fail_after_recovery() {
    let ((), report) = run_async_under_lab(0x79a0_0602, |root| async move {
        let (mut server, token, path) = served(&root, "ordered-recovery").await;
        let db = server.databases.get_mut("test").unwrap();
        let generation = accepted(ordered_generation(db, &token));
        let expired = db
            .authority
            .issue_at(
                &Grant::read_only(
                    crate::TRUNK,
                    1,
                    QueryLimits {
                        max_nodes: 100,
                        max_work: 100,
                        max_rows: 100,
                    },
                ),
                0,
            )
            .unwrap();
        assert_eq!(
            ordered_generation(db, &expired).err().unwrap().code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            read_ordered_session(&root, db, &expired, None, generation.clone())
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation.clone())
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Statement
        );
        let directory = path.join("ordered-scratch");
        asupersync::fs::create_dir(&directory).await.unwrap();
        let mut config = crate::OrderedReadConfig::new(&directory);
        config.max_concurrent = 1;
        config.sort.run_rows = 2;
        config.sort.page_bytes = 31;
        config.sort.max_sort_work = 1_000_000;
        Arc::get_mut(db).unwrap().ordered_reads = Some(
            crate::ordered::OrderedReads::open(&root, config)
                .await
                .unwrap(),
        );
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let mut batch = vertex(1);
        batch.create_vertex(fgdb_types::VId(2), vec![], vec![]);
        batch.create_vertex(fgdb_types::VId(3), vec![], vec![]);
        db.db
            .write(&root)
            .await
            .unwrap()
            .write(&contexts.commit(), batch)
            .await
            .unwrap();
        // External-sort admission also pins the published view without
        // waiting for an otherwise unchanged writer to release its lock.
        let held = db.db.write(&root).await.unwrap();
        let (mut session, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation.clone()).await);
        drop(held);
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation.clone())
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Budget
        );
        let params = fgdb_gql::GqlParameters::new();
        let prepared = accepted(session.prepare(
            &query,
            "MATCH (n) RETURN 1 AS value ORDER BY value",
            &params,
        ));
        let mut cursor = accepted(
            accepted(session.start(&query, &prepared, &params))
                .finish()
                .await,
        );
        assert_eq!(cursor.snapshot_seq(), 1);
        assert_eq!(accepted(cursor.next().await), Some(vec![WireValue::Int(1)]));
        fail_after_marker(&root, db, 4).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert_eq!(
            cursor.next().await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        drop(cursor);
        drop(session);
        assert_eq!(
            read_ordered_session(&root, db, &token, None, generation)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::DatabaseRecovering
        );
        let generation = accepted(ordered_generation(db, &token));
        let (mut fresh, _) =
            accepted(read_ordered_session(&root, db, &token, None, generation).await);
        let prepared = accepted(fresh.prepare(
            &query,
            "MATCH (n) RETURN 1 AS value ORDER BY value LIMIT 1",
            &params,
        ));
        let mut cursor = accepted(
            accepted(fresh.start(&query, &prepared, &params))
                .finish()
                .await,
        );
        assert_eq!(cursor.snapshot_seq(), 2);
        assert_eq!(accepted(cursor.next().await), Some(vec![WireValue::Int(1)]));
        drop(cursor);
        drop(fresh);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_pending_order_does_not_hold_a_recovery_operation() {
    let ((), report) = run_async_under_lab(0x79a0_0603, |root| async move {
        let (server, token, _) = served(&root, "ordered-pending-recovery").await;
        let db = &server.databases["test"];
        let generation = db.db.read(&root).await.unwrap().generation();
        let mut future = Box::pin(in_generation(
            &generation,
            core::future::pending::<Result<(), Refusal>>(),
        ));
        poll_fn(|context| {
            assert!(future.as_mut().poll(context).is_pending());
            Poll::Ready(())
        })
        .await;
        fail_after_marker(&root, db, 1).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        assert_eq!(
            future.await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        assert!(db.admits(&token));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
