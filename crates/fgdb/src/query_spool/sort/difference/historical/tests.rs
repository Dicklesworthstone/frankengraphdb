//! Public prepared entrypoints, real graph history and authenticated scratch.
use super::*;
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
fn keys() -> crate::DatabaseKeys {
    crate::DatabaseKeys::new(
        [0x71; 32],
        DatabaseSecurityNamespaceId([0x72; 32]),
        [0x73; 32],
    )
}
async fn history(cx: &CommitCx) -> (Database<crate::MemVfs>, [CommitSeq; 3]) {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = crate::WriteBatch::new(RelationId(1));
    for (id, p, q) in [(1, 3, 30), (2, 3, 10), (3, 7, 20)] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int(p)),
                (PropertyKeyId(2), CanonicalScalar::Int(q)),
            ],
        );
    }
    batch.add_edge(EId(1), VId(1), VId(3), vec![]);
    batch.add_edge(EId(2), VId(1), VId(3), vec![]);
    let first = db.write(cx, batch).await.unwrap();
    let mut batch = crate::WriteBatch::new(RelationId(1));
    batch.delete_vertex(VId(2));
    batch.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(9)));
    batch.create_vertex(
        VId(4),
        vec![],
        vec![
            (PropertyKeyId(1), CanonicalScalar::Int(5)),
            (PropertyKeyId(2), CanonicalScalar::Int(40)),
        ],
    );
    batch.add_edge(EId(3), VId(1), VId(4), vec![]);
    let second = db.write(cx, batch).await.unwrap();
    let mut batch = crate::WriteBatch::new(RelationId(1));
    batch.set_vertex_property(VId(3), PropertyKeyId(1), Some(CanonicalScalar::Int(5)));
    let third = db.write(cx, batch).await.unwrap();
    (db, [first, second, third])
}

type Bag = BTreeMap<Vec<crate::QueryValue>, i128>;
fn value(n: i64) -> Vec<crate::QueryValue> {
    vec![crate::QueryValue::Value(GraphValue::Scalar(
        CanonicalScalar::Int(n),
    ))]
}
async fn collect(cursor: &mut NativeSpoolDifference<'_, File>) -> VisitResult<Bag> {
    let mut changes = Bag::new();
    while let Some((weight, row)) = cursor.next_change().await? {
        let row = GraphValueRow::decode_canonical(row.as_ref()).unwrap();
        let row = row
            .values()
            .iter()
            .cloned()
            .map(crate::QueryValue::Value)
            .collect();
        assert_ne!(weight, 0);
        assert!(changes.insert(row, weight).is_none());
    }
    assert_eq!(cursor.state(), ScanState::Exhausted);
    Ok(changes)
}
#[allow(clippy::too_many_arguments)]
async fn attempt(
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &QueryCx,
    before: CommitSeq,
    after: CommitSeq,
    policy: GqlQueryPolicy,
    work: u64,
) -> VisitResult<(Bag, u64)> {
    attempt_on(cx, prepared, view, cx, before, after, policy, work).await
}
#[allow(clippy::too_many_arguments)]
async fn attempt_on(
    setup: &QueryCx,
    prepared: &PreparedNativeRead,
    view: &EmbeddedReadView,
    cx: &QueryCx,
    before: CommitSeq,
    after: CommitSeq,
    policy: GqlQueryPolicy,
    work: u64,
) -> VisitResult<(Bag, u64)> {
    let pool = MemoryPool::new(64 * 1024, 0).unwrap();
    let (mut source, _) = file(setup, &pool).await;
    let (mut scratch, _) = file(setup, &pool).await;
    let mut cursor = prepared
        .spool_difference_in_view(
            view,
            cx,
            &GqlParameters::new(),
            before,
            after,
            policy,
            &mut source,
            &mut scratch,
            2,
            10_000,
            257,
            8192,
            100_000,
            work,
        )
        .await?;
    let result = collect(&mut cursor).await?;
    Ok((result, cursor.work_units()))
}

