//! Actual native edge cursors exercise the CLI delivery boundary. A flushed
//! prefix is not a complete result, and transport failure must stop demand.
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::edge_stream::EdgeScanError;
use fgdb_gql::scan_stream::{ScanError, ScanKind, ScanState};
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::cell::Cell;
use std::rc::Rc;

fn okay<T>(value: Result<T, Failure>) -> T {
    value.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xd1; 32],
        DatabaseSecurityNamespaceId([0xd2; 32]),
        [0xd3; 32],
    )
}
fn options(text: &str) -> Options {
    let args = [
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--relation",
        "R=1",
        "--property",
        "p=1",
        "--stream",
        text,
    ]
    .map(str::to_owned);
    okay(crate::parse(&args, "query"))
}
fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, value) in [(0, 3), (1, 7), (u128::MAX, 11)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    for (id, from, to, value) in [
        (0, 1, 0, 4),
        (1, 1, 0, 7),
        (2, 1, 1, 8),
        (u128::MAX, 0, u128::MAX, i64::MIN),
    ] {
        batch.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    batch
}
fn row_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains(r#""event":"row""#))
        .collect()
}
const QUERY: &str = "MATCH (a)-[r:R]-(b) RETURN r, a, b, r.p AS weight";

#[test]
fn cli_edge_streams_preserve_lossless_rows_parameters_and_exact_temporal_metadata() {
    let ((), report) = run_async_under_lab(0x636c_7401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let basis = db.write(&commit, fixture()).await.unwrap();
        let mut changed = WriteBatch::new(RelationId(1));
        changed.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(19)));
        let latest = db.write(&commit, changed).await.unwrap();
        for (left, right) in [("-", "->"), ("<-", "-"), ("-", "-")] {
            for (filter, page, distinct) in [
                ("", "", "ALL"),
                (
                    "WHERE r.p > 0 AND a.p IS NOT NULL",
                    "SKIP 1 LIMIT 2",
                    "DISTINCT",
                ),
            ] {
                for historical in [false, true] {
                    let temporal = if historical {
                        format!("FOR SYSTEM_TIME AS OF SEQ {}", basis.0)
                    } else {
                        String::new()
                    };
                    let text = format!(
                        "MATCH (a){left}[r:R]{right}(b) {temporal} {filter} RETURN {distinct} r, a, b, r.p AS weight {page}"
                    );
                    let options = options(&text);
                    let expected = db
                        .query(&cx, &text, &options.params, &options, policy())
                        .unwrap();
                    let seq = if historical { basis.0 } else { latest.0 };
                    let mut eager = Vec::new();
                    okay(crate::render(expected, seq, "rows", true, &mut eager));
                    let mut bytes = Vec::new();
                    okay(run(&db, &cx, &options, true, &mut bytes));
                    let eager = String::from_utf8(eager).unwrap();
                    let streamed = String::from_utf8(bytes).unwrap();
                    assert_eq!(row_lines(&streamed), row_lines(&eager), "{text}");
                    let count = row_lines(&streamed).len();
                    assert!(streamed.starts_with(&format!(
                        "{{\"v\":1,\"event\":\"columns\",\"stream\":true,\"seq\":{seq},"
                    )));
                    assert!(streamed.ends_with(&format!("{{\"v\":1,\"event\":\"result\",\"kind\":\"rows\",\"stream\":true,\"seq\":{seq},\"count\":{count}}}\n")));
                    if filter.is_empty() {
                        assert!(streamed.contains(&format!(
                            "{{\"type\":\"edge\",\"value\":\"{}\"}}",
                            u128::MAX
                        )));
                        assert!(
                            streamed.contains(&format!(
                                "{{\"type\":\"int\",\"value\":\"{}\"}}",
                                i64::MIN
                            ))
                        );
                    }
                }
            }
        }
        let mut options =
            options("MATCH (a)-[r:R]->(b) WHERE r.p > $floor RETURN r, a, b, r.p AS weight");
        options.params = GqlParameters::new().with_int64("floor", 10).unwrap();
        let mut human = Vec::new();
        okay(run(&db, &cx, &options, false, &mut human));
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains("edge 1\tvertex 1\tvertex 0\t19"));
        assert!(human.ends_with(&format!("1 row(s) (stream complete at seq {})\n", latest.0)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct Output {
    bytes: Vec<u8>,
    pulls: Rc<Cell<usize>>,
    flushed: Rc<Cell<usize>>,
    fail_at: Option<usize>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let at = self.flushed.get() + 1;
        self.flushed.set(at);
        assert!(
            self.pulls.get() < at,
            "source advanced ahead of flushed output"
        );
        if self.fail_at == Some(at) {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "receiver closed",
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn broken_output_or_delivery_cancellation_stops_even_the_second_edge_orientation() {
    let ((), report) = run_async_under_lab(0x636c_7402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let options = options(QUERY);
        let prepared = PreparedNativeRead::prepare(QUERY, &options.params, &options).unwrap();
        for (fail_at, cancel) in [
            (Some(1), false),
            (Some(2), false),
            (Some(3), false),
            (None, true),
        ] {
            let (columns, mut cursor) = prepared
                .stream(&db, &cx, &options.params, policy())
                .unwrap();
            assert_eq!(cursor.kind(), ScanKind::Edge);
            let pulls = Rc::new(Cell::new(0));
            let flushed = Rc::new(Cell::new(0));
            let mut out = Output {
                bytes: Vec::new(),
                pulls: pulls.clone(),
                flushed: flushed.clone(),
                fail_at,
            };
            let error = deliver(
                &columns,
                cursor.snapshot_seq().0,
                &mut cursor.by_ref().inspect(|_| pulls.set(pulls.get() + 1)),
                true,
                &mut out,
                || {
                    if cancel && flushed.get() == 2 {
                        Err(Failure::query("delivery cancelled"))
                    } else {
                        Ok(())
                    }
                },
            )
            .err()
            .expect("delivery must refuse");
            let requested = if cancel { 1 } else { fail_at.unwrap() - 1 };
            assert_eq!(pulls.get(), requested);
            assert_eq!(error.code, if cancel { 3 } else { 5 });
            // The first two rows are two orientations of one candidate edge.
            assert_eq!(
                cursor.row_stats().snapshot_records,
                u64::from(requested > 0)
            );
            let before = (cursor.row_stats(), cursor.evaluator_stats());
            cursor.close();
            assert!(cursor.next().is_none());
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), before);
            assert_eq!(cursor.state(), ScanState::Closed);
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
fn edge_source_and_result_quotas_do_not_become_success_after_a_flushed_prefix() {
    let ((), report) = run_async_under_lab(0x636c_7403, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let options = options(QUERY);
        let prepared = PreparedNativeRead::prepare(QUERY, &options.params, &options).unwrap();
        for (allowance, count) in [
            (GqlQueryPolicy::new(100, 1, 100_000, 100_000), 1),
            (GqlQueryPolicy::new(1, 100, 100_000, 100_000), 2),
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
            .expect("quota exhausted");
            assert_eq!(error.code, 3);
            assert!(error.message.starts_with(&format!(
                "stream incomplete after {count} fully flushed row(s)"
            )));
            assert_eq!(cursor.state(), ScanState::Failed);
            assert_eq!(cursor.row_stats().snapshot_records, 1);
            assert_eq!(cursor.row_stats().result_rows, count as u64);
            assert!(cursor.next().is_none());
            let text = String::from_utf8(bytes).unwrap();
            assert_eq!(row_lines(&text).len(), count);
            assert!(!text.contains(r#""event":"result""#));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_edge_shapes_future_cuts_and_missing_parameters_emit_no_columns() {
    let ((), report) = run_async_under_lab(0x636c_7404, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN a, r LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN r, a, r.p AS p ORDER BY p LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN r, a, c LIMIT 0",
            "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 2 RETURN r, a, b LIMIT 0",
            "MATCH (a)-[r:R]->(b) WHERE r.p > $floor RETURN r, a, b",
        ] {
            let mut bytes = Vec::new();
            assert!(
                run(&db, &cx, &options(text), true, &mut bytes).is_err(),
                "{text}"
            );
            assert!(bytes.is_empty(), "admission must finish before a header");
        }
        let mut bytes = Vec::new();
        okay(run(
            &db,
            &cx,
            &options(&(QUERY.to_owned() + " LIMIT 0")),
            true,
            &mut bytes,
        ));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.ends_with("\"seq\":1,\"count\":0}\n"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_scan_error_wrapping_preserves_transport_io_classification_after_a_row() {
    let ((), report) = run_async_under_lab(0x636c_7405, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let options = options(QUERY);
        let prepared = PreparedNativeRead::prepare(QUERY, &options.params, &options).unwrap();
        let (columns, mut cursor) = prepared
            .stream(&db, &cx, &options.params, policy())
            .unwrap();
        let row = cursor.next().unwrap().unwrap();
        cursor.close();
        // Delivery-only injection, not a substitute graph source: the complete
        // first row came from the real cursor. Check the nested error chain.
        let error =
            GqlQueryError::<ScanError<std::io::Error>, std::io::Error>::Source(ScanError::Edge(
                EdgeScanError::Source(std::io::Error::other("source I/O failed")),
            ));
        let mut input = vec![Ok(row), Err(error)].into_iter();
        let mut bytes = Vec::new();
        let error = deliver(
            &columns,
            cursor.snapshot_seq().0,
            &mut input,
            true,
            &mut bytes,
            || Ok(()),
        )
        .err()
        .expect("injected source failure");
        assert_eq!(error.code, 5);
        assert!(
            error
                .message
                .starts_with("stream incomplete after 1 fully flushed row(s)")
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(row_lines(&text).len(), 1);
        assert!(!text.contains(r#""event":"result""#));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn help_and_robot_contract_expose_both_physical_scan_profiles_without_changing_delivery() {
    assert!(crate::HELP.contains("one-edge"));
    assert!(crate::HELP.contains("leading vertex identity"));
    assert!(crate::ROBOT_SCHEMA.contains("one-edge"));
    assert!(crate::ROBOT_SCHEMA.contains("edge/source identities"));
    assert!(crate::ROBOT_SCHEMA.contains("no eager fallback or spill"));
    assert!(crate::ROBOT_SCHEMA.contains("error or EOF without result means an incomplete result"));
    assert!(crate::ROBOT_SCHEMA.contains("max_buffered_output_bytes"));
}
