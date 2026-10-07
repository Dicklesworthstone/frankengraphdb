//! Completed-group result clauses over private aggregate runs. Canonical key
//! order fixes HAVING's first domain error; typed rank order then selects the
//! output window. No input occurrence or incomplete group can be paginated.

use super::super::sort::FrameOrder;
use super::*;
use std::cmp::Ordering;

struct AggregateOrder<'a> {
    definition: &'a SpillAggregateDefinition,
    input: &'a mut dyn GroupInput,
    pool: MemoryPool,
    resolver: Option<&'a (dyn CanonicalScalarResolver + Send + Sync)>,
}

impl FrameOrder for AggregateOrder<'_> {
    type Error = NativeAggregateSpoolError;

    fn admit(&mut self, columns: usize) -> Result<()> {
        if columns
            != self.definition.evaluation_key_columns().len()
                + self.definition.evaluation_aggregate_columns().len()
        {
            return invalid();
        }
        Ok(())
    }

    fn validate(&mut self, bytes: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
        self.admit(columns)?;
        work.charge(bytes.len())?;
        let _charge = decoded_reservation(&self.pool, work.cx, bytes, 2)?;
        let frame = decode_row(bytes, self.resolver)?;
        decode_envelope(
            &frame,
            self.definition.evaluation_key_columns().len(),
            self.definition.evaluation_aggregate_columns().len(),
        )?;
        Ok(())
    }

    fn compare(
        &mut self,
        left: &[u8],
        right: &[u8],
        _columns: usize,
        work: &mut Work<'_>,
    ) -> Result<Ordering> {
        work.charge(
            left.len()
                .checked_add(right.len())
                .ok_or(SpillError::SizeOverflow)?,
        )?;
        // Each exact row and its decoded wire envelope coexist. Both sides
        // reserve before decoding; all four allocations retire after comparison.
        let _left_charge = decoded_reservation(&self.pool, work.cx, left, 2)?;
        let left = decode_row(left, self.resolver)?;
        let left = decode_envelope(
            &left,
            self.definition.evaluation_key_columns().len(),
            self.definition.evaluation_aggregate_columns().len(),
        )?;
        let _right_charge = decoded_reservation(&self.pool, work.cx, right, 2)?;
        let right = decode_row(right, self.resolver)?;
        let right = decode_envelope(
            &right,
            self.definition.evaluation_key_columns().len(),
            self.definition.evaluation_aggregate_columns().len(),
        )?;
        self.definition
            .compare_output(&left, &right, &mut |event| self.input.charge(event))
            .map_err(execute_error)
    }
}

fn remaining(work: &Work<'_>) -> Result<u64> {
    work.limit
        .checked_sub(work.used)
        .ok_or_else(|| SpillError::SizeOverflow.into())
}

