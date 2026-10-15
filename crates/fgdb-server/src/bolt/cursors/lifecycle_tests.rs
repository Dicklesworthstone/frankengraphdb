//! Exercise the production Connection and real Chronicle-backed server.
//! Synthetic scalar rows isolate delivery-window laws; native MATCH, schema
//! RUN, concurrent autocommit, failure and recovery use the actual engine.

use super::*;
use crate::bolt::{Closed, CommitSeq, Dechunker, Request, Server, bookmark, get};
use crate::recovery::tests::{WireIo, fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;
use fgdb_warden::CapabilityToken;
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
        id: 0xb017_c0,
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

fn replies(io: &mut Io) -> Vec<Value> {
    let mut decoder = Dechunker::new(1 << 20);
    decoder.push(&std::mem::take(&mut io.out));
    let mut replies = Vec::new();
    while let Some(message) = decoder.next_message().unwrap() {
        replies.push(fgdb_bolt::packstream::decode(&message).unwrap());
    }
    replies
}

fn summary(replies: &[Value], expected_tag: u8) -> &Map {
    let Some(Value::Struct { tag, fields }) = replies.last() else {
        panic!("missing summary: {replies:?}");
    };
    assert_eq!(*tag, expected_tag, "{replies:?}");
    let [Value::Map(metadata)] = fields.as_slice() else {
        panic!("expected metadata: {fields:?}");
    };
    metadata
}

fn records(replies: &[Value]) -> Vec<Vec<Value>> {
    replies
        .iter()
        .filter_map(|reply| match reply {
            Value::Struct { tag: 0x71, fields } => match fields.as_slice() {
                [Value::List(row)] => Some(row.clone()),
                _ => panic!("invalid RECORD"),
            },
            _ => None,
        })
        .collect()
}

fn buffer(cx: &Cx, client: &mut Connection<'_>, values: &[i64]) -> i64 {
    let transaction = client.transaction.as_ref().unwrap();
    let pending = Pending {
        rows: values.iter().map(|value| vec![Value::Int(*value)]).collect(),
        database: transaction.name.clone(),
        db: transaction.db,
        seq: Some(transaction.seq),
        generation: transaction.session.generation(),
        write: None,
    };
    client.buffer_result(cx, pending).unwrap().unwrap()
}

async fn send(cx: &Cx, client: &mut Connection<'_>, io: &mut Io, request: Request) -> Vec<Value> {
    assert!(!client.handle(cx, io, request).await);
    replies(io)
}

#[test]
fn partial_pull_and_discard_interleave_without_crossing_result_boundaries() {
    let ((), report) = run_async_under_lab(0xb017_c001, |root| async move {
        let (server, token, _) = served(&root, "bolt-multiple-cursors").await;
        let mut client = connection(&server, token);
        client.begin(&root, &extra()).await.unwrap();
        let seq = client.transaction.as_ref().unwrap().seq;
        assert_eq!(buffer(&root, &mut client, &[1, 2, 3]), 0);
        assert_eq!(buffer(&root, &mut client, &[10, 11]), 1);
        let mut io = io();
        let first = send(&root, &mut client, &mut io, Request::Pull { n: 1, qid: 0 }).await;
        assert_eq!(records(&first), vec![vec![Value::Int(1)]]);
        assert_eq!(get(summary(&first, 0x70), "has_more"), Some(&Value::Bool(true)));
        let skipped = send(&root, &mut client, &mut io, Request::Discard { n: 1, qid: 1 }).await;
        assert!(records(&skipped).is_empty());
        assert_eq!(get(summary(&skipped, 0x70), "has_more"), Some(&Value::Bool(true)));
        let latest = send(&root, &mut client, &mut io, Request::Pull { n: -1, qid: -1 }).await;
        assert_eq!(records(&latest), vec![vec![Value::Int(11)]]);
        assert!(get(summary(&latest, 0x70), "bookmark").is_none());
        let older = send(&root, &mut client, &mut io, Request::Pull { n: i64::MAX, qid: 0 }).await;
        assert_eq!(records(&older), vec![vec![Value::Int(2)], vec![Value::Int(3)]]);
        assert!(get(summary(&older, 0x70), "has_more").is_none());
        let committed = send(&root, &mut client, &mut io, Request::Commit).await;
        assert_eq!(
            get(summary(&committed, 0x70), "bookmark").and_then(Value::as_str),
            Some(bookmark("test", seq).as_str())
        );
        assert!(client.transaction.is_none());
        assert!(client.pending.is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_graph_and_schema_runs_share_one_monotonic_id_space() {
    let ((), report) = run_async_under_lab(0xb017_c002, |root| async move {
        let (server, token, _) = served(&root, "bolt-schema-cursor-ids").await;
        let mut client = connection(&server, token);
        client.begin(&root, &extra()).await.unwrap();
        for (id, query) in [
            (0, "MATCH (n) RETURN n"),
            (1, "CALL db.labels()"),
            (2, "MATCH (n) RETURN n"),
        ] {
            let metadata = client.run(&root, query, &vec![], &extra()).await.unwrap();
            assert_eq!(get(&metadata, "qid"), Some(&Value::Int(id)));
        }
        let mut io = io();
        for qid in [1, -1, 0] {
            let done = send(&root, &mut client, &mut io, Request::Discard { n: -1, qid }).await;
            assert!(records(&done).is_empty());
            summary(&done, 0x70);
        }
        let committed = send(&root, &mut client, &mut io, Request::Commit).await;
        summary(&committed, 0x70);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn consumed_latest_id_does_not_alias_an_older_live_cursor() {
    let ((), report) = run_async_under_lab(0xb017_c003, |root| async move {
        let (server, token, _) = served(&root, "bolt-stale-last-cursor").await;
        let mut client = connection(&server, token);
        client.begin(&root, &extra()).await.unwrap();
        buffer(&root, &mut client, &[7]);
        buffer(&root, &mut client, &[8]);
        let mut io = io();
        let done = send(&root, &mut client, &mut io, Request::Discard { n: -1, qid: -1 }).await;
        summary(&done, 0x70);
        let refused = send(&root, &mut client, &mut io, Request::Pull { n: -1, qid: -1 }).await;
        summary(&refused, 0x7f);
        assert!(records(&refused).is_empty());
        assert!(client.failed);
        assert!(client.transaction.is_none());
        assert!(client.pending.is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_snapshot_does_not_advance_when_another_connection_commits() {
    let ((), report) = run_async_under_lab(0xb017_c004, |root| async move {
        let (server, token, _) = served(&root, "bolt-cursors-pinned-snapshot").await;
        let mut reader = connection(&server, token.clone());
        let mut writer = connection(&server, token);
        reader.begin(&root, &extra()).await.unwrap();
        assert_eq!(reader.transaction.as_ref().unwrap().seq, CommitSeq(0));
        reader.run(&root, "MATCH (n) RETURN n", &vec![], &extra()).await.unwrap();
        writer.run(&root, "CREATE (n)", &vec![], &extra()).await.unwrap();
        let mut write_io = io();
        let written = writer.pull(&root, &mut write_io, -1, true).await.unwrap();
        assert_eq!(get(&written, "bookmark").and_then(Value::as_str), Some("fgdb:test:1"));
        let next = reader.run(&root, "MATCH (n) RETURN n", &vec![], &extra()).await.unwrap();
        assert_eq!(get(&next, "qid"), Some(&Value::Int(1)));
        let mut read_io = io();
        for qid in [1, 0] {
            let rows = send(&root, &mut reader, &mut read_io, Request::Pull { n: -1, qid }).await;
            assert!(records(&rows).is_empty());
            summary(&rows, 0x70);
        }
        let committed = send(&root, &mut reader, &mut read_io, Request::Commit).await;
        assert_eq!(
            get(summary(&committed, 0x70), "bookmark").and_then(Value::as_str),
            Some("fgdb:test:0")
        );
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn commit_with_unread_results_fails_and_reset_releases_the_failed_transaction() {
    let ((), report) = run_async_under_lab(0xb017_c005, |root| async move {
        let (server, token, _) = served(&root, "bolt-commit-unread").await;
        let mut client = connection(&server, token);
        client.begin(&root, &extra()).await.unwrap();
        buffer(&root, &mut client, &[1]);
        buffer(&root, &mut client, &[2]);
        let mut io = io();
        let refused = send(&root, &mut client, &mut io, Request::Commit).await;
        assert!(get(summary(&refused, 0x7f), "bookmark").is_none());
        assert!(client.failed);
        assert!(client.transaction.is_none());
        assert!(client.pending.is_none());
        for request in [
            Request::Commit,
            Request::Rollback,
            Request::Pull { n: -1, qid: 0 },
            Request::Discard { n: -1, qid: -1 },
            Request::Begin { extra: extra() },
            Request::Run { query: "CREATE (n)".into(), parameters: vec![], extra: extra() },
        ] {
            let ignored = send(&root, &mut client, &mut io, request).await;
            assert_eq!(ignored, vec![Value::Struct { tag: 0x7e, fields: vec![] }]);
        }
        let reset = send(&root, &mut client, &mut io, Request::Reset).await;
        summary(&reset, 0x70);
        assert!(!client.failed);
        client.begin(&root, &extra()).await.unwrap();
        assert_eq!(buffer(&root, &mut client, &[3]), 0);
        let rolled_back = send(&root, &mut client, &mut io, Request::Rollback).await;
        summary(&rolled_back, 0x70);
        assert!(client.transaction.is_none());
        assert_eq!(server.databases["test"].db.read(&root).await.unwrap().frontier().unwrap(), CommitSeq(0));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn malformed_read_and_write_windows_do_not_consume_or_publish_anything() {
    let ((), report) = run_async_under_lab(0xb017_c006, |root| async move {
        let (server, token, _) = served(&root, "bolt-invalid-cursor-windows").await;
        for query in ["MATCH (n) RETURN n", "CREATE (n)"] {
            for request in [
                Request::Pull { n: 0, qid: -1 },
                Request::Discard { n: -2, qid: -1 },
                Request::Pull { n: -1, qid: 0 },
                Request::Discard { n: -1, qid: i64::MAX },
                Request::Pull { n: 1, qid: -2 },
            ] {
                let mut client = connection(&server, token.clone());
                client.run(&root, query, &vec![], &extra()).await.unwrap();
                let mut io = io();
                let refused = send(&root, &mut client, &mut io, request).await;
                summary(&refused, 0x7f);
                assert!(records(&refused).is_empty());
                assert!(client.failed);
                assert!(client.pending.is_none());
            }
        }
        assert_eq!(server.databases["test"].db.read(&root).await.unwrap().frontier().unwrap(), CommitSeq(0));
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn rollback_reset_and_goodbye_drop_every_outstanding_result() {
    let ((), report) = run_async_under_lab(0xb017_c007, |root| async move {
        let (server, token, _) = served(&root, "bolt-cursor-terminal-cleanup").await;
        for request in [Request::Rollback, Request::Reset, Request::Goodbye] {
            let mut client = connection(&server, token.clone());
            client.begin(&root, &extra()).await.unwrap();
            buffer(&root, &mut client, &[1]);
            buffer(&root, &mut client, &[2]);
            let goodbye = matches!(request, Request::Goodbye);
            let mut io = io();
            assert_eq!(client.handle(&root, &mut io, request).await, goodbye);
            assert!(client.transaction.is_none());
            assert!(client.pending.is_none());
            assert!(!client.failed);
            if goodbye {
                assert!(io.out.is_empty());
            } else {
                summary(&replies(&mut io), 0x70);
            }
        }
        let mut unauthenticated = connection(&server, token);
        unauthenticated.token = None;
        let mut io = io();
        assert!(unauthenticated.handle(&root, &mut io, Request::Reset).await);
        summary(&replies(&mut io), 0x7f);
        assert!(unauthenticated.token.is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn recovery_fences_an_explicitly_selected_older_cursor_and_drops_all_results() {
    let ((), report) = run_async_under_lab(0xb017_c008, |root| async move {
        let (server, token, _) = served(&root, "bolt-old-cursor-recovery-fence").await;
        let mut client = connection(&server, token);
        client.begin(&root, &extra()).await.unwrap();
        buffer(&root, &mut client, &[1]);
        buffer(&root, &mut client, &[2]);
        let db = &server.databases["test"];
        fail_after_marker(&root, db, 61).await;
        settled(db).await;
        let mut io = io();
        let refused = send(&root, &mut client, &mut io, Request::Pull { n: -1, qid: 0 }).await;
        assert_eq!(
            get(summary(&refused, 0x7f), "code").and_then(Value::as_str),
            Some("Neo.TransientError.General.DatabaseUnavailable")
        );
        assert!(records(&refused).is_empty());
        assert!(client.transaction.is_none());
        assert!(client.pending.is_none());
        assert!(client.output_generation.is_none());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_partial_delivery_cannot_reassign_its_id_or_restart_the_wire() {
    let ((), report) = run_async_under_lab(0xb017_c009, |root| async move {
        let (server, token, _) = served(&root, "bolt-cursor-delivery-drop").await;
        let mut client = connection(&server, token);
        client.begin(&root, &extra()).await.unwrap();
        let values: Vec<_> = (0..512).collect();
        buffer(&root, &mut client, &values);
        buffer(&root, &mut client, &[999]);
        let (stream, wire) = WireIo::new([0], false);
        let mut io = Io {
            stream: Box::new(stream),
            local: None,
            dechunker: Dechunker::new(4096),
            out: Vec::new(),
            failed: false,
        };
        let mut delivery = Box::pin(client.pull_selected(&root, &mut io, -1, 0, false));
        assert!(delivery.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        drop(delivery);
        assert!(io.failed);
        assert!(client.pending.is_some());
        assert!(client.pull_selected(&root, &mut io, -1, 1, false).await.is_err());
        assert!(wire.bytes().is_empty());
        assert!(!client.handle(&root, &mut io, Request::Reset).await);
        assert!(client.pending.is_none());
        assert!(client.transaction.is_none());
        assert_eq!(io.flush(&root, None, None).await, Err(Closed));
        assert!(wire.bytes().is_empty());
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
