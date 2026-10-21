//! Real cold history, native aggregate execution, authenticated spill and callbacks.

use super::*;
use crate::{BufferLimits, BufferedReadLimits, DatabaseKeys, MemVfs, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId};
use std::collections::BTreeMap;
use std::future::ready;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

const QUERY: &str = "MATCH (n:L) RETURN COUNT(*) AS n, SUM(n.p) AS total, AVG(n.p) AS mean";
const WORK: u64 = 100_000_000;
type Bag = BTreeMap<Vec<GraphAggregateValue>, i128>;

fn keys() -> DatabaseKeys {
    DatabaseKeys::new([0xa3; 32], DatabaseSecurityNamespaceId([0xa4; 32]), [0xa5; 32])
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, WORK, WORK)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn plan(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols).unwrap()
}
async fn history(cx: &CommitCx) -> (Database<MemVfs>, MemVfs, [CommitSeq; 3]) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, p, q) in [(1, i64::MAX, 0), (2, i64::MAX, 0), (3, -1, 1)] {
        batch.create_vertex(VId(id), vec![LabelId(1)], vec![
            (PropertyKeyId(1), CanonicalScalar::Int(p)),
            (PropertyKeyId(2), CanonicalScalar::Int(q)),
        ]);
    }
    batch.add_edge(EId(1), VId(1), VId(2), vec![]);
    batch.add_edge(EId(2), VId(1), VId(3), vec![]);
    let first = db.write(cx, batch).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
    batch.create_vertex(VId(4), vec![LabelId(1)], vec![
        (PropertyKeyId(1), CanonicalScalar::Int(-2)),
        (PropertyKeyId(2), CanonicalScalar::Int(2)),
    ]);
    batch.add_edge(EId(3), VId(1), VId(4), vec![]);
    let second = db.write(cx, batch).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    batch.delete_vertex(VId(2));
    let third = db.write(cx, batch).await.unwrap();
    (db, vfs, [first, second, third])
}
async fn cold(cx: &CommitCx, vfs: &MemVfs) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        cx, vfs.clone(), vfs.database_dir(), keys(),
        MemoryPool::new(8_000_000, 0).unwrap(),
        BufferedReadLimits {
            max_root_bytes: 8192,
            max_source_bytes: 16_000_000,
            max_blocks: 128,
            max_vertex_patches: 128,
            max_work: WORK as usize,
            buffer: BufferLimits {
                max_frames: 2,
                max_ghost_entries: 4,
                max_extent_bytes: 4096,
            },
        },
    ).await.unwrap()
}

#[derive(Default)]
struct FileState { bytes: Cursor<Vec<u8>>, reads: usize, writes: usize }
#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);
impl AsyncRead for File {
    fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, out: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        let at = state.bytes.position() as usize;
        let n = out.remaining().min(state.bytes.get_ref().len().saturating_sub(at));
        if n != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + n]);
            state.bytes.set_position((at + n) as u64);
        }
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let mut state = self.0.lock().unwrap();
        state.writes += 1;
        Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> { Poll::Ready(Ok(())) }
}
impl AsyncSeek for File {
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, at: SeekFrom) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().bytes.seek(at))
    }
}
async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let owner = SpillFile::new(cx, backing.clone(), pool.clone(), SpillLimits {
        max_file_bytes: 64_000_000, max_runs: 100_000, max_run_bytes: 16_000_000,
    }).await.unwrap();
    (owner, backing)
}

