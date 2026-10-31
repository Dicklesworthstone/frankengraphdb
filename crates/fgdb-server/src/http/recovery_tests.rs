use super::*;
use crate::recovery::tests::{WakeCounter, WireIo, fail_after_marker, served, settled};
use asupersync::lab::run_async_under_lab;

#[test]
fn batched_and_historical_read_errors_retain_the_output_recovery_fence() {
    let ((), report) = run_async_under_lab(0xba7c_1003, |root| async move {
        for (name, failed, difference) in [
            ("http-read-batch-success-fence", false, false),
            ("http-read-batch-error-fence", true, false),
            ("http-read-diff-error-fence", true, true),
        ] {
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let output = Arc::new(OutputAuthority::new());
            let mut request = asupersync::http::h1::types::RequestBuilder::new(
                Method::Post,
                if difference {
                    "/v1/databases/test/query-diff"
                } else {
                    "/v1/databases/test/query-batch"
                },
            )
            .header(
                "Authorization",
                format!("Bearer {}", fgdb_protocol::json::hex(&token.encode())),
            )
            .build();
            request.body = if difference {
                br#"{"statement":"MATCH (n) RETURN n","before":"0","after":"99"}"#.to_vec()
            } else if failed {
                br#"{"statements":[{"statement":"RETURN 1 AS one"},{"statement":"RETURN $missing AS absent"}]}"#.to_vec()
            } else {
                br#"{"statements":[{"statement":"RETURN 1 AS one"},{"statement":"RETURN 2 AS two"}]}"#.to_vec()
            };
            let response = respond(&root, &server, request, &output).await;
            if failed {
                assert!(response.status >= 400, "{response:?}");
                let body = std::str::from_utf8(&response.body).unwrap();
                assert!(!body.contains("\"results\""), "{body}");
                assert!(!body.contains("\"changes\""), "{body}");
            } else {
                assert_eq!(response.status, 200, "{response:?}");
            }
            fail_after_marker(&root, db, 61).await;
            let (io, wire) = WireIo::new(vec![], false);
            let mut io = GuardedIo::new(io, root.clone(), output);
            let sent = AsyncWrite::poll_write(
                Pin::new(&mut io),
                &mut Context::from_waker(std::task::Waker::noop()),
                &response.body,
            );
            assert!(
                matches!(sent, Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
            assert_eq!(wire.writes.load(Ordering::Relaxed), 0);
            settled(db).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn recovery_wakes_and_fences_http_headers_body_and_flush_without_replacement_bytes() {
    let ((), report) = run_async_under_lab(0x79a0_0401, |root| async move {
        for (name, writes, flush_pending) in [
            ("http-unwritten", vec![0], false),
            ("http-partial", vec![7, 0], false),
            ("http-flush", vec![], true),
        ] {
            let (server, token, _) = served(&root, name).await;
            let db = &server.databases["test"];
            let generation = db.db.generation().unwrap();
            let output = Arc::new(OutputAuthority::new());
            let verified = db
                .authority
                .verify_at(&token, crate::TRUNK, crate::unix_millis())
                .unwrap();
            assert!(output.protected(&root, &db.authority, verified));
            assert!(output.pin_generation(generation));
            let (io, wire) = WireIo::new(writes, flush_pending);
            let mut io = GuardedIo::new(io, root.clone(), Arc::clone(&output));
            let wakes = Arc::new(WakeCounter::default());
            let waker = std::task::Waker::from(Arc::clone(&wakes));
            let bytes = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}";
            match io.poll_write_at(
                &mut Context::from_waker(&waker),
                bytes,
                crate::unix_millis(),
            ) {
                Poll::Pending => {}
                Poll::Ready(Ok(count)) if flush_pending => {
                    assert_eq!(count, bytes.len());
                    assert!(
                        io.poll_flush_at(&mut Context::from_waker(&waker), crate::unix_millis())
                            .is_pending()
                    );
                }
                Poll::Ready(Ok(count)) => {
                    assert_eq!(count, 7);
                    assert!(
                        io.poll_write_at(
                            &mut Context::from_waker(&waker),
                            &bytes[count..],
                            crate::unix_millis()
                        )
                        .is_pending()
                    );
                }
                other => panic!("expected bounded physical progress: {other:?}"),
            }
            let before = wire.bytes();
            let writes = wire.writes.load(Ordering::Acquire);
            let flushes = wire.flushes.load(Ordering::Acquire);
            let before_wakes = wakes.0.load(Ordering::Acquire);
            fail_after_marker(&root, db, 61).await;
            assert!(
                wakes.0.load(Ordering::Acquire) > before_wakes,
                "a blocked HTTP writer must observe recovery without socket readiness"
            );
            let result = if flush_pending {
                io.poll_flush_at(&mut Context::from_waker(&waker), crate::unix_millis())
            } else {
                io.poll_write_at(
                    &mut Context::from_waker(&waker),
                    bytes,
                    crate::unix_millis(),
                )
                .map(|result| result.map(|_| ()))
            };
            assert!(
                matches!(result, Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::PermissionDenied)
            );
            assert_eq!(wire.bytes(), before);
            assert_eq!(wire.writes.load(Ordering::Acquire), writes);
            assert_eq!(wire.flushes.load(Ordering::Acquire), flushes);
            assert!(
                !output.public(&root),
                "a partial HTTP response cannot become a new response"
            );
            settled(db).await;
            server.join_database_workers(&root).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
