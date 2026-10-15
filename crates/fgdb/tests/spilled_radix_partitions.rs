//! Native grouping through real authenticated scratch with a tight partition
//! quota. Only the asynchronous file backing is a byte cursor; grouping,
//! storage, hashing, spill, result ordering and admission are production paths.

use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, MemVfs, NativeAggregateSpoolError, PreparedNativeRead, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_gql::{
    GqlParameters, GqlQueryPolicy, GraphAggregateRow, GraphAggregateTextSlot, GraphAggregateValue,
    GraphSymbol, GraphSymbolKind,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, PurposeContexts, QueryCx, VId,
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

const GROUPS: usize = 64;
const PARTITIONS: usize = 2 * GROUPS - 1;

fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Label, "L") => Some(GraphSymbol::Label(LabelId(1))),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}

async fn seed(cx: &CommitCx) -> Database<MemVfs> {
    let keys = DatabaseKeys::new(
        [0x31; 32],
        DatabaseSecurityNamespaceId([0x32; 32]),
        [0x33; 32],
    );
    let mut db = Database::open_memory(cx, keys).await.unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    // Repeated values test multiplicity and DISTINCT support, not just sets
    // of singleton keys. Their source occurrence order survives partitioning.
    for (occurrence, value) in [2, 5, 5].into_iter().enumerate() {
        for key in (0..GROUPS).rev() {
            batch.create_vertex(
                VId((occurrence * GROUPS + key) as u128),
                vec![LabelId(1)],
                vec![
                    (PropertyKeyId(1), CanonicalScalar::Int(key as i64)),
                    (PropertyKeyId(2), CanonicalScalar::Int(value)),
                ],
            );
        }
    }
    db.write(cx, batch).await.unwrap();
    db
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

fn integer(value: &GraphValue) -> i128 {
    match value {
        GraphValue::Scalar(CanonicalScalar::Int(value)) => i128::from(*value),
        other => panic!("integer cell expected: {other:?}"),
    }
}

fn output(row: &GraphAggregateRow, slots: &[GraphAggregateTextSlot]) -> Vec<i128> {
    slots
        .iter()
        .map(|slot| match *slot {
            GraphAggregateTextSlot::GroupKey(at) => integer(&row.keys()[at]),
            GraphAggregateTextSlot::Aggregate(at) => match &row.values()[at] {
                GraphAggregateValue::Count(value) => i128::from(*value),
                GraphAggregateValue::Integer(value) => *value,
                GraphAggregateValue::Value(value) => integer(value),
                other => panic!("integer aggregate expected: {other:?}"),
            },
        })
        .collect()
}

#[test]
fn singleton_group_capacity_needs_only_the_nonempty_binary_tree() {
    let ((), report) = run_async_under_lab(0x5ba1_1030, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let parameters = GqlParameters::new();
        for (text, totals) in [
            (
                "MATCH (n:L) RETURN n.p AS bucket,COUNT(*) AS n,SUM(n.q) AS total,MIN(n.q) AS low,MAX(n.q) AS high ORDER BY bucket",
                [3, 12, 2, 5],
            ),
            (
                "MATCH (n:L) RETURN n.p AS bucket,COUNT(DISTINCT n.q) AS n,SUM(DISTINCT n.q) AS total,MIN(n.q) AS low,MAX(n.q) AS high ORDER BY bucket",
                [2, 7, 2, 5],
            ),
        ] {
            let prepared = PreparedNativeRead::prepare(text, &parameters, resolve).unwrap();
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let mut a = scratch(&cx, &pool).await;
            let mut b = scratch(&cx, &pool).await;
            let mut c = scratch(&cx, &pool).await;
            let (spool, _) = prepared
                .spool_aggregate_in_view(
                    &view,
                    &cx,
                    &parameters,
                    GqlQueryPolicy::new(10_000, GROUPS as u64, 100_000_000, 100_000_000),
                    &mut a,
                    &mut b,
                    &mut c,
                    1,
                    PARTITIONS,
                    8,
                    128,
                    257,
                    16_384,
                    1000,
                    1_000_000_000,
                    None,
                )
                .await
                .unwrap_or_else(|error| panic!("{text}: {error}"));
            assert_eq!(spool.row_count(), GROUPS as u64);
            assert_eq!(pool.used(), 0);
            let slots = spool.output_slots().to_vec();
            let mut reader = spool.reader(&mut c, None);
            for key in 0..GROUPS {
                let row = reader.next_row(&cx).await.unwrap().unwrap();
                assert_eq!(
                    output(&row, &slots),
                    vec![key as i128, totals[0], totals[1], totals[2], totals[3]],
                    "{text}"
                );
            }
            assert!(reader.next_row(&cx).await.unwrap().is_none());
            drop(reader);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn the_partition_ceiling_is_not_relaxed_by_skipping_empty_prefixes() {
    let ((), report) = run_async_under_lab(0x5ba1_1031, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let db = seed(&contexts.commit()).await;
        let view = db.read_session().unwrap();
        let parameters = GqlParameters::new();
        let prepared = PreparedNativeRead::prepare(
            "MATCH (n:L) RETURN n.p AS bucket,COUNT(*) AS n ORDER BY bucket",
            &parameters,
            resolve,
        )
        .unwrap();
        let pool = MemoryPool::new(1_000_000, 0).unwrap();
        let mut a = scratch(&cx, &pool).await;
        let mut b = scratch(&cx, &pool).await;
        let mut c = scratch(&cx, &pool).await;
        let error = prepared
            .spool_aggregate_in_view(
                &view,
                &cx,
                &parameters,
                GqlQueryPolicy::new(10_000, GROUPS as u64, 100_000_000, 100_000_000),
                &mut a,
                &mut b,
                &mut c,
                1,
                PARTITIONS - 1,
                8,
                128,
                257,
                16_384,
                1000,
                1_000_000_000,
                None,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            NativeAggregateSpoolError::PartitionLimit { required, limit }
                if required == PARTITIONS && limit == PARTITIONS - 1
        ));
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
