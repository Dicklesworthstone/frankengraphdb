//! Public buffered joins over the real Database/MemVfs and authenticated spill.
//! Pair membership comes from literal matrices, not the production matcher.
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::lab::run_async_under_lab;
use fgdb::{
    BufferLimits, BufferedReadLimits, BufferedReadView, Database, DatabaseKeys, MemVfs,
    NativeSpoolError, PreparedBufferedOrder, WriteBatch,
};
use fgdb_delta_types::{PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow, IntegerComparison};
use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlParameters, GqlQueryPolicy, GraphSetOperand,
    GraphSetPredicateOp, GraphSetProjection, GraphSetQuantifier, GraphSetValue, GraphSymbol,
    GraphSymbolKind, PreparedGraphSet, PreparedGraphSetText,
};
use fgdb_strata::tiered::memory::{MemoryPool, SpillFile, SpillLimits};
use fgdb_types::{
    CanonicalDecimal, CanonicalF64, CanonicalScalar, CommitCx, CommitSeq,
    DatabaseSecurityNamespaceId, EId, PurposeContexts, QueryCx, VId,
};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

const WORK: u64 = 100_000_000;
const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner,
    RowJoinKind::Left,
    RowJoinKind::Right,
    RowJoinKind::Full,
    RowJoinKind::Semi,
    RowJoinKind::Anti,
];
type Pair = [GraphValue; 2];

#[derive(Default)]
struct File(Cursor<Vec<u8>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        out: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let bytes = &mut self.get_mut().0;
        let at = bytes.position() as usize;
        let len = out
            .remaining()
            .min(bytes.get_ref().len().saturating_sub(at));
        if len != 0 {
            out.put_slice(&bytes.get_ref()[at..at + len]);
            bytes.set_position((at + len) as u64);
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
async fn scratch(cx: &QueryCx, pool: &MemoryPool) -> SpillFile<File> {
    SpillFile::new(
        cx,
        File::default(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 64_000_000,
            max_runs: 4096,
            max_run_bytes: 4_000_000,
        },
    )
    .await
    .unwrap()
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xe1; 32],
        DatabaseSecurityNamespaceId([0xe2; 32]),
        [0xe3; 32],
    )
}
async fn store(cx: &CommitCx) -> (Database<MemVfs>, MemVfs) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for (id, key, payload) in [
        (1, CanonicalScalar::Int(7), 10),
        (2, CanonicalScalar::Float(CanonicalF64::new(7.0)), 20),
        (3, CanonicalScalar::Int(9), 30),
    ] {
        batch.create_vertex(
            VId(id),
            vec![],
            vec![
                (PropertyKeyId(1), key),
                (PropertyKeyId(2), CanonicalScalar::Int(payload)),
            ],
        );
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    (db, vfs)
}
async fn view(cx: &CommitCx, vfs: &MemVfs, pool: &MemoryPool) -> BufferedReadView<MemVfs> {
    Database::open_buffered_read_view_with_vfs(
        cx,
        vfs.clone(),
        vfs.database_dir(),
        keys(),
        pool.clone(),
        BufferedReadLimits {
            max_root_bytes: 8192,
            max_source_bytes: 16_000_000,
            max_blocks: 128,
            max_vertex_patches: 128,
            max_work: WORK as usize,
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
fn policy(rows: u64) -> GqlQueryPolicy {
    GqlQueryPolicy::new(10_000, rows, WORK, WORK)
}
fn int(v: i64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Int(v))
}
fn float(v: f64) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(v)))
}
fn decimal(v: i128) -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Decimal(
        CanonicalDecimal::from_integer(v).unwrap(),
    ))
}
fn null() -> GraphValue {
    GraphValue::Scalar(CanonicalScalar::Null)
}
fn equal(a: usize, b: usize) -> GraphSetPredicateOp {
    GraphSetPredicateOp::Compare {
        left: GraphSetOperand::Column(a),
        comparison: IntegerComparison::Equal,
        right: GraphSetOperand::Column(b),
    }
}
fn selected_code() -> Vec<GraphSetPredicateOp> {
    vec![
        equal(0, 2),
        GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1),
            comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(3),
        },
        GraphSetPredicateOp::And,
    ]
}
fn input(rows: &[Pair]) -> PreparedGraphSet {
    PreparedGraphSet::singleton()
        .unwind(
            "tuple".into(),
            GraphSetValue::List(
                rows.iter()
                    .map(|row| {
                        GraphSetValue::Value(GraphValue::List(row.to_vec().into_boxed_slice()))
                    })
                    .collect(),
            ),
        )
        .unwrap()
        .project(
            [("key", 0), ("payload", 1)]
                .into_iter()
                .map(|(name, at)| {
                    GraphSetProjection::new(
                        name,
                        GraphSetValue::Index {
                            list: Box::new(GraphSetValue::Column(0)),
                            index: Box::new(GraphSetValue::Value(int(at))),
                        },
                    )
                })
                .collect(),
            GraphSetQuantifier::All,
        )
        .unwrap()
}
fn query(
    left: PreparedGraphSet,
    right: PreparedGraphSet,
    kind: RowJoinKind,
    code: &[GraphSetPredicateOp],
) -> PreparedGraphSet {
    let spec = RowJoinSpec::cross(left.column_types(), right.column_types())
        .unwrap()
        .with_kind(kind)
        .with_predicate(code)
        .unwrap();
    left.join(right, spec).unwrap()
}
fn inferred(query: &PreparedGraphSet) -> bool {
    let plan = AsyncSpillSetPlan::compile(query).unwrap();
    matches!(plan.source(), AsyncSpillSetSourcePlan::RowJoin(join)
        if join.keys().is_empty() && join.supports_merge_filtered())
}

