//! WITH -> governed native source -> authenticated external grouping -> result.
//! Capacity-one groups and small sort runs force partition/replay, not merely
//! the resident reducer under a different API. The arithmetic oracle below
//! independently enumerates vertices, parallel edges and two-hop occurrences.
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeAggregateSpool, NativeAggregateSpoolError,
    PreparedNativeRead, QueryResult, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateRow,
    GraphAggregateTextSlot, GraphAggregateValue, GraphExactAverage, GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalScalar, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::task::{Context, Poll};

const P: PropertyKeyId = PropertyKeyId(1);
const Q: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const L: LabelId = LabelId(1);

#[derive(Default)]
struct IoState {
    writes: AtomicUsize,
    fail: AtomicBool,
}
#[derive(Default)]
struct File {
    data: Cursor<Vec<u8>>,
    state: Arc<IoState>,
}
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let file = &mut self.get_mut().data;
        let at = file.position() as usize;
        let count = out.remaining().min(file.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&file.get_ref()[at..at + count]);
            file.set_position((at + count) as u64);
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
        let file = self.get_mut();
        file.state.writes.fetch_add(1, Ordering::SeqCst);
        if file.state.fail.load(Ordering::SeqCst) {
            return Poll::Ready(Err(io::Error::other("injected scratch write")));
        }
        Poll::Ready(std::io::Write::write(&mut file.data, bytes))
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
        Poll::Ready(self.get_mut().data.seek(at))
    }
}
struct Files {
    a: SpillFile<File>,
    b: SpillFile<File>,
    c: SpillFile<File>,
    partition_io: Arc<IoState>,
}
async fn scratch(cx: &QueryCx, pool: &MemoryPool) -> (SpillFile<File>, Arc<IoState>) {
    let file = File::default();
    let state = file.state.clone();
    let spill = SpillFile::new(
        cx,
        file,
        pool.clone(),
        SpillLimits {
            max_file_bytes: 128_000_000,
            max_runs: 8192,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap();
    (spill, state)
}
async fn files(cx: &QueryCx, pool: &MemoryPool) -> Files {
    let (a, _) = scratch(cx, pool).await;
    let (b, partition_io) = scratch(cx, pool).await;
    let (c, _) = scratch(cx, pool).await;
    Files {
        a,
        b,
        c,
        partition_io,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
fn policy(results: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, results, 100_000_000, 10_000_000)
}
fn parameters(floor: i64) -> GqlParameters {
    GqlParameters::new().with_int64("floor", floor).unwrap()
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(L)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
        _ => None,
    }
}
fn prepare(text: &str, args: &GqlParameters) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, args, resolve).unwrap()
}
fn prefix(kind: usize) -> &'static str {
    match kind {
        0 => "MATCH (n:L) WITH n.p AS p,n.q AS q",
        1 => "MATCH (a)-[r:R]->(b) WITH r.p AS p,r.q AS q",
        2 => "MATCH (a)-[r:R]->(x)-[s:R]->(b) WITH r.p AS p,r.q AS q",
        _ => unreachable!(),
    }
}
fn text(kind: usize) -> String {
    format!(
        "{} WHERE p>=$floor WITH q*2 AS value,77 AS unused,p/2 AS bucket \
        RETURN bucket,COUNT(*) AS n,SUM(value) AS total,AVG(value) AS average,\
        COUNT(DISTINCT value) AS support,SUM(DISTINCT value) AS unique_total \
        GROUP BY bucket ORDER BY total DESC NULLS LAST,bucket ASC",
        prefix(kind)
    )
}
fn properties(p: Option<i64>, q: Option<i64>) -> Vec<(PropertyKeyId, CanonicalScalar)> {
    let mut properties = Vec::new();
    if let Some(p) = p {
        properties.push((P, CanonicalScalar::Int(p)));
    }
    if let Some(q) = q {
        properties.push((Q, CanonicalScalar::Int(q)));
    }
    properties
}

