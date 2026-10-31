//! Public prepared external-query routes, not explicit join-cursor selection.
//! Reuse the committed graph fixture; only scratch I/O is an in-memory test file.

use super::*;
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use fgdb::{NativeAggregateSpoolError, NativeSpoolError, PreparedNativeRead};
use fgdb_gql::algebra::GlaOperator;
use fgdb_gql::scan_stream::ScanState;
use fgdb_gql::spill_aggregate::AsyncSpillAggregatePlan;
use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_gql::{
    GraphAggregateValue, GraphExactAverage, PreparedGraphAggregateText, PreparedGraphSetText,
};
use fgdb_strata::tiered::memory::{SpillFile, SpillLimits};
use std::io::{self, Cursor, Seek, SeekFrom};
use std::pin::Pin;
use std::task::{Context, Poll};

const HEAD: &str = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b) WHERE b.p>=2 }";
const SORT_WORK: u64 = 100_000_000;
type AggregateRows = Vec<(Vec<GraphValue>, Vec<GraphAggregateValue>)>;

#[derive(Default)]
struct File(Cursor<Vec<u8>>);
impl AsyncRead for File {
    fn poll_read(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let bytes = &mut self.get_mut().0;
        let at = bytes.position() as usize;
        let count = output
            .remaining()
            .min(bytes.get_ref().len().saturating_sub(at));
        if count != 0 {
            output.put_slice(&bytes.get_ref()[at..at + count]);
            bytes.set_position((at + count) as u64);
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
    fn poll_seek(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        position: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        Poll::Ready(self.get_mut().0.seek(position))
    }
}
async fn file(cx: &QueryCx, pool: &MemoryPool) -> SpillFile<File> {
    SpillFile::new(
        cx,
        File::default(),
        pool.clone(),
        SpillLimits {
            max_file_bytes: 8_000_000,
            max_runs: 512,
            max_run_bytes: 1_000_000,
        },
    )
    .await
    .unwrap()
}
fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
    match (kind, name) {
        (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
        (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(P)),
        _ => None,
    }
}
fn native(text: &str) -> PreparedNativeRead {
    PreparedNativeRead::prepare(text, &GqlParameters::new(), resolve).unwrap()
}
fn encoded(values: &[i64]) -> Vec<Vec<u8>> {
    values
        .iter()
        .map(|value| {
            GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(*value))])
                .canonical_bytes()
                .unwrap()
        })
        .collect()
}
async fn ordered(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    text: &str,
    policy: GqlQueryPolicy,
) -> Result<(Vec<Vec<u8>>, CommitSeq), NativeSpoolError> {
    ordered_parameters(view, cx, text, &GqlParameters::new(), policy, 2)
        .await
        .map(|(rows, cut, _)| (rows, cut))
}

async fn ordered_parameters(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    text: &str,
    parameters: &GqlParameters,
    policy: GqlQueryPolicy,
    run_rows: usize,
) -> Result<(Vec<Vec<u8>>, CommitSeq, fgdb_gql::GqlExecutionStats), NativeSpoolError> {
    // A physical-plan refusal must fail the test here, not masquerade as the
    // execution refusal expected by the caller's LIMIT-zero/anti cases.
    let prepared = PreparedNativeRead::prepare(text, parameters, resolve)
        .unwrap()
        .prepare_buffered_order(parameters)
        .unwrap();
    let pool = MemoryPool::new(4_000_000, 0).unwrap();
    let mut scratch = file(cx, &pool).await;
    let mut destination = file(cx, &pool).await;
    let mut auxiliary = file(cx, &pool).await;
    let (spool, _) = prepared
        .spool_in_view(
            view,
            cx,
            policy,
            &mut scratch,
            &mut destination,
            &mut auxiliary,
            4,
            64,
            run_rows,
            256,
            1024,
            65_536,
            1000,
            SORT_WORK,
        )
        .await?;
    let mut reader = spool.reader(&mut destination);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await? {
        rows.push(row.as_ref().to_vec());
    }
    assert_eq!(reader.state(), ScanState::Exhausted);
    assert_eq!(spool.row_count(), rows.len() as u64);
    Ok((rows, spool.snapshot_seq(), spool.row_stats()))
}

