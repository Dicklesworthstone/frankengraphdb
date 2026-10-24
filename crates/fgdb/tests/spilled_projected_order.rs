//! Native query -> external grouping -> projected rank -> exact result rows.
//! Small run/group capacities force the real authenticated spill pipeline.
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    Database, DatabaseKeys, MemVfs, NativeAggregateSpoolError, PreparedNativeRead, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryError, GqlQueryPolicy, GraphAggregateError, GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Default)]
struct File(Cursor<Vec<u8>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let file = &mut self.get_mut().0;
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
        Poll::Ready(std::io::Write::write(&mut self.get_mut().0, bytes))
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
        Poll::Ready(self.get_mut().0.seek(at))
    }
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x91; 32],
        DatabaseSecurityNamespaceId([0x92; 32]),
        [0x93; 32],
    )
}
fn policy(results: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, results, 100_000_000, 10_000_000)
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
fn prepare(text: &str, parameters: &GqlParameters) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, parameters, resolve).unwrap()
}
async fn scratch(cx: &QueryCx, pool: &MemoryPool) -> SpillFile<File> {
    SpillFile::new(
        cx,
        File::default(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 128_000_000,
            max_runs: 4096,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap()
}
async fn seed(cx: &CommitCx) -> Database<MemVfs> {
    let mut db = Database::open_memory(cx, keys()).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 0..64_u128 {
        let mut props = vec![(PropertyKeyId(1), CanonicalScalar::Int((id % 8) as i64))];
        if id % 5 != 0 {
            props.push((PropertyKeyId(2), CanonicalScalar::Int(i64::MAX - id as i64)));
        }
        batch.create_vertex(VId(id), vec![LabelId(1)], props);
        if id != 0 {
            batch.add_edge(
                EId(id),
                VId(0),
                VId(id),
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int((id % 8) as i64)),
                    (PropertyKeyId(2), CanonicalScalar::Int(id as i64)),
                ],
            );
        }
    }
    // A late single-row/all-NULL group exercises both NULL ranking and errors.
    batch.create_vertex(
        VId(1000),
        vec![LabelId(1)],
        vec![(PropertyKeyId(1), CanonicalScalar::Int(8))],
    );
    db.write(cx, batch).await.unwrap();
    db
}

