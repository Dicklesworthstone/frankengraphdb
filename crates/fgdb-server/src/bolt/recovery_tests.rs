use super::*;
use crate::recovery::tests::{WakeCounter, WireIo, fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;
use std::future::Future;
use std::sync::Arc;

fn connection(server: &Server, token: CapabilityToken) -> Connection<'_> {
    Connection {
        server,
        token: Some(token),
        failed: false,
        pending: None,
        transaction: None,
        output_database: None,
        output_generation: None,
        id: 1,
    }
}

#[test]
fn retained_bolt_transactions_and_pending_results_stay_fenced_after_reopen() {
    let ((), report) = run_async_under_lab(0x79a0_0301, |root| async move {
        let (server, token, _) = served(&root, "bolt-retained-generation").await;
        let db = &server.databases["test"];
        let extra = vec![("db".into(), Value::string("test"))];
        let mut transaction = connection(&server, token.clone());
        let mut pending = connection(&server, token.clone());
        let mut commit = connection(&server, token.clone());
        transaction.begin(&root, &extra).await.unwrap();
        pending.begin(&root, &extra).await.unwrap();
        pending
            .run(&root, "MATCH (n) RETURN n", &vec![], &extra)
            .await
            .unwrap();
        commit.begin(&root, &extra).await.unwrap();
        fail_after_marker(&root, db, 41).await;
        assert!(matches!(
            settled(db).await,
            crate::DatabaseStatus::Ready { generation: 2 }
        ));
        let failure = transaction
            .run(&root, "MATCH (n) RETURN n", &vec![], &extra)
            .await
            .unwrap_err();
        assert_eq!(
            failure.code,
            "Neo.TransientError.General.DatabaseUnavailable"
        );
        for (connection, request) in [
            (&mut pending, Request::Pull { n: -1, qid: -1 }),
            (&mut commit, Request::Commit),
        ] {
            let (stream, wire) = WireIo::new([], false);
            let mut io = Io {
                stream: Box::new(stream),
                local: None,
                dechunker: Dechunker::new(4096),
                out: vec![],
                failed: false,
            };
            assert!(!connection.handle(&root, &mut io, request).await);
            assert!(connection.failed);
            assert!(connection.transaction.is_none());
            assert!(connection.pending.is_none());
            assert!(connection.output_generation.is_none());
            io.flush(
                &root,
                connection.output_authority(),
                connection.output_generation.as_ref(),
            )
            .await
            .unwrap();
            let mut decoder = Dechunker::new(4096);
            decoder.push(&wire.bytes());
            let message = decoder.next_message().unwrap().unwrap();
            let Value::Struct { tag, fields } = fgdb_bolt::packstream::decode(&message).unwrap()
            else {
                panic!("Bolt response must be structured")
            };
            assert_eq!(tag, 0x7f);
            let [Value::Map(metadata)] = fields.as_slice() else {
                panic!("FAILURE metadata")
            };
            assert_eq!(
                get(metadata, "code").and_then(Value::as_str),
                Some("Neo.TransientError.General.DatabaseUnavailable")
            );
        }
        let mut fresh = connection(&server, token);
        fresh
            .run(&root, "MATCH (n) RETURN n", &vec![], &extra)
            .await
            .unwrap();
        assert_eq!(fresh.pending.as_ref().unwrap().rows.len(), 1);
        server.join_database_workers(&root).await;
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn recovery_wakes_and_fences_every_bolt_physical_write_and_flush() {
    let ((), report) = run_async_under_lab(0x79a0_0302, |root| async move {
        for (name, writes, flush_pending) in [
            ("bolt-unwritten", vec![0], false),
            ("bolt-partial", vec![5, 0], false),
            ("bolt-flush", vec![], true),
        ] {
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let generation = db.db.generation().unwrap();
            let (mut io, wire) = WireIo::new(writes, flush_pending);
            let mut out = Vec::new();
            Response::Record(vec![Value::string("protected")]).frame(&mut out);
            let mut failed = false;
            let mut send = Box::pin(flush_generation_output(
                &mut io,
                &root,
                &mut out,
                &mut failed,
                Some((&db.authority, &token)),
                Some(&generation),
                crate::unix_millis,
            ));
            let wakes = Arc::new(WakeCounter::default());
            let waker = std::task::Waker::from(Arc::clone(&wakes));
            assert!(
                send.as_mut()
                    .poll(&mut std::task::Context::from_waker(&waker))
                    .is_pending()
            );
            let before = wire.bytes();
            let writes = wire.writes.load(Ordering::Acquire);
            let flushes = wire.flushes.load(Ordering::Acquire);
            let before_wakes = wakes.0.load(Ordering::Acquire);
            fail_after_marker(&root, db, 51).await;
            assert!(wakes.0.load(Ordering::Acquire) > before_wakes);
            assert_eq!(send.await, Err(Closed));
            assert!(failed);
            assert_eq!(wire.bytes(), before);
            assert_eq!(wire.writes.load(Ordering::Acquire), writes);
            assert_eq!(wire.flushes.load(Ordering::Acquire), flushes);
            assert_eq!(
                flush_generation_output(
                    &mut io,
                    &root,
                    &mut out,
                    &mut failed,
                    Some((&db.authority, &token)),
                    None,
                    crate::unix_millis
                )
                .await,
                Err(Closed)
            );
            assert_eq!(wire.bytes(), before);
            settled(db).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
