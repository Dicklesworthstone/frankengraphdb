//! Cold fixed-hop and bounded-path queries with completed relational stages
//! into the external reducer. Literal summaries below come from the fixture's
//! finite relation, not the join cursor or the external stage evaluator.

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, EmbeddedReadView,
    MemVfs, MemoryPool, NativeAggregateSpool, NativeAggregateSpoolError, NativeSpoolError,
    PreparedBufferedAggregate, PreparedNativeRead, QueryResult, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateTextSlot,
    GraphAggregateValue, GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalF64, CanonicalList, CanonicalPropertyValue, CanonicalScalar, CommitCx, CommitSeq,
    DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::future::poll_fn;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const WORK: u64 = 100_000_000;
const MATCH: &str = "MATCH REPEATABLE ELEMENTS (a)-[r:R]->(b)-[s:R]->(c)";
const TOTAL: &str = "RETURN COUNT(*) AS rows, SUM(r.p+s.p) AS total";
type LogicalRows = Vec<(Vec<GraphValue>, Vec<GraphAggregateValue>)>;
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xc1; 32],
        DatabaseSecurityNamespaceId([0xc2; 32]),
        [0xc3; 32],
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
fn prepare(text: &str) -> PreparedBufferedAggregate {
    let params = GqlParameters::new();
    PreparedNativeRead::prepare(text, &params, resolve)
        .unwrap()
        .prepare_buffered_aggregate(&params)
        .unwrap()
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, WORK, WORK)
}
async fn fixture(cx: &CommitCx) -> MemVfs {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut first = WriteBatch::new(RelationId(1));
    for id in 1..=4 {
        first.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                (
                    PropertyKeyId(2),
                    CanonicalScalar::ucs_basic_text(&format!("{id}:{}", "payload".repeat(80)))
                        .unwrap(),
                ),
            ],
        );
    }
    for (id, from, to, value) in [
        (1, 1, 2, 7),
        (2, 1, 2, 8),
        (3, 2, 2, 9),
        (4, 2, 3, 10),
        (5, 3, 4, 11),
    ] {
        first.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, first).await.unwrap(), CommitSeq(1));
    let mut second = WriteBatch::new(RelationId(1));
    second.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(17)));
    second.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(13)));
    assert_eq!(db.write(cx, second).await.unwrap(), CommitSeq(2));
    let mut third = WriteBatch::new(RelationId(1));
    third.delete_vertex(VId(4));
    assert_eq!(db.write(cx, third).await.unwrap(), CommitSeq(3));
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

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    writes: usize,
    pending: bool,
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
        state.writes += 1;
        if state.pending {
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
            max_runs: 2048,
            max_run_bytes: 4_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
async fn contents(
    spool: &NativeAggregateSpool,
    file: &mut SpillFile<File>,
    cx: &QueryCx,
) -> LogicalRows {
    let mut reader = spool.reader(file, None);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push((row.keys().to_vec(), row.values().to_vec()));
    }
    rows
}
fn summary(count: u64, sum: i128) -> LogicalRows {
    vec![(
        vec![],
        vec![
            GraphAggregateValue::Count(count),
            GraphAggregateValue::Integer(sum),
        ],
    )]
}

fn collected(values: impl IntoIterator<Item = GraphValue>) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::List(values.into_iter().collect()))
}

fn collected_ints(values: &[i64]) -> GraphAggregateValue {
    collected(
        values
            .iter()
            .map(|value| GraphValue::Scalar(CanonicalScalar::Int(*value))),
    )
}

