//! Authorized callbacks drive real history, scoped scans and authenticated spill.
use super::*;
use crate::{DatabaseKeys, MemVfs, QueryValue, WriteBatch};
use asupersync::io::ReadBuf;
use asupersync::lab::run_async_under_lab;
use asupersync::security::key::AuthKey;
use fgdb_delta_types::{LabelId, PropertyKeyId, SchemaEpoch};
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::{MemoryPool, SpillLimits};
use fgdb_types::{CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts};
use fgdb_warden::{Grant, LimitDimension, QueryLimits, Scope};
use std::collections::BTreeMap;
use std::future::{pending, ready};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Waker;

const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0xd8; 32]);
const P: PropertyKeyId = PropertyKeyId(1);
const H: PropertyKeyId = PropertyKeyId(2);
const R: RelationId = RelationId(1);
const ROWS: &str = "MATCH (n) RETURN n.p AS p";
type Resolver = fn(GraphSymbolKind, &str) -> Option<GraphSymbol>;
type Session<'a, C> = AuthorizedOrderedReadSession<'a, Resolver, C, File, File>;
type Bag = BTreeMap<Vec<QueryValue>, i128>;

fn authority() -> Authority {
    Authority::new(AuthKey::from_seed(0xd801), NS, "graph", SchemaEpoch(0), 1).unwrap()
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
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(100_000, rows, 10_000_000, 10_000_000)
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
        (GraphSymbolKind::Property, "hidden") => Some(GraphSymbol::Property(H)),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        _ => None,
    }
}
async fn history(cx: &CommitCx, hidden: bool) -> (Database<MemVfs>, [CommitSeq; 3]) {
    let mut db = Database::open_memory(cx, DatabaseKeys::new([0x18; 32], NS, [0x28; 32]))
        .await
        .unwrap();
    let mut batch = WriteBatch::new(R);
    for (id, value) in [(1, 3), (2, 3), (3, 7)] {
        let mut properties = vec![(P, CanonicalScalar::Int(value))];
        if hidden {
            properties.push((H, CanonicalScalar::Int(9000 + value)));
        }
        batch.create_vertex(VId(id), vec![LabelId(1)], properties);
    }
    batch.add_edge(EId(1), VId(1), VId(3), vec![]);
    if hidden {
        batch.create_vertex(
            VId(99),
            vec![LabelId(99)],
            vec![(P, CanonicalScalar::Int(100))],
        );
        batch.add_edge(EId(2), VId(99), VId(1), vec![]);
    }
    let first = db.write(cx, batch).await.unwrap();
    let mut batch = WriteBatch::new(R);
    batch.delete_vertex(VId(2));
    batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(9)));
    if hidden {
        batch.set_vertex_label(VId(3), LabelId(1), false);
        batch.set_vertex_label(VId(3), LabelId(99), true);
    } else {
        batch.delete_vertex(VId(3));
    }
    let second = db.write(cx, batch).await.unwrap();
    let mut batch = WriteBatch::new(R);
    if hidden {
        batch.set_vertex_label(VId(99), LabelId(99), false);
        batch.set_vertex_label(VId(99), LabelId(1), true);
        batch.set_vertex_property(VId(3), P, Some(CanonicalScalar::Int(12345)));
    } else {
        batch.create_vertex(
            VId(99),
            vec![LabelId(1)],
            vec![(P, CanonicalScalar::Int(100))],
        );
        batch.add_edge(EId(2), VId(99), VId(1), vec![]);
    }
    let third = db.write(cx, batch).await.unwrap();
    (db, [first, second, third])
}

