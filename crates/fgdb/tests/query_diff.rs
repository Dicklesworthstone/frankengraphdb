//! Real native endpoint reads feed exact result-bag differences, not event replay.
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, EmbeddedReadView, MemVfs, PreparedNativeRead, QueryError, QueryResult,
    ReadError, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::result_diff::{DiffEndpoint, GraphDiffError, GraphResultDiff};
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphAggregateValue,
    GraphSymbolKind, RelationBind,
};
use fgdb_types::{
    CanonicalScalar, CommitSeq, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::collections::BTreeMap;

type Bag = BTreeMap<Vec<GraphAggregateValue>, i128>;
const L: LabelId = LabelId(1);
const R: RelationId = RelationId(1);
const SCORE: PropertyKeyId = PropertyKeyId(1);
const BUCKET: PropertyKeyId = PropertyKeyId(2);
const SCORES: &str = "MATCH (n:L) RETURN ALL n.score AS score";
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x45; 32],
        DatabaseSecurityNamespaceId([0x46; 32]),
        [0x47; 32],
    )
}
fn symbols() -> RelationBind {
    RelationBind::new()
        .with_label("L", L)
        .with_relation("R", R)
        .with_property("score", SCORE)
        .with_property("bucket", BUCKET)
}
fn wide() -> GqlQueryPolicy {
    GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
}
fn value(n: i64) -> GraphAggregateValue {
    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Int(n)))
}
fn observed(result: &GraphResultDiff) -> Bag {
    result
        .changes()
        .iter()
        .map(|(row, weight)| (row.to_vec(), weight.to_i128().unwrap()))
        .collect()
}
fn bag(result: QueryResult) -> (Vec<String>, Bag) {
    let QueryResult::Rows { columns, rows } = result else {
        panic!("expected read rows")
    };
    let mut bag = Bag::new();
    for row in rows {
        *bag.entry(row).or_default() += 1;
    }
    (columns, bag)
}
fn difference(before: &Bag, after: &Bag) -> Bag {
    let mut result = after.clone();
    for (row, count) in before {
        *result.entry(row.clone()).or_default() -= count;
    }
    result.retain(|_, count| *count != 0);
    result
}
fn seed() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    for (id, score, bucket) in [
        (0, Some(7), Some(1)),
        (1, Some(7), Some(1)),
        (2, None, Some(2)),
        (u128::MAX, Some(-3), None),
    ] {
        let mut props = Vec::new();
        if let Some(score) = score {
            props.push((SCORE, CanonicalScalar::Int(score)));
        }
        if let Some(bucket) = bucket {
            props.push((BUCKET, CanonicalScalar::Int(bucket)));
        }
        batch.create_vertex(VId(id), vec![L], props);
    }
    batch.add_edge(
        EId(0),
        VId(0),
        VId(1),
        vec![(SCORE, CanonicalScalar::Int(5))],
    );
    batch.add_edge(
        EId(1),
        VId(0),
        VId(1),
        vec![(SCORE, CanonicalScalar::Int(5))],
    );
    batch.add_edge(EId(u128::MAX), VId(2), VId(2), vec![]);
    batch
}
fn edit() -> WriteBatch {
    let mut batch = WriteBatch::new(R);
    batch.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(9)));
    batch.set_vertex_property(VId(u128::MAX), BUCKET, Some(CanonicalScalar::Int(2)));
    batch.delete_edge(EId(0));
    batch.add_edge(
        EId(3),
        VId(1),
        VId(2),
        vec![(SCORE, CanonicalScalar::Int(-4))],
    );
    batch
}
fn check(
    old: &EmbeddedReadView,
    new: &EmbeddedReadView,
    cx: &fgdb_types::QueryCx,
    text: &str,
    params: &GqlParameters,
) -> GraphResultDiff {
    let (columns, before) = bag(old.query(cx, text, params, symbols(), wide()).unwrap());
    let (after_columns, after) = bag(new.query(cx, text, params, symbols(), wide()).unwrap());
    assert_eq!(columns, after_columns);
    let result = new
        .query_diff(
            cx,
            text,
            params,
            symbols(),
            old.frontier(),
            new.frontier(),
            wide(),
        )
        .unwrap();
    assert_eq!(result.columns(), columns);
    assert_eq!(observed(&result), difference(&before, &after), "{text}");
    assert_eq!(
        result.row_stats().result_rows,
        result.changes().len() as u64
    );
    let reverse = new
        .query_diff(
            cx,
            text,
            params,
            symbols(),
            new.frontier(),
            old.frontier(),
            wide(),
        )
        .unwrap();
    assert_eq!(
        observed(&reverse),
        observed(&result)
            .into_iter()
            .map(|(r, w)| (r, -w))
            .collect()
    );
    result
}

