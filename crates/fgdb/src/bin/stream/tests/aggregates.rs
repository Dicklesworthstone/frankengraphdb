//! Exercise the production CLI adapter with real lab/MemVfs aggregate cursors.
//! Transport faults observe demand; they do not replace the query/source engine.

use super::*;
use fgdb_gql::stream::VertexScanState;

const SUMMARY: &str = "MATCH (n) RETURN COUNT(*) AS total, COUNT(n.p) AS nonnull, \
    SUM(n.p) AS sum, AVG(n.p) AS average, MIN(n.p) AS low, MAX(n.p) AS high, \
    MIN(n) AS first, MAX(n) AS last";

#[test]
fn cli_aggregate_stream_matches_eager_cells_and_independent_exact_values() {
    let ((), report) = run_async_under_lab(0x636c_a601, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for (id, value) in [(0, Some(4)), (1, Some(7)), (1_u128 << 100, Some(11)),
            (u128::MAX - 1, None)] {
            seed.create_vertex(VId(id), vec![], value.into_iter()
                .map(|value| (PropertyKeyId(1), CanonicalScalar::Int(value))).collect());
        }
        seed.create_vertex(VId(u128::MAX), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Null)]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let options = options(SUMMARY);
        let eager = db.query(&cx, SUMMARY, &options.params, &options, policy()).unwrap();
        let mut expected = Vec::new();
        okay(crate::render(eager, 1, "rows", true, &mut expected));
        let mut bytes = Vec::new();
        okay(run(&db, &cx, &options, true, &mut bytes));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(row_lines(&text), row_lines(&String::from_utf8(expected).unwrap()));
        let independent = format!(concat!(
            "{{\"v\":1,\"event\":\"row\",\"cells\":[",
            "{{\"type\":\"count\",\"value\":\"5\"}},",
            "{{\"type\":\"count\",\"value\":\"3\"}},",
            "{{\"type\":\"wideint\",\"value\":\"22\"}},",
            "{{\"type\":\"average\",\"value\":\"22/3\"}},",
            "{{\"type\":\"int\",\"value\":\"4\"}},",
            "{{\"type\":\"int\",\"value\":\"11\"}},",
            "{{\"type\":\"vertex\",\"value\":\"0\"}},",
            "{{\"type\":\"vertex\",\"value\":\"{}\"}}]}}"), u128::MAX);
        assert_eq!(row_lines(&text), vec![independent.as_str()]);
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with("\"seq\":1,\"count\":1}\n"));
        let mut human = Vec::new();
        okay(run(&db, &cx, &options, false, &mut human));
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains(&format!("5\t3\t22\t22/3\t4\t11\tvertex 0\tvertex {}\n", u128::MAX)));
        assert!(human.ends_with("1 row(s) (stream complete at seq 1)\n"));

        // Non-numeric extrema still use their original scalar domains/escaping.
        let mut edit = WriteBatch::new(RelationId(1));
        for (id, value) in [(0, "a\nquoted\""), (1, "z")] {
            edit.set_vertex_property(VId(id), PropertyKeyId(1),
                Some(CanonicalScalar::ucs_basic_text(value).unwrap()));
        }
        db.write(&contexts.commit(), edit).await.unwrap();
        let opts = super::options("MATCH (n) WHERE n.p IS NOT NULL RETURN MIN(n.p) AS low, MAX(n.p) AS high");
        let eager = db.query(&cx, &opts.text, &opts.params, &opts, policy()).unwrap();
        let mut expected = Vec::new();
        okay(crate::render(eager, 2, "rows", true, &mut expected));
        let mut bytes = Vec::new();
        okay(run(&db, &cx, &opts, true, &mut bytes));
        assert_eq!(row_lines(&String::from_utf8(bytes).unwrap()),
            row_lines(&String::from_utf8(expected).unwrap()));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_parameters_empty_input_and_repeated_aggregates_keep_exact_metadata() {
    let ((), report) = run_async_under_lab(0x636c_a602, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        seed.create_vertex(VId(1), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(7))]);
        seed.create_vertex(VId(2), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(9))]);
        db.write(&contexts.commit(), seed).await.unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(11)));
        db.write(&contexts.commit(), edit).await.unwrap();
        let query = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $seq RETURN AVG(n.p) AS a, \
            COUNT(*) AS total, SUM(n.p) AS sum, AVG(n.p) AS again";
        for (seq, count, sum, average) in [(0, 0, None, None), (1, 2, Some(16), Some(8)),
            (2, 2, Some(20), Some(10))] {
            let mut options = options(query);
            options.params = GqlParameters::new().with_uint64("seq", seq).unwrap();
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &options, true, &mut bytes));
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.starts_with(&format!("{{\"v\":1,\"event\":\"columns\",\"stream\":true,\"seq\":{seq},")));
            assert!(text.contains("\"columns\":[\"a\",\"total\",\"sum\",\"again\"]"));
            let avg_cell = average.map_or_else(|| "{\"type\":\"null\"}".to_owned(),
                |value| format!("{{\"type\":\"average\",\"value\":\"{value}/1\"}}"));
            let sum_cell = sum.map_or_else(|| "{\"type\":\"null\"}".to_owned(),
                |value| format!("{{\"type\":\"wideint\",\"value\":\"{value}\"}}"));
            assert_eq!(row_lines(&text), vec![format!(
                "{{\"v\":1,\"event\":\"row\",\"cells\":[{avg_cell},{{\"type\":\"count\",\"value\":\"{count}\"}},{sum_cell},{avg_cell}]}}"
            ).as_str()]);
            assert!(text.ends_with(&format!("\"seq\":{seq},\"count\":1}}\n")));
        }
        let mut options = options("MATCH (n) WHERE n.p > $floor RETURN COUNT(*) AS total, AVG(n.p) AS a");
        for (floor, count) in [(0, 2), (10, 1), (100, 0)] {
            options.params = GqlParameters::new().with_int64("floor", floor).unwrap();
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &options, true, &mut bytes));
            let text = String::from_utf8(bytes).unwrap();
            assert!(row_lines(&text)[0].contains(&format!("\"type\":\"count\",\"value\":\"{count}\"")));
            assert_eq!(row_lines(&text).len(), 1);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_header_flush_or_predemand_cancellation_never_drives_an_aggregate() {
    let ((), report) = run_async_under_lab(0x636c_a603, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        db.write(&contexts.commit(), batch(1, 7)).await.unwrap();
        let options = options(SUMMARY);
        let prepared = PreparedNativeRead::prepare(SUMMARY, &options.params, &options).unwrap();
        for fail in [1, 2] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &options.params, policy()).unwrap();
            let columns = cursor.columns().to_vec();
            let pulls = Rc::new(Cell::new(0));
            let mut output = ObservedOutput { bytes: Vec::new(), pulls: Rc::clone(&pulls),
                flushed: Rc::new(Cell::new(0)), fail_on_flush: Some(fail) };
            let error = deliver(&columns, cursor.snapshot_seq().0,
                &mut cursor.by_ref().inspect(|_| pulls.set(pulls.get() + 1)), true, &mut output,
                || cx.checkpoint().map_err(Failure::query)).err().expect("broken output");
            assert_eq!(error.code, 5);
            assert_eq!(pulls.get(), fail - 1);
            if fail == 1 {
                assert_eq!(cursor.row_stats().snapshot_records, 0);
                assert_eq!(cursor.evaluator_stats().work_units, 0);
            }
            let stats = cursor.row_stats();
            cursor.close();
            assert!(cursor.next().is_none());
            assert_eq!(cursor.row_stats(), stats);
            assert!(!String::from_utf8(output.bytes).unwrap().contains("\"event\":\"result\""));
        }
        let mut cursor = prepared.stream_aggregate(&db, &cx, &options.params, policy()).unwrap();
        let columns = cursor.columns().to_vec();
        let mut bytes = Vec::new();
        let mut calls = 0;
        let error = deliver(&columns, cursor.snapshot_seq().0, &mut cursor, true, &mut bytes, || {
            calls += 1;
            if calls == 2 { Err(Failure::query("cancelled before demand")) } else { Ok(()) }
        }).err().expect("delivery cancellation");
        assert_eq!(error.code, 3);
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.evaluator_stats().work_units, 0);
        cursor.close();
        assert_eq!(cursor.state(), VertexScanState::Closed);
        assert!(cursor.next().is_none());
        assert_eq!(String::from_utf8(bytes).unwrap().lines().count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_late_data_and_quota_failures_release_no_partial_summary_or_success() {
    let ((), report) = run_async_under_lab(0x636c_a604, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        for id in 1..=3 { db.write(&contexts.commit(), batch(id, id as i64)).await.unwrap(); }
        let options = options("MATCH (n) RETURN AVG(n.p) AS average, MIN(n.p) AS low");
        let prepared = PreparedNativeRead::prepare(&options.text, &options.params, &options).unwrap();
        let mut baseline = prepared.stream_aggregate(&db, &cx, &options.params, policy()).unwrap();
        baseline.next().unwrap().unwrap();
        let stats = baseline.evaluator_stats();
        let records = baseline.row_stats().snapshot_records;
        for allowance in [GqlQueryPolicy::new(records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(records, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(records, 1, stats.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(records, 1, u64::MAX, stats.scratch_entries - 1)] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &options.params, allowance).unwrap();
            let columns = cursor.columns().to_vec();
            let mut bytes = Vec::new();
            let error = deliver(&columns, cursor.snapshot_seq().0, &mut cursor, true, &mut bytes,
                || cx.checkpoint().map_err(Failure::query)).err().expect("native quota refusal");
            assert_eq!(error.code, 3);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert_eq!(cursor.row_stats().result_rows, 0);
            assert!(cursor.next().is_none());
            let text = String::from_utf8(bytes).unwrap();
            assert_eq!(text.lines().count(), 1);
            assert!(row_lines(&text).is_empty());
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(3), PropertyKeyId(1),
            Some(CanonicalScalar::ucs_basic_text("secret invalid AVG operand").unwrap()));
        db.write(&contexts.commit(), edit).await.unwrap();
        let mut bytes = Vec::new();
        let error = run(&db, &cx, &options, true, &mut bytes).err().expect("late data error");
        assert_eq!(error.code, 3);
        assert!(!error.message.contains("secret invalid"));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(row_lines(&text).is_empty());
        assert!(!text.contains("\"event\":\"result\""));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_aggregate_clauses_and_future_history_refuse_before_transport() {
    let ((), report) = run_async_under_lab(0x636c_a605, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        db.write(&contexts.commit(), batch(1, 7)).await.unwrap();
        for text in [
            "MATCH (n) RETURN n.p AS p, COUNT(*) AS total GROUP BY n.p LIMIT 0",
            "MATCH (n) RETURN COUNT(DISTINCT n.p) AS total LIMIT 0",
            "MATCH (n) RETURN COLLECT(n.p) AS values",
            "MATCH (n) RETURN COUNT(*) AS total HAVING total > 0",
            "MATCH (n) RETURN COUNT(*) AS total LIMIT 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN AVG(n.p) AS average",
        ] {
            let options = options(text);
            let prepared = PreparedNativeRead::prepare(text, &options.params, &options).unwrap();
            assert!(matches!(prepared, PreparedNativeRead::Aggregate(_)
                | PreparedNativeRead::TemporalAggregate(_) | PreparedNativeRead::PipelineAggregate(_)), "{text}");
            let mut bytes = Vec::new();
            let error = run(&db, &cx, &options, true, &mut bytes).err().expect("physical/source refusal");
            assert_eq!(error.code, 3, "{}", error.message);
            assert!(bytes.is_empty(), "no header before full admission: {text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn aggregate_delivery_retains_the_cut_after_writer_and_definition_drop() {
    let ((), report) = run_async_under_lab(0x636c_a606, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys()).await.unwrap();
        db.write(&contexts.commit(), batch(1, 7)).await.unwrap();
        let options = options("MATCH (n) RETURN SUM(n.p) AS sum, AVG(n.p) AS average");
        let prepared = PreparedNativeRead::prepare(&options.text, &options.params, &options).unwrap();
        let mut cursor = prepared.stream_aggregate(&db, &cx, &options.params, policy()).unwrap();
        let columns = cursor.columns().to_vec();
        drop(prepared);
        drop(options);
        db.write(&contexts.commit(), batch(2, 99)).await.unwrap();
        drop(db);
        let mut bytes = Vec::new();
        okay(deliver(&columns, cursor.snapshot_seq().0, &mut cursor, true, &mut bytes,
            || cx.checkpoint().map_err(Failure::query)));
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(row_lines(&text), vec![concat!(
            "{\"v\":1,\"event\":\"row\",\"cells\":[{\"type\":\"wideint\",\"value\":\"7\"},",
            "{\"type\":\"average\",\"value\":\"7/1\"}]}")]);
        assert!(text.ends_with("\"seq\":1,\"count\":1}\n"));
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert!(cursor.next().is_none());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
