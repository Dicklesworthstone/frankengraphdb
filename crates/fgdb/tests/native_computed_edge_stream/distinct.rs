//! DISTINCT after complete hidden-key ranking, before the result window.
//! Reuse the parent's storage fixture, never its query result as the oracle.
use super::*;
use std::cmp::Ordering;

fn statement(direction: usize, hops: usize, mode: usize) -> String {
    let edge = |name, end| match direction {
        0 => format!("-[{name}:R]->({end})"),
        1 => format!("<-[{name}:R]-({end})"),
        _ => format!("-[{name}:R]-({end})"),
    };
    let mut body = format!("(a){}", edge("r", "b"));
    if hops == 2 {
        body += &edge("s", "c");
    }
    let end = if hops == 1 { "b" } else { "c" };
    let output = if mode == 0 {
        "COUNT(r.quantity)"
    } else {
        "COUNT(*)-COUNT(r.quantity)"
    };
    format!(
        "MATCH {body} FOR SYSTEM_TIME AS OF SEQ $cut RETURN DISTINCT {output} AS value \
        GROUP BY {end} HAVING COUNT(*) >= $minimum \
        ORDER BY AVG(r.quantity*r.price*$scale) DESC NULLS LAST SKIP $skip LIMIT $take"
    )
}
fn params(cut: u64, scale: i64, minimum: i64, skip: u64, take: u64) -> GqlParameters {
    arguments(cut, scale)
        .with_int64("minimum", minimum)
        .unwrap()
        .with_uint64("skip", skip)
        .unwrap()
        .with_uint64("take", take)
        .unwrap()
}

// Fully enumerate oriented occurrences; complete all groups; sort by their
// independent small-fixture fraction and full key; remove equal output values;
// only then apply the page. No production heap, index, parser or GLA is called.
// One argument per axis of the fixture matrix the oracle is compared across.
#[allow(clippy::too_many_arguments)]
fn expected(
    cut: u64,
    scale: i64,
    direction: usize,
    hops: usize,
    mode: usize,
    minimum: i64,
    skip: u64,
    take: u64,
) -> Vec<Vec<GraphAggregateValue>> {
    let mut arcs = Vec::new();
    for (_, from, to, quantity, price) in edges(cut) {
        let value = quantity.map(|q| i128::from(q) * i128::from(price) * i128::from(scale));
        if direction == 1 {
            arcs.push((to, from, value));
        } else {
            arcs.push((from, to, value));
            if direction == 2 && from != to {
                arcs.push((to, from, value));
            }
        }
    }
    let mut groups = BTreeMap::<VId, Vec<Option<i128>>>::new();
    for &(_, end, value) in &arcs {
        if hops == 1 {
            groups.entry(end).or_default().push(value);
        } else {
            for &(from, to, _) in &arcs {
                if from == end {
                    groups.entry(to).or_default().push(value);
                }
            }
        }
    }
    let mut finished: Vec<_> = groups
        .into_iter()
        .filter_map(|(key, values)| {
            let count = values.len() as u64;
            if i128::from(count) < i128::from(minimum) {
                return None;
            }
            let nonnull: Vec<_> = values.into_iter().flatten().collect();
            let present = nonnull.len() as u64;
            let rank = (present != 0).then(|| (nonnull.iter().sum::<i128>(), present));
            let output = if mode == 0 {
                GraphAggregateValue::Count(present)
            } else {
                GraphAggregateValue::Integer(i128::from(count - present))
            };
            Some((key, rank, output))
        })
        .collect();
    finished.sort_by(|a, b| {
        let order = match (a.1, b.1) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some((n, d)), Some((m, e))) => (m * i128::from(d)).cmp(&(n * i128::from(e))),
        };
        order.then_with(|| a.0.cmp(&b.0))
    });
    let mut seen = BTreeSet::new();
    finished
        .into_iter()
        .filter_map(|(_, _, value)| seen.insert(value.clone()).then_some(vec![value]))
        .skip(usize::try_from(skip).unwrap_or(usize::MAX))
        .take(usize::try_from(take).unwrap_or(usize::MAX))
        .collect()
}

