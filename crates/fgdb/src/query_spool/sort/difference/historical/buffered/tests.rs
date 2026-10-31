//! Cold checkpoint execution, complete relational children and independent bags.
use super::*;
use crate::{BufferLimits, BufferedReadLimits, DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::SpillLimits;
use fgdb_types::context::SimulationCheckpointProbe;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

#[derive(Default)]
struct State {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<State>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        let at = state.bytes.position() as usize;
        let count = out
            .remaining()
            .min(state.bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + count]);
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
        Poll::Ready(std::io::Write::write(&mut state.bytes, bytes))
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
            max_runs: 100_000,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap();
    (file, backing)
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 100_000_000, 100_000_000)
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        _ => None,
    }
}
fn plan(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), symbols).unwrap()
}
async fn cold(cx: &CommitCx, vfs: &MemVfs) -> BufferedReadView<MemVfs> {
    cold_with_extent(cx, vfs, 4096).await
}
async fn cold_with_extent(
    cx: &CommitCx,
    vfs: &MemVfs,
    max_extent_bytes: usize,
) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        cx,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        MemoryPool::new(8_000_000, 0).unwrap(),
        BufferedReadLimits {
            max_root_bytes: 65_536,
            max_source_bytes: 16_000_000,
            max_blocks: 512,
            max_vertex_patches: 512,
            max_work: 100_000_000,
            buffer: BufferLimits {
                max_frames: 2,
                max_ghost_entries: 4,
                max_extent_bytes,
            },
        },
    )
    .await
    .unwrap()
}
async fn history(cx: &CommitCx) -> (Database<MemVfs>, MemVfs, [CommitSeq; 3]) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut seed = WriteBatch::new(RelationId(1));
    for (id, p, q) in [(1, 3, 30), (2, 3, 10), (3, 7, 20)] {
        seed.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(q)),
            ],
        );
    }
    seed.add_edge(EId(1), VId(1), VId(3), vec![]);
    seed.add_edge(EId(2), VId(1), VId(3), vec![]);
    let first = db.write(cx, seed).await.unwrap();
    let mut edit = WriteBatch::new(RelationId(1));
    edit.delete_vertex(VId(2));
    edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
    edit.create_vertex(
        VId(4),
        vec![],
        vec![
            (PropertyKeyId(1), CanonicalScalar::Int(5)),
            (PropertyKeyId(2), CanonicalScalar::Int(40)),
        ],
    );
    edit.add_edge(EId(3), VId(1), VId(4), vec![]);
    let second = db.write(cx, edit).await.unwrap();
    let mut edit = WriteBatch::new(RelationId(1));
    edit.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
    let third = db.write(cx, edit).await.unwrap();
    (db, vfs, [first, second, third])
}