type Vertex = (VId, Option<i64>, Option<i64>);
type Edge = (EId, VId, VId, Option<i64>, Option<i64>);
#[derive(Clone)]
struct Fixture {
    vertices: Vec<Vertex>,
    edges: Vec<Edge>,
}
impl Fixture {
    fn new() -> Self {
        let mut fixture = Self {
            vertices: Vec::new(),
            edges: Vec::new(),
        };
        for id in 0..48_u128 {
            let p = (id % 11 != 0).then_some((id % 9) as i64 - 2);
            let q = (id % 5 != 0).then_some(i64::MAX / 2 - (id % 7) as i64);
            fixture.vertices.push((VId(id), p, q));
            fixture.edges.push((EId(id), VId(id), VId(id + 1), p, q));
            if id % 4 == 0 {
                fixture
                    .edges
                    .push((EId(1000 + id), VId(id), VId(id + 1), p, q));
            }
        }
        fixture
    }
    fn batch(&self) -> WriteBatch {
        let mut batch = WriteBatch::new(R);
        for &(id, p, q) in &self.vertices {
            batch.create_vertex(id, vec![L], properties(p, q));
        }
        batch.create_vertex(VId(48), vec![], vec![]);
        for &(id, a, b, p, q) in &self.edges {
            batch.add_edge(id, a, b, properties(p, q));
        }
        batch
    }
    fn occurrences(&self, kind: usize) -> Vec<(Option<i64>, Option<i64>)> {
        match kind {
            0 => self.vertices.iter().map(|&(_, p, q)| (p, q)).collect(),
            1 => self.edges.iter().map(|&(_, _, _, p, q)| (p, q)).collect(),
            2 => self
                .edges
                .iter()
                .flat_map(|&(id, _, to, p, q)| {
                    self.edges.iter().filter_map(move |&(next, from, _, _, _)| {
                        (id != next && to == from).then_some((p, q))
                    })
                })
                .collect(),
            _ => unreachable!(),
        }
    }
    fn expected(&self, kind: usize, floor: i64) -> Vec<Vec<GraphAggregateValue>> {
        let mut groups = BTreeMap::<i64, Vec<Option<i64>>>::new();
        for (p, q) in self.occurrences(kind) {
            if let Some(p) = p.filter(|p| *p >= floor) {
                groups.entry(p / 2).or_default().push(q.map(|q| q * 2));
            }
        }
        let mut ranked = Vec::new();
        for (key, values) in groups {
            let count = values.len() as u64;
            let nonnull: Vec<_> = values.into_iter().flatten().collect();
            let total =
                (!nonnull.is_empty()).then(|| nonnull.iter().map(|v| i128::from(*v)).sum::<i128>());
            let support: BTreeSet<_> = nonnull.iter().copied().collect();
            let unique =
                (!support.is_empty()).then(|| support.iter().map(|v| i128::from(*v)).sum::<i128>());
            let null = || GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null));
            let row = vec![
                GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(key))),
                GraphAggregateValue::Count(count),
                total.map_or_else(null, GraphAggregateValue::Integer),
                total.map_or_else(null, |sum| {
                    GraphAggregateValue::Average(
                        GraphExactAverage::new(sum, nonnull.len() as u64).unwrap(),
                    )
                }),
                GraphAggregateValue::Count(support.len() as u64),
                unique.map_or_else(null, GraphAggregateValue::Integer),
            ];
            ranked.push((key, total, row));
        }
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.into_iter().map(|(_, _, row)| row).collect()
    }
}
fn flatten(row: &GraphAggregateRow, slots: &[GraphAggregateTextSlot]) -> Vec<GraphAggregateValue> {
    slots
        .iter()
        .map(|slot| match slot {
            GraphAggregateTextSlot::GroupKey(at) => {
                GraphAggregateValue::Value(row.keys()[*at].clone())
            }
            GraphAggregateTextSlot::Aggregate(at) => row.values()[*at].clone(),
        })
        .collect()
}
async fn read(
    cx: &QueryCx,
    spool: &NativeAggregateSpool,
    file: &mut SpillFile<File>,
) -> Vec<Vec<GraphAggregateValue>> {
    let mut reader = spool.reader(file, None);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push(flatten(&row, spool.output_slots()));
    }
    assert!(reader.next_row(cx).await.unwrap().is_none());
    rows
}

// Return the actual future so tests can change/compact/reopen the database
// AFTER source opening but before the first private occurrence is demanded.
macro_rules! spool {
    ($prepared:expr, $view:expr, $cx:expr, $args:expr, $policy:expr, $files:ident, $groups:expr, $runs:expr, $inputs:expr) => {
        ($prepared).spool_aggregate_in_view(
            $view,
            $cx,
            $args,
            $policy,
            &mut $files.a,
            &mut $files.b,
            &mut $files.c,
            $groups,
            256,
            $runs,
            128,
            257,
            16_384,
            $inputs,
            100_000_000,
            None,
        )
    };
}

