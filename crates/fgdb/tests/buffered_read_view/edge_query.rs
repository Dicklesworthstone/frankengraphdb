//! Exercise the new source through the public buffered query API, real native
//! GQL preparation, and the durable Chronicle/Strata composition from history().
use super::*;
use asupersync::fs::Vfs;
use fgdb::BufferedReadView;
use fgdb_gql::edge_stream::{EdgeScanError, EdgeScanState};
use fgdb_strata::store::{BlockStore, RootReadLimits};
use fgdb_strata::tiered::sealed::{AdjRunSegmentVersion, SealedLimits};
use fgdb_types::QueryCx;
use fgdb_types::context::SimulationCheckpointProbe;
use std::sync::Arc;

// Independent finite fixture oracle. This is NOT the buffered point reader,
// an alternative executor, or expected values obtained from the new scan.
fn expected(
    cut: u64,
    direction: usize,
    any: bool,
    predicate: usize,
    skip: usize,
    limit: usize,
) -> Vec<Vec<GraphValue>> {
    let mut values = Vec::new();
    if cut != 0 {
        let a = if cut == 1 { 1 } else { 11 };
        for (eid, src, dst, ep, ap, bp) in [
            (101, 1, 2, if cut == 1 { 7 } else { 17 }, a, 2),
            (102, 1, 3, 8, a, 3),
            (103, 4, 1, 9, 4, a),
            (104, 1, 2, 10, a, 2),
            (105, 1, 4, 20, a, 4),
        ] {
            if eid == 102 && cut > 1 || eid == 105 && (cut < 3 || !any) {
                continue;
            }
            let orientations = match direction {
                0 => vec![(src, dst, ap, bp)],
                1 => vec![(dst, src, bp, ap)],
                _ => vec![(src, dst, ap, bp), (dst, src, bp, ap)],
            };
            for (src, dst, ap, bp) in orientations {
                if match predicate {
                    0 => false,
                    1 => ap >= bp,
                    2 => ep <= ap,
                    _ => true,
                } {
                    continue;
                }
                values.push(vec![
                    GraphValue::Edge(EId(eid)),
                    GraphValue::Vertex(VId(src)),
                    GraphValue::Vertex(VId(dst)),
                    GraphValue::Scalar(CanonicalScalar::Int(ep)),
                    GraphValue::Scalar(CanonicalScalar::Int(ap)),
                    GraphValue::Scalar(CanonicalScalar::Int(bp)),
                ]);
            }
        }
    }
    values.sort();
    values.into_iter().skip(skip).take(limit).collect()
}

fn statement(direction: usize, any: bool, predicate: usize, skip: usize, limit: usize) -> String {
    let relation = if any { "r" } else { "r:R" };
    let pattern = match direction {
        0 => format!("(a)-[{relation}]->(b)"),
        1 => format!("(a)<-[{relation}]-(b)"),
        _ => format!("(a)-[{relation}]-(b)"),
    };
    let predicate = [
        "",
        "WHERE a.p < b.p",
        "WHERE r.p > a.p",
        "WHERE a.missing < b.p",
    ][predicate];
    format!(
        "MATCH {pattern} {predicate} RETURN DISTINCT r, a, b, r.p AS ep, a.p AS ap, b.p AS bp SKIP {skip} LIMIT {limit}"
    )
}