#[test]
fn endpoint_orders_distinct_hidden_keys_and_edges_match_complete_historical_queries() {
    let ((), report) = run_async_under_lab(0xd1ff_6101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        for text in [
            "MATCH (n) RETURN n.p AS p",
            "MATCH (n) RETURN DISTINCT n.p AS p",
            "MATCH (n) RETURN n.p AS p ORDER BY p DESC SKIP 1 LIMIT 2",
            "MATCH (n) RETURN n.p AS p ORDER BY n.q DESC LIMIT 2",
            "MATCH (n) RETURN n, n.p AS p",
            "MATCH (a)-[r:R]->(b) RETURN a.p AS a, b.p AS b",
            "MATCH (a)-[r:R]->(b) RETURN r, b.p AS b ORDER BY b DESC LIMIT 2",
        ] {
            let prepared = plan(text);
            for before in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                for after in [CommitSeq::ORIGIN, cuts[0], cuts[1], cuts[2]] {
                    let expected = prepared
                        .diff_in_view(
                            &view,
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
                    let (actual, _) =
                        attempt(&prepared, &view, &cx, before, after, policy(100), u64::MAX)
                            .await
                            .unwrap();
                    assert_eq!(actual, expected, "{text} at {before:?} -> {after:?}");
                }
            }
        }
        // Independent count arithmetic, not just a comparison to another engine.
        assert_eq!(
            attempt(
                &plan("MATCH (n) RETURN n.p AS p"),
                &view,
                &cx,
                cuts[0],
                cuts[1],
                policy(3),
                u64::MAX
            )
            .await
            .unwrap()
            .0,
            Bag::from([(value(3), -2), (value(5), 1), (value(9), 1)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn binding_and_both_history_pins_are_fixed_before_writer_template_and_arguments_drop() {
    let ((), report) = run_async_under_lab(0xd1ff_6102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, cuts) = history(&contexts.commit()).await;
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n) WHERE n.p >= $floor RETURN n.p AS p",
            &params,
            symbols,
        )
        .unwrap();
        let rebound = GqlParameters::new().with_int64("floor", 4).unwrap();
        let pin = Arc::downgrade(&db.snapshot);
        let future = prepared.spool_difference(
            &db,
            &cx,
            &rebound,
            cuts[0],
            cuts[1],
            policy(2),
            &mut source,
            &mut scratch,
            2,
            100,
            257,
            8192,
            100,
            u64::MAX,
        );
        drop(prepared);
        drop(rebound);
        drop(params);
        let mut batch = crate::WriteBatch::new(RelationId(1));
        batch.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(999)));
        db.write(&contexts.commit(), batch).await.unwrap();
        drop(db);
        assert!(pin.upgrade().is_some());
        let mut cursor = future.await.unwrap();
        assert!(pin.upgrade().is_none()); // completed spools retain no graph
        assert_eq!((cursor.before(), cursor.after()), (cuts[0], cuts[1]));
        assert_eq!(cursor.columns(), ["p"]);
        drop(source); // the cursor retains only the final scratch file
        assert_eq!(
            collect(&mut cursor).await.unwrap(),
            Bag::from([(value(5), 1), (value(9), 1)])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn endpoint_and_shape_refusals_and_unpolled_opening_do_not_start_scratch_io() {
    let ((), report) = run_async_under_lab(0xd1ff_6103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (mut db, cuts) = history(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, a) = file(&cx, &pool).await;
        let (mut scratch, b) = file(&cx, &pool).await;
        for text in [
            "MATCH (n) RETURN COUNT(*) AS n",
            "MATCH (n) RETURN n.p AS p UNION ALL MATCH (m) RETURN m.p AS p",
            "MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS p",
        ] {
            let error = plan(text)
                .spool_difference_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    cuts[0],
                    cuts[1],
                    policy(0),
                    &mut source,
                    &mut scratch,
                    1,
                    100,
                    257,
                    8192,
                    100,
                    u64::MAX,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Source(NativeSpoolError::Prepare(error))
                if matches!(*error, QueryError::StreamingUnsupported { .. }))
            );
        }
        let prepared = plan("MATCH (n) RETURN n.p AS p LIMIT 0");
        let future = prepared.spool_difference_in_view(
            &view,
            &cx,
            &GqlParameters::new(),
            cuts[0],
            cuts[1],
            policy(0),
            &mut source,
            &mut scratch,
            1,
            100,
            257,
            8192,
            100,
            u64::MAX,
        );
        drop(future);
        let future_cut = cuts[2].checked_successor().unwrap();
        let mut edit = crate::WriteBatch::new(RelationId(1));
        edit.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(10)));
        assert_eq!(
            db.write(&contexts.commit(), edit).await.unwrap(),
            future_cut
        );
        // The cut now exists in the writer, but not in the previously pinned view.
        let error = prepared
            .spool_difference_in_view(
                &view,
                &cx,
                &GqlParameters::new(),
                cuts[0],
                future_cut,
                policy(0),
                &mut source,
                &mut scratch,
                1,
                100,
                257,
                8192,
                100,
                u64::MAX,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Source(NativeSpoolError::Prepare(error))
            if matches!(*error, QueryError::Read(_)))
        );
        for (run_rows, max_runs, page_bytes, work) in [
            (0, 10, 257, 100),
            (1, 0, 257, 100),
            (1, 10, 0, 100),
            (1, 10, 257, 0),
        ] {
            assert!(
                prepared
                    .spool_difference_in_view(
                        &view,
                        &cx,
                        &GqlParameters::new(),
                        cuts[0],
                        cuts[1],
                        policy(0),
                        &mut source,
                        &mut scratch,
                        run_rows,
                        max_runs,
                        page_bytes,
                        8192,
                        100,
                        work
                    )
                    .await
                    .is_err()
            );
        }
        for backing in [a, b] {
            let state = backing.0.lock().unwrap();
            assert_eq!((state.reads, state.writes), (0, 0));
        }
        assert_eq!(source.stats().reserved_runs, 0);
        assert_eq!(scratch.stats().reserved_runs, 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn both_source_traversals_spend_one_policy_and_only_the_final_delta_spends_rows() {
    let ((), report) = run_async_under_lab(0xd1ff_6104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan("MATCH (n) RETURN n.p AS p");
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        // Measure ONE endpoint through the ordinary source/pipeline, then set
        // an independently summed policy for two equal traversals. The empty
        // difference still owes both source admissions and private occurrences.
        let opened = open(
            &prepared,
            &view,
            &cx,
            &GqlParameters::new(),
            cuts[0],
            cuts[0],
        )
        .unwrap();
        let (input, tail) = opened.before;
        let (one, _) = super::super::super::ordered_input(
            &cx,
            opened.columns,
            input.cursor(&cx, private_policy(policy(0)), 100_000),
            tail,
            private_policy(policy(0)),
            &mut scratch,
            &mut source,
            2,
            10_000,
            257,
            8192,
            u64::MAX,
        )
        .await
        .unwrap();
        let records = one.rows.snapshot_records * 2;
        let work = one.evaluator.work_units * 2;
        let scratch_units = one.evaluator.scratch_entries * 2;
        assert!(records > 0 && work > 0 && scratch_units > 0);
        let exact = GqlQueryPolicy::new(records, 0, work, scratch_units);
        assert!(
            attempt(&prepared, &view, &cx, cuts[0], cuts[0], exact, u64::MAX)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        for short in [
            GqlQueryPolicy::new(records - 1, 0, work, scratch_units),
            GqlQueryPolicy::new(records, 0, work - 1, scratch_units),
            GqlQueryPolicy::new(records, 0, work, scratch_units - 1),
        ] {
            let error = attempt(&prepared, &view, &cx, cuts[0], cuts[0], short, u64::MAX)
                .await
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Source(NativeSpoolError::Execute(error))
                if matches!(*error, GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_)))
            );
        }
        assert!(
            attempt(&prepared, &view, &cx, cuts[0], cuts[1], policy(0), u64::MAX)
                .await
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn physical_work_accumulates_across_endpoint_windows_difference_sorts_and_eof() {
    let ((), report) = run_async_under_lab(0xd1ff_6105, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan("MATCH (n) RETURN n.p AS p ORDER BY p DESC LIMIT 2");
        let (expected, used) = attempt(
            &prepared,
            &view,
            &cx,
            cuts[0],
            cuts[1],
            policy(100),
            u64::MAX,
        )
        .await
        .unwrap();
        assert!(used > 0);
        let (exact, exact_used) =
            attempt(&prepared, &view, &cx, cuts[0], cuts[1], policy(100), used)
                .await
                .unwrap();
        assert_eq!((exact, exact_used), (expected, used));
        let error = attempt(
            &prepared,
            &view,
            &cx,
            cuts[0],
            cuts[1],
            policy(100),
            used - 1,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, GqlQueryError::Source(NativeSpoolError::SortWorkLimit { attempted, limit })
            if attempted == used && limit == used - 1)
        );
        let rebased = physical_error(
            NativeSpoolError::SortWorkLimit {
                attempted: 7,
                limit: 6,
            },
            11,
            17,
        );
        assert!(matches!(
            rebased,
            NativeSpoolError::SortWorkLimit {
                attempted: 18,
                limit: 17
            }
        ));
        assert!(subtract(4, 5).is_err()); // no saturation launders invalid counters
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_windows_still_evaluate_late_endpoint_errors_without_publishing_a_delta() {
    let ((), report) = run_async_under_lab(0xd1ff_6106, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan("MATCH (n) WHERE 100 / (n.p - 9) < 0 RETURN n.p AS p LIMIT 0");
        // Control proves physical admission, not a blanket unsupported refusal.
        assert!(
            attempt(&prepared, &view, &cx, cuts[0], cuts[0], policy(0), u64::MAX)
                .await
                .unwrap()
                .0
                .is_empty()
        );
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, backing) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let error = prepared
            .spool_difference_in_view(
                &view,
                &cx,
                &GqlParameters::new(),
                cuts[0],
                cuts[1],
                policy(0),
                &mut source,
                &mut scratch,
                2,
                100,
                257,
                8192,
                100,
                u64::MAX,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            GqlQueryError::Source(NativeSpoolError::Execute(_))
        ));
        assert!(backing.0.lock().unwrap().writes > 0); // first endpoint completed
        assert_eq!(db.frontier().unwrap(), cuts[2]);
        assert!(
            prepared
                .diff_in_view(
                    &view,
                    &cx,
                    &GqlParameters::new(),
                    cuts[0],
                    cuts[1],
                    policy(0)
                )
                .is_err()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn duplicate_endpoint_results_larger_than_the_pool_become_compressed_changes() {
    let ((), report) = run_async_under_lab(0xd1ff_6107, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let payload = CanonicalScalar::ucs_basic_text(&"payload:é\0".repeat(80)).unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        for id in 1..=256 {
            batch.create_vertex(VId(id), vec![], vec![(PropertyKeyId(1), payload.clone())]);
        }
        let before = db.write(&commit, batch).await.unwrap();
        let mut batch = crate::WriteBatch::new(RelationId(1));
        batch.delete_vertex(VId(256));
        let after = db.write(&commit, batch).await.unwrap();
        let pool = MemoryPool::new(64 * 1024, 0).unwrap();
        let (mut source, _) = file(&cx, &pool).await;
        let (mut scratch, _) = file(&cx, &pool).await;
        let mut cursor = plan("MATCH (n) RETURN n.p AS p")
            .spool_difference(
                &db,
                &cx,
                &GqlParameters::new(),
                before,
                after,
                policy(1),
                &mut source,
                &mut scratch,
                2,
                10_000,
                257,
                8192,
                1000,
                u64::MAX,
            )
            .await
            .unwrap();
        assert!(source.stats().reserved_bytes > pool.limit() as u64);
        assert_eq!(
            collect(&mut cursor).await.unwrap(),
            Bag::from([(
                vec![crate::QueryValue::Value(GraphValue::Scalar(payload))],
                -1,
            )])
        );
        drop(cursor);
        drop(source);
        drop(scratch);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn every_observed_checkpoint_refuses_without_fabricating_complete_delivery() {
    let ((), report) = run_async_under_lab(0xd1ff_6108, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, cuts) = history(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let prepared = plan("MATCH (n) RETURN n.p AS p LIMIT 1");
        let probe = Arc::new(SimulationCheckpointProbe::new(None));
        let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
        attempt_on(
            &cx,
            &prepared,
            &view,
            &observed,
            cuts[0],
            cuts[1],
            policy(2),
            u64::MAX,
        )
        .await
        .unwrap();
        assert!(probe.calls() > 5);
        for stop in 1..=probe.calls() {
            let control = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
            let interrupted = cx.with_checkpoint_probe(Arc::clone(&control));
            assert!(
                attempt_on(
                    &cx,
                    &prepared,
                    &view,
                    &interrupted,
                    cuts[0],
                    cuts[1],
                    policy(2),
                    u64::MAX
                )
                .await
                .is_err(),
                "checkpoint {stop}"
            );
        }
        // Refusals neither advance the graph nor damage the retained history.
        assert_eq!(db.frontier().unwrap(), cuts[2]);
        attempt(&prepared, &view, &cx, cuts[0], cuts[1], policy(2), u64::MAX)
            .await
            .unwrap();
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