#[allow(clippy::too_many_arguments)]
async fn attempt(
    setup: &QueryCx, prepared: &PreparedNativeRead, view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx, before: CommitSeq, after: CommitSeq, policy: GqlQueryPolicy, work: u64,
) -> DifferenceResult<(Bag, u64)> {
    let pool = MemoryPool::new(262_144, 0).unwrap();
    let (mut a, _) = file(setup, &pool).await;
    let (mut b, _) = file(setup, &pool).await;
    let (mut c, _) = file(setup, &pool).await;
    let mut bag = Bag::new();
    let result = prepared.visit_buffered_aggregate_difference_in_view(
        view, cx, &GqlParameters::new(), before, after, policy,
        &mut a, &mut b, &mut c, 1, 128, 2, 10_000, 257, 16_384, 100_000,
        work, None, |weight, row| {
            assert!(row.keys().is_empty(), "private group keys must not escape");
            assert_ne!(weight, 0);
            assert!(bag.insert(row.values().to_vec(), weight).is_none());
            ready(Ok(()))
        },
    ).await;
    assert_eq!(pool.used(), 0, "temporary and delivered row grants must retire");
    result.map(|(columns, rows, work)| {
        assert!(!columns.is_empty());
        assert!(bag.keys().all(|row| row.len() == columns.len()));
        assert_eq!(rows, bag.len() as u64);
        (bag, work)
    })
}