#[test]
fn with_spill_matches_independent_occurrences_batch_and_resident_across_layouts_and_pages() {
    let ((), report) = run_async_under_lab(0x5749_5350, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let fixture = Fixture::new();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, fixture.batch()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = parameters(0);
        for kind in 0..3 {
            let expected = fixture.expected(kind, 0);
            assert!(expected.len() > 1);
            for (suffix, skip, take) in [
                ("", 0, usize::MAX),
                (" SKIP 1 LIMIT 2", 1, 2),
                (" LIMIT 0", 0, 0),
                (" SKIP 1000 LIMIT 1", 1000, 1),
            ] {
                let statement = format!("{}{suffix}", text(kind));
                let prepared = prepare(&statement, &args);
                let expected: Vec<_> = expected.iter().skip(skip).take(take).cloned().collect();
                let QueryResult::Rows { columns, rows } =
                    prepared.execute(&db, &cx, &args, policy(100)).unwrap()
                else {
                    panic!("read");
                };
                assert_eq!(rows, expected, "batch: {statement}");
                let mut resident = prepared
                    .stream_aggregate_in_view(&view, &cx, &args, policy(100))
                    .unwrap();
                let slots = resident.output_slots().to_vec();
                let rows: Vec<_> = resident
                    .by_ref()
                    .map(|row| flatten(&row.unwrap(), &slots))
                    .collect();
                assert_eq!(rows, expected, "resident: {statement}");
                for (capacity, run_rows) in [(1, 1), (3, 3)] {
                    let pool = MemoryPool::new(1_000_000, 0).unwrap();
                    let mut io = files(&cx, &pool).await;
                    let before = io.partition_io.writes.load(Ordering::SeqCst);
                    let (spool, _) = spool!(
                        &prepared,
                        &view,
                        &cx,
                        &args,
                        policy(expected.len() as u64),
                        io,
                        capacity,
                        run_rows,
                        10_000
                    )
                    .await
                    .unwrap_or_else(|error| panic!("{statement}: {error}"));
                    assert_eq!(spool.columns(), columns);
                    assert_eq!(spool.row_count(), expected.len() as u64);
                    assert_eq!(spool.row_stats().result_rows, expected.len() as u64);
                    assert_eq!(pool.used(), 0);
                    if capacity == 1 {
                        assert!(
                            io.partition_io.writes.load(Ordering::SeqCst) > before,
                            "partition file was not exercised: {statement}"
                        );
                    }
                    assert_eq!(
                        read(&cx, &spool, &mut io.c).await,
                        expected,
                        "spill: {statement}"
                    );
                    assert_eq!(pool.used(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn prepared_spill_keeps_opened_and_pinned_history_across_edits_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0x5749_5351, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for kind in 0..3 {
            let fixture = Fixture::new();
            let vfs = MemVfs::new().unwrap();
            let path = vfs.database_dir();
            let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
                .await
                .unwrap();
            let basis = db.write(&commit, fixture.batch()).await.unwrap();
            let pin = db.read_session().unwrap();
            let args = parameters(0);
            let prepared = prepare(&text(kind), &args);
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let mut io = files(&cx, &pool).await;
            let pending = spool!(&prepared, &pin, &cx, &args, policy(100), io, 1, 2, 10_000);
            fn send(_: &impl Send) {}
            send(&pending);
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(3), Q, Some(CanonicalScalar::Int(2)));
            edit.set_edge_property(EId(3), Q, Some(CanonicalScalar::Int(2)));
            edit.delete_edge(EId(4));
            db.write(&commit, edit).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            let db = Database::open_with_vfs(&commit, vfs, &path, keys())
                .await
                .unwrap();
            let (old, _) = pending.await.unwrap();
            assert_eq!(old.snapshot_seq(), basis);
            assert_eq!(read(&cx, &old, &mut io.c).await, fixture.expected(kind, 0));
            assert_eq!(pool.used(), 0);
            let mut changed = fixture.clone();
            changed
                .vertices
                .iter_mut()
                .find(|v| v.0 == VId(3))
                .unwrap()
                .2 = Some(2);
            changed.edges.iter_mut().find(|e| e.0 == EId(3)).unwrap().4 = Some(2);
            changed.edges.retain(|e| e.0 != EId(4));
            assert_ne!(changed.expected(kind, 0), fixture.expected(kind, 0));
            let current = db.read_session().unwrap();
            for floor in [0, 4, 0, 99] {
                let args = parameters(floor);
                let mut io = files(&cx, &pool).await;
                let (spool, _) = spool!(
                    &prepared,
                    &current,
                    &cx,
                    &args,
                    policy(100),
                    io,
                    1,
                    2,
                    10_000
                )
                .await
                .unwrap();
                assert_eq!(
                    read(&cx, &spool, &mut io.c).await,
                    changed.expected(kind, floor)
                );
                assert_eq!(pool.used(), 0);
            }
            let mut io = files(&cx, &pool).await;
            let (old, _) = spool!(&prepared, &pin, &cx, &args, policy(100), io, 1, 2, 10_000)
                .await
                .unwrap();
            assert_eq!(read(&cx, &old, &mut io.c).await, fixture.expected(kind, 0));
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_with_errors_and_scratch_write_failure_publish_no_handle_and_release_all_grants() {
    let ((), report) = run_async_under_lab(0x5749_5352, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..17_u128 {
            batch.create_vertex(
                VId(id),
                vec![L],
                properties(Some(if id == 16 { 0 } else { 1 }), None),
            );
            if id != 0 {
                batch.add_edge(
                    EId(id),
                    VId(id - 1),
                    VId(id),
                    properties(Some(if id == 16 { 0 } else { 1 }), None),
                );
            }
        }
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        for kind in 0..2 {
            for tail in [
                "WITH 12/p AS unused WHERE FALSE RETURN COUNT(*) AS n",
                "WHERE p>=0 WITH 12/p AS unused RETURN COUNT(*) AS n LIMIT 0",
            ] {
                let prepared = prepare(&format!("{} {tail}", prefix(kind)), &args);
                let pool = MemoryPool::new(1_000_000, 0).unwrap();
                let mut io = files(&cx, &pool).await;
                let result =
                    spool!(&prepared, &view, &cx, &args, policy(1), io, 1, 1, 10_000).await;
                assert!(
                    matches!(result, Err(NativeAggregateSpoolError::Execute(error))
                    if matches!(error.as_ref(), GqlQueryError::Source(GraphAggregateError::InputRelation(_))))
                );
                assert_eq!(pool.used(), 0);
            }
        }
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, Fixture::new().batch()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = parameters(0);
        let prepared = prepare(&text(0), &args);
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut io = files(&cx, &pool).await;
        let before = io.partition_io.writes.load(Ordering::SeqCst);
        io.partition_io.fail.store(true, Ordering::SeqCst);
        let result = spool!(&prepared, &view, &cx, &args, policy(100), io, 1, 1, 10_000).await;
        assert!(matches!(result, Err(NativeAggregateSpoolError::Spool(_))));
        assert!(io.partition_io.writes.load(Ordering::SeqCst) > before);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn native_with_spill_uses_one_cumulative_budget_through_replay_and_delivery() {
    let ((), report) = run_async_under_lab(0x5749_5353, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, Fixture::new().batch()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = parameters(0);
        for kind in 0..2 {
            let prepared = prepare(
                &format!(
                    "{} WHERE p>=$floor WITH q*2 AS value RETURN COUNT(*) AS n,SUM(value) AS total",
                    prefix(kind)
                ),
                &args,
            );
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let mut io = files(&cx, &pool).await;
            let (full, _) = spool!(&prepared, &view, &cx, &args, policy(1), io, 1, 2, 10_000)
                .await
                .unwrap();
            let expected = read(&cx, &full, &mut io.c).await;
            let rows = full.row_stats();
            let stats = full.evaluator_stats();
            let exact = GqlQueryPolicy::new(
                rows.snapshot_records,
                rows.result_rows,
                stats.work_units,
                stats.scratch_entries,
            );
            for (limits, succeeds) in [
                (exact, true),
                (
                    GqlQueryPolicy::new(rows.snapshot_records - 1, u64::MAX, u64::MAX, u64::MAX),
                    false,
                ),
                (
                    GqlQueryPolicy::new(u64::MAX, rows.result_rows - 1, u64::MAX, u64::MAX),
                    false,
                ),
                (
                    GqlQueryPolicy::new(u64::MAX, u64::MAX, stats.work_units - 1, u64::MAX),
                    false,
                ),
                (
                    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, stats.scratch_entries - 1),
                    false,
                ),
            ] {
                let mut io = files(&cx, &pool).await;
                let result = spool!(&prepared, &view, &cx, &args, limits, io, 1, 2, 10_000).await;
                assert_eq!(result.is_ok(), succeeds);
                if let Ok((spool, _)) = result {
                    assert_eq!(spool.row_stats(), rows);
                    assert_eq!(spool.evaluator_stats(), stats);
                    assert_eq!(read(&cx, &spool, &mut io.c).await, expected);
                }
                assert_eq!(pool.used(), 0);
            }
            let mut io = files(&cx, &pool).await;
            let result = spool!(&prepared, &view, &cx, &args, policy(1), io, 1, 2, 1).await;
            assert!(matches!(
                result,
                Err(NativeAggregateSpoolError::InputRows { .. })
            ));
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_with_groups_and_argument_support_do_not_spend_the_single_output_grant() {
    let ((), report) = run_async_under_lab(0x5749_5354, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..1024_u128 {
            batch.create_vertex(
                VId(id),
                vec![L],
                properties(Some(id as i64), Some(i64::MAX / 2)),
            );
        }
        db.write(&commit, batch).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let prepared = prepare(
            "MATCH (n:L) WITH n.p AS p,n.q AS q WHERE p>=0 \
            WITH p/32 AS bucket,q*2 AS value RETURN DISTINCT COUNT(*) AS n,SUM(value) AS total,\
            COUNT(DISTINCT value) AS support GROUP BY bucket ORDER BY total DESC LIMIT 1",
            &args,
        );
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut io = files(&cx, &pool).await;
        let before = io.partition_io.writes.load(Ordering::SeqCst);
        let (spool, _) = spool!(
            &prepared,
            &view,
            &cx,
            &args,
            GqlQueryPolicy::new(1024, 1, 100_000_000, 10_000_000),
            io,
            1,
            2,
            1024
        )
        .await
        .unwrap();
        assert_eq!(spool.row_count(), 1);
        assert_eq!(spool.row_stats().snapshot_records, 1024);
        assert_eq!(spool.row_stats().result_rows, 1);
        assert!(io.partition_io.writes.load(Ordering::SeqCst) > before);
        assert_eq!(pool.used(), 0);
        assert_eq!(
            read(&cx, &spool, &mut io.c).await,
            vec![vec![
                GraphAggregateValue::Count(32),
                GraphAggregateValue::Integer(32 * i128::from(i64::MAX - 1)),
                GraphAggregateValue::Count(1),
            ]]
        );
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod buffered_elements {
    use super::*;
    use fgdb::{
        BufferLimits, BufferedReadLimits, BufferedReadView, NativeResultSpool, NativeSpoolError,
        PreparedBufferedOrder,
    };
    use fgdb_gql::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValueRow};
    use fgdb_gql::{
        GlaLimitDimension, GqlBudgetDimension, GraphIntegerErrorKind, GraphSetExecutionError,
        GraphSymbolResolver, ReverseSymbolCatalog,
    };
    use fgdb_types::{CommitCx, CommitSeq};

    const PAYLOAD: usize = 513;

    fn scalar(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }
    fn payload() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::bytes(vec![0xa5; PAYLOAD]).unwrap())
    }

    async fn fixture(cx: &CommitCx) -> (Database<MemVfs>, MemVfs) {
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(R);
        for (vid, p) in [(VId(1), Some(2)), (VId(2), None), (VId(u128::MAX), Some(7))] {
            let mut props = properties(p, None);
            props.push((Q, CanonicalScalar::bytes(vec![0xa5; PAYLOAD]).unwrap()));
            batch.create_vertex(vid, vec![L], props);
        }
        for (eid, from, to, value) in [
            (EId(11), VId(1), VId(u128::MAX), 11),
            (EId(12), VId(1), VId(u128::MAX), 17),
            (EId(u128::MAX), VId(u128::MAX), VId(2), 19),
        ] {
            batch.add_edge(eid, from, to, properties(Some(value), None));
        }
        assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
        (db, vfs)
    }

    async fn cold(cx: &CommitCx, vfs: &MemVfs, pool: &MemoryPool) -> BufferedReadView<MemVfs> {
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
                max_work: 100_000_000,
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

    async fn execute(
        prepared: &PreparedBufferedOrder,
        view: &mut BufferedReadView<MemVfs>,
        cx: &QueryCx,
        limits: GqlQueryPolicy,
        io: &mut Files,
    ) -> Result<(NativeResultSpool, u64), NativeSpoolError> {
        prepared
            .spool_in_view(
                view,
                cx,
                limits,
                &mut io.a,
                &mut io.b,
                &mut io.c,
                1,
                512,
                1,
                512,
                257,
                16 * 1024,
                100,
                100_000_000,
            )
            .await
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
        assert!(reader.next_row(cx).await.unwrap().is_none());
        rows
    }

    struct MetadataCatalog;

    impl GraphSymbolResolver for MetadataCatalog {
        fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
            resolve(kind, name)
        }

        fn reverse_catalog(&self) -> Option<ReverseSymbolCatalog> {
            let mut catalog = ReverseSymbolCatalog::new();
            catalog.insert_label(L, "L");
            catalog.insert_relation(R, "R");
            catalog.insert_property(P, "p");
            catalog.insert_property(Q, "q");
            Some(catalog)
        }
    }

    fn assert_record_refusal(error: NativeSpoolError, limit: u64) {
        assert!(
            matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                if matches!(**inner, GqlQueryError::Rows(ref exceeded)
                    if exceeded.dimension == GqlBudgetDimension::SnapshotRecords
                        && exceeded.limit == limit && exceeded.observed == limit + 1)),
            "{error:?}"
        );
    }

    #[test]
    fn cold_element_reads_admit_records_before_io_and_publish_exact_usage() {
        let ((), report) = run_async_under_lab(0x5749_5363, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let (_db, vfs) = fixture(&commit).await;
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
            let mut view = cold(&commit, &vfs, &source_pool).await;
            let text_value =
                |name: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
            let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
            for (identity, expression, expected) in [
                (
                    GraphValue::Vertex(VId(u128::MAX)),
                    "labels(x)",
                    list(vec![text_value("L")]),
                ),
                (GraphValue::Edge(EId(u128::MAX)), "type(x)", text_value("R")),
                (
                    GraphValue::Vertex(VId(u128::MAX)),
                    "keys(x)",
                    list(vec![text_value("p"), text_value("q")]),
                ),
                (
                    GraphValue::Vertex(VId(u128::MAX)),
                    "properties(x)",
                    GraphValue::map(vec![("p".into(), scalar(7)), ("q".into(), payload())])
                        .unwrap(),
                ),
                (GraphValue::Vertex(VId(u128::MAX)), "x.p", scalar(7)),
                (GraphValue::Edge(EId(u128::MAX)), "x.p", scalar(19)),
                (GraphValue::Vertex(VId(999)), "labels(x)", null()),
                (GraphValue::Edge(EId(999)), "x.p", null()),
            ] {
                let args = GqlParameters::new()
                    .with_list("refs", vec![identity.clone()])
                    .unwrap();
                let text = format!(
                    "UNWIND $refs AS x RETURN x AS owner,{expression} AS value ORDER BY owner"
                );
                let prepared = PreparedNativeRead::prepare(&text, &args, MetadataCatalog)
                    .unwrap()
                    .prepare_buffered_order(&args)
                    .unwrap();
                let mut io = files(&cx, &spill_pool).await;
                let source_stats = view.buffer_stats();
                let source_bytes = source_pool.used();
                let error = execute(
                    &prepared,
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(0, 1, 100_000_000, 10_000_000),
                    &mut io,
                )
                .await
                .unwrap_err();
                assert_record_refusal(error, 0);
                // Values and UNWIND can write scratch, but a refused graph
                // identity must not touch even a cached source extent.
                assert_eq!(view.buffer_stats(), source_stats, "{text}");
                assert_eq!(source_pool.used(), source_bytes, "{text}");
                assert_eq!(spill_pool.used(), 0, "{text}");

                let mut io = files(&cx, &spill_pool).await;
                let (spool, _) = execute(
                    &prepared,
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(1, 1, 100_000_000, 10_000_000),
                    &mut io,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(spool.row_stats().snapshot_records, 1, "{text}");
                assert_eq!(spool.row_count(), 1, "{text}");
                assert_eq!(
                    contents(&spool, &mut io.b, &cx).await,
                    vec![vec![identity, expected]],
                    "{text}"
                );
                assert_eq!(spill_pool.used(), 0, "{text}");
            }
            drop(view);
            assert_eq!(source_pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn cold_element_record_allowance_spans_stages_repeated_ids_and_graph_input() {
        let ((), report) = run_async_under_lab(0x5749_5364, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let (_db, vfs) = fixture(&commit).await;
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
            let mut view = cold(&commit, &vfs, &source_pool).await;
            let text_value =
                |name: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
            let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
            let labels = list(vec![text_value("L")]);
            let keys = list(vec![text_value("p"), text_value("q")]);
            let args = GqlParameters::new()
                .with_list("refs", vec![GraphValue::Vertex(VId(u128::MAX)); 2])
                .unwrap();
            for (text, args, required, refusals, expected) in [
                (
                    "UNWIND $refs AS x \
                    WITH x,labels(x) AS ls ORDER BY x LIMIT 2 \
                    WITH x,ls,keys(x) AS ks ORDER BY x LIMIT 2 \
                    RETURN x AS owner,ls,ks,x.p AS value ORDER BY owner",
                    args,
                    6,
                    vec![2, 4, 5],
                    vec![
                        vec![
                            GraphValue::Vertex(VId(u128::MAX)),
                            labels.clone(),
                            keys,
                            scalar(7),
                        ];
                        2
                    ],
                ),
                (
                    "MATCH (n:L) WITH {owner:n} AS state \
                    RETURN state.owner AS owner,labels(state.owner) AS ls,\
                    state.owner.p AS value ORDER BY owner",
                    GqlParameters::new(),
                    9,
                    vec![3, 8],
                    vec![
                        vec![GraphValue::Vertex(VId(1)), labels.clone(), scalar(2)],
                        vec![GraphValue::Vertex(VId(2)), labels.clone(), null()],
                        vec![GraphValue::Vertex(VId(u128::MAX)), labels, scalar(7)],
                    ],
                ),
            ] {
                let prepared = PreparedNativeRead::prepare(text, &args, MetadataCatalog)
                    .unwrap()
                    .prepare_buffered_order(&args)
                    .unwrap();
                for limit in refusals {
                    let mut io = files(&cx, &spill_pool).await;
                    let error = execute(
                        &prepared,
                        &mut view,
                        &cx,
                        GqlQueryPolicy::new(limit, 100, 100_000_000, 10_000_000),
                        &mut io,
                    )
                    .await
                    .unwrap_err();
                    assert_record_refusal(error, limit);
                    assert_eq!(spill_pool.used(), 0, "{text}");
                }
                let mut io = files(&cx, &spill_pool).await;
                let (spool, _) = execute(
                    &prepared,
                    &mut view,
                    &cx,
                    GqlQueryPolicy::new(required, 100, 100_000_000, 10_000_000),
                    &mut io,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                assert_eq!(spool.row_stats().snapshot_records, required, "{text}");
                assert_eq!(contents(&spool, &mut io.b, &cx).await, expected, "{text}");
                assert_eq!(spill_pool.used(), 0, "{text}");
            }
            drop(view);
            assert_eq!(source_pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn spilled_dynamic_metadata_matches_resident_values_and_keeps_the_opened_snapshot() {
        let ((), report) = run_async_under_lab(0x5749_5362, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let (mut db, vfs) = fixture(&commit).await;
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
            let mut view = cold(&commit, &vfs, &source_pool).await;
            let args = GqlParameters::new();
            let text = "MATCH p=(a:L)-[r:R]->(b:L) \
                WITH r AS edge,nodes(p) AS ns,relationships(p) AS rs \
                RETURN edge AS owner,type(rs[0]) AS kind,properties(rs[0]) AS props,\
                keys(rs[0]) AS keys,[n IN ns | labels(n)] AS labels,\
                [n IN ns | properties(n)] AS nodes ORDER BY owner";
            let text_value =
                |name: &str| GraphValue::Scalar(CanonicalScalar::ucs_basic_text(name).unwrap());
            let list = |values: Vec<_>| GraphValue::List(values.into_boxed_slice());
            let node = |p: Option<i64>| {
                let mut fields = vec![("q".into(), payload())];
                if let Some(p) = p {
                    fields.push(("p".into(), scalar(p)));
                }
                GraphValue::map(fields).unwrap()
            };
            let expected: Vec<_> = [
                (EId(11), 11, Some(2), Some(7)),
                (EId(12), 17, Some(2), Some(7)),
                (EId(u128::MAX), 19, Some(7), None),
            ]
            .into_iter()
            .map(|(id, weight, from, to)| {
                vec![
                    GraphValue::Edge(id),
                    text_value("R"),
                    GraphValue::map(vec![("p".into(), scalar(weight))]).unwrap(),
                    list(vec![text_value("p")]),
                    list(vec![
                        list(vec![text_value("L")]),
                        list(vec![text_value("L")]),
                    ]),
                    list(vec![node(from), node(to)]),
                ]
            })
            .collect();
            let native = PreparedNativeRead::prepare(text, &args, MetadataCatalog).unwrap();
            let QueryResult::Rows { rows, .. } =
                native.execute(&db, &cx, &args, policy(100)).unwrap()
            else {
                panic!("resident metadata rows");
            };
            let actual: Vec<Vec<_>> = rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|value| match value {
                            GraphAggregateValue::Value(value) => value,
                            value => panic!("unexpected metadata cell {value:?}"),
                        })
                        .collect()
                })
                .collect();
            assert_eq!(actual, expected);
            let prepared = native.prepare_buffered_order(&args).unwrap();
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(70)));
            edit.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Int(190)));
            assert_eq!(db.write(&commit, edit).await.unwrap(), CommitSeq(2));
            db.compact(&commit).await.unwrap();
            let mut io = files(&cx, &spill_pool).await;
            let (spool, _) = execute(&prepared, &mut view, &cx, policy(100), &mut io)
                .await
                .unwrap();
            assert_eq!(spool.snapshot_seq(), CommitSeq(1));
            assert_eq!(spool.row_count(), 3);
            assert_eq!(contents(&spool, &mut io.b, &cx).await, expected);
            assert!(io.partition_io.writes.load(Ordering::SeqCst) > 0);
            assert_eq!(spill_pool.used(), 0);
            drop(view);
            assert_eq!(source_pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn cold_carried_elements_use_the_opened_cut_after_edits_and_compaction() {
        let ((), report) = run_async_under_lab(0x5749_5360, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let (mut db, vfs) = fixture(&commit).await;
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
            let mut view = cold(&commit, &vfs, &source_pool).await;
            let args = GqlParameters::new();
            let cases = [
                (
                    "MATCH (n:L) WITH {owner:n,items:[n,n]} AS state RETURN state.owner AS owner,[x IN state.items | x.p] AS values ORDER BY owner",
                    vec![
                        vec![
                            GraphValue::Vertex(VId(1)),
                            GraphValue::List(vec![scalar(2), scalar(2)].into_boxed_slice()),
                        ],
                        vec![
                            GraphValue::Vertex(VId(2)),
                            GraphValue::List(vec![null(), null()].into_boxed_slice()),
                        ],
                        vec![
                            GraphValue::Vertex(VId(u128::MAX)),
                            GraphValue::List(vec![scalar(7), scalar(7)].into_boxed_slice()),
                        ],
                    ],
                ),
                (
                    "MATCH (a)-[r:R]->(b) WITH {owner:r,items:[a,b,a]} AS state RETURN state.owner AS owner,state.owner.p AS weight,[x IN state.items | x.p] AS values ORDER BY owner",
                    vec![
                        vec![
                            GraphValue::Edge(EId(11)),
                            scalar(11),
                            GraphValue::List(
                                vec![scalar(2), scalar(7), scalar(2)].into_boxed_slice(),
                            ),
                        ],
                        vec![
                            GraphValue::Edge(EId(12)),
                            scalar(17),
                            GraphValue::List(
                                vec![scalar(2), scalar(7), scalar(2)].into_boxed_slice(),
                            ),
                        ],
                        vec![
                            GraphValue::Edge(EId(u128::MAX)),
                            scalar(19),
                            GraphValue::List(vec![scalar(7), null(), scalar(7)].into_boxed_slice()),
                        ],
                    ],
                ),
                (
                    "MATCH (n:L) WITH [n,n] AS items UNWIND [x IN items | x.p] AS value RETURN value ORDER BY value NULLS LAST",
                    vec![
                        vec![scalar(2)],
                        vec![scalar(2)],
                        vec![scalar(7)],
                        vec![scalar(7)],
                        vec![null()],
                        vec![null()],
                    ],
                ),
            ];
            let mut prepared = Vec::new();
            for (text, expected) in cases {
                let ordinary = prepare(text, &args);
                let QueryResult::Rows { rows, .. } =
                    ordinary.execute(&db, &cx, &args, policy(100)).unwrap()
                else {
                    panic!("expected native rows");
                };
                let rows: Vec<Vec<_>> = rows
                    .into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|value| match value {
                                GraphAggregateValue::Value(value) => value,
                                other => panic!("unexpected aggregate value: {other:?}"),
                            })
                            .collect()
                    })
                    .collect();
                assert_eq!(rows, expected, "ordinary pre-edit: {text}");
                prepared.push((
                    text,
                    ordinary.prepare_buffered_order(&args).unwrap(),
                    expected,
                ));
            }
            let mut edit = WriteBatch::new(R);
            edit.set_vertex_property(VId(u128::MAX), P, Some(CanonicalScalar::Int(70)));
            edit.set_edge_property(EId(u128::MAX), P, Some(CanonicalScalar::Int(190)));
            assert_eq!(db.write(&commit, edit).await.unwrap(), CommitSeq(2));
            db.compact(&commit).await.unwrap();
            for (text, prepared, expected) in prepared {
                // Ordinary buffered point reads may retain one authenticated
                // frame. Compare identical completed access states, not the
                // deliberately empty cache immediately after opening.
                let mut quiescent = None;
                for _ in 0..2 {
                    let mut io = files(&cx, &spill_pool).await;
                    let (spool, _) = execute(&prepared, &mut view, &cx, policy(100), &mut io)
                        .await
                        .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                    assert_eq!(spool.snapshot_seq(), CommitSeq(1));
                    let actual = contents(&spool, &mut io.b, &cx).await;
                    assert_eq!(actual, expected, "buffered frozen cut: {text}");
                    assert_eq!(spool.row_count(), expected.len() as u64);
                    assert_eq!(spill_pool.used(), 0, "{text}");
                    if let Some(prior) = quiescent {
                        assert_eq!(source_pool.used(), prior, "{text}");
                    }
                    quiescent = Some(source_pool.used());
                }
            }
            drop(view);
            assert_eq!(source_pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn buffered_property_payloads_share_exact_allowances_and_preserve_late_errors() {
        let ((), report) = run_async_under_lab(0x5749_5361, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let (_db, vfs) = fixture(&commit).await;
            let source_pool = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
            let spill_pool = MemoryPool::new(2 * 1024 * 1024, 0).unwrap();
            let mut view = cold(&commit, &vfs, &source_pool).await;
            let args = GqlParameters::new();
            for (tail, count) in [
                ("RETURN payload", 3),
                ("RETURN payload LIMIT 0", 0),
                ("RETURN payload LIMIT 1", 1),
                ("WHERE FALSE RETURN payload", 0),
            ] {
                let text = format!(
                    "MATCH (n:L) WITH [n,n] AS identities WITH [x IN identities | x.q] AS payload {tail}"
                );
                let prepared = prepare(&text, &args).prepare_buffered_order(&args).unwrap();
                let small_text = text.replace("x.q", "x.p");
                let small = prepare(&small_text, &args)
                    .prepare_buffered_order(&args)
                    .unwrap();
                let mut io = files(&cx, &spill_pool).await;
                let (small, _) = execute(&small, &mut view, &cx, policy(100), &mut io)
                    .await
                    .unwrap();
                let small_scratch = small.evaluator_stats().scratch_entries;
                assert_eq!(spill_pool.used(), 0);
                let mut io = files(&cx, &spill_pool).await;
                let (full, _) = execute(&prepared, &mut view, &cx, policy(100), &mut io)
                    .await
                    .unwrap_or_else(|error| panic!("{text}: {error:?}"));
                let rows = full.row_stats();
                let stats = full.evaluator_stats();
                // There are three vertices and two reached q reads per vertex.
                // Each independent 513-byte destination scalar needs at least
                // ceil(513/64) payload entries, even behind a zero-result page.
                let payload_entries = 6 * PAYLOAD.div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) as u64;
                assert!(
                    stats.scratch_entries >= small_scratch + payload_entries,
                    "{text}"
                );
                let expected = vec![
                    vec![GraphValue::List(
                        vec![payload(), payload()].into_boxed_slice()
                    )];
                    count
                ];
                assert_eq!(contents(&full, &mut io.b, &cx).await, expected, "{text}");
                assert_eq!(rows.result_rows, count as u64);
                assert_eq!(spill_pool.used(), 0);
                let exact = GqlQueryPolicy::new(
                    rows.snapshot_records,
                    rows.result_rows,
                    stats.work_units,
                    stats.scratch_entries,
                );
                for dimension in [
                    None,
                    Some(GlaLimitDimension::WorkUnits),
                    Some(GlaLimitDimension::ScratchEntries),
                ] {
                    let mut limits = exact;
                    match dimension {
                        Some(GlaLimitDimension::WorkUnits) => limits.evaluator.max_work_units -= 1,
                        Some(GlaLimitDimension::ScratchEntries) => {
                            limits.evaluator.max_scratch_entries -= 1
                        }
                        _ => {}
                    }
                    let mut quiescent = None;
                    for _ in 0..2 {
                        let mut io = files(&cx, &spill_pool).await;
                        let result = execute(&prepared, &mut view, &cx, limits, &mut io).await;
                        if let Some(dimension) = dimension {
                            let error = result.unwrap_err();
                            assert!(
                                matches!(error, NativeSpoolError::BufferedExecute(ref inner)
                                if matches!(**inner, GqlQueryError::Evaluator(ref limit)
                                    if limit.dimension == dimension)),
                                "{text}: {error:?}"
                            );
                        } else {
                            let (spool, _) =
                                result.unwrap_or_else(|error| panic!("{text}: {error:?}"));
                            assert_eq!(spool.snapshot_seq(), CommitSeq(1));
                            assert_eq!(spool.row_stats(), rows);
                            assert_eq!(spool.evaluator_stats(), stats);
                            assert_eq!(contents(&spool, &mut io.b, &cx).await, expected);
                        }
                        assert_eq!(spill_pool.used(), 0, "{text}");
                        if let Some(prior) = quiescent {
                            assert_eq!(source_pool.used(), prior, "{text}");
                        }
                        quiescent = Some(source_pool.used());
                    }
                }
            }
            for limit in [0, 1] {
                let text = format!(
                    "MATCH (n:L) WITH n AS x RETURN 1/(coalesce(x.p,0)-7) AS value LIMIT {limit}"
                );
                let prepared = prepare(&text, &args).prepare_buffered_order(&args).unwrap();
                let mut quiescent = None;
                for _ in 0..2 {
                    let mut io = files(&cx, &spill_pool).await;
                    let error = execute(&prepared, &mut view, &cx, policy(1), &mut io)
                        .await
                        .unwrap_err();
                    assert!(
                        matches!(error, NativeSpoolError::SetExecution(
                        GraphSetExecutionError::Projection { error, .. }
                    ) if error.kind == GraphIntegerErrorKind::DivisionByZero),
                        "{text}: {error:?}"
                    );
                    assert_eq!(spill_pool.used(), 0);
                    if let Some(prior) = quiescent {
                        assert_eq!(source_pool.used(), prior, "{text}");
                    }
                    quiescent = Some(source_pool.used());
                }
            }
            drop(view);
            assert_eq!(source_pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
