//! Native indexed matches -> authenticated runs -> query-owned order/window.
use super::*;
use fgdb_gql::algebra::GraphValue;
use fgdb_types::EId;

async fn multigraph(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = seed(cx, 8).await;
    let mut edges = WriteBatch::new(RelationId(1));
    for (id, from, to, value) in [
        (0, 0, 1, Some(2)),
        (1, 0, 1, Some(1)),
        (2, 1, 1, Some(3)),
        (3, 1, 2, None),
        (4, 1, 2, None),
    ] {
        let props = match value {
            Some(value) => vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
            None if id == 4 => vec![(PropertyKeyId(1), CanonicalScalar::Null)],
            None => vec![],
        };
        edges.add_edge(EId(id), VId(from), VId(to), props);
    }
    db.write(cx, edges).await.unwrap();
    db
}

fn scalar_frames(values: &[Option<i64>]) -> Vec<Vec<u8>> {
    values
        .iter()
        .map(|value| {
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
            )])
            .canonical_bytes()
            .unwrap()
        })
        .collect()
}

#[test]
fn undirected_parallel_occurrences_are_sorted_before_distinct_and_pagination() {
    let ((), report) = run_async_under_lab(0xe50a_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = multigraph(&c.commit()).await;
        let pool = MemoryPool::new(32_768, 0).unwrap();
        // Independently counted: four non-self edges have two orientations;
        // the self-loop has one. Missing and explicit NULL form one class.
        for (distinct, suffix, values) in [
            (false, "", vec![None, None, None, None, Some(3), Some(2), Some(2), Some(1), Some(1)]),
            (true, "", vec![None, Some(3), Some(2), Some(1)]),
            (false, "SKIP 1 LIMIT 2", vec![None, None]),
            (true, "SKIP 1 LIMIT 2", vec![Some(3), Some(2)]),
            (true, "LIMIT 0", vec![]),
            (true, "SKIP 4", vec![]),
        ] {
            let text = format!(
                "MATCH (a)-[e:R]-(b) RETURN {}e.p AS value ORDER BY value DESC NULLS FIRST {suffix}",
                if distinct { "DISTINCT " } else { "" }
            );
            let prepared = plan(&text);
            let params = GqlParameters::new();
            assert!(prepared.stream(&db, &cx, &params, policy()).is_err());
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let mut allowance = policy();
            allowance.rows = GqlExecutionBudget::new(5, values.len() as u64);
            let (spool, _) = prepared
                .spool_ordered(
                    &db, &cx, &params, allowance, &mut scratch, &mut destination,
                    1, 9, 31, 4096, 9, 1_000_000,
                )
                .await
                .unwrap();
            assert_eq!(spool.kind(), ScanKind::Edge);
            assert_eq!(spool.columns(), &["value"]);
            assert_eq!(spool.row_stats().snapshot_records, 5);
            assert_eq!(spool.row_count(), values.len() as u64);
            assert_eq!(contents(&spool, &mut destination, &cx).await, scalar_frames(&values));
            assert_eq!(pool.used(), 0);
        }
        // Move the former edge-LIMIT-0 refusal into an actual success law.
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let (spool, _) = plan("MATCH (a)-[e:R]->(b) RETURN e AS edge, a AS source LIMIT 0")
            .spool_ordered(
                &db, &cx, &GqlParameters::new(), policy(), &mut scratch,
                &mut destination, 1, 5, 31, 4096, 5, 1_000_000,
            )
            .await
            .unwrap();
        assert_eq!(spool.row_count(), 0);
        assert_eq!(spool.row_stats().snapshot_records, 5);
        assert_eq!(spool.columns(), &["edge", "source"]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn chains_branches_cycles_paths_and_private_probes_match_the_eager_engine() {
    let ((), report) = run_async_under_lab(0xe50a_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = multigraph(&c.commit()).await;
        let mut extra = WriteBatch::new(RelationId(1));
        for (id, from, to) in [(5, 2, 0), (6, 3, 4), (7, 4, 3), (8, 6, 7)] {
            extra.add_edge(EId(id), VId(from), VId(to), vec![(PropertyKeyId(1), CanonicalScalar::Int(1))]);
        }
        db.write(&c.commit(), extra).await.unwrap();
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for text in [
            "MATCH (a)-[e:R]->(b)-[f:R]->(c) WHERE e.p >= 0 AND f.p >= 0 RETURN f.p AS cost, c AS target, a AS source ORDER BY cost DESC, target ASC SKIP 1 LIMIT 5",
            "MATCH (a)<-[e:R]-(b)<-[f:R]-(c) RETURN c.p AS cost, a AS target ORDER BY cost ASC NULLS FIRST",
            "MATCH (a)-[e:R]-(b)-[f:R]-(c) RETURN DISTINCT a.p AS cost, c AS target ORDER BY cost DESC NULLS LAST SKIP 1 LIMIT 5",
            "MATCH (a)-[e:R]->(b), (a)-[f:R]->(c) RETURN c AS target, f.p AS cost, b AS other ORDER BY cost DESC NULLS FIRST LIMIT 7",
            "MATCH (a)-[e:R]->(b)-[f:R]->(a) RETURN b AS target, f.p AS cost ORDER BY cost DESC NULLS FIRST",
            "MATCH p=(a)-[e:R]->(b)-[f:R]->(c) RETURN p AS path, nodes(p) AS vertices, edges(p) AS relationships ORDER BY path DESC LIMIT 5",
            "MATCH (a)-[e:R]->(b) WHERE EXISTS { MATCH (b)-[x:R]->(z) WHERE x.p > 0 } RETURN b.q AS payload, e.p AS cost ORDER BY cost DESC NULLS LAST",
            "MATCH (a)-[e:R]->(b) WHERE NOT EXISTS { MATCH (b)-[x:R]->(z) WHERE x.p > 0 } RETURN b AS target, e.p AS cost ORDER BY cost DESC NULLS FIRST",
        ] {
            let wanted = expected(&view, &cx, text);
            for run_rows in [1, 3] {
                let (mut scratch, _) = file(&cx, &pool).await;
                let (mut destination, _) = file(&cx, &pool).await;
                let (spool, _) = plan(text)
                    .spool_ordered_in_view(
                        &view, &cx, &GqlParameters::new(), policy(), &mut scratch,
                        &mut destination, run_rows, 128, 127, 4096, 1000, 100_000_000,
                    )
                    .await
                    .unwrap();
                assert_eq!(spool.kind(), ScanKind::Edge);
                assert_eq!(spool.snapshot_seq(), view.frontier());
                assert_eq!(spool.row_count(), wanted.len() as u64);
                assert_eq!(contents(&spool, &mut destination, &cx).await, wanted, "{text}");
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn joined_output_exceeds_the_pool_without_retaining_the_match_population() {
    let ((), report) = run_async_under_lab(0xe50a_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = seed(&c.commit(), 32).await;
        let mut edges = WriteBatch::new(RelationId(1));
        for from in 1..32 {
            for parallel in 0..2 {
                edges.add_edge(
                    EId(from * 2 + parallel), VId(from), VId(from - 1),
                    vec![(PropertyKeyId(1), CanonicalScalar::Int(parallel as i64))],
                );
            }
        }
        db.write(&c.commit(), edges).await.unwrap();
        let view = db.read_session().unwrap();
        let text = "MATCH (a)-[e:R]->(b)-[f:R]->(c) RETURN c.q AS payload, e.p AS cost, c AS target ORDER BY cost DESC, target ASC";
        let wanted = expected(&view, &cx, text);
        assert_eq!(wanted.len(), 30 * 2 * 2);
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let (spool, _) = plan(text)
            .spool_ordered(
                &db, &cx, &GqlParameters::new(), policy(), &mut scratch,
                &mut destination, 3, 40, 257, 4096, 120, 100_000_000,
            )
            .await
            .unwrap();
        assert!(spool.encoded_len() > pool.limit());
        assert_eq!(pool.used(), 0);
        assert_eq!(contents(&spool, &mut destination, &cx).await, wanted);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn edge_history_is_pinned_before_await_and_temporal_cut_never_becomes_head() {
    let ((), report) = run_async_under_lab(0xe50a_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = multigraph(&c.commit()).await;
        let view = db.read_session().unwrap();
        let at = view.frontier();
        let weak = Arc::downgrade(&db.snapshot);
        let text = "MATCH (a)-[e:R]->(b) RETURN e.p AS cost, b AS target ORDER BY cost DESC NULLS LAST";
        let wanted = expected(&view, &cx, text);
        let prepared = plan(text);
        let params = GqlParameters::new();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let future = prepared.spool_ordered(
            &db, &cx, &params, policy(), &mut scratch, &mut destination,
            2, 5, 31, 4096, 5, 1_000_000,
        );
        let mut drift = WriteBatch::new(RelationId(1));
        drift.set_edge_property(EId(0), PropertyKeyId(1), Some(CanonicalScalar::Int(999)));
        drift.delete_vertex(VId(1));
        db.write(&c.commit(), drift).await.unwrap();
        let newest = db.read_session().unwrap();
        drop(view);
        drop(db);
        drop(prepared);
        drop(params);
        let (spool, _) = future.await.unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(spool.snapshot_seq(), at);
        assert_eq!(contents(&spool, &mut destination, &cx).await, wanted);
        let historical = plan(&format!(
            "MATCH (a)-[e:R]->(b) FOR SYSTEM_TIME AS OF SEQ {} RETURN e.p AS cost, b AS target ORDER BY cost DESC NULLS LAST",
            at.0,
        ));
        let (spool, _) = historical
            .spool_ordered_in_view(
                &newest, &cx, &GqlParameters::new(), policy(), &mut scratch,
                &mut destination, 2, 5, 31, 4096, 5, 1_000_000,
            )
            .await
            .unwrap();
        assert_eq!(spool.snapshot_seq(), at);
        assert_eq!(contents(&spool, &mut destination, &cx).await, wanted);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_edge_data_errors_and_each_row_allowance_refuse_without_a_result_handle() {
    let ((), report) = run_async_under_lab(0xe50a_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = multigraph(&c.commit()).await;
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let prepared = plan("MATCH (a)-[e:R]-(b) RETURN DISTINCT e.p AS value ORDER BY value DESC NULLS FIRST LIMIT 1");
        for (records, input, output, dimension, limit) in [
            (4, 9, 1, GqlBudgetDimension::SnapshotRecords, 4),
            (5, 8, 1, GqlBudgetDimension::ResultRows, 8),
            (5, 9, 0, GqlBudgetDimension::ResultRows, 0),
        ] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let mut allowance = policy();
            allowance.rows = GqlExecutionBudget::new(records, output);
            let error = prepared
                .spool_ordered(
                    &db, &cx, &GqlParameters::new(), allowance, &mut scratch,
                    &mut destination, 1, 9, 31, 4096, input, 1_000_000,
                )
                .await
                .unwrap_err();
            assert!(matches!(error.execution_error(), Some(GqlQueryError::Rows(e))
                if e.dimension == dimension && e.limit == limit && e.observed == limit + 1));
            assert_eq!(pool.used(), 0);
        }
        let mut later = WriteBatch::new(RelationId(1));
        later.add_edge(EId(99), VId(6), VId(7), vec![(PropertyKeyId(1), CanonicalScalar::Int(0))]);
        db.write(&c.commit(), later).await.unwrap();
        for limit in [0, 1] {
            let prepared = plan(&format!(
                "MATCH (a)-[e:R]->(b) WHERE e.p / e.p = 1 RETURN e.p AS value ORDER BY value DESC LIMIT {limit}"
            ));
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let error = prepared
                .spool_ordered(
                    &db, &cx, &GqlParameters::new(), policy(), &mut scratch,
                    &mut destination, 1, 10, 31, 4096, 10, 1_000_000,
                )
                .await
                .unwrap_err();
            assert!(matches!(error.execution_error(), Some(GqlQueryError::Data(_))));
            assert_eq!(destination.stats().published_runs, 0);
            assert!(destination.is_poisoned());
            assert_eq!(scratch.stats(), SpillStats::default());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unpolled_and_dropped_edge_spools_release_their_pin_without_fallback() {
    let ((), report) = run_async_under_lab(0xe50a_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = multigraph(&c.commit()).await;
        let weak = Arc::downgrade(&db.snapshot);
        let prepared = plan("MATCH (a)-[e:R]->(b)-[f:R]->(c) RETURN f.p AS value ORDER BY value DESC LIMIT 1");
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, backing) = file(&cx, &pool).await;
        drop(prepared.spool_ordered(
            &db, &cx, &params, policy(), &mut scratch, &mut destination,
            1, 100, 31, 4096, 100, 1_000_000,
        ));
        assert_eq!(scratch.stats(), SpillStats::default());
        assert_eq!(destination.stats(), SpillStats::default());
        backing.0.lock().unwrap().pending_write = true;
        let future = prepared.spool_ordered(
            &db, &cx, &params, policy(), &mut scratch, &mut destination,
            1, 100, 31, 4096, 100, 1_000_000,
        );
        drop(db);
        drop(prepared);
        drop(params);
        {
            let mut future = std::pin::pin!(future);
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            assert!(weak.upgrade().is_some());
        }
        assert!(weak.upgrade().is_none());
        assert!(destination.is_poisoned());
        assert_eq!(destination.stats().published_runs, 0);
        assert_eq!(scratch.stats(), SpillStats::default());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_edge_instructions_remain_ineligible_even_with_zero_limits() {
    let ((), report) = run_async_under_lab(0xe50a_0007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = multigraph(&c.commit()).await;
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for text in [
            "MATCH (a)-[e:R]->(b) OPTIONAL MATCH (b)-[f:R]->(c) RETURN e AS edge, a AS source LIMIT 0",
            "MATCH (a)-[:R*1..2]->(b) RETURN b AS target LIMIT 0",
            "MATCH (a)-[e:R]->(b) RETURN type(e) AS kind LIMIT 0",
        ] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let error = plan(text)
                .spool_ordered(
                    &db,
                    &cx,
                    &GqlParameters::new(),
                    GqlQueryPolicy::new(0, 0, 0, 0),
                    &mut scratch,
                    &mut destination,
                    1,
                    1,
                    31,
                    4096,
                    0,
                    1,
                )
                .await
                .unwrap_err();
            assert!(error.prepare_error().is_some(), "{text}");
            assert_eq!(scratch.stats(), SpillStats::default());
            assert_eq!(destination.stats(), SpillStats::default());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
