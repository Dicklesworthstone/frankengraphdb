//! Public ordered-session laws over real spill pages and masked native input.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryResult, QueryValue, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{GraphSymbol, GraphSymbolKind};
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits};
use fgdb_types::{CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Scope};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Waker};

#[path = "tests/imported.rs"]
mod imported;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x86; 32]);
type Resolver = fn(GraphSymbolKind, &str) -> Option<GraphSymbol>;
type Session<'a, C> = AuthorizedOrderedReadSession<'a, Resolver, C, File, File>;

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(8601), NS, "graph", SchemaEpoch(0), 1).unwrap()
}
fn grant() -> Grant {
    let mut grant = Grant::read_only(
        "main",
        1000,
        QueryLimits {
            max_nodes: 100_000,
            max_work: 10_000_000,
            max_rows: 100,
        },
    );
    grant.labels = Scope::only([LabelId(1)]);
    grant.relations = Scope::only([RelationId(1)]);
    grant.properties = Scope::only([PropertyKeyId(1)]);
    grant
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, 100, 1_000_000, 1_000_000)
}
fn limits() -> AuthorizedSortLimits {
    AuthorizedSortLimits {
        run_rows: 2,
        max_runs: 16,
        page_bytes: 31,
        max_row_bytes: 4096,
        max_input_rows: 100,
        max_sort_work: 1_000_000,
    }
}
fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
async fn database(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x16; 32], NS, [0x26; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, p) in [(1, 3), (2, 1), (3, 3), (4, -1), (5, 2)] {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
            ],
        );
    }
    batch.create_vertex(
        VId(99),
        vec![LabelId(99)],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(999))],
    );
    for (id, from, to, p) in [
        (10, 1, 2, 7),
        (11, 1, 2, 9),
        (12, 2, 3, 5),
        (13, 1, 99, 999),
    ] {
        batch.add_edge(
            EId(id),
            VId(from),
            VId(to),
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
            ],
        );
    }
    db.write(cx, batch).await.unwrap();
    db
}

