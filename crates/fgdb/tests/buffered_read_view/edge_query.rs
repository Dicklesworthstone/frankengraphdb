//! Exercise the new source through the public buffered query API, real native
//! GQL preparation, and the durable Chronicle/Strata composition from history().
use super::*;
use fgdb_gql::edge_stream::{EdgeScanError, EdgeScanState};

// Independent finite fixture oracle. This is NOT the buffered point reader,
// an alternative executor, or expected values obtained from the new scan.
fn expected(cut: u64, direction: usize, any: bool, predicate: usize,
    skip: usize, limit: usize) -> Vec<Vec<GraphValue>> {
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
            if eid == 102 && cut > 1 || eid == 105 && (cut < 3 || !any) { continue; }
            let orientations = match direction {
                0 => vec![(src, dst, ap, bp)],
                1 => vec![(dst, src, bp, ap)],
                _ => vec![(src, dst, ap, bp), (dst, src, bp, ap)],
            };
            for (src, dst, ap, bp) in orientations {
                if match predicate { 0 => false, 1 => ap >= bp, 2 => ep <= ap, _ => true } {
                    continue;
                }
                values.push(vec![GraphValue::Edge(EId(eid)), GraphValue::Vertex(VId(src)),
                    GraphValue::Vertex(VId(dst)), GraphValue::Scalar(CanonicalScalar::Int(ep)),
                    GraphValue::Scalar(CanonicalScalar::Int(ap)), GraphValue::Scalar(CanonicalScalar::Int(bp))]);
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
    let predicate = ["", "WHERE a.p < b.p", "WHERE r.p > a.p", "WHERE a.missing < b.p"][predicate];
    format!("MATCH {pattern} {predicate} RETURN DISTINCT r, a, b, r.p AS ep, a.p AS ap, b.p AS bp SKIP {skip} LIMIT {limit}")
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
            &commit, vfs, &path, keys(), pool.clone(), limits()).await.unwrap();
        let baseline = pool.used();
        let mut cases = 0;
        for cut in 0..=3 {
            for direction in 0..3 {
                for any in [false, true] {
                    for predicate in 0..4 {
                        for (skip, count) in [(0,100), (1,2)] {
                            let text = statement(direction, any, predicate, skip, count);
                            let pattern = query(&text);
                            let identity = pattern.plan().canonical_bytes();
                            let mut cursor = view.stream_graph_edges_governed_at(
                                &cx, &pattern, CommitSeq(cut), query_policy()).unwrap();
                            assert_eq!(cursor.row_stats().snapshot_records, 0);
                            let mut actual = Vec::new();
                            while let Some(row) = cursor.next().await {
                                actual.push(row.unwrap().values().to_vec());
                                assert!(pool.used() <= pool.limit());
                            }
                            assert_eq!(actual, expected(cut, direction, any, predicate, skip, count), "{text} at {cut}");
                            assert_eq!(cursor.row_stats().result_rows, actual.len() as u64);
                            if count == 100 { assert_eq!(cursor.row_stats().snapshot_records, 5); }
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
fn query_rejection_zero_limits_unpolled_pulls_and_early_close_never_drain_input() {
    let ((), report) = run_async_under_lab(0x6275_e002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = history(&commit).await;
        let path = vfs.database_dir();
        let pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit, vfs, &path, keys(), pool.clone(), limits()).await.unwrap();
        let baseline = pool.used();
        let unsupported = query("MATCH (a)-[r:R]->(b) RETURN b");
        assert!(matches!(view.stream_graph_edges_governed(&cx, &unsupported, query_policy()),
            Err(GqlQueryError::Source(EdgeScanError::Plan(_)))));
        let zero = query(&statement(2, true, 0, 0, 0));
        let mut cursor = view.stream_graph_edges_governed(&cx, &zero, query_policy()).unwrap();
        assert!(cursor.next().await.is_none());
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.state(), EdgeScanState::Exhausted);
        drop(cursor);
        let pattern = query(&statement(2, true, 0, 0, 100));
        let mut cursor = view.stream_graph_edges_governed(&cx, &pattern, query_policy()).unwrap();
        let unpolled = cursor.next();
        drop(unpolled);
        assert_eq!(cursor.state(), EdgeScanState::Open);
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        cursor.close();
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(pool.used(), baseline);
        assert_eq!(view.buffer_stats().misses, 0);
        let mut cursor = view.stream_graph_edges_governed(&cx, &pattern, query_policy()).unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        assert_eq!(first.values().to_vec(), expected(3,2,true,0,0,1)[0]);
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
            &commit, vfs, &path, keys(), pool.clone(), limits()).await.unwrap();
        let baseline = pool.used();
        let pattern = query(&statement(2,true,0,0,100));
        let mut cursor = view.stream_graph_edges_governed(&cx, &pattern,
            GqlQueryPolicy::new(0,100,1_000_000,1_000_000)).unwrap();
        assert!(matches!(cursor.next().await, Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(cursor.row_stats().snapshot_records, 0);
        assert_eq!(cursor.state(), EdgeScanState::Failed);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        assert_eq!(view.buffer_stats().misses, 0);
        assert_eq!(pool.used(), baseline);
        let mut cursor = view.stream_graph_edges_governed(&cx, &pattern,
            GqlQueryPolicy::new(100,1,1_000_000,1_000_000)).unwrap();
        let first = cursor.next().await.unwrap().unwrap();
        assert!(matches!(cursor.next().await, Some(Err(GqlQueryError::Rows(_)))));
        assert_eq!(cursor.row_stats().result_rows, 1);
        assert!(cursor.next().await.is_none());
        drop(cursor);
        drop(first);
        assert_eq!(pool.used(), baseline);
        // The source must reserve its decode workspace; it cannot use a new
        // private allowance or fall back to an eager resident snapshot.
        let hold = pool.reserve(&cx, pool.limit() - pool.used() - 32 * 1024).unwrap();
        let held = pool.used();
        let mut cursor = view.stream_graph_edges_governed(&cx, &pattern, query_policy()).unwrap();
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
            &commit, vfs.clone(), &path, keys(), old_pool.clone(), limits()).await.unwrap();
        let pattern = query(&statement(0,true,0,0,100));
        let mut cursor = view.stream_graph_edges_governed(&cx, &pattern, query_policy()).unwrap();
        let mut actual = vec![cursor.next().await.unwrap().unwrap().values().to_vec()];
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), &path, keys()).await.unwrap();
        let mut change = WriteBatch::new(RELATION);
        change.set_vertex_property(VId(1), PROPERTY, Some(CanonicalScalar::Int(99)));
        change.set_edge_property(EId(101), PROPERTY, Some(CanonicalScalar::Int(77)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        drop(writer);
        while let Some(row) = cursor.next().await { actual.push(row.unwrap().values().to_vec()); }
        assert_eq!(actual, expected(3,0,true,0,0,100));
        assert_eq!(cursor.snapshot_seq(), CommitSeq(3));
        drop(cursor);
        drop(view);
        assert_eq!(old_pool.used(), 0);
        let new_pool = pool();
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit, vfs, &path, keys(), new_pool.clone(), limits()).await.unwrap();
        assert_eq!(view.frontier(), CommitSeq(4));
        let one = query(&statement(0,true,0,0,1));
        let mut cursor = view.stream_graph_edges_governed(&cx, &one, query_policy()).unwrap();
        let row = cursor.next().await.unwrap().unwrap();
        assert_eq!(row.values(), &[GraphValue::Edge(EId(101)), GraphValue::Vertex(VId(1)),
            GraphValue::Vertex(VId(2)), GraphValue::Scalar(CanonicalScalar::Int(77)),
            GraphValue::Scalar(CanonicalScalar::Int(99)), GraphValue::Scalar(CanonicalScalar::Int(2))]);
        drop(cursor);
        drop(view);
        assert!(new_pool.used() > 0);
        drop(row);
        assert_eq!(new_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
