//! Graph-backed and source-free UNWIND/set operations through external stages.
//! Literal expectations describe the fixture, independently of either executor.

#[path = "buffered_unwind/selective_properties.rs"]
mod selective_properties;

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs, MemoryPool,
    NativeAggregateSpoolError, NativeResultSpool, NativeSpoolError, PreparedBufferedOrder,
    PreparedNativeRead, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::scan_stream::ScanKind;
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError,
    GraphAggregateValue, GraphIntegerErrorKind, GraphSetExecutionError, GraphSymbol,
    GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalF64, CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId,
    PurposeContexts, QueryCx, VId,
};
use std::future::poll_fn;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const WORK: u64 = 1_000_000_000;
const SCRATCH_BYTES: usize = 512 * 1024;
const PAYLOAD_BYTES: usize = 8400;
const UNWIND_MARKER: &str = "unwind-output-witness-";
const BINARY_LEFT_MARKER: &str = "set-left-merge-witness";
const BINARY_RIGHT_MARKER: &str = "set-right-merge-witness";

// Canonical text inserts a memcomparable marker after each eight text bytes.
// The first complete group stays contiguous; a longer raw string does not.
fn text_witness(marker: &'static str) -> &'static [u8] {
    &marker.as_bytes()[..8]
}

#[test]
fn cancellation_witnesses_survive_text_encoding_and_every_short_page_alignment() {
    for marker in [UNWIND_MARKER, BINARY_LEFT_MARKER, BINARY_RIGHT_MARKER] {
        let encoded = CanonicalScalar::ucs_basic_text(&marker.repeat(16))
            .unwrap()
            .encode()
            .unwrap();
        let witness = text_witness(marker);
        // An unknown preceding row/frame length can put the first text group
        // at any page offset. Repetition must leave a whole witness in at
        // least one actual 63-byte append, even when earlier copies straddle.
        for prefix in 0..63 {
            let mut framed = vec![0; prefix];
            framed.extend_from_slice(&encoded);
            assert!(
                framed
                    .chunks(63)
                    .any(|page| page.windows(witness.len()).any(|bytes| bytes == witness)),
                "{marker}: no complete witness at page offset {prefix}"
            );
        }
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, WORK, WORK)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn prepare(text: &str, params: &GqlParameters) -> PreparedBufferedOrder {
    PreparedNativeRead::prepare(text, params, resolve)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .prepare_buffered_order(params)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}
async fn fixture(cx: &CommitCx) -> MemVfs {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 1..=3 {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                (
                    PropertyKeyId(2),
                    CanonicalScalar::ucs_basic_text(&"x".repeat(PAYLOAD_BYTES)).unwrap(),
                ),
            ],
        );
    }
    for (id, from, to, value) in [(1, 1, 2, 7), (2, 1, 2, 8), (3, 2, 3, 9)] {
        batch.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(13)));
    assert_eq!(db.write(cx, change).await.unwrap(), CommitSeq(2));
    drop(db);
    vfs
}
async fn view(cx: &CommitCx, vfs: &MemVfs, pool: &MemoryPool) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        cx,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool.clone(),
        BufferedReadLimits {
            max_root_bytes: 64 * 1024,
            max_source_bytes: 16_000_000,
            max_blocks: 512,
            max_vertex_patches: 512,
            max_work: WORK as usize,
            buffer: BufferLimits {
                max_frames: 1,
                max_ghost_entries: 2,
                max_extent_bytes: 16 * 1024,
            },
        },
    )
    .await
    .unwrap()
}

