use super::*;
use crate::recovery::tests::{served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb::{CrashPoint, WriteError};
use fgdb_gql::GraphSymbolKind;
use fgdb_protocol::body::{ExecuteConditionalWrite, ExecuteMode};
use fgdb_types::{CommitSeq, VId};
use fgdb_warden::{Grant, QueryLimits};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

fn request(expected_seq: u64, statement: &str) -> ExecuteConditionalWrite {
    ExecuteConditionalWrite {
        expected_seq,
        statement: statement.into(),
        parameters: Vec::new(),
    }
}

async fn state(cx: &Cx, db: &Served) -> (CommitSeq, usize) {
    let guard = db.db.read(cx).await.unwrap();
    (guard.frontier().unwrap(), guard.vertices().unwrap().len())
}

#[test]
fn conditional_write_stats_returning_and_no_effects_share_exact_frontier_admission() {
    let ((), report) = run_async_under_lab(0xc0d1_0001, |root| async move {
        let (mut server, token, _) = served(&root, "conditional-native-results").await;
        Arc::get_mut(server.databases.get_mut("test").unwrap())
            .unwrap()
            .symbols
            .bind(GraphSymbolKind::Property, "p", 1)
            .unwrap();
        let db = &server.databases["test"];
        let seed =
            accepted(write_if_current(&root, db, &token, &request(0, "CREATE (n {p:1})")).await);
        assert_eq!(
            seed.outcome,
            Outcome::WriteCommitted {
                seq: 1,
                statements: 1
            }
        );
        let winner =
            accepted(write_if_current(&root, db, &token, &request(1, "MATCH (n) SET n.p=2")).await);
        assert_eq!(
            winner.outcome,
            Outcome::WriteCommitted {
                seq: 2,
                statements: 1
            }
        );

        for text in [
            "MATCH (n) SET n.p=3",
            "CREATE (n) RETURN 7 AS value",
            "MATCH (n) WHERE n.p=99 SET n.p=3",
            "invalid syntax",
        ] {
            let error = write_if_current(&root, db, &token, &request(1, text))
                .await
                .err()
                .unwrap();
            assert_eq!(error.code, ErrorCode::Conflict);
            assert_eq!(
                error.message,
                "the expected frontier is no longer current; no write was executed"
            );
            assert_eq!(state(&root, db).await, (CommitSeq(2), 1));
        }
        let mut invalid = request(1, "CREATE (n {p:$p})");
        invalid.parameters = vec![("p".into(), WireValue::Vertex(9))];
        assert_eq!(
            write_if_current(&root, db, &token, &invalid)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Conflict
        );

        let no_effect = accepted(
            write_if_current(
                &root,
                db,
                &token,
                &request(2, "MATCH (n) WHERE n.p=99 SET n.p=3"),
            )
            .await,
        );
        assert_eq!(
            no_effect.outcome,
            Outcome::ReadClosed {
                seq: 2,
                statements: 1
            }
        );
        let returned = accepted(
            write_if_current(
                &root,
                db,
                &token,
                &request(2, "MATCH (n) SET n.p=3 RETURN n.p AS p"),
            )
            .await,
        );
        assert_eq!(returned.columns, ["p"]);
        assert_eq!(returned.rows, [vec![WireValue::Int(3)]]);
        assert_eq!(
            returned.outcome,
            Outcome::WriteCommitted {
                seq: 3,
                statements: 1
            }
        );

        let answer = accepted(
            read(
                &root,
                db,
                &token,
                &Execute {
                    mode: ExecuteMode::Read,
                    statement: "MATCH (n) RETURN n.p AS p".into(),
                    parameters: Vec::new(),
                },
            )
            .await,
        );
        assert_eq!(answer.rows, [vec![WireValue::Int(3)]]);
        assert_eq!(state(&root, db).await, (CommitSeq(3), 1));
        let read_only = db
            .authority
            .issue_at(
                &Grant::read_only(
                    TRUNK,
                    u64::MAX / 2,
                    QueryLimits {
                        max_nodes: 100_000,
                        max_work: 100_000_000,
                        max_rows: 100,
                    },
                ),
                unix_millis(),
            )
            .unwrap();
        assert_eq!(
            write_if_current(&root, db, &read_only, &invalid)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PermissionDenied
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conditional_write_rechecks_frontier_and_authority_after_waiting_for_writer() {
    let ((), report) = run_async_under_lab(0xc0d1_0002, |root| async move {
        for (name, returning, retired) in [
            ("conditional-wait-stats", false, false),
            ("conditional-wait-return", true, false),
            ("conditional-wait-retired", true, true),
        ] {
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let mut held = db.db.write(&root).await.unwrap();
            let request = request(
                0,
                if returning {
                    "CREATE (n) RETURN 7 AS value"
                } else {
                    "CREATE (n)"
                },
            );
            let mut waiting = write_if_current(&root, db, &token, &request);
            assert!(matches!(
                waiting
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
            held.write(&contexts.commit(), vertex(1)).await.unwrap();
            if retired {
                assert!(db.authority.retire());
            }
            drop(held);
            let error = waiting.await.err().unwrap();
            assert_eq!(
                error.code,
                if retired {
                    ErrorCode::PermissionDenied
                } else {
                    ErrorCode::Conflict
                }
            );
            assert_eq!(state(&root, db).await, (CommitSeq(1), 1));
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn conditional_write_waiter_cannot_cross_a_recovery_generation() {
    let ((), report) = run_async_under_lab(0xc0d1_0003, |root| async move {
        let (server, token, _) = served(&root, "conditional-wait-recovery").await;
        let db = &server.databases["test"];
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let mut held = db.db.write(&root).await.unwrap();
        let request = request(0, "CREATE (n) RETURN 7 AS value");
        let mut waiting = write_if_current(&root, db, &token, &request);
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let prepared = held.prepare_atomic_writes(vec![vertex(1)]).unwrap();
        let error = held
            .commit_prepared_with_crash(
                &contexts.commit(),
                prepared,
                Some(CrashPoint::AfterMarkerBeforeD2),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, WriteError::CommitOutcomeUnknown { .. }));
        drop(held);
        assert_eq!(
            waiting.await.err().unwrap().code,
            ErrorCode::DatabaseRecovering
        );
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { .. }
        ));
        let guard = db.db.read(&root).await.unwrap();
        assert_eq!(guard.frontier().unwrap(), CommitSeq(1));
        assert!(guard.vertex(VId(1)).unwrap().is_some());
        assert_eq!(guard.vertices().unwrap().len(), 1);
        drop(guard);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
