use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::{GqlParameters, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::cell::Cell;
use std::rc::Rc;

mod aggregates;
mod edge_aggregates;

fn okay<T>(value: Result<T, Failure>) -> T {
    value.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
    )
}
fn options(text: &str) -> Options {
    let args = [
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--property",
        "p=1",
        "--stream",
        text,
    ]
    .map(str::to_owned);
    okay(crate::parse(&args, "query"))
}
fn batch(vid: u128, value: i64) -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    batch.create_vertex(
        VId(vid),
        vec![],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
    );
    batch
}
fn row_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains(r#""event":"row""#))
        .collect()
}

#[test]
fn cli_pull_rows_equal_native_results_with_exact_temporal_metadata() {
    let ((), report) = run_async_under_lab(0x636c_7301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        for (vid, value) in [(0, 4), (2, 11), (9, 7), (u128::MAX, 23)] {
            db.write(&contexts.commit(), batch(vid, value))
                .await
                .unwrap();
        }
        let cases = [
            ("MATCH (n) RETURN n AS id, n.p AS p", 4, 4),
            (
                "MATCH (n) WHERE n.p > 10 OR n.p = 4 RETURN n AS id, n.p AS p",
                4,
                3,
            ),
            (
                "MATCH (n) RETURN DISTINCT n AS id, n.p AS p SKIP 1 LIMIT 2",
                4,
                2,
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS id, n.p AS p",
                1,
                1,
            ),
        ];
        for (text, seq, count) in cases {
            let options = options(text);
            let eager = db
                .query(&cx, text, &options.params, &options, policy())
                .unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, seq, "rows", true, &mut expected));
            let mut streamed = Vec::new();
            okay(run(&db, &cx, &options, true, &mut streamed));
            let expected = String::from_utf8(expected).unwrap();
            let streamed = String::from_utf8(streamed).unwrap();
            assert_eq!(row_lines(&streamed), row_lines(&expected), "{text}");
            assert_eq!(row_lines(&streamed).len(), count);
            assert!(
                streamed.starts_with(&format!(
                    "{{\"v\":1,\"event\":\"columns\",\"stream\":true,\"seq\":{seq},"
                )),
                "{streamed}"
            );
            assert!(streamed.ends_with(&format!(
                "{{\"v\":1,\"event\":\"result\",\"kind\":\"rows\",\"stream\":true,\"seq\":{seq},\"count\":{count}}}\n"
            )), "{streamed}");
        }
        let mut options = options("MATCH (n) WHERE n.p > $floor RETURN n AS id, n.p AS p LIMIT 1");
        options.params = GqlParameters::new().with_int64("floor", 9).unwrap();
        let mut text = Vec::new();
        okay(run(&db, &cx, &options, false, &mut text));
        let text = String::from_utf8(text).unwrap();
        assert!(text.contains("vertex 2\t11"), "{text}");
        assert!(text.ends_with("1 row(s) (stream complete at seq 4)\n"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct ObservedOutput {
    bytes: Vec<u8>,
    pulls: Rc<Cell<usize>>,
    flushed: Rc<Cell<usize>>,
    fail_on_flush: Option<usize>,
}
impl Write for ObservedOutput {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let flush = self.flushed.get() + 1;
        self.flushed.set(flush);
        // Header is flushed before the first pull, each row before the next.
        assert!(self.pulls.get() < flush, "pulled ahead of output demand");
        if self.fail_on_flush == Some(flush) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "receiver closed",
            ));
        }
        Ok(())
    }
}