fn add_sort_work(work: &mut Work<'_>, used: u64) -> Result<()> {
    work.used = work
        .used
        .checked_add(used)
        .ok_or(SpillError::SizeOverflow)?;
    if work.used > work.limit {
        return Err(NativeSpoolError::SortWorkLimit {
            attempted: work.used,
            limit: work.limit,
        }
        .into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish<A, B, C>(
    opened: &mut Opened<'_>,
    spool: NativeResultSpool,
    cx: &QueryCx,
    source: &mut SpillFile<A>,
    partition: &mut SpillFile<B>,
    destination: &mut SpillFile<C>,
    run_rows: usize,
    max_runs: usize,
    page_bytes: usize,
    max_row_bytes: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    C: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    // Reduction visits radix partitions, not canonical groups. Establish the
    // same complete-key order as the resident result stage before HAVING.
    let canonical = if !opened.definition.group_key_columns().is_empty() && spool.row_count() > 1 {
        let order: Vec<_> = (0..opened.definition.evaluation_key_columns().len())
            .map(|column| GraphValueOrder::ascending(column).with_nulls_first(true))
            .collect();
        let (sorted, used) = spool
            .sort_into(
                cx,
                destination,
                source,
                &order,
                run_rows,
                max_runs,
                page_bytes,
                remaining(work)?,
            )
            .await?;
        add_sort_work(work, used)?;
        sorted
    } else {
        copy_result(&spool, destination, source, page_bytes, work).await?
    };
    let filtered = filter(
        opened, &canonical, source, partition, page_bytes, work, resolver,
    )
    .await?;
    if !opened.definition.ordering().is_empty() && filtered.row_count() > 1 {
        let mut comparator = AggregateOrder {
            definition: &opened.definition,
            input: opened.input.as_mut(),
            pool: partition.memory_pool().clone(),
            resolver,
        };
        let (sorted, used) = filtered
            .sort_with(
                cx,
                partition,
                source,
                &mut comparator,
                run_rows,
                max_runs,
                page_bytes,
                remaining(work)?,
            )
            .await?;
        drop(comparator);
        add_sort_work(work, used)?;
        window(
            opened,
            &sorted,
            source,
            destination,
            page_bytes,
            max_row_bytes,
            work,
            resolver,
        )
        .await
    } else {
        window(
            opened,
            &filtered,
            partition,
            destination,
            page_bytes,
            max_row_bytes,
            work,
            resolver,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn filter<A, B>(
    opened: &mut Opened<'_>,
    spool: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    page_bytes: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let pool = source.memory_pool().clone();
    let mut reader = spool.reader(source);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    let mut count = 0_u64;
    let mut largest = 0;
    while let Some(bytes) = reader.next_row(work.cx).await? {
        work.charge(bytes.len())?;
        let _charge = decoded_reservation(&pool, work.cx, bytes.as_ref(), 2)?;
        let frame = decode_row(bytes.as_ref(), resolver)?;
        let row = decode_envelope(
            &frame,
            opened.definition.evaluation_key_columns().len(),
            opened.definition.evaluation_aggregate_columns().len(),
        )?;
        if opened
            .definition
            .qualifies_output(&row, &mut |event| opened.input.charge(event))
            .map_err(execute_error)?
        {
            work.write(&mut writer, bytes.as_ref()).await?;
            count = count.checked_add(1).ok_or(SpillError::SizeOverflow)?;
            largest = largest.max(bytes.len());
        }
    }
    if reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    let mut output = spool.clone();
    output.rows.result_rows = count;
    output.max_row_bytes = largest;
    output.run = writer.finish(work.cx).await?;
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
async fn window<A, B>(
    opened: &mut Opened<'_>,
    spool: &NativeResultSpool,
    source: &mut SpillFile<A>,
    destination: &mut SpillFile<B>,
    page_bytes: usize,
    max_row_bytes: usize,
    work: &mut Work<'_>,
    resolver: Option<&(dyn CanonicalScalarResolver + Send + Sync)>,
) -> Result<NativeResultSpool>
where
    A: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
    B: AsyncRead + AsyncWrite + AsyncSeek + Unpin,
{
    let pool = source.memory_pool().clone();
    let mut reader = spool.reader(source);
    let mut writer = destination.paged_writer(work.cx, page_bytes)?;
    let (offset, count) = opened.definition.result_window();
    let mut seen = 0_u64;
    let mut selected = 0_u64;
    let mut largest = 0;
    while let Some(bytes) = reader.next_row(work.cx).await? {
        work.charge(bytes.len())?;
        let take = seen >= offset && count.is_none_or(|count| selected < count);
        seen = seen.checked_add(1).ok_or(SpillError::SizeOverflow)?;
        if !take {
            continue;
        }
        let _decoded = decoded_reservation(&pool, work.cx, bytes.as_ref(), 2)?;
        // Reserve late key duplication, numeric copies and their output wire
        // envelope before invoking the existing governed projection helper.
        let copies = opened
            .definition
            .output_payload_copies()
            .checked_mul(2)
            .ok_or(SpillError::SizeOverflow)?;
        let _projected = decoded_reservation(&pool, work.cx, bytes.as_ref(), copies)?;
        let frame = decode_row(bytes.as_ref(), resolver)?;
        let row = decode_envelope(
            &frame,
            opened.definition.evaluation_key_columns().len(),
            opened.definition.evaluation_aggregate_columns().len(),
        )?;
        let row = opened
            .definition
            .project_output(row, &mut |event| opened.input.charge(event))
            .map_err(execute_error)?;
        opened.input.finish_result().map_err(execute_error)?;
        let envelope = envelope(&row);
        let (encoded, _codec) = account::encode(&pool, work, &envelope, max_row_bytes)?;
        work.write(&mut writer, &encoded).await?;
        largest = largest.max(encoded.len());
        selected = selected.checked_add(1).ok_or(SpillError::SizeOverflow)?;
    }
    if seen != spool.row_count() || reader.state() != ScanState::Exhausted {
        return Err(NativeSpoolError::IncompleteCursor.into());
    }
    let mut output = spool.clone();
    output.encoded_columns =
        opened.definition.key_columns().len() + opened.definition.aggregate_columns().len();
    output.rows = opened.input.row_stats();
    output.evaluator = opened.input.evaluator_stats();
    output.max_row_bytes = largest;
    output.run = writer.finish(work.cx).await?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_gql::{
        GraphAggregate, GraphAggregateColumn, GraphAggregateOrder, GraphNullPlacement,
        PreparedGraphAggregate,
    };
    use fgdb_types::PurposeContexts;

    struct Control {
        calls: usize,
        stop: usize,
    }
    impl GroupInput for Control {
        fn next_input(&mut self) -> core::result::Result<Option<GraphValueRow>, ExecutionError> {
            panic!("completed-group comparison must not reopen the graph")
        }
        fn charge(&mut self, _: VertexScanEvent) -> core::result::Result<(), ExecutionError> {
            self.calls += 1;
            if self.calls == self.stop {
                Err(GqlQueryError::Source(
                    fgdb_gql::GraphAggregateError::ResultCountOverflow,
                ))
            } else {
                Ok(())
            }
        }
        fn finish_result(&mut self) -> core::result::Result<(), ExecutionError> {
            panic!("comparison must not charge delivered rows")
        }
        fn exhausted(&self) -> bool {
            true
        }
        fn snapshot_seq(&self) -> CommitSeq {
            CommitSeq(1)
        }
        fn kind(&self) -> ScanKind {
            ScanKind::Vertex
        }
        fn row_stats(&self) -> GqlExecutionStats {
            GqlExecutionStats {
                snapshot_records: 0,
                result_rows: 0,
            }
        }
        fn evaluator_stats(&self) -> GlaExecutionStats {
            GlaExecutionStats::default()
        }
    }

    fn definition(order: GraphAggregateOrder) -> SpillAggregateDefinition {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[GraphColumn::property("p", "n", PropertyKeyId(1))],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let aggregate = PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[
                GraphAggregate::count_rows("count"),
                GraphAggregate::sum_int("sum", 0),
                GraphAggregate::average_int("average", 0),
            ],
            0,
            None,
        )
        .unwrap()
        .with_result_clauses(&[], &[order])
        .unwrap();
        SpillAggregatePlan::compile(&aggregate)
            .unwrap()
            .definition()
            .clone()
    }

    fn row(key: i64, count: u64, sum: i128, average: Option<(i128, u64)>) -> GraphAggregateRow {
        GraphAggregateRow::from_group_values(
            vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
            vec![
                GraphAggregateValue::Count(count),
                GraphAggregateValue::Integer(sum),
                average.map_or_else(
                    || GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                    |(sum, count)| {
                        GraphAggregateValue::Average(GraphExactAverage::new(sum, count).unwrap())
                    },
                ),
            ],
        )
    }

    #[test]
    fn exact_spilled_rank_handles_extremes_nulls_and_cancellation_without_losing_reservations() {
        let ((), report) = run_async_under_lab(0x5ba1_1002, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            // Rational cross-products here exceed i128. These inequalities
            // follow from (M-1)/(D-1) > M/D for M>D>1; negative ratios reverse
            // the conclusion. No floating or production comparator is an oracle.
            let cases = [
                (
                    0,
                    row(0, u64::MAX - 1, 0, Some((0, 1))),
                    row(1, u64::MAX, 0, Some((0, 1))),
                    Ordering::Less,
                ),
                (
                    1,
                    row(0, 1, i128::MIN, Some((0, 1))),
                    row(1, 1, i128::MAX, Some((0, 1))),
                    Ordering::Less,
                ),
                (
                    2,
                    row(0, 1, 0, Some((i128::MAX, u64::MAX))),
                    row(1, 1, 0, Some((i128::MAX - 1, u64::MAX - 1))),
                    Ordering::Less,
                ),
                (
                    2,
                    row(0, 1, 0, Some((i128::MIN, u64::MAX))),
                    row(1, 1, 0, Some((i128::MIN + 1, u64::MAX - 1))),
                    Ordering::Greater,
                ),
                (
                    2,
                    row(0, 1, 0, None),
                    row(1, 1, 0, Some((0, 1))),
                    Ordering::Greater,
                ),
            ];
            for (column, left, right, ascending) in cases {
                for descending in [false, true] {
                    for nulls in [GraphNullPlacement::First, GraphNullPlacement::Last] {
                        let definition = definition(GraphAggregateOrder {
                            column: GraphAggregateColumn::Aggregate(column),
                            descending,
                            nulls,
                        });
                        let left_is_null = left.values()[column].is_null();
                        let expected = if left_is_null {
                            if nulls == GraphNullPlacement::First {
                                Ordering::Less
                            } else {
                                Ordering::Greater
                            }
                        } else if descending {
                            ascending.reverse()
                        } else {
                            ascending
                        };
                        let a = envelope(&left).canonical_bytes().unwrap();
                        let b = envelope(&right).canonical_bytes().unwrap();
                        let mut baseline = Control {
                            calls: 0,
                            stop: usize::MAX,
                        };
                        let mut work = Work {
                            cx: &cx,
                            used: 0,
                            limit: u64::MAX,
                        };
                        let mut comparator = AggregateOrder {
                            definition: &definition,
                            input: &mut baseline,
                            pool: pool.clone(),
                            resolver: None,
                        };
                        assert_eq!(comparator.compare(&a, &b, 4, &mut work).unwrap(), expected);
                        drop(comparator);
                        assert_eq!(pool.used(), 0);
                        for stop in 1..=baseline.calls {
                            let mut control = Control { calls: 0, stop };
                            let mut comparator = AggregateOrder {
                                definition: &definition,
                                input: &mut control,
                                pool: pool.clone(),
                                resolver: None,
                            };
                            assert!(matches!(
                                comparator.compare(&a, &b, 4, &mut work),
                                Err(NativeAggregateSpoolError::Execute(_))
                            ));
                            drop(comparator);
                            assert_eq!(control.calls, stop);
                            assert_eq!(
                                pool.used(),
                                0,
                                "failed comparison must refund both decoded frames"
                            );
                        }
                    }
                }
            }
            let definition = definition(GraphAggregateOrder::descending(
                GraphAggregateColumn::Aggregate(2),
            ));
            let a = envelope(&row(2, 0, 0, Some((3, 2))))
                .canonical_bytes()
                .unwrap();
            let b = envelope(&row(1, 0, 0, Some((6, 4))))
                .canonical_bytes()
                .unwrap();
            let mut control = Control {
                calls: 0,
                stop: usize::MAX,
            };
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            let mut comparator = AggregateOrder {
                definition: &definition,
                input: &mut control,
                pool: pool.clone(),
                resolver: None,
            };
            assert_eq!(
                comparator.compare(&a, &b, 4, &mut work).unwrap(),
                Ordering::Greater,
                "DESC never reverses the hidden canonical-key tie break"
            );
            assert!(
                comparator
                    .validate(&a[..a.len() - 1], 4, &mut work)
                    .is_err()
            );
            assert_eq!(pool.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
