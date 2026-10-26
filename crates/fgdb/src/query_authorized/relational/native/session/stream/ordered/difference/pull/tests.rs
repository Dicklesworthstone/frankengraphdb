//! Resumable authorized-difference laws over real masked history and spill.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, QueryLimits, Scope};
use std::collections::BTreeMap;
use std::io::{self, Cursor, Seek, SeekFrom};
use std::task::Waker;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xe8; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const R: RelationId = RelationId(1);
type Resolver = fn(GraphSymbolKind, &str) -> Option<GraphSymbol>;
type Session<'a> = AuthorizedOrderedReadSession<'a, Resolver, fn() -> u64, File, File>;
type Bag = BTreeMap<Vec<QueryValue>, i128>;

fn now() -> u64 {
    100
}
fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xe801), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 100_000_000,
            max_rows: 100,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([R]);
    grant.properties = Scope::only([P]);
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100, 10_000_000, 10_000_000)
}
fn limits() -> AuthorizedSortLimits {
    AuthorizedSortLimits {
        run_rows: 2,
        max_runs: 128,
        page_bytes: 127,
        max_row_bytes: 8192,
        max_input_rows: 1000,
        max_sort_work: 10_000_000,
    }
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
async fn history(cx: &CommitCx) -> (Database<MemVfs>, [CommitSeq; 2]) {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x38; 32], NS, [0x48; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 3), (2, 3), (3, 7)] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(value))],
        );
    }
    let first = db.write(cx, batch).await.unwrap();
    let mut batch = WriteBatch::new(R);
    batch.delete_vertex(VId(2));
    batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
    let second = db.write(cx, batch).await.unwrap();
    (db, [first, second])
}

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
    pending_read: bool,
}
#[derive(Clone, Default)]
struct File(Arc<Mutex<FileState>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut state = self.0.lock().unwrap();
        state.reads += 1;
        if state.pending_read {
            return Poll::Pending;
        }
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
async fn session<'a>(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    authority: &'a Authority,
    pool: &MemoryPool,
) -> (Session<'a>, File, File) {
    let token = authority.issue_at(&grant(), 100).unwrap();
    let (scratch, a) = file(cx, pool).await;
    let (destination, b) = file(cx, pool).await;
    (
        db.authorized_ordered_read_session(
            cx,
            authority,
            &token,
            "main",
            symbols as Resolver,
            policy(),
            now as fn() -> u64,
            scratch,
            destination,
            limits(),
        )
        .unwrap(),
        a,
        b,
    )
}
fn cells(frame: &TrackedBytes) -> Vec<QueryValue> {
    GraphValueRow::decode_canonical(frame.as_ref())
        .unwrap()
        .values()
        .iter()
        .cloned()
        .map(QueryValue::Value)
        .collect()
}
fn value(value: i64) -> Vec<QueryValue> {
    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
        value,
    )))]
}