type Bag = BTreeMap<Vec<QueryValue>, i128>;
fn value(n: i64) -> Vec<QueryValue> {
    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
        n,
    )))]
}
async fn collect(cursor: &mut NativeSpoolDifference<'_, File>) -> VisitResult<Bag> {
    let mut result = Bag::new();
    while let Some((weight, bytes)) = cursor.next_change().await? {
        let row = GraphValueRow::decode_canonical(bytes.as_ref()).unwrap();
        let row = row
            .values()
            .iter()
            .cloned()
            .map(QueryValue::Value)
            .collect();
        assert_ne!(weight, 0);
        assert!(result.insert(row, weight).is_none());
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    Ok(result)
}
#[allow(clippy::too_many_arguments)]
async fn attempt(
    setup: &QueryCx,
    prepared: &PreparedNativeRead,
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    before: CommitSeq,
    after: CommitSeq,
    policy: GqlQueryPolicy,
    work: u64,
) -> VisitResult<(Bag, u64)> {
    let pool = MemoryPool::new(262_144, 0).unwrap();
    let (mut source, _) = file(setup, &pool).await;
    let (mut scratch, _) = file(setup, &pool).await;
    let (mut auxiliary, _) = file(setup, &pool).await;
    let mut cursor = prepared
        .spool_buffered_difference_in_view(
            view,
            cx,
            &GqlParameters::new(),
            before,
            after,
            policy,
            &mut source,
            &mut scratch,
            &mut auxiliary,
            2,
            128,
            2,
            10_000,
            257,
            8192,
            100_000,
            work,
            None,
        )
        .await?;
    let changes = collect(&mut cursor).await?;
    let work = cursor.work_units();
    drop(cursor);
    assert_eq!(pool.used(), 0);
    Ok((changes, work))
}

#[test]
fn complete_cold_relations_match_historical_queries_not_their_first_leaf() {
    let ((), report) = run_async_under_lab(0xd1ff_7101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (db, vfs, cuts) = history(&commit).await;
        let resident = db.read_session().unwrap();
        let mut view = cold(&commit, &vfs).await;
        for text in [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY n.q DESC SKIP 1 LIMIT 2",
            "MATCH (a)-[r:R]->(b) RETURN a.p AS a, b.p AS b",
            "MATCH route = WALK (a)-[:R*0..2]->(b) RETURN route",
            "MATCH route = TRAIL (a)-[:R*0..2]->(b) RETURN b.p AS p",
            "MATCH route = ACYCLIC (a)-[:R*0..2]->(b) RETURN DISTINCT b.p AS p",
            "MATCH route = SIMPLE (a)-[:R*0..2]->(b) WITH b.p AS p ORDER BY p DESC SKIP 1 LIMIT 3 RETURN p",
            "MATCH route = WALK (a)-[:R*0..2]->(b) WITH a.p AS p,COUNT(*) AS c RETURN p,c",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) RETURN n.p AS p UNION MATCH (m) RETURN m.p AS p",
            "MATCH (n) RETURN n.p AS p EXCEPT ALL MATCH (m) WHERE m.p > 4 RETURN m.p AS p",
            "MATCH (n) RETURN n.p AS p INTERSECT ALL MATCH (m) WHERE m.p > 4 RETURN m.p AS p",
            "MATCH (n) WITH n.p AS p ORDER BY p DESC LIMIT 2 UNWIND [1,2] AS x RETURN p + x AS v",
            "UNWIND [1,2] AS x MATCH (n) WHERE n.p > x RETURN n.p AS p, x",
            "MATCH (a) WITH a.p AS p MATCH (b) WHERE b.p = p RETURN p, b.p AS q",
            "MATCH (n) WITH n.p AS p, COUNT(*) AS c RETURN p, c",
            "UNWIND [3,1,3] AS x RETURN x ORDER BY x DESC LIMIT 2",
        ] {
            let prepared = plan(text);
            for before in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                for after in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                    let expected = prepared
                        .diff_in_view(
                            &resident,
                            &cx,
                            &GqlParameters::new(),
                            before,
                            after,
                            policy(100),
                        )
                        .unwrap();
                    let expected: Bag = expected
                        .changes()
                        .iter()
                        .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
                        .collect();
                    let actual = attempt(
                        &cx,
                        &prepared,
                        &mut view,
                        &cx,
                        before,
                        after,
                        policy(100),
                        u64::MAX,
                    )
                    .await
                    .unwrap()
                    .0;
                    assert_eq!(actual, expected, "{text} at {before:?} -> {after:?}");
                }
            }
        }
        let endpoints = plan("MATCH route = WALK (a)-[:R*0..2]->(b) RETURN b.p AS p");
        assert_eq!(
            attempt(
                &cx,
                &endpoints,
                &mut view,
                &cx,
                cuts[0],
                cuts[1],
                policy(3),
                u64::MAX,
            )
            .await
            .unwrap()
            .0,
            Bag::from([(value(3), -2), (value(5), 2), (value(9), 1)]),
        );
        let path_row = |start, steps: &[(u128, u128)]| {
            vec![QueryValue::Value(GraphValue::Path(
                fgdb_gql::algebra::GraphPath::new(
                    VId(start),
                    steps
                        .iter()
                        .map(|&(edge, target)| (EId(edge), VId(target)))
                        .collect::<Vec<_>>()
                        .into_boxed_slice(),
                ),
            ))]
        };
        let captures = plan("MATCH route = WALK (a)-[:R*0..2]->(b) RETURN route");
        assert_eq!(
            attempt(
                &cx,
                &captures,
                &mut view,
                &cx,
                cuts[0],
                cuts[1],
                policy(3),
                u64::MAX,
            )
            .await
            .unwrap()
            .0,
            Bag::from([
                (path_row(2, &[]), -1),
                (path_row(4, &[]), 1),
                (path_row(1, &[(3, 4)]), 1),
            ]),
        );
        let union = plan("MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p");
        assert_eq!(
            attempt(
                &cx,
                &union,
                &mut view,
                &cx,
                cuts[0],
                cuts[1],
                policy(3),
                u64::MAX
            )
            .await
            .unwrap()
            .0,
            Bag::from([(value(3), -4), (value(5), 2), (value(9), 2)])
        );
        let grouped = plan("MATCH (n) WITH n.p AS p, COUNT(*) AS c RETURN p, c");
        let group = |p, c| {
            [p, c]
                .into_iter()
                .map(|n| QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n))))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            attempt(
                &cx,
                &grouped,
                &mut view,
                &cx,
                cuts[0],
                cuts[1],
                policy(3),
                u64::MAX
            )
            .await
            .unwrap()
            .0,
            Bag::from([(group(3, 2), -1), (group(5, 1), 1), (group(9, 1), 1)])
        );
        assert_eq!(db.frontier().unwrap(), cuts[2]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn bound_arguments_and_cold_cut_outlive_writer_and_all_nonfinal_owners() {
    let ((), report) = run_async_under_lab(0xd1ff_7102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (mut db, vfs, cuts) = history(&commit).await;
        let mut view = cold(&commit, &vfs).await;
        let source_pool = view.memory_pool().clone();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut auxiliary, _) = file(&cx, &pool).await;
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n) WHERE n.p >= $floor RETURN n.p AS p",
            &params,
            symbols,
        )
        .unwrap();
        let rebound = GqlParameters::new().with_int64("floor", 4).unwrap();
        let future = prepared.spool_buffered_difference_in_view(
            &mut view,
            &cx,
            &rebound,
            cuts[0],
            cuts[1],
            policy(2),
            &mut source,
            &mut scratch,
            &mut auxiliary,
            2,
            16,
            2,
            100,
            257,
            8192,
            100,
            u64::MAX,
            None,
        );
        drop(prepared);
        drop(params);
        drop(rebound);
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(999)));
        db.write(&commit, edit).await.unwrap();
        let resident = Arc::downgrade(&db.snapshot);
        drop(db);
        assert!(
            resident.upgrade().is_none(),
            "cold execution must not pin a resident Snapshot"
        );
        let mut cursor = future.await.unwrap();
        drop(view);
        drop(source);
        drop(auxiliary);
        assert_eq!(source_pool.used(), 0);
        assert_eq!((cursor.before(), cursor.after()), (cuts[0], cuts[1]));
        assert_eq!(cursor.columns(), ["p"]);
        assert_eq!(
            collect(&mut cursor).await.unwrap(),
            Bag::from([(value(5), 1), (value(9), 1)])
        );
        drop(cursor);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_source_budgets_and_every_external_phase_share_their_original_allowance() {
    let ((), report) = run_async_under_lab(0xd1ff_7103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (db, vfs, cuts) = history(&commit).await;
        drop(db);
        let prepared = plan("MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p");
        let mut view = cold(&commit, &vfs).await;
        let endpoints = prepare(
            &prepared,
            &view,
            &cx,
            &GqlParameters::new(),
            cuts[0],
            cuts[1],
        )
        .unwrap();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut auxiliary, _) = file(&cx, &pool).await;
        let mut records = 0;
        let mut work = 0;
        let mut entries = 0;
        for endpoint in [&endpoints.before, &endpoints.after] {
            let (spool, _) = endpoint
                .spool_in_view(
                    &mut view,
                    &cx,
                    private_policy(policy(100)),
                    &mut scratch,
                    &mut source,
                    &mut auxiliary,
                    2,
                    128,
                    2,
                    10_000,
                    257,
                    8192,
                    100_000,
                    u64::MAX,
                )
                .await
                .unwrap();
            assert!(spool.row_stats().snapshot_records > 0);
            records += spool.row_stats().snapshot_records;
            work += spool.evaluator_stats().work_units;
            entries += spool.evaluator_stats().scratch_entries;
        }
        assert!(work > 0 && entries > 0);
        let exact = GqlQueryPolicy::new(records, 3, work, entries);
        let mut view = cold(&commit, &vfs).await;
        let (expected, physical) = attempt(
            &cx,
            &prepared,
            &mut view,
            &cx,
            cuts[0],
            cuts[1],
            exact,
            u64::MAX,
        )
        .await
        .unwrap();
        for (policy, physical) in [
            (GqlQueryPolicy::new(records - 1, 3, work, entries), physical),
            (GqlQueryPolicy::new(records, 3, work - 1, entries), physical),
            (GqlQueryPolicy::new(records, 3, work, entries - 1), physical),
            (GqlQueryPolicy::new(records, 2, work, entries), physical),
            (exact, physical - 1),
        ] {
            let mut view = cold(&commit, &vfs).await;
            assert!(
                attempt(
                    &cx, &prepared, &mut view, &cx, cuts[0], cuts[1], policy, physical
                )
                .await
                .is_err()
            );
        }
        let mut view = cold(&commit, &vfs).await;
        assert_eq!(
            attempt(
                &cx, &prepared, &mut view, &cx, cuts[0], cuts[1], exact, physical
            )
            .await
            .unwrap(),
            (expected, physical)
        );
        let mut view = cold(&commit, &vfs).await;
        assert!(
            attempt(
                &cx,
                &prepared,
                &mut view,
                &cx,
                cuts[1],
                cuts[1],
                policy(0),
                u64::MAX
            )
            .await
            .unwrap()
            .0
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn admission_precedes_binding_and_io_including_empty_and_source_free_results() {
    let ((), report) = run_async_under_lab(0xd1ff_7104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (mut db, vfs, cuts) = history(&commit).await;
        let mut view = cold(&commit, &vfs).await;
        let baseline = format!("{:?}", view.buffer_stats());
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut source, a) = file(&cx, &pool).await;
        let (mut scratch, b) = file(&cx, &pool).await;
        let (mut auxiliary, c) = file(&cx, &pool).await;
        let future = plan("RETURN 1 AS v").spool_buffered_difference_in_view(
            &mut view,
            &cx,
            &GqlParameters::new(),
            cuts[0],
            cuts[1],
            policy(0),
            &mut source,
            &mut scratch,
            &mut auxiliary,
            2,
            16,
            2,
            100,
            257,
            8192,
            100,
            u64::MAX,
            None,
        );
        drop(future);
        for text in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
            "MATCH (n) RETURN COUNT(*) AS n",
            "MATCH route = WALK (a)-[:R*]->(b) RETURN route LIMIT 0",
            "MATCH route = ANY SHORTEST (a)-[:R*0..2]->(b) RETURN route LIMIT 0",
            "MATCH route = WALK (a)-[:R*0..2]->(b)-[:R]->(c) RETURN route LIMIT 0",
        ] {
            let error = plan(text)
                .spool_buffered_difference_in_view(
                    &mut view,
                    &cx,
                    &GqlParameters::new(),
                    cuts[0],
                    cuts[1],
                    policy(0),
                    &mut source,
                    &mut scratch,
                    &mut auxiliary,
                    2,
                    16,
                    2,
                    100,
                    257,
                    8192,
                    100,
                    u64::MAX,
                    None,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Source(NativeSpoolError::Prepare(error))
                if matches!(*error, QueryError::StreamingUnsupported { .. }))
            );
        }
        let mut edit = WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(999)));
        let future_cut = db.write(&commit, edit).await.unwrap();
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n) WHERE n.p >= $floor RETURN n.p AS p LIMIT 0",
            &params,
            symbols,
        )
        .unwrap();
        // Missing binding must not mask an invalid endpoint, even now that
        // the writer has published that endpoint in a different generation.
        for (before, after) in [(future_cut, cuts[0]), (cuts[0], future_cut)] {
            let error = prepared
                .spool_buffered_difference_in_view(
                    &mut view,
                    &cx,
                    &GqlParameters::new(),
                    before,
                    after,
                    policy(0),
                    &mut source,
                    &mut scratch,
                    &mut auxiliary,
                    2,
                    16,
                    2,
                    100,
                    257,
                    8192,
                    100,
                    u64::MAX,
                    None,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                GqlQueryError::Source(NativeSpoolError::BufferedRead(_))
            ));
        }
        assert_eq!(format!("{:?}", view.buffer_stats()), baseline);
        for backing in [a, b, c] {
            let state = backing.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert_eq!(source.stats().reserved_runs, 0);
        assert_eq!(scratch.stats().reserved_runs, 0);
        assert_eq!(auxiliary.stats().reserved_runs, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_second_endpoint_failure_under_limit_zero_cannot_publish_a_difference() {
    let ((), report) = run_async_under_lab(0xd1ff_7105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (db, vfs, cuts) = history(&commit).await;
        drop(db);
        for (query, suffix) in [
            ("MATCH (n) RETURN 100 / (n.p - 9) AS x", ""),
            ("MATCH (n) RETURN 100 / (n.p - 9) AS x", " LIMIT 0"),
            (
                "MATCH route = WALK (a)-[:R*0..2]->(b) RETURN 100 / (b.p - 9) AS x",
                "",
            ),
            (
                "MATCH route = WALK (a)-[:R*0..2]->(b) RETURN 100 / (b.p - 9) AS x",
                " LIMIT 0",
            ),
        ] {
            let prepared = plan(&format!("{query}{suffix}"));
            let mut view = cold(&commit, &vfs).await;
            let pool = MemoryPool::new(262_144, 0).unwrap();
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            let (mut auxiliary, _) = file(&cx, &pool).await;
            assert!(
                prepared
                    .spool_buffered_difference_in_view(
                        &mut view,
                        &cx,
                        &GqlParameters::new(),
                        cuts[0],
                        cuts[1],
                        policy(0),
                        &mut source,
                        &mut scratch,
                        &mut auxiliary,
                        2,
                        16,
                        2,
                        100,
                        257,
                        8192,
                        100,
                        u64::MAX,
                        None,
                    )
                    .await
                    .is_err()
            );
            assert!(
                source.stats().published_runs > 0,
                "first endpoint must have completed privately"
            );
            assert_eq!(pool.used(), 0);
            // A fresh attempt on an error-free cut still works. File owners
            // from the failed attempt are not silently reset or reused.
            assert!(
                attempt(
                    &cx,
                    &prepared,
                    &mut view,
                    &cx,
                    cuts[0],
                    cuts[0],
                    policy(0),
                    u64::MAX
                )
                .await
                .unwrap()
                .0
                .is_empty()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn large_duplicate_inputs_use_cold_sources_and_return_one_compressed_change() {
    let ((), report) = run_async_under_lab(0xd1ff_7106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let payload = CanonicalScalar::ucs_basic_text(&"payload-é".repeat(120)).unwrap();
        let mut seed = WriteBatch::new(RelationId(1));
        for id in 0..128 {
            seed.create_vertex(VId(id), vec![], vec![(PropertyKeyId(2), payload.clone())]);
        }
        let before = db.write(&commit, seed).await.unwrap();
        let mut edit = WriteBatch::new(RelationId(1));
        for id in 0..32 {
            edit.delete_vertex(VId(id));
        }
        let after = db.write(&commit, edit).await.unwrap();
        let resident = Arc::downgrade(&db.snapshot);
        drop(db);
        assert!(resident.upgrade().is_none());
        let prepared = plan("MATCH (n) RETURN n.q AS q");
        let mut undersized = cold(&commit, &vfs).await;
        let source_pool = undersized.memory_pool().clone();
        let error = attempt(
            &cx,
            &prepared,
            &mut undersized,
            &cx,
            before,
            after,
            policy(1),
            u64::MAX,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(NativeSpoolError::BufferedExecute(error))
                if matches!(*error, GqlQueryError::Source(ScanError::Vertex(
                    fgdb_gql::stream::VertexScanError::Source(crate::BufferedReadError::Buffer(
                        fgdb_strata::tiered::buffer::BufferError::ExtentTooLarge { bytes, limit: 4096 }
                    ))
                )) if bytes > 4096)
        ));
        drop(undersized);
        assert_eq!(source_pool.used(), 0);
        // Native sealed objects are up to 16 KiB. Fund that actual source
        // frame while retaining the separate original 64 KiB spill pool and
        // all 128 duplicate rows; this does not enlarge any product limit.
        let mut view = cold_with_extent(&commit, &vfs, 16 * 1024).await;
        let pool = MemoryPool::new(65_536, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let (mut auxiliary, _) = file(&cx, &pool).await;
        let mut cursor = prepared
            .spool_buffered_difference_in_view(
                &mut view,
                &cx,
                &GqlParameters::new(),
                before,
                after,
                policy(1),
                &mut source,
                &mut scratch,
                &mut auxiliary,
                2,
                16,
                2,
                1000,
                257,
                8192,
                1000,
                u64::MAX,
                None,
            )
            .await
            .unwrap();
        assert!(source.stats().reserved_bytes > pool.limit() as u64);
        drop(source);
        drop(auxiliary);
        drop(view);
        assert_eq!(
            collect(&mut cursor).await.unwrap(),
            Bag::from([(vec![QueryValue::Value(GraphValue::Scalar(payload))], -32,)])
        );
        assert_eq!(cursor.changed_rows(), 1);
        drop(cursor);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_source_free_checkpoint_still_requires_complete_two_endpoint_execution() {
    let ((), report) = run_async_under_lab(0xd1ff_7107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (db, vfs, cuts) = history(&commit).await;
        drop(db);
        let prepared = plan("RETURN 1 AS value");
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        let mut view = cold(&commit, &vfs).await;
        assert!(
            attempt(
                &cx,
                &prepared,
                &mut view,
                &observed,
                cuts[0],
                cuts[1],
                policy(0),
                u64::MAX
            )
            .await
            .unwrap()
            .0
            .is_empty()
        );
        assert!(probe.calls() > 5);
        for stop in 1..=probe.calls() {
            let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
            let mut view = cold(&commit, &vfs).await;
            let result = attempt(
                &cx,
                &prepared,
                &mut view,
                &interrupted,
                cuts[0],
                cuts[1],
                policy(0),
                u64::MAX,
            )
            .await;
            assert!(result.is_err(), "checkpoint {stop}");
            assert_eq!(control.calls(), stop);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
