//! Native edge reductions and DISTINCT statistics reach the production CLI.
use super::*;
use fgdb_gql::stream::VertexScanState;
use fgdb_types::EId;

fn edge_options(text: &str) -> Options {
    let args = ["--db", "unused", "--key-file", "unused", "--relation", "R=1",
        "--property", "p=1", "--stream", text].map(str::to_owned);
    okay(crate::parse(&args, "query"))
}
fn fixture() -> WriteBatch {
    let mut batch = WriteBatch::new(RelationId(1));
    for id in [0, 1] {
        batch.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), CanonicalScalar::Int(10))]);
    }
    for id in [1, 2] {
        batch.add_edge(EId(id), VId(0), VId(1), vec![(PropertyKeyId(1), CanonicalScalar::Int(4))]);
    }
    batch.add_edge(EId(3), VId(1), VId(1), vec![]);
    batch
}
const EDGE_SUMMARY: &str = "MATCH (a)-[r:R]-(b) RETURN COUNT(*) AS total,SUM(r.p) AS sum,COUNT(r.p) AS present";
const DISTINCT_SUMMARY: &str = "MATCH (n) RETURN COUNT(DISTINCT n.p) AS different,SUM(DISTINCT n.p) AS sum,AVG(DISTINCT n.p) AS avg";

