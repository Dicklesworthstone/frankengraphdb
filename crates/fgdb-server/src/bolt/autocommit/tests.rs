//! The production Bolt state machine and real Chronicle-backed server fixture.
//! WireIo only captures transport bytes; it never replaces the database engine.

use super::*;
use crate::recovery::tests::{WireIo, fail_after_marker, served, settled, vertex};
use asupersync::lab::run_async_under_lab;
use fgdb::CrashPoint;
use fgdb_warden::{Grant, QueryLimits, Scope};
use std::future::Future;
use std::task::{Context, Waker};

fn connection(server: &Server, token: CapabilityToken) -> Connection<'_> {
    Connection {
        server,
        token: Some(token),
        failed: false,
        pending: None,
        transaction: None,
        output_database: None,
        output_generation: None,
        id: 0xb017,
    }
}

fn io() -> Io {
    let (stream, _) = WireIo::new([], false);
    Io {
        stream: Box::new(stream),
        local: None,
        dechunker: Dechunker::new(4096),
        out: Vec::new(),
        failed: false,
    }
}

fn extra() -> Map {
    vec![("db".into(), Value::string("test"))]
}

async fn frontier(cx: &Cx, db: &Served) -> CommitSeq {
    db.db.read(cx).await.unwrap().frontier().unwrap()
}

#[test]
fn write_options_preserve_local_causal_bookmarks_and_reject_weaker_semantics() {
    assert_eq!(write_options(&vec![], "social").unwrap(), CommitSeq(0));
    let options = vec![
        ("mode".into(), Value::string("w")),
        (
            "bookmarks".into(),
            Value::List(vec![
                Value::string("fgdb:social:3"),
                Value::string("fgdb:social:1"),
                Value::string("fgdb:social:3"),
            ]),
        ),
    ];
    assert_eq!(write_options(&options, "social").unwrap(), CommitSeq(3));
    assert_eq!(
        write_options(
            &vec![(
                "bookmarks".into(),
                Value::List(vec![Value::string("fgdb:a:b:7")]),
            )],
            "a:b",
        )
        .unwrap(),
        CommitSeq(7),
    );
    assert_eq!(
        write_options(&vec![("mode".into(), Value::string("r"))], "social")
            .unwrap_err()
            .code,
        "Neo.ClientError.Statement.AccessMode",
    );
    for value in [Value::Null, Value::Bool(true), Value::string("read")] {
        assert!(write_options(&vec![("mode".into(), value)], "social").is_err());
    }
    for (name, value) in [
        ("tx_timeout", Value::Int(0)),
        ("tx_timeout", Value::Int(1000)),
        ("imp_user", Value::string("another-user")),
    ] {
        assert!(write_options(&vec![(name.into(), value)], "social").is_err());
    }
}

#[test]
fn malformed_foreign_and_overflowing_bookmarks_are_not_ignored() {
    for bookmark in [
        "neo4j:bookmark:v1:tx7",
        "fgdb:other:7",
        "fgdb:social:",
        "fgdb:social:-1",
        "fgdb:social:+1",
        "fgdb:social:01",
        "fgdb:social:18446744073709551616",
        "fgdb:social:1junk",
    ] {
        let options = vec![(
            "bookmarks".into(),
            Value::List(vec![Value::string(bookmark)]),
        )];
        assert!(write_options(&options, "social").is_err(), "{bookmark}");
    }
    for value in [
        Value::string("fgdb:social:1"),
        Value::List(vec![Value::Int(1)]),
    ] {
        assert!(write_options(&vec![("bookmarks".into(), value)], "social").is_err());
    }
}