#[test]
fn cold_edge_queries_match_192_independent_history_direction_filter_and_window_cases() {
    let ((), report) = run_async_under_lab(0x6275_e001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let mut cases = 0;
        for cut in 0..=3 {
            for direction in 0..3 {
                for any in [false, true] {
                    for predicate in 0..4 {
                        for (skip, count) in [(0, 100), (1, 2)] {
                            let text = statement(direction, any, predicate, skip, count);
                            let pattern = query(&text);
                            let identity = pattern.plan().canonical_bytes();
                            let mut cursor = view
                                .stream_graph_edges_governed_at(
                                    &cx,
                                    &pattern,
                                    CommitSeq(cut),
                                    query_policy(),
                                )
                                .unwrap();
                            assert_eq!(cursor.row_stats().snapshot_records, 0);
                            let mut actual = Vec::new();
                            while let Some(row) = cursor.next().await {
                                actual.push(row.unwrap().values().to_vec());
                                assert!(pool.used() <= pool.limit());
                            }
                            assert_eq!(
                                actual,
                                expected(cut, direction, any, predicate, skip, count),
                                "{text} at {cut}"
                            );
                            assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                            if count == 100 {
                                assert_eq!(
                                    cursor.row_stats().snapshot_records,
                                    if any { 5 } else { 4 },
                                    "typed roots admit only their relation's historical identities"
                                );
                            }
                            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
                            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                            assert!(cursor.next().await.is_none());
                            drop(cursor);
                            assert_eq!(pool.used(), baseline);
                            assert_eq!(pattern.plan().canonical_bytes(), identity);
                            cases += 1;
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 192);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_roots_spend_only_selected_history_while_alternations_keep_every_member() {
    let ((), report) = run_async_under_lab(0x6275_e005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        for cut in 0..=3 {
            for (selector, candidates, keep_r, keep_s) in [
                (":R", 4, true, false),
                (":S", 1, false, true),
                (":Missing", 0, false, false),
                ("", 5, true, true),
                (":R|S", 5, true, true),
                (":R|Missing", 5, true, false),
            ] {
                let text = format!(
                    "MATCH (a)-[r{selector}]->(b) RETURN r,a,b,r.p AS ep,a.p AS ap,b.p AS bp"
                );
                let pattern = PreparedGraphText::prepare(&text, |kind, name| match (kind, name) {
                    (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RELATION)),
                    (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(8))),
                    (GraphSymbolKind::Relation, "Missing") => {
                        Some(GraphSymbol::Relation(RelationId(9)))
                    }
                    (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
                    _ => None,
                })
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
                let expected: Vec<_> = expected(cut, 0, true, 0, 0, 100)
                    .into_iter()
                    .filter(|row| {
                        if row[0] == GraphValue::Edge(EId(105)) {
                            keep_s
                        } else {
                            keep_r
                        }
                    })
                    .collect();
                let allowance = GqlQueryPolicy::new(candidates, 100, 1_000_000, 1_000_000);
                let mut cursor = view
                    .stream_graph_edges_governed_at(&cx, &pattern, CommitSeq(cut), allowance)
                    .unwrap();
                let mut actual = Vec::new();
                while let Some(row) = cursor.next().await {
                    actual.push(
                        row.unwrap_or_else(|error| panic!("{text} at {cut}: {error}"))
                            .values()
                            .to_vec(),
                    );
                }
                assert_eq!(actual, expected, "{text} at {cut}");
                assert_eq!(cursor.row_stats().snapshot_records, candidates);
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                drop(cursor);
                assert_eq!(pool.used(), baseline);

                if candidates > 0 {
                    let allowance = GqlQueryPolicy::new(candidates - 1, 100, 1_000_000, 1_000_000);
                    let mut cursor = view
                        .stream_graph_edges_governed_at(&cx, &pattern, CommitSeq(cut), allowance)
                        .unwrap();
                    loop {
                        match cursor.next().await {
                            Some(Ok(_)) => {}
                            Some(Err(GqlQueryError::Rows(_))) => break,
                            other => panic!(
                                "one missing history admission must refuse: {text}: {other:?}"
                            ),
                        }
                    }
                    assert_eq!(cursor.state(), EdgeScanState::Failed);
                    assert!(cursor.next().await.is_none());
                    drop(cursor);
                    assert_eq!(pool.used(), baseline);
                }
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn typed_query_work_and_payload_faults_stay_bounded_as_unrelated_blocks_grow() {
    let ((), report) = run_async_under_lab(0x6275_e006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let pattern = query("MATCH (a)-[r:R]->(b) RETURN r, r.p AS p");
        let missing = PreparedGraphText::prepare(
            "MATCH (a)-[r:Missing]->(b) RETURN r",
            |kind, name| match (kind, name) {
                (GraphSymbolKind::Relation, "Missing") => {
                    Some(GraphSymbol::Relation(RelationId(u64::MAX)))
                }
                _ => None,
            },
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let expected: Vec<_> = [(101, 17), (103, 9), (104, 10)]
            .into_iter()
            .map(|(id, value)| {
                vec![
                    GraphValue::Edge(EId(id)),
                    GraphValue::Scalar(CanonicalScalar::Int(value)),
                ]
            })
            .collect();
        let mut published = 0;
        let mut allowance = 1_000_000;
        let mut selected_faults = None;
        for unrelated in [8, 128] {
            for index in published..unrelated {
                let mut batch = WriteBatch::new(RelationId(1_000 + index));
                batch.add_edge(
                    EId(1_000 + u128::from(index)),
                    VId(1),
                    VId(4),
                    vec![(PROPERTY, CanonicalScalar::Int(1))],
                );
                writer.write(&commit, batch).await.unwrap();
            }
            published = unrelated;
            let memory = pool();
            // Open authenticates the larger generation under its own source
            // allowance. The already-admitted query gets no such increase.
            let mut view = Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                &path,
                keys(),
                memory.clone(),
                limits(),
            )
            .await
            .unwrap();
            let baseline = memory.used();
            let before = view.buffer_stats();
            let mut cursor = view
                .stream_graph_edges_governed_at(
                    &cx,
                    &pattern,
                    CommitSeq(3),
                    GqlQueryPolicy::new(4, 3, allowance, 1_000_000),
                )
                .unwrap();
            let mut actual = Vec::new();
            while let Some(row) = cursor.next().await {
                actual.push(
                    row.unwrap_or_else(|error| panic!("{unrelated} unrelated blocks: {error}"))
                        .values()
                        .to_vec(),
                );
            }
            assert_eq!(actual, expected);
            assert_eq!(cursor.row_stats().snapshot_records, 4);
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            let work = cursor.evaluator_stats().work_units;
            drop(cursor);
            let faults = view.buffer_stats().misses - before.misses;
            assert!(faults > 0);
            if let Some(first) = selected_faults {
                assert_eq!(faults, first, "unrelated relation payloads stay unfaulted");
                assert!(work <= allowance);
            } else {
                selected_faults = Some(faults);
                // Two binary bounds may grow logarithmically. Another 120
                // unrelated descriptors must not add two full linear walks.
                allowance = work + 64;
            }
            assert_eq!(memory.used(), baseline);
            let before = view.buffer_stats();
            let mut empty = view
                .stream_graph_edges_governed_at(
                    &cx,
                    &missing,
                    CommitSeq(3),
                    GqlQueryPolicy::new(0, 0, allowance, 1_000_000),
                )
                .unwrap();
            assert!(empty.next().await.is_none());
            assert_eq!(empty.row_stats().snapshot_records, 0);
            assert_eq!(empty.state(), EdgeScanState::Exhausted);
            drop(empty);
            assert_eq!(view.buffer_stats(), before);
            assert_eq!(memory.used(), baseline);
            drop(view);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn query_rejection_zero_limits_unpolled_pulls_and_early_close_never_drain_input() {
    let ((), report) = run_async_under_lab(0x6275_e002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let unsupported = query("MATCH (a)-[r:R]->(b) RETURN b");
        assert!(matches!(
            view.stream_graph_edges_governed(&cx, &unsupported, query_policy()),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))
        ));
        let zero = query(&statement(2, true, 0, 0, 0));
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &zero, query_policy())
            .unwrap();
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        drop(cursor);
        let pattern = query(&statement(2, true, 0, 0, 100));
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &pattern, query_policy())
            .unwrap();
        let unpolled = cursor.next();
        drop(unpolled);
        assert_eq!(cursor.state(), EdgeScanState::Open);
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        cursor.close();
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(pool.used(), baseline);
        assert_eq!(view.buffer_stats().misses, 0);
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &pattern, query_policy())
            .unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        assert_eq!(first.values().to_vec(), expected(3, 2, true, 0, 0, 1)[0]);
        let examined = cursor.row_stats().snapshot_records;
        assert_eq!(examined, 1);
        cursor.close(); // Also drops the pending undirected orientation's record.
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, examined);
        drop(cursor);
        drop(view);
        assert!(pool.used() > 0, "only the delivered row retains its charge");
        drop(first);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_and_output_admission_are_cumulative_and_errors_fuse_with_no_prefix_receipt() {
    let ((), report) = run_async_under_lab(0x6275_e003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let baseline = pool.used();
        let pattern = query(&statement(2, true, 0, 0, 100));
        let mut cursor = view
            .stream_graph_edges_governed(
                &cx,
                &pattern,
                GqlQueryPolicy::new(0, 100, 1_000_000, 1_000_000),
            )
            .unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(view.buffer_stats().misses, 0);
        assert_eq!(pool.used(), baseline);
        let mut cursor = view
            .stream_graph_edges_governed(
                &cx,
                &pattern,
                GqlQueryPolicy::new(100, 1, 1_000_000, 1_000_000),
            )
            .unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(first);
        assert_eq!(pool.used(), baseline);
        // The source must reserve its decode workspace; it cannot use a new
        // private allowance or fall back to an eager resident snapshot.
        let hold = pool
            .reserve(&cx, pool.limit() - pool.used() - 32 * 1024)
            .unwrap();
        let held = pool.used();
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &pattern, query_policy())
            .unwrap();
        assert!(cursor.next().await.unwrap().is_err());
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(pool.used(), held);
        drop(hold);
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_live_buffered_query_keeps_its_checkpoint_through_later_write_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x6275_e004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let old_pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            &path,
            keys(),
            old_pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        let pattern = query(&statement(0, true, 0, 0, 100));
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &pattern, query_policy())
            .unwrap();
        let mut actual = vec![cursor.next().await.unwrap().unwrap().values().to_vec()];
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let mut change = WriteBatch::new(RELATION);
        change.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(99)));
        change.set_edge_property(EId(101), PROPERTY, Some(CanonicalScalar::Int(77)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        drop(writer);
        while let Some(row) = cursor.next().await {
            actual.push(row.unwrap().values().to_vec());
        }
        assert_eq!(actual, expected(3, 0, true, 0, 0, 100));
        assert_eq!(cursor.snapshot_seq(), CommitSeq(3));
        drop(cursor);
        drop(view);
        assert_eq!(old_pool.used(), 0);
        let new_pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs,
            &path,
            keys(),
            new_pool.clone(),
            limits(),
        )
        .await
        .unwrap();
        assert_eq!(view.frontier(), CommitSeq(4));
        let one = query(&statement(0, true, 0, 0, 1));
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &one, query_policy())
            .unwrap();
        let row = cursor.next().await.unwrap().unwrap();
        assert_eq!(
            row.values(),
            &[
                GraphValue::Edge(EId(101)),
                GraphValue::Vertex(VId(1)),
                GraphValue::Vertex(VId(2)),
                GraphValue::Scalar(CanonicalScalar::Int(77)),
                GraphValue::Scalar(CanonicalScalar::Int(99)),
                GraphValue::Scalar(CanonicalScalar::Int(2))
            ]
        );
        drop(cursor);
        drop(view);
        assert!(new_pool.used() > 0);
        drop(row);
        assert_eq!(new_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn native_run_limits() -> SealedLimits {
    SealedLimits {
        max_rows: 64,
        max_incidences: 128,
        max_image_bytes: 64 * 1024,
        max_property_bytes: 4096,
    }
}

fn native_run_policy(records: u64) -> GqlQueryPolicy {
    // Run decoding admits its declared capacities as well as actual directory
    // construction. This does not assume the Tier-D source's work total.
    GqlQueryPolicy::new(records, 100, 1_000_000, 1_000_000)
}

async fn native_run_history(
    commit: &CommitCx,
    cx: &QueryCx,
    floor: CommitSeq,
) -> (MemVfs, Database<MemVfs>, AdjRunSegmentVersion) {
    let (vfs, reference) = history(commit).await;
    drop(reference);
    let mut db = Database::open_with_vfs(commit, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut extra = WriteBatch::new(RELATION);
    extra.add_edge(
        EId(0),
        VId(4),
        VId(1),
        vec![(PROPERTY, CanonicalScalar::Int(71))],
    );
    extra.add_edge(
        EId(u128::MAX),
        VId(1),
        VId(1),
        vec![(PROPERTY, CanonicalScalar::Int(72))],
    );
    assert_eq!(db.write(commit, extra).await.unwrap(), CommitSeq(4));
    let run = db
        .seal_relation(
            commit,
            cx,
            RELATION,
            floor,
            native_run_limits(),
            RootReadLimits::default(),
        )
        .await
        .unwrap();
    (vfs, db, run)
}

async fn native_run_view(
    commit: &CommitCx,
    cx: &QueryCx,
    vfs: &MemVfs,
) -> (BufferedReadView<MemVfs>, MemoryPool) {
    let memory = MemoryPool::new(64 * 1024 * 1024, 0).unwrap();
    let mut source_limits = limits();
    source_limits.max_work = 1_000_000;
    let mut view = Database::open_buffered_read_view_with_vfs(
        commit,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        memory.clone(),
        source_limits,
    )
    .await
    .unwrap();
    view.set_native_run_limits(cx, Some(native_run_limits()))
        .unwrap();
    (view, memory)
}

fn native_run_query(
    direction: usize,
    selector: &str,
    tail: &str,
) -> PreparedGraphPattern<GraphValueRow> {
    let pattern = match direction {
        0 => format!("(a)-[r{selector}]->(b)"),
        1 => format!("(a)<-[r{selector}]-(b)"),
        _ => format!("(a)-[r{selector}]-(b)"),
    };
    PreparedGraphText::prepare(
        &format!("MATCH {pattern} RETURN r,a,b,r.p AS ep,a.p AS ap,b.p AS bp {tail}"),
        |kind, name| match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RELATION)),
            (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(8))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PROPERTY)),
            _ => None,
        },
    )
    .unwrap()
    .bind_parameters(&GqlParameters::new())
    .unwrap()
}

// Literal identities and values, already in the required EId order. CSR's
// source/destination order puts MAX before 101 and 0 after 104 instead.
fn native_run_expected(cut: u64, direction: usize, any: bool) -> Vec<Vec<GraphValue>> {
    if cut == 0 {
        return Vec::new();
    }
    let one = if cut >= 5 {
        41
    } else if cut >= 2 {
        11
    } else {
        1
    };
    let first = if cut >= 6 {
        77
    } else if cut >= 2 {
        17
    } else {
        7
    };
    let mut rows = Vec::new();
    for (eid, src, dst, ep, ap, bp, visible) in [
        (0, 4, 1, 71, 4, one, cut >= 4),
        (42, 2, 4, 62, 2, 4, cut >= 6),
        (101, 1, 2, first, one, 2, true),
        (102, 1, 3, 8, one, 3, cut == 1),
        (103, 4, 1, 9, 4, one, cut < 6),
        (104, 1, 2, 10, one, 2, true),
        (105, 1, 4, 20, one, 4, any && cut >= 3),
        (u128::MAX, 1, 1, 72, one, one, cut >= 4),
    ] {
        if !visible {
            continue;
        }
        let forward = (src, dst, ap, bp);
        let reverse = (dst, src, bp, ap);
        let orientations = match direction {
            0 => vec![forward],
            1 => vec![reverse],
            _ if src == dst => vec![forward],
            _ if src < dst => vec![forward, reverse],
            _ => vec![reverse, forward],
        };
        for (src, dst, ap, bp) in orientations {
            rows.push(vec![
                GraphValue::Edge(EId(eid)),
                GraphValue::Vertex(VId(src)),
                GraphValue::Vertex(VId(dst)),
                GraphValue::Scalar(CanonicalScalar::Int(ep)),
                GraphValue::Scalar(CanonicalScalar::Int(ap)),
                GraphValue::Scalar(CanonicalScalar::Int(bp)),
            ]);
        }
    }
    rows
}

async fn native_run_rows(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    cut: u64,
    identities: u64,
) -> Vec<Vec<GraphValue>> {
    let mut cursor = view
        .stream_graph_edges_governed_at(cx, pattern, CommitSeq(cut), native_run_policy(identities))
        .unwrap();
    let mut rows = Vec::new();
    while let Some(row) = cursor.next().await {
        rows.push(row.unwrap().values().to_vec());
    }
    assert_eq!(cursor.row_stats().snapshot_records, identities);
    assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
    assert_eq!(cursor.state(), EdgeScanState::Exhausted);
    assert!(cursor.next().await.is_none());
    rows
}

#[test]
fn persisted_native_match_keeps_eid_order_properties_and_history_through_carried_roots() {
    let ((), report) = run_async_under_lab(0x6275_e010, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, _) = native_run_history(&commit, &cx, CommitSeq(0)).await;
        let (mut pinned, pinned_pool) = native_run_view(&commit, &cx, &vfs).await;
        let pinned_root = pinned.partition_root();
        let baseline = pinned_pool.used();
        for cut in 0..=4 {
            for direction in 0..3 {
                let plan = native_run_query(direction, ":R", "");
                let definition = plan.plan().canonical_bytes();
                assert_eq!(
                    native_run_rows(&mut pinned, &cx, &plan, cut, 6).await,
                    native_run_expected(cut, direction, false)
                );
                assert_eq!(plan.plan().canonical_bytes(), definition);
                assert_eq!(pinned_pool.used(), baseline);
            }
        }

        // The carried run is unchanged, but visible endpoint properties belong
        // to the requested cut of the new envelope, not to the run's birth.
        let mut vertex = WriteBatch::new(RelationId(8));
        vertex.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(41)));
        assert_eq!(db.write(&commit, vertex).await.unwrap(), CommitSeq(5));
        let (mut carried, carried_pool) = native_run_view(&commit, &cx, &vfs).await;
        assert_ne!(carried.partition_root(), pinned_root);
        for cut in 0..=5 {
            for direction in 0..3 {
                let plan = native_run_query(direction, ":R", "");
                assert_eq!(
                    native_run_rows(&mut carried, &cx, &plan, cut, 6).await,
                    native_run_expected(cut, direction, false)
                );
            }
        }
        drop(carried);
        assert_eq!(carried_pool.used(), 0);
        let before = db.partition_root().unwrap();
        db.compact(&commit).await.unwrap();
        assert_ne!(
            db.partition_root().unwrap(),
            before,
            "real history compaction"
        );
        drop(db);

        let plan = native_run_query(0, ":R", "");
        assert_eq!(
            native_run_rows(&mut pinned, &cx, &plan, 4, 6).await,
            native_run_expected(4, 0, false)
        );
        assert_eq!(pinned.partition_root(), pinned_root);
        drop(pinned);
        assert_eq!(pinned_pool.used(), 0);
        let (mut reopened, memory) = native_run_view(&commit, &cx, &vfs).await;
        assert_eq!(reopened.frontier(), CommitSeq(5));
        for cut in [1, 4, 5] {
            assert_eq!(
                native_run_rows(&mut reopened, &cx, &plan, cut, 6).await,
                native_run_expected(cut, 0, false)
            );
        }
        drop(reopened);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selected_native_runs_supply_match_rows_and_corruption_never_falls_back() {
    let ((), report) = run_async_under_lab(0x6275_e011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut db, run) = native_run_history(&commit, &cx, CommitSeq(0)).await;
        let unrelated = db
            .seal_relation(
                &commit,
                &cx,
                RelationId(8),
                CommitSeq(0),
                native_run_limits(),
                RootReadLimits::default(),
            )
            .await
            .unwrap();
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
        )
        .await
        .unwrap();
        let selected = store
            .get_root(&commit, db.partition_root().unwrap())
            .await
            .unwrap();
        let mut redundant = None;
        for reference in selected.blocks {
            let bytes = store
                .get_bytes(&commit, fgdb_strata::DeltaBlockVersion(reference.block_id))
                .await
                .unwrap();
            if fgdb_strata::decode_block(&bytes)
                .unwrap()
                .iter()
                .any(|entry| entry.relation == RELATION)
            {
                redundant = Some(store.path(reference.block_id));
                break;
            }
        }
        let redundant = redundant.unwrap();
        let original_block = vfs.read(&redundant).await.unwrap();
        let run_path = store.path(run.0);
        let original_run = vfs.read(&run_path).await.unwrap();
        let unrelated_path = store.path(unrelated.0);
        let original_unrelated = vfs.read(&unrelated_path).await.unwrap();
        let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
        let plan = native_run_query(0, ":R", "");

        // Corruption follows complete owner admission. A Tier-D rescan cannot
        // produce this answer. The unrelated run must never be opened for :R;
        // relation selection here is an owner scope, not a Warden grant.
        vfs.write(&redundant, b"damaged redundant native MATCH source")
            .await
            .unwrap();
        vfs.write(&unrelated_path, b"unrelated run must remain untouched")
            .await
            .unwrap();
        assert_eq!(
            native_run_rows(&mut view, &cx, &plan, 4, 6).await,
            native_run_expected(4, 0, false)
        );
        assert!(
            Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                pool(),
                limits(),
            )
            .await
            .is_err(),
            "run reuse must not bypass complete initial source authentication"
        );
        vfs.write(&redundant, &original_block).await.unwrap();
        vfs.write(&run_path, b"damaged selected native MATCH run")
            .await
            .unwrap();

        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, native_run_policy(0))
            .unwrap();
        assert!(matches!(
            cursor.next().await,
            Some(Err(GqlQueryError::Rows(_)))
        ));
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        let zero = native_run_query(0, ":R", "LIMIT 0");
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &zero, native_run_policy(6))
            .unwrap();
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        drop(cursor);
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, native_run_policy(6))
            .unwrap();
        drop(cursor.next());
        assert_eq!(cursor.state(), EdgeScanState::Open);
        cursor.close();
        assert!(cursor.next().await.is_none());
        drop(cursor);

        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, native_run_policy(6))
            .unwrap();
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(
                error,
                GqlQueryError::Source(EdgeScanError::Source(BufferedReadError::Store(_)))
            ),
            "{error:?}"
        );
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        // Disabling the opt-in is an explicit new source selection; the failed
        // selected-run cursor above did not secretly retry through Tier D.
        view.set_native_run_limits(&cx, None).unwrap();
        assert_eq!(
            native_run_rows(&mut view, &cx, &plan, 4, 6).await,
            native_run_expected(4, 0, false)
        );
        vfs.write(&run_path, &original_run).await.unwrap();
        vfs.write(&unrelated_path, &original_unrelated)
            .await
            .unwrap();
        view.set_native_run_limits(&cx, Some(native_run_limits()))
            .unwrap();
        assert_eq!(
            native_run_rows(&mut view, &cx, &plan, 4, 6).await,
            native_run_expected(4, 0, false)
        );
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ineligible_native_runs_preserve_the_complete_existing_candidate_domain() {
    let ((), report) = run_async_under_lab(0x6275_e012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        for case in 0..4 {
            let floor = if case == 0 {
                CommitSeq(2)
            } else {
                CommitSeq(0)
            };
            let (vfs, mut db, run) = native_run_history(&commit, &cx, floor).await;
            if case == 1 {
                let mut vertex = WriteBatch::new(RELATION);
                vertex.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(41)));
                assert_eq!(db.write(&commit, vertex).await.unwrap(), CommitSeq(5));
                let mut suffix = WriteBatch::new(RELATION);
                suffix.set_edge_property(EId(101), PROPERTY, Some(CanonicalScalar::Int(77)));
                suffix.delete_edge(EId(103));
                suffix.add_edge(
                    EId(42),
                    VId(2),
                    VId(4),
                    vec![(PROPERTY, CanonicalScalar::Int(62))],
                );
                assert_eq!(db.write(&commit, suffix).await.unwrap(), CommitSeq(6));
            }
            let store = BlockStore::open_existing_for_read_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                [0xb1; 32],
                DatabaseSecurityNamespaceId([0xb2; 32]),
            )
            .await
            .unwrap();
            let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
            vfs.write(&store.path(run.0), b"ineligible run must not be decoded")
                .await
                .unwrap();
            let selector = match case {
                2 => "",
                3 => ":R|S",
                _ => ":R",
            };
            let plan = native_run_query(0, selector, "");
            let identities = if case == 0 { 6 } else { 7 };
            let cuts: &[u64] = if case == 1 { &[1, 4, 6] } else { &[1, 4] };
            for &cut in cuts {
                assert_eq!(
                    native_run_rows(&mut view, &cx, &plan, cut, identities).await,
                    native_run_expected(cut, 0, case >= 2),
                    "source eligibility case {case}, cut {cut}"
                );
            }
            drop(view);
            assert_eq!(memory.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn persisted_native_match_limits_cancellation_and_outputs_keep_their_reservations() {
    let ((), report) = run_async_under_lab(0x6275_e013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _, _) = native_run_history(&commit, &cx, CommitSeq(0)).await;
        let plan = native_run_query(0, ":R", "");
        let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
        let baseline = memory.used();
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, native_run_policy(5))
            .unwrap();
        loop {
            match cursor.next().await {
                Some(Ok(_)) => {}
                Some(Err(GqlQueryError::Rows(_))) => break,
                other => panic!("six historical identities require six admissions: {other:?}"),
            }
        }
        assert_eq!(cursor.row_stats().snapshot_records, 5);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(memory.used(), baseline);

        let hold = memory
            .reserve(&cx, memory.limit() - memory.used() - 64 * 1024)
            .unwrap();
        let held = memory.used();
        let mut cursor = view
            .stream_graph_edges_governed(&cx, &plan, native_run_policy(6))
            .unwrap();
        let error = cursor.next().await.unwrap().unwrap_err();
        assert!(
            matches!(
                error,
                GqlQueryError::Source(EdgeScanError::Source(
                    BufferedReadError::Memory(_)
                        | BufferedReadError::Buffer(
                            fgdb_strata::tiered::buffer::BufferError::Memory(_)
                        )
                ))
            ),
            "{error:?}"
        );
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(memory.used(), held);
        drop(hold);
        assert_eq!(
            native_run_rows(&mut view, &cx, &plan, 4, 6).await,
            native_run_expected(4, 0, false)
        );
        drop(view);
        assert_eq!(memory.used(), 0);

        // Measure one successful demand, then cancel a bounded set of early,
        // intermediate and final checkpoints on fresh owners. Capacity work
        // admission itself can contain many thousands of checkpoints; avoid a
        // quadratic sweep while still testing interruptions throughout demand.
        let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut cursor = view
            .stream_graph_edges_governed(&observed, &plan, native_run_policy(6))
            .unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        let checkpoints = probe.calls();
        assert!(checkpoints > 1);
        assert_eq!(
            first.values(),
            native_run_expected(4, 0, false)[0].as_slice()
        );
        cursor.close();
        drop(cursor);
        drop(view);
        assert!(
            memory.used() > 0,
            "delivered output still owns its reservation"
        );
        drop(first);
        assert_eq!(memory.used(), 0);
        let mut stops = vec![
            1,
            2,
            checkpoints / 4,
            checkpoints / 2,
            checkpoints - checkpoints / 4,
            checkpoints.saturating_sub(32),
            checkpoints.saturating_sub(8),
            checkpoints - 1,
            checkpoints,
        ];
        stops.retain(|stop| *stop > 0);
        stops.sort_unstable();
        stops.dedup();
        for stop in stops {
            let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
            let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
            match view.stream_graph_edges_governed(&interrupted, &plan, native_run_policy(6)) {
                Err(_) => {}
                Ok(mut cursor) => {
                    assert!(cursor.next().await.unwrap().is_err(), "checkpoint {stop}");
                    assert_eq!(cursor.state(), EdgeScanState::Failed);
                    assert!(cursor.next().await.is_none());
                }
            }
            assert_eq!(probe.calls(), stop);
            assert_eq!(
                native_run_rows(&mut view, &cx, &plan, 4, 6).await,
                native_run_expected(4, 0, false),
                "healthy retry after checkpoint {stop}"
            );
            drop(view);
            assert_eq!(memory.used(), 0, "checkpoint {stop}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn persisted_native_match_joins_and_correlated_probes_reuse_the_root_run() {
    let ((), report) = run_async_under_lab(0x6275_e014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, db, _) = native_run_history(&commit, &cx, CommitSeq(0)).await;
        let store = BlockStore::open_existing_for_read_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            [0xb1; 32],
            DatabaseSecurityNamespaceId([0xb2; 32]),
        )
        .await
        .unwrap();
        let selected = store
            .get_root(&commit, db.partition_root().unwrap())
            .await
            .unwrap();
        let mut redundant = Vec::new();
        for reference in selected.blocks {
            let bytes = store
                .get_bytes(&commit, fgdb_strata::DeltaBlockVersion(reference.block_id))
                .await
                .unwrap();
            if fgdb_strata::decode_block(&bytes)
                .unwrap()
                .iter()
                .any(|entry| entry.relation == RELATION)
            {
                let path = store.path(reference.block_id);
                let bytes = vfs.read(&path).await.unwrap();
                redundant.push((path, bytes));
            }
        }
        assert!(!redundant.is_empty());
        let (mut view, memory) = native_run_view(&commit, &cx, &vfs).await;
        let baseline = memory.used();
        // Every relation-R history was admitted before corruption. Both root
        // and nested reads must now use the authenticated run; a child source
        // that silently returns to its ordinary Tier-D path cannot succeed.
        for (path, _) in &redundant {
            vfs.write(path, b"nested native MATCH must reuse the selected run")
                .await
                .unwrap();
        }
        let maximum = u128::MAX;
        let cases = [
            (
                "-[s:R]->(c)",
                vec![
                    (0, 4, 101, 1, 2, 17),
                    (0, 4, 104, 1, 2, 10),
                    (0, 4, maximum, 1, 1, 72),
                    (103, 4, 101, 1, 2, 17),
                    (103, 4, 104, 1, 2, 10),
                    (103, 4, maximum, 1, 1, 72),
                    (maximum, 1, 101, 1, 2, 17),
                    (maximum, 1, 104, 1, 2, 10),
                ],
            ),
            (
                "<-[s:R]-(c)",
                vec![
                    (0, 4, 103, 1, 4, 9),
                    (0, 4, maximum, 1, 1, 72),
                    (101, 1, 104, 2, 1, 10),
                    (103, 4, 0, 1, 4, 71),
                    (103, 4, maximum, 1, 1, 72),
                    (104, 1, 101, 2, 1, 17),
                    (maximum, 1, 0, 1, 4, 71),
                    (maximum, 1, 103, 1, 4, 9),
                ],
            ),
            (
                "-[s:R]-(c)",
                vec![
                    (0, 4, 101, 1, 2, 17),
                    (0, 4, 103, 1, 4, 9),
                    (0, 4, 104, 1, 2, 10),
                    (0, 4, maximum, 1, 1, 72),
                    (101, 1, 104, 2, 1, 10),
                    (103, 4, 0, 1, 4, 71),
                    (103, 4, 101, 1, 2, 17),
                    (103, 4, 104, 1, 2, 10),
                    (103, 4, maximum, 1, 1, 72),
                    (104, 1, 101, 2, 1, 17),
                    (maximum, 1, 0, 1, 4, 71),
                    (maximum, 1, 101, 1, 2, 17),
                    (maximum, 1, 103, 1, 4, 9),
                    (maximum, 1, 104, 1, 2, 10),
                ],
            ),
        ];
        for (atom, expected) in cases {
            let text = format!("MATCH (a)-[r:R]->(b){atom} RETURN r,a,s,b,c,s.p AS sp");
            let plan = query(&text);
            let mut cursor = view
                .stream_graph_edge_joins_governed_at(
                    &cx,
                    &plan,
                    CommitSeq(4),
                    native_run_policy(1000),
                )
                .unwrap();
            let mut actual = Vec::new();
            while let Some(row) = cursor.next().await {
                actual.push(row.unwrap().values().to_vec());
            }
            let expected: Vec<_> = expected
                .into_iter()
                .map(|(r, a, s, b, c, property)| {
                    vec![
                        GraphValue::Edge(EId(r)),
                        GraphValue::Vertex(VId(a)),
                        GraphValue::Edge(EId(s)),
                        GraphValue::Vertex(VId(b)),
                        GraphValue::Vertex(VId(c)),
                        GraphValue::Scalar(CanonicalScalar::Int(property)),
                    ]
                })
                .collect();
            // Literal tuples include both parallel edges and one self-loop
            // occurrence. DIFFERENT EDGES excludes r == s within this clause.
            // The final root MAX still has children after root exhaustion.
            assert_eq!(actual, expected, "{text}");
            assert_eq!(cursor.snapshot_seq(), CommitSeq(4));
            assert_eq!(cursor.state(), EdgeScanState::Exhausted);
            assert!(cursor.next().await.is_none());
            drop(cursor);
            assert_eq!(memory.used(), baseline);
        }

        for cut in [1, 4] {
            for anti in [false, true] {
                let negation = if anti { "NOT " } else { "" };
                let text = format!(
                    "MATCH (a)-[r:R]->(b) WHERE {negation}EXISTS {{ \
                     MATCH (b)-[s:R]->(c) WHERE s.p>=70 }} RETURN r,a,b"
                );
                let plan = query(&text);
                let mut cursor = view
                    .stream_graph_edge_joins_governed_at(
                        &cx,
                        &plan,
                        CommitSeq(cut),
                        native_run_policy(1000),
                    )
                    .unwrap();
                let mut actual = Vec::new();
                while let Some(row) = cursor.next().await {
                    actual.push(row.unwrap().values().to_vec());
                }
                let roots = match (cut, anti) {
                    (1, false) => Vec::new(),
                    (1, true) => vec![(101, 1, 2), (102, 1, 3), (103, 4, 1), (104, 1, 2)],
                    (4, false) => vec![(0, 4, 1), (103, 4, 1), (maximum, 1, 1)],
                    (4, true) => vec![(101, 1, 2), (104, 1, 2)],
                    _ => unreachable!(),
                };
                let expected: Vec<_> = roots
                    .into_iter()
                    .map(|(r, a, b)| {
                        vec![
                            GraphValue::Edge(EId(r)),
                            GraphValue::Vertex(VId(a)),
                            GraphValue::Vertex(VId(b)),
                        ]
                    })
                    .collect();
                // The probe has its own edge-uniqueness domain: MAX may
                // witness itself. Its future version cannot witness at cut 1.
                assert_eq!(actual, expected, "{text}, cut {cut}");
                assert_eq!(cursor.state(), EdgeScanState::Exhausted);
                assert!(cursor.next().await.is_none());
                drop(cursor);
                assert_eq!(memory.used(), baseline);
            }
        }
        for (path, bytes) in redundant {
            vfs.write(&path, &bytes).await.unwrap();
        }
        drop(view);
        assert_eq!(memory.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