#[test]
fn cli_native_edge_and_distinct_summaries_preserve_lossless_cells_and_metadata() {
    let ((), report) = run_async_under_lab(0x636c_e101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let at = db.write(&commit, fixture()).await.unwrap();
        let cases = [
            (EDGE_SUMMARY, concat!(
                "{\"v\":1,\"event\":\"row\",\"cells\":[",
                "{\"type\":\"count\",\"value\":\"5\"},",
                "{\"type\":\"wideint\",\"value\":\"16\"},",
                "{\"type\":\"count\",\"value\":\"4\"}]}")),
            (DISTINCT_SUMMARY, concat!(
                "{\"v\":1,\"event\":\"row\",\"cells\":[",
                "{\"type\":\"count\",\"value\":\"1\"},",
                "{\"type\":\"wideint\",\"value\":\"10\"},",
                "{\"type\":\"average\",\"value\":\"10/1\"}]}")),
        ];
        for (text, independent) in cases {
            let options = edge_options(text);
            let eager = db.query(&cx, text, &options.params, &options, policy()).unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, at.0, "rows", true, &mut expected));
            let mut bytes = Vec::new();
            okay(run(&db, &cx, &options, true, &mut bytes));
            let bytes = String::from_utf8(bytes).unwrap();
            assert_eq!(row_lines(&bytes), vec![independent]);
            assert_eq!(row_lines(&bytes), row_lines(&String::from_utf8(expected).unwrap()));
            assert!(bytes.starts_with(&format!("{{\"v\":1,\"event\":\"columns\",\"stream\":true,\"seq\":{},", at.0)));
            assert!(bytes.ends_with(&format!("\"seq\":{},\"count\":1}}\n", at.0)));
            let mut human = Vec::new();
            okay(run(&db, &cx, &options, false, &mut human));
            assert!(String::from_utf8(human).unwrap().ends_with(&format!("1 row(s) (stream complete at seq {})\n", at.0)));
        }
        let mut options = edge_options("MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ $seq WHERE r.p>$floor RETURN SUM(r.p) AS sum,COUNT(*) AS count");
        options.params = GqlParameters::new().with_uint64("seq", at.0).unwrap().with_int64("floor", 0).unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(100)));
        db.write(&commit, edit).await.unwrap();
        let mut bytes = Vec::new();
        okay(run(&db, &cx, &options, true, &mut bytes));
        let bytes = String::from_utf8(bytes).unwrap();
        assert_eq!(row_lines(&bytes), vec![concat!(
            "{\"v\":1,\"event\":\"row\",\"cells\":[",
            "{\"type\":\"wideint\",\"value\":\"8\"},{\"type\":\"count\",\"value\":\"2\"}]}"
        )]);
        assert!(bytes.ends_with(&format!("\"seq\":{},\"count\":1}}\n", at.0)));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_delivery_or_predemand_cancellation_does_not_drive_either_aggregate_family() {
    let ((), report) = run_async_under_lab(0x636c_e102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for text in [EDGE_SUMMARY, DISTINCT_SUMMARY] {
            let options = edge_options(text);
            let prepared = PreparedNativeRead::prepare(text, &options.params, &options).unwrap();
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
                cursor.close();
                assert!(cursor.next().is_none());
                assert!(!String::from_utf8(output.bytes).unwrap().contains("\"event\":\"result\""));
            }
            let mut cursor = prepared.stream_aggregate(&db, &cx, &options.params, policy()).unwrap();
            let columns = cursor.columns().to_vec();
            let mut output = Vec::new(); let mut polls = 0;
            let result = deliver(&columns, cursor.snapshot_seq().0, &mut cursor, true, &mut output, || {
                polls += 1;
                if polls == 2 { Err(Failure::query("stop before demand")) } else { Ok(()) }
            });
            assert!(result.is_err());
            assert_eq!(cursor.row_stats().snapshot_records, 0);
            assert_eq!(cursor.evaluator_stats().work_units, 0);
            cursor.close();
            assert_eq!(cursor.state(), VertexScanState::Closed);
            assert!(cursor.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_shape_refusals_precede_headers_and_late_data_refusals_emit_no_success() {
    let ((), report) = run_async_under_lab(0x636c_e103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN COLLECT(r.p) AS values",
            "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 2 RETURN COUNT(*) AS total",
        ] {
            let mut output = Vec::new();
            assert!(run(&db, &cx, &edge_options(text), true, &mut output).is_err(), "{text}");
            assert!(output.is_empty());
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_edge_property(EId(3), PropertyKeyId(1), Some(CanonicalScalar::ucs_basic_text("private edge operand").unwrap()));
        db.write(&commit, edit).await.unwrap();
        let mut output = Vec::new();
        let error = run(&db, &cx, &edge_options(EDGE_SUMMARY), true, &mut output).err().expect("data refusal");
        assert_eq!(error.code, 3);
        assert!(!error.message.contains("private edge operand"));
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.lines().count(), 1);
        assert!(row_lines(&output).is_empty());
        assert!(!output.contains("\"event\":\"result\""));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cli_edge_average_distinct_and_extrema_retain_exact_numeric_and_identity_cells() {
    let ((), report) = run_async_under_lab(0x636c_e104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let text = "MATCH (a)-[r:R]-(b) RETURN AVG(r.p) AS average,AVG(DISTINCT r.p) AS distinct_average,\
            SUM(DISTINCT r.p) AS sum,COUNT(DISTINCT r) AS edges,MIN(r) AS first,MAX(r) AS last";
        let options = edge_options(text);
        let mut output = Vec::new();
        okay(run(&db, &cx, &options, true, &mut output));
        let output = String::from_utf8(output).unwrap();
        assert_eq!(row_lines(&output), vec![concat!(
            "{\"v\":1,\"event\":\"row\",\"cells\":[",
            "{\"type\":\"average\",\"value\":\"4/1\"},",
            "{\"type\":\"average\",\"value\":\"4/1\"},",
            "{\"type\":\"wideint\",\"value\":\"4\"},",
            "{\"type\":\"count\",\"value\":\"3\"},",
            "{\"type\":\"edge\",\"value\":\"1\"},",
            "{\"type\":\"edge\",\"value\":\"3\"}]}"
        )]);
        let eager = db.query(&cx, text, &options.params, &options, policy()).unwrap();
        let mut expected = Vec::new();
        okay(crate::render(eager, 1, "rows", true, &mut expected));
        assert_eq!(row_lines(&output), row_lines(&String::from_utf8(expected).unwrap()));
        assert!(output.ends_with("\"seq\":1,\"count\":1}\n"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cli_having_output_expressions_and_pages_use_native_layouts_and_completion_counts() {
    let ((), report) = run_async_under_lab(0x636c_e111, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let text = "MATCH (a)-[r:R]-(b) RETURN COUNT(*) AS n,b AS destination,SUM(r.p)+COUNT(*) AS adjusted,AVG(r.p) AS average,b AS again GROUP BY b HAVING n>=2 SKIP 1 LIMIT 1";
        let opts = edge_options(text); let mut bytes = Vec::new();
        okay(run(&db, &cx, &opts, true, &mut bytes));
        let output = String::from_utf8(bytes).unwrap();
        let literal = concat!("{\"v\":1,\"event\":\"row\",\"cells\":[",
            "{\"type\":\"count\",\"value\":\"3\"},",
            "{\"type\":\"vertex\",\"value\":\"1\"},",
            "{\"type\":\"wideint\",\"value\":\"11\"},",
            "{\"type\":\"average\",\"value\":\"4/1\"},",
            "{\"type\":\"vertex\",\"value\":\"1\"}]}");
        assert_eq!(row_lines(&output), vec![literal]);
        assert!(output.ends_with("\"seq\":1,\"count\":1}\n"));
        let eager = db.query(&cx, text, &opts.params, &opts, policy()).unwrap();
        let mut expected = Vec::new(); okay(crate::render(eager, 1, "rows", true, &mut expected));
        assert_eq!(row_lines(&output), row_lines(&String::from_utf8(expected).unwrap()));
        let mut human = Vec::new(); okay(run(&db, &cx, &opts, false, &mut human));
        assert!(String::from_utf8(human).unwrap().ends_with("1 row(s) (stream complete at seq 1)\n"));
        for text in [
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n LIMIT 0",
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n HAVING n>99",
            "MATCH (a)-[r:R]->(b) RETURN 1/(COUNT(*)-3) AS rejected HAVING COUNT(*)<0 LIMIT 1",
        ] {
            let mut bytes = Vec::new(); okay(run(&db, &cx, &edge_options(text), true, &mut bytes));
            let output = String::from_utf8(bytes).unwrap();
            assert!(row_lines(&output).is_empty()); assert_eq!(output.lines().count(), 2);
            assert!(output.ends_with("\"seq\":1,\"count\":0}\n"));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cli_offpage_errors_never_emit_a_partial_group_or_success_record() {
    let ((), report) = run_async_under_lab(0x636c_e112, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for limit in [0, 1] {
            let text = format!("MATCH (a)-[r:R]-(b) RETURN b,1/(COUNT(*)-3) AS bad GROUP BY b LIMIT {limit}");
            let mut bytes = Vec::new();
            let error = run(&db, &cx, &edge_options(&text), true, &mut bytes).err().expect("off-page divide by zero");
            assert_eq!(error.code, 3);
            let output = String::from_utf8(bytes).unwrap();
            assert_eq!(output.lines().count(), 1); assert!(row_lines(&output).is_empty());
            assert!(!output.contains("\"event\":\"result\""));
        }
        let mut invalid = WriteBatch::new(RelationId(1));
        invalid.set_edge_property(EId(3), PropertyKeyId(1), Some(CanonicalScalar::Bool(true)));
        db.write(&commit, invalid).await.unwrap();
        let text = "MATCH (a)-[r:R]-(b) RETURN b,SUM(r.p) AS total GROUP BY b LIMIT 0";
        let mut bytes = Vec::new(); assert!(run(&db, &cx, &edge_options(text), true, &mut bytes).is_err());
        assert_eq!(String::from_utf8(bytes).unwrap().lines().count(), 1);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn output_page_flush_failure_never_demands_another_completed_group() {
    let ((), report) = run_async_under_lab(0x636c_e113, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let text = "MATCH (a)-[r:R]-(b) RETURN b,COUNT(*) AS n GROUP BY b HAVING n>0 LIMIT 2";
        let opts = edge_options(text);
        let prepared = PreparedNativeRead::prepare(text, &opts.params, &opts).unwrap();
        for fail in [1, 2, 3] {
            let mut cursor = prepared.stream_aggregate(&db, &cx, &opts.params, policy()).unwrap();
            let columns = cursor.columns().to_vec(); let slots = cursor.output_slots().to_vec();
            let pulls = Rc::new(Cell::new(0));
            let mut output = ObservedOutput { bytes: Vec::new(), pulls: pulls.clone(),
                flushed: Rc::new(Cell::new(0)), fail_on_flush: Some(fail) };
            let result = deliver(&columns, cursor.snapshot_seq().0,
                &mut cursor.by_ref().inspect(|_| pulls.set(pulls.get()+1)).map(|row|
                    row.map(|row| super::super::AggregateDeliveryRow { row, slots: &slots })),
                true, &mut output, || cx.checkpoint().map_err(Failure::query));
            let error = result.err().expect("broken output");
            assert_eq!(error.code, 5); assert_eq!(pulls.get(), fail-1);
            if fail==1 { assert_eq!(cursor.row_stats().snapshot_records, 0); }
            let stats = (cursor.row_stats(), cursor.evaluator_stats()); cursor.close();
            assert!(cursor.next().is_none()); assert_eq!((cursor.row_stats(), cursor.evaluator_stats()), stats);
            assert!(!String::from_utf8(output.bytes).unwrap().contains("\"event\":\"result\""));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cli_ranked_pages_preserve_exact_return_order_and_complete_empty_windows() {
    let ((), report) = run_async_under_lab(0x636c_e121, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        let text = "MATCH (a)-[r:R]-(b) RETURN COUNT(*) AS n,b AS destination,SUM(r.p)+COUNT(*) AS adjusted,AVG(r.p) AS average,b AS again GROUP BY b HAVING n>0 ORDER BY n DESC LIMIT 1";
        let opts = edge_options(text); let mut bytes = Vec::new();
        okay(run(&db,&cx,&opts,true,&mut bytes));
        let output = String::from_utf8(bytes).unwrap();
        assert_eq!(row_lines(&output), vec![concat!("{\"v\":1,\"event\":\"row\",\"cells\":[",
            "{\"type\":\"count\",\"value\":\"3\"},",
            "{\"type\":\"vertex\",\"value\":\"1\"},",
            "{\"type\":\"wideint\",\"value\":\"11\"},",
            "{\"type\":\"average\",\"value\":\"4/1\"},",
            "{\"type\":\"vertex\",\"value\":\"1\"}]}")]);
        assert!(output.ends_with("\"seq\":1,\"count\":1}\n"));
        let eager = db.query(&cx,text,&opts.params,&opts,policy()).unwrap();
        let mut expected = Vec::new(); okay(crate::render(eager,1,"rows",true,&mut expected));
        assert_eq!(row_lines(&output),row_lines(&String::from_utf8(expected).unwrap()));
        let mut human = Vec::new(); okay(run(&db,&cx,&opts,false,&mut human));
        assert!(String::from_utf8(human).unwrap().ends_with("1 row(s) (stream complete at seq 1)\n"));
        for (text, count) in [
            ("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n ORDER BY n",1),
            ("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n HAVING n>0 ORDER BY n",1),
            ("MATCH (a)-[r:R]->(b) RETURN SUM(r.p)+1 AS n ORDER BY n",1),
            ("MATCH (a)-[r:R]-(b) RETURN COUNT(*)+1 AS n GROUP BY b ORDER BY COUNT(*) DESC LIMIT 1",1),
            ("MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n ORDER BY n LIMIT 0",0),
            ("MATCH (a)-[r:R]->(b) RETURN 1/(COUNT(*)-3) AS bad HAVING COUNT(*)<0 ORDER BY COUNT(*) LIMIT 1",0),
        ] {
            let opts = edge_options(text); let mut bytes = Vec::new();
            okay(run(&db,&cx,&opts,true,&mut bytes)); let output = String::from_utf8(bytes).unwrap();
            assert_eq!(row_lines(&output).len(),count);
            let eager = db.query(&cx,text,&opts.params,&opts,policy()).unwrap();
            let mut expected = Vec::new(); okay(crate::render(eager,1,"rows",true,&mut expected));
            assert_eq!(row_lines(&output),row_lines(&String::from_utf8(expected).unwrap()));
            assert!(output.ends_with(&format!("\"seq\":1,\"count\":{count}}}\n")));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cli_ordered_late_failures_and_backpressure_never_emit_a_success_marker() {
    let ((), report) = run_async_under_lab(0x636c_e122, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query(); let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture()).await.unwrap();
        for limit in [0,1] {
            let text = format!("MATCH (a)-[r:R]-(b) RETURN b,1/(COUNT(*)-3) AS bad GROUP BY b ORDER BY COUNT(*) DESC LIMIT {limit}");
            let mut bytes = Vec::new();
            let error = run(&db,&cx,&edge_options(&text),true,&mut bytes).err().expect("late output failure");
            assert_eq!(error.code,3);
            let output = String::from_utf8(bytes).unwrap();
            assert_eq!(output.lines().count(),1); assert!(row_lines(&output).is_empty());
            assert!(!output.contains("\"event\":\"result\""));
        }
        let text = "MATCH (a)-[r:R]-(b) RETURN b,COUNT(*) AS n GROUP BY b ORDER BY n DESC LIMIT 2";
        let opts = edge_options(text); let prepared = PreparedNativeRead::prepare(text,&opts.params,&opts).unwrap();
        for fail in [1,2,3] {
            let mut cursor = prepared.stream_aggregate(&db,&cx,&opts.params,policy()).unwrap();
            let columns = cursor.columns().to_vec(); let slots = cursor.output_slots().to_vec();
            let pulls = Rc::new(Cell::new(0));
            let mut output = ObservedOutput { bytes:Vec::new(),pulls:pulls.clone(),
                flushed:Rc::new(Cell::new(0)),fail_on_flush:Some(fail) };
            let result = deliver(&columns,cursor.snapshot_seq().0,
                &mut cursor.by_ref().inspect(|_|pulls.set(pulls.get()+1)).map(|row|
                    row.map(|row|super::super::AggregateDeliveryRow{row,slots:&slots})),
                true,&mut output,||cx.checkpoint().map_err(Failure::query));
            assert_eq!(result.err().expect("broken output").code,5); assert_eq!(pulls.get(),fail-1);
            if fail==1 { assert_eq!(cursor.row_stats().snapshot_records,0); }
            let stats = (cursor.row_stats(),cursor.evaluator_stats()); cursor.close();
            assert!(cursor.next().is_none()); assert_eq!((cursor.row_stats(),cursor.evaluator_stats()),stats);
            assert!(!String::from_utf8(output.bytes).unwrap().contains("\"event\":\"result\""));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
