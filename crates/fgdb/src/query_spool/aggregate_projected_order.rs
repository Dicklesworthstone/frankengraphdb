//! Decode admitted paired scratch rows for the shared native rank comparator.
//! Output expressions ran during the canonical-group validation pass. Sorting
//! borrows their retained exact cells; it never evaluates or reads a source.

use super::*;

impl AggregateOrder<'_> {
    fn decode_rank_row(
        &self,
        bytes: &[u8],
        projected: bool,
        work: &Work<'_>,
    ) -> Result<NativeAggregateSpoolRow> {
        let (keys, values) = if projected {
            (
                self.definition.key_columns().len(),
                self.definition.aggregate_columns().len(),
            )
        } else {
            (
                self.definition.evaluation_key_columns().len(),
                self.definition.evaluation_aggregate_columns().len(),
            )
        };
        self.decode_rank_cells(bytes, keys, values, work)
    }

    fn decode_rank_cells(
        &self,
        bytes: &[u8],
        keys: usize,
        values: usize,
        work: &Work<'_>,
    ) -> Result<NativeAggregateSpoolRow> {
        // Reserve both the decoded envelope and its exact row before either
        // allocation. The envelope retires here; the row's guard survives the
        // comparison, including every early error.
        let charge = decoded_reservation(&self.pool, work.cx, bytes, 2)?;
        let frame = decode_row(bytes, self.resolver)?;
        let row = decode_envelope(&frame, keys, values)?;
        Ok(NativeAggregateSpoolRow {
            row,
            _charge: charge,
        })
    }

    pub(super) fn compare_ranked_projected(
        &mut self,
        left: &[u8],
        right: &[u8],
        work: &mut Work<'_>,
    ) -> Result<Ordering> {
        let (left, left_projected) = split_projected_frame(left)?;
        let (right, right_projected) = split_projected_frame(right)?;
        let rank = if self.definition.computed_rank_width() != 0 {
            Some((split_projected_frame(left)?, split_projected_frame(right)?))
        } else {
            None
        };
        let (left, right) = rank.map_or((left, right), |((left, _), (right, _))| (left, right));
        // Validate all four rows before a decisive projected key could hide a
        // malformed complete group or a malformed later projected column.
        // Only these two candidates coexist, independently of total groups.
        let left = self.decode_rank_row(left, false, work)?;
        let left_projected = self.decode_rank_row(left_projected, true, work)?;
        let right = self.decode_rank_row(right, false, work)?;
        let right_projected = self.decode_rank_row(right_projected, true, work)?;
        if let Some(((_, left_rank), (_, right_rank))) = rank {
            let width = self.definition.computed_rank_width();
            let left_rank = self.decode_rank_cells(left_rank, 0, width, work)?;
            let right_rank = self.decode_rank_cells(right_rank, 0, width, work)?;
            return self
                .definition
                .compare_precomputed_rank(&left, &left_rank, &right, &right_rank, &mut |event| {
                    self.input.charge(event)
                })
                .map_err(execute_error);
        }
        self.definition
            .compare_precomputed_output(
                &left,
                &left_projected,
                &right,
                &right_projected,
                &mut |event| self.input.charge(event),
            )
            .map_err(execute_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::algebra::{GraphColumn, GraphPatternBuilder};
    use fgdb_gql::{GraphAggregate, GraphSetProjection, GraphSetValue, PreparedGraphAggregate};
    use fgdb_types::PurposeContexts;

    struct Control {
        calls: usize,
        stop: usize,
    }
    impl GroupInput for Control {
        fn next_input(&mut self) -> crate::SendFuture<'_, Result<Option<SpoolRow>>> {
            panic!("ranking cannot read the graph")
        }
        fn charge(&mut self, event: VertexScanEvent) -> core::result::Result<(), ExecutionError> {
            assert_eq!(
                event,
                VertexScanEvent::Work,
                "native comparison only borrows"
            );
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
            panic!("comparison cannot deliver a result")
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

    fn definition(descending: bool, nulls_first: bool) -> SpillAggregateDefinition {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let input = builder
            .prepare_values(
                &[GraphColumn::property("key", "n", PropertyKeyId(1))],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let aggregate = PreparedGraphAggregate::prepare(
            input,
            &[0],
            &[GraphAggregate::average_int("average", 0)],
            0,
            None,
        )
        .unwrap()
        .with_output_projection(vec![GraphSetProjection::new(
            "score",
            GraphSetValue::Column(1),
        )])
        .unwrap()
        .with_projected_order_by(&[GraphValueOrder {
            column: 0,
            descending,
            nulls_first,
        }])
        .unwrap();
        let definition = SpillAggregatePlan::compile(&aggregate)
            .unwrap()
            .definition()
            .clone();
        assert!(definition.has_ordering());
        assert!(definition.has_projected_ordering());
        assert!(definition.ordering().is_empty());
        definition
    }

    fn pair(
        key: i64,
        value: GraphAggregateValue,
        pool: &MemoryPool,
        work: &mut Work<'_>,
    ) -> TrackedBytes {
        let complete = envelope(&GraphAggregateRow::from_group_values(
            vec![GraphValue::Scalar(CanonicalScalar::Int(key))],
            vec![value.clone()],
        ))
        .canonical_bytes()
        .unwrap();
        let projected = envelope(&GraphAggregateRow::from_group_values(vec![], vec![value]))
            .canonical_bytes()
            .unwrap();
        projected_frame(&complete, &projected, pool, work, 8192).unwrap()
    }

    fn fraction(numerator: i128, denominator: u64) -> GraphAggregateValue {
        GraphAggregateValue::Average(GraphExactAverage::new(numerator, denominator).unwrap())
    }

    #[test]
    fn external_projected_rank_preserves_wide_fractions_nulls_and_original_key_ties() {
        let ((), report) = run_async_under_lab(0x5ba1_1010, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let cases = [
                (
                    fraction(i128::MAX, u64::MAX),
                    fraction(i128::MAX - 1, u64::MAX - 1),
                    Ordering::Less,
                ),
                (
                    fraction(i128::MIN, u64::MAX),
                    fraction(i128::MIN + 1, u64::MAX - 1),
                    Ordering::Greater,
                ),
                (
                    GraphAggregateValue::Value(GraphValue::Scalar(CanonicalScalar::Null)),
                    fraction(0, 1),
                    Ordering::Greater,
                ),
            ];
            for (left, right, ascending) in cases {
                for descending in [false, true] {
                    for nulls_first in [false, true] {
                        let definition = definition(descending, nulls_first);
                        let expected = if left.is_null() {
                            if nulls_first {
                                Ordering::Less
                            } else {
                                Ordering::Greater
                            }
                        } else if descending {
                            ascending.reverse()
                        } else {
                            ascending
                        };
                        let mut work = Work {
                            cx: &cx,
                            used: 0,
                            limit: u64::MAX,
                        };
                        let a = pair(9, left.clone(), &pool, &mut work);
                        let b = pair(1, right.clone(), &pool, &mut work);
                        let retained = pool.used();
                        let mut control = Control {
                            calls: 0,
                            stop: usize::MAX,
                        };
                        let mut comparator = AggregateOrder {
                            definition: &definition,
                            input: &mut control,
                            pool: pool.clone(),
                            resolver: None,
                            distinct: false,
                        };
                        comparator.validate(a.as_ref(), 2, &mut work).unwrap();
                        comparator.validate(b.as_ref(), 2, &mut work).unwrap();
                        assert_eq!(
                            comparator
                                .compare(a.as_ref(), b.as_ref(), 2, &mut work)
                                .unwrap(),
                            expected
                        );
                        assert_eq!(pool.used(), retained);
                        drop(comparator);
                        drop(a);
                        drop(b);
                        assert_eq!(pool.used(), 0);
                    }
                }
            }
            for descending in [false, true] {
                let definition = definition(descending, false);
                for distinct in [false, true] {
                    let mut work = Work {
                        cx: &cx,
                        used: 0,
                        limit: u64::MAX,
                    };
                    let a = pair(9, fraction(6, 3), &pool, &mut work);
                    let b = pair(1, GraphAggregateValue::Integer(2), &pool, &mut work);
                    let mut control = Control {
                        calls: 0,
                        stop: usize::MAX,
                    };
                    let mut comparator = AggregateOrder {
                        definition: &definition,
                        input: &mut control,
                        pool: pool.clone(),
                        resolver: None,
                        distinct,
                    };
                    assert_eq!(
                        comparator
                            .compare_projected(a.as_ref(), b.as_ref(), &mut work)
                            .unwrap(),
                        Ordering::Equal
                    );
                    assert_eq!(
                        comparator
                            .compare(a.as_ref(), b.as_ref(), 2, &mut work)
                            .unwrap(),
                        Ordering::Greater
                    );
                    drop(comparator);
                    drop(a);
                    drop(b);
                    assert_eq!(pool.used(), 0);
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn projected_rank_refunds_every_interrupted_comparison_and_failed_decode_reservation() {
        let ((), report) = run_async_under_lab(0x5ba1_1011, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let storage = MemoryPool::new(1_000_000, 0).unwrap();
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let definition = definition(true, false);
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            let a = pair(9, fraction(3, 2), &storage, &mut work);
            let b = pair(1, fraction(6, 4), &storage, &mut work);
            let mut baseline = Control {
                calls: 0,
                stop: usize::MAX,
            };
            {
                let mut comparator = AggregateOrder {
                    definition: &definition,
                    input: &mut baseline,
                    pool: pool.clone(),
                    resolver: None,
                    distinct: false,
                };
                assert_eq!(
                    comparator
                        .compare(a.as_ref(), b.as_ref(), 2, &mut work)
                        .unwrap(),
                    Ordering::Greater
                );
            }
            assert!(baseline.calls > 0);
            assert_eq!(pool.used(), 0);
            for stop in 1..=baseline.calls {
                let mut control = Control { calls: 0, stop };
                {
                    let mut comparator = AggregateOrder {
                        definition: &definition,
                        input: &mut control,
                        pool: pool.clone(),
                        resolver: None,
                        distinct: false,
                    };
                    assert!(matches!(
                        comparator.compare(a.as_ref(), b.as_ref(), 2, &mut work),
                        Err(NativeAggregateSpoolError::Execute(_))
                    ));
                }
                assert_eq!(control.calls, stop);
                assert_eq!(pool.used(), 0);
            }
            let (ac, ap) = split_projected_frame(a.as_ref()).unwrap();
            let (bc, bp) = split_projected_frame(b.as_ref()).unwrap();
            let mut required = 0;
            // Refuse at each of the four decode allocations, not only the first.
            for bytes in [ac, ap, bc, bp] {
                required += account::decoded(bytes, &cx).unwrap() * 2;
                let limited = MemoryPool::new(required - 1, 0).unwrap();
                let mut control = Control {
                    calls: 0,
                    stop: usize::MAX,
                };
                let mut comparator = AggregateOrder {
                    definition: &definition,
                    input: &mut control,
                    pool: limited.clone(),
                    resolver: None,
                    distinct: false,
                };
                assert!(matches!(
                    comparator.compare(a.as_ref(), b.as_ref(), 2, &mut work),
                    Err(NativeAggregateSpoolError::Spool(NativeSpoolError::Spill(
                        SpillError::Memory(_)
                    )))
                ));
                assert_eq!(limited.used(), 0);
            }
            let mut control = Control {
                calls: 0,
                stop: usize::MAX,
            };
            let mut comparator = AggregateOrder {
                definition: &definition,
                input: &mut control,
                pool: pool.clone(),
                resolver: None,
                distinct: false,
            };
            work.used = 0;
            work.limit = 0;
            assert!(matches!(
                comparator.compare(a.as_ref(), b.as_ref(), 2, &mut work),
                Err(NativeAggregateSpoolError::Spool(
                    NativeSpoolError::SortWorkLimit { .. }
                ))
            ));
            assert_eq!(pool.used(), 0);
            drop(comparator);
            drop(a);
            drop(b);
            assert_eq!(storage.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn projected_rank_validates_both_complete_and_visible_schemas_before_decisive_keys() {
        let ((), report) = run_async_under_lab(0x5ba1_1012, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let storage = MemoryPool::new(1_000_000, 0).unwrap();
            let pool = MemoryPool::new(1_000_000, 0).unwrap();
            let definition = definition(false, false);
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            let good = pair(0, fraction(-100, 1), &storage, &mut work);
            let other = pair(1, fraction(100, 1), &storage, &mut work);
            let (complete, visible) = split_projected_frame(other.as_ref()).unwrap();
            let malformed = envelope(&GraphAggregateRow::from_group_values(vec![], vec![]))
                .canonical_bytes()
                .unwrap();
            for (complete, visible) in [
                (malformed.as_slice(), visible),
                (complete, malformed.as_slice()),
            ] {
                let bad = projected_frame(complete, visible, &storage, &mut work, 8192).unwrap();
                let mut control = Control {
                    calls: 0,
                    stop: usize::MAX,
                };
                let mut comparator = AggregateOrder {
                    definition: &definition,
                    input: &mut control,
                    pool: pool.clone(),
                    resolver: None,
                    distinct: false,
                };
                assert!(comparator.validate(bad.as_ref(), 2, &mut work).is_err());
                assert!(
                    comparator
                        .compare(good.as_ref(), bad.as_ref(), 2, &mut work)
                        .is_err()
                );
                assert!(
                    comparator
                        .compare(bad.as_ref(), good.as_ref(), 2, &mut work)
                        .is_err()
                );
                assert_eq!(pool.used(), 0);
            }
            drop(good);
            drop(other);
            assert_eq!(storage.used(), 0);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
