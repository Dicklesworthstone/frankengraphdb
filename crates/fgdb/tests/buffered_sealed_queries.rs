//! Composed local-owner queries consume the real buffered sealed procedure
//! source and the existing external relational executor. Literal graph answers
//! are independent of both the resident and buffered query implementations.
#![recursion_limit = "256"]

#[path = "buffered_sealed_queries/aggregates.rs"]
mod aggregates;
#[path = "buffered_sealed_queries/repeated.rs"]
mod repeated;

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, GqlError, MemVfs,
    MemoryPool, NativeSpoolError, PreparedBufferedSealedOrder, PreparedNativeRead, ProcedureError,
    QueryResult, QueryValue, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GqlBudgetDimension, GqlParameters, GqlQueryError, GqlQueryPolicy, GraphIntegerErrorKind,
    GraphSetExecutionError, GraphSymbol, GraphSymbolKind,
};
use fgdb_prism::{FnxMemoryLimits, FnxReadError, FnxSealedReadError};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use fgdb_strata::tiered::sealed::SealedLimits;
use fgdb_types::{
    CanonicalScalar, CommitCx, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts,
    QueryCx, VId,
};
use std::future::poll_fn;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

const R: RelationId = RelationId(7);
const S: RelationId = RelationId(8);
const PERSON: LabelId = LabelId(10);
const OTHER: LabelId = LabelId(11);
const P: PropertyKeyId = PropertyKeyId(9);
const BASE: u128 = 1_u128 << 100;
const SECOND: u128 = BASE + 2;
const THIRD: u128 = BASE + 3;
const FOURTH: u128 = BASE + 4;
const HIDDEN: u128 = BASE + 99;
const WORK: u64 = 100_000_000;
const RAW: usize = 1000;
const BFS: &str = "CALL fnx.single_source_shortest_path_length(\
    args => [$start], label => 'Person', relation => 'R', direction => 'directed', \
    parallel => 'collapse', self_loops => 'drop')";
const WCC: &str = "CALL fnx.weakly_connected_components(\
    label => 'Person', relation => 'R', direction => 'directed', \
    parallel => 'collapse', self_loops => 'drop')";

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x9a; 32],
        DatabaseSecurityNamespaceId([0x9b; 32]),
        [0x9c; 32],
    )
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Relation, "S") => Some(GraphSymbol::Relation(S)),
        (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(PERSON)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn parameters() -> GqlParameters {
    GqlParameters::new().with_int64("start", 1).unwrap()
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 100, WORK, WORK)
}
fn memory() -> FnxMemoryLimits {
    FnxMemoryLimits {
        max_kernel_workspace_bytes: 1024 * 1024,
        max_result_bytes: 1024 * 1024,
    }
}
fn sealing() -> SealedLimits {
    SealedLimits {
        max_rows: 100,
        max_incidences: 1000,
        max_image_bytes: 1024 * 1024,
        max_property_bytes: 64 * 1024,
    }
}
fn vertex(id: u128) -> GraphValue {
    GraphValue::Vertex(VId(id))
}
fn integer(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn path(start: u128, steps: &[(u128, u128)]) -> GraphValue {
    GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
        VId(start),
        steps
            .iter()
            .map(|&(edge, vertex)| (EId(edge), VId(vertex)))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    ))
}
fn prepare(text: &str, params: &GqlParameters) -> PreparedBufferedSealedOrder {
    PreparedNativeRead::prepare(text, params, resolve)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
        .prepare_buffered_sealed_order(params, R, memory(), sealing(), RAW)
        .unwrap_or_else(|error| panic!("{text}: {error:?}"))
}

/// Selected R is 1 -> SECOND -> THIRD, with a parallel first edge, a reverse
/// edge and a loop. FOURTH and MAX are isolated. The excluded-label bridge and
/// unrelated S shortcut would both change the answers if selection leaked.
async fn fixture(cx: &CommitCx) -> (MemVfs, Database<MemVfs>) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, value) in [
        (1, 10),
        (SECOND, 20),
        (THIRD, 30),
        (FOURTH, 40),
        (u128::MAX, 50),
    ] {
        batch.create_vertex(
            VId(id),
            vec![PERSON],
            vec![(P, CanonicalScalar::Int(value))],
        );
    }
    batch.create_vertex(
        VId(HIDDEN),
        vec![OTHER],
        vec![(P, CanonicalScalar::Int(99))],
    );
    for (eid, from, to) in [
        (1, 1, SECOND),
        (2, 1, SECOND),
        (3, SECOND, THIRD),
        (4, THIRD, SECOND),
        (5, THIRD, THIRD),
        (6, THIRD, HIDDEN),
        (7, HIDDEN, FOURTH),
    ] {
        batch.add_edge(EId(BASE + eid), VId(from), VId(to), vec![]);
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    let mut unrelated = WriteBatch::new(S);
    unrelated.add_edge(EId(u128::MAX), VId(1), VId(FOURTH), vec![]);
    assert_eq!(db.write(cx, unrelated).await.unwrap(), CommitSeq(2));
    (vfs, db)
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
            max_source_bytes: 4 * 1024 * 1024,
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

// Real SpillFile framing/authentication is retained. Only the backing I/O
// adapter can pause on a complete canonical procedure row for the drop law.
#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
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
        if state
            .suspend_row
            .as_ref()
            .is_some_and(|row| bytes.windows(row.len()).any(|part| part == row.as_slice()))
        {
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
            max_file_bytes: 16 * 1024 * 1024,
            max_runs: 4096,
            max_run_bytes: 4 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    snapshot: CommitSeq,
    rows: Vec<Vec<GraphValue>>,
    records: u64,
    evaluator_work: u64,
    evaluator_scratch: u64,
    sort_work: u64,
}
async fn execute(
    prepared: &PreparedBufferedSealedOrder,
    cold: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    allowance: GqlQueryPolicy,
    pool: &MemoryPool,
    run_rows: usize,
    max_sort_work: u64,
) -> Result<Answer, NativeSpoolError> {
    let (mut scratch, _) = file(cx, pool).await;
    let (mut destination, _) = file(cx, pool).await;
    let (mut auxiliary, _) = file(cx, pool).await;
    let (spool, sort_work) = prepared
        .spool_in_view(
            cold,
            cx,
            allowance,
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
            max_sort_work,
        )
        .await?;
    let mut reader = spool.reader(&mut destination);
    let mut rows = Vec::new();
    while let Some(bytes) = reader.next_row(cx).await? {
        rows.push(
            GraphValueRow::decode_canonical(bytes.as_ref())
                .map_err(NativeSpoolError::Decode)?
                .values()
                .to_vec(),
        );
    }
    assert_eq!(spool.row_count(), rows.len() as u64);
    Ok(Answer {
        snapshot: spool.snapshot_seq(),
        rows,
        records: spool.row_stats().snapshot_records,
        evaluator_work: spool.evaluator_stats().work_units,
        evaluator_scratch: spool.evaluator_stats().scratch_entries,
        sort_work,
    })
}
fn resident_rows(result: QueryResult) -> Vec<Vec<GraphValue>> {
    let QueryResult::Rows { rows, .. } = result else {
        panic!("read query must return rows")
    };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    QueryValue::Value(value) => value,
                    _ => panic!("intermediate grouping must return ordinary native row values"),
                })
                .collect()
        })
        .collect()
}
fn bfs_rows() -> Vec<Vec<GraphValue>> {
    [(1, 0), (SECOND, 1), (THIRD, 2)]
        .into_iter()
        .map(|(id, distance)| vec![vertex(id), integer(distance)])
        .collect()
}

