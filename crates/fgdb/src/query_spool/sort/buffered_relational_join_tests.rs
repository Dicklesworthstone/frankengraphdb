//! All graph sources use the existing Database/MemVfs fixture. Only expected
//! result construction and authenticated scratch I/O are test-owned.
use super::*;
use fgdb_gql::algebra::IntegerComparison;
use fgdb_gql::row_join::{RowJoinKind, RowJoinSpec};
use fgdb_gql::spill_set::{AsyncSpillSetPlan, AsyncSpillSetSourcePlan};
use fgdb_gql::{
    GraphSetColumnType, GraphSetOperand, GraphSetPredicateOp, PreparedGraphSet,
    PreparedGraphSetText, PreparedGraphText,
};

const KINDS: [RowJoinKind; 6] = [
    RowJoinKind::Inner, RowJoinKind::Left, RowJoinKind::Right,
    RowJoinKind::Full, RowJoinKind::Semi, RowJoinKind::Anti,
];
fn leaf(side: usize, null_key: bool) -> PreparedGraphSet {
    let endpoint = if side == 0 { "a" } else { "b" };
    let key = if null_key { "missing" } else { "p" };
    PreparedGraphText::prepare(
        &format!("MATCH (a)-[r:R]->(b) RETURN {endpoint}.{key} AS k,r.p AS value"), resolve,
    ).unwrap().bind_parameters(&GqlParameters::new()).unwrap().with_duplicates().into()
}
fn joined(kind: RowJoinKind, keyed: bool, residual: bool, null_key: bool) -> PreparedGraphSet {
    let types = [GraphSetColumnType::Scalar; 2];
    let spec = if keyed { RowJoinSpec::new(&types, &types, &[(0, 0)]) } else { RowJoinSpec::cross(&types, &types) }
        .unwrap().with_kind(kind);
    let spec = if residual {
        spec.with_predicate(&[GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(1), comparison: IntegerComparison::Less,
            right: GraphSetOperand::Column(3),
        }]).unwrap()
    } else { spec };
    leaf(0, null_key).join(leaf(1, null_key), spec).unwrap()
}
fn expected(cut: u64, kind: RowJoinKind, keyed: bool) -> Vec<Vec<u8>> {
    let first = if cut == 1 { 7 } else { 17 };
    let last = if cut == 1 { 4 } else { 14 };
    let left = [[3, first], [3, 8], [1, 9], [1, 10]];
    let right = [[1, first], [1, 8], [1, 9], [last, 10]];
    let matches = |l: &[i64; 2], r: &[i64; 2]| (!keyed || l[0] == r[0]) && l[1] < r[1];
    let pair = |l: Option<&[i64; 2]>, r: Option<&[i64; 2]>| {
        let values = l.map_or_else(|| vec![GraphValue::Scalar(CanonicalScalar::Null); 2], |l| l.iter().copied().map(scalar).collect());
        let mut values: Vec<GraphValue> = values;
        if !matches!(kind, RowJoinKind::Semi | RowJoinKind::Anti) {
            values.extend(r.map_or_else(|| vec![GraphValue::Scalar(CanonicalScalar::Null); 2], |r| r.iter().copied().map(scalar).collect()));
        }
        GraphValueRow::from_owned_values(values)
    };
    let mut rows = Vec::new();
    for l in &left {
        let witnesses: Vec<_> = right.iter().filter(|r| matches(l, r)).collect();
        match kind {
            RowJoinKind::Semi if !witnesses.is_empty() => rows.push(pair(Some(l), None)),
            RowJoinKind::Anti if witnesses.is_empty() => rows.push(pair(Some(l), None)),
            RowJoinKind::Semi | RowJoinKind::Anti => {},
            RowJoinKind::Left | RowJoinKind::Full if witnesses.is_empty() => rows.push(pair(Some(l), None)),
            _ => rows.extend(witnesses.into_iter().map(|r| pair(Some(l), Some(r)))),
        }
    }
    if matches!(kind, RowJoinKind::Right | RowJoinKind::Full) {
        rows.extend(right.iter().filter(|r| !left.iter().any(|l| matches(l, r))).map(|r| pair(None, Some(r))));
    }
    rows.sort();
    rows.into_iter().map(|row| row.canonical_bytes().unwrap()).collect()
}
type Measured = (Vec<Vec<u8>>, GqlExecutionStats, GlaExecutionStats, u64);
async fn execute(
    query: &PreparedGraphSet,
    cut: u64,
    view: &mut BufferedReadView<MemVfs>,
    cx: &QueryCx,
    allowance: GqlQueryPolicy,
    inputs: u64,
    work: u64,
) -> Result<Measured> {
    let prepared = PreparedBufferedOrder::from_prepared_relation(query, Some(CommitSeq(cut))).unwrap();
    let pool = MemoryPool::new(2_000_000, 0).unwrap();
    let (mut scratch, _) = file(cx, &pool).await;
    let (mut destination, _) = file(cx, &pool).await;
    let (mut auxiliary, _) = file(cx, &pool).await;
    let (spool, work) = prepared.spool_in_view(
        view, cx, allowance, &mut scratch, &mut destination, &mut auxiliary,
        2, 64, 2, 128, 127, 8192, inputs, work,
    ).await?;
    assert_eq!(spool.snapshot_seq(), CommitSeq(cut));
    let rows = contents(&spool, &mut destination, cx).await;
    assert_eq!(rows.len() as u64, spool.row_count());
    assert_eq!(pool.used(), 0);
    Ok((rows, spool.row_stats(), spool.evaluator_stats(), work))
}

