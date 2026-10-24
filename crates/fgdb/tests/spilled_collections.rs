//! Public buffered queries over Chronicle/Strata, not a replacement graph.
//! The resident native query is the semantic oracle. Cursor-backed files below
//! exercise the real authenticated scratch writer, partitioner and reader.
#![recursion_limit = "256"]

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs,
    PreparedNativeRead, QueryResult, QueryValue, WriteBatch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateTextSlot, GraphAggregateValue, GraphSymbol,
    GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId};
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
        [0xc4; 32],
        DatabaseSecurityNamespaceId([0xc5; 32]),
        [0xc6; 32],
    )
}
fn policy() -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, 10_000, 100_000_000, 100_000_000)
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "g") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "v") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
async fn fixture(contexts: &PurposeContexts) -> (Database<MemVfs>, BufferedReadView<MemVfs>) {
    let vfs = MemVfs::new().unwrap();
    let mut db =
        Database::create_with_vfs(&contexts.commit(), vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for id in 0..12 {
        batch.create_vertex(
            VId(id),
            vec![LabelId(1)],
            vec![
                (PropertyKeyId(1), CanonicalScalar::Int((id % 3) as i64)),
                (PropertyKeyId(2), CanonicalScalar::Int(9 - (id % 5) as i64)),
            ],
        );
    }
    db.write(&contexts.commit(), batch).await.unwrap();
    let view = Database::open_buffered_read_view_with_vfs(
        &contexts.commit(),
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        MemoryPool::new(64 * 1024 * 1024, 0).unwrap(),
        BufferedReadLimits {
            max_root_bytes: 64 * 1024,
            max_source_bytes: 4 * 1024 * 1024,
            max_blocks: 512,
            max_vertex_patches: 512,
            max_work: 10_000_000,
            buffer: BufferLimits {
                max_frames: 1,
                max_ghost_entries: 2,
                max_extent_bytes: 16 * 1024,
            },
        },
    )
    .await
    .unwrap();
    (db, view)
}
async fn scratch(cx: &QueryCx, pool: &MemoryPool) -> SpillFile<File> {
    SpillFile::new(
        cx,
        File::default(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 128_000_000,
            max_runs: 8192,
            max_run_bytes: 16_000_000,
        },
    )
    .await
    .unwrap()
}
async fn external(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    prepared: &PreparedNativeRead,
    parameters: &GqlParameters,
    capacity: usize,
) -> (Vec<String>, Vec<Vec<QueryValue>>) {
    let pool = MemoryPool::new(1_000_000, 0).unwrap();
    let mut a = scratch(cx, &pool).await;
    let mut b = scratch(cx, &pool).await;
    let mut c = scratch(cx, &pool).await;
    let plan = prepared.prepare_buffered_aggregate(parameters).unwrap();
    let (spool, _) = plan
        .spool_in_view(
            view,
            cx,
            policy(),
            &mut a,
            &mut b,
            &mut c,
            capacity,
            127,
            1,
            512,
            257,
            65_536,
            10_000,
            1_000_000_000,
            None,
        )
        .await
        .unwrap();
    assert_eq!(pool.used(), 0);
    let columns = spool.columns().to_vec();
    let slots = spool.output_slots().to_vec();
    let mut rows = Vec::new();
    let mut reader = spool.reader(&mut c, None);
    while let Some(row) = reader.next_row(cx).await.unwrap() {
        rows.push(
            slots
                .iter()
                .map(|slot| match *slot {
                    GraphAggregateTextSlot::GroupKey(at) => {
                        GraphAggregateValue::Value(row.keys()[at].clone())
                    }
                    GraphAggregateTextSlot::Aggregate(at) => row.values()[at].clone(),
                })
                .collect(),
        );
    }
    assert_eq!(rows.len() as u64, spool.row_count());
    drop(reader);
    assert_eq!(pool.used(), 0);
    (columns, rows)
}

#[test]
fn public_external_collections_match_native_source_free_and_ordered_graph_queries() {
    let ((), report) = run_async_under_lab(0xc011_0101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, mut cold) = fixture(&contexts).await;
        let view = db.read_session().unwrap();
        let parameters = GqlParameters::new();
        for text in [
            "UNWIND [9,2,9,null,4] AS v RETURN COLLECT(v) AS items,COLLECT(DISTINCT v) AS unique_items",
            "UNWIND [] AS v RETURN COLLECT(v) AS items",
            "UNWIND [null,null] AS v RETURN COLLECT(DISTINCT v) AS items",
            "UNWIND [[9,2],[9,2],[],[1]] AS v RETURN COLLECT(v) AS items,COLLECT(DISTINCT v) AS unique_items",
            "MATCH (n:L) WITH n.g AS g,n.v AS v ORDER BY v DESC RETURN g,COLLECT(v) AS items,COLLECT(DISTINCT v) AS unique_items ORDER BY g",
            "MATCH (n:L) WITH n.g AS g,n.v AS v ORDER BY v DESC RETURN g,COLLECT(v) AS items,SUM(DISTINCT v) AS total ORDER BY g SKIP 1 LIMIT 1",
            "MATCH (n:L) WITH n ORDER BY n.g,n.v RETURN COLLECT(n) AS nodes",
        ] {
            let QueryResult::Rows { columns, rows } = view
                .query(&cx, text, &parameters, resolve, policy())
                .unwrap()
            else {
                panic!("read rows");
            };
            let prepared = PreparedNativeRead::prepare(text, &parameters, resolve).unwrap();
            for capacity in [1, 4] {
                let actual = external(&mut cold, &cx, &prepared, &parameters, capacity).await;
                assert_eq!(actual, (columns.clone(), rows.clone()), "{text}");
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn an_intermediate_collection_can_feed_unwind_and_another_external_group() {
    let ((), report) = run_async_under_lab(0xc011_0102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let (db, mut cold) = fixture(&contexts).await;
        let view = db.read_session().unwrap();
        let parameters = GqlParameters::new();
        let text = "MATCH (n:L) WITH n.g AS g,n.v AS v ORDER BY v DESC WITH g,COLLECT(v) AS items UNWIND items AS v RETURN g,COLLECT(DISTINCT v) AS unique_items,COUNT(*) AS occurrences ORDER BY g";
        let QueryResult::Rows { columns, rows } = view
            .query(&cx, text, &parameters, resolve, policy())
            .unwrap()
        else {
            panic!("read rows");
        };
        let prepared = PreparedNativeRead::prepare(text, &parameters, resolve).unwrap();
        assert!(
            prepared
                .prepare_buffered_aggregate(&parameters)
                .unwrap()
                .aggregate_count()
                >= 2
        );
        assert_eq!(
            external(&mut cold, &cx, &prepared, &parameters, 1).await,
            (columns, rows)
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