#[test]
fn duplicates_pages_reverse_endpoints_and_net_cancellation_are_exact() {
    let ((), report) = run_async_under_lab(0xd1ff_1001, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let empty = db.read_session().unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.read_session().unwrap();
        db.write(&commit, edit()).await.unwrap();
        let after = db.read_session().unwrap();
        let args = GqlParameters::new();
        let initial = check(&empty, &before, &cx, SCORES, &args);
        assert_eq!(initial.before(), CommitSeq(0));
        assert_eq!(initial.after(), CommitSeq(1));
        assert_eq!(observed(&initial)[&vec![value(7)]], 2);
        let result = check(&before, &after, &cx, SCORES, &args);
        assert_eq!(
            observed(&result),
            Bag::from([(vec![value(7)], -1), (vec![value(9)], 1)])
        );
        let distinct = check(
            &before,
            &after,
            &cx,
            "MATCH (n:L) RETURN DISTINCT n.score AS score",
            &args,
        );
        assert_eq!(observed(&distinct), Bag::from([(vec![value(9)], 1)]));
        let selected = check(
            &before,
            &after,
            &cx,
            "MATCH (n:L) RETURN n.score AS score ORDER BY score DESC NULLS LAST LIMIT 1",
            &args,
        );
        assert_eq!(observed(&selected), observed(&result));
        let selected = check(
            &before,
            &after,
            &cx,
            "MATCH (n:L) RETURN n.score AS score ORDER BY score DESC NULLS LAST SKIP 1 LIMIT 1",
            &args,
        );
        assert!(selected.changes().is_empty());
        let text = "MATCH (n:L) WHERE n.score >= $floor RETURN n.score AS score";
        let args = GqlParameters::new().with_int64("floor", 8).unwrap();
        assert_eq!(
            observed(&check(&before, &after, &cx, text, &args)),
            Bag::from([(vec![value(9)], 1)])
        );
        let mut restore = WriteBatch::new(R);
        restore.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(7)));
        let restored = db.write(&commit, restore).await.unwrap();
        let net = db
            .query_diff(
                &cx,
                SCORES,
                &GqlParameters::new(),
                symbols(),
                before.frontier(),
                restored,
                wide(),
            )
            .unwrap();
        assert!(net.changes().is_empty()); // intervening changes are not CDC events.
        assert_eq!(net.before(), before.frontier());
        assert_eq!(net.after(), restored);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn complete_native_families_keep_groups_aliases_compound_inputs_and_exact_cells() {
    let ((), report) = run_async_under_lab(0xd1ff_1002, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        db.write(&commit, seed()).await.unwrap();
        let before = db.read_session().unwrap();
        db.write(&commit, edit()).await.unwrap();
        let after = db.read_session().unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n:L) RETURN n AS id, n.score AS score",
            "MATCH (n:L) RETURN COUNT(*) AS n, SUM(n.score) AS sum, AVG(n.score) AS avg, COUNT(DISTINCT n.score) AS d",
            "MATCH (n:L) RETURN SUM(n.score) AS total, n.bucket AS bucket, AVG(n.score) AS avg GROUP BY n.bucket",
            "MATCH (n:L) RETURN n.bucket AS first, n.bucket AS again, COUNT(*) AS n GROUP BY n.bucket",
            "MATCH (n:L) RETURN n.bucket AS bucket, SUM(n.score) AS total GROUP BY n.bucket HAVING total > 0 ORDER BY total DESC LIMIT 1",
            "MATCH (n:L) WITH n.score AS score RETURN SUM(score) AS sum, AVG(score) AS avg",
            "MATCH (n:L) RETURN n.score AS score UNION ALL MATCH (m:L) RETURN m.score AS score",
            "MATCH (n:L) RETURN n.score AS score EXCEPT ALL MATCH (m:L) WHERE m.score < 0 RETURN m.score AS score",
            "MATCH (a)-[r:R]->(b) RETURN r AS edge, a AS src, b AS dst, r.score AS score",
            "MATCH p=(a)-[:R]->(b) RETURN p AS path",
            "MATCH (a)-[r:R]->(b) RETURN COUNT(*) AS n, COUNT(DISTINCT r) AS edges, AVG(r.score) AS average",
            "MATCH (n:L) RETURN COLLECT(n.score) AS scores",
        ] {
            check(&before, &after, &cx, text, &args);
        }
        // Exact average 11/3 -> 13/3, without scalar or float conversion.
        let avg = after
            .query_diff(
                &cx,
                "MATCH (n:L) RETURN AVG(n.score) AS avg",
                &args,
                symbols(),
                before.frontier(),
                after.frontier(),
                wide(),
            )
            .unwrap();
        let expected = Bag::from([
            (
                vec![GraphAggregateValue::Average(
                    fgdb_gql::GraphExactAverage::new(11, 3).unwrap(),
                )],
                -1,
            ),
            (
                vec![GraphAggregateValue::Average(
                    fgdb_gql::GraphExactAverage::new(13, 3).unwrap(),
                )],
                1,
            ),
        ]);
        assert_eq!(observed(&avg), expected);
        // Vertex deletion cascades through the existing edge snapshot reader.
        let mut remove = WriteBatch::new(R);
        remove.delete_vertex(VId(1));
        db.write(&commit, remove).await.unwrap();
        let final_view = db.read_session().unwrap();
        check(
            &after,
            &final_view,
            &cx,
            "MATCH p=(a)-[:R]->(b) RETURN p AS path",
            &args,
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn endpoint_work_is_cumulative_and_changed_tuple_quota_is_not_an_input_row_limit() {
    let ((), report) = run_async_under_lab(0xd1ff_1003, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let mut batch = WriteBatch::new(R);
        for id in 0..64 {
            batch.create_vertex(VId(id), vec![L], vec![(SCORE, CanonicalScalar::Int(7))]);
        }
        let before = db.write(&commit, batch).await.unwrap();
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(
            VId(u128::MAX),
            vec![L],
            vec![(SCORE, CanonicalScalar::Int(7))],
        );
        let after = db.write(&commit, batch).await.unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(SCORES, &args, symbols()).unwrap();
        let full = prepared
            .diff(&db, &cx, &args, before, after, wide())
            .unwrap();
        assert_eq!(observed(&full), Bag::from([(vec![value(7)], 1)]));
        let rows = full.row_stats();
        let work = full.evaluator_stats();
        assert!(rows.snapshot_records >= 129);
        let exact = GqlQueryPolicy::new(
            rows.snapshot_records,
            1,
            work.work_units,
            work.scratch_entries,
        );
        assert_eq!(
            prepared
                .diff(&db, &cx, &args, before, after, exact)
                .unwrap(),
            full
        );
        for policy in [
            GqlQueryPolicy::new(rows.snapshot_records - 1, 1, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, work.work_units - 1, u64::MAX),
            GqlQueryPolicy::new(u64::MAX, 1, u64::MAX, work.scratch_entries - 1),
        ] {
            assert!(matches!(
                prepared.diff(&db, &cx, &args, before, after, policy),
                Err(GqlQueryError::Rows(_) | GqlQueryError::Evaluator(_))
            ));
        }
        let no_changes = prepared
            .diff(
                &db,
                &cx,
                &args,
                before,
                before,
                GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
            )
            .unwrap();
        assert!(no_changes.changes().is_empty());
        assert!(no_changes.row_stats().snapshot_records > 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn pinned_revision_differences_survive_writer_changes_compaction_and_reopen() {
    let ((), report) = run_async_under_lab(0xd1ff_1004, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let vfs = MemVfs::new().unwrap();
        let path = vfs.database_dir();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), &path, keys())
            .await
            .unwrap();
        let before = db.write(&commit, seed()).await.unwrap();
        let after = db.write(&commit, edit()).await.unwrap();
        let view = db.read_session().unwrap();
        let args = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(SCORES, &args, symbols()).unwrap();
        let expected = prepared
            .diff_in_view(&view, &cx, &args, before, after, wide())
            .unwrap();
        let mut update = WriteBatch::new(R);
        update.set_vertex_property(VId(1), SCORE, Some(CanonicalScalar::Int(100)));
        let latest = db.write(&commit, update).await.unwrap();
        db.compact(&commit).await.unwrap();
        drop(db);
        let db = Database::open_with_vfs(&commit, vfs, &path, keys())
            .await
            .unwrap();
        assert_eq!(
            observed(
                &prepared
                    .diff(&db, &cx, &args, before, after, wide())
                    .unwrap()
            ),
            observed(&expected)
        );
        drop(db);
        assert_eq!(
            prepared
                .diff_in_view(&view, &cx, &args, before, after, wide())
                .unwrap(),
            expected
        );
        // Exact metadata, never clamped to the pinned view or live writer.
        for (from, to, side) in [
            (latest, before, DiffEndpoint::Before),
            (before, latest, DiffEndpoint::After),
            (latest, latest, DiffEndpoint::Before),
        ] {
            let mut called = false;
            let error = view
                .query_diff(
                    &cx,
                    "not valid syntax",
                    &args,
                    |_: GraphSymbolKind, _: &str| {
                        called = true;
                        None
                    },
                    from,
                    to,
                    GqlQueryPolicy::new(0, 0, 0, 0),
                )
                .unwrap_err();
            assert!(!called);
            assert!(matches!(
                error,
                GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint,
                    source: QueryError::Read(ReadError::BeyondFrontier { .. }),
                }) if endpoint == side
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn temporal_overrides_bad_binding_and_late_endpoint_failures_never_become_empty_diffs() {
    let ((), report) = run_async_under_lab(0xd1ff_1005, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let before = db.write(&commit, seed()).await.unwrap();
        let args = GqlParameters::new();
        for text in [
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n AS id",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN COUNT(*) AS n",
        ] {
            assert!(matches!(
                db.query_diff(&cx, text, &args, symbols(), before, before, wide()),
                Err(GqlQueryError::Source(GraphDiffError::TemporalSelector))
            ));
        }
        let params = GqlParameters::new().with_int64("floor", 0).unwrap();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) WHERE n.score >= $floor RETURN n.score AS score",
            &params,
            symbols(),
        )
        .unwrap();
        assert!(matches!(
            prepared.diff(&db, &cx, &args, before, before, wide()),
            Err(GqlQueryError::Source(GraphDiffError::Definition(
                QueryError::PatternText(_)
            )))
        ));
        assert!(
            db.query_diff(
                &cx,
                "CREATE (n:L)",
                &args,
                symbols(),
                before,
                before,
                wide()
            )
            .is_err()
        );
        assert_eq!(db.frontier().unwrap(), before);
        let mut invalid = WriteBatch::new(R);
        invalid.set_vertex_property(
            VId(u128::MAX),
            SCORE,
            Some(CanonicalScalar::ucs_basic_text("private incompatible number").unwrap()),
        );
        let after = db.write(&commit, invalid).await.unwrap();
        for (from, to, failed) in [
            (before, after, DiffEndpoint::After),
            (after, before, DiffEndpoint::Before),
            (after, after, DiffEndpoint::Before),
        ] {
            let error = db
                .query_diff(
                    &cx,
                    "MATCH (n:L) RETURN SUM(n.score) AS sum",
                    &args,
                    symbols(),
                    from,
                    to,
                    GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX),
                )
                .unwrap_err();
            assert!(!format!("{error}").contains("private incompatible"));
            assert!(matches!(
                error,
                GqlQueryError::Source(GraphDiffError::Endpoint {
                    endpoint,
                    source: QueryError::Aggregate(GqlQueryError::Source(
                        GraphAggregateError::NonIntegerSum { .. }
                    )),
                }) if endpoint == failed
            ));
        }
        // The accepted historical cut is still usable after the bad new data.
        assert!(
            db.query_diff(
                &cx,
                "MATCH (n:L) RETURN SUM(n.score) AS sum",
                &args,
                symbols(),
                before,
                before,
                wide()
            )
            .unwrap()
            .changes()
            .is_empty()
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

mod spill {
    use super::*;
    use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
    use fgdb::{NativeSpoolDifference, NativeSpoolError};
    use fgdb_gql::algebra::GraphValueRow;
    use fgdb_gql::scan_stream::ScanState;
    use fgdb_gql::{
        GlaLimitDimension, GqlBudgetDimension, GraphIntegerErrorKind, GraphSetExecutionError,
    };
    use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
    use fgdb_types::{CommitCx, QueryCx};
    use std::io::{self, Cursor, Seek, SeekFrom};
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    type Result<T> =
        core::result::Result<T, GqlQueryError<NativeSpoolError, Box<asupersync::error::Error>>>;

    #[derive(Default)]
    struct State {
        bytes: Cursor<Vec<u8>>,
        reads: usize,
        writes: usize,
    }
    #[derive(Clone, Default)]
    struct File(Arc<Mutex<State>>);
    impl File {
        fn activity(&self) -> (usize, usize) {
            let state = self.0.lock().unwrap();
            (state.reads, state.writes)
        }
    }
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
        fn poll_seek(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            at: SeekFrom,
        ) -> Poll<io::Result<u64>> {
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
                max_file_bytes: 64_000_000,
                max_runs: 10_000,
                max_run_bytes: 16_000_000,
            },
        )
        .await
        .unwrap();
        (file, backend)
    }
    async fn history(cx: &CommitCx) -> (Database<MemVfs>, CommitSeq, CommitSeq) {
        let mut db = Database::open_memory(cx, keys()).await.unwrap();
        let before = db.write(cx, seed()).await.unwrap();
        let after = db.write(cx, edit()).await.unwrap();
        (db, before, after)
    }
    async fn collect(cursor: &mut NativeSpoolDifference<'_, File>) -> Result<Bag> {
        let mut changes = Bag::new();
        while let Some((weight, row)) = cursor.next_change().await? {
            let row = GraphValueRow::decode_canonical(row.as_ref()).unwrap();
            let row = row
                .values()
                .iter()
                .cloned()
                .map(GraphAggregateValue::Value)
                .collect();
            assert_ne!(weight, 0);
            assert!(changes.insert(row, weight).is_none());
        }
        assert_eq!(cursor.state(), ScanState::Exhausted);
        Ok(changes)
    }
    #[allow(clippy::too_many_arguments)]
    async fn run(
        prepared: &PreparedNativeRead,
        view: &EmbeddedReadView,
        cx: &QueryCx,
        params: &GqlParameters,
        before: CommitSeq,
        after: CommitSeq,
        allowance: GqlQueryPolicy,
        max_input_rows: u64,
        work: u64,
        run_rows: usize,
    ) -> Result<(Bag, u64)> {
        let pool = MemoryPool::new(262_144, 0).unwrap();
        let (mut source, _) = file(cx, &pool).await;
        let (mut scratch, _) = file(cx, &pool).await;
        let result = async {
            let mut cursor = prepared
                .spool_difference_in_view(
                    view,
                    cx,
                    params,
                    before,
                    after,
                    allowance,
                    &mut source,
                    &mut scratch,
                    run_rows,
                    128,
                    127,
                    8192,
                    max_input_rows,
                    work,
                )
                .await?;
            assert_eq!((cursor.before(), cursor.after()), (before, after));
            let changes = collect(&mut cursor).await?;
            Ok((changes, cursor.work_units()))
        }
        .await;
        assert_eq!(pool.used(), 0);
        result
    }

    #[test]
    fn staged_spill_differences_compare_complete_historical_endpoint_bags() {
        let ((), report) = run_async_under_lab(0xd1ff_1010, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let (db, before, after) = history(&contexts.commit()).await;
            let view = db.read_session().unwrap();
            for text in [
                "MATCH (n:L) WITH 7 AS marker RETURN marker",
                "MATCH (n:L) WITH n.score AS score ORDER BY score DESC NULLS LAST SKIP 1 LIMIT 2 RETURN score * 2 AS scaled ORDER BY scaled",
                "MATCH (n:L) WITH n AS item RETURN [item][0].score AS score ORDER BY score DESC NULLS LAST LIMIT 2",
                "MATCH (n:L) WITH n.score AS score UNWIND [score, score] AS value RETURN value ORDER BY value NULLS FIRST",
                "MATCH (n:L) WITH n.score AS score UNWIND [score, 7, score] AS value WITH DISTINCT value AS selected ORDER BY selected DESC NULLS LAST SKIP 1 LIMIT 2 RETURN selected * 2 AS doubled ORDER BY doubled",
                "MATCH ()-[edge:R]->() WITH edge AS item RETURN [item][0].score AS score ORDER BY score NULLS FIRST",
                "MATCH (seed:L) WITH seed LIMIT 1 WITH $items AS items UNWIND items AS item RETURN [item][0].score AS score ORDER BY score NULLS FIRST",
            ] {
                let params = if text.contains("$items") {
                    GqlParameters::new()
                        .with_list(
                            "items",
                            vec![
                                GraphValue::Vertex(VId(u128::MAX)),
                                GraphValue::Edge(EId(0)),
                                GraphValue::Edge(EId(u128::MAX)),
                                GraphValue::Vertex(VId(u128::MAX - 1)),
                                GraphValue::Edge(EId(u128::MAX - 1)),
                                GraphValue::Vertex(VId(1)),
                                GraphValue::Vertex(VId(1)),
                            ],
                        )
                        .unwrap()
                } else {
                    GqlParameters::new()
                };
                let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
                for (from, to) in [(before, after), (after, before), (after, after)] {
                    let expected = prepared
                        .diff_in_view(&view, &cx, &params, from, to, wide())
                        .unwrap();
                    for run_rows in [1, 3] {
                        let (actual, _) = run(
                            &prepared,
                            &view,
                            &cx,
                            &params,
                            from,
                            to,
                            wide(),
                            100,
                            u64::MAX,
                            run_rows,
                        )
                        .await
                        .unwrap();
                        assert_eq!(
                            actual,
                            observed(&expected),
                            "{text}: {from:?} -> {to:?}, run_rows={run_rows}"
                        );
                    }
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn staged_spill_differences_share_all_endpoint_allowances_and_preserve_short_circuiting() {
        let ((), report) = run_async_under_lab(0xd1ff_1011, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let (db, before, at) = history(&contexts.commit()).await;
            let view = db.read_session().unwrap();
            let params = GqlParameters::new();
            let text = "MATCH (n:L) WITH n AS item RETURN [item][0].score AS value ORDER BY value";
            let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
            let pool = MemoryPool::new(262_144, 0).unwrap();
            let (mut source, _) = file(&cx, &pool).await;
            let (mut scratch, _) = file(&cx, &pool).await;
            // Independently measure one ordinary ordered endpoint, then require
            // the difference to pay exactly twice its complete native costs.
            let (one, _) = prepared
                .spool_ordered_in_view(
                    &view,
                    &cx,
                    &params,
                    wide(),
                    &mut scratch,
                    &mut source,
                    2,
                    128,
                    127,
                    8192,
                    100,
                    u64::MAX,
                )
                .await
                .unwrap();
            assert_eq!(pool.used(), 0);
            let records = one.row_stats().snapshot_records;
            let work = one.evaluator_stats().work_units;
            let scratch_entries = one.evaluator_stats().scratch_entries;
            assert!(records >= 8 && work > 0 && scratch_entries > 0);
            let exact = GqlQueryPolicy::new(records * 2, 0, work * 2, scratch_entries * 2);
            let (changes, physical) = run(
                &prepared,
                &view,
                &cx,
                &params,
                at,
                at,
                exact,
                100,
                u64::MAX,
                2,
            )
            .await
            .unwrap();
            assert!(changes.is_empty());
            for dimension in 0..3 {
                let allowance = GqlQueryPolicy::new(
                    records * 2 - u64::from(dimension == 0),
                    0,
                    work * 2 - u64::from(dimension == 1),
                    scratch_entries * 2 - u64::from(dimension == 2),
                );
                let error = run(
                    &prepared,
                    &view,
                    &cx,
                    &params,
                    at,
                    at,
                    allowance,
                    100,
                    u64::MAX,
                    2,
                )
                .await
                .unwrap_err();
                let GqlQueryError::Source(error) = error else {
                    panic!("{error:?}")
                };
                match (dimension, error.execution_error()) {
                    (0, Some(GqlQueryError::Rows(error))) => {
                        assert_eq!(error.dimension, GqlBudgetDimension::SnapshotRecords);
                        assert_eq!((error.limit, error.observed), (records - 1, records));
                    }
                    (1, Some(GqlQueryError::Evaluator(error))) => {
                        assert_eq!(error.dimension, GlaLimitDimension::WorkUnits);
                        assert_eq!(error.limit, work - 1);
                        assert_eq!(error.observed, u128::from(work));
                    }
                    (2, Some(GqlQueryError::Evaluator(error))) => {
                        assert_eq!(error.dimension, GlaLimitDimension::ScratchEntries);
                        assert_eq!(error.limit, scratch_entries - 1);
                        assert_eq!(error.observed, u128::from(scratch_entries));
                    }
                    _ => panic!("wrong endpoint allowance refusal: {error:?}"),
                }
            }
            assert_eq!(
                run(
                    &prepared, &view, &cx, &params, at, at, exact, 100, physical, 2
                )
                .await
                .unwrap()
                .1,
                physical
            );
            assert!(matches!(run(
                &prepared, &view, &cx, &params, at, at, exact, 100, physical - 1, 2,
            ).await, Err(GqlQueryError::Source(NativeSpoolError::SortWorkLimit { attempted, limit }))
                if attempted == physical && limit == physical - 1));
            assert!(matches!(run(
                &prepared, &view, &cx, &params, before, at,
                GqlQueryPolicy::new(u64::MAX, 0, u64::MAX, u64::MAX), 100, u64::MAX, 2,
            ).await, Err(GqlQueryError::Rows(error))
                if error.dimension == GqlBudgetDimension::ResultRows && error.limit == 0));

            for expression in [
                "CASE WHEN true THEN 7 ELSE [item][0].score END",
                "coalesce(7, [item][0].score)",
            ] {
                let skipped = PreparedNativeRead::prepare(
                    &format!("MATCH (n:L) WITH n AS item RETURN {expression} AS value"),
                    &params,
                    symbols(),
                )
                .unwrap();
                let (one_source, _) = skipped
                    .spool_ordered_in_view(
                        &view,
                        &cx,
                        &params,
                        wide(),
                        &mut scratch,
                        &mut source,
                        2,
                        128,
                        127,
                        8192,
                        100,
                        u64::MAX,
                    )
                    .await
                    .unwrap();
                assert_eq!(records, one_source.row_stats().snapshot_records + 4);
                let no_lookups = GqlQueryPolicy::new(
                    one_source.row_stats().snapshot_records * 2,
                    0,
                    u64::MAX,
                    u64::MAX,
                );
                assert!(
                    run(
                        &skipped,
                        &view,
                        &cx,
                        &params,
                        at,
                        at,
                        no_lookups,
                        100,
                        u64::MAX,
                        2
                    )
                    .await
                    .unwrap()
                    .0
                    .is_empty()
                );
            }
            for window in ["", " LIMIT 0"] {
                let expanded = PreparedNativeRead::prepare(
                    &format!("MATCH (n:L) WITH n.score AS score UNWIND [score, score] AS value RETURN value{window}"),
                    &params, symbols(),
                ).unwrap();
                let error = run(
                    &expanded,
                    &view,
                    &cx,
                    &params,
                    at,
                    at,
                    wide(),
                    7,
                    u64::MAX,
                    2,
                )
                .await
                .unwrap_err();
                assert!(
                    matches!(error, GqlQueryError::Source(NativeSpoolError::Execute(error))
                    if matches!(*error, GqlQueryError::Rows(error)
                        if error.dimension == GqlBudgetDimension::ResultRows && error.limit == 7 && error.observed > 7))
                );
            }
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn staged_spill_difference_opening_pins_both_cuts_and_preserves_late_errors() {
        let ((), report) = run_async_under_lab(0xd1ff_1012, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let cx = contexts.query();
            let commit = contexts.commit();
            let (mut db, before, after) = history(&commit).await;
            let view = db.read_session().unwrap();
            let params = GqlParameters::new().with_int64("shift", 1).unwrap();
            let prepared = PreparedNativeRead::prepare(
                "MATCH (n:L) WITH n AS item RETURN coalesce([item][0].score, 0) + $shift AS value ORDER BY value DESC LIMIT 2",
                &params, symbols(),
            ).unwrap();
            let expected = observed(
                &prepared
                    .diff_in_view(&view, &cx, &params, before, after, wide())
                    .unwrap(),
            );
            let pool = MemoryPool::new(262_144, 0).unwrap();
            let (mut source, a) = file(&cx, &pool).await;
            let (mut scratch, b) = file(&cx, &pool).await;
            let activity = (a.activity(), b.activity());
            drop(prepared.spool_difference_in_view(
                &view,
                &cx,
                &params,
                before,
                after,
                wide(),
                &mut source,
                &mut scratch,
                2,
                128,
                127,
                8192,
                100,
                u64::MAX,
            ));
            assert_eq!((a.activity(), b.activity()), activity);
            assert_eq!(pool.used(), 0);
            let future = prepared.spool_difference(
                &db,
                &cx,
                &params,
                before,
                after,
                wide(),
                &mut source,
                &mut scratch,
                2,
                128,
                127,
                8192,
                100,
                u64::MAX,
            );
            drop(prepared);
            drop(params);
            let mut change = WriteBatch::new(R);
            change.delete_vertex(VId(1));
            db.write(&commit, change).await.unwrap();
            db.compact(&commit).await.unwrap();
            drop(db);
            let mut cursor = future.await.unwrap();
            assert_eq!((cursor.before(), cursor.after()), (before, after));
            assert_eq!(collect(&mut cursor).await.unwrap(), expected);
            drop(cursor);
            assert_eq!(pool.used(), 0);

            let params = GqlParameters::new();
            let late = PreparedNativeRead::prepare(
                "MATCH (n:L) WITH n AS item RETURN 10 / ([item][0].score - 9) AS value ORDER BY value LIMIT 0",
                &params, symbols(),
            ).unwrap();
            assert!(
                run(
                    &late,
                    &view,
                    &cx,
                    &params,
                    before,
                    before,
                    wide(),
                    100,
                    u64::MAX,
                    2
                )
                .await
                .unwrap()
                .0
                .is_empty()
            );
            let writes = a.activity().1;
            let error = late
                .spool_difference_in_view(
                    &view,
                    &cx,
                    &params,
                    before,
                    after,
                    wide(),
                    &mut source,
                    &mut scratch,
                    2,
                    128,
                    127,
                    8192,
                    100,
                    u64::MAX,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Source(NativeSpoolError::SetExecution(
                GraphSetExecutionError::Projection { row: 1, column: 0, error }))
                if error.kind == GraphIntegerErrorKind::DivisionByZero)
            );
            assert!(
                a.activity().1 > writes,
                "the first endpoint completed before the late failure"
            );
            assert_eq!(pool.used(), 0);

            let activity = (a.activity(), b.activity());
            for text in [
                "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 WITH n AS item RETURN [item][0].score AS value LIMIT 0",
                "MATCH (n:L) RETURN n.score AS score UNION ALL MATCH (m:L) RETURN m.score AS score",
                "MATCH (n:L) WITH n.score AS score RETURN SUM(score) AS total",
            ] {
                let prepared = PreparedNativeRead::prepare(text, &params, symbols()).unwrap();
                let error = prepared
                    .spool_difference_in_view(
                        &view,
                        &cx,
                        &params,
                        before,
                        after,
                        wide(),
                        &mut source,
                        &mut scratch,
                        2,
                        128,
                        127,
                        8192,
                        100,
                        u64::MAX,
                    )
                    .await
                    .unwrap_err();
                assert!(
                    matches!(error, GqlQueryError::Source(NativeSpoolError::Prepare(error))
                    if matches!(*error, QueryError::StreamingUnsupported { .. })),
                    "{text}"
                );
            }
            let error = late
                .spool_difference_in_view(
                    &view,
                    &cx,
                    &params,
                    before,
                    CommitSeq(after.0 + 1),
                    wide(),
                    &mut source,
                    &mut scratch,
                    2,
                    128,
                    127,
                    8192,
                    100,
                    u64::MAX,
                )
                .await
                .unwrap_err();
            assert!(
                matches!(error, GqlQueryError::Source(NativeSpoolError::Prepare(error))
                if matches!(*error, QueryError::Read(ReadError::BeyondFrontier { .. })))
            );
            assert_eq!((a.activity(), b.activity()), activity);
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