async fn assert_bounded_summary(
    commit: &CommitCx,
    cx: &QueryCx,
    vfs: &MemVfs,
    resident: &EmbeddedReadView,
    text: &str,
    cut: u64,
    expected: LogicalRows,
) {
    let params = GqlParameters::new();
    let native = PreparedNativeRead::prepare(text, &params, resolve)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    let prepared = native
        .prepare_buffered_aggregate(&params)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
    let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
    let mut cold = view(commit, vfs, &source_pool).await;
    let baseline = source_pool.used();
    let (mut source, _) = file(cx, &spill_pool).await;
    let (mut partition, _) = file(cx, &spill_pool).await;
    let (mut destination, _) = file(cx, &spill_pool).await;
    let allowance = policy(expected.len() as u64);
    let (spool, _) = prepared
        .spool_in_view(
            &mut cold,
            cx,
            allowance,
            &mut source,
            &mut partition,
            &mut destination,
            1,
            512,
            1,
            512,
            1024,
            16 * 1024,
            1000,
            WORK,
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    assert_eq!(spool.snapshot_seq(), CommitSeq(cut), "{text}");
    assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
    assert_eq!(spool.row_stats().result_rows, spool.row_count(), "{text}");
    let actual = contents(&spool, &mut destination, cx).await;
    assert_eq!(actual, expected, "{text}");
    let (native_result, native_cut) = native
        .execute_in_view_with_snapshot(resident, cx, &params, allowance)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
    let QueryResult::Rows { columns, rows } = native_result else {
        panic!("an aggregate read must return rows: {text}")
    };
    let external_rows: Vec<Vec<_>> = actual
        .iter()
        .map(|(keys, values)| {
            spool
                .output_slots()
                .iter()
                .map(|slot| match slot {
                    GraphAggregateTextSlot::GroupKey(index) => {
                        GraphAggregateValue::Value(keys[*index].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(index) => values[*index].clone(),
                })
                .collect()
        })
        .collect();
    assert_eq!(native_cut, CommitSeq(cut), "{text}");
    assert_eq!(spool.columns(), columns, "{text}");
    assert_eq!(external_rows, rows, "{text}");
    assert!(cold.buffer_stats().bypasses > 0, "{text}");
    assert_eq!(source_pool.used(), baseline, "{text}");
    assert_eq!(spill_pool.used(), 0, "{text}");
    drop(cold);
    assert_eq!(source_pool.used(), 0, "{text}");
}

#[test]
fn cold_bounded_path_modes_reduce_exact_bags_at_every_retained_cut() {
    let ((), report) = run_async_under_lab(0xc01d_a006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let writer = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let resident = writer.read_session().unwrap();
        drop(writer);

        // Endpoint multiplicities at cut 1, including one zero-hop row per
        // vertex, are WALK [1,10,8,6], TRAIL [1,6,7,6], ACYCLIC [1,3,4,5]
        // and SIMPLE [1,4,4,5]. At cut 2 only vertex 3's value changes; at
        // cut 3 every row ending at retired vertex 4 disappears. Repeated
        // loop use distinguishes WALK from TRAIL, while the one closing
        // self-loop distinguishes SIMPLE from ACYCLIC. Parallel edges keep
        // distinct occurrences in every mode.
        for (mode, counts, sums) in [
            ("WALK", [25, 25, 19], [69, 149, 125]),
            ("TRAIL", [20, 20, 14], [58, 128, 104]),
            ("ACYCLIC", [13, 13, 8], [39, 79, 59]),
            ("SIMPLE", [14, 14, 9], [41, 81, 61]),
        ] {
            for (at, (count, sum)) in counts.into_iter().zip(sums).enumerate() {
                let cut = at as u64 + 1;
                let text = format!(
                    "MATCH REPEATABLE ELEMENTS {mode} (a)-[:R*0..3]->(b) \
                     FOR SYSTEM_TIME AS OF SEQ {cut} \
                     RETURN COUNT(*) AS paths,COUNT(DISTINCT b) AS endpoints, \
                     SUM(b.p) AS total,MIN(b.p) AS least,MAX(b.p) AS greatest"
                );
                assert_bounded_summary(
                    &commit,
                    &cx,
                    &vfs,
                    &resident,
                    &text,
                    cut,
                    vec![(
                        vec![],
                        vec![
                            GraphAggregateValue::Count(count),
                            GraphAggregateValue::Count(if cut == 3 { 3 } else { 4 }),
                            GraphAggregateValue::Integer(sum),
                            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(1))),
                            GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
                                if cut == 1 { 4 } else { 13 },
                            ))),
                        ],
                    )],
                )
                .await;
            }
        }

        // Both surviving nontrivial groups exceed the resident capacity of
        // one; HAVING excludes the root-only endpoint before final ordering.
        for (mode, groups) in [
            ("WALK", [(2, 10, 20), (13, 8, 104)]),
            ("TRAIL", [(13, 7, 91), (2, 6, 12)]),
            ("ACYCLIC", [(13, 4, 52), (2, 3, 6)]),
            ("SIMPLE", [(2, 4, 8), (13, 4, 52)]),
        ] {
            let text = format!(
                "MATCH REPEATABLE ELEMENTS {mode} (a)-[:R*0..3]->(b) \
                 RETURN b.p AS endpoint,COUNT(*) AS paths,SUM(b.p) AS total \
                 GROUP BY b.p HAVING paths>1 ORDER BY paths DESC,endpoint"
            );
            let expected = groups
                .into_iter()
                .map(|(endpoint, count, sum)| {
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(endpoint))],
                        vec![
                            GraphAggregateValue::Count(count),
                            GraphAggregateValue::Integer(sum),
                        ],
                    )
                })
                .collect();
            assert_bounded_summary(&commit, &cx, &vfs, &resident, &text, 3, expected).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_bounded_path_summaries_keep_complete_with_ordering_distinct_and_collections() {
    let ((), report) = run_async_under_lab(0xc01d_a007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let writer = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let resident = writer.read_session().unwrap();
        drop(writer);
        let head = "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0..3]->(b)";
        // At the current cut the descending endpoint values are eight 13s,
        // ten 2s and one 1. Page seven through ten crosses the value boundary;
        // these clauses therefore cannot be pushed after final reduction.
        for (text, cut, expected) in [
            (
                format!("{head} WITH b.p AS value ORDER BY value DESC SKIP 7 LIMIT 4 RETURN COUNT(*) AS paths,SUM(value) AS total"),
                3,
                summary(4, 19),
            ),
            (
                format!("{head} WITH DISTINCT b.p AS value ORDER BY value DESC SKIP 1 LIMIT 2 RETURN COUNT(*) AS paths,SUM(value) AS total"),
                3,
                summary(2, 3),
            ),
            (
                format!("{head} WITH b.p AS value ORDER BY value DESC SKIP 7 LIMIT 4 RETURN COLLECT(value) AS items,COLLECT(DISTINCT value) AS support,COUNT(*) AS paths,SUM(value) AS total"),
                3,
                vec![(
                    vec![],
                    vec![
                        collected_ints(&[13, 2, 2, 2]),
                        collected_ints(&[13, 2]),
                        GraphAggregateValue::Count(4),
                        GraphAggregateValue::Integer(19),
                    ],
                )],
            ),
            (
                // Computed direct collections retain complete-source value
                // order [1,2,13] before negation, not argument order.
                format!("{head} RETURN COLLECT(DISTINCT -b.p) AS support"),
                3,
                vec![(vec![], vec![collected_ints(&[-1, -2, -13])])],
            ),
            (
                format!("{head} WITH b.p AS value ORDER BY value DESC SKIP 7 LIMIT 4 WITH value%2 AS bucket,value AS amount RETURN bucket,COUNT(*) AS paths,SUM(amount) AS total GROUP BY bucket ORDER BY bucket"),
                3,
                vec![
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(0))],
                        vec![GraphAggregateValue::Count(3),GraphAggregateValue::Integer(6)],
                    ),
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
                        vec![GraphAggregateValue::Count(1),GraphAggregateValue::Integer(13)],
                    ),
                ],
            ),
            (
                "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*2..2]->(b) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS paths,SUM(b.p) AS total".to_owned(),
                1,
                summary(7, 19),
            ),
            (
                format!("{head} FOR SYSTEM_TIME AS OF SEQ 1 WITH b.p AS value ORDER BY value DESC SKIP 5 LIMIT 3 RETURN COUNT(*) AS paths,SUM(value) AS total"),
                1,
                summary(3, 10),
            ),
            (
                format!("{head} WHERE a.p=1 RETURN COUNT(*) AS paths,SUM(b.p) AS total"),
                3,
                summary(11, 65),
            ),
            (
                format!("{head} WHERE a.p>100 RETURN COUNT(*) AS paths"),
                3,
                vec![(vec![], vec![GraphAggregateValue::Count(0)])],
            ),
            (
                format!("{head} RETURN COUNT(*) AS paths LIMIT 0"),
                3,
                vec![],
            ),
        ] {
            assert_bounded_summary(&commit, &cx, &vfs, &resident, &text, cut, expected).await;
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_direct_collections_preserve_native_occurrence_order_and_complete_result_clauses() {
    let ((), report) = run_async_under_lab(0xc01d_c011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(7)));
        writer.write(&commit, change).await.unwrap();
        writer.compact(&commit).await.unwrap();
        drop(writer);

        // Current identity order yields [7,2,7], not value order [2,7,7].
        // The computed path instead sorts its complete source before applying
        // negation. Literal outputs distinguish both contracts and preserve
        // the two parallel edges from 1 to 2 and the self-loop at 2.
        for (text, cut, expected) in [
            (
                "MATCH (n) RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS support,COLLECT(n) AS nodes",
                4,
                vec![(
                    vec![],
                    vec![
                        collected_ints(&[7, 2, 7]),
                        collected_ints(&[7, 2]),
                        collected([1, 2, 3].map(|id| GraphValue::Vertex(VId(id)))),
                    ],
                )],
            ),
            (
                "MATCH (n) RETURN COLLECT(-n.p) AS items,COLLECT(DISTINCT -n.p) AS support",
                4,
                vec![(
                    vec![],
                    vec![collected_ints(&[-2, -7, -7]), collected_ints(&[-2, -7])],
                )],
            ),
            (
                "MATCH (a)-[:R]->(b) RETURN COLLECT(a.p) AS items,COLLECT(DISTINCT a.p) AS support",
                4,
                vec![(
                    vec![],
                    vec![collected_ints(&[7, 7, 2, 2]), collected_ints(&[7, 2])],
                )],
            ),
            (
                "MATCH (a)<-[:R]-(b) RETURN COLLECT(a.p) AS items,COLLECT(DISTINCT a.p) AS support",
                4,
                vec![(
                    vec![],
                    vec![collected_ints(&[2, 2, 2, 7]), collected_ints(&[2, 7])],
                )],
            ),
            (
                "MATCH REPEATABLE ELEMENTS (a)-[r:R]->(b)-[s:R]->(c) RETURN COLLECT(r.p+s.p) AS items",
                4,
                vec![(vec![], vec![collected_ints(&[17, 18, 18, 19, 26, 27])])],
            ),
            (
                "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 2 RETURN COLLECT(n.p) AS items",
                2,
                vec![(vec![], vec![collected_ints(&[1, 2, 13, 4])])],
            ),
            (
                "MATCH (n) RETURN n.p AS bucket,COLLECT(n) AS nodes,COLLECT(DISTINCT n.p) AS support GROUP BY n.p ORDER BY bucket DESC",
                4,
                vec![
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(7))],
                        vec![
                            collected([1, 3].map(|id| GraphValue::Vertex(VId(id)))),
                            collected_ints(&[7]),
                        ],
                    ),
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(2))],
                        vec![
                            collected([GraphValue::Vertex(VId(2))]),
                            collected_ints(&[2]),
                        ],
                    ),
                ],
            ),
            (
                "MATCH (n) RETURN DISTINCT [x IN COLLECT(n.p) | x*2] AS items GROUP BY n.p HAVING COUNT(*)>0 ORDER BY COUNT(*) DESC SKIP 1 LIMIT 1",
                4,
                vec![(vec![], vec![collected_ints(&[4])])],
            ),
            (
                "MATCH (n) WHERE n.p<0 RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS support,COUNT(*) AS total",
                4,
                vec![(
                    vec![],
                    vec![
                        collected_ints(&[]),
                        collected_ints(&[]),
                        GraphAggregateValue::Count(0),
                    ],
                )],
            ),
            ("MATCH (n) RETURN COLLECT(n.p) AS items LIMIT 0", 4, vec![]),
        ] {
            let prepared = prepare(text);
            assert!(prepared.stage_count() >= 2, "{text}");
            assert_eq!(prepared.aggregate_count(), 1);
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(expected.len() as u64),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut), "{text}");
            assert!(
                spool
                    .columns()
                    .iter()
                    .all(|name| !name.starts_with("_fgdb_collect_order_"))
            );
            assert_eq!(spool.row_count(), expected.len() as u64);
            assert_eq!(
                contents(&spool, &mut destination, &cx).await,
                expected,
                "{text}"
            );
            assert!(view.buffer_stats().bypasses > 0, "{text}");
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_direct_collections_keep_typed_values_null_elision_and_full_width_identities() {
    let ((), report) = run_async_under_lab(0xc01d_c012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut writer =
            Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
                .await
                .unwrap();
        let integer = CanonicalScalar::Int(2);
        let float = CanonicalScalar::Float(CanonicalF64::new(2.0));
        let nested = CanonicalPropertyValue::List(
            CanonicalList::try_new(vec![integer.clone().into(), CanonicalScalar::Null.into()])
                .unwrap(),
        );
        let mut batch = WriteBatch::new(RelationId(1));
        for (id, value) in [
            (0, None),
            (1, Some(CanonicalScalar::Null.into())),
            (2, Some(float.clone().into())),
            (3, Some(integer.clone().into())),
            (4, Some(float.clone().into())),
            (5, Some(integer.clone().into())),
            (6, Some(nested.clone())),
            (u128::MAX, Some(nested)),
        ] {
            batch.create_vertex_values(
                VId(id),
                vec![],
                value
                    .into_iter()
                    .map(|value| (PropertyKeyId(1), value))
                    .collect(),
            );
        }
        writer.write(&commit, batch).await.unwrap();
        drop(writer);
        let prepared = prepare(
            "MATCH (n) RETURN COLLECT(n.p) AS items,COLLECT(DISTINCT n.p) AS support,COLLECT(n) AS nodes,COUNT(*) AS count",
        );
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
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
                16 * 1024,
                1000,
                WORK,
                None,
            )
            .await
            .unwrap();
        let integer = GraphValue::Scalar(integer);
        let float = GraphValue::Scalar(float);
        let nested = GraphValue::List(
            vec![integer.clone(), GraphValue::Scalar(CanonicalScalar::Null)].into_boxed_slice(),
        );
        assert_eq!(
            contents(&spool, &mut destination, &cx).await,
            vec![(
                vec![],
                vec![
                    collected([
                        float.clone(),
                        integer.clone(),
                        float.clone(),
                        integer.clone(),
                        nested.clone(),
                        nested.clone(),
                    ]),
                    // Canonical Int and Float are distinct domains. Repeated
                    // same-domain values and whole equal lists contribute once.
                    collected([float, integer, nested]),
                    collected(
                        [0, 1, 2, 3, 4, 5, 6, u128::MAX].map(|id| GraphValue::Vertex(VId(id)))
                    ),
                    GraphAggregateValue::Count(8),
                ]
            )],
        );
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_direct_collections_refuse_oversized_results_and_late_errors_before_limit_zero() {
    let ((), report) = run_async_under_lab(0xc01d_c013, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        for (text, max_row_bytes, diagnostic) in [
            (
                "MATCH (n) RETURN COLLECT(n.q) AS items",
                1024,
                "RowTooLarge",
            ),
            (
                "MATCH (n) RETURN COLLECT(n.q) AS items LIMIT 0",
                1024,
                "RowTooLarge",
            ),
            (
                "MATCH (n) RETURN COLLECT(1/(n.p-13)) AS items LIMIT 0",
                16 * 1024,
                "DivisionByZero",
            ),
        ] {
            let prepared = prepare(text);
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let error = prepared
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
                    max_row_bytes,
                    1000,
                    WORK,
                    None,
                )
                .await
                .expect_err("no result handle may survive an incomplete collection");
            assert!(
                format!("{error:?}").contains(diagnostic),
                "{text}: {error:?}"
            );
            assert_eq!(source_pool.used(), baseline, "{text}");
            assert_eq!(spill_pool.used(), 0, "{text}");
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_a_direct_collection_during_ordered_input_releases_source_and_spill_memory() {
    let ((), report) = run_async_under_lab(0xc01d_c014, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare("MATCH (n) RETURN COLLECT(n.q) AS items");
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let (mut source, backing) = file(&cx, &spill_pool).await;
        let (mut partition, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        {
            let mut state = backing.0.lock().unwrap();
            state.pending = true;
            state.writes = 0;
        }
        let mut future = prepared.spool_in_view(
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
            63,
            16 * 1024,
            1000,
            WORK,
            None,
        );
        poll_fn(|task| {
            let result = future.as_mut().poll(task);
            if backing.0.lock().unwrap().writes != 0 {
                assert!(result.is_pending());
                Poll::Ready(())
            } else {
                assert!(
                    result.is_pending(),
                    "collection failed before the suspended append"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(spill_pool.used() > 0);
        drop(future);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        assert!(source.is_poisoned());
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_pipeline_aggregates_preserve_every_input_page_distinct_and_computed_schema() {
    let ((), report) = run_async_under_lab(0xc01d_a101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        // At frontier 3 the vertices have values [1,2,13]. The two-hop
        // occurrences have r.p+s.p = [26,27,17,18,18,19], including the
        // repeatable self-loop. These literals are independent of execution.
        for (text, expected) in [
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) WITH r.p+s.p AS value RETURN COUNT(*) AS rows,SUM(value) AS total".to_owned(),
                summary(5, 107),
            ),
            (
                "MATCH DIFFERENT EDGES (a)-[r:R]->(b)-[s:R]->(c) WITH r.p+s.p AS value RETURN COUNT(*) AS rows,SUM(value) AS total".to_owned(),
                summary(5, 107),
            ),
            (
                "MATCH (n) WITH n.p AS value ORDER BY value DESC LIMIT 2 RETURN COUNT(*) AS rows,SUM(value) AS total".to_owned(),
                summary(2, 15),
            ),
            (
                "MATCH (n) WITH DISTINCT n.p%2 AS bucket ORDER BY bucket DESC SKIP 1 LIMIT 1 RETURN COUNT(*) AS rows,SUM(bucket) AS total".to_owned(),
                summary(1, 0),
            ),
            (
                "MATCH (n) WITH n.p AS value ORDER BY value DESC LIMIT 2 WITH value%2 AS bucket,value+1 AS amount RETURN bucket,COUNT(*) AS rows,SUM(amount) AS total GROUP BY bucket HAVING total>=3 ORDER BY total DESC".to_owned(),
                vec![
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(1))],
                        vec![GraphAggregateValue::Count(1), GraphAggregateValue::Integer(14)],
                    ),
                    (
                        vec![GraphValue::Scalar(CanonicalScalar::Int(0))],
                        vec![GraphAggregateValue::Count(1), GraphAggregateValue::Integer(3)],
                    ),
                ],
            ),
            (
                format!("{MATCH} WITH r.p+s.p AS value ORDER BY value DESC SKIP 1 LIMIT 4 RETURN COUNT(*) AS rows,SUM(value) AS total"),
                summary(4, 81),
            ),
            (
                format!("{MATCH} WITH r.p+s.p AS value ORDER BY value DESC SKIP 1 LIMIT 4 WITH DISTINCT value RETURN COUNT(*) AS rows,SUM(value+1) AS total"),
                summary(3, 66),
            ),
            (
                format!("{MATCH} WITH r.p+s.p AS value RETURN COUNT(*) AS rows,SUM(DISTINCT value) AS total"),
                summary(6, 107),
            ),
            (
                "MATCH (n) WITH n.p AS value ORDER BY value LIMIT 2 WITH 1/(value-13) AS unused RETURN COUNT(*) AS rows".to_owned(),
                vec![(vec![], vec![GraphAggregateValue::Count(2)])],
            ),
            (
                "MATCH (n) WITH n.p AS value WHERE value<0 RETURN COUNT(*) AS rows,SUM(value) AS total".to_owned(),
                vec![(
                    vec![],
                    vec![
                        GraphAggregateValue::Count(0),
                        GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                    ],
                )],
            ),
            (
                format!("{MATCH} WITH r.p+s.p AS value RETURN COUNT(*) AS rows LIMIT 0"),
                vec![],
            ),
        ] {
            let prepared = prepare(&text);
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(expected.len() as u64),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(spool.snapshot_seq(), CommitSeq(3));
            assert_eq!(spool.row_stats().result_rows, expected.len() as u64);
            assert_eq!(contents(&spool, &mut destination, &cx).await, expected, "{text}");
            assert!(view.buffer_stats().bypasses > 0, "{text}");
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_cartesian_groups_round_trip_through_ordering_into_a_final_aggregate() {
    let ((), report) = run_async_under_lab(0xc01d_a105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(concat!(
            "UNWIND [1,1,2,13] AS wanted MATCH (n {p:wanted}) ",
            "WITH n.p AS value,COUNT(*) AS copies ",
            "ORDER BY value DESC SKIP 1 LIMIT 2 ",
            "RETURN COUNT(*) AS rows,SUM(value*copies) AS total"
        ));
        // Seeded correlation (fgdb-2acff) admits the four UNWIND rows and the
        // three vertices whose p was unwound: seven rows, not the 4 x 3 product.
        for input_limit in [7, 6] {
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(4, 1, WORK, WORK),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    // Three intermediate groups exceed the resident group
                    // capacity, and one-row runs require external merging.
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    input_limit,
                    WORK,
                    None,
                )
                .await;
            if input_limit == 7 {
                let (spool, _) = result.unwrap();
                // The fixture has p=[1,2,13]. Correlation keeps [1,1,2,13],
                // grouped as (1,2),(2,1),(13,1). The ordered window removes
                // (13,1), leaving two groups with weighted total 1*2+2*1=4.
                assert_eq!(spool.columns(), &["rows", "total"]);
                assert_eq!(spool.snapshot_seq(), CommitSeq(3));
                // The one graph scan charges all four identities, including
                // the retired vertex, across both reduction stages.
                assert_eq!(spool.row_stats().snapshot_records, 4);
                assert_eq!(spool.row_count(), 1);
                assert_eq!(contents(&spool, &mut destination, &cx).await, summary(2, 4));
            } else {
                let error = result.expect_err("the correlated input needs seven rows");
                // The final one-row aggregate and intermediate window must
                // not hide admission of the four UNWIND rows and three
                // seeded vertex rows.
                assert!(
                    matches!(
                        &error,
                        NativeAggregateSpoolError::Spool(NativeSpoolError::BufferedExecute(inner))
                            if matches!(**inner, GqlQueryError::Rows(ref exceeded)
                                if exceeded.dimension == GqlBudgetDimension::ResultRows
                                    && exceeded.limit == 6
                                    && exceeded.observed == 7)
                    ),
                    "{error:?}"
                );
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_pipeline_aggregate_budgets_continue_across_source_stages_and_reduction() {
    let ((), report) = run_async_under_lab(0xc01d_a102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!(
            "{MATCH} WITH r.p+s.p AS value ORDER BY value DESC SKIP 1 LIMIT 3 RETURN COUNT(*) AS rows,SUM(value) AS total"
        ));
        let mut exact = None;
        for case in 0..8 {
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let mut budget = policy(if case == 2 { 0 } else { 1 });
            let mut work = WORK;
            if let Some((records, native_work, scratch, external)) = exact {
                budget = GqlQueryPolicy::new(
                    records - u64::from(case == 4),
                    if case == 2 { 0 } else { 1 },
                    native_work - u64::from(case == 5),
                    scratch - u64::from(case == 6),
                );
                work = external - u64::from(case == 7);
            }
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    budget,
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    // The source has six rows even though its WITH page
                    // selects three and the final aggregate has just one.
                    if case == 3 { 5 } else { 6 },
                    work,
                    None,
                )
                .await;
            if case < 2 {
                let (spool, spent) = result.unwrap();
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    summary(3, 63)
                );
                let stats = spool.evaluator_stats();
                let measured = (
                    spool.row_stats().snapshot_records,
                    stats.work_units,
                    stats.scratch_entries,
                    spent,
                );
                if let Some(exact) = exact {
                    assert_eq!(measured, exact);
                }
                exact = Some(measured);
            } else {
                let error = result.expect_err("a cumulative boundary must refuse");
                let diagnostic = format!("{error:?}");
                let dimension = match case {
                    2 => "ResultRows",
                    3 => "Rows",
                    4 => "SnapshotRecords",
                    5 => "WorkUnits",
                    6 => "ScratchEntries",
                    7 => "SortWorkLimit",
                    _ => unreachable!(),
                };
                assert!(diagnostic.contains(dimension), "case {case}: {diagnostic}");
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_pipeline_aggregate_orders_more_input_payload_than_its_scratch_pool() {
    const ROWS: usize = 128;
    const PAYLOAD_BYTES: usize = 4200;
    const SCRATCH_BYTES: usize = 512 * 1024;
    let ((), report) = run_async_under_lab(0xc01d_a103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 1..=ROWS {
            let text = format!("{id:04}:{}", "x".repeat(PAYLOAD_BYTES));
            batch.create_vertex(
                VId(id as u128),
                vec![],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::ucs_basic_text(&text).unwrap(),
                    ),
                ],
            );
        }
        assert_eq!(db.write(&commit, batch).await.unwrap(), CommitSeq(1));
        drop(db);
        const { assert!(ROWS * PAYLOAD_BYTES > SCRATCH_BYTES) };
        let prepared = prepare(
            "MATCH (n) WITH n.q AS label,n.p AS value ORDER BY label DESC LIMIT 96 RETURN COUNT(*) AS rows,SUM(value) AS total",
        );
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(SCRATCH_BYTES, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
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
                16 * 1024,
                ROWS as u64,
                1_000_000_000,
                None,
            )
            .await
            .unwrap();
        // The source values selected by the descending label page are
        // 128 through 33; their independent arithmetic-series sum is 7,728.
        assert_eq!(
            contents(&spool, &mut destination, &cx).await,
            summary(96, 7728)
        );
        assert_eq!(spool.snapshot_seq(), CommitSeq(1));
        assert!(
            source.stats().reserved_bytes
                + partition.stats().reserved_bytes
                + destination.stats().reserved_bytes
                > SCRATCH_BYTES as u64
        );
        assert!(view.buffer_stats().bypasses > 0);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_a_blocked_relational_aggregate_phase_releases_source_and_scratch_owners() {
    let ((), report) = run_async_under_lab(0xc01d_a104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(
            "MATCH (n) WITH n.q AS label,n.p%2 AS bucket ORDER BY label RETURN bucket,COUNT(*) AS rows GROUP BY bucket ORDER BY bucket",
        );
        // Every scratch role is interrupted independently. One resident group
        // makes the partition file necessary after the ordered child succeeds.
        for blocked in 0..3 {
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, source_backing) = file(&cx, &spill_pool).await;
            let (mut partition, partition_backing) = file(&cx, &spill_pool).await;
            let (mut destination, destination_backing) = file(&cx, &spill_pool).await;
            let backing = [&source_backing, &partition_backing, &destination_backing][blocked];
            {
                let mut state = backing.0.lock().unwrap();
                state.pending = true;
                state.writes = 0;
            }
            let mut future = prepared.spool_in_view(
                &mut view,
                &cx,
                policy(2),
                &mut source,
                &mut partition,
                &mut destination,
                1,
                512,
                1,
                512,
                63,
                16 * 1024,
                1000,
                WORK,
                None,
            );
            poll_fn(|task| {
                let result = future.as_mut().poll(task);
                if backing.0.lock().unwrap().writes != 0 {
                    assert!(result.is_pending());
                    Poll::Ready(())
                } else {
                    assert!(result.is_pending(), "phase {blocked} was not reached");
                    Poll::Pending
                }
            })
            .await;
            assert!(spill_pool.used() > 0);
            drop(future);
            assert_eq!(source_pool.used(), baseline, "phase {blocked}");
            assert_eq!(spill_pool.used(), 0, "phase {blocked}");
            assert!(
                [&source, &partition, &destination][blocked].is_poisoned(),
                "phase {blocked} must retire its interrupted writer"
            );
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn cold_join_aggregates_preserve_history_retirement_grouping_distinct_and_having() {
    let ((), report) = run_async_under_lab(0xc01d_a001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        for (cut, count, sum, triples, triple_sum, distinct_sum, winner, winning_sum) in [
            (1, 7, 126, 9, 246, 30, 3, 54),
            (2, 7, 146, 9, 276, 30, 13, 64),
            (3, 6, 125, 6, 179, 19, 13, 64),
        ] {
            for (text, expected) in [
                (
                    format!("{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} {TOTAL}"),
                    summary(count, sum),
                ),
                (
                    format!(
                        "{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS rows, COUNT(DISTINCT s.p) AS support, SUM(DISTINCT s.p) AS total"
                    ),
                    vec![(
                        vec![],
                        vec![
                            GraphAggregateValue::Count(count),
                            GraphAggregateValue::Count(if cut == 3 { 2 } else { 3 }),
                            GraphAggregateValue::Integer(distinct_sum),
                        ],
                    )],
                ),
                (
                    format!(
                        "{MATCH}-[t:R]->(d) FOR SYSTEM_TIME AS OF SEQ {cut} RETURN COUNT(*) AS rows, SUM(r.p+s.p+t.p) AS total"
                    ),
                    summary(triples, triple_sum),
                ),
                (
                    format!(
                        "{MATCH} FOR SYSTEM_TIME AS OF SEQ {cut} RETURN c.p AS destination, COUNT(*) AS rows, SUM(r.p+s.p) AS total HAVING COUNT(*) >= 2 ORDER BY total DESC LIMIT 1"
                    ),
                    vec![(
                        vec![GraphValue::Scalar(CanonicalScalar::Int(winner))],
                        vec![
                            GraphAggregateValue::Count(3),
                            GraphAggregateValue::Integer(winning_sum),
                        ],
                    )],
                ),
            ] {
                let prepared = prepare(&text);
                let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
                let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
                let mut view = view(&commit, &vfs, &source_pool).await;
                let baseline = source_pool.used();
                let (mut source, _) = file(&cx, &spill_pool).await;
                let (mut partition, _) = file(&cx, &spill_pool).await;
                let (mut destination, _) = file(&cx, &spill_pool).await;
                // A single resident group forces grouped input through the
                // real external partitioner. Final quota is ONE, not input N.
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
                        16 * 1024,
                        1000,
                        WORK,
                        None,
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{text}: {error}"));
                assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
                assert_eq!(spool.row_count(), 1);
                assert_eq!(spool.row_stats().result_rows, 1);
                assert!(
                    spool.row_stats().snapshot_records > 5,
                    "nested reads share the source meter"
                );
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    expected,
                    "{text}"
                );
                assert_eq!(source_pool.used(), baseline);
                assert_eq!(spill_pool.used(), 0);
                drop(view);
                assert_eq!(source_pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn private_input_and_final_output_limits_remain_distinct_and_all_work_is_cumulative() {
    let ((), report) = run_async_under_lab(0xc01d_a002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!("{MATCH} {TOTAL}"));
        let mut exact = None;
        for case in 0..8 {
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let mut budget = policy(if case == 2 { 0 } else { 1 });
            let mut work = WORK;
            if let Some((records, native_work, scratch, external)) = exact {
                budget = GqlQueryPolicy::new(
                    records - u64::from(case == 4),
                    if case == 2 { 0 } else { 1 },
                    native_work - u64::from(case == 5),
                    scratch - u64::from(case == 6),
                );
                work = external - u64::from(case == 7);
            }
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    budget,
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    if case == 3 { 5 } else { 6 },
                    work,
                    None,
                )
                .await;
            if case < 2 {
                let (spool, spent) = result.unwrap();
                assert_eq!(
                    contents(&spool, &mut destination, &cx).await,
                    summary(6, 125)
                );
                let stats = spool.evaluator_stats();
                let measured = (
                    spool.row_stats().snapshot_records,
                    stats.work_units,
                    stats.scratch_entries,
                    spent,
                );
                if let Some(exact) = exact {
                    assert_eq!(measured, exact);
                }
                exact = Some(measured);
            } else {
                let error = result.unwrap_err();
                match (case, error) {
                    (2, NativeAggregateSpoolError::Execute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Rows(_)));
                    }
                    (
                        3,
                        NativeAggregateSpoolError::InputRows {
                            attempted: 6,
                            limit: 5,
                        },
                    ) => {}
                    (4, NativeAggregateSpoolError::BufferedExecute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Rows(_)));
                    }
                    (5 | 6, NativeAggregateSpoolError::Execute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Evaluator(_)));
                    }
                    (5 | 6, NativeAggregateSpoolError::BufferedExecute(error)) => {
                        assert!(matches!(*error, GqlQueryError::Evaluator(_)));
                    }
                    (
                        7,
                        NativeAggregateSpoolError::Spool(NativeSpoolError::SortWorkLimit {
                            ..
                        }),
                    ) => {}
                    (_, error) => panic!("wrong quota refusal in case {case}: {error}"),
                }
                // No partial result handle exists on any input/reduction or
                // final-admission refusal, even if scratch contains prefixes.
                assert!(view.buffer_stats().bypasses > 0);
            }
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_joined_expression_failures_precede_limit_zero_and_empty_global_count_survives() {
    let ((), report) = run_async_under_lab(0xc01d_a003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        for (text, succeeds, outputs) in [
            (
                format!("{MATCH} RETURN SUM(1/(s.p-10)) AS total LIMIT 0"),
                false,
                0,
            ),
            (format!("{MATCH} {TOTAL} LIMIT 0"), true, 0),
            (
                format!("{MATCH} WHERE a.p > 100 RETURN COUNT(*) AS rows"),
                true,
                1,
            ),
        ] {
            let prepared = prepare(&text);
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
            let mut view = view(&commit, &vfs, &source_pool).await;
            let baseline = source_pool.used();
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let result = prepared
                .spool_in_view(
                    &mut view,
                    &cx,
                    policy(outputs),
                    &mut source,
                    &mut partition,
                    &mut destination,
                    1,
                    512,
                    1,
                    512,
                    1024,
                    16 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await;
            if succeeds {
                let (spool, _) = result.unwrap();
                let expected = if outputs == 0 {
                    vec![]
                } else {
                    vec![(vec![], vec![GraphAggregateValue::Count(0)])]
                };
                assert_eq!(contents(&spool, &mut destination, &cx).await, expected);
            } else {
                assert!(matches!(
                    result,
                    Err(NativeAggregateSpoolError::BufferedExecute(_))
                ));
            }
            assert!(view.buffer_stats().bypasses > 0);
            assert_eq!(source_pool.used(), baseline);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        }
        // A correlated fixed-hop probe is a buffered join input now (3faae262,
        // 0e7c7e7c; fgdb-4wrnd). Mixed fixed/variable atoms still refuse,
        // as do unprojected path/join collections without an occurrence-order
        // proof. LIMIT zero cannot bypass complete preparation.
        for (text, buffered) in [
            (
                "MATCH (a)-[r:R]->(b)-[:R*1..2]->(c) RETURN COUNT(*) AS rows LIMIT 0",
                false,
            ),
            (
                "MATCH REPEATABLE ELEMENTS WALK (a)-[:R*0..3]->(b) RETURN COLLECT(b.p) AS values LIMIT 0",
                false,
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) WHERE EXISTS { MATCH (c)-[:R]->(d) } RETURN COUNT(*) AS rows LIMIT 0",
                true,
            ),
            (
                "MATCH (a)-[r:R]->(b)-[s:R]->(c) RETURN COLLECT(s.p) AS values LIMIT 0",
                false,
            ),
        ] {
            let params = GqlParameters::new();
            let native = PreparedNativeRead::prepare(text, &params, resolve).unwrap();
            assert_eq!(
                native.prepare_buffered_aggregate(&params).is_ok(),
                buffered,
                "{text}"
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_a_blocked_aggregate_append_releases_cold_ancestors_computed_row_and_encoder() {
    let ((), report) = run_async_under_lab(0xc01d_a004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!(
            "{MATCH} RETURN a.q AS label, SUM(r.p+s.p) AS total"
        ));
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut view = view(&commit, &vfs, &source_pool).await;
        let baseline = source_pool.used();
        let (mut source, backing) = file(&cx, &spill_pool).await;
        let (mut partition, _) = file(&cx, &spill_pool).await;
        let (mut destination, _) = file(&cx, &spill_pool).await;
        {
            let mut state = backing.0.lock().unwrap();
            state.pending = true;
            state.writes = 0;
        }
        let mut future = prepared.spool_in_view(
            &mut view,
            &cx,
            policy(10),
            &mut source,
            &mut partition,
            &mut destination,
            1,
            512,
            1,
            512,
            63,
            16 * 1024,
            1000,
            WORK,
            None,
        );
        poll_fn(|task| {
            let result = future.as_mut().poll(task);
            if backing.0.lock().unwrap().writes != 0 {
                assert!(result.is_pending());
                Poll::Ready(())
            } else {
                assert!(
                    result.is_pending(),
                    "source failed before the injected scratch suspension"
                );
                Poll::Pending
            }
        })
        .await;
        assert!(source_pool.used() > baseline);
        assert!(spill_pool.used() > 0);
        drop(future);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(spill_pool.used(), 0);
        assert!(source.is_poisoned());
        assert_eq!(source.stats().published_runs, 0);
        assert_eq!(destination.stats().published_runs, 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_join_summary_stays_pinned_across_writer_compaction_and_result_ownership() {
    let ((), report) = run_async_under_lab(0xc01d_a005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = fixture(&commit).await;
        let prepared = prepare(&format!("{MATCH} {TOTAL}"));
        let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &source_pool).await;
        let mut writer = Database::open_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(77)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(4));
        writer.compact(&commit).await.unwrap();
        drop(writer);
        for (cut, sum) in [(3, 125), (4, 245)] {
            let (mut source, _) = file(&cx, &spill_pool).await;
            let (mut partition, _) = file(&cx, &spill_pool).await;
            let (mut destination, _) = file(&cx, &spill_pool).await;
            let (spool, _) = prepared
                .spool_in_view(
                    &mut old,
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
                    16 * 1024,
                    1000,
                    WORK,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
            let mut reader = spool.reader(&mut destination, None);
            let row = reader.next_row(&cx).await.unwrap().unwrap();
            assert_eq!(
                row.values(),
                &[
                    GraphAggregateValue::Count(6),
                    GraphAggregateValue::Integer(sum)
                ]
            );
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            drop(old);
            assert_eq!(source_pool.used(), 0);
            assert!(
                spill_pool.used() > 0,
                "the delivered row retains its decoded reservation"
            );
            drop(row);
            assert_eq!(spill_pool.used(), 0);
            old = view(&commit, &vfs, &source_pool).await;
        }
        drop(old);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
