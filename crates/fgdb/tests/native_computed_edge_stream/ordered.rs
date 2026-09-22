//! Ranked pages over the same real database, history and occurrence oracle.
use super::*;
use core::cmp::Ordering;

fn statement(direction: usize, hops: usize, grouped: bool) -> String {
    text(direction, hops, grouped)
        + " HAVING COUNT(*)>0 AND (average>$floor OR total IS NULL) ORDER BY average DESC NULLS FIRST,occurrences DESC SKIP $skip LIMIT $limit"
}
#[allow(clippy::too_many_arguments)]
fn expected(
    cut: u64,
    scale: i64,
    direction: usize,
    hops: usize,
    grouped: bool,
    floor: i64,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphAggregateValue>> {
    let mut rows = output_oracle(cut, scale, direction, hops, grouped, floor, 0, usize::MAX);
    let (count, average) = if grouped { (2, 3) } else { (1, 2) };
    // Small fixture products fit i128. Do not call the production comparator,
    // compare enum tags, or round rational averages to floating point.
    rows.sort_by(|a, b| {
        let order = match (a[average].as_average(), b[average].as_average()) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(a), Some(b)) => (b.numerator() * i128::from(a.denominator()))
                .cmp(&(a.numerator() * i128::from(b.denominator()))),
        };
        order
            .then_with(|| b[count].as_count().cmp(&a[count].as_count()))
            .then_with(|| {
                if grouped {
                    a[1].cmp(&b[1])
                } else {
                    Ordering::Equal
                }
            })
    });
    rows.into_iter().skip(skip).take(limit).collect()
}

#[test]
fn ranked_native_pages_keep_bound_layouts_and_snapshot_history_after_reopen() {
    let ((), report) = run_async_under_lab(0xc0a5_5201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let view = db.read_session().unwrap();
        let mut paused = Vec::new();
        for direction in 0..3 {
            for hops in 1..=2 {
                for grouped in [false, true] {
                    let args = output_arguments(1, 2, -99, 0, 2);
                    let template = PreparedNativeRead::prepare(
                        &statement(direction, hops, grouped),
                        &args,
                        symbols(),
                    )
                    .unwrap();
                    let mut cursor = template.stream_aggregate(&db, &cx, &args, wide()).unwrap();
                    assert_eq!(cursor.kind(), ScanKind::Edge);
                    assert_eq!(cursor.row_stats().snapshot_records, 0);
                    let slots = cursor.output_slots().to_vec();
                    let first = cursor.next().transpose().unwrap().map(|row| {
                        slots
                            .iter()
                            .map(|slot| match *slot {
                                GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                                GraphAggregateTextSlot::GroupKey(at) => {
                                    GraphAggregateValue::Value(row.keys()[at].clone())
                                }
                            })
                            .collect::<Vec<_>>()
                    });
                    paused.push((direction, hops, grouped, first, cursor));
                    drop(template);
                    drop(args);
                }
            }
        }
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(10), Q, Some(CanonicalScalar::Int(4)));
        edit.delete_edge(EId(11));
        db.write(&commit, edit).await.unwrap();
        let mut cascade = WriteBatch::new(R);
        cascade.delete_vertex(VId(1));
        db.write(&commit, cascade).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        for (direction, hops, grouped, first, mut cursor) in paused {
            let mut rows: Vec<_> = first.into_iter().collect();
            rows.extend(drain(&mut cursor));
            assert_eq!(rows, expected(1, 2, direction, hops, grouped, -99, 0, 2));
            assert_eq!(cursor.snapshot_seq(), basis);
            let template = PreparedNativeRead::prepare(
                &statement(direction, hops, grouped),
                &output_arguments(1, 1, 0, 0, 1),
                symbols(),
            )
            .unwrap();
            for cut in 0..=3 {
                for scale in [1, 2] {
                    for floor in [-1, 10] {
                        for (skip, limit) in [(0, 0), (0, 1), (1, 2)] {
                            let args = output_arguments(cut, scale, floor, skip, limit);
                            let QueryResult::Rows { columns, rows } =
                                template.execute(&db, &cx, &args, wide()).unwrap()
                            else {
                                panic!("native rows");
                            };
                            let mut stream =
                                template.stream_aggregate(&db, &cx, &args, wide()).unwrap();
                            assert_eq!(stream.columns(), columns);
                            assert_eq!(drain(&mut stream), rows);
                            assert_eq!(
                                rows,
                                expected(
                                    cut,
                                    scale,
                                    direction,
                                    hops,
                                    grouped,
                                    floor,
                                    skip as usize,
                                    limit as usize
                                )
                            );
                            assert_eq!(stream.snapshot_seq(), CommitSeq(cut));
                            assert_eq!(stream.row_stats().result_rows, rows.len() as u64);
                        }
                    }
                }
            }
            let args = output_arguments(1, 2, -99, 0, 2);
            let mut pinned = template
                .stream_aggregate_in_view(&view, &cx, &args, wide())
                .unwrap();
            assert_eq!(
                drain(&mut pinned),
                expected(1, 2, direction, hops, grouped, -99, 0, 2)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_hidden_ranks_obey_selected_row_quotas_and_cannot_hide_late_bad_input() {
    let ((), report) = run_async_under_lab(0xc0a5_5202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..1024 {
            batch.create_vertex(VId(id), vec![], vec![]);
        }
        for id in 0..1024 {
            batch.add_edge(
                EId(id),
                VId(0),
                VId(id),
                vec![
                    (Q, CanonicalScalar::Int(id as i64)),
                    (P, CanonicalScalar::Int(3)),
                ],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n,SUM(r.quantity*r.price)+COUNT(*) AS adjusted GROUP BY b ORDER BY AVG(r.quantity*r.price) DESC,b ASC SKIP 1 LIMIT 1";
        let args = GqlParameters::new();
        let template = PreparedNativeRead::prepare(text, &args, symbols()).unwrap();
        let mut full = template
            .stream_aggregate(
                &db,
                &cx,
                &args,
                GqlQueryPolicy::new(1024, 1, u64::MAX, u64::MAX),
            )
            .unwrap();
        let rows = drain(&mut full);
        assert_eq!(
            rows,
            vec![vec![
                GraphAggregateValue::Value(GraphValue::Vertex(VId(1022))),
                GraphAggregateValue::Count(1),
                GraphAggregateValue::Integer(3067)
            ]]
        );
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(r.snapshot_records, 1, e.work_units, e.scratch_entries);
        assert_eq!(
            drain(&mut template.stream_aggregate(&db, &cx, &args, exact).unwrap()),
            rows
        );
        for policy in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut stream = template.stream_aggregate(&db, &cx, &args, policy).unwrap();
            assert!(stream.by_ref().collect::<Result<Vec<_>, _>>().is_err());
            assert_eq!(stream.state(), VertexScanState::Failed);
            assert!(stream.next().is_none());
        }
        let mut edit = WriteBatch::new(R);
        edit.set_edge_property(EId(1023), Q, Some(CanonicalScalar::Bool(true)));
        db.write(&commit, edit).await.unwrap();
        for count in [0, 1] {
            let text = format!(
                "MATCH (a)-[r:R]->(b) RETURN b,COUNT(*) AS n GROUP BY b ORDER BY AVG(r.quantity*r.price) DESC LIMIT {count}"
            );
            let mut stream = db
                .query_aggregate_stream(&cx, &text, &args, symbols(), wide())
                .unwrap();
            assert!(matches!(
                stream.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::InputExpression { .. }
                )))
            ));
            assert_eq!(stream.row_stats().result_rows, 0);
            assert!(stream.next().is_none());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