#[test]
fn external_identity_roots_preserve_duplicates_nulls_and_history_with_bounded_fallback() {
    let ((), report) = run_async_under_lab(0xe6_71, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let mut extra = WriteBatch::new(R);
        extra.create_vertex(VId(13), vec![], vec![(P, CanonicalScalar::Int(13))]);
        for id in 100..164 {
            extra.create_vertex(VId(id), vec![], vec![(P, CanonicalScalar::Int(id as i64))]);
        }
        assert_eq!(
            db.write(&contexts.commit(), extra).await.unwrap(),
            CommitSeq(2)
        );
        let mut change = WriteBatch::new(R);
        change.delete_vertex(VId(13));
        change.set_vertex_property(IDS[2], P, Some(CanonicalScalar::Int(33)));
        assert_eq!(
            db.write(&contexts.commit(), change).await.unwrap(),
            CommitSeq(3)
        );
        let parameters = GqlParameters::new()
            .with_list(
                "ids",
                vec![
                    GraphValue::Vertex(IDS[2]),
                    GraphValue::Vertex(IDS[0]),
                    GraphValue::Vertex(IDS[2]),
                    GraphValue::Vertex(VId(13)),
                    GraphValue::Scalar(CanonicalScalar::Null),
                    GraphValue::Scalar(CanonicalScalar::Int(42)),
                    GraphValue::Vertex(VId(7)),
                ],
            )
            .unwrap();
        let source_pool = memory();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        for cut in [2, 3] {
            let text = format!(
                "UNWIND $ids AS wanted MATCH (n) WHERE n=wanted FOR SYSTEM_TIME AS OF SEQ {cut} RETURN n"
            );
            let logical = PreparedGraphSetText::prepare(
                "UNWIND $ids AS wanted MATCH (n) WHERE n=wanted RETURN n",
                resolve,
            )
            .unwrap()
            .bind_parameters(&parameters)
            .unwrap();
            let physical = AsyncSpillSetPlan::compile(&logical).unwrap();
            assert!(
                matches!(physical.source(), AsyncSpillSetSourcePlan::Cross(plan) if plan.vertex_seed_column() == Some(0))
            );
            let ids = if cut == 2 {
                vec![IDS[2], IDS[0], IDS[2], VId(13)]
            } else {
                vec![IDS[2], IDS[0], IDS[2]]
            };
            let expected: Vec<_> = ids
                .iter()
                .map(|id| {
                    GraphValueRow::from_owned_values(vec![GraphValue::Vertex(*id)])
                        .canonical_bytes()
                        .unwrap()
                })
                .collect();
            let (rows, snapshot, stats) = ordered_parameters(
                &mut view,
                &cx,
                &text,
                &parameters,
                GqlQueryPolicy::new(4, 100, SORT_WORK, SORT_WORK),
                8,
            )
            .await
            .unwrap();
            assert_eq!(rows, expected);
            assert_eq!(snapshot, CommitSeq(cut));
            assert_eq!(
                stats.snapshot_records, 4,
                "only distinct vertex seeds, including missing/retired identities"
            );
            for run_rows in [1, 2] {
                let (rows, _, stats) =
                    ordered_parameters(&mut view, &cx, &text, &parameters, policy(), run_rows)
                        .await
                        .unwrap();
                assert_eq!(
                    rows, expected,
                    "the ordinary larger-bag fallback preserves occurrences"
                );
                assert_eq!(stats.snapshot_records, 68);
            }
            let property = text.replace("RETURN n", "RETURN n,n.p AS value ORDER BY n");
            let (rows, _, _) =
                ordered_parameters(&mut view, &cx, &property, &parameters, policy(), 8)
                    .await
                    .unwrap();
            let mut fields: Vec<_> = ids
                .iter()
                .map(|id| {
                    let value = if *id == IDS[2] {
                        if cut == 2 { 3 } else { 33 }
                    } else if *id == IDS[0] {
                        1
                    } else {
                        13
                    };
                    (*id, value)
                })
                .collect();
            fields.sort();
            let expected: Vec<_> = fields
                .into_iter()
                .map(|(id, value)| {
                    GraphValueRow::from_owned_values(vec![
                        GraphValue::Vertex(id),
                        GraphValue::Scalar(CanonicalScalar::Int(value)),
                    ])
                    .canonical_bytes()
                    .unwrap()
                })
                .collect();
            assert_eq!(
                rows, expected,
                "all property reads keep the selected historical cut"
            );
        }
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_apply_identity_roots_do_not_rescan_the_graph_or_hide_late_errors() {
    let ((), report) = run_async_under_lab(0xe6_72, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let mut unrelated = WriteBatch::new(R);
        for id in 100..164 {
            unrelated.create_vertex(VId(id), vec![], vec![]);
        }
        db.write(&contexts.commit(), unrelated).await.unwrap();
        let parameters = GqlParameters::new()
            .with_list(
                "ids",
                vec![
                    GraphValue::Vertex(IDS[2]),
                    GraphValue::Vertex(IDS[0]),
                    GraphValue::Vertex(IDS[2]),
                    GraphValue::Scalar(CanonicalScalar::Null),
                    GraphValue::Scalar(CanonicalScalar::Int(42)),
                    GraphValue::Vertex(VId(7)),
                ],
            )
            .unwrap();
        let source_pool = memory();
        let mut view = cold(&contexts, &vfs, source_pool.clone()).await;
        let text =
            "UNWIND $ids AS wanted CALL (wanted) { MATCH (n) WHERE n=wanted RETURN n } RETURN n";
        let (rows, _, stats) = ordered_parameters(
            &mut view,
            &cx,
            text,
            &parameters,
            GqlQueryPolicy::new(4, 100, SORT_WORK, SORT_WORK),
            1,
        )
        .await
        .unwrap();
        let expected: Vec<_> = [IDS[2], IDS[0], IDS[2]]
            .into_iter()
            .map(|id| {
                GraphValueRow::from_owned_values(vec![GraphValue::Vertex(id)])
                    .canonical_bytes()
                    .unwrap()
            })
            .collect();
        assert_eq!(rows, expected);
        assert_eq!(
            stats.snapshot_records, 4,
            "each reached body spends only its bound root, duplicates included"
        );
        let limited = format!("{text} LIMIT 0");
        let error = ordered_parameters(
            &mut view,
            &cx,
            &limited,
            &parameters,
            GqlQueryPolicy::new(3, 0, SORT_WORK, SORT_WORK),
            1,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::BufferedExecute(ref error) if matches!(**error, GqlQueryError::Rows(ref error) if error.dimension == fgdb_gql::GqlBudgetDimension::SnapshotRecords && error.limit == 3 && error.observed == 4)),
            "{error:?}"
        );
        let groups = GqlParameters::new()
            .with_list(
                "groups",
                vec![
                    GraphValue::List(vec![GraphValue::Vertex(IDS[2])].into_boxed_slice()),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ],
            )
            .unwrap();
        let before = view.buffer_stats();
        let error = ordered_parameters(&mut view, &cx, "UNWIND $groups AS items UNWIND items AS wanted MATCH (n) WHERE n=wanted RETURN n LIMIT 0", &groups, policy(), 8).await.unwrap_err();
        assert!(
            matches!(error, NativeSpoolError::SetExecution(fgdb_gql::GraphSetExecutionError::Projection { error, .. }) if error.kind == fgdb_gql::GraphIntegerErrorKind::IncompatibleOperands)
        );
        assert_eq!(
            view.buffer_stats(),
            before,
            "all left errors precede point reads"
        );
        let empty = GqlParameters::new().with_list("ids", vec![]).unwrap();
        let (rows, _, stats) = ordered_parameters(
            &mut view,
            &cx,
            text,
            &empty,
            GqlQueryPolicy::new(0, 0, SORT_WORK, SORT_WORK),
            1,
        )
        .await
        .unwrap();
        assert!(rows.is_empty());
        assert_eq!(stats.snapshot_records, 0);
        drop(view);
        assert_eq!(source_pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
async fn aggregate(
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    text: &str,
    policy: GqlQueryPolicy,
) -> Result<AggregateRows, NativeAggregateSpoolError> {
    let prepared = native(text)
        .prepare_buffered_aggregate(&GqlParameters::new())
        .unwrap();
    let pool = MemoryPool::new(4_000_000, 0).unwrap();
    let mut source = file(cx, &pool).await;
    let mut partition = file(cx, &pool).await;
    let mut destination = file(cx, &pool).await;
    let (spool, _) = prepared
        .spool_in_view(
            view,
            cx,
            policy,
            &mut source,
            &mut partition,
            &mut destination,
            4,
            64,
            2,
            256,
            1024,
            65_536,
            1000,
            SORT_WORK,
            None,
        )
        .await?;
    let mut reader = spool.reader(&mut destination, None);
    let mut rows = Vec::new();
    while let Some(row) = reader.next_row(cx).await? {
        rows.push((row.keys().to_vec(), row.values().to_vec()));
    }
    assert_eq!(reader.state(), ScanState::Exhausted);
    assert_eq!(spool.row_count(), rows.len() as u64);
    Ok(rows)
}

#[test]
fn copy_only_probes_reach_external_order_and_with_stages_at_the_selected_cut() {
    let ((), report) = run_async_under_lab(0xe6_54, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(IDS[1], P, Some(CanonicalScalar::Null));
        db.write(&contexts.commit(), change).await.unwrap();
        let mut view = cold(&contexts, &vfs, memory()).await;
        let logical = prepare(&format!("{HEAD} RETURN b.p AS value"));
        assert!(
            logical
                .plan()
                .operators()
                .iter()
                .any(|op| matches!(op, GlaOperator::Probe { .. }))
        );
        assert!(
            !logical
                .plan()
                .operators()
                .iter()
                .any(|op| matches!(op, GlaOperator::Expand { .. }))
        );
        let relation = PreparedGraphSetText::prepare(
            &format!("{HEAD} WITH b.p AS value RETURN value"),
            resolve,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let physical = AsyncSpillSetPlan::compile(&relation).unwrap();
        assert!(matches!(
            physical.source(),
            AsyncSpillSetSourcePlan::Join(_)
        ));
        for cut in [1, 2] {
            for (tail, wanted) in [
                (
                    "RETURN b.p AS value ORDER BY value DESC SKIP 1 LIMIT 2",
                    if cut == 1 { vec![2, 2] } else { vec![] },
                ),
                (
                    "RETURN DISTINCT b.p AS value ORDER BY value DESC",
                    if cut == 1 { vec![3, 2] } else { vec![3] },
                ),
                (
                    "WITH b.p AS value RETURN value+10 AS result ORDER BY result DESC",
                    if cut == 1 {
                        vec![13, 12, 12, 12]
                    } else {
                        vec![13]
                    },
                ),
            ] {
                let text = format!("{HEAD} FOR SYSTEM_TIME AS OF SEQ {cut} {tail}");
                let (rows, snapshot) = ordered(&mut view, &cx, &text, policy()).await.unwrap();
                assert_eq!(rows, encoded(&wanted), "{text}");
                assert_eq!(snapshot, CommitSeq(cut));
            }
        }
        // NOT EXISTS with no expansion must also take the join contract. NULL
        // fails the child comparison, so those outer edges now survive the anti.
        let anti = "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b) WHERE b.p>=2 } \
            RETURN r ORDER BY r";
        let (rows, _) = ordered(&mut view, &cx, anti, policy()).await.unwrap();
        let expected: Vec<_> = [0, 1, 3, u128::MAX]
            .into_iter()
            .map(|id| {
                GraphValueRow::from_owned_values(vec![GraphValue::Edge(EId(id))])
                    .canonical_bytes()
                    .unwrap()
            })
            .collect();
        assert_eq!(rows, expected);
        // Ordinary single-edge scans remain admitted through their original path.
        let plain = "MATCH (a)-[r:R]->(b) RETURN r.p AS value ORDER BY value DESC";
        assert_eq!(
            ordered(&mut view, &cx, plain, policy()).await.unwrap().0,
            encoded(&[11, 10, 9, 8, 7])
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn copy_only_probes_feed_terminal_and_pipeline_exact_aggregates_without_spending_output_rows() {
    let ((), report) = run_async_under_lab(0xe6_55, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let definition = PreparedGraphAggregateText::prepare(
            &format!("{HEAD} RETURN COUNT(*) AS hits"),
            resolve,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        assert!(matches!(
            AsyncSpillAggregatePlan::compile(&definition).unwrap(),
            AsyncSpillAggregatePlan::Join(_)
        ));
        for (cut, count, sum) in [(1, 4, 9), (2, 1, 3)] {
            if cut == 2 {
                let mut change = WriteBatch::new(R);
                change.set_vertex_property(IDS[1], P, Some(CanonicalScalar::Null));
                db.write(&contexts.commit(), change).await.unwrap();
            }
            let mut view = cold(&contexts, &vfs, memory()).await;
            for tail in [
                "RETURN COUNT(*) AS hits,SUM(b.p) AS total,AVG(b.p) AS mean",
                "WITH b.p AS value RETURN COUNT(*) AS hits,SUM(value) AS total,AVG(value) AS mean",
            ] {
                let text = format!("{HEAD} {tail}");
                let one_output = GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000);
                let rows = aggregate(&mut view, &cx, &text, one_output).await.unwrap();
                assert_eq!(
                    rows,
                    vec![(
                        vec![],
                        vec![
                            GraphAggregateValue::Count(count),
                            GraphAggregateValue::Integer(sum),
                            GraphAggregateValue::Average(
                                GraphExactAverage::new(sum, count).unwrap()
                            ),
                        ]
                    )],
                    "{text} at {cut}"
                );
            }
            let empty = "MATCH (a)-[r:R]->(b) WHERE EXISTS { MATCH (b) WHERE b.p>99 } \
                RETURN COUNT(*) AS hits";
            assert_eq!(
                aggregate(&mut view, &cx, empty, policy()).await.unwrap(),
                vec![(vec![], vec![GraphAggregateValue::Count(0)])]
            );
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_probe_refusals_are_not_hidden_by_zero_windows_or_absence() {
    let ((), report) = run_async_under_lab(0xe6_56, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = seed(&contexts, &vfs).await;
        let pool = memory();
        let mut view = cold(&contexts, &vfs, pool.clone()).await;
        let late = "MATCH (a)-[r:R]->(b) WHERE EXISTS { \
            MATCH (b) WHERE b.p/(b.p-1)>0 } RETURN r ORDER BY r LIMIT 0";
        assert!(matches!(
            ordered(&mut view, &cx, late, policy()).await,
            Err(NativeSpoolError::BufferedExecute(_))
        ));
        let anti = "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (b) WHERE b.p=999 } \
            RETURN r ORDER BY r LIMIT 0";
        let refused = ordered(
            &mut view,
            &cx,
            anti,
            GqlQueryPolicy::new(1, 100, 10_000_000, 10_000_000),
        )
        .await;
        assert!(matches!(refused,
            Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Rows(_))));
        // Zero output is legal when full input evaluation actually succeeds.
        assert!(
            ordered(&mut view, &cx, anti, policy())
                .await
                .unwrap()
                .0
                .is_empty()
        );
        let independent = "MATCH (a)-[r:R]->(b) WHERE NOT EXISTS { MATCH (x) }";
        let before = pool.used();
        for tail in ["RETURN r LIMIT 0", "WITH r AS edge RETURN edge LIMIT 0"] {
            assert!(
                native(&format!("{independent} {tail}"))
                    .prepare_buffered_order(&GqlParameters::new())
                    .is_err()
            );
        }
        assert!(
            native(&format!("{independent} RETURN COUNT(*) AS hits LIMIT 0"))
                .prepare_buffered_aggregate(&GqlParameters::new())
                .is_err()
        );
        assert_eq!(pool.used(), before, "physical refusal opened no source");
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_probe_sources_feed_ordered_stages_finite_paths_and_exact_aggregates() {
    let ((), report) = run_async_under_lab(0xe6_59, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = seed(&contexts, &vfs).await;
        let mut change = WriteBatch::new(R);
        change.set_vertex_property(IDS[2], P, Some(CanonicalScalar::Int(13)));
        db.write(&contexts.commit(), change).await.unwrap();
        let pool = memory();
        let mut view = cold(&contexts, &vfs, pool.clone()).await;
        for cut in [1, 2] {
            let end = if cut == 1 { 3 } else { 13 };
            for (mode, interval, anti, expected) in [
                ("WALK", "2..2", false, vec![2, 1]),
                ("TRAIL", "2..2", false, vec![2, 1]),
                ("ACYCLIC", "2..2", false, vec![1]),
                ("SIMPLE", "2..2", false, vec![1]),
                ("WALK", "0..0", false, vec![end]),
                ("WALK", "1..2", true, vec![end]),
            ] {
                let text = format!(
                    "MATCH (a) FOR SYSTEM_TIME AS OF SEQ {cut} WHERE {}EXISTS {{ MATCH {mode} (a)-[:R*{interval}]->(b) WHERE b.p={end} }} WITH a.p AS value ORDER BY value DESC RETURN value ORDER BY value DESC",
                    if anti { "NOT " } else { "" }
                );
                let (rows, snapshot) = ordered(&mut view, &cx, &text, policy()).await.unwrap();
                assert_eq!(rows, encoded(&expected), "{text}");
                assert_eq!(snapshot, CommitSeq(cut));
            }
            let head = format!(
                "MATCH (a) FOR SYSTEM_TIME AS OF SEQ {cut} WHERE EXISTS {{ MATCH WALK (a)-[:R*1..2]->(b) WHERE b.p={end} }}"
            );
            for tail in [
                "RETURN COUNT(*) AS hits,SUM(a.p) AS total,AVG(a.p) AS average",
                "WITH a.p AS value RETURN COUNT(*) AS hits,SUM(value) AS total,AVG(value) AS average",
            ] {
                assert_eq!(
                    aggregate(
                        &mut view,
                        &cx,
                        &format!("{head} {tail}"),
                        GqlQueryPolicy::new(100_000, 1, 10_000_000, 10_000_000)
                    )
                    .await
                    .unwrap(),
                    vec![(
                        vec![],
                        vec![
                            GraphAggregateValue::Count(2),
                            GraphAggregateValue::Integer(3),
                            GraphAggregateValue::Average(GraphExactAverage::new(3, 2).unwrap())
                        ]
                    )]
                );
            }
            let text = format!(
                "{head} RETURN a.p AS value UNION ALL MATCH (x) FOR SYSTEM_TIME AS OF SEQ {cut} WHERE NOT EXISTS {{ MATCH (x)-[:R]->(y) WHERE y.p={end} }} RETURN x.p AS value ORDER BY value DESC"
            );
            assert_eq!(
                ordered(&mut view, &cx, &text, policy()).await.unwrap().0,
                encoded(&[end, 2, 1, 1])
            );
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn vertex_probe_external_errors_preserve_predicate_order_and_zero_window_barriers() {
    let ((), report) = run_async_under_lab(0xe6_5a, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let _db = seed(&contexts, &vfs).await;
        let pool = memory();
        let mut view = cold(&contexts, &vfs, pool.clone()).await;
        // The native predicate preceding the probe excludes every root. Its
        // unreachable local arithmetic must not be speculatively evaluated.
        let skipped =
            "MATCH (a) WHERE a.p<0 AND EXISTS { MATCH (a) WHERE 1/(a.p-a.p)=0 } RETURN a LIMIT 0";
        assert!(
            ordered(&mut view, &cx, skipped, policy())
                .await
                .unwrap()
                .0
                .is_empty()
        );
        for anti in ["", "NOT "] {
            for body in [
                "MATCH (a) WHERE 1/(a.p-a.p)=0",
                "MATCH (a)-[:R]->(b) WHERE 1/(b.p-b.p)=0",
            ] {
                let head = format!("MATCH (a) WHERE {anti}EXISTS {{ {body} }}");
                for tail in ["RETURN a LIMIT 0", "WITH a AS x RETURN x LIMIT 0"] {
                    assert!(matches!(
                        ordered(&mut view, &cx, &format!("{head} {tail}"), policy()).await,
                        Err(NativeSpoolError::BufferedExecute(_))
                    ));
                }
                assert!(
                    aggregate(
                        &mut view,
                        &cx,
                        &format!("{head} RETURN COUNT(*) AS hits LIMIT 0"),
                        policy()
                    )
                    .await
                    .is_err()
                );
            }
        }
        let anti =
            "MATCH (a) WHERE NOT EXISTS { MATCH (a)-[:R]->(b) WHERE b.p=999 } RETURN a LIMIT 0";
        let refused = ordered(
            &mut view,
            &cx,
            anti,
            GqlQueryPolicy::new(1, 100, 10_000_000, 10_000_000),
        )
        .await;
        assert!(
            matches!(refused, Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Rows(_)))
        );
        for head in [
            "MATCH (a) WHERE NOT EXISTS { MATCH (x) }",
            "MATCH (a) WHERE EXISTS { MATCH WALK (a)-[:R*]->(b) }",
        ] {
            for tail in ["RETURN a LIMIT 0", "WITH a AS x RETURN x LIMIT 0"] {
                assert!(
                    native(&format!("{head} {tail}"))
                        .prepare_buffered_order(&GqlParameters::new())
                        .is_err()
                );
            }
            assert!(
                native(&format!("{head} RETURN COUNT(*) AS hits LIMIT 0"))
                    .prepare_buffered_aggregate(&GqlParameters::new())
                    .is_err()
            );
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