// Literal matrix expansion knows nothing about predicate evaluation, probing,
// sorting runs or their witness searches. Only final canonical row order is shared.
fn expected(
    left: &[Pair],
    right: &[Pair],
    matches: &[(usize, usize)],
    kind: RowJoinKind,
) -> Vec<GraphValueRow> {
    let mut rows = Vec::new();
    for (l, row) in left.iter().enumerate() {
        let peers: Vec<_> = matches
            .iter()
            .filter(|(a, _)| *a == l)
            .map(|(_, b)| *b)
            .collect();
        if matches!(
            kind,
            RowJoinKind::Inner | RowJoinKind::Left | RowJoinKind::Right | RowJoinKind::Full
        ) {
            for r in &peers {
                rows.push(row.iter().chain(&right[*r]).cloned().collect::<Vec<_>>());
            }
        }
        match kind {
            RowJoinKind::Left | RowJoinKind::Full if peers.is_empty() => {
                rows.push(vec![row[0].clone(), row[1].clone(), null(), null()])
            }
            RowJoinKind::Semi if !peers.is_empty() => rows.push(row.to_vec()),
            RowJoinKind::Anti if peers.is_empty() => rows.push(row.to_vec()),
            _ => {}
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        for (r, row) in right.iter().enumerate() {
            if !matches.iter().any(|(_, b)| *b == r) {
                rows.push(vec![null(), null(), row[0].clone(), row[1].clone()]);
            }
        }
    }
    let mut rows: Vec<_> = rows
        .into_iter()
        .map(GraphValueRow::from_owned_values)
        .collect();
    rows.sort();
    rows
}
#[derive(Debug)]
struct Measured {
    rows: Vec<GraphValueRow>,
    records: GqlExecutionStats,
    evaluator: GlaExecutionStats,
    work: u64,
    snapshot: CommitSeq,
}
#[allow(clippy::too_many_arguments)]
async fn run(
    query: &PreparedGraphSet,
    cut: Option<CommitSeq>,
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    allowance: GqlQueryPolicy,
    inputs: u64,
    work: u64,
) -> Result<Measured, NativeSpoolError> {
    let prepared = PreparedBufferedOrder::from_prepared_relation(query, cut).unwrap();
    let pool = MemoryPool::new(2_000_000, 0).unwrap();
    let (mut a, mut b, mut c) = (
        scratch(cx, &pool).await,
        scratch(cx, &pool).await,
        scratch(cx, &pool).await,
    );
    let result = prepared
        .spool_in_view(
            view, cx, allowance, &mut a, &mut b, &mut c, 2, 64, 2, 256, 127, 8192, inputs, work,
        )
        .await;
    let result = match result {
        Ok((spool, work)) => {
            let mut reader = spool.reader(&mut b);
            let mut rows = Vec::new();
            while let Some(bytes) = reader.next_row(cx).await.unwrap() {
                rows.push(GraphValueRow::decode_canonical(bytes.as_ref()).unwrap());
            }
            assert_eq!(reader.state(), fgdb_gql::scan_stream::ScanState::Exhausted);
            drop(reader);
            assert_eq!(rows.len() as u64, spool.row_count());
            Ok(Measured {
                rows,
                records: spool.row_stats(),
                evaluator: spool.evaluator_stats(),
                work,
                snapshot: spool.snapshot_seq(),
            })
        }
        Err(error) => Err(error),
    };
    assert_eq!(
        pool.used(),
        0,
        "scratch ownership survives neither acceptance nor refusal"
    );
    result
}
fn fixture_rows() -> (Vec<Pair>, Vec<Pair>) {
    (
        vec![
            [int(7), int(1)],
            [int(7), int(1)],
            [float(7.0), int(4)],
            [decimal(7), int(0)],
            [null(), int(0)],
            [int(9), int(0)],
        ],
        vec![
            [float(7.0), int(2)],
            [int(7), int(3)],
            [float(7.0), int(0)],
            [null(), int(2)],
            [int(8), int(3)],
        ],
    )
}

#[test]
fn inferred_on_keys_preserve_literal_bags_and_per_occurrence_absence_in_all_six_modes() {
    let ((), report) = run_async_under_lab(0x5a11_0201, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let (_db, vfs) = store(&c.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&c.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        let (left, right) = fixture_rows();
        for kind in KINDS {
            let query = query(input(&left), input(&right), kind, &selected_code());
            assert!(inferred(&query));
            let expected = expected(&left, &right, &[(0, 0), (0, 1), (1, 0), (1, 1)], kind);
            let result = run(&query, None, &mut view, &cx, policy(100), 100, WORK)
                .await
                .unwrap();
            assert_eq!(result.rows, expected, "{kind:?}");
            assert_eq!(result.records.snapshot_records, 0);
            for (skip, take) in [(0, 0), (1, 2), (100, 2)] {
                let page = query.clone().with_page(skip, Some(take));
                assert_eq!(
                    run(&page, None, &mut view, &cx, policy(take), 100, WORK)
                        .await
                        .unwrap()
                        .rows,
                    expected
                        .iter()
                        .skip(skip as usize)
                        .take(take as usize)
                        .cloned()
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(pool.used(), baseline);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn mapped_repeated_and_dynamic_probe_keys_match_the_live_unindexed_predicate() {
    let ((), report) = run_async_under_lab(0x5a11_0202, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let (_db, vfs) = store(&c.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&c.commit(), &vfs, &pool).await;
        let rows = vec![
            [int(7), float(7.0)],
            [float(7.0), int(7)],
            [decimal(7), int(7)],
            [int(9_007_199_254_740_993), float(9_007_199_254_740_992.0)],
            [null(), int(7)],
            [
                GraphValue::Vertex(VId(u128::MAX)),
                GraphValue::Edge(EId(u128::MAX)),
            ],
            [GraphValue::List(vec![int(7)].into_boxed_slice()), int(7)],
            [
                GraphValue::map(vec![("value".into(), int(7))]).unwrap(),
                int(7),
            ],
            [GraphValue::List(vec![null()].into_boxed_slice()), int(7)],
            [
                GraphValue::map(vec![("value".into(), null())]).unwrap(),
                int(7),
            ],
        ];
        for pairs in [
            vec![(0, 3), (1, 2)],
            vec![(0, 2), (0, 3)],
            vec![(0, 2), (1, 2)],
            vec![(0, 2), (0, 2)],
        ] {
            let mut code = vec![equal(pairs[0].0, pairs[0].1)];
            code.extend([equal(pairs[1].0, pairs[1].1), GraphSetPredicateOp::And]);
            let mut unindexed = code.clone();
            unindexed.extend([GraphSetPredicateOp::Not, GraphSetPredicateOp::Not]);
            for kind in KINDS {
                let selected = query(input(&rows), input(&rows), kind, &code);
                let fallback = query(input(&rows), input(&rows), kind, &unindexed);
                assert!(inferred(&selected));
                assert!(!inferred(&fallback));
                let expected = run(&fallback, None, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap();
                let actual = run(&selected, None, &mut view, &cx, policy(100), 100, WORK)
                    .await
                    .unwrap();
                assert_eq!(actual.rows, expected.rows, "{kind:?} {pairs:?}");
            }
        }
        let unsafe_or = [
            equal(0, 2),
            GraphSetPredicateOp::Truth(Some(true)),
            GraphSetPredicateOp::Or,
        ];
        let all = query(
            input(&rows[..2]),
            input(&rows[2..4]),
            RowJoinKind::Inner,
            &unsafe_or,
        );
        assert!(!inferred(&all));
        assert_eq!(
            run(&all, None, &mut view, &cx, policy(4), 100, WORK)
                .await
                .unwrap()
                .rows,
            expected(
                &rows[..2],
                &rows[2..4],
                &[(0, 0), (0, 1), (1, 0), (1, 1)],
                RowJoinKind::Inner
            )
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn inferred_join_empty_children_and_zero_pages_do_not_hide_input_failures_or_products() {
    let ((), report) = run_async_under_lab(0x5a11_0203, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let (_db, vfs) = store(&c.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&c.commit(), &vfs, &pool).await;
        let rows = vec![[int(1), int(2)]; 3];
        let hidden = query(
            input(&rows),
            input(&rows),
            RowJoinKind::Inner,
            &[equal(0, 2)],
        )
        .with_page(0, Some(0));
        assert!(inferred(&hidden));
        assert!(
            matches!(run(&hidden, None, &mut view, &cx, policy(0), 8, WORK).await,
            Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, fgdb_gql::GqlQueryError::Rows(_)))
        );
        assert!(
            run(&hidden, None, &mut view, &cx, policy(0), 9, WORK)
                .await
                .unwrap()
                .rows
                .is_empty()
        );
        for kind in KINDS {
            for empty_left in [false, true] {
                let (l, r): (&[Pair], &[Pair]) = if empty_left {
                    (&[], &rows)
                } else {
                    (&rows, &[])
                };
                let q = query(input(l), input(r), kind, &[equal(0, 2)]);
                assert_eq!(
                    run(&q, None, &mut view, &cx, policy(10), 100, WORK)
                        .await
                        .unwrap()
                        .rows,
                    expected(l, r, &[], kind)
                );
            }
        }
        let failed =
            PreparedGraphSetText::prepare("UNWIND [0] AS n RETURN n AS k,1/n AS v", symbols)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap();
        let q = query(input(&[]), failed, RowJoinKind::Anti, &[equal(0, 2)]).with_page(0, Some(0));
        assert!(inferred(&q));
        assert!(matches!(
            run(&q, None, &mut view, &cx, policy(0), 100, WORK).await,
            Err(NativeSpoolError::SetExecution(
                fgdb_gql::GraphSetExecutionError::Projection { .. }
            ))
        ));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn inferred_probe_sort_and_sweeps_share_exact_native_and_physical_allowances() {
    let ((), report) = run_async_under_lab(0x5a11_0204, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let (_db, vfs) = store(&c.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&c.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        let (left, right) = fixture_rows();
        for kind in KINDS {
            let q = query(input(&left), input(&right), kind, &selected_code());
            let measured = run(&q, None, &mut view, &cx, policy(100), 100, WORK)
                .await
                .unwrap();
            let exact = GqlQueryPolicy::new(
                measured.records.snapshot_records,
                measured.records.result_rows,
                measured.evaluator.work_units,
                measured.evaluator.scratch_entries,
            );
            let replay = run(&q, None, &mut view, &cx, exact, 100, measured.work)
                .await
                .unwrap();
            assert_eq!(replay.rows, measured.rows);
            assert_eq!(replay.work, measured.work);
            for dimension in 0..3 {
                let mut allowance = exact;
                let mut physical = measured.work;
                match dimension {
                    0 => allowance.evaluator.max_work_units -= 1,
                    1 => allowance.evaluator.max_scratch_entries -= 1,
                    _ => physical -= 1,
                }
                let error = run(&q, None, &mut view, &cx, allowance, 100, physical)
                    .await
                    .unwrap_err();
                if dimension == 2 {
                    assert!(
                        matches!(error, NativeSpoolError::SortWorkLimit { limit, .. } if limit == physical)
                    );
                }
                assert_eq!(pool.used(), baseline);
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn selective_on_keys_reduce_native_candidate_work_against_the_live_full_scan() {
    let ((), report) = run_async_under_lab(0x5a11_0205, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let (_db, vfs) = store(&c.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&c.commit(), &vfs, &pool).await;
        for n in [32_i64, 128] {
            let left: Vec<_> = (0..n).rev().map(|k| [int(k), int(0)]).collect();
            let right: Vec<_> = (0..n).map(|k| [float(k as f64), int(1)]).collect();
            let matches: Vec<_> = (0..n as usize).map(|l| (l, n as usize - 1 - l)).collect();
            for kind in [RowJoinKind::Inner, RowJoinKind::Full] {
                let selected = query(input(&left), input(&right), kind, &[equal(0, 2)]);
                let scan = query(
                    input(&left),
                    input(&right),
                    kind,
                    &[
                        equal(0, 2),
                        GraphSetPredicateOp::Not,
                        GraphSetPredicateOp::Not,
                    ],
                );
                assert!(inferred(&selected));
                assert!(!inferred(&scan));
                let indexed = run(
                    &selected,
                    None,
                    &mut view,
                    &cx,
                    policy(n as u64),
                    2 * n as u64,
                    WORK,
                )
                .await
                .unwrap();
                let scanned = run(
                    &scan,
                    None,
                    &mut view,
                    &cx,
                    policy(n as u64),
                    2 * n as u64,
                    WORK,
                )
                .await
                .unwrap();
                let expected = expected(&left, &right, &matches, kind);
                assert_eq!(indexed.rows, expected);
                assert_eq!(scanned.rows, expected);
                assert!(
                    indexed.evaluator.work_units < scanned.evaluator.work_units,
                    "n={n} {kind:?}: inferred={} full_scan={}",
                    indexed.evaluator.work_units,
                    scanned.evaluator.work_units
                );
                // These are logical counts, not a throughput/tail-latency claim.
                eprintln!(
                    "n={n} {kind:?} native={}/{} physical={}/{}",
                    indexed.evaluator.work_units,
                    scanned.evaluator.work_units,
                    indexed.work,
                    scanned.work
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
        (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
        _ => None,
    }
}
#[test]
fn inferred_spilled_equality_observes_historical_mixed_number_properties() {
    let ((), report) = run_async_under_lab(0x5a11_0206, |root| async move {
        let c = PurposeContexts::narrow_runtime_root(&root);
        let cx = c.query();
        let (mut db, vfs) = store(&c.commit()).await;
        let mut update = WriteBatch::new(RelationId(1));
        update.set_vertex_property(
            VId(2),
            PropertyKeyId(1),
            Some(CanonicalScalar::Float(CanonicalF64::new(8.0))),
        );
        assert_eq!(db.write(&c.commit(), update).await.unwrap(), CommitSeq(2));
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&c.commit(), &vfs, &pool).await;
        let leaf = PreparedGraphSetText::prepare("MATCH (n) RETURN n.p AS k,n.q AS v", symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let q = query(leaf.clone(), leaf, RowJoinKind::Full, &[equal(0, 2)]);
        assert!(inferred(&q));
        for (cut, middle, matches) in [
            (1, 7.0, vec![(0, 0), (0, 1), (1, 0), (1, 1), (2, 2)]),
            (2, 8.0, vec![(0, 0), (1, 1), (2, 2)]),
        ] {
            let rows = [
                [int(7), int(10)],
                [float(middle), int(20)],
                [int(9), int(30)],
            ];
            let measured = run(
                &q,
                Some(CommitSeq(cut)),
                &mut view,
                &cx,
                policy(10),
                100,
                WORK,
            )
            .await
            .unwrap();
            assert_eq!(measured.snapshot, CommitSeq(cut));
            assert_eq!(
                measured.rows,
                expected(&rows, &rows, &matches, RowJoinKind::Full)
            );
            assert!(measured.records.snapshot_records > 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