#[test]
fn external_all_kind_joins_preserve_literal_bags_and_historical_inputs() {
    let ((), report) = run_async_under_lab(0xc01d_3101, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        for cut in [1, 2] {
            for keyed in [false, true] {
                for kind in KINDS {
                    let query = joined(kind, keyed, true, false);
                    let compiled = AsyncSpillSetPlan::compile(&query).unwrap();
                    assert!(matches!(compiled.source(), AsyncSpillSetSourcePlan::RowJoin(_)));
                    let (rows, _, _, _) = execute(&query, cut, &mut view, &cx, policy(100), 100, WORK).await.unwrap();
                    assert_eq!(rows, expected(cut, kind, keyed), "{cut} {kind:?} keyed={keyed}");
                    assert_eq!(pool.used(), baseline);
                }
            }
        }
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_null_keys_never_match_and_pages_do_not_hide_private_join_limits() {
    let ((), report) = run_async_under_lab(0xc01d_3102, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        for (kind, rows) in [(RowJoinKind::Inner, 0), (RowJoinKind::Full, 8), (RowJoinKind::Semi, 0), (RowJoinKind::Anti, 4)] {
            let query = joined(kind, true, false, true);
            assert_eq!(execute(&query, 2, &mut view, &cx, policy(100), 100, WORK).await.unwrap().0.len(), rows);
        }
        let query = joined(RowJoinKind::Full, true, false, false).with_page(0, Some(1));
        assert_eq!(execute(&query, 2, &mut view, &cx, policy(1), 9, WORK).await.unwrap().0.len(), 1);
        assert!(matches!(execute(&query, 2, &mut view, &cx, policy(1), 8, WORK).await,
            Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Rows(_))));
        let empty = query.with_page(0, Some(0));
        assert!(matches!(execute(&empty, 2, &mut view, &cx, policy(0), 8, WORK).await,
            Err(NativeSpoolError::BufferedExecute(error)) if matches!(*error, GqlQueryError::Rows(_))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn external_join_carries_exact_work_and_refuses_failed_absence_searches() {
    let ((), report) = run_async_under_lab(0xc01d_3103, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let baseline = pool.used();
        let query = joined(RowJoinKind::Anti, true, true, false);
        let (expected, records, native, physical) = execute(&query, 2, &mut view, &cx, policy(100), 100, WORK).await.unwrap();
        let exact = GqlQueryPolicy::new(records.snapshot_records, records.result_rows, native.work_units, native.scratch_entries);
        assert_eq!(execute(&query, 2, &mut view, &cx, exact, 100, physical).await.unwrap().0, expected);
        for dimension in 0..3 {
            let mut limited = exact;
            let mut external = physical;
            match dimension {
                0 => limited.evaluator.max_work_units -= 1,
                1 => limited.evaluator.max_scratch_entries -= 1,
                _ => external -= 1,
            }
            assert!(execute(&query, 2, &mut view, &cx, limited, 100, external).await.is_err());
            assert_eq!(pool.used(), baseline);
        }
        assert_eq!(execute(&query, 2, &mut view, &cx, policy(100), 100, WORK).await.unwrap().0, expected);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn empty_left_and_zero_parent_page_still_execute_the_failing_right_child() {
    let ((), report) = run_async_under_lab(0xc01d_3104, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let vfs = fixture(&contexts.commit()).await;
        let pool = MemoryPool::new(32_000_000, 0).unwrap();
        let mut view = view(&contexts.commit(), &vfs, &pool).await;
        let right = PreparedGraphSetText::prepare(
            "MATCH (a)-[r:R]->(b) RETURN b.p AS k,1/(a.p-a.p) AS value", resolve,
        ).unwrap().bind_parameters(&GqlParameters::new()).unwrap();
        let types = [GraphSetColumnType::Scalar; 2];
        let query = leaf(0, false).with_page(0, Some(0)).join(right,
            RowJoinSpec::new(&types, &types, &[(0, 0)]).unwrap().with_kind(RowJoinKind::Anti),
        ).unwrap().with_page(0, Some(0));
        assert!(matches!(execute(&query, 2, &mut view, &cx, policy(0), 100, WORK).await,
            Err(NativeSpoolError::SetExecution(fgdb_gql::GraphSetExecutionError::Projection { .. }))));
    });
    assert!(report.lab_test_passed(), "{report:?}");
}
