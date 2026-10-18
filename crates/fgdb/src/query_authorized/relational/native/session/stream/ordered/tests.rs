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
            assert!(matches!(
                error,
                NativeSpoolError::Authorized(error)
                    if matches!(*error, QueryError::Stream(GqlQueryError::Data(error))
                        if error.kind == fgdb_gql::GraphIntegerErrorKind::DivisionByZero)
            ));
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
        // A set that reads a relationship has no ordered pull. It refuses
        // before any scratch is written, with no eager fallback.
        let scratch_bytes = a.0.lock().unwrap().bytes.get_ref().len();
        let destination_bytes = b.0.lock().unwrap().bytes.get_ref().len();
        let edge = ordered
            .prepare(
                &cx,
                "MATCH (n:L)-[e:R]->(m:L) RETURN 1 AS one ORDER BY one",
                &params,
            )
            .unwrap();
        assert!(matches!(ordered.execute_ordered(&cx, &edge, &params),
            Err(NativeSpoolError::Authorized(error)) if matches!(*error, QueryError::StreamingUnsupported { .. })));
        assert_eq!(a.0.lock().unwrap().bytes.get_ref().len(), scratch_bytes);
        assert_eq!(b.0.lock().unwrap().bytes.get_ref().len(), destination_bytes);
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
