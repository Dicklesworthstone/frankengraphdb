//! Real native grouped cursors drive the same lossless CLI delivery boundary.
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::stream::VertexScanState;
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::cell::Cell;
use std::rc::Rc;

fn okay<T>(result: Result<T, Failure>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x41; 32],
        DatabaseSecurityNamespaceId([0x42; 32]),
        [0x43; 32],
    )
}
fn options(text: &str) -> Options {
    okay(crate::parse(
        &[
            "--db",
            "unused",
            "--key-file",
            "unused",
            "--property",
            "p=1",
            "--property",
            "q=2",
            "--stream",
            text,
        ]
        .map(str::to_owned),
        "query",
    ))
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, key, n) in [(0, 2, 5), (1, 1, 7), (2, 2, 2), (u128::MAX, 1, 3)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(key)),
                (PropertyKeyId(2), CanonicalScalar::Int(n)),
            ],
        );
    }
    batch
}
const TEXT: &str = "MATCH (n) RETURN SUM(n.q) AS total, n.p AS bucket, AVG(n.q) AS mean, COUNT(*) AS count GROUP BY n.p";
fn rows(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains("\"event\":\"row\""))
        .collect()
}

#[test]
fn grouped_cli_cells_follow_return_slots_and_keep_exact_fractions_and_temporal_cuts() {
    let ((), report) = run_async_under_lab(0x636c_a701, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        db.write(&c.commit(), seed()).await.unwrap();
        let options = options(TEXT);
        let mut bytes = Vec::new();
        okay(run(&db, &cx, &options, true, &mut bytes));
        let actual = String::from_utf8(bytes).unwrap();
        let eager = db
            .query(&cx, TEXT, &options.params, &options, policy())
            .unwrap();
        let mut expected = Vec::new();
        okay(crate::render(eager, 1, "rows", true, &mut expected));
        assert_eq!(rows(&actual), rows(&String::from_utf8(expected).unwrap()));
        assert_eq!(
            rows(&actual),
            vec![
                r#"{"v":1,"event":"row","cells":[{"type":"wideint","value":"10"},{"type":"int","value":"1"},{"type":"average","value":"5/1"},{"type":"count","value":"2"}]}"#,
                r#"{"v":1,"event":"row","cells":[{"type":"wideint","value":"7"},{"type":"int","value":"2"},{"type":"average","value":"7/2"},{"type":"count","value":"2"}]}"#,
            ]
        );
        assert!(actual.contains("\"columns\":[\"total\",\"bucket\",\"mean\",\"count\"]"));
        assert!(actual.ends_with("\"seq\":1,\"count\":2}\n"));
        let mut human = Vec::new();
        okay(run(&db, &cx, &options, false, &mut human));
        assert!(
            String::from_utf8(human)
                .unwrap()
                .contains("10\t1\t5\t2\n7\t2\t7/2\t2\n")
        );
        for seq in [0, 1] {
            let text = format!(
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ {seq} RETURN COUNT(*) AS count, n AS id GROUP BY n"
            );
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &self::options(&text), true, &mut bytes));
            let text = String::from_utf8(bytes).unwrap();
            assert_eq!(rows(&text).len(), if seq == 0 { 0 } else { 4 });
            assert!(text.contains(&format!("\"seq\":{seq}")));
            if seq == 1 {
                assert!(text.contains(&u128::MAX.to_string()));
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct Output {
    bytes: Vec<u8>,
    flushed: Rc<Cell<usize>>,
    fail_at: Option<usize>,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flushed.set(self.flushed.get() + 1);
        if self.fail_at == Some(self.flushed.get()) {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed receiver",
            ))
        } else {
            Ok(())
        }
    }
}

#[test]
fn grouped_transport_stops_before_demand_or_between_groups_without_success() {
    let ((), report) = run_async_under_lab(0x636c_a702, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = Database::open_memory(&c.commit(), keys()).await.unwrap();
        db.write(&c.commit(), seed()).await.unwrap();
        let options = options(TEXT);
        let prepared = PreparedNativeRead::prepare(TEXT, &options.params, &options).unwrap();
        for (fail_at, cancelled) in [(Some(1), false), (Some(2), false), (None, true)] {
            let mut cursor = prepared
                .stream_aggregate(&db, &cx, &options.params, policy())
                .unwrap();
            let slots = cursor.output_slots().to_vec();
            let columns = cursor.columns().to_vec();
            let flushed = Rc::new(Cell::new(0));
            let mut out = Output {
                bytes: Vec::new(),
                flushed: Rc::clone(&flushed),
                fail_at,
            };
            let mut pulls = 0;
            let error = deliver(
                &columns,
                cursor.snapshot_seq().0,
                &mut cursor.by_ref().map(|result| {
                    pulls += 1;
                    result.map(|row| AggregateDeliveryRow { row, slots: &slots })
                }),
                true,
                &mut out,
                || {
                    if cancelled && flushed.get() == 2 {
                        Err(Failure::query("cancelled"))
                    } else {
                        cx.checkpoint().map_err(Failure::query)
                    }
                },
            )
            .err()
            .expect("injected delivery failure");
            assert_eq!(error.code, if cancelled { 3 } else { 5 });
            assert_eq!(pulls, usize::from(fail_at != Some(1)));
            if fail_at == Some(1) {
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(cursor.evaluator_stats().work_units, 0);
            } else {
                assert_eq!(cursor.row_stats().result_rows, 1);
            }
            let stats = (cursor.row_stats(), cursor.evaluator_stats());
            cursor.close();
            assert_eq!(cursor.state(), VertexScanState::Closed);
            assert!(cursor.next().is_none());
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
            assert!(
                !String::from_utf8(out.bytes)
                    .unwrap()
                    .contains("\"event\":\"result\"")
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
