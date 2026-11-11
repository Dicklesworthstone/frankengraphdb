//! Exercise the production Connection and real Chronicle-backed server.
//! Synthetic scalar rows isolate delivery-window laws; native MATCH, schema
//! RUN, concurrent autocommit, failure and recovery use the actual engine.

use super::*;
use crate::bolt::{Closed, CommitSeq, Dechunker, Request, Server, bookmark, get};
use crate::recovery::tests::{WireIo, fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;
use fgdb_warden::{CapabilityToken, Grant, QueryLimits, Rights, Scope};
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
        id: 0x00b0_17c0,
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

fn read_extra() -> Map {
    vec![
        ("db".into(), Value::string("test")),
        ("mode".into(), Value::string("r")),
    ]
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
        rows: values
            .iter()
            .map(|value| vec![Value::Int(*value)])
            .collect(),
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
        client.begin(&root, &read_extra()).await.unwrap();
        let seq = client.transaction.as_ref().unwrap().seq;
        assert_eq!(buffer(&root, &mut client, &[1, 2, 3]), 0);
        assert_eq!(buffer(&root, &mut client, &[10, 11]), 1);
        let mut io = io();
        let first = send(&root, &mut client, &mut io, Request::Pull { n: 1, qid: 0 }).await;
        assert_eq!(records(&first), vec![vec![Value::Int(1)]]);
        assert_eq!(
            get(summary(&first, 0x70), "has_more"),
            Some(&Value::Bool(true))
        );
        let skipped = send(
            &root,
            &mut client,
            &mut io,
            Request::Discard { n: 1, qid: 1 },
        )
        .await;
        assert!(records(&skipped).is_empty());
        assert_eq!(
            get(summary(&skipped, 0x70), "has_more"),
            Some(&Value::Bool(true))
        );
        let latest = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull { n: -1, qid: -1 },
        )
        .await;
        assert_eq!(records(&latest), vec![vec![Value::Int(11)]]);
        assert!(get(summary(&latest, 0x70), "bookmark").is_none());
        let older = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull {
                n: i64::MAX,
                qid: 0,
            },
        )
        .await;
        assert_eq!(
            records(&older),
            vec![vec![Value::Int(2)], vec![Value::Int(3)]]
        );
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
        client.begin(&root, &read_extra()).await.unwrap();
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
        client.begin(&root, &read_extra()).await.unwrap();
        buffer(&root, &mut client, &[7]);
        buffer(&root, &mut client, &[8]);
        let mut io = io();
        let done = send(
            &root,
            &mut client,
            &mut io,
            Request::Discard { n: -1, qid: -1 },
        )
        .await;
        summary(&done, 0x70);
        let refused = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull { n: -1, qid: -1 },
        )
        .await;
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
        reader.begin(&root, &read_extra()).await.unwrap();
        assert_eq!(reader.transaction.as_ref().unwrap().seq, CommitSeq(0));
        reader
            .run(&root, "MATCH (n) RETURN n", &vec![], &extra())
            .await
            .unwrap();
        writer
            .run(&root, "CREATE (n)", &vec![], &extra())
            .await
            .unwrap();
        let mut write_io = io();
        let written = writer.pull(&root, &mut write_io, -1, true).await.unwrap();
        assert_eq!(
            get(&written, "bookmark").and_then(Value::as_str),
            Some("fgdb:test:1")
        );
        let next = reader
            .run(&root, "MATCH (n) RETURN n", &vec![], &extra())
            .await
            .unwrap();
        assert_eq!(get(&next, "qid"), Some(&Value::Int(1)));
        let mut read_io = io();
        for qid in [1, 0] {
            let rows = send(
                &root,
                &mut reader,
                &mut read_io,
                Request::Pull { n: -1, qid },
            )
            .await;
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
        client.begin(&root, &read_extra()).await.unwrap();
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
            Request::Begin {
                extra: read_extra(),
            },
            Request::Run {
                query: "CREATE (n)".into(),
                parameters: vec![],
                extra: extra(),
            },
        ] {
            let ignored = send(&root, &mut client, &mut io, request).await;
            assert_eq!(
                ignored,
                vec![Value::Struct {
                    tag: 0x7e,
                    fields: vec![]
                }]
            );
        }
        let reset = send(&root, &mut client, &mut io, Request::Reset).await;
        summary(&reset, 0x70);
        assert!(!client.failed);
        client.begin(&root, &read_extra()).await.unwrap();
        assert_eq!(buffer(&root, &mut client, &[3]), 0);
        let rolled_back = send(&root, &mut client, &mut io, Request::Rollback).await;
        summary(&rolled_back, 0x70);
        assert!(client.transaction.is_none());
        assert_eq!(
            server.databases["test"]
                .db
                .read(&root)
                .await
                .unwrap()
                .frontier()
                .unwrap(),
            CommitSeq(0)
        );
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
                Request::Discard {
                    n: -1,
                    qid: i64::MAX,
                },
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
        assert_eq!(
            server.databases["test"]
                .db
                .read(&root)
                .await
                .unwrap()
                .frontier()
                .unwrap(),
            CommitSeq(0)
        );
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
            client.begin(&root, &read_extra()).await.unwrap();
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
        client.begin(&root, &read_extra()).await.unwrap();
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
        client.begin(&root, &read_extra()).await.unwrap();
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
        assert!(
            delivery
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(delivery);
        assert!(io.failed);
        assert!(client.pending.is_some());
        assert!(
            client
                .pull_selected(&root, &mut io, -1, 1, false)
                .await
                .is_err()
        );
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

async fn explicit_fixture(cx: &Cx, name: &str) -> (Server, CapabilityToken) {
    let (mut server, token, _) = served(cx, name).await;
    let db = std::sync::Arc::get_mut(server.databases.get_mut("test").unwrap()).unwrap();
    for (kind, name, id) in [
        (fgdb_gql::GraphSymbolKind::Label, "Visible", 1),
        (fgdb_gql::GraphSymbolKind::Relation, "R", 1),
        (fgdb_gql::GraphSymbolKind::Property, "p", 1),
        (fgdb_gql::GraphSymbolKind::Property, "secret", 2),
    ] {
        db.symbols.bind(kind, name, id).unwrap();
    }
    (server, token)
}

fn explicit_token(
    db: &crate::Served,
    rights: Rights,
    rows: u64,
    properties: Scope<fgdb_delta_types::PropertyKeyId>,
) -> CapabilityToken {
    db.authority
        .issue_at(
            &Grant {
                branch: crate::TRUNK.into(),
                labels: Scope::All,
                relations: Scope::All,
                properties,
                rights,
                limits: QueryLimits {
                    max_nodes: 1_000_000,
                    max_work: 100_000_000,
                    max_rows: rows,
                },
                expires_at_ms: u64::MAX / 2,
            },
            crate::unix_millis(),
        )
        .unwrap()
}

async fn stage_explicit(
    cx: &Cx,
    client: &mut Connection<'_>,
    io: &mut Io,
    query: &str,
    parameters: Map,
) -> i64 {
    let replies = send(
        cx,
        client,
        io,
        Request::Run {
            query: query.into(),
            parameters,
            extra: Vec::new(),
        },
    )
    .await;
    let metadata = summary(&replies, 0x70);
    assert!(get(metadata, "bookmark").is_none());
    match get(metadata, "qid") {
        Some(Value::Int(qid)) => *qid,
        value => panic!("missing explicit statement qid: {value:?}"),
    }
}

async fn assert_committed_vertices(cx: &Cx, db: &crate::Served, seq: u64, count: usize) {
    let guard = db.db.read(cx).await.unwrap();
    assert_eq!(guard.frontier().unwrap(), CommitSeq(seq));
    assert_eq!(guard.vertices().unwrap().len(), count);
}

#[test]
fn explicit_write_transactions_stage_own_reads_and_publish_once_on_commit() {
    let ((), report) = run_async_under_lab(0xb017_e001, |root| async move {
        let (server, token) = explicit_fixture(&root, "bolt-explicit-own-writes").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        let mut io = io();
        let opened = send(
            &root,
            &mut client,
            &mut io,
            Request::Begin { extra: extra() },
        )
        .await;
        summary(&opened, 0x70);
        let created = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "CREATE (n:Visible $props) RETURN n",
            vec![(
                "props".into(),
                Value::Map(vec![("p".into(), Value::Int(7))]),
            )],
        )
        .await;
        let updated = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "MATCH (n:Visible) SET n.p=8 RETURN n.p AS p",
            vec![],
        )
        .await;
        let read = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "MATCH (n:Visible) RETURN n.p AS p",
            vec![],
        )
        .await;
        assert_committed_vertices(&root, db, 0, 0).await;
        let own = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull { n: -1, qid: read },
        )
        .await;
        assert_eq!(records(&own), vec![vec![Value::Int(8)]]);
        assert!(get(summary(&own, 0x70), "bookmark").is_none());
        assert_eq!(
            get(summary(&own, 0x70), "fgdb_transaction_local"),
            Some(&Value::Bool(true))
        );
        let original = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull {
                n: -1,
                qid: created,
            },
        )
        .await;
        let rows = records(&original);
        let Value::Struct { tag: 0x4e, fields } = &rows[0][0] else {
            panic!("expected the created node");
        };
        let Value::Map(properties) = &fields[2] else {
            panic!("expected node properties");
        };
        // A later SET cannot rewrite an older outstanding cursor's metadata.
        assert_eq!(get(properties, "p"), Some(&Value::Int(7)));
        let Value::Map(stats) = get(summary(&original, 0x70), "stats").unwrap() else {
            panic!("expected provisional creation counters");
        };
        assert_eq!(get(stats, "nodes-created"), Some(&Value::Int(1)));
        assert_eq!(get(stats, "contains-updates"), Some(&Value::Bool(true)));
        assert!(get(summary(&original, 0x70), "bookmark").is_none());
        let changed = send(
            &root,
            &mut client,
            &mut io,
            Request::Discard {
                n: -1,
                qid: updated,
            },
        )
        .await;
        summary(&changed, 0x70);
        assert_committed_vertices(&root, db, 0, 0).await;
        let committed = send(&root, &mut client, &mut io, Request::Commit).await;
        assert_eq!(
            get(summary(&committed, 0x70), "bookmark").and_then(Value::as_str),
            Some("fgdb:test:1")
        );
        assert!(client.transaction.is_none());
        assert_committed_vertices(&root, db, 1, 1).await;
        client
            .run(
                &root,
                "MATCH (n:Visible) RETURN n.p AS p",
                &vec![],
                &extra(),
            )
            .await
            .unwrap();
        let persisted = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull { n: -1, qid: -1 },
        )
        .await;
        assert_eq!(records(&persisted), vec![vec![Value::Int(8)]]);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_aggregates_read_staged_rows_and_freeze_collected_graph_metadata() {
    let ((), report) = run_async_under_lab(0xb017_e009, |root| async move {
        let (server, token) = explicit_fixture(&root, "bolt-explicit-aggregates").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        let mut io = io();
        client.begin(&root, &extra()).await.unwrap();
        let empty = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "MATCH (n:Visible) RETURN count(*) AS count, sum(n.p) AS total, avg(n.p) AS mean",
            vec![],
        )
        .await;
        let empty = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull { n: -1, qid: empty },
        )
        .await;
        assert_eq!(
            records(&empty),
            vec![vec![Value::Int(0), Value::Null, Value::Null]]
        );
        let created = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "CREATE (:Visible {p:1,secret:0}), (:Visible {p:2,secret:0}), (:Visible {p:4,secret:1})",
            vec![],
        )
        .await;
        for (query, expected) in [
            (
                "MATCH (create:Visible) RETURN count(create) AS count, sum(create.p) AS total, avg(create.p) AS mean",
                vec![vec![Value::Int(3), Value::Int(7), Value::Float(7.0 / 3.0)]],
            ),
            (
                "MATCH (n:Visible) RETURN sum(n.p) AS total, n.secret AS bucket, count(*) AS count, avg(n.p) AS mean ORDER BY bucket",
                vec![
                    vec![
                        Value::Int(3),
                        Value::Int(0),
                        Value::Int(2),
                        Value::Float(1.5),
                    ],
                    vec![
                        Value::Int(4),
                        Value::Int(1),
                        Value::Int(1),
                        Value::Float(4.0),
                    ],
                ],
            ),
            (
                "MATCH (n:Visible) WITH n.p AS p WHERE p >= 2 RETURN avg(p) AS mean, count(*) AS count, sum(p) AS total",
                vec![vec![Value::Float(3.0), Value::Int(2), Value::Int(6)]],
            ),
            (
                "CALL db.labels() YIELD label RETURN count(*) AS count",
                vec![vec![Value::Int(1)]],
            ),
        ] {
            let qid = stage_explicit(&root, &mut client, &mut io, query, vec![]).await;
            let response = send(&root, &mut client, &mut io, Request::Pull { n: -1, qid }).await;
            assert_eq!(records(&response), expected, "{query}");
            let metadata = summary(&response, 0x70);
            assert!(get(metadata, "bookmark").is_none());
            assert_eq!(get(metadata, "type"), Some(&Value::string("r")));
            assert_eq!(
                get(metadata, "fgdb_transaction_local"),
                Some(&Value::Bool(true))
            );
            assert_committed_vertices(&root, db, 0, 0).await;
        }
        let collected = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "MATCH (n:Visible) WITH n ORDER BY n.p RETURN collect(n) AS nodes",
            vec![],
        )
        .await;
        let changed = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "MATCH (n:Visible) SET n.p=n.p+10",
            vec![],
        )
        .await;
        let response = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull {
                n: -1,
                qid: collected,
            },
        )
        .await;
        let rows = records(&response);
        let [row] = rows.as_slice() else {
            panic!("one aggregate group: {rows:?}");
        };
        let [Value::List(nodes)] = row.as_slice() else {
            panic!("collected node values: {row:?}");
        };
        assert_eq!(nodes.len(), 3);
        for (node, expected) in nodes.iter().zip([1, 2, 4]) {
            let Value::Struct { tag: 0x4e, fields } = node else {
                panic!("a hydrated node: {node:?}");
            };
            let Value::Map(properties) = &fields[2] else {
                panic!("node properties: {fields:?}");
            };
            // Hydration belongs to the aggregate statement, not this later PULL.
            assert_eq!(get(properties, "p"), Some(&Value::Int(expected)));
        }
        for qid in [created, changed] {
            let discarded =
                send(&root, &mut client, &mut io, Request::Discard { n: -1, qid }).await;
            assert!(records(&discarded).is_empty());
            assert!(get(summary(&discarded, 0x70), "bookmark").is_none());
        }
        assert_committed_vertices(&root, db, 0, 0).await;
        let rolled_back = send(&root, &mut client, &mut io, Request::Rollback).await;
        summary(&rolled_back, 0x70);
        assert!(client.transaction.is_none());
        assert_committed_vertices(&root, db, 0, 0).await;
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_write_terminal_paths_discard_the_entire_unpublished_prefix() {
    let ((), report) = run_async_under_lab(0xb017_e002, |root| async move {
        let (server, token) = explicit_fixture(&root, "bolt-explicit-abandonment").await;
        let db = &server.databases["test"];
        for terminal in 0..6 {
            let mut client = connection(&server, token.clone());
            let mut io = io();
            client.begin(&root, &extra()).await.unwrap();
            stage_explicit(
                &root,
                &mut client,
                &mut io,
                "CREATE (n:Visible {p:1})",
                vec![],
            )
            .await;
            match terminal {
                0 => {
                    let response = send(&root, &mut client, &mut io, Request::Rollback).await;
                    summary(&response, 0x70);
                }
                1 => {
                    let response = send(&root, &mut client, &mut io, Request::Reset).await;
                    summary(&response, 0x70);
                }
                2 => {
                    assert!(client.handle(&root, &mut io, Request::Goodbye).await);
                }
                3 => {
                    // Disconnect drops the same owned workspace as GOODBYE.
                    drop(client);
                    assert_committed_vertices(&root, db, 0, 0).await;
                    continue;
                }
                _ => {
                    let response = send(
                        &root,
                        &mut client,
                        &mut io,
                        Request::Run {
                            query: match terminal {
                                4 => "MATCH (n) RETURN id(n)",
                                // Algorithm procedures still have no staged
                                // graph host and must never run as autocommit.
                                _ => "CALL fnx.pagerank() YIELD node RETURN node",
                            }
                            .into(),
                            parameters: vec![],
                            extra: vec![],
                        },
                    )
                    .await;
                    summary(&response, 0x7f);
                    assert!(records(&response).is_empty());
                    assert!(client.failed);
                }
            }
            assert!(client.transaction.is_none());
            assert!(client.pending.is_none());
            assert_committed_vertices(&root, db, 0, 0).await;
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_write_scopes_mask_graph_output_and_abort_forbidden_followup() {
    let ((), report) = run_async_under_lab(0xb017_e003, |root| async move {
        let (server, all) = explicit_fixture(&root, "bolt-explicit-scopes").await;
        let db = &server.databases["test"];
        let mut writer = connection(&server, all);
        writer
            .run(
                &root,
                "CREATE (n:Visible {p:1,secret:9})",
                &vec![],
                &extra(),
            )
            .await
            .unwrap();
        writer.pull(&root, &mut io(), -1, true).await.unwrap();
        let token = explicit_token(
            db,
            Rights::ReadWrite,
            100,
            Scope::only([fgdb_delta_types::PropertyKeyId(1)]),
        );
        let mut client = connection(&server, token);
        let mut io = io();
        client.begin(&root, &extra()).await.unwrap();
        let node = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "MATCH (n:Visible) SET n.p=2 RETURN n",
            vec![],
        )
        .await;
        let response = send(
            &root,
            &mut client,
            &mut io,
            Request::Pull { n: -1, qid: node },
        )
        .await;
        let rows = records(&response);
        let Value::Struct { tag: 0x4e, fields } = &rows[0][0] else {
            panic!("expected scoped node");
        };
        let Value::Map(properties) = &fields[2] else {
            panic!("expected scoped properties");
        };
        assert_eq!(properties, &vec![("p".into(), Value::Int(2))]);
        let denied = send(
            &root,
            &mut client,
            &mut io,
            Request::Run {
                query: "MATCH (n:Visible) SET n.secret=10".into(),
                parameters: vec![],
                extra: vec![],
            },
        )
        .await;
        assert_eq!(
            get(summary(&denied, 0x7f), "code").and_then(Value::as_str),
            Some("Neo.ClientError.Security.Forbidden")
        );
        assert!(records(&denied).is_empty());
        assert!(client.failed);
        assert!(client.transaction.is_none());
        assert_committed_vertices(&root, db, 1, 1).await;
        // The allowed earlier SET was provisional and was discarded too.
        writer
            .run(&root, "MATCH (n:Visible) RETURN n.p", &vec![], &extra())
            .await
            .unwrap();
        assert_eq!(
            writer.pending.as_ref().unwrap().rows,
            std::collections::VecDeque::from([vec![Value::Int(1)]])
        );
        writer.pull(&root, &mut io, -1, true).await.unwrap();
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_read_mode_and_read_only_capabilities_never_gain_write_authority() {
    let ((), report) = run_async_under_lab(0xb017_e004, |root| async move {
        let (server, all) = explicit_fixture(&root, "bolt-explicit-read-authority").await;
        let db = &server.databases["test"];
        let read = explicit_token(db, Rights::Read, 100, Scope::All);
        for (token, options) in [(all, read_extra()), (read, extra())] {
            let mut client = connection(&server, token);
            let mut io = io();
            client.begin(&root, &options).await.unwrap();
            let qid =
                stage_explicit(&root, &mut client, &mut io, "RETURN 7 AS marker", vec![]).await;
            let result = send(&root, &mut client, &mut io, Request::Pull { n: -1, qid }).await;
            assert_eq!(records(&result), vec![vec![Value::Int(7)]]);
            let refused = send(
                &root,
                &mut client,
                &mut io,
                Request::Run {
                    query: "CREATE (n)".into(),
                    parameters: vec![],
                    extra: vec![],
                },
            )
            .await;
            assert_eq!(
                get(summary(&refused, 0x7f), "code").and_then(Value::as_str),
                Some("Neo.ClientError.Statement.AccessMode")
            );
            assert!(client.transaction.is_none());
            assert_committed_vertices(&root, db, 0, 0).await;
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_write_transactions_honor_the_read_only_run_ceiling() {
    let ((), report) = run_async_under_lab(0xb017_e005, |root| async move {
        let (server, token) = explicit_fixture(&root, "bolt-explicit-run-read-mode").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token);
        let mut io = io();
        client.begin(&root, &extra()).await.unwrap();
        let prefix =
            stage_explicit(&root, &mut client, &mut io, "CREATE (n:Visible)", vec![]).await;
        let read = send(
            &root,
            &mut client,
            &mut io,
            Request::Run {
                query: "MATCH (n:Visible) RETURN 7 AS marker".into(),
                parameters: vec![],
                extra: vec![("mode".into(), Value::string("r"))],
            },
        )
        .await;
        let Some(Value::Int(qid)) = get(summary(&read, 0x70), "qid") else {
            panic!("missing read qid");
        };
        let qid = *qid;
        let rows = send(&root, &mut client, &mut io, Request::Pull { n: -1, qid }).await;
        assert_eq!(records(&rows), vec![vec![Value::Int(7)]]);
        let discarded = send(
            &root,
            &mut client,
            &mut io,
            Request::Discard { n: -1, qid: prefix },
        )
        .await;
        summary(&discarded, 0x70);
        let refused = send(
            &root,
            &mut client,
            &mut io,
            Request::Run {
                query: "CREATE (n)".into(),
                parameters: vec![],
                extra: vec![("mode".into(), Value::string("r"))],
            },
        )
        .await;
        assert_eq!(
            get(summary(&refused, 0x7f), "code").and_then(Value::as_str),
            Some("Neo.ClientError.Statement.AccessMode")
        );
        assert!(client.transaction.is_none());
        assert_committed_vertices(&root, db, 0, 0).await;
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_transaction_cumulative_limits_and_late_output_failure_abort_prefix() {
    let ((), report) = run_async_under_lab(0xb017_e006, |root| async move {
        for case in 0..3 {
            let (mut server, token) =
                explicit_fixture(&root, &format!("bolt-explicit-cumulative-{case}")).await;
            if case == 1 {
                std::sync::Arc::get_mut(server.databases.get_mut("test").unwrap())
                    .unwrap()
                    .max_statements = 1;
            }
            if case == 2 {
                server.limits.max_frame_len = 256;
            }
            let db = &server.databases["test"];
            let token = if case == 0 {
                explicit_token(db, Rights::ReadWrite, 1, Scope::All)
            } else {
                token
            };
            let mut client = connection(&server, token);
            let mut io = io();
            client.begin(&root, &extra()).await.unwrap();
            let prefix = stage_explicit(
                &root,
                &mut client,
                &mut io,
                "CREATE (n:Visible {p:1})",
                vec![],
            )
            .await;
            let discarded = send(
                &root,
                &mut client,
                &mut io,
                Request::Discard { n: -1, qid: prefix },
            )
            .await;
            summary(&discarded, 0x70);
            if case == 0 {
                let first = stage_explicit(
                    &root,
                    &mut client,
                    &mut io,
                    "MATCH (n:Visible) RETURN count(*) AS value",
                    vec![],
                )
                .await;
                let consumed = send(
                    &root,
                    &mut client,
                    &mut io,
                    Request::Pull { n: -1, qid: first },
                )
                .await;
                assert_eq!(records(&consumed), vec![vec![Value::Int(1)]]);
            }
            let (query, parameters) = match case {
                0 => ("MATCH (n:Visible) RETURN count(*) AS value", vec![]),
                1 => ("CREATE (n)", vec![]),
                // A small request whose computed output exceeds the frame.
                _ => ("RETURN range(1,512) AS value", vec![]),
            };
            let refused = send(
                &root,
                &mut client,
                &mut io,
                Request::Run {
                    query: query.into(),
                    parameters,
                    extra: vec![],
                },
            )
            .await;
            let failure = summary(&refused, 0x7f);
            assert_eq!(
                get(failure, "code").and_then(Value::as_str),
                Some(if case == 1 {
                    "Neo.DatabaseError.Statement.ExecutionFailed"
                } else {
                    "Neo.ClientError.Statement.ExecutionFailed"
                })
            );
            if case == 2 {
                assert!(
                    get(failure, "message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains("Bolt write RETURN exceeds"))
                );
            }
            assert!(records(&refused).is_empty());
            assert!(client.failed);
            assert!(client.transaction.is_none());
            assert_committed_vertices(&root, db, 0, 0).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_preparation_refusals_abort_the_prefix_before_any_result_escapes() {
    let ((), report) = run_async_under_lab(0xb017_e00a, |root| async move {
        let (server, all) = explicit_fixture(&root, "bolt-explicit-preparation").await;
        let db = &server.databases["test"];
        let metered_text = format!("/*{}*/ RETURN 9 AS value", "x".repeat(8192));
        // The statement is valid under ample work. Its comment still consumes
        // the original ingress-byte allowance before the compiler runs.
        let mut control = connection(&server, all.clone());
        let mut io = io();
        control.begin(&root, &extra()).await.unwrap();
        let qid = stage_explicit(&root, &mut control, &mut io, &metered_text, vec![]).await;
        let accepted = send(&root, &mut control, &mut io, Request::Pull { n: -1, qid }).await;
        assert_eq!(records(&accepted), vec![vec![Value::Int(9)]]);
        summary(
            &send(&root, &mut control, &mut io, Request::Rollback).await,
            0x70,
        );
        let work_limited = db
            .authority
            .issue_at(
                &Grant {
                    branch: crate::TRUNK.into(),
                    labels: Scope::All,
                    relations: Scope::All,
                    properties: Scope::All,
                    rights: Rights::ReadWrite,
                    limits: QueryLimits {
                        max_nodes: 1_000_000,
                        max_work: 8192,
                        max_rows: 100,
                    },
                    expires_at_ms: u64::MAX / 2,
                },
                crate::unix_millis(),
            )
            .unwrap();
        let scoped = explicit_token(
            db,
            Rights::ReadWrite,
            100,
            Scope::only([fgdb_delta_types::PropertyKeyId(1)]),
        );
        for (token, query, parameters, expected_code) in [
            (
                work_limited,
                metered_text.as_str(),
                vec![],
                "Neo.ClientError.Statement.ExecutionFailed",
            ),
            (
                scoped,
                "MATCH (n:Visible) RETURN sum(n.secret) AS hidden",
                vec![],
                "Neo.ClientError.Security.Forbidden",
            ),
            (
                all,
                "MATCH (n:Visible) RETURN 'Ann' =~ $pattern AS matches",
                vec![("pattern".into(), Value::string("["))],
                "Neo.ClientError.Statement.SyntaxError",
            ),
        ] {
            let mut client = connection(&server, token);
            client.begin(&root, &extra()).await.unwrap();
            // Leave this successful result outstanding: failure must release
            // both its cursor and the staged write it describes.
            stage_explicit(
                &root,
                &mut client,
                &mut io,
                "CREATE (n:Visible {p:1}) RETURN n.p",
                vec![],
            )
            .await;
            let refused = send(
                &root,
                &mut client,
                &mut io,
                Request::Run {
                    query: query.into(),
                    parameters,
                    extra: vec![],
                },
            )
            .await;
            assert_eq!(
                get(summary(&refused, 0x7f), "code").and_then(Value::as_str),
                Some(expected_code)
            );
            assert!(records(&refused).is_empty());
            assert!(client.failed);
            assert!(client.transaction.is_none());
            assert!(client.pending.is_none());
            let ignored = send(&root, &mut client, &mut io, Request::Commit).await;
            assert!(matches!(
                ignored.as_slice(),
                [Value::Struct { tag: 0x7e, .. }]
            ));
            assert_committed_vertices(&root, db, 0, 0).await;
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_write_owners_release_idle_locks_and_fail_on_stale_or_recovered_bases() {
    let ((), report) = run_async_under_lab(0xb017_e007, |root| async move {
        let (server, token) = explicit_fixture(&root, "bolt-explicit-stale-generation").await;
        let db = &server.databases["test"];
        let mut client = connection(&server, token.clone());
        let mut io = io();
        client.begin(&root, &extra()).await.unwrap();
        let prefix = stage_explicit(
            &root,
            &mut client,
            &mut io,
            "CREATE (n:Visible {p:1})",
            vec![],
        )
        .await;
        send(
            &root,
            &mut client,
            &mut io,
            Request::Discard { n: -1, qid: prefix },
        )
        .await;
        // This separate writer must run while the explicit owner is idle.
        let mut writer = connection(&server, token.clone());
        writer
            .run(&root, "CREATE (n)", &vec![], &extra())
            .await
            .unwrap();
        writer.pull(&root, &mut io, -1, true).await.unwrap();
        let refused = send(
            &root,
            &mut client,
            &mut io,
            Request::Run {
                query: "RETURN 7 AS marker".into(),
                parameters: vec![],
                extra: vec![],
            },
        )
        .await;
        summary(&refused, 0x7f);
        assert!(client.transaction.is_none());
        assert_committed_vertices(&root, db, 1, 1).await;
        let mut recovered = connection(&server, token);
        recovered.begin(&root, &extra()).await.unwrap();
        let prefix = stage_explicit(
            &root,
            &mut recovered,
            &mut io,
            "CREATE (n:Visible {p:2})",
            vec![],
        )
        .await;
        send(
            &root,
            &mut recovered,
            &mut io,
            Request::Discard { n: -1, qid: prefix },
        )
        .await;
        fail_after_marker(&root, db, 0xe007).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        ));
        let refused = send(&root, &mut recovered, &mut io, Request::Commit).await;
        summary(&refused, 0x7f);
        assert!(recovered.transaction.is_none());
        assert_committed_vertices(&root, db, 2, 2).await;
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancelled_explicit_statement_or_commit_wait_drops_the_native_owner() {
    let ((), report) = run_async_under_lab(0xb017_e008, |root| async move {
        let (server, token) = explicit_fixture(&root, "bolt-explicit-cancel-wait").await;
        let db = &server.databases["test"];
        for commit in [false, true] {
            let mut client = connection(&server, token.clone());
            let mut io = io();
            client.begin(&root, &extra()).await.unwrap();
            let prefix = stage_explicit(
                &root,
                &mut client,
                &mut io,
                "CREATE (n:Visible {p:1})",
                vec![],
            )
            .await;
            send(
                &root,
                &mut client,
                &mut io,
                Request::Discard { n: -1, qid: prefix },
            )
            .await;
            let held = db.db.write(&root).await.unwrap();
            if commit {
                let mut committing = Box::pin(client.commit_transaction(&root));
                assert!(
                    committing
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                drop(committing);
            } else {
                let parameters = vec![];
                let extra = vec![];
                let mut staging = Box::pin(client.run(&root, "CREATE (n)", &parameters, &extra));
                assert!(
                    staging
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                drop(staging);
            }
            assert!(client.transaction.is_none());
            assert!(client.pending.is_none());
            drop(held);
            assert_committed_vertices(&root, db, 0, 0).await;
        }
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
