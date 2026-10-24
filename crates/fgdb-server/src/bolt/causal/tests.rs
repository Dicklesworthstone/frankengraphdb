//! The production Bolt connection over the real Chronicle-backed server fixture.
//! The byte sink only replaces transport. These tests are not cluster, signed
//! RYW-token, power-loss, or public protocol-conformance evidence.

use super::super::{Connection, Dechunker, Io, Request};
use super::*;
use crate::Server;
use crate::recovery::tests::{WireIo, fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;
use fgdb_warden::{Grant, QueryLimits, Rights, Scope};
use std::future::Future;
use std::task::{Context, Waker};

const UNREACHED: &str = "Neo.TransientError.General.DatabaseUnavailable";
const INVALID: &str = "Neo.ClientError.Request.Invalid";

fn connection(server: &Server, token: CapabilityToken) -> Connection<'_> {
    Connection {
        server,
        token: Some(token),
        failed: false,
        pending: None,
        transaction: None,
        output_database: None,
        output_generation: None,
        id: 0xca05,
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

fn extra(sequence: u64) -> Map {
    vec![
        ("db".into(), Value::string("test")),
        (
            "bookmarks".into(),
            Value::List(vec![Value::string(format!("fgdb:test:{sequence}"))]),
        ),
    ]
}

async fn write_vertex(cx: &Cx, client: &mut Connection<'_>) {
    client
        .run(cx, "CREATE (n)", &vec![], &extra(0))
        .await
        .unwrap();
    client.pull(cx, &mut io(), -1, true).await.unwrap();
    assert!(client.pending.is_none());
}

fn assert_markers(client: &Connection<'_>, count: usize) {
    let pending = client
        .pending
        .as_ref()
        .or_else(|| client.transaction.as_ref()?.cursors.latest())
        .expect("a read result");
    let rows = &pending.rows;
    assert_eq!(rows.len(), count);
    for row in rows {
        assert_eq!(row, &vec![Value::Int(7)]);
    }
}

#[test]
fn canonical_bookmarks_share_the_write_grammar_and_full_u64_domain() {
    assert_eq!(options(&vec![], "test").unwrap().minimum_seq, CommitSeq(0));
    assert_eq!(options(&vec![], "test").unwrap().mode, AccessMode::Write);
    for value in [Value::Null, Value::List(vec![])] {
        assert_eq!(
            options(&vec![("bookmarks".into(), value)], "test")
                .unwrap()
                .minimum_seq,
            CommitSeq(0),
        );
    }
    let multiple = vec![(
        "bookmarks".into(),
        Value::List(vec![
            Value::string("fgdb:test:2"),
            Value::string("fgdb:test:7"),
            Value::string("fgdb:test:1"),
            Value::string("fgdb:test:7"),
        ]),
    )];
    assert_eq!(
        options(&multiple, "test").unwrap().minimum_seq,
        CommitSeq(7)
    );
    assert_eq!(
        options(&extra(u64::MAX), "test").unwrap().minimum_seq,
        CommitSeq(u64::MAX),
    );
    let colon = vec![(
        "bookmarks".into(),
        Value::List(vec![Value::string("fgdb:a:b:7")]),
    )];
    assert_eq!(options(&colon, "a:b").unwrap().minimum_seq, CommitSeq(7));
    assert_eq!(options(&colon, "a").unwrap_err().code, INVALID);
    assert!(require_frontier(CommitSeq(7), CommitSeq(7)).is_ok());
    assert!(require_frontier(CommitSeq(8), CommitSeq(7)).is_ok());
    assert_eq!(
        require_frontier(CommitSeq(6), CommitSeq(7))
            .unwrap_err()
            .code,
        UNREACHED,
    );
}

#[test]
fn malformed_and_foreign_bookmarks_never_collapse_into_an_empty_requirement() {
    for text in [
        "",
        "neo4j:bookmark:v1:tx7",
        "fgdb:other:7",
        "fgdb:test:",
        "fgdb:test:-1",
        "fgdb:test:+1",
        "fgdb:test:01",
        "fgdb:test:1 ",
        "fgdb:test: 1",
        "fgdb:test:1junk",
        "fgdb:test:1:2",
        "fgdb:test:١",
        "fgdb:test:18446744073709551616",
        "fgdb:test:999999999999999999999",
    ] {
        let extra = vec![("bookmarks".into(), Value::List(vec![Value::string(text)]))];
        assert_eq!(options(&extra, "test").unwrap_err().code, INVALID, "{text}");
    }
    for value in [Value::Null, Value::Int(1), Value::Bool(false)] {
        let extra = vec![(
            "bookmarks".into(),
            Value::List(vec![Value::string("fgdb:test:1"), value]),
        )];
        assert_eq!(options(&extra, "test").unwrap_err().code, INVALID);
    }
    for value in [
        Value::string("fgdb:test:1"),
        Value::Int(1),
        Value::Map(vec![]),
    ] {
        assert_eq!(
            options(&vec![("bookmarks".into(), value)], "test")
                .unwrap_err()
                .code,
            INVALID,
        );
    }
}

#[test]
fn transaction_options_cannot_change_authority_or_move_a_pinned_snapshot() {
    for mode in ["r", "w"] {
        assert!(options(&vec![("mode".into(), Value::string(mode))], "test").is_ok());
    }
    for mode in [Value::Null, Value::Bool(true), Value::string("read")] {
        assert_eq!(
            options(&vec![("mode".into(), mode)], "test")
                .unwrap_err()
                .code,
            INVALID,
        );
    }
    for key in ["tx_timeout", "imp_user"] {
        assert!(options(&vec![(key.into(), Value::Null)], "test").is_ok());
        for value in [Value::Int(0), Value::Int(10), Value::string("other")] {
            assert_eq!(
                options(&vec![(key.into(), value)], "test")
                    .unwrap_err()
                    .code,
                INVALID,
            );
        }
    }
    for key in ["db", "mode", "bookmarks", "tx_timeout", "imp_user"] {
        let extra = vec![(key.into(), Value::Null), (key.into(), Value::Null)];
        assert_eq!(options(&extra, "test").unwrap_err().code, INVALID);
    }
    assert!(in_transaction(&vec![], "test", CommitSeq(1)).is_ok());
    assert!(in_transaction(&extra(1), "test", CommitSeq(1)).is_ok());
    assert_eq!(
        in_transaction(&extra(2), "test", CommitSeq(1))
            .unwrap_err()
            .code,
        INVALID,
    );
    for db in [Value::string("other"), Value::Int(1)] {
        assert_eq!(
            in_transaction(&vec![("db".into(), db)], "test", CommitSeq(1))
                .unwrap_err()
                .code,
            INVALID,
        );
    }
    for db in [Value::Null, Value::string(""), Value::string("test")] {
        assert!(in_transaction(&vec![("db".into(), db)], "test", CommitSeq(1)).is_ok());
    }
}

#[test]
fn future_bookmarks_refuse_before_any_query_or_transaction_result_escapes() {
    let ((), report) = run_async_under_lab(0xca05_0001, |root| async move {
        let (server, token, _) = served(&root, "bolt-causal-before-result").await;
        let mut client = connection(&server, token);
        for text in [
            "RETURN 7 AS marker",
            "RETURN 1 / 0 AS marker",
            "RETURN 7 AS marker LIMIT 0",
            "MATCH (n) RETURN 7 AS marker",
            "CALL db.labels()",
            "CALL db.relationshipTypes()",
            "CALL db.propertyKeys()",
        ] {
            assert_eq!(
                client
                    .run(&root, text, &vec![], &extra(1))
                    .await
                    .unwrap_err()
                    .code,
                UNREACHED,
                "{text}",
            );
            assert!(client.pending.is_none());
            assert!(client.transaction.is_none());
        }
        assert_eq!(
            client.begin(&root, &extra(1)).await.unwrap_err().code,
            UNREACHED
        );
        assert!(client.transaction.is_none());
        let mut wire = io();
        client
            .handle(
                &root,
                &mut wire,
                Request::Run {
                    query: "RETURN 7 AS marker".into(),
                    parameters: vec![],
                    extra: extra(1),
                },
            )
            .await;
        assert!(client.failed);
        assert!(client.pending.is_none());
        client.handle(&root, &mut wire, Request::Reset).await;
        assert!(!client.failed);
        client
            .run(&root, "RETURN 7 AS marker", &vec![], &extra(0))
            .await
            .unwrap();
        assert_markers(&client, 1);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn autocommit_reads_observe_satisfied_write_bookmarks_but_keep_explicit_history() {
    let ((), report) = run_async_under_lab(0xca05_0002, |root| async move {
        let (server, token, _) = served(&root, "bolt-causal-write-read-history").await;
        let mut writer = connection(&server, token.clone());
        write_vertex(&root, &mut writer).await;
        let mut reader = connection(&server, token);
        reader
            .run(&root, "MATCH (n) RETURN 7 AS marker", &vec![], &extra(1))
            .await
            .unwrap();
        assert_markers(&reader, 1);
        assert_eq!(reader.pending.as_ref().unwrap().seq, Some(CommitSeq(1)));
        let done = reader.pull(&root, &mut io(), -1, true).await.unwrap();
        assert_eq!(
            get(&done, "bookmark").and_then(Value::as_str),
            Some("fgdb:test:1")
        );
        // The causal admission floor is not a replacement for intentional time travel.
        reader
            .run(
                &root,
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN 7 AS marker",
                &vec![],
                &extra(1),
            )
            .await
            .unwrap();
        assert_markers(&reader, 0);
        reader.pull(&root, &mut io(), -1, true).await.unwrap();
        assert_eq!(
            reader
                .run(&root, "RETURN 7 AS marker", &vec![], &extra(2))
                .await
                .unwrap_err()
                .code,
            UNREACHED,
        );
        write_vertex(&root, &mut writer).await;
        reader
            .run(&root, "MATCH (n) RETURN 7 AS marker", &vec![], &extra(2))
            .await
            .unwrap();
        assert_markers(&reader, 2);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_later_commit_cannot_satisfy_a_bookmark_for_an_already_pinned_transaction() {
    let ((), report) = run_async_under_lab(0xca05_0003, |root| async move {
        let (server, token, _) = served(&root, "bolt-causal-fixed-transaction").await;
        let mut writer = connection(&server, token.clone());
        write_vertex(&root, &mut writer).await;
        let mut reader = connection(&server, token);
        reader.begin(&root, &extra(1)).await.unwrap();
        assert_eq!(reader.transaction.as_ref().unwrap().seq, CommitSeq(1));
        write_vertex(&root, &mut writer).await;
        reader
            .run(&root, "MATCH (n) RETURN 7 AS marker", &vec![], &vec![])
            .await
            .unwrap();
        assert_markers(&reader, 1);
        reader.pull(&root, &mut io(), -1, true).await.unwrap();
        let last_qid = reader.transaction.as_ref().unwrap().cursors.last_qid();
        assert_eq!(
            reader
                .run(&root, "MATCH (n) RETURN 7 AS marker", &vec![], &extra(2))
                .await
                .unwrap_err()
                .code,
            INVALID,
        );
        assert_eq!(reader.transaction.as_ref().unwrap().seq, CommitSeq(1));
        assert_eq!(
            reader.transaction.as_ref().unwrap().cursors.last_qid(),
            last_qid
        );
        assert!(reader.pending.is_none());
        assert_eq!(
            reader
                .run(&root, "CALL db.labels()", &vec![], &extra(2))
                .await
                .unwrap_err()
                .code,
            INVALID,
        );
        let mut wire = io();
        reader
            .handle(
                &root,
                &mut wire,
                Request::Run {
                    query: "MATCH (n) RETURN 7 AS marker".into(),
                    parameters: vec![],
                    extra: extra(2),
                },
            )
            .await;
        assert!(reader.failed);
        assert!(reader.transaction.is_none());
        assert!(reader.pending.is_none());
        reader.handle(&root, &mut wire, Request::Reset).await;
        reader.begin(&root, &extra(2)).await.unwrap();
        reader
            .run(&root, "MATCH (n) RETURN 7 AS marker", &vec![], &vec![])
            .await
            .unwrap();
        assert_markers(&reader, 2);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn schema_fast_paths_enforce_bookmarks_and_reject_unsupported_transaction_options() {
    let ((), report) = run_async_under_lab(0xca05_0004, |root| async move {
        let (server, token, _) = served(&root, "bolt-causal-schema-options").await;
        let mut writer = connection(&server, token.clone());
        write_vertex(&root, &mut writer).await;
        let mut reader = connection(&server, token);
        for text in [
            "CALL db.labels()",
            "CALL db.relationshipTypes()",
            "CALL db.propertyKeys()",
        ] {
            reader.run(&root, text, &vec![], &extra(1)).await.unwrap();
            // Operator bindings are not graph rows: do not fabricate a schema bookmark.
            assert!(reader.pending.as_ref().unwrap().seq.is_none());
            let done = reader.pull(&root, &mut io(), -1, true).await.unwrap();
            assert!(get(&done, "bookmark").is_none());
            assert_eq!(
                reader
                    .run(&root, text, &vec![], &extra(2))
                    .await
                    .unwrap_err()
                    .code,
                UNREACHED,
            );
        }
        for option in [
            ("imp_user", Value::string("another-user")),
            ("tx_timeout", Value::Int(0)),
            ("tx_timeout", Value::Int(1000)),
            ("mode", Value::string("invalid")),
            (
                "bookmarks",
                Value::List(vec![Value::string("fgdb:other:1")]),
            ),
        ] {
            let input = vec![
                ("db".into(), Value::string("test")),
                (option.0.into(), option.1),
            ];
            for text in ["RETURN 7 AS marker", "CALL db.labels()"] {
                assert_eq!(
                    reader
                        .run(&root, text, &vec![], &input)
                        .await
                        .unwrap_err()
                        .code,
                    INVALID,
                );
                assert!(reader.pending.is_none());
            }
            assert_eq!(reader.begin(&root, &input).await.unwrap_err().code, INVALID);
            assert!(reader.transaction.is_none());
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn read_bookmarks_do_not_grant_write_rights_or_reattach_a_recovered_transaction() {
    let ((), report) = run_async_under_lab(0xca05_0005, |root| async move {
        let (server, token, _) = served(&root, "bolt-causal-authority-recovery").await;
        let db = &server.databases["test"];
        let read_token = db
            .authority
            .issue_at(
                &Grant {
                    branch: crate::TRUNK.into(),
                    labels: Scope::All,
                    relations: Scope::All,
                    properties: Scope::All,
                    rights: Rights::Read,
                    limits: QueryLimits {
                        max_nodes: 1_000_000,
                        max_work: 100_000_000,
                        max_rows: 1_000_000,
                    },
                    expires_at_ms: u64::MAX / 2,
                },
                crate::unix_millis(),
            )
            .unwrap();
        let mut reader = connection(&server, read_token);
        assert_eq!(
            reader
                .run(&root, "CREATE (n)", &vec![], &extra(0))
                .await
                .unwrap_err()
                .code,
            "Neo.ClientError.Statement.AccessMode",
        );
        assert!(reader.pending.is_none());
        reader.begin(&root, &extra(0)).await.unwrap();
        fail_after_marker(&root, db, 0xca05).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        ));
        assert!(
            reader
                .run(&root, "RETURN 7 AS marker", &vec![], &vec![])
                .await
                .is_err()
        );
        assert!(reader.pending.is_none());
        // A new request may use the recovered root, never the old transaction.
        let mut fresh = connection(&server, token);
        fresh
            .run(&root, "MATCH (n) RETURN 7 AS marker", &vec![], &extra(1))
            .await
            .unwrap();
        assert_markers(&fresh, 1);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_causal_session_admission_leaves_no_transaction_or_result_owner() {
    let ((), report) = run_async_under_lab(0xca05_0006, |root| async move {
        let (server, token, _) = served(&root, "bolt-causal-cancel-admission").await;
        let db = &server.databases["test"];
        let mut reader = connection(&server, token);
        let mut held = db.db.write(&root).await.unwrap();
        // A completed successor has dissociated the otherwise unpinned weak
        // view, but this write guard has not yet published its replacement.
        // Admission genuinely waits on the guarded fallback, not on a normal
        // writer that still owns an immediately pinnable old generation.
        let contexts = fgdb_types::PurposeContexts::narrow_runtime_root(&root);
        held.write(&contexts.commit(), crate::recovery::tests::vertex(1))
            .await
            .unwrap();
        let input = extra(0);
        let mut admission = Box::pin(reader.begin(&root, &input));
        assert!(
            admission
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(admission);
        assert!(reader.transaction.is_none());
        assert!(reader.pending.is_none());
        drop(held);
        reader.begin(&root, &extra(0)).await.unwrap();
        assert_eq!(reader.transaction.as_ref().unwrap().seq, CommitSeq(1));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
