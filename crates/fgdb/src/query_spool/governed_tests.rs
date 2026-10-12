//! The scoped-source bridge keeps the native cursor's semantics and statistics
//! while admitting its canonical encoding through the existing spill pool.

use super::*;
use crate::{DatabaseKeys, MemVfs, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::scan_stream::ScanSortTail;
use fgdb_gql::stream::{VertexScanCursor, VertexScanPlan};
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryError, MemoryPool, SpillLimits, SpillStats};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, VId};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

const INPUT_ROWS: u64 = 8;
const PAGE: usize = 63;
const WORK: u64 = 100_000_000;

fn policy(results: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(INPUT_ROWS, results, WORK, 1_000_000)
}

fn prepare(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(
        text,
        &GqlParameters::new(),
        |kind: GraphSymbolKind, name: &str| match (kind, name) {
            (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        },
    )
    .unwrap()
}

async fn seed(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(
        cx,
        DatabaseKeys::new(
            [0xb4; 32],
            DatabaseSecurityNamespaceId([0xb5; 32]),
            [0xb6; 32],
        ),
    )
    .await
    .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    let payload = CanonicalScalar::ucs_basic_text(&"payload-é".repeat(256)).unwrap();
    for (id, value) in [
        Some(3),
        Some(1),
        Some(3),
        None,
        Some(2),
        Some(1),
        None,
        Some(2),
    ]
    .into_iter()
    .enumerate()
    {
        let mut properties = vec![(PropertyKeyId(2), payload.clone())];
        if let Some(value) = value {
            properties.push((PropertyKeyId(1), CanonicalScalar::Int(value)));
        }
        batch.create_vertex(VId(id as u128), vec![LabelId(1)], properties);
    }
    db.write(cx, batch).await.unwrap();
    db
}

fn expected(view: &EmbeddedReadView, cx: &QueryCx, text: &str) -> Vec<Vec<u8>> {
    let PreparedNativeRead::Pattern(template) = prepare(text) else {
        panic!("pattern fixture");
    };
    let query = template.bind_parameters(&GqlParameters::new()).unwrap();
    view.execute_graph_pattern_governed_at(cx, &query, view.frontier(), policy(INPUT_ROWS))
        .unwrap()
        .value
        .into_iter()
        .map(|row| row.canonical_bytes().unwrap())
        .collect()
}

#[derive(Clone, Copy, Debug)]
struct Observation {
    state: ScanState,
    rows: GqlExecutionStats,
    evaluator: GlaExecutionStats,
}

// This adapter drives a real native cursor; it never invents source rows,
// completion or statistics. The optional fault isolates error transport at the
// SpoolInput boundary, rather than pretending to exercise Warden admission.
struct Governed<I> {
    inner: I,
    observed: Arc<Mutex<Observation>>,
    refuse_after: Option<u64>,
    failed: bool,
}

impl<I: SpoolInput> SpoolInput for Governed<I> {
    async fn pull(&mut self) -> Option<Result<SpoolRow, NativeSpoolError>> {
        if self.failed {
            return None;
        }
        if self.refuse_after == Some(self.inner.spool_stats().2.result_rows) {
            self.failed = true;
            self.observed.lock().unwrap().state = ScanState::Failed;
            return Some(Err(NativeSpoolError::Authorized(Box::new(
                QueryError::Authorization(fgdb_warden::Error::Expired),
            ))));
        }
        let row = self.inner.pull().await;
        let (_, _, rows, evaluator) = self.inner.spool_stats();
        *self.observed.lock().unwrap() = Observation {
            state: self.inner.spool_state(),
            rows,
            evaluator,
        };
        row.map(|row| row.map(|row| SpoolRow::governed(row.row)))
    }

    fn spool_state(&self) -> ScanState {
        if self.failed {
            ScanState::Failed
        } else {
            self.inner.spool_state()
        }
    }

    fn spool_stats(&self) -> (CommitSeq, ScanKind, GqlExecutionStats, GlaExecutionStats) {
        self.inner.spool_stats()
    }
}

fn input<'q>(
    view: &EmbeddedReadView,
    cx: &'q QueryCx,
    text: &str,
    refuse_after: Option<u64>,
) -> (
    Vec<String>,
    impl SpoolInput + 'q + use<'q>,
    ScanSortTail,
    Arc<Mutex<Observation>>,
) {
    let PreparedNativeRead::Pattern(template) = prepare(text) else {
        panic!("pattern fixture");
    };
    let query = template.bind_parameters(&GqlParameters::new()).unwrap();
    let (plan, tail) = VertexScanPlan::<GraphValueRow>::compile_sort_input(query.plan()).unwrap();
    let cursor = VertexScanCursor::new(
        view.vertex_scan_source(cx, view.frontier()).unwrap(),
        plan,
        policy(INPUT_ROWS),
        move || cx.with_restriction(|| cx.checkpoint()),
    );
    let observed = Arc::new(Mutex::new(Observation {
        state: cursor.state(),
        rows: cursor.row_stats(),
        evaluator: cursor.evaluator_stats(),
    }));
    (
        query.columns().to_vec(),
        Governed {
            inner: cursor,
            observed: Arc::clone(&observed),
            refuse_after,
            failed: false,
        },
        tail,
        observed,
    )
}

