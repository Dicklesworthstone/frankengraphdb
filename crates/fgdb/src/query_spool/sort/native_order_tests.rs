use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits, SpillStats};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn plan(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), resolve).unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(1000, 1000, 100_000_000, 10_000_000)
}
async fn seed(cx: &CommitCx, count: u128) -> Database<MemVfs> {
    let keys = DatabaseKeys::new([0x72; 32], DatabaseSecurityNamespaceId([0x73; 32]), [0x74; 32]);
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    let text = CanonicalScalar::ucs_basic_text(&"padding-é".repeat(150)).unwrap();
    for id in 0..count {
        let mut props = Vec::new();
        if id % 7 != 0 {
            props.push((PropertyKeyId(1), CanonicalScalar::Int(((id * 11) % 17) as i64 - 8)));
        }
        props.push((PropertyKeyId(2), text.clone()));
        batch.create_vertex(VId(id), vec![LabelId(1)], props);
    }
    db.write(cx, batch).await.unwrap();
    db
}
fn expected(view: &EmbeddedReadView, cx: &QueryCx, text: &str) -> Vec<Vec<u8>> {
    let PreparedNativeRead::Pattern(prepared) = plan(text) else { panic!("pattern profile") };
    let bound = prepared.bind_parameters(&GqlParameters::new()).unwrap();
    view.execute_graph_pattern_governed_at(cx, &bound, view.frontier(), policy()).unwrap()
        .value.into_iter().map(|row| row.canonical_bytes().unwrap()).collect()
}

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    pending_write: bool,
    fail_flush: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);