// The graph uses real MemVfs storage. This adapter only exposes scratch write
// suspension so dropping execution can be checked at actual asynchronous I/O.
#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    suspend_on: Option<&'static [u8]>,
    suspend_also_on: Option<&'static [u8]>,
    suspend_row: Option<Vec<u8>>,
    suspended: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        let at = state.bytes.position() as usize;
        let count = output
            .remaining()
            .min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            output.put_slice(&state.bytes.get_ref()[at..at + count]);
            state.bytes.set_position((at + count) as u64);
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
        let mut state = self.0.lock().unwrap();
        let marker = state
            .suspend_on
            .is_some_and(|marker| bytes.windows(marker.len()).any(|part| part == marker))
            && state
                .suspend_also_on
                .is_none_or(|marker| bytes.windows(marker.len()).any(|part| part == marker));
        let row = state
            .suspend_row
            .as_ref()
            .is_some_and(|row| bytes.windows(row.len()).any(|part| part == row.as_slice()));
        if marker || row {
            state.suspended = true;
            Poll::Pending
        } else {
            Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
        }
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
        Poll::Ready(self.0.lock().unwrap().bytes.seek(at))
    }
}
async fn file(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, File) {
    let backing = File::default();
    let file = SpillFile::new(
        cx,
        backing.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 64_000_000,
            max_runs: 4096,
            max_run_bytes: 8_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
async fn contents(
    spool: &NativeResultSpool,
    file: &mut SpillFile<File>,
    cx: &QueryCx,
) -> Vec<Vec<GraphValue>> {
    let mut reader = spool.reader(file);
    let mut rows = Vec::new();
    while let Some(bytes) = reader.next_row(cx).await.unwrap() {
        rows.push(
            GraphValueRow::decode_canonical(bytes.as_ref())
                .unwrap()
                .values()
                .to_vec(),
        );
    }
    rows
}

#[test]
fn collected_graph_lists_keep_selected_order_duplicates_nulls_scope_and_history() {
    let ((), report) = run_async_under_lab(0xc01d_4801, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let db = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let resident = db.read_session().unwrap();
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let params = GqlParameters::new()
            .with_list(
                "items",
                vec![
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(2)),
                    null(),
                    GraphValue::Vertex(VId(3)),
                ],
            )
            .unwrap();
        let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
        // Parallel edges are only witnesses to the predicate. Input positions,
        // including equal positions, determine the list's order/multiplicity.
        for (text, cut, expected) in [
            (
                "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | x.p] AS values",
                2,
                vec![vec![list(vec![scalar(2), scalar(1), scalar(2)])]],
            ),
            (
                "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | NULL] AS values",
                2,
                vec![vec![list(vec![null(), null(), null()])]],
            ),
            (
                "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | [x.p,NULL,x.p]] AS values",
                2,
                vec![vec![list(vec![
                    list(vec![scalar(2), null(), scalar(2)]),
                    list(vec![scalar(1), null(), scalar(1)]),
                    list(vec![scalar(2), null(), scalar(2)]),
                ])]],
            ),
            (
                "UNWIND [7,7] AS occurrence WITH occurrence,$items AS xs \
                 RETURN occurrence,[x IN xs WHERE (x)-[:R]->() | x.p] AS values",
                2,
                vec![vec![scalar(7), list(vec![scalar(2), scalar(1), scalar(2)])]; 2],
            ),
            (
                "WITH 7 AS x,$items AS xs,10 AS bias CALL (xs,bias) { \
                 RETURN [x IN xs WHERE (x)-[:R]->() | x.p+bias] AS values } RETURN x,values",
                2,
                vec![vec![
                    scalar(7),
                    list(vec![scalar(12), scalar(11), scalar(12)]),
                ]],
            ),
            (
                "MATCH (anchor) FOR SYSTEM_TIME AS OF SEQ 1 WHERE anchor.p=1 \
                 WITH $items AS xs RETURN [x IN xs WHERE (x)<-[:R]-() | x.p] AS values",
                1,
                vec![vec![list(vec![scalar(2), scalar(2), scalar(3)])]],
            ),
            (
                "MATCH (anchor) WHERE anchor.p=1 WITH $items AS xs \
                 RETURN [x IN xs WHERE (x)<-[:R]-() | x.p] AS values",
                2,
                vec![vec![list(vec![scalar(2), scalar(2), scalar(13)])]],
            ),
            (
                "WITH $items AS xs LIMIT 0 RETURN [x IN xs WHERE (x)-[:R]->() | 1/0] AS values",
                2,
                vec![],
            ),
        ] {
            let prepared = PreparedNativeRead::prepare(text, &params, resolve)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let fgdb::QueryResult::Rows { rows, .. } = prepared
                .execute_in_view(&resident, &cx, &params, policy(100))
                .unwrap_or_else(|error| panic!("resident {text}: {error:?}"))
            else {
                panic!("list projection returns rows")
            };
            let expected_native = expected
                .iter()
                .map(|row| {
                    row.iter()
                        .cloned()
                        .map(GraphAggregateValue::Value)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            assert_eq!(rows, expected_native, "resident {text}");
            let buffered = prepared
                .prepare_buffered_order(&params)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            for run_rows in [1, 3] {
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = prepared
                    .spool_ordered_in_view(
                        &resident,
                        &cx,
                        &params,
                        policy(100),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        2,
                        512,
                        run_rows,
                        512,
                        127,
                        32 * 1024,
                        1000,
                        WORK,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("resident spill {text}: {error:?}"));
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                assert_eq!(spill_pool.used(), 0, "resident spill {text}");

                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = buffered
                    .spool_in_view(
                        &mut cold,
                        &cx,
                        policy(100),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        2,
                        512,
                        run_rows,
                        512,
                        127,
                        32 * 1024,
                        1000,
                        WORK,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("buffered {text}: {error:?}"));
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(spool.row_count(), expected.len() as u64);
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                assert_eq!(source_pool.used(), baseline, "{text}");
                assert_eq!(spill_pool.used(), 0, "{text}");
            }
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn null_and_empty_collected_inputs_skip_graph_reads_and_mapping_errors() {
    let ((), report) = run_async_under_lab(0xc01d_4802, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let initial = GqlParameters::new()
            .with_list("items", vec![GraphValue::Vertex(VId(1))])
            .unwrap();
        for tail in ["", "LIMIT 0"] {
            let text = format!(
                "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | 1/0] AS values {tail}"
            );
            let template = PreparedNativeRead::prepare(&text, &initial, resolve).unwrap();
            for (params, value) in [
                (GqlParameters::new().with_null("items").unwrap(), null()),
                (
                    GqlParameters::new().with_list("items", vec![]).unwrap(),
                    GraphValue::List(vec![].into_boxed_slice()),
                ),
            ] {
                // Rebinding NULL retains the originally prepared List domain.
                // A zero graph-record allowance makes an accidental body read
                // fail even if it would otherwise produce an empty relation.
                let prepared = template.prepare_buffered_order(&params).unwrap();
                let before = cold.buffer_stats();
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = prepared
                    .spool_in_view(
                        &mut cold,
                        &cx,
                        GqlQueryPolicy::new(0, 1, WORK, WORK),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        1,
                        512,
                        1,
                        512,
                        127,
                        32 * 1024,
                        1000,
                        WORK,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                let expected = if tail.is_empty() {
                    vec![vec![value]]
                } else {
                    vec![]
                };
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                assert_eq!(spool.row_stats().snapshot_records, 0);
                assert_eq!(cold.buffer_stats(), before);
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn collected_bodies_finish_before_pages_and_keep_cumulative_refusal_boundaries() {
    let ((), report) = run_async_under_lab(0xc01d_4803, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let params = GqlParameters::new()
            .with_list(
                "items",
                vec![
                    GraphValue::Vertex(VId(2)),
                    GraphValue::Vertex(VId(1)),
                    GraphValue::Vertex(VId(2)),
                ],
            )
            .unwrap();
        for tail in ["LIMIT 0", "LIMIT 1", "SKIP 1000 LIMIT 1"] {
            // Mapping the first selected item succeeds. The next item's
            // division must still fail before an outer page can be returned.
            let text = format!(
                "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | 1/(x.p-1)] AS values {tail}"
            );
            let prepared = prepare(&text, &params);
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut cold,
                    &cx,
                    policy(1),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    127,
                    32 * 1024,
                    1000,
                    WORK,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(&error, NativeSpoolError::SetExecution(
                    GraphSetExecutionError::Projection { error, .. })
                    if error.kind == GraphIntegerErrorKind::DivisionByZero),
                "{text}: {error:?}"
            );
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }

        let prepared = prepare(
            "UNWIND [7,7] AS occurrence WITH occurrence,$items AS xs \
             RETURN [x IN xs WHERE (x)-[:R]->() | x.p] AS values LIMIT 0",
            &params,
        );
        let mut exact = None;
        for trial in 0..6 {
            let pool = MemoryPool::new(if trial == 5 { 0 } else { SCRATCH_BYTES }, 0).unwrap();
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut destination, _) = file(&cx, &pool).await;
            let (mut auxiliary, _) = file(&cx, &pool).await;
            let mut allowance = policy(0);
            let mut external = WORK;
            if let Some((work, entries, sort)) = exact {
                allowance.evaluator.max_work_units = work - u64::from(trial == 2);
                allowance.evaluator.max_scratch_entries = entries - u64::from(trial == 3);
                external = sort - u64::from(trial == 4);
            }
            let result = prepared
                .spool_in_view(
                    &mut cold,
                    &cx,
                    allowance,
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    127,
                    32 * 1024,
                    1000,
                    external,
                )
                .await;
            if trial < 2 {
                let (spool, work) = result.unwrap();
                assert!(contents(&spool, &mut destination, &cx).await.is_empty());
                let stats = spool.evaluator_stats();
                let measured = (stats.work_units, stats.scratch_entries, work);
                assert!(measured.0 > 0 && measured.1 > 0 && measured.2 > 0);
                if let Some(previous) = exact {
                    assert_eq!(measured, previous);
                }
                exact = Some(measured);
            } else {
                let error = result.unwrap_err();
                match trial {
                    2 | 3 => assert!(
                        matches!(&error, NativeSpoolError::BufferedExecute(inner)
                            if matches!(**inner, GqlQueryError::Evaluator(_))),
                        "{error:?}"
                    ),
                    4 => assert!(
                        matches!(error, NativeSpoolError::SortWorkLimit { .. }),
                        "{error:?}"
                    ),
                    5 => assert!(
                        matches!(
                            error,
                            NativeSpoolError::Spill(
                                fgdb_strata::tiered::memory::SpillError::Memory(_)
                            )
                        ),
                        "{error:?}"
                    ),
                    _ => unreachable!(),
                }
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(pool.used(), 0, "trial {trial}");
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_suspended_collected_output_refunds_rows_frames_and_list_storage() {
    use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan, SpillSetApplyKind};

    let ((), report) = run_async_under_lab(0xc01d_4804, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let items = [2, 1, 2].map(|id| GraphValue::Vertex(VId(id))).to_vec();
        let input = GraphValue::List(items.clone().into_boxed_slice());
        let params = GqlParameters::new().with_list("items", items).unwrap();
        let template = PreparedNativeRead::prepare(
            "WITH $items AS xs RETURN [x IN xs WHERE (x)-[:R]->() | x.p] AS values",
            &params,
            resolve,
        )
        .unwrap();
        let PreparedNativeRead::Set(bound) = &template else {
            panic!("graph-reading list must use the native relational plan")
        };
        let plan = AsyncSpillSetPlan::compile(&bound.bind_parameters(&params).unwrap()).unwrap();
        let AsyncSpillSetSourcePlan::Apply(collect) = plan.source() else {
            panic!("expected the compiler's collected Apply source")
        };
        // The binder retains xs and a private evaluated copy of the input
        // list. Derive the raw Apply row from its actual helper/schema, before
        // the downstream Project removes those two cells.
        assert_eq!(collect.kind(), SpillSetApplyKind::Collect { null_input: 1 });
        assert_eq!(collect.left().columns().len(), 2);
        assert_eq!(collect.left().columns()[0], "xs");
        let left = GraphValueRow::from_owned_values(vec![input.clone(), input.clone()]);
        let mut collection = collect
            .start_collection(0, &mut |_| Ok::<_, ()>(()))
            .unwrap();
        for value in [2, 1, 2] {
            collection = collection
                .push(
                    &GraphValueRow::from_owned_values(vec![scalar(value)]),
                    &mut |_| Ok::<_, ()>(()),
                )
                .unwrap();
        }
        let row = collection.finish(&left, &mut |_| Ok::<_, ()>(())).unwrap();
        assert_eq!(
            row.values(),
            &[
                input.clone(),
                input,
                GraphValue::List(vec![scalar(2), scalar(1), scalar(2)].into_boxed_slice()),
            ]
        );
        let marker = row.canonical_bytes().unwrap();
        // One outer occurrence starts an empty Apply output. Its eight-byte
        // length plus this exact raw row fills a page, so write_row must await
        // its append while the list, imports and outer-row guards are live.
        // A larger page could defer that I/O until writer.finish, after they
        // were released; the final one-column row would be too late as well.
        let page_bytes = std::mem::size_of::<u64>() + marker.len();
        assert!(page_bytes <= 64 * 1024);
        let prepared = template.prepare_buffered_order(&params).unwrap();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        let (mut auxiliary, c) = file(&cx, &spill_pool).await;
        for backing in [&a, &b, &c] {
            backing.0.lock().unwrap().suspend_row = Some(marker.clone());
        }
        let mut future = prepared.spool_in_view(
            &mut cold,
            &cx,
            policy(1),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            1,
            512,
            1,
            512,
            page_bytes,
            32 * 1024,
            1000,
            WORK,
        );
        let blocked = poll_fn(|task| {
            let result = future.as_mut().poll(task);
            let suspended = [
                a.0.lock().unwrap().suspended,
                b.0.lock().unwrap().suspended,
                c.0.lock().unwrap().suspended,
            ];
            if let Some(index) = suspended.iter().position(|&seen| seen) {
                assert!(result.is_pending());
                assert_eq!(suspended.iter().filter(|&&seen| seen).count(), 1);
                Poll::Ready(index)
            } else {
                assert!(
                    result.is_pending(),
                    "raw collected Apply append must be reached"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > 0);
        drop(future);
        assert!([&scratch, &destination, &auxiliary][blocked].is_poisoned());
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn element_metadata_matches_bound_values_through_resident_and_cold_spill() {
    use fgdb_delta_types::LabelId;
    use fgdb_gql::{GraphSymbolResolver, ReverseSymbolCatalog};

    struct Catalog;
    impl GraphSymbolResolver for Catalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            match (kind, name) {
                (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(RelationId(2))),
                _ => resolve(kind, name),
            }
        }
        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_label(LabelId(1), "Alpha");
            catalog.insert_label(LabelId(2), "Omega");
            catalog.insert_relation(RelationId(1), "R");
            catalog.insert_relation(RelationId(2), "S");
            catalog.insert_property(PropertyKeyId(1), "p");
            Some(catalog)
        }
    }

    let ((), report) = run_async_under_lab(0xc01d_4510, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        batch.create_vertex(
            VId(1),
            vec![LabelId(1), LabelId(2)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(1))],
        );
        batch.create_vertex(
            VId(u128::MAX),
            vec![LabelId(2)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(2))],
        );
        batch.add_edge(
            EId(u128::MAX),
            VId(1),
            VId(u128::MAX),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
        );
        db.write(&commit, batch).await.unwrap();
        let mut batch = WriteBatch::new(RelationId(2));
        batch.add_edge(
            EId(2),
            VId(u128::MAX),
            VId(1),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(8))],
        );
        db.write(&commit, batch).await.unwrap();
        let resident = db.read_session().unwrap();
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = source_pool.used();
        let params = GqlParameters::new();
        let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
        let text = |name| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
        let props = |value| GraphValue::map(vec![("p".into(), scalar(value))]).unwrap();
        let labels = [
            list(vec![text("Alpha"), text("Omega")]),
            list(vec![text("Omega")]),
        ];
        let vertex_query = "MATCH (n) WITH [n,n,NULL] AS items RETURN \
            [x IN items | labels(x)] AS ls,[x IN items | properties(x)] AS ps,\
            [x IN items | keys(x)] AS ks ORDER BY ps";
        let vertex_expected = (0..2)
            .map(|at| {
                vec![
                    list(vec![labels[at].clone(), labels[at].clone(), null()]),
                    list(vec![props(at as i64 + 1), props(at as i64 + 1), null()]),
                    list(vec![list(vec![text("p")]), list(vec![text("p")]), null()]),
                ]
            })
            .collect::<Vec<_>>();
        for (dynamic, bound, expected) in [
            (
                vertex_query,
                "MATCH (n) RETURN [labels(n),labels(n),NULL] AS ls,\
                 [properties(n),properties(n),NULL] AS ps,[keys(n),keys(n),NULL] AS ks ORDER BY ps",
                vertex_expected.clone(),
            ),
            (
                "MATCH ()-[r]->() WITH [r,NULL,r] AS items RETURN \
                 [x IN items | type(x)] AS ts,[x IN items | keys(x)] AS ks,\
                 [x IN items | properties(x)] AS ps ORDER BY ps",
                "MATCH ()-[r]->() RETURN [type(r),NULL,type(r)] AS ts,\
                 [keys(r),NULL,keys(r)] AS ks,[properties(r),NULL,properties(r)] AS ps ORDER BY ps",
                [("R", 7), ("S", 8)]
                    .into_iter()
                    .map(|(relation, value)| {
                        vec![
                            list(vec![text(relation), null(), text(relation)]),
                            list(vec![list(vec![text("p")]), null(), list(vec![text("p")])]),
                            list(vec![props(value), null(), props(value)]),
                        ]
                    })
                    .collect(),
            ),
            (
                "MATCH path = (a)-[r:R]->(b) WITH nodes(path) AS ns,relationships(path) AS rs \
                 RETURN [x IN ns | labels(x)] AS ls,[x IN rs | type(x)] AS ts,\
                 [x IN rs | properties(x)] AS ps ORDER BY ts",
                "MATCH (a)-[r:R]->(b) RETURN [labels(a),labels(b)] AS ls,\
                 [type(r)] AS ts,[properties(r)] AS ps ORDER BY ts",
                vec![vec![
                    list(labels.to_vec()),
                    list(vec![text("R")]),
                    list(vec![props(7)]),
                ]],
            ),
        ] {
            let expected_native = expected
                .iter()
                .map(|row| {
                    row.iter()
                        .cloned()
                        .map(GraphAggregateValue::Value)
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            for statement in [bound, dynamic] {
                let query = PreparedNativeRead::prepare(statement, &params, Catalog).unwrap();
                let fgdb::QueryResult::Rows { rows, .. } = query
                    .execute_in_view(&resident, &cx, &params, policy(100))
                    .unwrap_or_else(|error| panic!("{statement}: {error:?}"))
                else {
                    panic!("metadata reads return rows")
                };
                assert_eq!(rows, expected_native, "{statement}");
            }
            let prepared = PreparedNativeRead::prepare(dynamic, &params, Catalog).unwrap();
            let buffered = prepared.prepare_buffered_order(&params).unwrap();
            for run_rows in [1, 3] {
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = prepared
                    .spool_ordered_in_view(
                        &resident,
                        &cx,
                        &params,
                        policy(100),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        run_rows,
                        512,
                        run_rows,
                        64,
                        127,
                        32 * 1024,
                        100,
                        WORK,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("resident {dynamic}: {error:?}"));
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                assert_eq!(spool.snapshot_seq(), CommitSeq(2));
                assert_eq!(spill_pool.used(), 0);

                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let (spool, _) = buffered
                    .spool_in_view(
                        &mut cold,
                        &cx,
                        policy(100),
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        run_rows,
                        512,
                        1,
                        512,
                        127,
                        32 * 1024,
                        100,
                        WORK,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("buffered {dynamic}: {error:?}"));
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
                assert_eq!(spool.snapshot_seq(), CommitSeq(2));
                let admitted = spool.evaluator_stats();
                // Point reads may retain the configured one authenticated
                // cache frame; query-owned decoded rows must all be released.
                assert!(source_pool.used() <= retained + 16 * 1024);
                assert_eq!(spill_pool.used(), 0);
                let mut below = policy(100);
                below.evaluator.max_work_units = admitted.work_units - 1;
                let error = buffered
                    .spool_in_view(
                        &mut cold,
                        &cx,
                        below,
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        run_rows,
                        512,
                        1,
                        512,
                        127,
                        32 * 1024,
                        100,
                        WORK,
                    )
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, NativeSpoolError::BufferedExecute(ref error)
                    if matches!(**error, GqlQueryError::Evaluator(_))),
                    "{error:?}"
                );
                assert!(source_pool.used() <= retained + 16 * 1024);
                assert_eq!(spill_pool.used(), 0);
            }
        }

        // Suspend an actual metadata output frame, then drop the operation.
        // No borrowed cold record, recursive frame or output row may survive.
        let prepared = PreparedNativeRead::prepare(vertex_query, &params, Catalog)
            .unwrap()
            .prepare_buffered_order(&params)
            .unwrap();
        let marker = GraphValueRow::from_owned_values(vertex_expected[0].clone())
            .canonical_bytes()
            .unwrap();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        let (mut auxiliary, c) = file(&cx, &spill_pool).await;
        for backing in [&a, &b, &c] {
            backing.0.lock().unwrap().suspend_row = Some(marker.clone());
        }
        let mut future = prepared.spool_in_view(
            &mut cold,
            &cx,
            policy(100),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            1,
            512,
            1,
            512,
            8 + marker.len(),
            32 * 1024,
            100,
            WORK,
        );
        let blocked = poll_fn(|task| {
            let result = future.as_mut().poll(task);
            let suspended = [
                a.0.lock().unwrap().suspended,
                b.0.lock().unwrap().suspended,
                c.0.lock().unwrap().suspended,
            ];
            if let Some(index) = suspended.iter().position(|&seen| seen) {
                assert!(result.is_pending());
                Poll::Ready(index)
            } else {
                assert!(
                    result.is_pending(),
                    "metadata output append must be reached: {result:?}"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > 0);
        drop(future);
        assert!([&scratch, &destination, &auxiliary][blocked].is_poisoned());
        assert!(source_pool.used() <= retained + 16 * 1024);
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_unwind_keeps_nested_values_multiplicity_history_and_completed_pages() {
    let ((), report) = run_async_under_lab(0xc01d_4101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, cut, expected) in [
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,NULL,p] AS item RETURN p,item ORDER BY p,item",
                2,
                vec![
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), null()],
                    vec![scalar(2), scalar(2)],
                    vec![scalar(2), scalar(2)],
                    vec![scalar(2), null()],
                    vec![scalar(13), scalar(13)],
                    vec![scalar(13), scalar(13)],
                    vec![scalar(13), null()],
                ],
            ),
            (
                "MATCH (n) WITH NULL AS items UNWIND items AS item RETURN item",
                2,
                vec![],
            ),
            (
                "MATCH (n) WITH [] AS items UNWIND items AS item RETURN item",
                2,
                vec![],
            ),
            (
                "MATCH (n) WITH n.p AS p UNWIND [[],NULL,[p,p+1]] AS items UNWIND items AS item RETURN item ORDER BY item",
                2,
                [1, 2, 2, 3, 13, 14]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 WITH [{owner:n.p,items:[n.p,n.p+10]}] AS records UNWIND records AS record UNWIND record.items AS item RETURN record.owner AS owner,item ORDER BY owner,item",
                1,
                vec![
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), scalar(11)],
                    vec![scalar(2), scalar(2)],
                    vec![scalar(2), scalar(12)],
                    vec![scalar(3), scalar(3)],
                    vec![scalar(3), scalar(13)],
                ],
            ),
            (
                "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 2 UNWIND [p,p-1,p] AS item WITH DISTINCT item ORDER BY item SKIP 1 LIMIT 2 RETURN item+100 AS value ORDER BY value",
                2,
                vec![vec![scalar(102)], vec![scalar(112)]],
            ),
            (
                "MATCH (a)-[r:R]->(b) WITH r.p AS p UNWIND [p,p] AS item RETURN item ORDER BY item",
                2,
                [7, 7, 8, 8, 9, 9]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH r.p+s.p AS p UNWIND [p,p+1] AS item RETURN item ORDER BY item",
                2,
                [16, 17, 17, 18]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "MATCH (n) WITH n.p AS p ORDER BY p LIMIT 0 UNWIND [1/(p-p)] AS item RETURN item",
                2,
                vec![],
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(expected.len() as u64),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    1000,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        assert!(view.buffer_stats().bypasses > 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_rows_use_values_provenance_without_any_graph_record_allowance() {
    let ((), report) = run_async_under_lab(0xc01d_4201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let buffer = view.buffer_stats();
        for (text, cut, expected) in [
            ("RETURN 42 AS answer", 2, vec![vec![scalar(42)]]),
            (
                "UNWIND [3,1,3,NULL] AS item RETURN item ORDER BY item",
                2,
                vec![
                    vec![scalar(1)],
                    vec![scalar(3)],
                    vec![scalar(3)],
                    vec![null()],
                ],
            ),
            (
                "WITH 2 AS x UNWIND [x,x+1,x] AS item RETURN item ORDER BY item",
                2,
                vec![vec![scalar(2)], vec![scalar(2)], vec![scalar(3)]],
            ),
            (
                "UNWIND [{items:[1,2]},{items:[]},{items:NULL}] AS record UNWIND record.items AS item RETURN item ORDER BY item",
                2,
                vec![vec![scalar(1)], vec![scalar(2)]],
            ),
            (
                "UNWIND [3,1,3] AS item WITH DISTINCT item ORDER BY item DESC LIMIT 1 RETURN item+10 AS value",
                2,
                vec![vec![scalar(13)]],
            ),
            (
                "UNWIND [7] AS item FOR SYSTEM_TIME AS OF SEQ 1 RETURN item",
                1,
                vec![vec![scalar(7)]],
            ),
            (
                "UNWIND [7] AS item FOR SYSTEM_TIME AS OF SEQ 0 RETURN item",
                0,
                vec![vec![scalar(7)]],
            ),
            ("UNWIND [] AS item RETURN item", 2, vec![]),
            ("UNWIND NULL AS item RETURN item", 2, vec![]),
            ("RETURN 42 AS answer LIMIT 0", 2, vec![]),
            (
                "WITH 0 AS x LIMIT 0 UNWIND [1/x] AS item RETURN item",
                2,
                vec![],
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, expected.len() as u64, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    100,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), ScanKind::Values, "{text}");
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
            assert_eq!(spool.row_stats().snapshot_records, 0);
            assert!(spool.evaluator_stats().work_units > 0);
            assert!(spool.evaluator_stats().scratch_entries > 0);
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            assert_eq!(
                view.buffer_stats(),
                buffer,
                "{text}: no graph load is needed"
            );
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        let future = prepare(
            "UNWIND [7] AS item FOR SYSTEM_TIME AS OF SEQ 3 RETURN item LIMIT 0",
            &GqlParameters::new(),
        );
        let (mut scratch, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        let (mut auxiliary, _) = file(&cx, &spill_pool).await;
        let error = future
            .spool_in_view(
                &mut view,
                &cx,
                GqlQueryPolicy::new(0, 0, WORK, WORK),
                &mut scratch,
                &mut destination,
                &mut auxiliary,
                1,
                512,
                1,
                512,
                1024,
                32 * 1024,
                1,
                WORK,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::BufferedRead(ref error)
            if matches!(**error, fgdb::BufferedReadError::BeyondPublication {
                requested: CommitSeq(3), publication: CommitSeq(2),
            })),
            "{error:?}"
        );
        assert_eq!(view.buffer_stats(), buffer);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_binary_sets_preserve_six_bags_nested_values_and_all_source_provenance() {
    let ((), report) = run_async_under_lab(0xc01d_4301, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let mut cases = Vec::new();
        for (operation, expected) in [
            (
                "UNION ALL",
                vec![
                    None,
                    None,
                    None,
                    Some(1),
                    Some(2),
                    Some(2),
                    Some(2),
                    Some(3),
                ],
            ),
            ("UNION DISTINCT", vec![None, Some(1), Some(2), Some(3)]),
            ("INTERSECT ALL", vec![None, Some(2)]),
            ("INTERSECT DISTINCT", vec![None, Some(2)]),
            ("EXCEPT ALL", vec![None, Some(1), Some(2)]),
            ("EXCEPT DISTINCT", vec![Some(1)]),
        ] {
            cases.push((
                format!(
                    "(UNWIND [2,NULL,1,2,NULL] AS value RETURN value) {operation} (UNWIND [3,NULL,2] AS value RETURN value)"
                ),
                2,
                0,
                expected.into_iter().map(|n| vec![n.map_or_else(null, scalar)]).collect::<Vec<_>>(),
            ));
        }
        for (text, cut, records, expected) in [
            (
                "MATCH (n) RETURN n.p AS value UNION ALL UNWIND [2,2] AS value RETURN value+0 AS value",
                2,
                3,
                vec![
                    vec![scalar(1)],
                    vec![scalar(2)],
                    vec![scalar(2)],
                    vec![scalar(2)],
                    vec![scalar(13)],
                ],
            ),
            (
                "UNWIND [8,10] AS value RETURN value+0 AS value EXCEPT ALL MATCH (a)-[r:R]->(b) RETURN r.p AS value",
                2,
                3,
                vec![vec![scalar(10)]],
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS value UNION ALL (MATCH (a)-[r:R]->(b) RETURN r.p AS value UNION ALL RETURN 1 AS value)",
                1,
                6,
                [1, 1, 2, 3, 7, 8, 9].map(|n| vec![scalar(n)]).to_vec(),
            ),
            (
                "(UNWIND [4,1,4,2] AS value RETURN value ORDER BY value DESC LIMIT 2) EXCEPT ALL (UNWIND [4] AS value RETURN value)",
                2,
                0,
                vec![vec![scalar(4)]],
            ),
            (
                "((UNWIND [1,2,2] AS value RETURN value) EXCEPT ALL (UNWIND [2] AS value RETURN value)) UNION ALL (UNWIND [3] AS value RETURN value) ORDER BY value DESC SKIP 1 LIMIT 2",
                2,
                0,
                vec![vec![scalar(2)], vec![scalar(1)]],
            ),
            (
                "UNWIND [1,2] AS n WITH n WHERE n>1 UNWIND [n,n] AS value RETURN value UNION ALL UNWIND [3] AS value RETURN value",
                2,
                0,
                vec![vec![scalar(2)], vec![scalar(2)], vec![scalar(3)]],
            ),
            (
                "RETURN 1 AS a,2 AS b UNION DISTINCT RETURN 2 AS b,1 AS a",
                2,
                0,
                vec![vec![scalar(1), scalar(2)]],
            ),
            (
                "UNWIND [] AS value RETURN value EXCEPT ALL UNWIND [1] AS value RETURN value",
                2,
                0,
                vec![],
            ),
        ] {
            cases.push((text.to_owned(), cut, records, expected));
        }
        let mut nested = vec![
            vec![null()],
            vec![scalar(1)],
            vec![GraphValue::Scalar(CanonicalScalar::Float(
                CanonicalF64::new(1.0),
            ))],
            vec![GraphValue::List(
                vec![scalar(1), scalar(2)].into_boxed_slice(),
            )],
            vec![
                GraphValue::map(vec![
                    ("a".into(), scalar(1)),
                    (
                        "z".into(),
                        GraphValue::List(vec![scalar(2), null()].into_boxed_slice()),
                    ),
                ])
                .unwrap(),
            ],
        ];
        nested.sort();
        cases.push((
            "UNWIND [1,1.0,NULL,{z:[2,NULL],a:1},{a:1,z:[2,NULL]},[1,2]] AS value RETURN value UNION DISTINCT UNWIND [1.0,NULL,{a:1,z:[2,NULL]}] AS value RETURN value".to_owned(),
            2,
            0,
            nested,
        ));
        for (text, cut, records, expected) in cases {
            let prepared = prepare(&text, &GqlParameters::new());
            let before = view.buffer_stats();
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(records, expected.len() as u64, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    100,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), ScanKind::Set, "{text}");
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut), "{text}");
            assert_eq!(spool.row_stats().snapshot_records, records, "{text}");
            assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            if records == 0 {
                assert_eq!(view.buffer_stats(), before, "{text}: graph-free operands");
            }
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_binary_allowances_accumulate_across_children_before_the_final_page() {
    let ((), report) = run_async_under_lab(0xc01d_4302, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, private_rows, highest) in [
            (
                "MATCH (n) RETURN n.p AS value UNION ALL MATCH (m) RETURN m.p AS value ORDER BY value DESC LIMIT 1",
                6,
                13,
            ),
            (
                "MATCH (n) WITH n.p AS value,COUNT(*) AS c WITH value,SUM(c) AS copies RETURN value UNION ALL MATCH (m) WITH m.p AS value,COUNT(*) AS c RETURN value ORDER BY value DESC LIMIT 1",
                6,
                13,
            ),
            (
                "MATCH (n) RETURN n.p AS value UNION ALL UNWIND [0,1] AS bump MATCH (m) RETURN m.p+bump AS value ORDER BY value DESC LIMIT 1",
                9,
                14,
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let mut exact = None;
            for trial in 0..7 {
                let (mut scratch, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                let (mut auxiliary, _) = file(&cx, &spill_pool).await;
                let mut budget = GqlQueryPolicy::new(if trial == 2 { 5 } else { 6 }, 1, WORK, WORK);
                let mut physical = WORK;
                if let Some((native, entries, sort)) = exact {
                    budget.evaluator.max_work_units = native - u64::from(trial == 3);
                    budget.evaluator.max_scratch_entries = entries - u64::from(trial == 4);
                    physical = sort - u64::from(trial == 5);
                }
                let result = prepared
                    .spool_in_view(
                        &mut view,
                        &cx,
                        budget,
                        &mut scratch,
                        &mut destination,
                        &mut auxiliary,
                        1,
                        512,
                        1,
                        512,
                        1024,
                        32 * 1024,
                        private_rows - u64::from(trial == 6),
                        physical,
                    )
                    .await;
                if trial < 2 {
                    let (spool, work) =
                        result.unwrap_or_else(|error| panic!("trial {trial}: {error:?}"));
                    assert_eq!(spool.kind(), ScanKind::Set);
                    assert_eq!(spool.row_stats().snapshot_records, 6);
                    assert_eq!(
                        contents(&spool, &mut destination, &cx).await,
                        vec![vec![scalar(highest)]]
                    );
                    let stats = spool.evaluator_stats();
                    let measured = (stats.work_units, stats.scratch_entries, work);
                    if let Some(prior) = exact {
                        assert_eq!(prior, measured);
                    }
                    exact = Some(measured);
                } else {
                    let error = result.unwrap_err();
                    match trial {
                        2 => assert!(
                            matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Rows(ref limit)
                        if limit.dimension == GqlBudgetDimension::SnapshotRecords && limit.limit == 5 && limit.observed == 6)),
                            "{error:?}"
                        ),
                        3 | 4 => assert!(
                            matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Evaluator(_))),
                            "{error:?}"
                        ),
                        5 => assert!(
                            matches!(error, NativeSpoolError::SortWorkLimit { .. }),
                            "{error:?}"
                        ),
                        6 => assert!(
                            matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Rows(ref limit)
                        if limit.dimension == GqlBudgetDimension::ResultRows && limit.limit == private_rows - 1 && limit.observed == private_rows)),
                            "{error:?}"
                        ),
                        _ => unreachable!(),
                    }
                }
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_binary_zero_pages_preserve_both_child_failures_and_admission() {
    let ((), report) = run_async_under_lab(0xc01d_4303, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let before = view.buffer_stats();
        for (text, input_limit, expected) in [
            (
                "(UNWIND [] AS value RETURN value+0 AS value) INTERSECT ALL (UNWIND [1,0] AS n RETURN 1/n AS value) LIMIT 0",
                3,
                Some(GraphIntegerErrorKind::DivisionByZero),
            ),
            (
                "(UNWIND [] AS value RETURN value) EXCEPT DISTINCT (UNWIND [[1],2] AS items UNWIND items AS item RETURN item AS value) LIMIT 0",
                3,
                Some(GraphIntegerErrorKind::IncompatibleOperands),
            ),
            (
                "(UNWIND [1,0] AS n RETURN 1/n AS value) UNION ALL (UNWIND [[1],2] AS items UNWIND items AS item RETURN item+0 AS value) LIMIT 0",
                3,
                Some(GraphIntegerErrorKind::DivisionByZero),
            ),
            (
                "(UNWIND [1,2] AS value RETURN value) INTERSECT ALL (UNWIND [3,4] AS value RETURN value) LIMIT 0",
                3,
                None,
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, 0, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    input_limit,
                    WORK,
                )
                .await
                .unwrap_err();
            if let Some(kind) = expected {
                assert!(
                    matches!(error, NativeSpoolError::SetExecution(
                    GraphSetExecutionError::Projection { row: 1, error, .. }
                ) if error.kind == kind),
                    "{text}: {error:?}"
                );
            } else {
                assert!(
                    matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                    if matches!(**inner, GqlQueryError::Rows(ref limit)
                    if limit.limit == 3 && limit.observed == 4)),
                    "{error:?}"
                );
            }
            assert_eq!(view.buffer_stats(), before);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_numeric_aggregates_distinguish_the_unit_empty_groups_and_selected_groups() {
    let ((), report) = run_async_under_lab(0xc01d_4202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let buffer = view.buffer_stats();
        for (text, expected) in [
            (
                "RETURN COUNT(*) AS rows,SUM(7) AS total",
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Count(1),
                        GraphAggregateValue::Integer(7),
                    ],
                )],
            ),
            (
                "UNWIND [] AS x RETURN COUNT(*) AS rows,SUM(x) AS total",
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Count(0),
                        GraphAggregateValue::Value(null()),
                    ],
                )],
            ),
            (
                "UNWIND NULL AS x RETURN COUNT(*) AS rows,SUM(x) AS total",
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Count(0),
                        GraphAggregateValue::Value(null()),
                    ],
                )],
            ),
            (
                "UNWIND [3,1,3,NULL] AS x RETURN COUNT(*) AS rows,SUM(x) AS total",
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Count(4),
                        GraphAggregateValue::Integer(7),
                    ],
                )],
            ),
            (
                "UNWIND [3,1,3,NULL] AS x RETURN COUNT(DISTINCT x) AS rows,SUM(DISTINCT x) AS total",
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Count(2),
                        GraphAggregateValue::Integer(4),
                    ],
                )],
            ),
            (
                "UNWIND [1,1,2,3,3] AS x WITH x,COUNT(*) AS c WHERE c>1 RETURN SUM(c) AS total,COUNT(*) AS groups",
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Integer(4),
                        GraphAggregateValue::Count(2),
                    ],
                )],
            ),
            (
                "UNWIND [{category:'a',amount:5},{category:'b',amount:7},{category:'a',amount:3}] AS row RETURN row.category AS category, sum(row.amount) AS total ORDER BY total DESC",
                vec![
                    (
                        vec![GraphValue::Scalar(
                            CanonicalScalar::ucs_basic_text("a").unwrap(),
                        )],
                        vec![GraphAggregateValue::Integer(8)],
                    ),
                    (
                        vec![GraphValue::Scalar(
                            CanonicalScalar::ucs_basic_text("b").unwrap(),
                        )],
                        vec![GraphAggregateValue::Integer(7)],
                    ),
                ],
            ),
            (
                "UNWIND [4,1,3,4,NULL] AS x WITH x WHERE x IS NOT NULL RETURN x%2 AS bucket,COUNT(*) AS rows,SUM(x) AS total GROUP BY x%2 HAVING total>0 ORDER BY total DESC",
                vec![
                    (
                        vec![scalar(0)],
                        vec![
                            GraphAggregateValue::Count(2),
                            GraphAggregateValue::Integer(8),
                        ],
                    ),
                    (
                        vec![scalar(1)],
                        vec![
                            GraphAggregateValue::Count(2),
                            GraphAggregateValue::Integer(4),
                        ],
                    ),
                ],
            ),
            (
                "UNWIND [] AS x RETURN x AS bucket,COUNT(*) AS rows,SUM(x) AS total GROUP BY x",
                vec![],
            ),
            ("RETURN COUNT(*) AS rows,SUM(7) AS total LIMIT 0", vec![]),
        ] {
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(text, &params, resolve)
                .unwrap()
                .prepare_buffered_aggregate(&params)
                .unwrap();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, expected.len() as u64, WORK, WORK),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    100,
                    WORK,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), ScanKind::Values);
            assert_eq!(spool.snapshot_seq(), CommitSeq(2));
            assert_eq!(spool.row_stats().snapshot_records, 0);
            assert!(spool.evaluator_stats().work_units > 0);
            let mut reader = spool.reader(&mut destination, None);
            let mut actual = Vec::new();
            while let Some(row) = reader.next_row(&cx).await.unwrap() {
                actual.push((row.keys().to_vec(), row.values().to_vec()));
            }
            drop(reader);
            assert_eq!(actual, expected, "{text}");
            assert_eq!(view.buffer_stats(), buffer);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_free_zero_private_input_quota_and_late_failures_never_succeed_via_limit_zero() {
    let ((), report) = run_async_under_lab(0xc01d_4203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let buffer = view.buffer_stats();
        for (text, input_limit, expression) in [
            ("RETURN 1 AS value LIMIT 0", 0, false),
            ("UNWIND [] AS item RETURN item", 0, false),
            ("UNWIND [1,2] AS item RETURN item LIMIT 0", 1, false),
            (
                "UNWIND [1,0] AS item RETURN 1/item AS value LIMIT 0",
                2,
                true,
            ),
            (
                "UNWIND [[1],2] AS items UNWIND items AS item RETURN 1/(item-item) AS value LIMIT 0",
                2,
                true,
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, 0, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    input_limit,
                    WORK,
                )
                .await
                .unwrap_err();
            if expression {
                assert!(
                    matches!(
                        error,
                        NativeSpoolError::SetExecution(GraphSetExecutionError::Projection {
                            row: 1,
                            ..
                        })
                    ),
                    "{text}: {error:?}"
                );
            } else {
                assert!(
                    matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                    if matches!(**inner,GqlQueryError::Rows(_))),
                    "{text}: {error:?}"
                );
            }
            assert_eq!(view.buffer_stats(), buffer);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expanded_occurrences_feed_external_aggregates_including_empty_and_distinct_inputs() {
    let ((), report) = run_async_under_lab(0xc01d_4102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, count, sum) in [
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,p,NULL] AS item RETURN COUNT(*) AS rows,SUM(item) AS total",
                9,
                GraphAggregateValue::Integer(32),
            ),
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,p,NULL] AS item RETURN COUNT(DISTINCT item) AS rows,SUM(DISTINCT item) AS total",
                3,
                GraphAggregateValue::Integer(16),
            ),
            (
                "MATCH (n) WITH n.p AS p UNWIND [p,p+1] AS item WITH DISTINCT item ORDER BY item DESC LIMIT 2 RETURN COUNT(*) AS rows,SUM(item) AS total",
                2,
                GraphAggregateValue::Integer(27),
            ),
            (
                "MATCH (n) WITH [] AS items UNWIND items AS item RETURN COUNT(*) AS rows,SUM(item) AS total",
                0,
                GraphAggregateValue::Value(null()),
            ),
            (
                "MATCH (n) WITH n.p%2 AS bucket,COUNT(*) AS c,SUM(n.p) AS total RETURN COUNT(*) AS rows,SUM(total) AS total",
                2,
                GraphAggregateValue::Integer(16),
            ),
            (
                "UNWIND [1,1,2] AS wanted MATCH (n {p:wanted}) WITH n.p AS matched RETURN COUNT(*) AS rows,SUM(matched) AS total",
                3,
                GraphAggregateValue::Integer(4),
            ),
        ] {
            let params = GqlParameters::new();
            let prepared = PreparedNativeRead::prepare(text, &params, resolve)
                .unwrap()
                .prepare_buffered_aggregate(&params)
                .unwrap();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(1),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let mut reader = spool.reader(&mut destination, None);
            let row = reader.next_row(&cx).await.unwrap().unwrap();
            assert!(row.keys().is_empty());
            assert_eq!(
                row.values(),
                &[GraphAggregateValue::Count(count), sum],
                "{text}"
            );
            drop(row);
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_unwind_errors_precede_parent_expressions_and_final_zero_limits() {
    let ((), report) = run_async_under_lab(0xc01d_4103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for tail in ["LIMIT 0", "LIMIT 1", "SKIP 1000 LIMIT 1"] {
            let text = format!(
                "MATCH (n) WITH n.p AS p UNWIND [[p],2] AS items UNWIND items AS item RETURN 1/(p-p) AS value {tail}"
            );
            let prepared = prepare(&text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(1),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    1000,
                    WORK,
                )
                .await
                .unwrap_err();
            // An incorrectly fused parent would divide by zero on its first
            // row. The child must finish and report its second-row type error.
            assert!(
                matches!(error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { row: 1, column: 2, error }
            ) if error.kind == GraphIntegerErrorKind::IncompatibleOperands),
                "{text}: {error:?}"
            );
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expanded_input_quota_and_native_work_are_independent_of_the_final_page() {
    let ((), report) = run_async_under_lab(0xc01d_4104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let prepared = prepare(
            "MATCH (n) WITH n.p AS p UNWIND [p,p,p] AS item RETURN item ORDER BY item DESC LIMIT 1",
            &GqlParameters::new(),
        );
        let mut exact = None;
        for trial in 0..6 {
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let mut budget = policy(1);
            let mut external = WORK;
            if let Some((native, entries, sort)) = exact {
                budget.evaluator.max_work_units = native - u64::from(trial == 3);
                budget.evaluator.max_scratch_entries = entries - u64::from(trial == 4);
                external = sort - u64::from(trial == 5);
            }
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    budget,
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    if trial == 2 { 8 } else { 9 },
                    external,
                )
                .await;
            if trial < 2 {
                let (spool, work) = result.unwrap();
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    vec![vec![scalar(13)]]
                );
                let stats = spool.evaluator_stats();
                let measured = (stats.work_units, stats.scratch_entries, work);
                if let Some(prior) = exact {
                    assert_eq!(prior, measured);
                }
                exact = Some(measured);
            } else {
                let error = result.unwrap_err();
                match trial {
                    2 => assert!(
                        matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Rows(_))),
                        "{error:?}"
                    ),
                    3 | 4 => assert!(
                        matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                        if matches!(**inner, GqlQueryError::Evaluator(_))),
                        "{error:?}"
                    ),
                    5 => assert!(
                        matches!(error, NativeSpoolError::SortWorkLimit { .. }),
                        "{error:?}"
                    ),
                    _ => unreachable!(),
                }
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn one_large_expansion_exceeds_scratch_capacity_without_retaining_emitted_rows() {
    const ITEMS: usize = 128;
    let ((), report) = run_async_under_lab(0xc01d_4105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        const { assert!(ITEMS * PAYLOAD_BYTES > SCRATCH_BYTES) };
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let payload = GraphValue::Scalar(
            CanonicalScalar::ucs_basic_text(&"x".repeat(PAYLOAD_BYTES)).unwrap(),
        );
        for shape in 0..5 {
            let graph_backed = shape == 0;
            let binary = shape == 2;
            let grouped = shape == 3;
            let cross = shape == 4;
            let has_graph = graph_backed || cross;
            let mut params = GqlParameters::new()
                .with_list("items", (0..ITEMS as i64).map(scalar).collect())
                .unwrap();
            let text = if graph_backed {
                "MATCH (n) WHERE n.p=1 WITH n.q AS payload UNWIND $items AS item RETURN payload,item ORDER BY item DESC"
            } else {
                params = params
                    .with_map("document", vec![("payload".into(), payload.clone())])
                    .unwrap();
                if binary {
                    "(WITH $document.payload AS payload UNWIND $items AS item RETURN payload,item) UNION ALL (WITH $document.payload AS payload UNWIND $items AS item RETURN payload,item) ORDER BY item DESC"
                } else if grouped {
                    "WITH $document.payload AS payload UNWIND $items AS item WITH payload,item,COUNT(*) AS copies WHERE copies=1 RETURN payload,item ORDER BY item DESC"
                } else if cross {
                    "UNWIND [$document.payload] AS payload UNWIND $items AS item MATCH (n) RETURN payload,item,n.p AS matched ORDER BY item DESC,matched"
                } else {
                    "WITH $document.payload AS payload UNWIND $items AS item RETURN payload,item ORDER BY item DESC"
                }
            };
            let prepared = prepare(text, &params);
            drop(params);
            let buffer = view.buffer_stats();
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let copies = if cross {
                3
            } else if binary {
                2
            } else {
                1
            };
            let output_rows = ITEMS as u64 * copies;
            let budget = if has_graph {
                policy(output_rows)
            } else {
                GqlQueryPolicy::new(0, output_rows, WORK, WORK)
            };
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    budget,
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    output_rows,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.row_count(), output_rows);
            assert!(
                scratch.stats().reserved_bytes
                    + destination.stats().reserved_bytes
                    + auxiliary.stats().reserved_bytes
                    > SCRATCH_BYTES as u64
            );
            assert_eq!(
                spool.kind(),
                if binary || cross {
                    ScanKind::Set
                } else if graph_backed {
                    ScanKind::Vertex
                } else {
                    ScanKind::Values
                }
            );
            assert_eq!(
                spool.row_stats().snapshot_records,
                if has_graph { 3 } else { 0 }
            );
            if !has_graph {
                assert_eq!(view.buffer_stats(), buffer);
            }
            let mut reader = spool.reader(&mut destination);
            for value in (0..ITEMS as i64).rev() {
                for copy in 0..copies {
                    let bytes = reader.next_row(&cx).await.unwrap().unwrap();
                    let row = GraphValueRow::decode_canonical(bytes.as_ref()).unwrap();
                    let mut expected = vec![payload.clone(), scalar(value)];
                    if cross {
                        expected.push(scalar([1, 2, 13][copy as usize]));
                    }
                    assert_eq!(row.values(), expected, "{text}");
                }
            }
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_suspended_expansion_output_releases_all_row_and_cursor_reservations() {
    let ((), report) = run_async_under_lab(0xc01d_4106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        // The marker exists only in the UNWIND instruction, never in source
        // or WITH rows. Repeating it across several short pages guarantees
        // one complete eight-byte witness reaches an append. A second element
        // remains owned by the cursor during the first output's suspension.
        let text = format!(
            "MATCH (n) WHERE n.p=1 WITH n.q AS payload UNWIND ['{}','remaining-element'] AS item RETURN payload,item",
            UNWIND_MARKER.repeat(16),
        );
        let prepared = prepare(&text, &GqlParameters::new());
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        let (mut auxiliary, _) = file(&cx, &spill_pool).await;
        a.0.lock().unwrap().suspend_on = Some(text_witness(UNWIND_MARKER));
        b.0.lock().unwrap().suspend_on = Some(text_witness(UNWIND_MARKER));
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            policy(2),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            1,
            512,
            1,
            512,
            63,
            32 * 1024,
            3,
            WORK,
        );
        let blocked = poll_fn(|task| {
            let result = future.as_mut().poll(task);
            let suspended = [a.0.lock().unwrap().suspended, b.0.lock().unwrap().suspended];
            if let Some(index) = suspended.iter().position(|&seen| seen) {
                assert!(result.is_pending());
                assert_eq!(suspended.iter().filter(|&&seen| seen).count(), 1);
                Poll::Ready(index)
            } else {
                assert!(
                    result.is_pending(),
                    "UNWIND output append must be reached: {result:?}"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > PAYLOAD_BYTES * 2);
        drop(future);
        assert!([&scratch, &destination][blocked].is_poisoned());
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_binary_execution_during_right_child_or_merge_releases_both_sides() {
    const LEFT: &str = BINARY_LEFT_MARKER;
    const RIGHT: &str = BINARY_RIGHT_MARKER;
    let ((), report) = run_async_under_lab(0xc01d_4305, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for during_merge in [false, true] {
            let text = if during_merge {
                format!("RETURN '{LEFT}' AS value UNION ALL RETURN '{RIGHT}' AS value")
            } else {
                format!(
                    "MATCH (n) WHERE n.p=1 RETURN n.q AS value UNION ALL RETURN '{}' AS value",
                    RIGHT.repeat(16)
                )
            };
            let prepared = prepare(&text, &GqlParameters::new());
            let (mut scratch, a) = file(&cx, &spill_pool).await;
            let (mut destination, b) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            for backing in [&a, &b] {
                let mut state = backing.0.lock().unwrap();
                state.suspend_on = Some(text_witness(RIGHT));
                // No child contains BOTH witnesses. The first physical page
                // containing both can only be the binary merge output; no
                // fragile sorter pass or write-occurrence count is assumed.
                state.suspend_also_on = during_merge.then_some(text_witness(LEFT));
            }
            let mut future = prepared.spool_in_view(
                &mut view,
                &cx,
                if during_merge {
                    GqlQueryPolicy::new(0, 2, WORK, WORK)
                } else {
                    policy(2)
                },
                &mut scratch,
                &mut destination,
                &mut auxiliary,
                1,
                512,
                1,
                512,
                if during_merge { 1024 } else { 63 },
                32 * 1024,
                3,
                WORK,
            );
            let blocked = poll_fn(|task| {
                let result = future.as_mut().poll(task);
                let suspended = [a.0.lock().unwrap().suspended, b.0.lock().unwrap().suspended];
                if let Some(index) = suspended.iter().position(|&seen| seen) {
                    assert!(result.is_pending());
                    assert_eq!(suspended.iter().filter(|&&seen| seen).count(), 1);
                    Poll::Ready(index)
                } else {
                    assert!(
                        result.is_pending(),
                        "binary suspension must be reached: {text}: {result:?}"
                    );
                    Poll::Pending
                }
            })
            .await;
            assert!(spill_pool.used() > 0);
            drop(future);
            assert!([&scratch, &destination][blocked].is_poisoned());
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_set_any_domains_preserve_literal_graph_and_permuted_nested_values() {
    let ((), report) = run_async_under_lab(0xc01d_4401, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, records, expected) in [
            (
                "RETURN 2 AS value UNION ALL UNWIND [1,2,NULL] AS value RETURN value",
                0,
                vec![
                    vec![null()],
                    vec![scalar(1)],
                    vec![scalar(2)],
                    vec![scalar(2)],
                ],
            ),
            (
                "MATCH (n) RETURN n.p AS value INTERSECT ALL UNWIND [2,13,13] AS value RETURN value",
                3,
                vec![vec![scalar(2)], vec![scalar(13)]],
            ),
            (
                "RETURN 1 AS a,[2] AS b UNION ALL UNWIND [3] AS x RETURN [4] AS b,x AS a",
                0,
                vec![
                    vec![
                        scalar(1),
                        GraphValue::List(vec![scalar(2)].into_boxed_slice()),
                    ],
                    vec![
                        scalar(3),
                        GraphValue::List(vec![scalar(4)].into_boxed_slice()),
                    ],
                ],
            ),
        ] {
            // The complete native schema widens only the open column. Neither
            // operand needs a conversion expression to pass type admission.
            let prepared = prepare(text, &GqlParameters::new());
            let before = view.buffer_stats();
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(records, expected.len() as u64, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    100,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), ScanKind::Set, "{text}");
            assert_eq!(spool.snapshot_seq(), CommitSeq(2));
            assert_eq!(spool.row_stats().snapshot_records, records, "{text}");
            assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            if records == 0 {
                assert_eq!(view.buffer_stats(), before, "{text}: graph-free operands");
            }
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_nullable_collection_parameters_rebind_without_losing_domains() {
    let ((), report) = run_async_under_lab(0xc01d_4402, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, name, initial, kind, expected) in [
            (
                "WITH $items AS items UNWIND items AS item RETURN item",
                "items",
                GqlParameters::new()
                    .with_list("items", vec![scalar(7)])
                    .unwrap(),
                ScanKind::Values,
                Vec::new(),
            ),
            (
                "RETURN $payload AS value UNION ALL RETURN [3] AS value",
                "payload",
                GqlParameters::new()
                    .with_map("payload", vec![("n".into(), scalar(7))])
                    .unwrap(),
                ScanKind::Set,
                vec![
                    vec![null()],
                    vec![GraphValue::List(vec![scalar(3)].into_boxed_slice())],
                ],
            ),
        ] {
            // Preparation records the collection contract from a nonnull
            // argument. Rebinding NULL must preserve that contract through
            // WITH/UNWIND and when combining Any with a concrete List set column.
            let template = PreparedNativeRead::prepare(text, &initial, resolve)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            drop(initial);
            let params = GqlParameters::new().with_null(name).unwrap();
            let prepared = template
                .prepare_buffered_order(&params)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            let before = view.buffer_stats();
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, expected.len() as u64, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    100,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), kind, "{text}");
            assert_eq!(spool.snapshot_seq(), CommitSeq(2));
            assert_eq!(spool.row_stats().snapshot_records, 0, "{text}");
            assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            assert_eq!(view.buffer_stats(), before, "{text}: graph-free parameters");
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_intermediate_groups_feed_complete_relational_stages() {
    let ((), report) = run_async_under_lab(0xc01d_4501, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, cut, records, kind, expected) in [
            (
                "UNWIND [3,1,3,2,2,2,NULL] AS x WITH x,COUNT(*) AS c WHERE c>1 RETURN x,c ORDER BY c DESC,x",
                2,
                0,
                ScanKind::Values,
                vec![vec![scalar(2), scalar(3)], vec![scalar(3), scalar(2)]],
            ),
            (
                "UNWIND [3,1,3,2] AS x WITH DISTINCT x ORDER BY x DESC SKIP 1 LIMIT 1 WITH COUNT(*) AS c,SUM(x) AS total RETURN c,total",
                2,
                0,
                ScanKind::Values,
                vec![vec![scalar(1), scalar(2)]],
            ),
            (
                "UNWIND [3,1,3,NULL] AS x WITH COUNT(DISTINCT x) AS c,SUM(DISTINCT x) AS total,MIN(x) AS lo,MAX(x) AS hi UNWIND [lo,hi,total] AS value RETURN value,c ORDER BY value DESC",
                2,
                0,
                ScanKind::Values,
                vec![
                    vec![scalar(4), scalar(2)],
                    vec![scalar(3), scalar(2)],
                    vec![scalar(1), scalar(2)],
                ],
            ),
            (
                "UNWIND [1,1,2,3,3] AS x WITH x,COUNT(*) AS c WITH c,COUNT(*) AS groups WITH SUM(groups) AS total RETURN total",
                2,
                0,
                ScanKind::Values,
                vec![vec![scalar(3)]],
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 WITH n.p%2 AS bucket,COUNT(*) AS c,SUM(n.p) AS total RETURN bucket,c,total ORDER BY bucket",
                1,
                3,
                ScanKind::Vertex,
                vec![
                    vec![scalar(0), scalar(1), scalar(2)],
                    vec![scalar(1), scalar(2), scalar(4)],
                ],
            ),
            (
                "MATCH (n) WITH n.p%2 AS bucket,COUNT(*) AS c,SUM(n.p) AS total ORDER BY total DESC LIMIT 1 WHERE c>1 RETURN bucket,total",
                2,
                3,
                ScanKind::Vertex,
                vec![vec![scalar(1), scalar(14)]],
            ),
            (
                "UNWIND [1,1,2] AS x WITH x,COUNT(*) AS c RETURN x,c UNION ALL (UNWIND [1,3,3] AS x WITH x,COUNT(*) AS c RETURN c,x) ORDER BY x,c",
                2,
                0,
                ScanKind::Set,
                vec![
                    vec![scalar(1), scalar(1)],
                    vec![scalar(1), scalar(2)],
                    vec![scalar(2), scalar(1)],
                    vec![scalar(3), scalar(2)],
                ],
            ),
            (
                "UNWIND [] AS x WITH COUNT(*) AS c,SUM(x) AS total,MIN(x) AS lo RETURN c,total,lo",
                2,
                0,
                ScanKind::Values,
                vec![vec![scalar(0), null(), null()]],
            ),
            (
                "UNWIND [] AS x WITH x,COUNT(*) AS c RETURN x,c",
                2,
                0,
                ScanKind::Values,
                vec![],
            ),
            (
                "UNWIND [NULL,NULL,1] AS x WITH x,COUNT(x) AS c,COUNT(*) AS occurrences RETURN x,c,occurrences ORDER BY x",
                2,
                0,
                ScanKind::Values,
                vec![
                    vec![scalar(1), scalar(1), scalar(1)],
                    vec![null(), scalar(0), scalar(2)],
                ],
            ),
            (
                "UNWIND [1.0,3.0,NULL] AS x WITH AVG(x) AS mean RETURN mean",
                2,
                0,
                ScanKind::Values,
                vec![vec![GraphValue::Scalar(CanonicalScalar::Float(
                    CanonicalF64::new(2.0),
                ))]],
            ),
            // An integer AVG reaches the row as the binary64 nearest its
            // exact mean, 7/3 (fgdb-y255t).
            (
                "UNWIND [1,3,3,NULL] AS x WITH AVG(x) AS mean RETURN mean",
                2,
                0,
                ScanKind::Values,
                vec![vec![GraphValue::Scalar(CanonicalScalar::Float(
                    CanonicalF64::new(7.0 / 3.0),
                ))]],
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let before = view.buffer_stats();
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(records, expected.len() as u64, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    16,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), kind, "{text}");
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut), "{text}");
            assert_eq!(spool.row_stats().snapshot_records, records, "{text}");
            assert_eq!(
                spool.row_stats().result_rows,
                expected.len() as u64,
                "{text}"
            );
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            if records == 0 {
                assert_eq!(view.buffer_stats(), before, "{text}: graph-free grouping");
            }
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_group_conversion_finishes_before_downstream_filters_expressions_and_pages() {
    let ((), report) = run_async_under_lab(0xc01d_4502, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let before = view.buffer_stats();
        for (text, kind) in [
            (
                "UNWIND [9223372036854775807,1] AS x WITH SUM(x) AS total RETURN total LIMIT 0",
                GraphIntegerErrorKind::Overflow,
            ),
            (
                "UNWIND [[0,1],[1,9223372036854775807],[1,1]] AS pair WITH pair[0] AS bucket,SUM(pair[1]) AS total RETURN 1/0 AS value LIMIT 0",
                GraphIntegerErrorKind::Overflow,
            ),
            (
                "UNWIND [9223372036854775807,1] AS x WITH SUM(x) AS total WHERE false RETURN total",
                GraphIntegerErrorKind::Overflow,
            ),
            (
                "(UNWIND [] AS x RETURN x+0 AS value) INTERSECT ALL (UNWIND [9223372036854775807,1] AS x WITH SUM(x) AS total RETURN total AS value) LIMIT 0",
                GraphIntegerErrorKind::Overflow,
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, 0, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    16,
                    WORK,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error,
                    NativeSpoolError::SetExecution(GraphSetExecutionError::Aggregate(ref inner))
                    if matches!(**inner, GraphAggregateError::OutputExpression { error, .. }
                    if error.kind == kind)),
                "{text}: {error:?}"
            );
            assert_eq!(view.buffer_stats(), before, "{text}");
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        let prepared = prepare(
            "UNWIND [1,2,3] AS x WITH x,COUNT(*) AS c RETURN x LIMIT 0",
            &GqlParameters::new(),
        );
        let (mut scratch, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        let (mut auxiliary, _) = file(&cx, &spill_pool).await;
        let error = prepared
            .spool_in_view(
                &mut view,
                &cx,
                GqlQueryPolicy::new(0, 0, WORK, WORK),
                &mut scratch,
                &mut destination,
                &mut auxiliary,
                1,
                1,
                1,
                512,
                1024,
                32 * 1024,
                3,
                WORK,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::Aggregate(ref inner)
                if matches!(**inner, NativeAggregateSpoolError::PartitionLimit { required, limit: 1 }
                if required > 1)),
            "{error:?}"
        );
        assert_eq!(view.buffer_stats(), before);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_suspended_intermediate_group_conversion_releases_all_three_files() {
    let ((), report) = run_async_under_lab(0xc01d_4503, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let before = view.buffer_stats();
        let prepared = prepare(
            "UNWIND [101,103,107] AS x WITH SUM(x) AS total,COUNT(*) AS copies RETURN total,copies",
            &GqlParameters::new(),
        );
        // Only the ordinary converted group row has this exact frame. Source
        // rows contain one of 101/103/107, and the exact aggregate spool stores
        // tagged wide summaries. This does not depend on sorter pass counts.
        let marker = GraphValueRow::from_owned_values(vec![scalar(311), scalar(3)])
            .canonical_bytes()
            .unwrap();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        let (mut auxiliary, c) = file(&cx, &spill_pool).await;
        for backing in [&a, &b, &c] {
            backing.0.lock().unwrap().suspend_row = Some(marker.clone());
        }
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            GqlQueryPolicy::new(0, 1, WORK, WORK),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            1,
            512,
            1,
            512,
            1024,
            32 * 1024,
            3,
            WORK,
        );
        let blocked = poll_fn(|task| {
            let result = future.as_mut().poll(task);
            let suspended = [
                a.0.lock().unwrap().suspended,
                b.0.lock().unwrap().suspended,
                c.0.lock().unwrap().suspended,
            ];
            if let Some(index) = suspended.iter().position(|&seen| seen) {
                assert!(result.is_pending());
                assert_eq!(suspended.iter().filter(|&&seen| seen).count(), 1);
                Poll::Ready(index)
            } else {
                assert!(
                    result.is_pending(),
                    "converted aggregate append must be reached"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > 0);
        drop(future);
        assert!([&scratch, &destination, &auxiliary][blocked].is_poisoned());
        assert_eq!(view.buffer_stats(), before);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_leading_unwind_match_preserves_correlations_occurrences_and_composition() {
    let ((), report) = run_async_under_lab(0xc01d_4601, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (text, cut, records, identities, expected) in [
            (
                "UNWIND [13,1,13,99,NULL] AS wanted MATCH (n {p:wanted}) RETURN wanted,n",
                2,
                3,
                false,
                vec![
                    vec![scalar(13), GraphValue::Vertex(VId(3))],
                    vec![scalar(1), GraphValue::Vertex(VId(1))],
                    vec![scalar(13), GraphValue::Vertex(VId(3))],
                ],
            ),
            (
                "UNWIND [2,2,99,NULL] AS wanted MATCH (n) WHERE n.p=wanted RETURN wanted,n.p AS found",
                2,
                3,
                false,
                vec![vec![scalar(2), scalar(2)], vec![scalar(2), scalar(2)]],
            ),
            (
                "UNWIND [3,1,3] AS wanted MATCH (n {p:wanted}) FOR SYSTEM_TIME AS OF SEQ 1 RETURN wanted,n",
                1,
                3,
                false,
                vec![
                    vec![scalar(3), GraphValue::Vertex(VId(3))],
                    vec![scalar(1), GraphValue::Vertex(VId(1))],
                    vec![scalar(3), GraphValue::Vertex(VId(3))],
                ],
            ),
            (
                "UNWIND [1,2] AS factor MATCH (a)-[r:R]->(b) WITH r.p*factor AS value RETURN value ORDER BY value",
                2,
                3,
                false,
                [7, 8, 9, 14, 16, 18]
                    .into_iter()
                    .map(|n| vec![scalar(n)])
                    .collect(),
            ),
            (
                "UNWIND $ids AS n MATCH (n)-[r:R]->(m) WITH n,r.p AS weight RETURN n,weight ORDER BY weight,n",
                2,
                3,
                true,
                vec![
                    vec![GraphValue::Vertex(VId(1)), scalar(7)],
                    vec![GraphValue::Vertex(VId(1)), scalar(7)],
                    vec![GraphValue::Vertex(VId(1)), scalar(8)],
                    vec![GraphValue::Vertex(VId(1)), scalar(8)],
                    vec![GraphValue::Vertex(VId(2)), scalar(9)],
                ],
            ),
            (
                "UNWIND [[1,NULL],{nested:[2]}] AS value MATCH (n) WHERE n.p=2 RETURN value,n",
                2,
                3,
                false,
                vec![
                    vec![
                        GraphValue::List(vec![scalar(1), null()].into_boxed_slice()),
                        GraphValue::Vertex(VId(2)),
                    ],
                    vec![
                        GraphValue::map(vec![(
                            "nested".into(),
                            GraphValue::List(vec![scalar(2)].into_boxed_slice()),
                        )])
                        .unwrap(),
                        GraphValue::Vertex(VId(2)),
                    ],
                ],
            ),
            (
                "UNWIND [1,1,2] AS wanted MATCH (n {p:wanted}) WITH n.p AS p,COUNT(*) AS copies WHERE copies>1 UNWIND [p,copies] AS value RETURN value ORDER BY value DESC",
                2,
                3,
                false,
                vec![vec![scalar(2)], vec![scalar(1)]],
            ),
            (
                "(UNWIND [1,1,2] AS wanted MATCH (n {p:wanted}) WITH n.p AS value,COUNT(*) AS copies RETURN value,copies) UNION ALL (UNWIND [13] AS wanted MATCH (n {p:wanted}) WITH n.p AS value,COUNT(*) AS copies RETURN copies,value) ORDER BY value",
                2,
                6,
                false,
                vec![
                    vec![scalar(1), scalar(2)],
                    vec![scalar(2), scalar(1)],
                    vec![scalar(13), scalar(1)],
                ],
            ),
            (
                "(UNWIND [13,1,13] AS wanted MATCH (n {p:wanted}) RETURN wanted AS value) EXCEPT ALL RETURN 13 AS value",
                2,
                3,
                false,
                vec![vec![scalar(1)], vec![scalar(13)]],
            ),
            (
                "UNWIND [] AS item MATCH (n) RETURN item,n LIMIT 0",
                2,
                3,
                false,
                vec![],
            ),
        ] {
            let params = if identities {
                GqlParameters::new()
                    .with_list(
                        "ids",
                        vec![
                            GraphValue::Vertex(VId(2)),
                            null(),
                            GraphValue::Vertex(VId(1)),
                            GraphValue::Vertex(VId(1)),
                        ],
                    )
                    .unwrap()
            } else {
                GqlParameters::new()
            };
            let prepared = prepare(text, &params);
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(records, expected.len() as u64, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    32,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            assert_eq!(spool.kind(), ScanKind::Set, "{text}");
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut), "{text}");
            assert_eq!(spool.row_stats().snapshot_records, records, "{text}");
            assert_eq!(
                spool.row_stats().result_rows,
                expected.len() as u64,
                "{text}"
            );
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_cross_children_and_full_product_finish_before_empty_parent_pages() {
    let ((), report) = run_async_under_lab(0xc01d_4602, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        for (case, text, records, private_rows) in [
            (
                0,
                "UNWIND [[1],2] AS items UNWIND items AS item MATCH (n) WHERE 10/(n.p-2)>0 RETURN item,n LIMIT 0",
                3,
                32,
            ),
            (
                1,
                "UNWIND [] AS item MATCH (n) WHERE 10/(n.p-2)>0 RETURN item,n LIMIT 0",
                3,
                32,
            ),
            (
                2,
                "UNWIND [] AS item MATCH (n) RETURN item,n LIMIT 0",
                0,
                32,
            ),
            // Seeded correlation (fgdb-2acff) reads only the vertices whose p
            // was unwound: three UNWIND rows and three seeded rows, not the
            // nine-row product. LIMIT 0 still cannot hide their admission.
            (
                3,
                "UNWIND [1,2,13] AS wanted MATCH (n {p:wanted}) RETURN n LIMIT 0",
                3,
                5,
            ),
        ] {
            let prepared = prepare(text, &GqlParameters::new());
            let before = view.buffer_stats();
            let (mut scratch, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (mut auxiliary, _) = file(&cx, &spill_pool).await;
            let error = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(records, 0, WORK, WORK),
                    &mut scratch,
                    &mut destination,
                    &mut auxiliary,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    32 * 1024,
                    private_rows,
                    WORK,
                )
                .await
                .unwrap_err();
            match case {
                0 => {
                    assert!(
                        matches!(error, NativeSpoolError::SetExecution(
                        GraphSetExecutionError::Projection { row: 1, error, .. }
                    ) if error.kind == GraphIntegerErrorKind::IncompatibleOperands),
                        "{text}: {error:?}"
                    );
                    assert_eq!(
                        view.buffer_stats(),
                        before,
                        "the complete left child fails first"
                    );
                }
                1 => assert!(
                    matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                    if matches!(**inner, GqlQueryError::Data(error)
                    if error.kind == GraphIntegerErrorKind::DivisionByZero)),
                    "{text}: {error:?}"
                ),
                2 => assert!(
                    matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                    if matches!(**inner, GqlQueryError::Rows(ref error)
                    if error.dimension == GqlBudgetDimension::SnapshotRecords
                    && error.limit == 0 && error.observed == 1)),
                    "{text}: {error:?}"
                ),
                3 => assert!(
                    matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                    if matches!(**inner, GqlQueryError::Rows(ref error)
                    if error.dimension == GqlBudgetDimension::ResultRows
                    && error.limit == 5 && error.observed == 6)),
                    "{text}: {error:?}"
                ),
                _ => unreachable!(),
            }
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_suspended_cross_pair_append_releases_both_readers_and_all_row_guards() {
    let ((), report) = run_async_under_lab(0xc01d_4603, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let prepared = prepare(
            "UNWIND [101,103] AS marker MATCH (n) RETURN marker,n",
            &GqlParameters::new(),
        );
        // Neither one-column child contains this frame. Its first appearance
        // is the native product append, with both input cursors and the copied
        // pair still reserved across the awaited write.
        let marker =
            GraphValueRow::from_owned_values(vec![scalar(101), GraphValue::Vertex(VId(1))])
                .canonical_bytes()
                .unwrap();
        // One complete framed row fills a page, so the first pair suspends
        // inside write_row rather than waiting until writer.finish.
        let page_bytes = 8 + marker.len();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        let (mut auxiliary, c) = file(&cx, &spill_pool).await;
        for backing in [&a, &b, &c] {
            backing.0.lock().unwrap().suspend_row = Some(marker.clone());
        }
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            GqlQueryPolicy::new(3, 6, WORK, WORK),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            1,
            512,
            1,
            512,
            page_bytes,
            32 * 1024,
            6,
            WORK,
        );
        let blocked = poll_fn(|task| {
            let result = future.as_mut().poll(task);
            let suspended = [
                a.0.lock().unwrap().suspended,
                b.0.lock().unwrap().suspended,
                c.0.lock().unwrap().suspended,
            ];
            if let Some(index) = suspended.iter().position(|&seen| seen) {
                assert!(result.is_pending());
                assert_eq!(suspended.iter().filter(|&&seen| seen).count(), 1);
                Poll::Ready(index)
            } else {
                assert!(result.is_pending(), "the cross pair append must be reached");
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > 0);
        drop(future);
        assert!([&scratch, &destination, &auxiliary][blocked].is_poisoned());
        assert!(view.buffer_stats().bypasses > 0);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