#[test]
fn pending_writes_are_consumed_once_and_committed_rows_survive_real_recovery() {
    let ((), report) = run_async_under_lab(0xb017_0001, |root| async move {
        let (server, token, _) = served(&root, "bolt-write-consume-once").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        let fields = client
            .run(&root, "CREATE (n)", &vec![], &extra())
            .await
            .unwrap();
        assert_eq!(get(&fields, "fields"), Some(&Value::List(vec![])));
        assert!(client.pending.as_ref().unwrap().write.is_some());
        assert_eq!(frontier(&root, db).await, CommitSeq(0));
        let mut wire = io();
        let done = client.pull(&root, &mut wire, -1, false).await.unwrap();
        assert_eq!(get(&done, "type").and_then(Value::as_str), Some("w"));
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:test:1"),
        );
        assert!(client.pending.is_none());
        assert_eq!(frontier(&root, db).await, CommitSeq(1));
        assert!(client.pull(&root, &mut wire, -1, false).await.is_err());
        assert_eq!(frontier(&root, db).await, CommitSeq(1));
        fail_after_marker(&root, db, 41).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        ));
        let guard = db.db.read(&root).await.unwrap();
        assert_eq!(guard.frontier().unwrap(), CommitSeq(2));
        assert_eq!(guard.vertices_at(CommitSeq(2)).unwrap().len(), 2);
        drop(guard);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn creation_chains_are_deferred_and_consumed_once_without_return_rows() {
    let ((), report) = run_async_under_lab(0xb017_0011, |root| async move {
        let (server, token, _) = served(&root, "bolt-creation-chain").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        let mut wire = io();
        for (discard, before) in [(false, 0), (true, 1)] {
            let fields = client
                .run(&root, "CREATE (a) WITH a CREATE (b)", &vec![], &extra())
                .await
                .unwrap();
            assert_eq!(get(&fields, "fields"), Some(&Value::List(vec![])));
            assert!(client.pending.as_ref().unwrap().write.is_some());
            assert_eq!(frontier(&root, db).await, CommitSeq(before));
            let done = client.pull(&root, &mut wire, -1, discard).await.unwrap();
            assert_eq!(get(&done, "type").and_then(Value::as_str), Some("w"));
            assert_eq!(
                get(&done, "bookmark").and_then(Value::as_str),
                Some(format!("fgdb:test:{}", before + 1).as_str()),
            );
            assert_eq!(frontier(&root, db).await, CommitSeq(before + 1));
            assert!(client.pending.is_none());
            assert!(client.pull(&root, &mut wire, -1, false).await.is_err());
            assert_eq!(
                db.db.read(&root).await.unwrap().vertices().unwrap().len(),
                ((before + 1) * 2) as usize,
            );
        }
        // The Bolt profile still excludes write RETURN. Recognizing a chain
        // must not silently discard an explicitly requested public result.
        assert!(
            client
                .run(
                    &root,
                    "CREATE (a) WITH a CREATE (b) RETURN 1 AS value",
                    &vec![],
                    &extra(),
                )
                .await
                .is_err()
        );
        assert_eq!(frontier(&root, db).await, CommitSeq(2));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_write_is_not_replayed_after_database_reopen() {
    let ((), report) = run_async_under_lab(0xb017_0002, |root| async move {
        let (server, token, _) = served(&root, "bolt-write-stale-queue").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        client
            .run(&root, "CREATE (n)", &vec![], &extra())
            .await
            .unwrap();
        // An idle queue retains no operation permit that could delay recovery.
        fail_after_marker(&root, db, 42).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        ));
        let mut wire = io();
        client
            .handle(&root, &mut wire, Request::Pull { n: -1, qid: -1 })
            .await;
        assert!(client.failed);
        assert!(client.pending.is_none());
        assert_eq!(frontier(&root, db).await, CommitSeq(1));
        let guard = db.db.read(&root).await.unwrap();
        assert_eq!(guard.vertices_at(CommitSeq(1)).unwrap().len(), 1);
        drop(guard);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn recovery_while_a_pull_waits_for_the_write_lock_cannot_move_it_to_the_new_generation() {
    let ((), report) = run_async_under_lab(0xb017_0003, |root| async move {
        let (server, token, _) = served(&root, "bolt-write-waiting-generation").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        client
            .run(&root, "CREATE (n)", &vec![], &extra())
            .await
            .unwrap();
        let mut held = db.db.write(&root).await.unwrap();
        let mut wire = io();
        let mut pull = Box::pin(client.pull(&root, &mut wire, -1, false));
        assert!(
            pull.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let prepared = held.prepare_atomic_writes(vec![vertex(43)]).unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        assert!(
            held.commit_prepared_with_crash(
                &contexts.commit(),
                prepared,
                Some(CrashPoint::AfterMarkerBeforeD2),
            )
            .await
            .is_err()
        );
        drop(held);
        assert_eq!(
            pull.await.unwrap_err().code,
            "Neo.TransientError.General.DatabaseUnavailable",
        );
        assert!(client.pending.is_none());
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        ));
        assert_eq!(frontier(&root, db).await, CommitSeq(1));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn retired_authority_and_zero_work_budget_refuse_without_a_write() {
    let ((), report) = run_async_under_lab(0xb017_0004, |root| async move {
        for retire in [false, true] {
            let name = if retire {
                "bolt-write-retired"
            } else {
                "bolt-write-zero-work"
            };
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let token = if retire {
                token
            } else {
                db.authority
                    .issue_at(
                        &Grant {
                            branch: crate::TRUNK.into(),
                            labels: Scope::All,
                            relations: Scope::All,
                            properties: Scope::All,
                            rights: Rights::Write,
                            limits: QueryLimits {
                                max_nodes: 100,
                                max_work: 0,
                                max_rows: 0,
                            },
                            expires_at_ms: u64::MAX / 2,
                        },
                        crate::unix_millis(),
                    )
                    .unwrap()
            };
            let mut client = connection(&server, token);
            client
                .run(&root, "CREATE (n)", &vec![], &extra())
                .await
                .unwrap();
            if retire {
                db.authority.retire();
            }
            let mut wire = io();
            client
                .handle(&root, &mut wire, Request::Pull { n: -1, qid: -1 })
                .await;
            assert!(client.failed);
            assert!(client.pending.is_none());
            assert_eq!(frontier(&root, db).await, CommitSeq(0));
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn future_bookmarks_and_invalid_pull_identifiers_never_execute_the_queue() {
    let ((), report) = run_async_under_lab(0xb017_0005, |root| async move {
        let (server, token, _) = served(&root, "bolt-write-invalid-consume").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token.clone());
        let mut options = extra();
        options.push((
            "bookmarks".into(),
            Value::List(vec![Value::string("fgdb:test:1")]),
        ));
        client
            .run(&root, "CREATE (n)", &vec![], &options)
            .await
            .unwrap();
        let mut wire = io();
        assert_eq!(
            client
                .pull(&root, &mut wire, -1, false)
                .await
                .unwrap_err()
                .code,
            "Neo.TransientError.General.DatabaseUnavailable",
        );
        assert_eq!(frontier(&root, db).await, CommitSeq(0));
        for request in [
            Request::Pull { n: 0, qid: -1 },
            Request::Pull { n: -2, qid: -1 },
            Request::Pull { n: -1, qid: 5 },
            Request::Discard { n: -1, qid: 5 },
        ] {
            let mut client = connection(&server, token.clone());
            client
                .run(&root, "CREATE (n)", &vec![], &extra())
                .await
                .unwrap();
            client.handle(&root, &mut wire, request).await;
            assert!(client.failed);
            assert!(client.pending.is_none());
            assert_eq!(frontier(&root, db).await, CommitSeq(0));
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn write_hint_does_not_let_quotes_inside_block_comments_hide_a_mutation() {
    assert!(looks_like_write("/* a ' quote */ CREATE (n)"));
    assert!(looks_like_write(
        "/* outer /* nested */ comment */ MATCH (n) DELETE n"
    ));
    assert!(!looks_like_write("/* CREATE (n) */ RETURN 1"));
    assert!(!looks_like_write("RETURN '/* CREATE (n) */' AS text"));
}