#[test]
fn failed_header_or_row_flush_stops_demand_without_draining_the_cursor() {
    let ((), report) = run_async_under_lab(0x636c_7302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        for vid in 1..=4 {
            db.write(&contexts.commit(), batch(vid, vid as i64))
                .await
                .unwrap();
        }
        let options = options("MATCH (n) RETURN n AS id, n.p AS p");
        let prepared =
            PreparedNativeRead::prepare(&options.text, &options.params, &options).unwrap();
        for fail in [1, 2, 3] {
            let (columns, mut cursor) = prepared
                .stream(&db, &cx, &options.params, policy())
                .unwrap();
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            let pulls = Rc::new(Cell::new(0));
            let mut out = ObservedOutput {
                bytes: Vec::new(),
                pulls: pulls.clone(),
                flushed: Rc::new(Cell::new(0)),
                fail_on_flush: Some(fail),
            };
            let result = deliver(
                &columns,
                cursor.snapshot_seq().0,
                &mut cursor.by_ref().inspect(|_| pulls.set(pulls.get() + 1)),
                true,
                &mut out,
                || cx.checkpoint().map_err(Failure::query),
            );
            let error = result.err().expect("injected output refusal");
            assert_eq!(error.code, 5);
            assert_eq!(pulls.get(), fail - 1);
            assert_eq!(cursor.row_stats().snapshot_records as usize, fail - 1);
            let stats = cursor.row_stats();
            cursor.close();
            assert!(cursor.next().is_none());
            assert_eq!(cursor.row_stats(), stats);
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cumulative_native_quota_failure_leaves_a_prefix_but_never_a_success_result() {
    let ((), report) = run_async_under_lab(0x636c_7303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        for vid in 1..=4 {
            db.write(&contexts.commit(), batch(vid, vid as i64))
                .await
                .unwrap();
        }
        let options = options("MATCH (n) RETURN n AS id, n.p AS p");
        let prepared =
            PreparedNativeRead::prepare(&options.text, &options.params, &options).unwrap();
        for allowance in [
            GqlQueryPolicy::new(100, 1, 100_000, 100_000),
            GqlQueryPolicy::new(1, 100, 100_000, 100_000),
        ] {
            let (columns, mut cursor) = prepared
                .stream(&db, &cx, &options.params, allowance)
                .unwrap();
            let mut bytes = Vec::new();
            let error = deliver(
                &columns,
                cursor.snapshot_seq().0,
                &mut cursor,
                true,
                &mut bytes,
                || cx.checkpoint().map_err(Failure::query),
            )
            .err()
            .unwrap();
            assert_eq!(error.code, 3);
            assert!(
                error
                    .message
                    .starts_with("stream incomplete after 1 fully flushed row(s)"),
                "{}",
                error.message
            );
            let text = String::from_utf8(bytes).unwrap();
            assert_eq!(row_lines(&text).len(), 1);
            assert!(!text.contains(r#""event":"result""#));
            assert!(cursor.next().is_none());
        }
        assert_eq!(db.frontier().unwrap(), CommitSeq(4));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cancellation_after_a_flushed_row_does_not_request_another() {
    let ((), report) = run_async_under_lab(0x636c_7304, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        for vid in 1..=4 {
            db.write(&contexts.commit(), batch(vid, vid as i64))
                .await
                .unwrap();
        }
        let options = options("MATCH (n) RETURN n AS id, n.p AS p");
        let prepared =
            PreparedNativeRead::prepare(&options.text, &options.params, &options).unwrap();
        let (columns, mut cursor) = prepared
            .stream(&db, &cx, &options.params, policy())
            .unwrap();
        let pulls = Rc::new(Cell::new(0));
        let flushed = Rc::new(Cell::new(0));
        let mut out = ObservedOutput {
            bytes: Vec::new(),
            pulls: pulls.clone(),
            flushed: flushed.clone(),
            fail_on_flush: None,
        };
        let error = deliver(
            &columns,
            cursor.snapshot_seq().0,
            &mut cursor.by_ref().inspect(|_| pulls.set(pulls.get() + 1)),
            true,
            &mut out,
            || {
                if flushed.get() == 2 {
                    Err(Failure::query("cancelled"))
                } else {
                    Ok(())
                }
            },
        )
        .err()
        .expect("delivery cancellation");
        assert_eq!(error.code, 3);
        assert_eq!(pulls.get(), 1);
        assert_eq!(cursor.row_stats().snapshot_records, 1);
        cursor.close();
        assert!(cursor.next().is_none());
        let text = String::from_utf8(out.bytes).unwrap();
        assert_eq!(row_lines(&text).len(), 1);
        assert!(!text.contains(r#""event":"result""#));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_plans_or_future_cuts_emit_no_header_but_limit_zero_completes() {
    let ((), report) = run_async_under_lab(0x636c_7305, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        db.write(&contexts.commit(), batch(1, 4)).await.unwrap();
        for text in [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN COUNT(DISTINCT n.p) AS total LIMIT 0",
            "MATCH (n) RETURN n UNION ALL MATCH (m) RETURN m",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN n AS id LIMIT 0",
        ] {
            let mut bytes = Vec::new();
            assert!(
                run(&db, &cx, &options(text), true, &mut bytes).is_err(),
                "{text}"
            );
            assert!(
                bytes.is_empty(),
                "preparation/admission must precede transport"
            );
        }
        let mut bytes = Vec::new();
        okay(run(
            &db,
            &cx,
            &options("MATCH (n) RETURN n AS id LIMIT 0"),
            true,
            &mut bytes,
        ));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(row_lines(&text).is_empty());
        assert!(text.ends_with("\"seq\":1,\"count\":0}\n"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn stream_is_query_only_unique_and_incompatible_with_eager_certification() {
    for (command, tail) in [
        ("query", vec!["--stream", "--stream", "MATCH (n) RETURN n"]),
        (
            "query",
            vec!["--stream", "--certify-to", "x", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--certify-to", "x", "--stream", "MATCH (n) RETURN n"],
        ),
        ("write", vec!["--stream", "CREATE (n)"]),
        ("create", vec!["--stream"]),
        (
            "transaction",
            vec!["--stream", "--query", "MATCH (n) RETURN n"],
        ),
    ] {
        let mut args = vec!["--db", "x", "--key-file", "y"];
        args.extend(tail);
        assert!(
            crate::parse(
                &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                command
            )
            .is_err()
        );
    }
    let args = [
        "--db",
        "x",
        "--key-file",
        "y",
        "MATCH (n) RETURN n",
        "--stream",
    ]
    .map(str::to_owned);
    assert!(okay(crate::parse(&args, "query")).stream);
}