impl AsyncRead for File {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        let at = state.bytes.position() as usize;
        let count = out.remaining().min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + count]);
            state.bytes.set_position((at + count) as u64);
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        if state.pending_write { return Poll::Pending; }
        Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(if self.0.lock().unwrap().fail_flush {
            Err(io::Error::other("injected flush failure"))
        } else { Ok(()) })
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl AsyncSeek for File {
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, at: SeekFrom) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().bytes.seek(at))
    }
}
async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let scratch = SpillFile::new(cx, backing.clone(), pool.clone(), SpillLimits {
        max_file_bytes: 8_000_000, max_runs: 512, max_run_bytes: 1_000_000,
    }).await.unwrap();
    (scratch, backing)
}
async fn contents(spool: &NativeResultSpool, file: &mut SpillFile<File>, cx: &QueryCx) -> Vec<Vec<u8>> {
    let mut cursor = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(row) = cursor.next_row(cx).await.unwrap() {
        rows.push(row.as_ref().to_vec());
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    rows
}

#[test]
fn native_property_order_and_window_match_eager_execution_beyond_the_pool() {
    let ((), report) = run_async_under_lab(0x50ed_0001, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 36).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for text in [
            "MATCH (n:L) RETURN n.p AS p, n.q AS payload, n AS id ORDER BY p DESC NULLS FIRST, id ASC SKIP 3 LIMIT 5",
            "MATCH (n:L) RETURN n.p AS p, n.q AS payload, n AS id ORDER BY p ASC NULLS LAST, id DESC SKIP 2 LIMIT 7",
            "MATCH (n:L) RETURN n.p AS p, n.q AS payload, n AS id",
        ] {
            let prepared = plan(text);
            let params = GqlParameters::new();
            assert!(prepared.stream(&db, &cx, &params, policy()).is_err());
            let expected = expected(&view, &cx, text);
            let mut allowance = policy();
            allowance.rows = GqlExecutionBudget::new(36, expected.len() as u64);
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, backing) = file(&cx, &pool).await;
            let (spool, work) = prepared.spool_ordered(&db, &cx, &params, allowance,
                &mut scratch, &mut destination, 3, 12, 127, 4096, 36, 100_000_000)
                .await.unwrap();
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(spool.row_stats().snapshot_records, 36);
            assert_eq!(spool.snapshot_seq(), view.frontier());
            assert_eq!(spool.columns(), &["p", "payload", "id"]);
            assert!(work > 0);
            assert!(backing.0.lock().unwrap().bytes.get_ref().len() > pool.limit());
            assert_eq!(pool.used(), 0);
            assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            assert_eq!(pool.used(), 0);
            // Compilation/execution never mutates the original prepared query.
            assert!(prepared.stream(&db, &cx, &params, policy()).is_err());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn paging_never_selects_the_first_source_rows_before_ordering() {
    let ((), report) = run_async_under_lab(0x50ed_0002, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 10).await;
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for (suffix, ids) in [
            ("SKIP 1 LIMIT 2", vec![8, 7]),
            ("SKIP 99 LIMIT 2", vec![]),
            ("LIMIT 0", vec![]),
            ("SKIP 8", vec![1, 0]),
        ] {
            let text = format!("MATCH (n:L) RETURN n AS id ORDER BY id DESC {suffix}");
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let mut allowance = policy();
            allowance.rows = GqlExecutionBudget::new(10, ids.len() as u64);
            let (spool, _) = plan(&text).spool_ordered(&db, &cx, &GqlParameters::new(), allowance,
                &mut scratch, &mut destination, 2, 5, 31, 4096, 10, 1_000_000).await.unwrap();
            let expected: Vec<_> = ids.into_iter().map(|id| {
                GraphValueRow::from_owned_values(vec![fgdb_gql::algebra::GraphValue::Vertex(VId(id))])
                    .canonical_bytes().unwrap()
            }).collect();
            assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            assert_eq!(spool.row_stats().snapshot_records, 10);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_input_and_final_output_have_separate_exact_row_allowances() {
    let ((), report) = run_async_under_lab(0x50ed_0003, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 10).await;
        let prepared = plan("MATCH (n:L) RETURN n AS id ORDER BY id DESC LIMIT 1");
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for (records, input, output, dimension, limit) in [
            (9, 10, 1, GqlBudgetDimension::SnapshotRecords, 9),
            (10, 9, 1, GqlBudgetDimension::ResultRows, 9),
            (10, 10, 0, GqlBudgetDimension::ResultRows, 0),
        ] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let mut allowance = policy();
            allowance.rows = GqlExecutionBudget::new(records, output);
            let error = prepared.spool_ordered(&db, &cx, &GqlParameters::new(), allowance,
                &mut scratch, &mut destination, 2, 5, 31, 4096, input, 1_000_000)
                .await.unwrap_err();
            assert!(matches!(error.execution_error(), Some(GqlQueryError::Rows(e))
                if e.dimension == dimension && e.limit == limit && e.observed == limit + 1));
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sort_and_final_window_share_one_work_allowance() {
    let ((), report) = run_async_under_lab(0x50ed_0004, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 12).await;
        let prepared = plan("MATCH (n:L) RETURN n.p AS p, n AS id ORDER BY p DESC SKIP 1 LIMIT 3");
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let (first, work) = prepared.spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
            &mut scratch, &mut destination, 2, 6, 31, 4096, 12, 1_000_000)
            .await.unwrap();
        let expected = contents(&first, &mut destination, &cx).await;
        assert!(work > 1);
        for limit in [work - 1, work] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let result = prepared.spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
                &mut scratch, &mut destination, 2, 6, 31, 4096, 12, limit).await;
            if limit == work {
                let (spool, spent) = result.unwrap();
                assert_eq!(spent, work);
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            } else {
                assert!(matches!(result, Err(NativeSpoolError::SortWorkLimit { limit: actual, .. }) if actual == limit));
            }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn opening_pins_the_old_values_and_releases_the_generation_after_source_drain() {
    let ((), report) = run_async_under_lab(0x50ed_0005, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = seed(&c.commit(), 12).await;
        let view = db.read_session().unwrap();
        let weak = Arc::downgrade(&db.snapshot);
        let at = view.frontier();
        let text = "MATCH (n:L) RETURN n.p AS p, n AS id ORDER BY p DESC LIMIT 4";
        let expected = expected(&view, &cx, text);
        let prepared = plan(text);
        let params = GqlParameters::new();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let future = prepared.spool_ordered_in_view(&view, &cx, &params, policy(),
            &mut scratch, &mut destination, 2, 6, 31, 4096, 12, 1_000_000);
        let mut drift = WriteBatch::new(RelationId(1));
        drift.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(999)));
        drift.delete_vertex(VId(2));
        db.write(&c.commit(), drift).await.unwrap();
        drop(view);
        drop(db);
        drop(params);
        drop(prepared);
        assert!(weak.upgrade().is_some());
        let (spool, _) = future.await.unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(spool.snapshot_seq(), at);
        assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn invalid_shapes_settings_and_unpolled_futures_never_start_scratch() {
    let ((), report) = run_async_under_lab(0x50ed_0006, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 3).await;
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for text in [
            "MATCH (n:L) RETURN count(*) AS count LIMIT 0",
            "MATCH (a)-[e:R]->(b) RETURN e AS edge, a AS source LIMIT 0",
        ] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let result = plan(text).spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
                &mut scratch, &mut destination, 1, 10, 31, 4096, 10, 1_000_000).await;
            assert!(matches!(result, Err(NativeSpoolError::Prepare(_))));
            assert_eq!(scratch.stats(), SpillStats::default());
            assert_eq!(destination.stats(), SpillStats::default());
        }
        let prepared = plan("MATCH (n:L) RETURN n.p AS p ORDER BY p DESC");
        for (run_rows, max_runs, page_bytes, work) in [(0, 10, 31, 100), (1, 0, 31, 100),
            (1, 10, 0, 100), (1, 10, 65_537, 100), (1, 10, 31, 0)] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            assert!(prepared.spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
                &mut scratch, &mut destination, run_rows, max_runs, page_bytes, 4096, 10, work).await.is_err());
            assert_eq!(scratch.stats(), SpillStats::default());
            assert_eq!(destination.stats(), SpillStats::default());
        }
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        drop(prepared.spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
            &mut scratch, &mut destination, 1, 10, 31, 4096, 10, 1_000_000));
        assert_eq!(scratch.stats(), SpillStats::default());
        assert_eq!(destination.stats(), SpillStats::default());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn failed_and_dropped_transfers_do_not_publish_a_native_page() {
    let ((), report) = run_async_under_lab(0x50ed_0007, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 3).await;
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let prepared = plan("MATCH (n:L) RETURN n.p AS p ORDER BY p DESC LIMIT 1");
        for during_sort in [false, true] {
            let (mut scratch, scratch_file) = file(&cx, &pool).await;
            let (mut destination, destination_file) = file(&cx, &pool).await;
            if during_sort { scratch_file.0.lock().unwrap().fail_flush = true; }
            else { destination_file.0.lock().unwrap().fail_flush = true; }
            assert!(prepared.spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
                &mut scratch, &mut destination, 1, 10, 31, 4096, 10, 1_000_000).await.is_err());
            assert_eq!(pool.used(), 0);
        }
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, backing) = file(&cx, &pool).await;
        backing.0.lock().unwrap().pending_write = true;
        {
            let future = prepared.spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
                &mut scratch, &mut destination, 1, 10, 31, 4096, 10, 1_000_000);
            let mut future = std::pin::pin!(future);
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        }
        assert!(destination.is_poisoned());
        assert_eq!(destination.stats().published_runs, 0);
        assert_eq!(scratch.stats(), SpillStats::default());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_and_all_keep_native_row_classes_across_many_spill_runs() {
    let ((), report) = run_async_under_lab(0x50ed_0010, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 60).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for text in [
            "MATCH (n:L) RETURN DISTINCT n.p AS p, n.q AS payload",
            "MATCH (n:L) RETURN DISTINCT n.p AS p, n.q AS payload ORDER BY p DESC NULLS FIRST SKIP 1 LIMIT 4",
            "MATCH (n:L) RETURN DISTINCT n.p AS p, n.q AS payload ORDER BY p ASC NULLS LAST SKIP 2 LIMIT 5",
            "MATCH (n:L) RETURN n.p AS p, n.q AS payload ORDER BY p DESC NULLS FIRST SKIP 1 LIMIT 4",
        ] {
            let expected = expected(&view, &cx, text);
            for run_rows in [1, 3, 7] {
                let (mut scratch, _) = file(&cx, &pool).await;
                let (mut destination, backing) = file(&cx, &pool).await;
                let mut allowance = policy();
                allowance.rows = GqlExecutionBudget::new(60, expected.len() as u64);
                let (spool, _) = plan(text).spool_ordered(&db, &cx, &GqlParameters::new(), allowance,
                    &mut scratch, &mut destination, run_rows, 60, 127, 4096, 60, 100_000_000)
                    .await.unwrap();
                assert_eq!(spool.row_count(), expected.len() as u64);
                assert_eq!(spool.row_stats().snapshot_records, 60);
                assert!(backing.0.lock().unwrap().bytes.get_ref().len() > pool.limit());
                assert_eq!(pool.used(), 0);
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_classes_not_occurrences_are_paginated_and_counted() {
    let ((), report) = run_async_under_lab(0x50ed_0011, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = seed(&c.commit(), 10).await;
        let mut changes = WriteBatch::new(RelationId(1));
        for (id, value) in [Some(2), Some(1), Some(2), Some(0), Some(1), Some(3),
            Some(0), Some(3), None, None].into_iter().enumerate()
        {
            changes.set_vertex_property(VId(id as u128), PropertyKeyId(1), value.map(CanonicalScalar::Int));
        }
        db.write(&c.commit(), changes).await.unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        for (distinct, suffix, wanted) in [
            (true, "SKIP 1 LIMIT 2", vec![Some(3), Some(2)]),
            (false, "SKIP 1 LIMIT 2", vec![None, Some(3)]),
            (true, "SKIP 3", vec![Some(1), Some(0)]),
            (true, "SKIP 5", vec![]),
            (true, "LIMIT 0", vec![]),
            (true, "", vec![None, Some(3), Some(2), Some(1), Some(0)]),
        ] {
            let text = format!("MATCH (n:L) RETURN {}n.p AS p ORDER BY p DESC NULLS FIRST {suffix}",
                if distinct { "DISTINCT " } else { "" });
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let mut allowance = policy();
            allowance.rows = GqlExecutionBudget::new(10, wanted.len() as u64);
            let (spool, _) = plan(&text).spool_ordered(&db, &cx, &GqlParameters::new(), allowance,
                &mut scratch, &mut destination, 1, 10, 31, 4096, 10, 1_000_000).await.unwrap();
            let expected: Vec<_> = wanted.into_iter().map(|value| {
                GraphValueRow::from_owned_values(vec![fgdb_gql::algebra::GraphValue::Scalar(
                    value.map_or(CanonicalScalar::Null, CanonicalScalar::Int),
                )]).canonical_bytes().unwrap()
            }).collect();
            assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(spool.row_stats().snapshot_records, 10);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_uses_the_whole_typed_row_not_just_order_keys_or_scalar_spellings() {
    let ((), report) = run_async_under_lab(0x50ed_0012, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let mut db = seed(&c.commit(), 9).await;
        let values = [
            (None, 0),
            (Some(CanonicalScalar::Null), 0),
            (Some(CanonicalScalar::Int(1)), 0),
            (Some(CanonicalScalar::Int(1)), 1),
            (Some(CanonicalScalar::Int(1)), 0),
            (Some(CanonicalScalar::Bool(true)), 0),
            (Some(CanonicalScalar::ucs_basic_text("1").unwrap()), 0),
            (Some(CanonicalScalar::Int(1)), 1),
            (Some(CanonicalScalar::Bool(true)), 0),
        ];
        let mut changes = WriteBatch::new(RelationId(1));
        for (id, (p, q)) in values.into_iter().enumerate() {
            changes.set_vertex_property(VId(id as u128), PropertyKeyId(1), p);
            changes.set_vertex_property(VId(id as u128), PropertyKeyId(2), Some(CanonicalScalar::Int(q)));
        }
        db.write(&c.commit(), changes).await.unwrap();
        let text = "MATCH (n:L) RETURN DISTINCT n.p AS p, n.q AS q ORDER BY p ASC NULLS FIRST";
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let (spool, _) = plan(text).spool_ordered(&db, &cx, &GqlParameters::new(), policy(),
            &mut scratch, &mut destination, 1, 9, 31, 4096, 9, 1_000_000).await.unwrap();
        use fgdb_gql::algebra::GraphValue;
        let expected: Vec<_> = [
            (CanonicalScalar::Null, 0),
            (CanonicalScalar::Bool(true), 0),
            (CanonicalScalar::Int(1), 0),
            (CanonicalScalar::Int(1), 1),
            (CanonicalScalar::ucs_basic_text("1").unwrap(), 0),
        ].into_iter().map(|(p, q)| GraphValueRow::from_owned_values(vec![
            GraphValue::Scalar(p), GraphValue::Scalar(CanonicalScalar::Int(q)),
        ]).canonical_bytes().unwrap()).collect();
        assert_eq!(spool.row_count(), 5);
        assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn distinct_never_refunds_input_admission_or_resets_final_comparison_work() {
    let ((), report) = run_async_under_lab(0x50ed_0013, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit(), 10).await;
        let prepared = plan("MATCH (n:L) RETURN DISTINCT n.q AS value LIMIT 1");
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let mut allowance = policy();
        allowance.rows = GqlExecutionBudget::new(10, 1);
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, _) = file(&cx, &pool).await;
        let (spool, work) = prepared.spool_ordered(&db, &cx, &GqlParameters::new(), allowance,
            &mut scratch, &mut destination, 2, 5, 127, 4096, 10, 1_000_000).await.unwrap();
        assert_eq!(spool.row_count(), 1);
        assert!(work > 1);
        for (input, output, limit) in [(9, 1, 1_000_000), (10, 0, 1_000_000), (10, 1, work - 1), (10, 1, work)] {
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            allowance.rows = GqlExecutionBudget::new(10, output);
            let result = prepared.spool_ordered(&db, &cx, &GqlParameters::new(), allowance,
                &mut scratch, &mut destination, 2, 5, 127, 4096, input, limit).await;
            if input == 9 || output == 0 {
                let error = result.unwrap_err();
                assert!(matches!(error.execution_error(), Some(GqlQueryError::Rows(e))
                    if e.dimension == GqlBudgetDimension::ResultRows
                    && e.limit == if input == 9 { 9 } else { 0 }));
            } else if limit < work {
                assert!(matches!(result, Err(NativeSpoolError::SortWorkLimit { limit: actual, .. }) if actual == limit));
            } else {
                let (spool, used) = result.unwrap();
                assert_eq!(spool.row_count(), 1);
                assert_eq!(used, work);
            }
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_frame_equality_checks_every_byte_and_propagates_every_control_cut() {
    let left: Vec<_> = (0..12_295).map(|index| (index % 251) as u8).collect();
    let mut units = Vec::new();
    assert!(equal_frame(&left, &left, &mut |unit| { units.push(unit); Ok(()) }).unwrap());
    assert_eq!(units, vec![1, 4096, 4096, 4096, 7]);
    for at in [0, 4095, 4096, 8191, left.len() - 1] {
        let mut right = left.clone();
        right[at] ^= 1;
        assert!(!equal_frame(&left, &right, &mut |_| Ok(())).unwrap());
    }
    assert!(!equal_frame(&left, &left[..left.len() - 1], &mut |_| Ok(())).unwrap());
    for cut in 1..=units.len() {
        let mut seen = 0;
        let result = equal_frame(&left, &left, &mut |_| {
            seen += 1;
            if seen == cut {
                Err(NativeSpoolError::SortWorkLimit { attempted: seen as u64, limit: (cut - 1) as u64 })
            } else { Ok(()) }
        });
        assert!(matches!(result, Err(NativeSpoolError::SortWorkLimit { .. })));
        assert_eq!(seen, cut);
        let mut seen = 0;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            equal_frame(&left, &left, &mut |_| {
                seen += 1;
                assert_ne!(seen, cut, "injected equality unwind");
                Ok(())
            })
        }));
        assert!(panic.is_err());
        assert_eq!(seen, cut);
    }
    assert!(equal_frame(&left, &left, &mut |_| Ok(())).unwrap());
}
