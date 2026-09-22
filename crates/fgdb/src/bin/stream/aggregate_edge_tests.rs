//! Real native edge-aggregate cursors through the production CLI delivery loop.
use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{DatabaseKeys, WriteBatch};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::scan_stream::{ScanError, ScanKind};
use fgdb_gql::stream::VertexScanState;
use fgdb_gql::{GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::cell::Cell;
use std::rc::Rc;

const R: RelationId = RelationId(1);
const S: RelationId = RelationId(2);
const P: PropertyKeyId = PropertyKeyId(1);
const START: VId = VId(1_u128 << 100);
const END: VId = VId(u128::MAX);
const SUMMARY: &str =
    "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS total,COUNT(r.p) AS present,SUM(r.p) AS sum";
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
        "--relation",
        "R=1",
        "--relation",
        "S=2",
        "--property",
        "p=1",
        "--stream",
        text,
    ]
    .map(str::to_owned);
    okay(crate::parse(&args, "query"))
}
fn row_lines(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| line.contains(r#""event":"row""#))
        .collect()
}
async fn seed(db: &mut Database<fgdb::MemVfs>, cx: &CommitCx) {
    let mut r = WriteBatch::new(R);
    for id in [START, VId(1), END] {
        r.create_vertex(id, vec![], vec![]);
    }
    for (id, from, to, value) in [
        (0, START, VId(1), Some(i64::MAX)),
        (1, START, VId(1), Some(i64::MAX)),
        (2, VId(1), END, None),
        (u128::MAX, START, START, Some(-3)),
    ] {
        r.add_edge(
            EId(id),
            from,
            to,
            value
                .into_iter()
                .map(|value| (P, CanonicalScalar::Int(value)))
                .collect(),
        );
    }
    let mut s = WriteBatch::new(S);
    s.add_edge(EId(5), VId(1), END, vec![(P, CanonicalScalar::Int(1))]);
    s.add_edge(EId(6), VId(1), VId(1), vec![]);
    db.write_atomic(cx, vec![r, s]).await.unwrap();
}

#[test]
fn cli_edges_chains_and_probes_keep_eager_cells_and_independent_wide_sums() {
    let ((), report) = run_async_under_lab(0x636c_e601, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let max = i128::from(i64::MAX);
        for (body, count, present, sum) in [
            ("(a)-[r:R]->(b)", 4, 3, 2 * max - 3),
            ("(a)<-[r:R]-(b)", 4, 3, 2 * max - 3),
            ("(a)-[r:R]-(b)", 7, 5, 4 * max - 3),
            ("(a)-[r:R]->(b)-[s:S]->(c)", 4, 4, 4 * max),
            (
                "(a)-[r:R]->(b) WHERE EXISTS { MATCH (b)-[:S*1..3]->(c) }",
                2,
                2,
                2 * max,
            ),
            (
                "(a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b)-[:S]->(c) }",
                2,
                1,
                -3,
            ),
        ] {
            let text = format!(
                "MATCH {body} RETURN COUNT(*) AS total,COUNT(r.p) AS present,SUM(r.p) AS sum"
            );
            let options = options(&text);
            let eager = db
                .query(&cx, &text, &options.params, &options, policy())
                .unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, 1, "rows", true, &mut expected));
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &options, true, &mut bytes));
            let output = String::from_utf8(bytes).unwrap();
            assert_eq!(
                row_lines(&output),
                row_lines(&String::from_utf8(expected).unwrap())
            );
            let independent = format!(
                r#"{{"v":1,"event":"row","cells":[{{"type":"count","value":"{count}"}},{{"type":"count","value":"{present}"}},{{"type":"wideint","value":"{sum}"}}]}}"#
            );
            assert_eq!(row_lines(&output), vec![independent.as_str()]);
            assert_eq!(output.lines().count(), 3);
            assert!(output.ends_with("\"seq\":1,\"count\":1}\n"));
            let mut human = Vec::new();
            okay(run(&db, &cx, &options, false, &mut human));
            let human = String::from_utf8(human).unwrap();
            assert!(human.contains(&format!("{count}\t{present}\t{sum}\n")));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_rebinding_empty_summary_and_alias_order_do_not_follow_the_writer() {
    let ((), report) = run_async_under_lab(0x636c_e602, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(0), P, Some(CanonicalScalar::Int(-7)));
        db.write(&commit, edit).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $seq RETURN SUM(r.p) AS s,COUNT(*) AS n,SUM(r.p) AS again";
        for (seq, count, sum) in [
            (0, 0, None),
            (1, 4, Some(2 * i128::from(i64::MAX) - 3)),
            (2, 4, Some(i128::from(i64::MAX) - 10)),
        ] {
            let mut opts = options(text);
            opts.params = GqlParameters::new().with_uint64("seq", seq).unwrap();
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &opts, true, &mut bytes));
            let output = String::from_utf8(bytes).unwrap();
            let value = sum.map_or_else(
                || "{\"type\":\"null\"}".to_owned(),
                |sum| format!(r#"{{"type":"wideint","value":"{sum}"}}"#),
            );
            let row = format!(
                r#"{{"v":1,"event":"row","cells":[{value},{{"type":"count","value":"{count}"}},{value}]}}"#
            );
            assert_eq!(row_lines(&output), vec![row.as_str()]);
            assert!(output.contains("\"columns\":[\"s\",\"n\",\"again\"]"));
            assert!(output.ends_with(&format!("\"seq\":{seq},\"count\":1}}\n")));
        }
        let mut opts =
            options("MATCH (a)-[r:R]->(b) WHERE r.p > $floor RETURN COUNT(*) AS n,SUM(r.p) AS s");
        opts.params = GqlParameters::new().with_int64("floor", i64::MAX).unwrap();
        let prepared = PreparedNativeRead::prepare(&opts.text, &opts.params, &opts).unwrap();
        let mut cursor = prepared
            .stream_aggregate(&db, &cx, &opts.params, policy())
            .unwrap();
        assert_eq!(cursor.kind(), ScanKind::Edge);
        drop(opts);
        drop(prepared);
        drop(db);
        let columns = cursor.columns().to_vec();
        let mut bytes = Vec::new();
        okay(deliver(
            &columns,
            cursor.snapshot_seq().0,
            &mut cursor,
            true,
            &mut bytes,
            || Ok(()),
        ));
        let output = String::from_utf8(bytes).unwrap();
        assert!(row_lines(&output)[0].contains("\"type\":\"count\",\"value\":\"0\""));
        assert!(row_lines(&output)[0].contains("\"type\":\"null\""));
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct Output {
    bytes: Vec<u8>,
    pulls: Rc<Cell<usize>>,
    flushes: usize,
    fail: usize,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.flushes += 1;
        assert!(self.pulls.get() < self.flushes);
        if self.flushes == self.fail {
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
fn output_failure_and_predemand_cancellation_do_not_start_or_repeat_aggregation() {
    let ((), report) = run_async_under_lab(0x636c_e603, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let opts = options(SUMMARY);
        let prepared = PreparedNativeRead::prepare(SUMMARY, &opts.params, &opts).unwrap();
        for fail in [1, 2] {
            let mut cursor = prepared
                .stream_aggregate(&db, &cx, &opts.params, policy())
                .unwrap();
            let columns = cursor.columns().to_vec();
            let pulls = Rc::new(Cell::new(0));
            let mut output = Output {
                bytes: Vec::new(),
                pulls: pulls.clone(),
                flushes: 0,
                fail,
            };
            let error = deliver(
                &columns,
                cursor.snapshot_seq().0,
                &mut cursor.by_ref().inspect(|_| pulls.set(pulls.get() + 1)),
                true,
                &mut output,
                || cx.checkpoint().map_err(Failure::query),
            )
            .err()
            .expect("injected flush failure");
            assert_eq!(error.code, 5);
            assert_eq!(pulls.get(), fail - 1);
            if fail == 1 {
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(cursor.evaluator_stats().work_units, 0);
            }
            let before = (cursor.row_stats(), cursor.evaluator_stats());
            cursor.close();
            assert!(cursor.next().is_none());
            assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), before);
            assert!(
                !String::from_utf8(output.bytes)
                    .unwrap()
                    .contains("\"event\":\"result\"")
            );
        }
        let mut cursor = prepared
            .stream_aggregate(&db, &cx, &opts.params, policy())
            .unwrap();
        let columns = cursor.columns().to_vec();
        let mut bytes = Vec::new();
        let mut calls = 0;
        let error = deliver(
            &columns,
            cursor.snapshot_seq().0,
            &mut cursor,
            true,
            &mut bytes,
            || {
                calls += 1;
                if calls == 2 {
                    Err(Failure::query("cancelled before demand"))
                } else {
                    Ok(())
                }
            },
        )
        .err()
        .expect("cancelled delivery");
        assert_eq!(error.code, 3);
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.evaluator_stats().work_units, 0);
        cursor.close();
        assert!(cursor.next().is_none());
        assert_eq!(String::from_utf8(bytes).unwrap().lines().count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
#[test]
fn late_sum_and_quota_failures_never_encode_partial_summaries() {
    let ((), report) = run_async_under_lab(0x636c_e604, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let opts = options(SUMMARY);
        let prepared = PreparedNativeRead::prepare(SUMMARY, &opts.params, &opts).unwrap();
        let mut full = prepared
            .stream_aggregate(&db, &cx, &opts.params, policy())
            .unwrap();
        full.next().unwrap().unwrap();
        let r = full.row_stats();
        let e = full.evaluator_stats();
        for quota in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(r.snapshot_records, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(r.snapshot_records, 1, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut cursor = prepared
                .stream_aggregate(&db, &cx, &opts.params, quota)
                .unwrap();
            let columns = cursor.columns().to_vec();
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
            .expect("quota refusal");
            assert_eq!(error.code, 3);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert!(cursor.next().is_none());
            assert_eq!(String::from_utf8(bytes).unwrap().lines().count(), 1);
        }
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(
            EId(u128::MAX),
            P,
            Some(CanonicalScalar::ucs_basic_text("private late invalid operand").unwrap()),
        );
        db.write(&commit, edit).await.unwrap();
        let mut bytes = Vec::new();
        let error = run(&db, &cx, &opts, true, &mut bytes)
            .err()
            .expect("late SUM refusal");
        assert_eq!(error.code, 3);
        assert!(!error.message.contains("private late"));
        let output = String::from_utf8(bytes).unwrap();
        assert_eq!(output.lines().count(), 1);
        assert!(row_lines(&output).is_empty());
        assert!(!output.contains("\"event\":\"result\""));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_admission_precedes_headers_and_io_errors_keep_their_cause() {
    let ((), report) = run_async_under_lab(0x636c_e605, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(DISTINCT r) AS n",
            "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 2 RETURN COUNT(*) AS n",
        ] {
            let opts = options(text);
            PreparedNativeRead::prepare(text, &opts.params, &opts).unwrap();
            let mut bytes = Vec::new();
            assert!(run(&db, &cx, &opts, true, &mut bytes).is_err());
            assert!(bytes.is_empty(), "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
    let io = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "source failure");
    let error =
        GqlQueryError::<GraphAggregateError<ScanError<std::io::Error>>, std::io::Error>::Source(
            GraphAggregateError::Source(ScanError::Edge(
                fgdb_gql::edge_stream::EdgeScanError::Source(io),
            )),
        );
    assert_eq!(execution_failure(error).code, 5);
}

#[test]
fn computed_group_inputs_reach_robot_and_human_delivery_without_narrowing() {
    let ((), report) = run_async_under_lab(0x636c_ec01, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        seed(&mut db, &commit).await;
        let text = "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n, COALESCE(r.p,0)/2 AS bucket, SUM(r.p-1) AS total, AVG(r.p-1) AS average, COUNT(DISTINCT r.p-1) AS support GROUP BY COALESCE(r.p,0)/2";
        let opts = options(text);
        let eager = db.query(&cx, text, &opts.params, &opts, policy()).unwrap();
        let mut expected = Vec::new();
        okay(crate::render(eager, 1, "rows", true, &mut expected));
        let mut robot = Vec::new();
        okay(run(&db, &cx, &opts, true, &mut robot));
        let robot = String::from_utf8(robot).unwrap();
        assert_eq!(
            row_lines(&robot),
            row_lines(&String::from_utf8(expected).unwrap())
        );
        assert_eq!(row_lines(&robot).len(), 3);
        assert!(robot.contains("\"columns\":[\"n\",\"bucket\",\"total\",\"average\",\"support\"]"));
        assert!(robot.contains(&(2 * (i128::from(i64::MAX) - 1)).to_string()));
        let mut human = Vec::new();
        okay(run(&db, &cx, &opts, false, &mut human));
        assert!(
            String::from_utf8(human)
                .unwrap()
                .contains("3 row(s) (stream complete at seq 1)")
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