#[derive(Default)]
struct FileState {
    bytes: Cursor<Vec<u8>>,
    reads: usize,
    writes: usize,
    pending_write: bool,
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
        let at = state.bytes.position() as usize;
        let n = out
            .remaining()
            .min(state.bytes.get_ref().len().saturating_sub(at));
        if n != 0 {
            out.put_slice(&state.bytes.get_ref()[at..at + n]);
            state.bytes.set_position((at + n) as u64);
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
async fn session<'a, C: FnMut() -> u64>(
    db: &Database<MemVfs>,
    cx: &QueryCx,
    authority: &'a Authority,
    grant: &Grant,
    pool: &MemoryPool,
    clock: C,
    policy: GqlQueryPolicy,
) -> (Session<'a, C>, File, File) {
    let token = authority.issue_at(grant, 100).unwrap();
    let (scratch, a) = file(cx, pool).await;
    let (destination, b) = file(cx, pool).await;
    (
        db.authorized_ordered_read_session(
            cx,
            authority,
            &token,
            "main",
            symbols as Resolver,
            policy,
            clock,
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
fn value(n: i64) -> Vec<QueryValue> {
    vec![QueryValue::Value(GraphValue::Scalar(CanonicalScalar::Int(
        n,
    )))]
}
fn refusal<T>(result: Result<T>, expected: AuthorizationError) {
    assert!(matches!(result, Err(NativeSpoolError::Authorized(error))
        if matches!(*error, QueryError::Authorization(actual) if actual == expected)));
}
async fn collect<C: FnMut() -> u64 + Send>(
    session: &mut Session<'_, C>,
    cx: &QueryCx,
    prepared: &AuthorizedPreparedRead,
    params: &GqlParameters,
    before: CommitSeq,
    after: CommitSeq,
) -> Result<(Vec<String>, Bag)> {
    let mut bag = Bag::new();
    let columns = session
        .visit_difference(cx, prepared, params, before, after, |weight, row| {
            assert_ne!(weight, 0);
            assert!(bag.insert(cells(&row), weight).is_none());
            ready(Ok(()))
        })?
        .await?;
    Ok((columns, bag))
}

#[test]
fn both_endpoint_masks_match_physically_absent_histories_before_order_and_subtraction() {
    let ((), report) = run_async_under_lab(0xd1ff_8101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let (visible, expected_cuts) = history(&contexts.commit(), false).await;
        assert_eq!(cuts, expected_cuts);
        let view = visible.read_session().unwrap();
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        for text in [
            ROWS,
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2",
            "MATCH (n) RETURN n.p AS p ORDER BY n.hidden DESC, p SKIP 1 LIMIT 2",
            "MATCH (n) RETURN n.hidden AS secret",
            "MATCH (a)-[e:R]->(b) RETURN a.p AS a, b.p AS b",
            "MATCH (n) RETURN 1 AS one ORDER BY one",
        ] {
            let (mut scoped, _, _) =
                session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
            let prepared = scoped.prepare(&cx, text, &params).unwrap();
            let native = PreparedNativeRead::prepare(text, &params, symbols).unwrap();
            for before in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                for after in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                    let expected = native
                        .diff_in_view(&view, &cx, &params, before, after, policy(100))
                        .unwrap();
                    let expected: Bag = expected
                        .changes()
                        .iter()
                        .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
                        .collect();
                    assert_eq!(
                        collect(&mut scoped, &cx, &prepared, &params, before, after)
                            .await
                            .unwrap()
                            .1,
                        expected,
                        "{text}: {before:?} -> {after:?}"
                    );
                    assert_eq!(pool.used(), 0);
                }
            }
        }
        // Independent occurrence arithmetic for visibility loss AND gain.
        let (mut scoped, _, _) =
            session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        assert_eq!(
            collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[1])
                .await
                .unwrap()
                .1,
            Bag::from([(value(3), -2), (value(7), -1), (value(9), 1)])
        );
        assert_eq!(
            collect(&mut scoped, &cx, &prepared, &params, cuts[1], cuts[2])
                .await
                .unwrap()
                .1,
            Bag::from([(value(100), 1)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn exact_session_binding_and_both_cut_fences_precede_scratch_io() {
    let ((), report) = run_async_under_lab(0xd1ff_8102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let (mut scoped, a, b) =
            session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
        let (mut foreign, _, _) =
            session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
        let other = foreign.prepare(&cx, ROWS, &params).unwrap();
        refusal(
            scoped.visit_difference(
                &cx,
                &other,
                &params,
                CommitSeq(u64::MAX),
                cuts[0],
                |_, _| ready(Ok(())),
            ),
            AuthorizationError::WrongAuthority,
        );
        for text in [
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
            "MATCH (n) RETURN COUNT(*) AS n",
        ] {
            let prepared = scoped.prepare(&cx, text, &params).unwrap();
            assert!(
                scoped
                    .visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], |_, _| ready(Ok(
                        ()
                    )))
                    .is_err()
            );
        }
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        for (before, after) in [
            (CommitSeq(u64::MAX), cuts[0]),
            (cuts[0], CommitSeq(u64::MAX)),
        ] {
            assert!(
                scoped
                    .visit_difference(&cx, &prepared, &params, before, after, |_, _| ready(Ok(())))
                    .is_err()
            );
        }
        let future = scoped
            .visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], |_, _| {
                ready(Ok(()))
            })
            .unwrap();
        drop(future);
        for backing in [a, b] {
            let state = backing.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert!(!scoped.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn parameter_rebinding_and_history_pin_survive_writer_and_template_destruction() {
    let ((), report) = run_async_under_lab(0xd1ff_8103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, cuts) = history(&contexts.commit(), true).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut scoped, _, _) =
            session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = scoped
            .prepare(
                &cx,
                "MATCH (n) WHERE n.p >= $floor RETURN n.p AS p",
                &params,
            )
            .unwrap();
        let rebound = GqlParameters::new().with_int64("floor", 4).unwrap();
        let mut delivered = Bag::new();
        let future = scoped
            .visit_difference(&cx, &prepared, &rebound, cuts[0], cuts[1], |weight, row| {
                delivered.insert(cells(&row), weight);
                ready(Ok(()))
            })
            .unwrap();
        drop(prepared);
        drop(params);
        drop(rebound);
        let mut batch = WriteBatch::new(R);
        batch.set_vertex_property(VId(1), P, Some(CanonicalScalar::Int(999)));
        db.write(&contexts.commit(), batch).await.unwrap();
        drop(db);
        assert_eq!(future.await.unwrap(), ["p"]);
        assert_eq!(delivered, Bag::from([(value(7), -1), (value(9), 1)]));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn signed_and_native_budgets_span_both_endpoints_and_charge_only_changed_output() {
    let ((), report) = run_async_under_lab(0xd1ff_8104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        let mut bounded = grant();
        bounded.limits.max_rows = 0;
        let (mut scoped, _, _) =
            session(&db, &cx, &authority, &bounded, &pool, || 100, policy(0)).await;
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        assert!(
            collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[0])
                .await
                .unwrap()
                .1
                .is_empty()
        );
        assert!(
            collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[1])
                .await
                .is_err()
        );
        assert!(!scoped.is_closed());
        bounded.limits.max_rows = 100;
        bounded.limits.max_nodes = 3;
        let (mut scoped, _, _) =
            session(&db, &cx, &authority, &bounded, &pool, || 100, policy(100)).await;
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        refusal(
            collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[0]).await,
            AuthorizationError::LimitExceeded(LimitDimension::Nodes),
        );
        bounded.limits.max_nodes = 100_000;
        bounded.limits.max_rows = 1;
        let (mut scoped, _, _) =
            session(&db, &cx, &authority, &bounded, &pool, || 100, policy(100)).await;
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        refusal(
            collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[1]).await,
            AuthorizationError::LimitExceeded(LimitDimension::Rows),
        );
        let mut native = policy(100);
        native.rows = GqlExecutionBudget::new(3, 100);
        let (mut scoped, _, _) =
            session(&db, &cx, &authority, &grant(), &pool, || 100, native).await;
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        assert!(
            collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[0])
                .await
                .is_err()
        );
        assert!(!scoped.is_closed());
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn expiry_while_scratch_or_callback_is_pending_stops_without_another_delivery() {
    let ((), report) = run_async_under_lab(0xd1ff_8105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        for block_in_callback in [false, true] {
            let now = Arc::new(AtomicU64::new(100));
            let clock = Arc::clone(&now);
            let (mut scoped, backing, _) = session(
                &db,
                &cx,
                &authority,
                &grant(),
                &pool,
                move || clock.load(Ordering::SeqCst),
                policy(100),
            )
            .await;
            let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
            if !block_in_callback {
                backing.0.lock().unwrap().pending_write = true;
            }
            let calls = Arc::new(AtomicU64::new(0));
            let received = Arc::clone(&calls);
            let mut future = scoped
                .visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], move |_, _| {
                    received.fetch_add(1, Ordering::SeqCst);
                    pending::<Result<()>>()
                })
                .unwrap();
            let mut task = Context::from_waker(Waker::noop());
            assert!(future.as_mut().poll(&mut task).is_pending());
            assert_eq!(calls.load(Ordering::SeqCst), u64::from(block_in_callback));
            now.store(1000, Ordering::SeqCst);
            let outcome = match future.as_mut().poll(&mut task) {
                Poll::Ready(outcome) => outcome,
                Poll::Pending => panic!("expiry must not wait for blocked I/O or callback"),
            };
            refusal(outcome, AuthorizationError::Expired);
            drop(future);
            assert!(scoped.is_closed());
            assert_eq!(calls.load(Ordering::SeqCst), u64::from(block_in_callback));
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn callback_errors_preserve_identity_but_cannot_hide_retirement_or_unwind() {
    let ((), report) = run_async_under_lab(0xd1ff_8106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        for mode in 0..3 {
            let authority = authority();
            let (mut scoped, _, _) =
                session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
            let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
            let calls = AtomicU64::new(0);
            let mut future = scoped
                .visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if mode == 1 {
                        let _ = authority.retire();
                    }
                    assert_ne!(mode, 2, "injected callback unwind");
                    ready(Err(NativeSpoolError::RowTooLarge {
                        bytes: 987,
                        limit: 123,
                    }))
                })
                .unwrap();
            // The Context is not Send: end it before the next await.
            let outcome = {
                let mut task = Context::from_waker(Waker::noop());
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    future.as_mut().poll(&mut task)
                }))
            };
            drop(future);
            if mode == 2 {
                assert!(outcome.is_err());
                assert!(scoped.is_closed());
            } else {
                let Poll::Ready(outcome) = outcome.unwrap() else {
                    panic!("ready backends");
                };
                if mode == 1 {
                    refusal(outcome, AuthorizationError::AuthorityRetired);
                    assert!(scoped.is_closed());
                } else {
                    assert!(matches!(
                        outcome,
                        Err(NativeSpoolError::RowTooLarge {
                            bytes: 987,
                            limit: 123
                        })
                    ));
                    assert!(!scoped.is_closed());
                    assert!(
                        !collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[1])
                            .await
                            .unwrap()
                            .1
                            .is_empty()
                    );
                }
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn late_second_endpoint_failure_and_signed_work_refusal_publish_no_changes() {
    let ((), report) = run_async_under_lab(0xd1ff_8107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit(), true).await;
        let authority = authority();
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let params = GqlParameters::new();
        for suffix in ["", " LIMIT 0"] {
            let (mut scoped, _, _) =
                session(&db, &cx, &authority, &grant(), &pool, || 100, policy(100)).await;
            let text = format!("MATCH (n) RETURN 100 / (n.p - 9) AS x{suffix}");
            let prepared = scoped.prepare(&cx, &text, &params).unwrap();
            // A property-only source cannot keep eager source-row ordinals
            // in an identity-ordered pull. Preserve its early typed refusal.
            assert!(matches!(
                scoped.visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], |_, _| ready(Ok(()))),
                Err(NativeSpoolError::Authorized(error))
                    if matches!(*error, QueryError::StreamingUnsupported { .. })
            ));
            assert_eq!(scoped.scratch.stats().published_runs, 0);
            assert_eq!(scoped.destination.stats().published_runs, 0);
            assert!(!scoped.is_closed());
            // Leading identity makes the source's canonical order exactly
            // the pull order. The second endpoint must reach its real late
            // arithmetic failure, even when the public page is empty.
            let text = format!("MATCH (n) RETURN n AS id, 100 / (n.p - 9) AS x{suffix}");
            let prepared = scoped.prepare(&cx, &text, &params).unwrap();
            let calls = AtomicU64::new(0);
            let outcome = scoped
                .visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], |_, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    ready(Ok(()))
                })
                .unwrap()
                .await;
            assert!(outcome.is_err());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(!scoped.is_closed());
            assert!(
                scoped.destination.stats().published_runs > 0,
                "the first endpoint must have finished privately"
            );
            assert!(
                collect(&mut scoped, &cx, &prepared, &params, cuts[0], cuts[0])
                    .await
                    .unwrap()
                    .1
                    .is_empty()
            );
        }
        let mut bounded = grant();
        bounded.limits.max_work = limits().max_sort_work - 1;
        let (mut scoped, a, b) =
            session(&db, &cx, &authority, &bounded, &pool, || 100, policy(100)).await;
        let prepared = scoped.prepare(&cx, ROWS, &params).unwrap();
        refusal(
            scoped.visit_difference(&cx, &prepared, &params, cuts[0], cuts[1], |_, _| {
                ready(Ok(()))
            }),
            AuthorizationError::LimitExceeded(LimitDimension::Work),
        );
        for backing in [a, b] {
            let state = backing.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