#[test]
fn complete_cold_aggregate_endpoints_match_native_history_in_both_directions() {
    let ((), report) = run_async_under_lab(0xa663_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (db, vfs, cuts) = history(&commit).await;
        let resident = db.read_session().unwrap();
        drop(db);
        for text in [
            QUERY,
            "MATCH (n:L) RETURN COUNT(*) AS n GROUP BY n.q",
            "MATCH (n:L) RETURN n.q AS bucket, SUM(n.p) AS total ORDER BY total DESC LIMIT 2",
            "MATCH (n:L) RETURN SUM(n.p) AS total, AVG(n.p) AS mean HAVING SUM(n.p)>0",
            "MATCH (n:L) RETURN COUNT(DISTINCT n.p) AS n, SUM(DISTINCT n.p) AS total, AVG(DISTINCT n.p) AS mean",
            "MATCH (a)-[r:R]->(b) RETURN SUM(b.p) AS total, COUNT(*) AS n",
            "MATCH (n:L) WITH n.p AS x ORDER BY x DESC LIMIT 2 RETURN COUNT(*) AS n, SUM(x) AS total",
            "UNWIND [1,3,5] AS x RETURN COUNT(*) AS n, SUM(x) AS total, AVG(x) AS mean",
        ] {
            let prepared = plan(text);
            let mut view = cold(&commit, &vfs).await;
            for before in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                for after in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                    let expected = prepared.diff_in_view(
                        &resident, &cx, &GqlParameters::new(), before, after, policy(100),
                    ).unwrap();
                    let expected: Bag = expected.changes().iter()
                        .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap())).collect();
                    let (actual, _) = attempt(
                        &cx, &prepared, &mut view, &cx, before, after, policy(100), u64::MAX,
                    ).await.unwrap_or_else(|error| panic!("{text}: {error}"));
                    assert_eq!(actual, expected, "{text} at {before:?} -> {after:?}");
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_wide_sum_average_and_hidden_group_multiplicities_are_exact() {
    let ((), report) = run_async_under_lab(0xa663_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs, cuts) = history(&contexts.commit()).await;
        drop(db);
        let mut view = cold(&contexts.commit(), &vfs).await;
        let first = 2 * i128::from(i64::MAX) - 1;
        let second = 2 * i128::from(i64::MAX) + 3;
        let row = |count, total| vec![
            GraphAggregateValue::Count(count),
            GraphAggregateValue::Integer(total),
            GraphAggregateValue::Average(GraphExactAverage::new(total, count).unwrap()),
        ];
        let expected = Bag::from([(row(3, first), -1), (row(4, second), 1)]);
        assert_eq!(attempt(&cx, &plan(QUERY), &mut view, &cx, cuts[0], cuts[1], policy(2), u64::MAX)
            .await.unwrap().0, expected);
        let hidden = plan("MATCH (n:L) RETURN COUNT(*) AS n GROUP BY n.q");
        assert_eq!(attempt(&cx, &hidden, &mut view, &cx, cuts[0], cuts[1], policy(1), u64::MAX)
            .await.unwrap().0, Bag::from([(vec![GraphAggregateValue::Count(1)], 1)]));
        assert!(attempt(&cx, &hidden, &mut view, &cx, cuts[1], cuts[1], policy(0), u64::MAX)
            .await.unwrap().0.is_empty());
        assert!(matches!(
            attempt(&cx, &plan(QUERY), &mut view, &cx, cuts[0], cuts[1], policy(1), u64::MAX).await,
            Err(GqlQueryError::Rows(_))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_bound_definition_and_cold_generation_survive_writer_and_argument_destruction() {
    let ((), report) = run_async_under_lab(0xa663_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, vfs, cuts) = history(&contexts.commit()).await;
        let mut view = cold(&contexts.commit(), &vfs).await;
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut a, _) = file(&cx, &pool).await;
        let (mut b, _) = file(&cx, &pool).await;
        let (mut c, _) = file(&cx, &pool).await;
        let params = GqlParameters::new().with_int64("floor", -10).unwrap();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) WHERE n.p > $floor RETURN COUNT(*) AS n", &params, symbols,
        ).unwrap();
        let rebound = GqlParameters::new().with_int64("floor", 0).unwrap();
        let mut bag = Bag::new();
        let future = prepared.visit_buffered_aggregate_difference_in_view(
            &mut view, &cx, &rebound, cuts[0], cuts[1], policy(2),
            &mut a, &mut b, &mut c, 1, 128, 2, 10_000, 257, 16_384, 100_000,
            u64::MAX, None, |weight, row| {
                bag.insert(row.values().to_vec(), weight);
                ready(Ok(()))
            },
        );
        drop(prepared);
        drop(params);
        drop(rebound);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(-99)));
        db.write(&contexts.commit(), edit).await.unwrap();
        let resident = Arc::downgrade(&db.snapshot);
        drop(db);
        assert!(resident.upgrade().is_none());
        let (columns, changed, _) = future.await.unwrap();
        assert_eq!(columns, ["n"]);
        assert_eq!(changed, 2);
        assert_eq!(bag, Bag::from([
            (vec![GraphAggregateValue::Count(2)], -1),
            (vec![GraphAggregateValue::Count(3)], 1),
        ]));
        assert_eq!(view.frontier(), cuts[2]);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_complete_reducers_and_all_physical_phases_share_exact_allowances() {
    let ((), report) = run_async_under_lab(0xa663_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (db, vfs, cuts) = history(&commit).await;
        drop(db);
        let prepared = plan(QUERY);
        let mut view = cold(&commit, &vfs).await;
        let mut bound = prepare(&prepared, &view, &cx, &GqlParameters::new(), cuts[0], cuts[1]).unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut a, _) = file(&cx, &pool).await;
        let (mut b, _) = file(&cx, &pool).await;
        let (mut c, _) = file(&cx, &pool).await;
        let (mut records, mut work, mut entries) = (0, 0, 0);
        for at in [cuts[0], cuts[1]] {
            bound.as_of = Some(at);
            let (spool, _) = bound.spool_in_view(
                &mut view, &cx, private_policy(policy(0)), &mut a, &mut b, &mut c,
                1, 128, 2, 10_000, 257, 16_384, 100_000, u64::MAX, None,
            ).await.unwrap();
            records += spool.row_stats().snapshot_records;
            work += spool.evaluator_stats().work_units;
            entries += spool.evaluator_stats().scratch_entries;
        }
        assert!(records > 0 && work > 0 && entries > 0);
        let exact = GqlQueryPolicy::new(records, 2, work, entries);
        let mut view = cold(&commit, &vfs).await;
        let (expected, physical) = attempt(
            &cx, &prepared, &mut view, &cx, cuts[0], cuts[1], exact, u64::MAX,
        ).await.unwrap();
        assert!(physical > 1);
        let mut view = cold(&commit, &vfs).await;
        let (actual, used) = attempt(
            &cx, &prepared, &mut view, &cx, cuts[0], cuts[1], exact, physical,
        ).await.unwrap();
        assert_eq!((actual, used), (expected, physical));
        for bounded in [
            GqlQueryPolicy::new(records - 1, 2, work, entries),
            GqlQueryPolicy::new(records, 2, work - 1, entries),
            GqlQueryPolicy::new(records, 2, work, entries - 1),
        ] {
            let mut view = cold(&commit, &vfs).await;
            let error = attempt(&cx, &prepared, &mut view, &cx, cuts[0], cuts[1], bounded, physical)
                .await.unwrap_err();
            match error {
                GqlQueryError::Source(NativeAggregateSpoolError::Execute(error)) => {
                    assert!(matches!(*error, GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)));
                }
                GqlQueryError::Source(NativeAggregateSpoolError::BufferedExecute(error)) => {
                    assert!(matches!(*error, GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)));
                }
                other => panic!("expected native cumulative-budget refusal: {other}"),
            }
        }
        let mut view = cold(&commit, &vfs).await;
        assert!(matches!(attempt(
            &cx, &prepared, &mut view, &cx, cuts[0], cuts[1], exact, physical - 1,
        ).await, Err(GqlQueryError::Source(NativeAggregateSpoolError::Spool(
            NativeSpoolError::SortWorkLimit { .. }
        )))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn preflight_refusals_and_unpolled_opening_leave_sources_and_files_untouched() {
    let ((), report) = run_async_under_lab(0xa663_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs, cuts) = history(&contexts.commit()).await;
        drop(db);
        let mut view = cold(&contexts.commit(), &vfs).await;
        let baseline = format!("{:?}", view.buffer_stats());
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut a, ab) = file(&cx, &pool).await;
        let (mut b, bb) = file(&cx, &pool).await;
        let (mut c, cb) = file(&cx, &pool).await;
        for text in ["MATCH (n) RETURN n.p AS p", "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS n"] {
            let error = plan(text).visit_buffered_aggregate_difference_in_view(
                &mut view, &cx, &GqlParameters::new(), cuts[0], cuts[1], policy(0),
                &mut a, &mut b, &mut c, 1, 128, 2, 100, 257, 16_384, 1000,
                u64::MAX, None, |_, _| -> std::future::Ready<Result<()>> {
                    panic!("no callback during preparation refusal")
                },
            ).await.unwrap_err();
            assert!(matches!(error, GqlQueryError::Source(NativeAggregateSpoolError::Unsupported)));
        }
        let prepared = plan(QUERY);
        for (before, after) in [(CommitSeq(u64::MAX), cuts[0]), (cuts[0], CommitSeq(u64::MAX))] {
            let error = prepared.visit_buffered_aggregate_difference_in_view(
                &mut view, &cx, &GqlParameters::new(), before, after, policy(0),
                &mut a, &mut b, &mut c, 1, 128, 2, 100, 257, 16_384, 1000,
                u64::MAX, None, |_, _| -> std::future::Ready<Result<()>> {
                    panic!("no callback for future cut")
                },
            ).await.unwrap_err();
            assert!(matches!(error, GqlQueryError::Source(NativeAggregateSpoolError::Spool(
                NativeSpoolError::BufferedRead(_)
            ))));
        }
        for (groups, runs, page, work) in [(0, 100, 257, 100), (1, 0, 257, 100), (1, 100, 0, 100), (1, 100, 257, 0)] {
            assert!(prepared.visit_buffered_aggregate_difference_in_view(
                &mut view, &cx, &GqlParameters::new(), cuts[0], cuts[1], policy(0),
                &mut a, &mut b, &mut c, groups, 128, 2, runs, page, 16_384, 1000,
                work, None, |_, _| ready(Ok(())),
            ).await.is_err());
        }
        let future = prepared.visit_buffered_aggregate_difference_in_view(
            &mut view, &cx, &GqlParameters::new(), cuts[0], cuts[1], policy(2),
            &mut a, &mut b, &mut c, 1, 128, 2, 100, 257, 16_384, 1000,
            u64::MAX, None, |_, _| ready(Ok(())),
        );
        drop(future);
        assert_eq!(format!("{:?}", view.buffer_stats()), baseline);
        for file in [ab, bb, cb] {
            let state = file.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert_eq!(a.stats().reserved_runs + b.stats().reserved_runs + c.stats().reserved_runs, 0);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_second_endpoint_errors_even_beneath_limit_zero_deliver_nothing() {
    let ((), report) = run_async_under_lab(0xa663_1006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs, cuts) = history(&contexts.commit()).await;
        drop(db);
        for suffix in ["", " LIMIT 0"] {
            let prepared = plan(&format!("MATCH (n:L) RETURN SUM(100 / (n.p - 5)) AS total{suffix}"));
            let mut view = cold(&contexts.commit(), &vfs).await;
            assert!(attempt(&cx, &prepared, &mut view, &cx, cuts[0], cuts[0], policy(0), u64::MAX)
                .await.unwrap().0.is_empty());
            let pool = MemoryPool::new(262_144, 0).unwrap();
            let (mut a, _) = file(&cx, &pool).await;
            let (mut b, _) = file(&cx, &pool).await;
            let (mut c, _) = file(&cx, &pool).await;
            let mut callbacks = 0;
            assert!(prepared.visit_buffered_aggregate_difference_in_view(
                &mut view, &cx, &GqlParameters::new(), cuts[0], cuts[1], policy(0),
                &mut a, &mut b, &mut c, 1, 128, 2, 100, 257, 16_384, 1000,
                u64::MAX, None, |_, _| { callbacks += 1; ready(Ok(())) },
            ).await.is_err());
            assert_eq!(callbacks, 0);
            assert!(c.stats().published_runs > 0, "first endpoint completed privately");
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn callback_failure_is_preserved_and_retry_does_not_change_history() {
    let ((), report) = run_async_under_lab(0xa663_1007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs, cuts) = history(&contexts.commit()).await;
        drop(db);
        let mut view = cold(&contexts.commit(), &vfs).await;
        let prepared = plan(QUERY);
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut a, _) = file(&cx, &pool).await;
        let (mut b, _) = file(&cx, &pool).await;
        let (mut c, _) = file(&cx, &pool).await;
        let mut calls = 0;
        let error = prepared.visit_buffered_aggregate_difference_in_view(
            &mut view, &cx, &GqlParameters::new(), cuts[0], cuts[1], policy(2),
            &mut a, &mut b, &mut c, 1, 128, 2, 100, 257, 16_384, 1000,
            u64::MAX, None, |_, _| {
                calls += 1;
                // Deliberately use a physical-limit-shaped callback error.
                // The adapter must not add endpoint work or rewrite its limit.
                ready(Err(NativeSpoolError::SortWorkLimit { attempted: 17, limit: 13 }.into()))
            },
        ).await.unwrap_err();
        assert_eq!(calls, 1);
        assert!(matches!(error, GqlQueryError::Source(NativeAggregateSpoolError::Spool(
            NativeSpoolError::SortWorkLimit { attempted: 17, limit: 13 }
        ))));
        assert_eq!(pool.used(), 0);
        assert_eq!(view.frontier(), cuts[2]);
        assert_eq!(attempt(&cx, &prepared, &mut view, &cx, cuts[0], cuts[1], policy(2), u64::MAX)
            .await.unwrap().0.len(), 2);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_reached_source_free_boundary_requires_both_endpoints_and_full_comparison() {
    let ((), report) = run_async_under_lab(0xa663_1008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, vfs, cuts) = history(&contexts.commit()).await;
        drop(db);
        let prepared = plan("UNWIND [1,3] AS x RETURN SUM(x) AS total, AVG(x) AS mean");
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut view = cold(&contexts.commit(), &vfs).await;
        assert!(attempt(&cx, &prepared, &mut view, &observed, cuts[0], cuts[1], policy(0), u64::MAX)
            .await.unwrap().0.is_empty());
        assert!(probe.calls() > 5);
        for stop in 1..=probe.calls() {
            let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
            let mut view = cold(&contexts.commit(), &vfs).await;
            assert!(attempt(&cx, &prepared, &mut view, &interrupted, cuts[0], cuts[1], policy(0), u64::MAX)
                .await.is_err(), "checkpoint {stop}");
            assert_eq!(control.calls(), stop);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