// Only the trusted test host retains an observation handle. The public
// session API exposes neither File, SpillFile, run handles nor private frames.
#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    pending_read: bool,
    pending_write: bool,
    expire_after_read: Option<Arc<AtomicU64>>,
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
        if let Some(now) = &state.expire_after_read {
            now.store(1001, Ordering::SeqCst);
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
        if state.pending_write {
            return Poll::Pending;
        }
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
    let backend = File::default();
    let file = SpillFile::new(
        cx,
        backend.clone(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 4_000_000,
            max_runs: 512,
            max_run_bytes: 1_000_000,
        },
    )
    .await
    .unwrap();
    (file, backend)
}
#[allow(clippy::too_many_arguments)]
async fn session<'a, C: FnMut() -> u64>(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    authority: &'a Authority,
    token: &CapabilityToken,
    pool: &MemoryPool,
    clock: C,
    limits: AuthorizedSortLimits,
) -> (Session<'a, C>, File, File) {
    let (scratch, a) = file(cx, pool).await;
    let (destination, b) = file(cx, pool).await;
    let session = db
        .authorized_ordered_read_session(
            cx,
            authority,
            token,
            "main",
            symbols as Resolver,
            policy(),
            clock,
            scratch,
            destination,
            limits,
        )
        .unwrap();
    (session, a, b)
}
fn scalar(value: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(value))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
async fn values(cursor: &mut AuthorizedSpoolCursor<'_, File>) -> Vec<Vec<GraphValue>> {
    let mut rows = Vec::new();
    while let Some(bytes) = cursor.next_row().await.unwrap() {
        rows.push(
            GraphValueRow::decode_canonical(bytes.as_ref())
                .unwrap()
                .values()
                .to_vec(),
        );
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    rows
}
fn refusal<T>(result: Result<T>, expected: AuthorizationError) {
    assert!(matches!(result, Err(NativeSpoolError::Authorized(error))
        if matches!(*error, QueryError::Authorization(actual) if actual == expected)));
}

#[test]
fn keyed_ordered_streams_bound_input_before_sort_and_preserve_the_eager_window() {
    let ((), report) = run_async_under_lab(0x8600_0011, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 100..228_u128 {
            batch.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(id as i64))],
            );
            batch.create_vertex(
                VId(id + 1000),
                vec![LabelId(99)],
                vec![(PropertyKeyId(1), CanonicalScalar::Int(3))],
            );
        }
        batch.create_vertex(
            VId(u128::MAX),
            vec![LabelId(1)],
            vec![(
                PropertyKeyId(1),
                CanonicalScalar::Float(fgdb_types::CanonicalF64::new(3.0)),
            )],
        );
        db.write(&commit, batch).await.unwrap();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (scratch, _) = file(&cx, &pool).await;
        let (destination, _) = file(&cx, &pool).await;
        let mut ordered = db
            .authorized_ordered_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols as Resolver,
                GqlQueryPolicy::new(3, 3, 1_000_000, 1_000_000),
                || 100,
                scratch,
                destination,
                AuthorizedSortLimits {
                    max_input_rows: 3,
                    ..limits()
                },
            )
            .unwrap();
        let args = GqlParameters::new().with_int64("key", 3).unwrap();
        for text in [
            "MATCH (n:L) WHERE n.p = $key RETURN n AS id ORDER BY id DESC",
            "MATCH (n:L) WHERE n.p = $key RETURN n AS id ORDER BY id DESC SKIP 1 LIMIT 1",
            "MATCH (n:L) WHERE n.p = $key RETURN n.p AS p ORDER BY p DESC",
        ] {
            let eager = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap()
                .query(&cx, text, &args)
                .unwrap();
            let prepared = ordered.prepare(&cx, text, &args).unwrap();
            let future = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
            fn is_send<T: Send>(_: &T) {}
            is_send(&future);
            let mut cursor = future.await.unwrap();
            let columns = cursor.columns().to_vec();
            let rows = values(&mut cursor)
                .await
                .into_iter()
                .map(|row| row.into_iter().map(QueryValue::Value).collect())
                .collect();
            assert_eq!(QueryResult::Rows { columns, rows }, eager, "{text}");
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_equality_preserves_denied_key_quota_and_earlier_expression_errors() {
    let ((), report) = run_async_under_lab(0x8600_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for hidden in [55, 99] {
            let mut db =
                Database::open_memory(&commit, DatabaseKeys::new([0x16; 32], NS, [0x26; 32]))
                    .await
                    .unwrap();
            let mut batch = WriteBatch::new(RelationId(1));
            for id in 1..=2 {
                batch.create_vertex(
                    VId(id),
                    vec![LabelId(1)],
                    vec![
                        (PropertyKeyId(1), CanonicalScalar::Int(0)),
                        (PropertyKeyId(2), CanonicalScalar::Int(hidden)),
                    ],
                );
            }
            db.write(&commit, batch).await.unwrap();
            let pool = MemoryPool::new(32_768, 0).unwrap();
            let (scratch, _) = file(&cx, &pool).await;
            let (destination, _) = file(&cx, &pool).await;
            let mut ordered = db
                .authorized_ordered_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols as Resolver,
                    GqlQueryPolicy::new(1, 2, 1_000_000, 1_000_000),
                    || 100,
                    scratch,
                    destination,
                    limits(),
                )
                .unwrap();
            let args = GqlParameters::new();
            let denied = ordered
                .prepare(
                    &cx,
                    "MATCH (n:L) WHERE n.hidden = 55 RETURN n AS id ORDER BY id DESC",
                    &args,
                )
                .unwrap();
            let error = ordered
                .execute_ordered(&cx, &denied, &args)
                .unwrap()
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                NativeSpoolError::Authorized(error)
                    if matches!(*error, QueryError::Stream(GqlQueryError::Rows(error))
                        if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                            && error.limit == 1 && error.observed == 2)
            ));
            // The failed execution may poison this session's spill files, and
            // poisoning persists across calls (the session's documented law).
            // The earlier-error claim gets its own session and fresh files.
            drop(ordered);
            let (scratch, _) = file(&cx, &pool).await;
            let (destination, _) = file(&cx, &pool).await;
            let mut ordered = db
                .authorized_ordered_read_session(
                    &cx,
                    &issuer,
                    &token,
                    "main",
                    symbols as Resolver,
                    GqlQueryPolicy::new(1, 2, 1_000_000, 1_000_000),
                    || 100,
                    scratch,
                    destination,
                    limits(),
                )
                .unwrap();
            let fallible = ordered
                .prepare(
                    &cx,
                    "MATCH (n:L) WHERE 10 / n.p > 0 AND n.p = 7 RETURN n AS id ORDER BY id DESC",
                    &args,
                )
                .unwrap();
            let error = ordered
                .execute_ordered(&cx, &fallible, &args)
                .unwrap()
                .await
                .unwrap_err();
            let observed = format!("{error:?}");
            assert!(
                matches!(
                    error,
                    NativeSpoolError::Authorized(error)
                        if matches!(*error, QueryError::Stream(GqlQueryError::Data(error))
                            if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)
                ),
                "{observed}"
            );
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn masked_vertex_and_edge_ordering_keep_bags_hidden_keys_and_final_windows() {
    let ((), report) = run_async_under_lab(0x8600_0001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let params = GqlParameters::new();
        for (text, expected) in [
            (
                "MATCH (n:L) RETURN n.p AS p ORDER BY p DESC SKIP 1 LIMIT 3",
                vec![vec![scalar(3)], vec![scalar(2)], vec![scalar(1)]],
            ),
            (
                "MATCH (n:L) RETURN DISTINCT n.p AS p ORDER BY p DESC SKIP 1 LIMIT 3",
                vec![vec![scalar(2)], vec![scalar(1)], vec![scalar(-1)]],
            ),
            (
                "MATCH (n:L) RETURN n.p AS p ORDER BY n.hidden DESC, p ASC SKIP 1 LIMIT 2",
                vec![vec![scalar(1)], vec![scalar(2)]],
            ),
            (
                "MATCH (a:L)-[e:R]->(b:L) RETURN e.p AS p,a.hidden AS hidden ORDER BY p DESC SKIP 1 LIMIT 2",
                vec![vec![scalar(7), null()], vec![scalar(5), null()]],
            ),
            (
                "MATCH (a:L)-[e:R]->(b:L)-[f:R]->(c:L) RETURN e.p AS p,f.p AS next ORDER BY p DESC LIMIT 2",
                vec![vec![scalar(9), scalar(5)], vec![scalar(7), scalar(5)]],
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let future = ordered.execute_ordered(&cx, &prepared, &params).unwrap();
            fn require_send<T: Send>(_: &T) {}
            require_send(&future);
            let mut cursor = future.await.unwrap();
            require_send(&cursor);
            assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
            assert_eq!(values(&mut cursor).await, expected, "{text}");
            drop(cursor);
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_vertex_set_with_constant_columns_sorts_its_projected_scan() {
    // fgdb-7xqxb: no pattern facade projects a literal, so these reads are
    // sets. Each scanned root takes the set's projection, then the external
    // sort applies the set's own ORDER BY and page. Ties break on the whole
    // row, as the eager projection's canonical sort does. VId 99 is outside
    // the label scope and never appears.
    let ((), report) = run_async_under_lab(0x8600_000b, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut ordered, a, b) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let params = GqlParameters::new();
        let vertex = |id| GraphValue::Vertex(VId(id));
        for (text, expected) in [
            (
                "MATCH (n:L) RETURN 1 AS value ORDER BY value",
                vec![vec![scalar(1)]; 5],
            ),
            (
                "MATCH (n:L) RETURN n AS id, 7 AS seven ORDER BY id DESC SKIP 1 LIMIT 2",
                vec![vec![vertex(4), scalar(7)], vec![vertex(3), scalar(7)]],
            ),
            (
                "MATCH (n:L) RETURN 7 AS seven, n AS id ORDER BY seven LIMIT 3",
                vec![
                    vec![scalar(7), vertex(1)],
                    vec![scalar(7), vertex(2)],
                    vec![scalar(7), vertex(3)],
                ],
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let mut cursor = ordered
                .execute_ordered(&cx, &prepared, &params)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
            assert_eq!(values(&mut cursor).await, expected, "{text}");
            drop(cursor);
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);
        }
        // Relationship projections finish the masked graph leaf before the
        // shared native stages. Parallel occurrences survive.
        let edge = ordered
            .prepare(
                &cx,
                "MATCH (n:L)-[e:R]->(m:L) RETURN 1 AS one ORDER BY one",
                &params,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &edge, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(values(&mut cursor).await, vec![vec![scalar(1)]; 3]);
        drop(cursor);
        // Binary relations still require a separate physical owner. Refuse
        // before scratch, even when the final page is empty.
        let scratch_bytes = a.0.lock().unwrap().bytes.get_ref().len();
        let destination_bytes = b.0.lock().unwrap().bytes.get_ref().len();
        let binary = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n.p + 1 AS value UNION ALL \
                 MATCH (m:L) RETURN m.p + 1 AS value LIMIT 0",
                &params,
            )
            .unwrap();
        assert!(matches!(ordered.execute_ordered(&cx, &binary, &params),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error, QueryError::StreamingUnsupported { .. })));
        assert_eq!(a.0.lock().unwrap().bytes.get_ref().len(), scratch_bytes);
        assert_eq!(b.0.lock().unwrap().bytes.get_ref().len(), destination_bytes);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_projected_ordering_binds_its_own_cut_and_sort_tail() {
    let params = GqlParameters::new()
        .with_uint64("cut", 3)
        .unwrap()
        .with_int64("value", 17)
        .unwrap();
    let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut \
                RETURN n AS id, $value AS value ORDER BY id DESC SKIP 1 LIMIT 2";
    let prepared = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
    assert!(matches!(prepared, PreparedNativeRead::TemporalSet(_)));
    let (_, _, at, columns, pull, stages) = bind_ordered(&prepared, &params, CommitSeq(7)).unwrap();
    assert_eq!(at, CommitSeq(3));
    assert_eq!(columns, ["id", "value"]);
    assert!(pull.is_some());
    assert!(stages.is_none());
}

#[test]
fn staged_ordered_relations_preserve_masked_bags_and_every_intermediate_window() {
    let ((), report) = run_async_under_lab(0x8600_0023, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let params = GqlParameters::new();
        for (text, expected) in [
            (
                "MATCH (n) RETURN DISTINCT n.p + 1 AS value, n.hidden AS hidden \
                 ORDER BY value DESC SKIP 1 LIMIT 2",
                vec![vec![scalar(3), null()], vec![scalar(2), null()]],
            ),
            (
                "MATCH (n) WITH n.p AS value ORDER BY value DESC LIMIT 3 \
                 RETURN value + 2 AS value ORDER BY value",
                vec![vec![scalar(4)], vec![scalar(5)], vec![scalar(5)]],
            ),
            (
                "MATCH (n) WITH n.p AS value WHERE value > 1 \
                 UNWIND [value, value + 1] AS item \
                 RETURN item ORDER BY item DESC LIMIT 5",
                vec![
                    vec![scalar(4)],
                    vec![scalar(4)],
                    vec![scalar(3)],
                    vec![scalar(3)],
                    vec![scalar(3)],
                ],
            ),
            (
                "MATCH (a)-[e:R]->(b) RETURN e.p + 1 AS value, b.hidden AS hidden \
                 ORDER BY value DESC",
                vec![
                    vec![scalar(10), null()],
                    vec![scalar(8), null()],
                    vec![scalar(6), null()],
                ],
            ),
            (
                "MATCH (n) WITH n AS item \
                 RETURN CASE WHEN TRUE THEN 7 ELSE [item][0].p END AS value ORDER BY value",
                vec![vec![scalar(7)]; 5],
            ),
        ] {
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let execution = ordered.execute_ordered(&cx, &prepared, &params).unwrap();
            assert_eq!(execution.snapshot_seq(), CommitSeq(1));
            let columns = execution.columns().to_vec();
            let mut cursor = execution.await.unwrap();
            assert_eq!(cursor.columns(), columns);
            assert_eq!(values(&mut cursor).await, expected, "{text}");
            drop(cursor);
            assert_eq!(pool.used(), 0);
            assert!(!ordered.is_closed());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_temporal_ordering_rebinds_the_cut_without_reacquiring_the_writer() {
    let ((), report) = run_async_under_lab(0x8600_0024, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut pinned, a, b) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let text = "MATCH (n) FOR SYSTEM_TIME AS OF SEQ $cut \
                    RETURN n.p + $bump AS value, n.hidden AS hidden ORDER BY value DESC LIMIT 2";
        let args = |cut, bump| {
            GqlParameters::new()
                .with_uint64("cut", cut)
                .unwrap()
                .with_int64("bump", bump)
                .unwrap()
        };
        let params = args(1, 1);
        let prepared = pinned.prepare(&cx, text, &params).unwrap();
        let execution = pinned.execute_ordered(&cx, &prepared, &params).unwrap();
        assert_eq!(execution.columns(), ["value", "hidden"]);
        assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
        db.write(&commit, change).await.unwrap();
        db.compact(&contexts.maint()).await.unwrap();
        let mut cursor = execution.await.unwrap();
        assert_eq!(cursor.snapshot_seq(), CommitSeq(1));
        assert_eq!(values(&mut cursor).await, vec![vec![scalar(4), null()]; 2]);
        drop(cursor);
        assert!(
            matches!(pinned.execute_ordered(&cx, &prepared, &args(2, 1)),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error,
                QueryError::Read(crate::ReadError::BeyondFrontier { asked: CommitSeq(2), frontier: CommitSeq(1) })))
        );
        let (mut current, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = current.prepare(&cx, text, &params).unwrap();
        for (cut, bump, expected) in [(0, 1, vec![]), (1, 2, vec![5, 5]), (2, 3, vec![33, 6])] {
            let mut cursor = current
                .execute_ordered(&cx, &prepared, &args(cut, bump))
                .unwrap()
                .await
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            assert_eq!(
                values(&mut cursor).await,
                expected
                    .into_iter()
                    .map(|value| vec![scalar(value), null()])
                    .collect::<Vec<_>>()
            );
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_ordering_preserves_canonical_failure_ordinals_even_under_limit_zero() {
    let ((), report) = run_async_under_lab(0x8600_0025, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        for text in [
            "MATCH (n) RETURN 10 / (n.p - 2) AS value ORDER BY value LIMIT 0",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN 10 / (n.p - 2) AS value ORDER BY value LIMIT 0",
        ] {
            let (mut ordered, _, _) =
                session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
            let prepared = ordered.prepare(&cx, text, &params).unwrap();
            let error = ordered
                .execute_ordered(&cx, &prepared, &params)
                .unwrap()
                .await
                .unwrap_err();
            assert!(
                matches!(error, NativeSpoolError::Authorized(error)
                if matches!(*error, QueryError::Set(GqlQueryError::Source(
                    GraphSetExecutionError::Projection { row: 2, column: 0, error }))
                    if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)),
                "{text}"
            );
            assert!(!ordered.is_closed());
            assert_eq!(pool.used(), 0);
        }
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n) WITH n AS item RETURN [item][0].p AS value ORDER BY value LIMIT 0",
                &params,
            )
            .unwrap();
        let error = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Set(GqlQueryError::Source(
                GraphSetExecutionError::Projection { row: 0, column: 0, error }))
                if error.kind == fgdb_gql::GraphIntegerErrorKind::IncompatibleOperands)));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_projected_ordering_preserves_history_masking_and_eager_windows() {
    let ((), report) = run_async_under_lab(0x8600_0012, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = database(&commit).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut pinned, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let text = "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut \
                    RETURN n AS id, n.p AS p, n.hidden AS hidden, $tag AS tag \
                    ORDER BY id DESC SKIP 1 LIMIT 2";
        let args = |cut, tag| {
            GqlParameters::new()
                .with_uint64("cut", cut)
                .unwrap()
                .with_int64("tag", tag)
                .unwrap()
        };
        let prepared = pinned.prepare(&cx, text, &args(1, 17)).unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
        change.delete_vertex(VId(4));
        change.create_vertex(
            VId(6),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(60))],
        );
        db.write(&commit, change).await.unwrap();
        db.compact(&commit).await.unwrap();
        let mut old = pinned
            .execute_ordered(&cx, &prepared, &args(1, 17))
            .unwrap()
            .await
            .unwrap();
        assert_eq!(old.snapshot_seq(), CommitSeq(1));
        assert_eq!(
            values(&mut old).await,
            vec![
                vec![GraphValue::Vertex(VId(4)), scalar(-1), null(), scalar(17)],
                vec![GraphValue::Vertex(VId(3)), scalar(3), null(), scalar(17)],
            ]
        );
        drop(old);
        assert_eq!(pool.used(), 0);
        assert!(matches!(
            pinned.execute_ordered(&cx, &prepared, &args(2, 17)),
            Err(NativeSpoolError::Authorized(error))
                if matches!(*error, QueryError::Read(crate::ReadError::BeyondFrontier {
                    asked: CommitSeq(2), frontier: CommitSeq(1)
                }))
        ));
        assert!(!pinned.is_closed());

        let (mut current, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let template = current.prepare(&cx, text, &args(1, 17)).unwrap();
        for (cut, tag, expected) in [
            (0, 5, Vec::new()),
            (
                1,
                23,
                vec![
                    vec![GraphValue::Vertex(VId(4)), scalar(-1), null(), scalar(23)],
                    vec![GraphValue::Vertex(VId(3)), scalar(3), null(), scalar(23)],
                ],
            ),
            (
                2,
                29,
                vec![
                    vec![GraphValue::Vertex(VId(5)), scalar(2), null(), scalar(29)],
                    vec![GraphValue::Vertex(VId(3)), scalar(30), null(), scalar(29)],
                ],
            ),
        ] {
            let params = args(cut, tag);
            let eager = db
                .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
                .unwrap()
                .query(&cx, text, &params)
                .unwrap();
            let mut cursor = current
                .execute_ordered(&cx, &template, &params)
                .unwrap()
                .await
                .unwrap();
            assert_eq!(cursor.snapshot_seq(), CommitSeq(cut));
            let columns = cursor.columns().to_vec();
            let rows = values(&mut cursor).await;
            assert_eq!(rows, expected);
            assert_eq!(
                QueryResult::Rows {
                    columns,
                    rows: rows
                        .into_iter()
                        .map(|row| row.into_iter().map(QueryValue::Value).collect())
                        .collect(),
                },
                eager
            );
            drop(cursor);
            assert_eq!(pool.used(), 0);
        }
        let now = Arc::new(AtomicU64::new(100));
        let sampled = Arc::clone(&now);
        let (mut expiring, a, b) = session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || sampled.load(Ordering::SeqCst),
            limits(),
        )
        .await;
        let prepared = expiring.prepare(&cx, text, &args(1, 17)).unwrap();
        now.store(1001, Ordering::SeqCst);
        // Authorization still precedes parameter validation and source/sort
        // admission, even for a historical projected statement.
        refusal(
            expiring.execute_ordered(&cx, &prepared, &GqlParameters::new()),
            AuthorizationError::Expired,
        );
        assert!(expiring.is_closed());
        assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_ordering_keeps_private_input_and_final_delivery_allowances_separate() {
    let ((), report) = run_async_under_lab(0x8600_0026, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let mut small = grant();
        small.limits.max_rows = 1;
        let token = issuer.issue_at(&small, 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let (scratch, _) = file(&cx, &pool).await;
        let (destination, _) = file(&cx, &pool).await;
        let mut ordered = db
            .authorized_ordered_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols as Resolver,
                GqlQueryPolicy::new(100_000, 1, 1_000_000, 1_000_000),
                || 100,
                scratch,
                destination,
                limits(),
            )
            .unwrap();
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n) RETURN n.p + 1 AS value ORDER BY value DESC LIMIT 1",
                &params,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        // Five private source occurrences are legal under a one-row FINAL
        // native allowance, and construction spends no signed result rows.
        assert_eq!(values(&mut cursor).await, vec![vec![scalar(4)]]);
        drop(cursor);
        drop(ordered);
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n) RETURN n.p + 1 AS value ORDER BY value DESC",
                &params,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        let bytes = cursor.next_row().await.unwrap().unwrap();
        assert_eq!(
            GraphValueRow::decode_canonical(bytes.as_ref())
                .unwrap()
                .values(),
            &[scalar(4)]
        );
        drop(bytes);
        refusal(
            cursor.next_row().await,
            AuthorizationError::LimitExceeded(LimitDimension::Rows),
        );
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        let mut bounded = limits();
        bounded.max_input_rows = 4;
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, bounded).await;
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n) RETURN n.p + 1 AS value ORDER BY value LIMIT 0",
                &params,
            )
            .unwrap();
        let error = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Stream(GqlQueryError::Rows(error))
                if error.dimension == fgdb_gql::GqlBudgetDimension::ResultRows
                    && error.limit == 4 && error.observed == 5)));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn staged_ordering_charges_live_signed_work_and_cumulative_native_work() {
    let ((), report) = run_async_under_lab(0x8600_0027, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let params = GqlParameters::new();
        let mut small = grant();
        small.limits.max_work = limits().max_sort_work + 2_000;
        let token = issuer.issue_at(&small, 100).unwrap();
        for (count, permitted) in [(2, true), (500, false)] {
            let (mut ordered, a, b) =
                session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
            let text = format!(
                "MATCH (n) WITH n.p AS p \
                RETURN size([x IN range(1, {count}) | x + p]) AS value LIMIT 0"
            );
            let prepared = ordered.prepare(&cx, &text, &params).unwrap();
            let result = ordered
                .execute_ordered(&cx, &prepared, &params)
                .unwrap()
                .await;
            if permitted {
                let mut cursor = result.unwrap();
                assert!(values(&mut cursor).await.is_empty());
            } else {
                refusal(
                    result,
                    AuthorizationError::LimitExceeded(LimitDimension::Work),
                );
                // Source ordering has completed; the same live permit then
                // refuses native stage work before any final result escapes.
                assert!(!a.0.lock().unwrap().bytes.get_ref().is_empty());
                assert!(!b.0.lock().unwrap().bytes.get_ref().is_empty());
            }
            assert_eq!(pool.used(), 0);
        }
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let (scratch, _) = file(&cx, &pool).await;
        let (destination, _) = file(&cx, &pool).await;
        let mut ordered = db
            .authorized_ordered_read_session(
                &cx,
                &issuer,
                &token,
                "main",
                symbols as Resolver,
                GqlQueryPolicy::new(100_000, 100, 500, 1_000_000),
                || 100,
                scratch,
                destination,
                limits(),
            )
            .unwrap();
        let prepared = ordered
            .prepare(
                &cx,
                "MATCH (n) WITH n.p AS p \
             RETURN size([x IN range(1, 500) | x + p]) AS value LIMIT 0",
                &params,
            )
            .unwrap();
        let error = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap_err();
        assert!(matches!(error, NativeSpoolError::Authorized(error)
            if matches!(*error, QueryError::Set(GqlQueryError::Evaluator(error))
                if error.dimension == fgdb_gql::GlaLimitDimension::WorkUnits
                    && error.limit == 500 && error.observed == 501)));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn sorted_cursor_pins_the_selected_history_while_the_writer_advances() {
    let ((), report) = run_async_under_lab(0x8600_0002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let params = GqlParameters::new().with_uint64("cut", 1).unwrap();
        let text =
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ $cut RETURN n.p AS p ORDER BY p DESC LIMIT 1";
        let prepared = ordered.prepare(&cx, text, &params).unwrap();
        let mut old = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        let mut change = WriteBatch::new(RelationId(1));
        change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(30)));
        db.write(&contexts.commit(), change).await.unwrap();
        assert_eq!(old.snapshot_seq(), CommitSeq(1));
        assert_eq!(values(&mut old).await, vec![vec![scalar(3)]]);
        drop(old);
        let newer = GqlParameters::new().with_uint64("cut", 2).unwrap();
        assert!(ordered.execute_ordered(&cx, &prepared, &newer).is_err());
        assert!(!ordered.is_closed());
        let (mut fresh, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let fresh_prepared = fresh.prepare(&cx, text, &params).unwrap();
        let mut cursor = fresh
            .execute_ordered(&cx, &fresh_prepared, &newer)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(cursor.snapshot_seq(), CommitSeq(2));
        assert_eq!(values(&mut cursor).await, vec![vec![scalar(30)]]);
        drop(cursor);
        let mut historic = fresh
            .execute_ordered(&cx, &fresh_prepared, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(historic.snapshot_seq(), CommitSeq(1));
        assert_eq!(values(&mut historic).await, vec![vec![scalar(3)]]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_rows_charge_only_final_pulls_and_work_reserves_before_scratch() {
    let ((), report) = run_async_under_lab(0x8600_0003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let mut small = grant();
        small.limits.max_rows = 2;
        let token = issuer.issue_at(&small, 100).unwrap();
        let (mut ordered, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p DESC", &params)
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        for expected in [3, 3] {
            let bytes = cursor.next_row().await.unwrap().unwrap();
            assert_eq!(
                GraphValueRow::decode_canonical(bytes.as_ref())
                    .unwrap()
                    .values(),
                &[scalar(expected)]
            );
        }
        refusal(
            cursor.next_row().await,
            AuthorizationError::LimitExceeded(LimitDimension::Rows),
        );
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(!ordered.is_closed());
        let selected = ordered
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
                &params,
            )
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &selected, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(
            values(&mut cursor).await,
            vec![vec![scalar(3)], vec![scalar(3)]]
        );
        drop(cursor);
        let mut limited = grant();
        limited.limits.max_work = limits().max_sort_work;
        let token = issuer.issue_at(&limited, 100).unwrap();
        let (mut blocked, a, b) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = blocked
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p", &params)
            .unwrap();
        refusal(
            blocked.execute_ordered(&cx, &prepared, &params),
            AuthorizationError::LimitExceeded(LimitDimension::Work),
        );
        assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn foreign_handles_unsupported_limit_zero_and_unpolled_work_touch_no_scratch() {
    let ((), report) = run_async_under_lab(0x8600_0004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let (mut ordered, a, b) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let params = GqlParameters::new();
        let mut foreign = db
            .authorized_read_session(&cx, &issuer, &token, "main", symbols, policy(), || 100)
            .unwrap();
        let other = foreign
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p", &params)
            .unwrap();
        refusal(
            ordered.execute_ordered(&cx, &other, &params),
            AuthorizationError::WrongAuthority,
        );
        let aggregate = ordered
            .prepare(&cx, "MATCH (n:L) RETURN count(*) AS n LIMIT 0", &params)
            .unwrap();
        assert!(matches!(ordered.execute_ordered(&cx, &aggregate, &params),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error, QueryError::StreamingUnsupported { .. })));
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p", &params)
            .unwrap();
        let unopened = ordered.execute_ordered(&cx, &prepared, &params).unwrap();
        assert_eq!(unopened.columns(), &["p"]);
        assert_eq!(unopened.snapshot_seq(), CommitSeq(1));
        drop(unopened);
        assert!(!ordered.is_closed());
        assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        assert_eq!(values(&mut cursor).await.len(), 5);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expiry_during_pending_sort_or_completed_read_closes_the_pin_without_rows() {
    let ((), report) = run_async_under_lab(0x8600_0005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let pins = Arc::strong_count(&db.snapshot);
        let now = Arc::new(AtomicU64::new(100));
        let sampled = Arc::clone(&now);
        let (mut ordered, _, b) = session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || sampled.load(Ordering::SeqCst),
            limits(),
        )
        .await;
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
            .unwrap();
        b.0.lock().unwrap().pending_write = true;
        let mut future = Box::pin(ordered.execute_ordered(&cx, &prepared, &params).unwrap());
        {
            let mut context = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut context).is_pending());
            now.store(1001, Ordering::SeqCst);
            let Poll::Ready(result) = future.as_mut().poll(&mut context) else {
                panic!("expiry must end pending I/O")
            };
            refusal(result, AuthorizationError::Expired);
        }
        drop(future);
        assert!(ordered.is_closed());
        assert!(ordered.destination.is_poisoned());
        assert_eq!(Arc::strong_count(&db.snapshot), pins);
        assert_eq!(pool.used(), 0);
        now.store(100, Ordering::SeqCst);
        let sampled = Arc::clone(&now);
        let (mut ordered, _, b) = session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || sampled.load(Ordering::SeqCst),
            limits(),
        )
        .await;
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        b.0.lock().unwrap().expire_after_read = Some(Arc::clone(&now));
        refusal(cursor.next_row().await, AuthorizationError::Expired);
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(ordered.is_closed());
        assert_eq!(Arc::strong_count(&db.snapshot), pins);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pending_read_drop_fuses_only_the_cursor_and_retirement_closes_the_session() {
    let ((), report) = run_async_under_lab(0x8600_0006, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let (mut ordered, _, b) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        b.0.lock().unwrap().pending_read = true;
        let mut read = Box::pin(cursor.next_row());
        {
            let mut context = Context::from_waker(Waker::noop());
            assert!(read.as_mut().poll(&mut context).is_pending());
        }
        drop(read);
        assert_eq!(cursor.state(), ScanState::Failed);
        assert!(cursor.next_row().await.unwrap().is_none());
        cursor.close();
        drop(cursor);
        assert!(!ordered.is_closed());
        assert!(ordered.destination.is_poisoned());
        assert_eq!(pool.used(), 0);
        let (mut fresh, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
        let prepared = fresh
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
            .unwrap();
        let mut cursor = fresh
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        assert!(cursor.next_row().await.unwrap().is_some());
        issuer.retire();
        refusal(
            cursor.next_row().await,
            AuthorizationError::AuthorityRetired,
        );
        assert!(cursor.next_row().await.unwrap().is_none());
        drop(cursor);
        assert!(fresh.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_unwound_clock_cannot_leave_a_reusable_ordered_cursor_or_session_pin() {
    let ((), report) = run_async_under_lab(0x8600_0007, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let fail = Arc::new(AtomicBool::new(false));
        let callback = Arc::clone(&fail);
        let pins = Arc::strong_count(&db.snapshot);
        let (mut ordered, _, _) = session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || {
                assert!(
                    !callback.load(Ordering::SeqCst),
                    "injected host clock unwind"
                );
                100
            },
            limits(),
        )
        .await;
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
            .unwrap();
        let mut cursor = ordered
            .execute_ordered(&cx, &prepared, &params)
            .unwrap()
            .await
            .unwrap();
        fail.store(true, Ordering::SeqCst);
        let mut read = Box::pin(cursor.next_row());
        let mut context = Context::from_waker(Waker::noop());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                read.as_mut().poll(&mut context)
            }))
            .is_err()
        );
        drop(read);
        assert_eq!(cursor.state(), ScanState::Failed);
        cursor.close();
        drop(cursor);
        assert!(ordered.is_closed());
        assert_eq!(Arc::strong_count(&db.snapshot), pins);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn the_first_future_poll_rechecks_expiry_and_needs_no_retained_template_or_params() {
    let ((), report) = run_async_under_lab(0x8600_0008, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = database(&contexts.commit()).await;
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let now = Arc::new(AtomicU64::new(100));
        let sampled = Arc::clone(&now);
        let (mut ordered, a, b) = session(
            &db,
            &cx,
            &issuer,
            &token,
            &pool,
            move || sampled.load(Ordering::SeqCst),
            limits(),
        )
        .await;
        let params = GqlParameters::new();
        let prepared = ordered
            .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
            .unwrap();
        let future = ordered.execute_ordered(&cx, &prepared, &params).unwrap();
        drop(prepared);
        drop(params);
        now.store(1001, Ordering::SeqCst);
        refusal(future.await, AuthorizationError::Expired);
        assert!(ordered.is_closed());
        assert!(a.0.lock().unwrap().bytes.get_ref().is_empty());
        assert!(b.0.lock().unwrap().bytes.get_ref().is_empty());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn limit_zero_keeps_input_budget_and_pending_query_cancellation_is_nonterminal() {
    let ((), report) = run_async_under_lab(0x8600_0009, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let db = database(&contexts.commit()).await;
        // Separate cancellation ownership keeps the lab's root task alive so
        // it can inspect the interrupted operation's resource/session state.
        // A runtime request context owns it, as fgdb's other cancellation
        // laws do: `Cx::for_testing` needs asupersync's test-internals, which
        // this package never enables (`cargo test -p fgdb` did not compile).
        let runtime = asupersync::runtime::RuntimeBuilder::new().build().unwrap();
        let cancel_root = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
        let query_contexts = PurposeContexts::narrow_runtime_root(&cancel_root);
        let cx = query_contexts.query();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let params = GqlParameters::new();
        let mut small = limits();
        small.max_input_rows = 1;
        let (mut bounded, _, _) = session(&db, &cx, &issuer, &token, &pool, || 100, small).await;
        let prepared = bounded
            .prepare(
                &cx,
                "MATCH (n:L) RETURN n.p AS p ORDER BY p LIMIT 0",
                &params,
            )
            .unwrap();
        assert!(
            matches!(bounded.execute_ordered(&cx, &prepared, &params).unwrap().await,
            Err(NativeSpoolError::Authorized(error))
                if matches!(*error, QueryError::Stream(GqlQueryError::Rows(_))))
        );
        assert!(!bounded.is_closed());
        assert_eq!(pool.used(), 0);
        for during_read in [false, true] {
            let (mut ordered, _, b) =
                session(&db, &cx, &issuer, &token, &pool, || 100, limits()).await;
            let prepared = ordered
                .prepare(&cx, "MATCH (n:L) RETURN n.p AS p ORDER BY p", &params)
                .unwrap();
            if during_read {
                let mut cursor = ordered
                    .execute_ordered(&cx, &prepared, &params)
                    .unwrap()
                    .await
                    .unwrap();
                b.0.lock().unwrap().pending_read = true;
                let mut read = Box::pin(cursor.next_row());
                {
                    let mut context = Context::from_waker(Waker::noop());
                    assert!(read.as_mut().poll(&mut context).is_pending());
                    cancel_root.set_cancel_requested(true);
                    let Poll::Ready(result) = read.as_mut().poll(&mut context) else {
                        panic!("cancelled read stayed pending")
                    };
                    assert!(matches!(result, Err(NativeSpoolError::Authorized(error))
                        if matches!(*error, QueryError::Pattern(GqlQueryError::Interrupted(_)))));
                }
                drop(read);
                assert_eq!(cursor.state(), ScanState::Failed);
                drop(cursor);
            } else {
                b.0.lock().unwrap().pending_write = true;
                let mut future =
                    Box::pin(ordered.execute_ordered(&cx, &prepared, &params).unwrap());
                {
                    let mut context = Context::from_waker(Waker::noop());
                    assert!(future.as_mut().poll(&mut context).is_pending());
                    cancel_root.set_cancel_requested(true);
                    let Poll::Ready(result) = future.as_mut().poll(&mut context) else {
                        panic!("cancelled sort stayed pending")
                    };
                    assert!(matches!(result, Err(NativeSpoolError::Authorized(error))
                        if matches!(*error, QueryError::Pattern(GqlQueryError::Interrupted(_)))));
                }
                drop(future);
            }
            assert!(!ordered.is_closed());
            assert!(ordered.destination.is_poisoned());
            assert_eq!(pool.used(), 0);
            cancel_root.set_cancel_requested(false);
            assert!(
                ordered
                    .prepare(&cx, "MATCH (n:L) RETURN n.p AS p", &params)
                    .is_ok()
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn range_and_prefix_ordered_streams_bound_sort_input_across_capacity_fallback() {
    let ((), report) = run_async_under_lab(0x8600_0021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let issuer = authority();
        for prefix in [false, true] {
            for hidden_values in [0_u128, 70] {
                let mut db =
                    Database::open_memory(&commit, DatabaseKeys::new([0x16; 32], NS, [0x26; 32]))
                        .await
                        .unwrap();
                let value = |number: i64| {
                    if prefix {
                        CanonicalScalar::ucs_basic_text(&format!("Ad{number}")).unwrap()
                    } else {
                        CanonicalScalar::Int(number)
                    }
                };
                let mut batch = WriteBatch::new(RelationId(1));
                for (id, number) in [(0, 30), (3, 20), (u128::MAX, 40)] {
                    batch.create_vertex(
                        VId(id),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), value(number))],
                    );
                }
                for id in 100..356_u128 {
                    let unrelated = if prefix {
                        CanonicalScalar::ucs_basic_text(&format!("zz{id}")).unwrap()
                    } else {
                        CanonicalScalar::Int(-(id as i64))
                    };
                    batch.create_vertex(
                        VId(id),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), unrelated)],
                    );
                }
                for offset in 0..hidden_values {
                    batch.create_vertex(
                        VId(1000 + offset),
                        vec![LabelId(99)],
                        vec![(PropertyKeyId(1), value(300 + offset as i64))],
                    );
                    batch.create_vertex(
                        VId(2000 + offset),
                        vec![LabelId(1)],
                        vec![(PropertyKeyId(1), value(500 + offset as i64))],
                    );
                }
                db.write(&commit, batch).await.unwrap();
                if hidden_values != 0 {
                    let mut change = WriteBatch::new(RelationId(1));
                    for offset in 0..hidden_values {
                        let outside = if prefix {
                            CanonicalScalar::ucs_basic_text("outside").unwrap()
                        } else {
                            CanonicalScalar::Int(-1)
                        };
                        change.set_vertex_property(
                            VId(2000 + offset),
                            PropertyKeyId(1),
                            Some(outside),
                        );
                    }
                    db.write(&commit, change).await.unwrap();
                }
                let mut scoped = grant();
                scoped.limits.max_nodes = 3;
                let token = issuer.issue_at(&scoped, 100).unwrap();
                let broad_token = issuer.issue_at(&grant(), 100).unwrap();
                let pool = MemoryPool::new(32_768, 0).unwrap();
                let (scratch, _) = file(&cx, &pool).await;
                let (destination, _) = file(&cx, &pool).await;
                let mut ordered = db
                    .authorized_ordered_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols as Resolver,
                        GqlQueryPolicy::new(3, 3, 1_000_000, 1_000_000),
                        || 100,
                        scratch,
                        destination,
                        AuthorizedSortLimits {
                            max_input_rows: 3,
                            ..limits()
                        },
                    )
                    .unwrap();
                let filter = if prefix {
                    "n.p STARTS WITH 'Ad'"
                } else {
                    "n.p >= 10 AND n.p < 1000"
                };
                let args = GqlParameters::new();
                for tail in [
                    "RETURN n AS id, n.p AS p ORDER BY p DESC",
                    "RETURN n AS id, n.p AS p ORDER BY p DESC SKIP 1 LIMIT 1",
                    "RETURN n.p AS p ORDER BY p DESC",
                ] {
                    let text = format!("MATCH (n:L) WHERE {filter} {tail}");
                    let eager = db
                        .authorized_read_session(
                            &cx,
                            &issuer,
                            &broad_token,
                            "main",
                            symbols,
                            policy(),
                            || 100,
                        )
                        .unwrap()
                        .query(&cx, &text, &args)
                        .unwrap();
                    let prepared = ordered.prepare(&cx, &text, &args).unwrap();
                    let future = ordered.execute_ordered(&cx, &prepared, &args).unwrap();
                    fn is_send<T: Send>(_: &T) {}
                    is_send(&future);
                    let mut cursor = future.await.unwrap();
                    let columns = cursor.columns().to_vec();
                    let rows = values(&mut cursor)
                        .await
                        .into_iter()
                        .map(|row| row.into_iter().map(QueryValue::Value).collect())
                        .collect();
                    assert_eq!(
                        QueryResult::Rows { columns, rows },
                        eager,
                        "prefix={prefix} hidden_values={hidden_values} {tail}"
                    );
                    drop(cursor);
                    assert_eq!(pool.used(), 0);
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn ordered_range_and_prefix_keep_denied_key_refusals_and_earlier_arithmetic_errors() {
    let ((), report) = run_async_under_lab(0x8600_0022, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let issuer = authority();
        let token = issuer.issue_at(&grant(), 100).unwrap();
        for prefix in [false, true] {
            for matches in [false, true] {
                let mut db =
                    Database::open_memory(&commit, DatabaseKeys::new([0x16; 32], NS, [0x26; 32]))
                        .await
                        .unwrap();
                let hidden = if prefix {
                    CanonicalScalar::ucs_basic_text(if matches { "Ad" } else { "other" }).unwrap()
                } else {
                    CanonicalScalar::Int(if matches { 55 } else { 99 })
                };
                let mut batch = WriteBatch::new(RelationId(1));
                for id in 1..=2 {
                    batch.create_vertex(
                        VId(id),
                        vec![LabelId(1)],
                        vec![
                            (PropertyKeyId(1), CanonicalScalar::Int(0)),
                            (PropertyKeyId(2), hidden.clone()),
                        ],
                    );
                }
                db.write(&commit, batch).await.unwrap();
                let pool = MemoryPool::new(32_768, 0).unwrap();
                let (scratch, _) = file(&cx, &pool).await;
                let (destination, _) = file(&cx, &pool).await;
                let mut ordered = db
                    .authorized_ordered_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols as Resolver,
                        GqlQueryPolicy::new(1, 2, 1_000_000, 1_000_000),
                        || 100,
                        scratch,
                        destination,
                        limits(),
                    )
                    .unwrap();
                let args = GqlParameters::new();
                let text = if prefix {
                    "MATCH (n:L) WHERE n.hidden STARTS WITH 'Ad' RETURN n AS id ORDER BY id DESC"
                } else {
                    "MATCH (n:L) WHERE n.hidden > 50 AND n.hidden < 60 RETURN n AS id ORDER BY id DESC"
                };
                let denied = ordered.prepare(&cx, text, &args).unwrap();
                let error = ordered
                    .execute_ordered(&cx, &denied, &args)
                    .unwrap()
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    NativeSpoolError::Authorized(error)
                        if matches!(*error, QueryError::Stream(GqlQueryError::Rows(error))
                            if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords
                                && error.limit == 1 && error.observed == 2)
                ));
                // As above: a fresh session and files, since a failed execution
                // may poison spill files and poisoning persists across calls.
                drop(ordered);
                let (scratch, _) = file(&cx, &pool).await;
                let (destination, _) = file(&cx, &pool).await;
                let mut ordered = db
                    .authorized_ordered_read_session(
                        &cx,
                        &issuer,
                        &token,
                        "main",
                        symbols as Resolver,
                        GqlQueryPolicy::new(1, 2, 1_000_000, 1_000_000),
                        || 100,
                        scratch,
                        destination,
                        limits(),
                    )
                    .unwrap();
                let text = if prefix {
                    "MATCH (n:L) WHERE 10 / n.p > 0 AND n.p STARTS WITH 'Ad' RETURN n AS id ORDER BY id DESC"
                } else {
                    "MATCH (n:L) WHERE 10 / n.p > 0 AND n.p > 7 RETURN n AS id ORDER BY id DESC"
                };
                let fallible = ordered.prepare(&cx, text, &args).unwrap();
                let error = ordered
                    .execute_ordered(&cx, &fallible, &args)
                    .unwrap()
                    .await
                    .unwrap_err();
                let observed = format!("{error:?}");
                assert!(
                    matches!(
                        error,
                        NativeSpoolError::Authorized(error)
                            if matches!(*error, QueryError::Stream(GqlQueryError::Data(error))
                                if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)
                    ),
                    "{observed}"
                );
                assert_eq!(pool.used(), 0);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