#[test]
fn pull_delivery_exposes_metadata_before_work_and_certifies_only_clean_exhaustion() {
    let ((), report) = run_async_under_lab(0xd1ff_8201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut scoped, _, _) = session(&db, &cx, &authority, &pool).await;
        let params = GqlParameters::new();
        let prepared = scoped
            .prepare(&cx, "MATCH (n) RETURN n.p AS p", &params)
            .unwrap();
        let execution = scoped
            .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
            .unwrap();
        assert_eq!(execution.columns(), ["p"]);
        assert_eq!((execution.before(), execution.after()), (cuts[0], cuts[1]));
        let mut cursor = execution.await.unwrap();
        assert_eq!(cursor.columns(), ["p"]);
        assert_eq!((cursor.before(), cursor.after()), (cuts[0], cuts[1]));
        assert_eq!(cursor.state(), ScanState::Open);
        let mut changes = Bag::new();
        while let Some((weight, row)) = cursor.next_change().await.unwrap() {
            assert_ne!(weight, 0);
            assert!(changes.insert(cells(&row), weight).is_none());
        }
        assert_eq!(cursor.state(), ScanState::Exhausted);
        assert_eq!(changes, Bag::from([(value(3), -2), (value(9), 1)]));
        drop(cursor);
        assert!(!scoped.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_relations_and_projected_pulls_complete_each_endpoint_before_delivery() {
    let ((), report) = run_async_under_lab(0xd1ff_8204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut scoped, a, b) = session(&db, &cx, &authority, &pool).await;
        let params = GqlParameters::new();
        for text in [
            "MATCH (n) RETURN n.p + 1 AS value ORDER BY value LIMIT 0",
            "MATCH (n) WITH n.p AS value RETURN value + 1 AS value ORDER BY value LIMIT 0",
        ] {
            let prepared = scoped.prepare(&cx, text, &params).unwrap();
            assert!(
                bind_ordered(&prepared.native, &params, cuts[0])
                    .unwrap()
                    .5
                    .is_some()
            );
            let before_io = [&a, &b].map(|backing| {
                let state = backing.0.lock().unwrap();
                (state.reads, state.writes, state.bytes.get_ref().len())
            });
            let execution = scoped
                .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
                .unwrap();
            assert_eq!(execution.columns(), ["value"]);
            assert_eq!((execution.before(), execution.after()), (cuts[0], cuts[1]));
            drop(execution);
            for (backing, before) in [&a, &b].into_iter().zip(before_io) {
                let state = backing.0.lock().unwrap();
                assert_eq!(
                    (state.reads, state.writes, state.bytes.get_ref().len()),
                    before
                );
            }
            let execution = scoped
                .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
                .unwrap();
            assert_eq!(execution.columns(), ["value"]);
            assert_eq!((execution.before(), execution.after()), (cuts[0], cuts[1]));
            let mut cursor = execution.await.unwrap();
            assert_eq!(cursor.columns(), ["value"]);
            assert_eq!((cursor.before(), cursor.after()), (cuts[0], cuts[1]));
            assert!(cursor.next_change().await.unwrap().is_none());
            assert_eq!(cursor.state(), ScanState::Exhausted);
            drop(cursor);
            assert!(!scoped.is_closed());
            assert_eq!(pool.used(), 0);
        }
        let prepared = scoped
            .prepare(
                &cx,
                "MATCH (n) RETURN n AS id, n.p + 1 AS value ORDER BY id",
                &params,
            )
            .unwrap();
        let mut cursor = scoped
            .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
            .unwrap()
            .await
            .unwrap();
        let mut changes = Bag::new();
        while let Some((weight, row)) = cursor.next_change().await.unwrap() {
            assert!(changes.insert(cells(&row), weight).is_none());
        }
        let projected = |id, p| {
            vec![
                QueryValue::Value(GraphValue::Vertex(VId(id))),
                QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(p))),
            ]
        };
        assert_eq!(
            changes,
            Bag::from([
                (projected(1, 4), -1),
                (projected(2, 4), -1),
                (projected(1, 10), 1),
            ])
        );
        assert_eq!(cursor.state(), ScanState::Exhausted);
        drop(cursor);
        assert!(!scoped.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unpolled_open_and_explicit_close_do_not_consume_a_difference_suffix() {
    let ((), report) = run_async_under_lab(0xd1ff_8202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut scoped, a, b) = session(&db, &cx, &authority, &pool).await;
        let params = GqlParameters::new();
        let prepared = scoped
            .prepare(&cx, "MATCH (n) RETURN n.p AS p", &params)
            .unwrap();
        let execution = scoped
            .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
            .unwrap();
        drop(execution);
        for backing in [&a, &b] {
            let state = backing.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert!(!scoped.is_closed());
        let execution = scoped
            .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
            .unwrap();
        let mut cursor = execution.await.unwrap();
        let reads = a.0.lock().unwrap().reads;
        cursor.close();
        assert_eq!(cursor.state(), ScanState::Closed);
        assert!(cursor.next_change().await.unwrap().is_none());
        assert_eq!(a.0.lock().unwrap().reads, reads);
        drop(cursor);
        assert!(!scoped.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn dropped_in_flight_pull_fuses_delivery_and_preserves_scratch_poisoning() {
    let ((), report) = run_async_under_lab(0xd1ff_8203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut scoped, scratch, _) = session(&db, &cx, &authority, &pool).await;
        let params = GqlParameters::new();
        let prepared = scoped
            .prepare(&cx, "MATCH (n) RETURN n.p AS p", &params)
            .unwrap();
        let execution = scoped
            .difference_stream(&cx, &prepared, &params, cuts[0], cuts[1])
            .unwrap();
        let mut cursor = execution.await.unwrap();
        scratch.0.lock().unwrap().pending_read = true;
        // The polled future and its Context (not Send) end before the next
        // await, so the lab task stays Send.
        {
            let mut future = Box::pin(cursor.next_change());
            let mut task = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut task).is_pending());
        }
        assert_eq!(cursor.state(), ScanState::Failed);
        let reads = scratch.0.lock().unwrap().reads;
        assert!(cursor.next_change().await.unwrap().is_none());
        assert_eq!(scratch.0.lock().unwrap().reads, reads);
        drop(cursor);
        assert!(!scoped.is_closed());
        assert!(scoped.scratch.is_poisoned());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