#[test]
fn distinct_hidden_ranking_rebinding_and_pages_match_pinned_history_and_complete_output_oracles() {
    let ((), report) = run_async_under_lab(0xc0a5_d101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let basis = db.write(&commit, seed()).await.unwrap();
        let pinned = db.read_session().unwrap();
        let mut paused = Vec::new();
        for direction in 0..3 {
            for hops in 1..=2 {
                for mode in 0..2 {
                    let text = statement(direction, hops, mode);
                    let args = params(1, 1, 0, 0, u64::MAX);
                    let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
                    let mut cursor = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
                    assert_eq!(cursor.kind(), ScanKind::Edge);
                    assert!(cursor.key_columns().is_empty());
                    assert_eq!(cursor.columns(), &["value"]);
                    let first = cursor
                        .next()
                        .transpose()
                        .unwrap()
                        .map(|r| r.values().to_vec());
                    paused.push((direction, hops, mode, first, cursor));
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
        for (direction, hops, mode, first, mut old) in paused {
            let mut delivered: Vec<_> = first.into_iter().collect();
            delivered.extend(drain(&mut old));
            assert_eq!(
                delivered,
                expected(1, 1, direction, hops, mode, 0, 0, u64::MAX)
            );
            assert_eq!(old.snapshot_seq(), basis);
            let text = statement(direction, hops, mode);
            let prepared =
                PreparedNativeRead::prepare(&text, &params(1, 1, 0, 0, 2), symbols()).unwrap();
            for cut in 0..=3 {
                for scale in [1, -2] {
                    for minimum in [0, 2] {
                        for (skip, take) in [(0, 0), (0, 1), (1, 2)] {
                            let args = params(cut, scale, minimum, skip, take);
                            let want =
                                expected(cut, scale, direction, hops, mode, minimum, skip, take);
                            let QueryResult::Rows { columns, rows } =
                                prepared.execute(&db, &cx, &args, wide()).unwrap()
                            else {
                                panic!("expected native read");
                            };
                            let mut stream =
                                prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
                            assert_eq!(stream.columns(), columns);
                            assert_eq!(drain(&mut stream), want);
                            assert_eq!(rows, want);
                            assert_eq!(stream.row_stats().result_rows, want.len() as u64);
                            assert_eq!(stream.snapshot_seq(), CommitSeq(cut));
                        }
                    }
                }
            }
            let mut old = prepared
                .stream_aggregate_in_view(&pinned, &cx, &params(1, 1, 0, 0, 2), wide())
                .unwrap();
            assert_eq!(
                drain(&mut old),
                expected(1, 1, direction, hops, mode, 0, 0, 2)
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_summary_refusals_never_release_a_partial_page_and_zero_windows_keep_data_checks() {
    let ((), report) = run_async_under_lab(0xc0a5_d102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let text = statement(0, 1, 0);
        let args = params(1, 1, 0, 0, 2);
        let prepared = PreparedNativeRead::prepare(&text, &args, symbols()).unwrap();
        let mut full = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        let want = drain(&mut full);
        assert_eq!(want.len(), 2);
        let r = full.row_stats();
        let e = full.evaluator_stats();
        let exact = GqlQueryPolicy::new(
            r.snapshot_records,
            r.result_rows,
            e.work_units,
            e.scratch_entries,
        );
        let mut repeat = prepared.stream_aggregate(&db, &cx, &args, exact).unwrap();
        assert_eq!(drain(&mut repeat), want);
        for policy in [
            GqlQueryPolicy::new(r.snapshot_records - 1, 2, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 2, e.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 2, u64::MAX, e.scratch_entries - 1),
        ] {
            let mut stream = prepared.stream_aggregate(&db, &cx, &args, policy).unwrap();
            let mut prefix = Vec::new();
            loop {
                match stream.next() {
                    Some(Ok(row)) => prefix.push(row.values().to_vec()),
                    Some(Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))) => break,
                    other => panic!("quota refusal became another outcome: {other:?}"),
                }
            }
            assert_eq!(prefix, want[..prefix.len()]);
            assert_eq!(stream.row_stats().result_rows, prefix.len() as u64);
            assert_eq!(stream.state(), VertexScanState::Failed);
            assert!(stream.next().is_none());
        }
        let mut closed = prepared.stream_aggregate(&db, &cx, &args, wide()).unwrap();
        closed.close();
        closed.close();
        assert_eq!(closed.row_stats().snapshot_records, 0);
        assert!(closed.next().is_none());
        for limit in [0, 1] {
            let bad = format!(
                "MATCH (a)-[r:R]->(b) RETURN DISTINCT 1/(COUNT(*)-3) AS value \
                GROUP BY b ORDER BY COUNT(*) ASC LIMIT {limit}"
            );
            let mut stream = db
                .query_aggregate_stream(&cx, &bad, &GqlParameters::new(), symbols(), wide())
                .unwrap();
            assert!(matches!(
                stream.next(),
                Some(Err(GqlQueryError::Source(
                    GraphAggregateError::OutputExpression { .. }
                )))
            ));
            assert_eq!(stream.row_stats().result_rows, 0);
            assert!(stream.next().is_none());
        }
        let empty = "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 0 RETURN DISTINCT COUNT(*) AS n LIMIT 1";
        let mut zero = db
            .query_aggregate_stream(&cx, empty, &GqlParameters::new(), symbols(), wide())
            .unwrap();
        assert_eq!(
            zero.next().unwrap().unwrap().values()[0].as_count(),
            Some(0)
        );
        assert!(zero.next().is_none());
        let mut invalid = WriteBatch::new(R);
        invalid.set_edge_property(
            EId(15),
            Q,
            Some(CanonicalScalar::ucs_basic_text("private late operand").unwrap()),
        );
        db.write(&commit, invalid).await.unwrap();
        let mut stream = prepared
            .stream_aggregate(&db, &cx, &params(2, 1, 0, 0, 0), wide())
            .unwrap();
        assert!(matches!(
            stream.next(),
            Some(Err(GqlQueryError::Source(
                GraphAggregateError::InputExpression { .. }
            )))
        ));
        assert_eq!(stream.row_stats().result_rows, 0);
        assert!(!format!("{stream:?}").contains("private late operand"));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn many_groups_share_few_ranked_distinct_classes_under_only_the_selected_output_allowance() {
    let ((), report) = run_async_under_lab(0xc0a5_d103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(HIGH, vec![], vec![]);
        for id in 0..1024_u128 {
            batch.create_vertex(VId(id), vec![], vec![]);
            batch.add_edge(
                EId(id),
                HIGH,
                VId(id),
                vec![(Q, CanonicalScalar::Int((id % 257) as i64))],
            );
        }
        db.write(&commit, batch).await.unwrap();
        let text = "MATCH (a)-[r:R]->(b) RETURN DISTINCT SUM(r.quantity) AS value \
            GROUP BY b ORDER BY b DESC SKIP 1 LIMIT 2";
        let mut cursor = db
            .query_aggregate_stream(
                &cx,
                text,
                &GqlParameters::new(),
                symbols(),
                GqlQueryPolicy::new(1024, 2, 20_000_000, 2_000_000),
            )
            .unwrap();
        assert_eq!(
            drain(&mut cursor),
            vec![
                vec![GraphAggregateValue::Integer(251)],
                vec![GraphAggregateValue::Integer(250)]
            ]
        );
        assert_eq!(cursor.row_stats().snapshot_records, 1024);
        assert_eq!(cursor.row_stats().result_rows, 2);
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