#[derive(Clone, Default)]
struct File(Arc<Mutex<Cursor<Vec<u8>>>>);

impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut bytes = self.0.lock().unwrap();
        let at = bytes.position() as usize;
        let count = out
            .remaining()
            .min(bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&bytes.get_ref()[at..at + count]);
            bytes.set_position((at + count) as u64);
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for File {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(std::io::Write::write(&mut *self.0.lock().unwrap(), bytes))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for File {
    fn poll_seek(self: Pin<&mut Self>, _: &mut Context<'_>, at: SeekFrom) -> Poll<io::Result<u64>> {
        Poll::Ready(self.0.lock().unwrap().seek(at))
    }
}

async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let scratch = SpillFile::new(
        cx,
        backing.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 1_000_000,
            max_runs: 32,
            max_run_bytes: 100_000,
        },
    )
    .await
    .unwrap();
    (scratch, backing)
}

#[test]
fn governed_ordering_keeps_native_rows_hidden_keys_windows_and_distinct() {
    let ((), report) = run_async_under_lab(0x50ed_0101, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit()).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(65_536, 0).unwrap();
        for (text, column, hidden) in [
            (
                "MATCH (n:L) RETURN n.p AS p ORDER BY p DESC NULLS FIRST SKIP 1 LIMIT 4",
                "p",
                false,
            ),
            (
                "MATCH (n:L) RETURN n AS id ORDER BY n.p ASC NULLS LAST SKIP 2 LIMIT 3",
                "id",
                true,
            ),
            (
                "MATCH (n:L) RETURN DISTINCT n.p AS p ORDER BY p ASC NULLS LAST SKIP 1 LIMIT 3",
                "p",
                false,
            ),
        ] {
            let wanted = expected(&view, &cx, text);
            let (columns, cursor, tail, observed) = input(&view, &cx, text, None);
            assert_eq!(tail.evaluation_width() > tail.visible_width(), hidden);
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let (spool, work) = ordered_input(
                &cx,
                columns,
                cursor,
                tail,
                policy(wanted.len() as u64),
                &mut scratch,
                &mut destination,
                2,
                4,
                PAGE,
                4096,
                WORK,
            )
            .await
            .unwrap();
            let observed = *observed.lock().unwrap();
            assert_eq!(observed.state, ScanState::Exhausted);
            assert_eq!(observed.rows.result_rows, INPUT_ROWS);
            assert_eq!(observed.rows.snapshot_records, INPUT_ROWS);
            assert!(observed.evaluator.work_units > 0);
            assert_eq!(
                spool.row_stats().snapshot_records,
                observed.rows.snapshot_records
            );
            assert_eq!(spool.evaluator_stats(), observed.evaluator);
            assert_eq!(spool.row_count(), wanted.len() as u64);
            assert_eq!(spool.snapshot_seq(), view.frontier());
            assert_eq!(spool.kind(), ScanKind::Vertex);
            assert_eq!(spool.columns(), &[column]);
            assert_eq!(spool.encoded_columns, 1);
            assert!(work > 0);
            assert_eq!(pool.used(), 0);
            let mut reader = spool.reader(&mut destination);
            for expected in wanted {
                let row = reader.next_row(&cx).await.unwrap().unwrap();
                assert_eq!(row.as_ref(), expected);
            }
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            assert_eq!(reader.state(), ScanState::Exhausted);
            drop(reader);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn governed_encoder_refusals_precede_any_scratch_payload_or_completed_run() {
    let ((), report) = run_async_under_lab(0x50ed_0102, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit()).await;
        let view = db.read_session().unwrap();
        let text = "MATCH (n:L) RETURN n.q AS payload";
        let row_bytes = expected(&view, &cx, text)[0].len();
        for (pool_limit, row_limit, work_limit) in [
            (65_536, row_bytes - 1, WORK),
            (8192, row_bytes, WORK),
            (65_536, row_bytes, 1),
        ] {
            let pool = MemoryPool::new(pool_limit, 0).unwrap();
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, backing) = file(&cx, &pool).await;
            let (columns, cursor, tail, observed) = input(&view, &cx, text, None);
            let error = ordered_input(
                &cx,
                columns,
                cursor,
                tail,
                policy(INPUT_ROWS),
                &mut scratch,
                &mut destination,
                2,
                4,
                PAGE,
                row_limit,
                work_limit,
            )
            .await
            .unwrap_err();
            if row_limit < row_bytes {
                assert!(matches!(
                    error,
                    NativeSpoolError::RowTooLarge { bytes, limit }
                        if bytes == row_bytes && limit == row_limit
                ));
            } else if work_limit == 1 {
                assert!(matches!(
                    error,
                    NativeSpoolError::SortWorkLimit { attempted, limit: 1 }
                        if attempted > 1
                ));
            } else {
                assert!(matches!(
                    error,
                    NativeSpoolError::Spill(SpillError::Memory(
                        MemoryError::ResourceExhausted { requested, available, limit: 8192 }
                    )) if requested > available
                ));
            }
            // The writer itself admitted successfully, then the first real
            // source row was refused before its frame could reach scratch.
            assert_eq!(observed.lock().unwrap().rows.result_rows, 1);
            assert_eq!(scratch.stats(), SpillStats::default());
            assert!(!scratch.is_poisoned());
            assert_eq!(destination.stats().reserved_runs, 1);
            assert_eq!(destination.stats().reserved_bytes, 0);
            assert_eq!(destination.stats().published_runs, 0);
            assert!(destination.is_poisoned());
            assert!(backing.0.lock().unwrap().get_ref().is_empty());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn governed_authorization_failure_keeps_its_cause_and_never_accepts_partial_input() {
    let ((), report) = run_async_under_lab(0x50ed_0103, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let db = seed(&c.commit()).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(65_536, 0).unwrap();
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut destination, backing) = file(&cx, &pool).await;
        let (columns, cursor, tail, observed) = input(
            &view,
            &cx,
            "MATCH (n:L) RETURN n.q AS payload ORDER BY payload LIMIT 0",
            Some(1),
        );
        let error = ordered_input(
            &cx,
            columns,
            cursor,
            tail,
            policy(0),
            &mut scratch,
            &mut destination,
            2,
            4,
            PAGE,
            4096,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(matches!(&error, NativeSpoolError::Authorized(_)));
        let cause = core::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<QueryError>()
            .unwrap();
        assert!(matches!(
            cause,
            QueryError::Authorization(fgdb_warden::Error::Expired)
        ));
        let observed = *observed.lock().unwrap();
        assert_eq!(observed.state, ScanState::Failed);
        assert_eq!(observed.rows.result_rows, 1);
        assert_eq!(observed.rows.snapshot_records, 1);
        assert_eq!(scratch.stats(), SpillStats::default());
        assert_eq!(destination.stats().reserved_runs, 1);
        assert!(destination.stats().reserved_bytes > 0);
        assert!(!backing.0.lock().unwrap().get_ref().is_empty());
        assert_eq!(destination.stats().published_runs, 0);
        assert!(destination.is_poisoned());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