#[test]
fn projected_alias_spill_matches_native_streams_across_sources_distinct_and_windows() {
    let ((), report) = run_async_under_lab(0x5ba1_1020, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let parameters = GqlParameters::new();
        for statement in [
            "MATCH (n:L) RETURN n.p AS bucket,-SUM(n.q) AS score,AVG(n.q) AS average ORDER BY score DESC NULLS LAST,bucket ASC",
            "MATCH (n:L) RETURN n.p AS bucket,-SUM(n.q) AS score ORDER BY score ASC NULLS FIRST,bucket DESC",
            "MATCH (n:L) RETURN n.p AS bucket,SUM(n.q) AS total,-COUNT(*) AS score ORDER BY score,total DESC",
            "MATCH (n:L) RETURN DISTINCT -COUNT(*) AS score GROUP BY n.p ORDER BY score DESC",
            "MATCH (a)-[e:R]->(b) RETURN e.p AS bucket,-SUM(e.q) AS score ORDER BY score DESC,bucket ASC",
            "MATCH (n:L) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n.p AS bucket,-COUNT(*) AS score ORDER BY score DESC,bucket DESC",
        ] {
            for window in ["", " SKIP 1 LIMIT 3", " LIMIT 0", " SKIP 1000 LIMIT 1"] {
                let text = format!("{statement}{window}");
                let prepared = prepare(&text, &parameters);
                let cursor = prepared
                    .stream_aggregate_in_view(&view, &cx, &parameters, policy(100))
                    .unwrap();
                let columns = cursor.columns().to_vec();
                let expected = cursor.collect::<Result<Vec<_>, _>>().unwrap();
                for (capacity, run_rows) in [(1, 1), (3, 3)] {
                    let pool = MemoryPool::new(1_000_000, 0).unwrap();
                    let mut a = scratch(&cx, &pool).await;
                    let mut b = scratch(&cx, &pool).await;
                    let mut c = scratch(&cx, &pool).await;
                    let (spool, _) = prepared
                        .spool_aggregate_in_view(
                            &view,
                            &cx,
                            &parameters,
                            policy(expected.len() as u64),
                            &mut a,
                            &mut b,
                            &mut c,
                            capacity,
                            256,
                            run_rows,
                            128,
                            257,
                            16_384,
                            1000,
                            100_000_000,
                            None,
                        )
                        .await
                        .unwrap_or_else(|error| panic!("{text}: {error}"));
                    assert_eq!(spool.columns(), columns, "{text}");
                    assert_eq!(spool.row_count(), expected.len() as u64, "{text}");
                    assert_eq!(pool.used(), 0, "{text}");
                    let mut actual = Vec::new();
                    {
                        let mut reader = spool.reader(&mut c, None);
                        while let Some(row) = reader.next_row(&cx).await.unwrap() {
                            actual.push((*row).clone());
                        }
                        assert!(reader.next_row(&cx).await.unwrap().is_none());
                    }
                    assert_eq!(actual, expected, "{text}");
                    assert_eq!(pool.used(), 0, "{text}");
                }
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_spill_rebinding_changes_rank_without_changing_the_prepared_template() {
    let ((), report) = run_async_under_lab(0x5ba1_1021, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let initial = GqlParameters::new().with_int64("scale", 1).unwrap();
        let prepared = prepare(
            "MATCH (n:L) RETURN n.p AS bucket,SUM(n.q)*$scale AS score ORDER BY score DESC LIMIT 2",
            &initial,
        );
        let mut results = Vec::new();
        for scale in [1, -1, 1] {
            let parameters = GqlParameters::new().with_int64("scale", scale).unwrap();
            let expected = prepared
                .stream_aggregate_in_view(&view, &cx, &parameters, policy(2))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let mut a = scratch(&cx, &pool).await;
            let mut b = scratch(&cx, &pool).await;
            let mut c = scratch(&cx, &pool).await;
            let (spool, _) = prepared
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &parameters,
                    policy(2),
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    256,
                    1,
                    128,
                    257,
                    16_384,
                    1000,
                    100_000_000,
                    None,
                )
                .await
                .unwrap();
            let mut actual = Vec::new();
            {
                let mut reader = spool.reader(&mut c, None);
                while let Some(row) = reader.next_row(&cx).await.unwrap() {
                    actual.push((*row).clone());
                }
            }
            assert_eq!(actual, expected);
            assert_eq!(pool.used(), 0);
            results.push(actual);
        }
        assert_ne!(results[0], results[1]);
        assert_eq!(results[0], results[2]);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_spill_never_publishes_a_page_that_hides_late_expression_errors() {
    let ((), report) = run_async_under_lab(0x5ba1_1022, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let parameters = GqlParameters::new();
        for window in ["", " LIMIT 0", " LIMIT 1", " SKIP 1000 LIMIT 1"] {
            let text = format!(
                "MATCH (n:L) RETURN n.p AS bucket,1/(COUNT(*)-1) AS score ORDER BY score{window}"
            );
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let mut a = scratch(&cx, &pool).await;
            let mut b = scratch(&cx, &pool).await;
            let mut c = scratch(&cx, &pool).await;
            let error = prepare(&text, &parameters)
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &parameters,
                    policy(100),
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    256,
                    1,
                    128,
                    257,
                    16_384,
                    1000,
                    100_000_000,
                    None,
                )
                .await
                .unwrap_err();
            let NativeAggregateSpoolError::Execute(error) = error else {
                panic!("{text}: expected native expression failure, got {error}");
            };
            assert!(matches!(
                *error,
                GqlQueryError::Source(GraphAggregateError::OutputExpression { .. })
            ));
            assert_eq!(pool.used(), 0, "{text}");
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn projected_ranked_result_larger_than_its_pool_remains_external() {
    let ((), report) = run_async_under_lab(0x5ba1_1023, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let mut db = Database::open_memory(&contexts.commit(), keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for id in 0..256_u128 {
            let payload = format!("{:04}:{}", 255 - id, "x".repeat(1024));
            batch.create_vertex(
                VId(id),
                vec![LabelId(1)],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(id as i64)),
                    (
                        PropertyKeyId(2),
                        CanonicalScalar::ucs_basic_text(&payload).unwrap(),
                    ),
                ],
            );
        }
        db.write(&contexts.commit(), batch).await.unwrap();
        let view = db.read_session().unwrap();
        let pool = MemoryPool::new(196_608, 0).unwrap();
        assert!(256 * 1024 > pool.limit());
        let mut a = scratch(&cx, &pool).await;
        let mut b = scratch(&cx, &pool).await;
        let mut c = scratch(&cx, &pool).await;
        let parameters = GqlParameters::new();
        let prepared = prepare(
            "MATCH (n:L) RETURN MIN(n.q) AS payload,-COUNT(*) AS score GROUP BY n.p ORDER BY score DESC,payload ASC",
            &parameters,
        );
        let (spool, _) = prepared
            .spool_aggregate_in_view(
                &view,
                &cx,
                &parameters,
                policy(256),
                &mut a,
                &mut b,
                &mut c,
                1,
                512,
                16,
                128,
                257,
                16_384,
                1000,
                100_000_000,
                None,
            )
            .await
            .unwrap();
        assert_eq!(spool.row_count(), 256);
        assert!(b.stats().published_runs > 1);
        assert_eq!(pool.used(), 0);
        {
            let mut reader = spool.reader(&mut c, None);
            for index in 0..256 {
                let row = reader.next_row(&cx).await.unwrap().unwrap();
                let Some(CanonicalScalar::Text(payload)) =
                    row.values()[0].as_value().and_then(GraphValue::as_scalar)
                else {
                    panic!("expected original text extremum");
                };
                assert!(payload.as_str().starts_with(&format!("{index:04}:")));
                assert_eq!(row.values()[1].as_integer(), Some(-1));
                assert!(row.keys().is_empty());
            }
            assert!(reader.next_row(&cx).await.unwrap().is_none());
        }
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