#[test]
fn sealed_bfs_and_components_compose_with_properties_matches_and_grouping() {
    let ((), report) = run_async_under_lab(0x6273_5101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = cold.sealed_read_cache_pool().clone();
        let cases = [
            (
                format!("{BFS} YIELD node,distance RETURN node,distance ORDER BY node"),
                bfs_rows(),
                0,
            ),
            (
                format!("{BFS} YIELD distance AS hops RETURN hops"),
                vec![vec![integer(0)], vec![integer(1)], vec![integer(2)]],
                0,
            ),
            (
                format!(
                    "{BFS} YIELD distance AS hops,node AS person \
                     RETURN hops,person ORDER BY hops DESC SKIP 1 LIMIT 1"
                ),
                vec![vec![integer(1), vertex(SECOND)]],
                0,
            ),
            (
                format!("{WCC} YIELD component AS group_id RETURN group_id"),
                vec![
                    vec![vertex(1)],
                    vec![vertex(1)],
                    vec![vertex(1)],
                    vec![vertex(FOURTH)],
                    vec![vertex(u128::MAX)],
                ],
                0,
            ),
            (
                format!(
                    "{BFS} YIELD node,distance MATCH (node:Person) WHERE distance>0 \
                         RETURN node,node.p AS p,distance ORDER BY p DESC,node LIMIT 2"
                ),
                vec![
                    vec![vertex(THIRD), integer(30), integer(2)],
                    vec![vertex(SECOND), integer(20), integer(1)],
                ],
                0,
            ),
            (
                format!(
                    "{BFS} YIELD node MATCH p = WALK (node)-[:R*0..1]->(next:Person) \
                     RETURN node,next,p ORDER BY node,length(p),p"
                ),
                vec![
                    vec![vertex(1), vertex(1), path(1, &[])],
                    vec![vertex(1), vertex(SECOND), path(1, &[(BASE + 1, SECOND)])],
                    vec![vertex(1), vertex(SECOND), path(1, &[(BASE + 2, SECOND)])],
                    vec![vertex(SECOND), vertex(SECOND), path(SECOND, &[])],
                    vec![
                        vertex(SECOND),
                        vertex(THIRD),
                        path(SECOND, &[(BASE + 3, THIRD)]),
                    ],
                    vec![vertex(THIRD), vertex(THIRD), path(THIRD, &[])],
                    vec![
                        vertex(THIRD),
                        vertex(SECOND),
                        path(THIRD, &[(BASE + 4, SECOND)]),
                    ],
                    vec![
                        vertex(THIRD),
                        vertex(THIRD),
                        path(THIRD, &[(BASE + 5, THIRD)]),
                    ],
                ],
                0,
            ),
            (
                format!(
                    "{WCC} YIELD node MATCH p = SIMPLE (node)-[:R*0..0]->(next) \
                     RETURN node,next,p ORDER BY node"
                ),
                [1, SECOND, THIRD, FOURTH, u128::MAX]
                    .map(|id| vec![vertex(id), vertex(id), path(id, &[])])
                    .to_vec(),
                0,
            ),
            (
                format!("{WCC} YIELD node,component RETURN node,component ORDER BY node"),
                vec![
                    vec![vertex(1), vertex(1)],
                    vec![vertex(SECOND), vertex(1)],
                    vec![vertex(THIRD), vertex(1)],
                    vec![vertex(FOURTH), vertex(FOURTH)],
                    vec![vertex(u128::MAX), vertex(u128::MAX)],
                ],
                0,
            ),
            (
                format!(
                    "{WCC} YIELD node,component MATCH (component:Person) \
                         RETURN node,component,component.p AS p ORDER BY node"
                ),
                vec![
                    vec![vertex(1), vertex(1), integer(10)],
                    vec![vertex(SECOND), vertex(1), integer(10)],
                    vec![vertex(THIRD), vertex(1), integer(10)],
                    vec![vertex(FOURTH), vertex(FOURTH), integer(40)],
                    vec![vertex(u128::MAX), vertex(u128::MAX), integer(50)],
                ],
                0,
            ),
            (
                format!(
                    "{WCC} YIELD node,component WITH component,COUNT(*) AS n \
                         RETURN component,n ORDER BY n DESC,component"
                ),
                vec![
                    vec![vertex(1), integer(3)],
                    vec![vertex(FOURTH), integer(1)],
                    vec![vertex(u128::MAX), integer(1)],
                ],
                1,
            ),
            (
                format!(
                    "{BFS} YIELD node,distance MATCH (node)-[:R]->(next:Person) \
                         RETURN node,next,distance ORDER BY node,next"
                ),
                // The procedure's collapse/drop recipe must not change later
                // native MATCH bag semantics: both parallel edges and the loop survive.
                vec![
                    vec![vertex(1), vertex(SECOND), integer(0)],
                    vec![vertex(1), vertex(SECOND), integer(0)],
                    vec![vertex(SECOND), vertex(THIRD), integer(1)],
                    vec![vertex(THIRD), vertex(SECOND), integer(2)],
                    vec![vertex(THIRD), vertex(THIRD), integer(2)],
                ],
                0,
            ),
        ];
        for (text, expected, aggregates) in cases {
            let params = if text.contains("$start") {
                parameters()
            } else {
                GqlParameters::new()
            };
            assert_eq!(
                resident_rows(
                    writer
                        .query(&cx, &text, &params, resolve, policy())
                        .unwrap()
                ),
                expected,
                "resident {text}",
            );
            let prepared = prepare(&text, &params);
            assert!(prepared.stage_count() > 0);
            assert_eq!(prepared.aggregate_count(), aggregates);
            for run_rows in [1, 3] {
                let answer = execute(
                    &prepared,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    run_rows,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(answer.snapshot, CommitSeq(2));
                assert_eq!(answer.rows, expected, "run_rows={run_rows}: {text}");
                assert_eq!(spill_pool.used(), 0);
            }
        }
        assert_eq!(writer.frontier().unwrap(), CommitSeq(2));
        assert!(retained.used() > 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn compressed_bridge_pairs_preserve_shared_sources_and_typed_neighbor_composition() {
    let ((), report) = run_async_under_lab(0x6273_5110, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut writer) = fixture(&commit).await;
        let mut branches = WriteBatch::new(R);
        branches.add_edge(EId(BASE + 20), VId(1), VId(FOURTH), vec![]);
        branches.add_edge(EId(BASE + 21), VId(1), VId(u128::MAX), vec![]);
        assert_eq!(writer.write(&commit, branches).await.unwrap(), CommitSeq(3));
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let params = GqlParameters::new();
        let call = "CALL fnx.bridges(label => 'Person', relation => 'R', direction => 'undirected', parallel => 'collapse', self_loops => 'keep')";
        for (text, expected) in [
            (
                format!("{call} YIELD node,neighbor RETURN node,neighbor ORDER BY node,neighbor"),
                vec![
                    vec![vertex(1), vertex(SECOND)],
                    vec![vertex(1), vertex(FOURTH)],
                    vec![vertex(1), vertex(u128::MAX)],
                    vec![vertex(SECOND), vertex(THIRD)],
                ],
            ),
            (
                format!(
                    "{call} YIELD neighbor AS target,node AS origin \
                     WITH target,origin SKIP 1 LIMIT 2 RETURN target,origin"
                ),
                // The source's pair order is (1,SECOND), (1,FOURTH),
                // (1,MAX), (SECOND,THIRD). Reordered YIELD must sort by
                // target before its WITH window can select these rows.
                vec![
                    vec![vertex(THIRD), vertex(SECOND)],
                    vec![vertex(FOURTH), vertex(1)],
                ],
            ),
            (
                format!("{call} YIELD neighbor AS target RETURN target SKIP 1 LIMIT 2"),
                vec![vec![vertex(THIRD)], vec![vertex(FOURTH)]],
            ),
            (
                format!("{call} YIELD node,neighbor RETURN node,-neighbor.p AS value"),
                // A unique-leading-vertex shortcut would keep the wrong
                // secondary order within the three rows rooted at vertex 1.
                vec![
                    vec![vertex(1), integer(-50)],
                    vec![vertex(1), integer(-40)],
                    vec![vertex(1), integer(-20)],
                    vec![vertex(SECOND), integer(-30)],
                ],
            ),
            (
                format!("{call} YIELD node,neighbor MATCH (neighbor:Person) RETURN node,neighbor.p AS p ORDER BY node,p"),
                vec![
                    vec![vertex(1), integer(20)],
                    vec![vertex(1), integer(40)],
                    vec![vertex(1), integer(50)],
                    vec![vertex(SECOND), integer(30)],
                ],
            ),
            (
                format!("{call} YIELD node,neighbor WITH node,COUNT(*) AS n RETURN node,n ORDER BY node"),
                vec![vec![vertex(1), integer(3)], vec![vertex(SECOND), integer(1)]],
            ),
            (
                "CALL fnx.articulation_points(label => 'Person', relation => 'R', direction => 'undirected', parallel => 'collapse', self_loops => 'keep') YIELD node RETURN node,node.p AS p ORDER BY node".to_owned(),
                vec![vec![vertex(1), integer(10)], vec![vertex(SECOND), integer(20)]],
            ),
        ] {
            assert_eq!(
                resident_rows(writer.query(&cx, &text, &params, resolve, policy()).unwrap()),
                expected,
                "resident {text}",
            );
            let prepared = prepare(&text, &params);
            for run_rows in [1, 3] {
                let answer = execute(
                    &prepared,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    run_rows,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(answer.snapshot, CommitSeq(3));
                assert_eq!(answer.rows, expected, "run_rows={run_rows}: {text}");
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn neighbor_only_bridges_preserve_duplicate_targets_before_distinct_and_paging() {
    let ((), report) = run_async_under_lab(0x6273_5111, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let (commit, cx) = (contexts.commit(), contexts.query());
        let (vfs, mut writer) = fixture(&commit).await;
        let mut links = WriteBatch::new(R);
        links.add_edge(EId(BASE + 20), VId(THIRD), VId(u128::MAX), vec![]);
        links.add_edge(EId(BASE + 21), VId(FOURTH), VId(u128::MAX), vec![]);
        assert_eq!(writer.write(&commit, links).await.unwrap(), CommitSeq(3));
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let params = GqlParameters::new();
        let call = "CALL fnx.bridges(label => 'Person',relation => 'R',direction => 'undirected',parallel => 'collapse',self_loops => 'keep')";
        // The projected tree is 1--SECOND--THIRD--MAX--FOURTH. Its last
        // two canonical endpoint pairs both project to the same neighbor.
        for (text, expected) in [
            (
                format!("{call} YIELD neighbor AS target RETURN target"),
                vec![
                    vec![vertex(SECOND)],
                    vec![vertex(THIRD)],
                    vec![vertex(u128::MAX)],
                    vec![vertex(u128::MAX)],
                ],
            ),
            (
                format!("{call} YIELD neighbor AS target RETURN DISTINCT target"),
                vec![
                    vec![vertex(SECOND)],
                    vec![vertex(THIRD)],
                    vec![vertex(u128::MAX)],
                ],
            ),
            (
                format!(
                    "{call} YIELD neighbor AS target \
                     WITH target SKIP 3 LIMIT 1 RETURN target"
                ),
                vec![vec![vertex(u128::MAX)]],
            ),
            (
                format!(
                    "{call} YIELD neighbor AS target WITH target,COUNT(*) AS occurrences \
                     RETURN target,occurrences ORDER BY target"
                ),
                vec![
                    vec![vertex(SECOND), integer(1)],
                    vec![vertex(THIRD), integer(1)],
                    vec![vertex(u128::MAX), integer(2)],
                ],
            ),
            (
                format!(
                    "{call} YIELD neighbor AS target MATCH (target:Person) \
                     RETURN target,target.p AS value ORDER BY target"
                ),
                vec![
                    vec![vertex(SECOND), integer(20)],
                    vec![vertex(THIRD), integer(30)],
                    vec![vertex(u128::MAX), integer(50)],
                    vec![vertex(u128::MAX), integer(50)],
                ],
            ),
        ] {
            assert_eq!(
                resident_rows(
                    writer
                        .query(&cx, &text, &params, resolve, policy())
                        .unwrap()
                ),
                expected,
                "resident {text}",
            );
            let native = PreparedNativeRead::prepare(&text, &params, resolve).unwrap();
            assert!(native.prepare_buffered_order(&params).is_err());
            let prepared = native
                .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
                .unwrap();
            for run_rows in [1, 3] {
                let actual = execute(
                    &prepared,
                    &mut cold,
                    &cx,
                    policy(),
                    &spill_pool,
                    run_rows,
                    WORK,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(actual.snapshot, CommitSeq(3));
                assert_eq!(actual.rows, expected, "run_rows={run_rows}: {text}");
                assert_eq!(spill_pool.used(), 0);
            }
        }
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn old_publication_and_explicit_history_keep_topology_and_properties_together() {
    let ((), report) = run_async_under_lab(0x6273_5102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut writer) = fixture(&commit).await;
        let old_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let current_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &old_pool).await;
        let old_root = old.partition_root();
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(BASE + 8), VId(THIRD), VId(FOURTH), vec![]);
        change.set_vertex_property(VId(THIRD), P, Some(CanonicalScalar::Int(300)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(3));
        writer.compact(&commit).await.unwrap();
        let mut current = view(&commit, &vfs, &current_pool).await;
        assert_ne!(current.partition_root(), old_root);
        let params = parameters();
        let text = format!(
            "{BFS} YIELD node,distance MATCH (node:Person) \
                           RETURN node,distance,node.p AS p ORDER BY node"
        );
        let historical = format!(
            "{BFS} YIELD node,distance MATCH (node:Person) \
                                 FOR SYSTEM_TIME AS OF SEQ 2 \
                                 RETURN node,distance,node.p AS p ORDER BY node"
        );
        let earlier = vec![
            vec![vertex(1), integer(0), integer(10)],
            vec![vertex(SECOND), integer(1), integer(20)],
            vec![vertex(THIRD), integer(2), integer(30)],
        ];
        let later = vec![
            vec![vertex(1), integer(0), integer(10)],
            vec![vertex(SECOND), integer(1), integer(20)],
            vec![vertex(THIRD), integer(2), integer(300)],
            vec![vertex(FOURTH), integer(3), integer(40)],
        ];
        let prepared = prepare(&text, &params);
        let pinned = execute(&prepared, &mut old, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(
            (pinned.snapshot, pinned.rows),
            (CommitSeq(2), earlier.clone())
        );
        let historical_prepared = prepare(&historical, &params);
        assert!(
            historical_prepared
                .clone()
                .with_as_of(CommitSeq(3))
                .is_err()
        );
        let selected = execute(
            &historical_prepared,
            &mut current,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(
            (selected.snapshot, selected.rows),
            (CommitSeq(2), earlier.clone())
        );
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &historical, &params, resolve, policy())
                    .unwrap()
            ),
            earlier,
        );
        let owner_cut = prepared.clone().with_as_of(CommitSeq(2)).unwrap();
        let owner_selected = execute(
            &owner_cut,
            &mut current,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(
            (owner_selected.snapshot, owner_selected.rows),
            (CommitSeq(2), earlier.clone())
        );
        let latest = execute(&prepared, &mut current, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!((latest.snapshot, latest.rows), (CommitSeq(3), later));
        // Cache entries for the selected cut cannot replace this reader's publication.
        let pinned_again = execute(&prepared, &mut old, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(pinned_again.snapshot, CommitSeq(2));
        assert_eq!(pinned_again.rows, earlier);
        assert_eq!(spill_pool.used(), 0);
        drop(old);
        drop(current);
        assert_eq!(old_pool.used(), 0);
        assert_eq!(current_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_algorithms_and_property_reads_share_one_exact_historical_cut() {
    let ((), report) = run_async_under_lab(0x6273_5107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let text = format!(
            "{BFS} YIELD node,distance {WCC} YIELD node AS member,component \
             WITH node,distance,member,component WHERE node=member \
             MATCH route = WALK (node)-[:R*0..1]->(next:Person) \
             WITH node,distance,component,node.p AS p,COUNT(*) AS paths \
             RETURN node,distance,component,p,paths ORDER BY node"
        );
        let params = parameters();
        let prepared = prepare(&text, &params);
        let earlier = vec![
            vec![vertex(1), integer(0), vertex(1), integer(10), integer(3)],
            vec![
                vertex(SECOND),
                integer(1),
                vertex(1),
                integer(20),
                integer(2),
            ],
            vec![
                vertex(THIRD),
                integer(2),
                vertex(1),
                integer(30),
                integer(3),
            ],
        ];
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &text, &params, resolve, policy())
                    .unwrap()
            ),
            earlier,
        );
        let mut change = WriteBatch::new(R);
        change.add_edge(EId(BASE + 8), VId(THIRD), VId(FOURTH), vec![]);
        change.set_vertex_property(VId(THIRD), P, Some(CanonicalScalar::Int(300)));
        assert_eq!(writer.write(&commit, change).await.unwrap(), CommitSeq(3));
        writer.compact(&commit).await.unwrap();
        let mut current = view(&commit, &vfs, &source_pool).await;
        let retained = current.sealed_read_cache_pool().clone();
        let historical = prepared.clone().with_as_of(CommitSeq(2)).unwrap();
        assert!(historical.clone().with_as_of(CommitSeq(3)).is_err());
        let mut historical_answer = None;
        for run_rows in [1, 3] {
            let answer = execute(
                &historical,
                &mut current,
                &cx,
                policy(),
                &spill_pool,
                run_rows,
                WORK,
            )
            .await
            .unwrap();
            assert_eq!(answer.snapshot, CommitSeq(2));
            assert_eq!(answer.rows, earlier);
            assert_eq!(current.sealed_read_cache_usage().entries, 1);
            if run_rows == 1 {
                historical_answer = Some(answer);
            }
            assert_eq!(spill_pool.used(), 0);
        }
        let later = vec![
            vec![vertex(1), integer(0), vertex(1), integer(10), integer(3)],
            vec![
                vertex(SECOND),
                integer(1),
                vertex(1),
                integer(20),
                integer(2),
            ],
            vec![
                vertex(THIRD),
                integer(2),
                vertex(1),
                integer(300),
                integer(4),
            ],
            vec![
                vertex(FOURTH),
                integer(3),
                vertex(1),
                integer(40),
                integer(1),
            ],
        ];
        let latest = execute(&prepared, &mut current, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(latest.snapshot, CommitSeq(3));
        assert_eq!(latest.rows, later);
        let both_cuts = current.sealed_read_cache_usage();
        assert_eq!(both_cuts.entries, 2, "one shared graph per exact cut");
        assert_eq!(
            resident_rows(
                writer
                    .query(&cx, &text, &params, resolve, policy())
                    .unwrap()
            ),
            later,
        );
        let again = execute(
            &historical,
            &mut current,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap();
        assert_eq!(again.snapshot, CommitSeq(2));
        assert_eq!(again.rows, earlier);
        assert_eq!(again, historical_answer.unwrap());
        assert_eq!(current.sealed_read_cache_usage(), both_cuts);
        assert_eq!(spill_pool.used(), 0);
        drop(current);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn independent_source_receipts_and_external_limits_remain_cumulative_on_reuse() {
    let ((), report) = run_async_under_lab(0x6273_5108, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _writer) = fixture(&commit).await;
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let retained = cold.sealed_read_cache_pool().clone();
        let params = parameters();
        let left_text = format!("{BFS} YIELD node RETURN node ORDER BY node");
        let right_text = format!("{WCC} YIELD node RETURN node ORDER BY node");
        let union_text = format!("({left_text}) UNION ALL ({right_text})");
        let left = prepare(&left_text, &params);
        let right = prepare(&right_text, &GqlParameters::new());
        let union = prepare(&union_text, &params);
        let first = execute(&left, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        let shared = cold.sealed_read_cache_usage();
        assert_eq!(shared.entries, 1);
        let source_stats = cold.buffer_stats();
        let second = execute(&right, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(cold.sealed_read_cache_usage(), shared);
        assert_eq!(cold.buffer_stats(), source_stats);
        let combined = execute(&union, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(cold.sealed_read_cache_usage(), shared);
        assert_eq!(
            cold.buffer_stats(),
            source_stats,
            "the second source's smaller remaining allowance reuses construction",
        );
        assert_eq!(
            combined.rows,
            [1, 1, SECOND, SECOND, THIRD, THIRD, FOURTH, u128::MAX]
                .map(|id| vec![vertex(id)])
                .to_vec(),
        );
        assert_eq!(combined.records, first.records + second.records);
        assert!(combined.evaluator_work > first.evaluator_work.max(second.evaluator_work));
        assert!(combined.evaluator_scratch > first.evaluator_scratch.max(second.evaluator_scratch));
        assert!(combined.sort_work > first.sort_work.max(second.sort_work));
        let faults = cold.buffer_stats();
        let warm = execute(&union, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(warm, combined);
        assert_eq!(cold.buffer_stats(), faults);
        let exact = execute(
            &union,
            &mut cold,
            &cx,
            GqlQueryPolicy::new(
                10_000,
                100,
                combined.evaluator_work,
                combined.evaluator_scratch,
            ),
            &spill_pool,
            1,
            combined.sort_work,
        )
        .await
        .unwrap();
        assert_eq!(exact, combined);
        assert_eq!(cold.sealed_read_cache_usage(), shared);
        assert_eq!(
            cold.buffer_stats(),
            source_stats,
            "lower fitting budgets replay the same construction counters",
        );
        for dimension in 0..4 {
            let (allowance, sort_work) = match dimension {
                0 => (
                    GqlQueryPolicy::new(10_000, 100, combined.evaluator_work - 1, WORK),
                    WORK,
                ),
                1 => (
                    GqlQueryPolicy::new(10_000, 100, WORK, combined.evaluator_scratch - 1),
                    WORK,
                ),
                2 => (policy(), combined.sort_work - 1),
                3 => (GqlQueryPolicy::new(10_000, 7, WORK, WORK), WORK),
                _ => unreachable!(),
            };
            // The same cap admits either complete source alone. Combining them
            // cannot reset the query or physical counters at the second CALL.
            for single in [&left, &right] {
                execute(single, &mut cold, &cx, allowance, &spill_pool, 1, sort_work)
                    .await
                    .unwrap();
            }
            let error = execute(&union, &mut cold, &cx, allowance, &spill_pool, 1, sort_work)
                .await
                .unwrap_err();
            if dimension == 2 {
                assert!(
                    matches!(error, NativeSpoolError::SortWorkLimit { .. }),
                    "{error:?}"
                );
            }
            assert_eq!(spill_pool.used(), 0, "dimension {dimension}");
            assert_eq!(
                execute(&union, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
                    .await
                    .unwrap(),
                combined,
            );
        }
        let late = format!(
            "({BFS} YIELD node RETURN node LIMIT 0) UNION ALL \
             (CALL fnx.single_source_shortest_path_length(\
             args => [1/0],relation => 'R') YIELD node RETURN node LIMIT 0)"
        );
        let error = execute(
            &prepare(&late, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { error, .. }
            ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}",
        );
        // Named selector values retain reached-source binding. They are not
        // interchangeable with the static inline projection checked below.
        let mismatched = format!(
            "({BFS} YIELD node RETURN node LIMIT 0) UNION ALL \
             (CALL fnx.weakly_connected_components(relation => 'S') \
             YIELD node RETURN node LIMIT 0)"
        );
        let error = execute(
            &prepare(&mismatched, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::Procedure(error)
                if matches!(error.as_ref(), GqlQueryError::Source(
                    GqlError::Procedure(ProcedureError::SealedRelationMismatch { requested: Some(requested), retained })
                ) if *requested == S && *retained == R)),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn warm_construction_replays_admission_and_cannot_hide_limits_or_late_errors() {
    let ((), report) = run_async_under_lab(0x6273_5103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, writer) = fixture(&commit).await;
        let params = parameters();
        let text = format!("{BFS} YIELD node,distance RETURN node,distance ORDER BY node");
        let template = PreparedNativeRead::prepare(&text, &params, resolve).unwrap();
        let prepared = prepare(&text, &params);
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let first = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(first.rows, bfs_rows());
        assert!(first.records > 0 && first.sort_work > 0);
        let faults = cold.buffer_stats();
        let baseline = source_pool.used();
        let retained = cold.sealed_read_cache_pool().clone();
        assert!(retained.used() > 0);
        let warm = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(
            warm, first,
            "a cache hit must replay logical source admission"
        );
        assert_eq!(
            cold.buffer_stats(),
            faults,
            "a warm pure CALL needs no source faults"
        );
        let cache_usage = cold.sealed_read_cache_usage();
        let zero_work = GqlQueryPolicy::new(10_000, 100, 0, WORK);
        let warm_refusal = execute(&prepared, &mut cold, &cx, zero_work, &spill_pool, 1, WORK)
            .await
            .unwrap_err();
        assert_eq!(cold.buffer_stats(), faults);
        assert_eq!(cold.sealed_read_cache_usage(), cache_usage);
        let mut fresh = view(&commit, &vfs, &source_pool).await;
        let fresh_stats = fresh.buffer_stats();
        let cold_refusal = execute(&prepared, &mut fresh, &cx, zero_work, &spill_pool, 1, WORK)
            .await
            .unwrap_err();
        assert_eq!(
            format!("{warm_refusal:?}"),
            format!("{cold_refusal:?}"),
            "an argument-work refusal precedes either cache or source",
        );
        assert_eq!(fresh.buffer_stats(), fresh_stats);
        assert_eq!(fresh.sealed_read_cache_usage().entries, 0);
        drop(fresh);
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), baseline);
        for dimension in 0..6 {
            let mut kernel = memory();
            let mut image = sealing();
            let mut raw = RAW;
            let mut allowance = policy();
            let mut sort_work = WORK;
            match dimension {
                0 => kernel.max_kernel_workspace_bytes = 0,
                1 => kernel.max_result_bytes = 0,
                2 => image.max_image_bytes = 0,
                3 => raw = 0,
                4 => allowance = GqlQueryPolicy::new(first.records - 1, 100, WORK, WORK),
                5 => sort_work = first.sort_work - 1,
                _ => unreachable!(),
            }
            let limited = template
                .prepare_buffered_sealed_order(&params, R, kernel, image, raw)
                .unwrap();
            let error = execute(
                &limited,
                &mut cold,
                &cx,
                allowance,
                &spill_pool,
                1,
                sort_work,
            )
            .await
            .unwrap_err();
            if dimension == 5 {
                assert!(
                    matches!(error, NativeSpoolError::SortWorkLimit { .. }),
                    "{error:?}"
                );
            }
            assert_eq!(spill_pool.used(), 0, "dimension {dimension}");
            let recovered = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
                .await
                .unwrap();
            assert_eq!(recovered, first, "dimension {dimension}");
            assert_eq!(source_pool.used(), baseline);
        }
        let late = format!(
            "{BFS} YIELD distance \
                            RETURN 1/(2-distance) AS value ORDER BY value LIMIT 0"
        );
        let error = execute(
            &prepare(&late, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { error, .. }
            ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), baseline);
        let lazy = "CALL fnx.single_source_shortest_path_length(\
                    args => [1/0], relation => 'R', parallel => 'collapse') \
                    YIELD node,distance RETURN node,distance";
        let lazy_prepared = prepare(lazy, &GqlParameters::new());
        let faults = cold.buffer_stats();
        let error = execute(
            &lazy_prepared,
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { error, .. }
            ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}",
        );
        assert_eq!(
            cold.buffer_stats(),
            faults,
            "argument failure precedes source faults"
        );
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(source_pool.used(), baseline);
        assert_eq!(writer.frontier().unwrap(), CommitSeq(2));
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropping_a_suspended_second_procedure_refunds_both_children_and_live_rows() {
    let ((), report) = run_async_under_lab(0x6273_5104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _writer) = fixture(&commit).await;
        let params = parameters();
        let text = format!(
            "{BFS} YIELD node,distance {WCC} YIELD node AS member,component \
             RETURN node,distance,member,component ORDER BY node,member"
        );
        let mut expected = Vec::new();
        for row in bfs_rows() {
            for (member, component) in [
                (1, 1),
                (SECOND, 1),
                (THIRD, 1),
                (FOURTH, FOURTH),
                (u128::MAX, u128::MAX),
            ] {
                expected.push(vec![
                    row[0].clone(),
                    row[1].clone(),
                    vertex(member),
                    vertex(component),
                ]);
            }
        }
        let prepared = prepare(&text, &params);
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
                .await
                .unwrap()
                .rows,
            expected,
        );
        let baseline = source_pool.used();
        let retained = cold.sealed_read_cache_pool().clone();
        let retained_bytes = retained.used();
        let marker = GraphValueRow::from_owned_values(vec![vertex(1), vertex(1)])
            .canonical_bytes()
            .unwrap();
        let (mut scratch, a) = file(&cx, &spill_pool).await;
        let (mut destination, b) = file(&cx, &spill_pool).await;
        let (mut auxiliary, c) = file(&cx, &spill_pool).await;
        for backing in [&a, &b, &c] {
            backing.0.lock().unwrap().suspend_row = Some(marker.clone());
        }
        // Two vertex cells cannot match a BFS row's vertex/integer frame. This
        // marker therefore suspends the second source, after the first child
        // completed, while the component row and encoder guards remain live.
        let mut future = prepared.spool_in_view(
            &mut cold,
            &cx,
            policy(),
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            2,
            512,
            1,
            512,
            8 + marker.len(),
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
                    "the procedure-row append must be reached"
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
        assert_eq!(retained.used(), retained_bytes);
        // New query-private files can reuse the unchanged reader after dropping
        // the interrupted writer; no partial result handle has escaped.
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
                .await
                .unwrap()
                .rows,
            expected,
        );
        drop(cold);
        assert_eq!(source_pool.used(), 0);
        assert_eq!(retained.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn superseded_history_is_charged_before_compaction_even_under_limit_zero_and_reuse() {
    let ((), report) = run_async_under_lab(0x6273_5105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, mut writer) = fixture(&commit).await;
        let params = parameters();
        let text = format!("{BFS} YIELD node,distance RETURN node,distance LIMIT 0");
        let prepared = prepare(&text, &params);
        let old_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let current_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut old = view(&commit, &vfs, &old_pool).await;
        let before = execute(&prepared, &mut old, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert!(before.rows.is_empty());
        assert!(before.records > 0);
        // Sealing at SEQ 4 supersedes these older versions. Their authenticated
        // source records still cost admission before the compacted image exists.
        for (seq, value) in [(3, 101), (4, 102)] {
            let mut restatement = WriteBatch::new(R);
            restatement.set_edge_property(EId(BASE + 1), P, Some(CanonicalScalar::Int(value)));
            assert_eq!(
                writer.write(&commit, restatement).await.unwrap(),
                CommitSeq(seq)
            );
        }
        let mut current = view(&commit, &vfs, &current_pool).await;
        let after = execute(&prepared, &mut current, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(after.snapshot, CommitSeq(4));
        assert!(after.rows.is_empty());
        assert!(
            after.records > before.records,
            "raw superseded incidences were omitted"
        );
        assert!(after.evaluator_work > before.evaluator_work);
        let faults = current.buffer_stats();
        let warm = execute(&prepared, &mut current, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(warm, after);
        assert_eq!(current.buffer_stats(), faults);
        let error = execute(
            &prepared,
            &mut current,
            &cx,
            GqlQueryPolicy::new(before.records, 0, WORK, WORK),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        // The policy's source-record cap also bounds the existing FnX source
        // work allowance. That earlier admission may refuse before the raw
        // receipt reaches GQL; either typed path must preserve the original cap.
        let refused_original_cap = match &error {
            NativeSpoolError::Procedure(inner) => match inner.as_ref() {
                GqlQueryError::Rows(refusal) => {
                    refusal.dimension == GqlBudgetDimension::SnapshotRecords
                        && refusal.limit == before.records
                        && refusal.observed > refusal.limit
                }
                GqlQueryError::Source(GqlError::Procedure(ProcedureError::BufferedSealedRead(
                    inner,
                ))) => matches!(
                    inner.as_ref(),
                    FnxSealedReadError::Input(FnxReadError::SourceLimit {
                        resource: "work units",
                        limit,
                        requested,
                    }) if *limit == u128::from(before.records) && requested > limit
                ),
                _ => false,
            },
            _ => false,
        };
        assert!(refused_original_cap, "{error:?}");
        let visible = prepare(
            &format!("{BFS} YIELD node,distance RETURN node,distance ORDER BY node"),
            &params,
        );
        assert_eq!(
            execute(&visible, &mut current, &cx, policy(), &spill_pool, 1, WORK)
                .await
                .unwrap()
                .rows,
            bfs_rows(),
            "property restatements do not change this unit-weight graph answer",
        );
        assert_eq!(spill_pool.used(), 0);
        drop(old);
        drop(current);
        assert_eq!(old_pool.used(), 0);
        assert_eq!(current_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sealed_paths_share_limits_and_preserve_reached_errors_under_limit_zero() {
    let ((), report) = run_async_under_lab(0x6273_5111, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let (vfs, _) = fixture(&commit).await;
        let params = parameters();
        let source_pool = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
        let spill_pool = MemoryPool::new(1024 * 1024, 0).unwrap();
        let mut cold = view(&commit, &vfs, &source_pool).await;
        let text = format!(
            "{BFS} YIELD node MATCH route = WALK (node)-[:R*0..1]->(next:Person) \
             RETURN node,next,length(route) AS hops ORDER BY node,hops,next"
        );
        let prepared = prepare(&text, &params);
        let complete = execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
            .await
            .unwrap();
        assert_eq!(complete.rows.len(), 8);
        let exact = GqlQueryPolicy::new(
            complete.records,
            8,
            complete.evaluator_work,
            complete.evaluator_scratch,
        );
        assert_eq!(
            execute(
                &prepared,
                &mut cold,
                &cx,
                exact,
                &spill_pool,
                1,
                complete.sort_work,
            )
            .await
            .unwrap(),
            complete,
        );
        for dimension in 0..4 {
            let (allowance, work) = match dimension {
                0 => (
                    GqlQueryPolicy::new(complete.records - 1, 100, WORK, WORK),
                    WORK,
                ),
                1 => (
                    GqlQueryPolicy::new(10_000, 100, complete.evaluator_work - 1, WORK),
                    WORK,
                ),
                2 => (
                    GqlQueryPolicy::new(10_000, 100, WORK, complete.evaluator_scratch - 1),
                    WORK,
                ),
                3 => (policy(), complete.sort_work - 1),
                _ => unreachable!(),
            };
            assert!(
                execute(&prepared, &mut cold, &cx, allowance, &spill_pool, 1, work)
                    .await
                    .is_err(),
                "dimension {dimension}",
            );
            assert_eq!(spill_pool.used(), 0);
        }
        let late = format!(
            "{BFS} YIELD node MATCH route = WALK (node)-[:R*0..1]->(next:Person) \
             RETURN 1/(next.p-30) AS broken LIMIT 0"
        );
        let error = execute(
            &prepare(&late, &params),
            &mut cold,
            &cx,
            policy(),
            &spill_pool,
            1,
            WORK,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(&error, NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { error, .. }
            ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
            "{error:?}",
        );
        assert_eq!(spill_pool.used(), 0);
        assert_eq!(
            execute(&prepared, &mut cold, &cx, policy(), &spill_pool, 1, WORK)
                .await
                .unwrap(),
            complete,
        );
        drop(cold);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn explicit_owner_admission_does_not_broaden_default_or_terminal_aggregate_paths() {
    let params = parameters();
    let text = format!("{BFS} YIELD node,distance RETURN node,distance");
    let native = PreparedNativeRead::prepare(&text, &params, resolve).unwrap();
    assert!(native.prepare_buffered_order(&params).is_err());
    assert!(
        native
            .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
            .is_ok()
    );
    for text in [
        "MATCH (node) RETURN node".to_owned(),
        format!("{BFS} YIELD node RETURN COUNT(*) AS n"),
    ] {
        let params = if text.contains("$start") {
            parameters()
        } else {
            GqlParameters::new()
        };
        let native = PreparedNativeRead::prepare(&text, &params, resolve)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert!(
            native
                .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
                .is_err(),
            "{text}",
        );
    }
    let repeated =
        format!("({BFS} YIELD node RETURN node) UNION ALL ({WCC} YIELD node RETURN node)");
    let native = PreparedNativeRead::prepare(&repeated, &params, resolve).unwrap();
    assert!(native.prepare_buffered_order(&params).is_err());
    assert!(
        native
            .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
            .is_ok()
    );
    let composed = format!(
        "{BFS} YIELD node MATCH route = WALK (node)-[:R*0..2]->(next) \
         RETURN node,next,route"
    );
    let native = PreparedNativeRead::prepare(&composed, &params, resolve).unwrap();
    assert!(native.prepare_buffered_order(&params).is_err());
    assert!(
        native
            .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
            .is_ok()
    );
    let PreparedNativeRead::Set(template) = native else {
        panic!("procedure composition must retain its complete relation");
    };
    let bound = template.bind_parameters(&params).unwrap();
    use fgdb_gql::spill_set::AsyncSpillSetPlan;
    assert!(AsyncSpillSetPlan::compile(&bound).is_err());
    assert!(AsyncSpillSetPlan::compile_with_bounded_walks(&bound).is_err());
    assert!(AsyncSpillSetPlan::compile_with_single_procedure(&bound).is_err());
    assert!(AsyncSpillSetPlan::compile_with_independent_procedures(&bound).is_err());
    assert!(
        AsyncSpillSetPlan::compile_with_independent_procedures_and_bounded_walks(&bound).is_ok()
    );
    let empty = fgdb_gql::PreparedGraphSet::singleton();
    assert!(
        AsyncSpillSetPlan::compile_with_independent_procedures_and_bounded_walks(&empty).is_err()
    );
    for apply in [
        empty.clone().apply(bound.clone(), Vec::new()).unwrap(),
        empty.optional_apply(bound, Vec::new()).unwrap(),
    ] {
        assert!(
            AsyncSpillSetPlan::compile_with_independent_procedures_and_bounded_walks(
                &apply.with_page(0, Some(0)),
            )
            .is_err()
        );
    }
    // An empty bounded-path branch must not hide an invalid analytics shape.
    for second in [
        "CALL fnx.does_not_exist(relation => 'R') YIELD node RETURN node LIMIT 0",
        "CALL fnx.pagerank(relation => 'R',bogus => 1) YIELD node RETURN node LIMIT 0",
        "CALL fnx.weakly_connected_components({relations:['S']}) YIELD node RETURN node LIMIT 0",
    ] {
        let text = format!(
            "(MATCH route = WALK (a)-[:R*0..1]->(node) RETURN node LIMIT 0) \
             UNION ALL ({second})"
        );
        let native = PreparedNativeRead::prepare(&text, &GqlParameters::new(), resolve)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert!(
            native
                .prepare_buffered_sealed_order(&GqlParameters::new(), R, memory(), sealing(), RAW,)
                .is_err(),
            "{text}",
        );
    }
    // An empty first source never hides a statically inadmissible second CALL.
    for second in [
        "CALL fnx.does_not_exist(relation => 'R') YIELD node RETURN node LIMIT 0",
        "CALL fnx.pagerank(relation => 'R',bogus => 1) YIELD node RETURN node LIMIT 0",
        "CALL fnx.weakly_connected_components({relations:['S']}) YIELD node RETURN node LIMIT 0",
    ] {
        let text = format!("({BFS} YIELD node RETURN node LIMIT 0) UNION ALL ({second})");
        let native = PreparedNativeRead::prepare(&text, &params, resolve)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert!(native.prepare_buffered_order(&params).is_err());
        assert!(
            native
                .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
                .is_err(),
            "{text}",
        );
    }
    // These are complete ordinary CALL spellings. Static host admission must
    // reject their registered signature/output/projection mismatch before I/O.
    for text in [
        "CALL fnx.does_not_exist(relation => 'R') YIELD vertex RETURN vertex",
        "CALL fnx.single_source_shortest_path_length(relation => 'R') \
         YIELD vertex,distance RETURN vertex,distance",
        "CALL fnx.pagerank(relation => 'R',bogus => 1) \
         YIELD vertex,score RETURN vertex,score",
        "CALL fnx.triangles(relation => 'R') YIELD vertex,score RETURN vertex,score",
        "CALL fnx.pagerank(relation => 'R') YIELD vertex AS a,node AS b RETURN a,b",
        "CALL fnx.single_source_shortest_path_length(args => [1],relation => 'R') \
         YIELD vertex,distance MATCH (distance) RETURN vertex,distance",
        "CALL fnx.pagerank({relations:['S']}) YIELD vertex,score RETURN vertex,score",
        "CALL fnx.triangles({relations:['R'],direction:'directed'}) \
         YIELD vertex,triangles RETURN vertex,triangles",
    ] {
        let params = GqlParameters::new();
        let admitted = PreparedNativeRead::prepare(text, &params, resolve).is_ok_and(|native| {
            native
                .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW)
                .is_ok()
        });
        assert!(!admitted, "{text}");
    }
    for text in [
        "CALL fnx.pagerank(relation => 'R') \
         YIELD node AS person,score RETURN person,score",
        "CALL fnx.pagerank(relation => 'R') YIELD score,vertex RETURN score,vertex",
        "CALL fnx.pagerank(relation => 'R') YIELD score RETURN score",
        // Native evaluation of this valid argument shape remains deferred.
        "CALL fnx.single_source_shortest_path_length(\
         args => [1/0], relation => 'R') YIELD node,distance RETURN node,distance",
    ] {
        let params = GqlParameters::new();
        let native = PreparedNativeRead::prepare(text, &params, resolve)
            .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        assert!(
            native
                .prepare_buffered_sealed_order(&params, R, memory(), sealing(), RAW,)
                .is_ok(),
            "{text}",
        );
    }
}
