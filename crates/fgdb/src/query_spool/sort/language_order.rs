//! Query ORDER BY over authenticated frames, distinct from typed join/group keys.
//! Variable payloads stay borrowed. Only fixed-width numeric scalars are decoded
//! by fgdb-types, then compared by GQL's existing exact orderability kernel.
use super::*;
use fgdb_types::CanonicalScalar;

struct QueryOrder<'a> {
    keys: &'a [GraphValueOrder],
    explicit: bool,
}
impl FrameOrder for QueryOrder<'_> {
    type Error = NativeSpoolError;

    fn admit(&mut self, columns: usize) -> Result<()> {
        validate_order(self.keys, columns)
    }
    fn validate(&mut self, bytes: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
        // Includes every hidden/discarded cell before any comparison or page.
        validate(bytes, columns, work)
    }
    fn compare(
        &mut self,
        left: &[u8],
        right: &[u8],
        columns: usize,
        work: &mut Work<'_>,
    ) -> Result<Ordering> {
        if !self.explicit {
            return compare(left, right, self.keys, columns, work);
        }
        for key in self.keys {
            let a = column(left, columns, key.column, work)?;
            let b = column(right, columns, key.column, work)?;
            let order = match (a.is_null()?, b.is_null()?) {
                (true, true) => Ordering::Equal,
                (true, false) => {
                    if key.nulls_first {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (false, true) => {
                    if key.nulls_first {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
                (false, false) => {
                    let order = orderable(a, b, 0, work)?;
                    if key.descending {
                        order.reverse()
                    } else {
                        order
                    }
                }
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        // Numeric/list representation ties survive ALL explicit keys. Only
        // then may typed whole-row identity break ties, as in resident GQL.
        compare(left, right, &[], columns, work)
    }
}

// This is the frame adapter for GraphValue::compare_orderability's rank law.
// Scalar tags are the versioned fgdb-types encoding, not enum discriminants.
fn rank(value: Cell<'_>) -> Result<u8> {
    Ok(match value.tag {
        7 => 0,         // map
        1 => 1,         // vertex
        5 => 2,         // edge
        3 | 4 | 6 => 3, // all three list representations
        2 => 4,         // path
        0 => match value.scalar()?[0] {
            0x06 => 5,         // zoned datetime
            0x0d => 6,         // local datetime
            0x0a => 7,         // date
            0x0c => 8,         // zoned time
            0x0b => 9,         // local time
            0x0e => 10,        // duration
            0x07 => 11,        // bytes extension
            0x05 => 12,        // text
            0x01 => 13,        // Boolean
            0x02..=0x04 => 14, // exact Int/Decimal/Float numeric order
            0x00 => 15,        // nested NULL is last
            _ => return Err(invalid()),
        },
        _ => return Err(invalid()),
    })
}

fn list_cell<'a>(tag: u8, bytes: &mut &'a [u8]) -> Result<Cell<'a>> {
    match tag {
        3 | 4 => Ok(Cell {
            tag: if tag == 3 { 1 } else { 5 },
            body: take(bytes, 16)?,
        }),
        6 => Cell::next(bytes),
        _ => Err(invalid()),
    }
}

fn orderable(a: Cell<'_>, b: Cell<'_>, depth: usize, work: &mut Work<'_>) -> Result<Ordering> {
    work.charge(1)?;
    // Full frame validation already enforces MAX_LIST_DEPTH. A specialized
    // identity list has one implicit scalar level not present in its framing.
    if depth > GraphValue::MAX_LIST_DEPTH + 1 {
        return Err(invalid());
    }
    let (a_rank, b_rank) = (rank(a)?, rank(b)?);
    if a_rank != b_rank {
        return Ok(a_rank.cmp(&b_rank));
    }
    let (mut left, mut right) = (a.body, b.body);
    match a_rank {
        0 => {
            let a_len = count(&mut left)?;
            let b_len = count(&mut right)?;
            if a_len != b_len {
                return Ok(a_len.cmp(&b_len));
            }
            let (a_entries, b_entries) = (left, right);
            // Maps order by size, their COMPLETE key vectors, then their
            // orderable value vectors. Interleaved key/value order is wrong.
            for _ in 0..a_len {
                work.charge(1)?;
                let order = lex(frame(&mut left)?, frame(&mut right)?, work)?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
                Cell::next(&mut left)?;
                Cell::next(&mut right)?;
            }
            (left, right) = (a_entries, b_entries);
            for _ in 0..a_len {
                work.charge(1)?;
                frame(&mut left)?;
                frame(&mut right)?;
                let order = orderable(
                    Cell::next(&mut left)?,
                    Cell::next(&mut right)?,
                    depth + 1,
                    work,
                )?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(Ordering::Equal)
        }
        3 => {
            let a_len = count(&mut left)?;
            let b_len = count(&mut right)?;
            for _ in 0..a_len.min(b_len) {
                let order = orderable(
                    list_cell(a.tag, &mut left)?,
                    list_cell(b.tag, &mut right)?,
                    depth + 1,
                    work,
                )?;
                if order != Ordering::Equal {
                    return Ok(order);
                }
            }
            Ok(a_len.cmp(&b_len))
        }
        14 => {
            // The rank proves fixed-width numeric tags. Never decode Text,
            // Bytes, a collection or an artifact-bound temporal payload here.
            let (left, right) = (a.scalar()?, b.scalar()?);
            work.charge(left.len().saturating_add(right.len()))?;
            let left = CanonicalScalar::decode(left).map_err(|_| invalid())?;
            let right = CanonicalScalar::decode(right).map_err(|_| invalid())?;
            GraphValue::Scalar(left)
                .compare_orderability_with_control(&GraphValue::Scalar(right), &mut |_| {
                    work.charge(1)
                })
        }
        // Inside any remaining rank the native language and typed orders
        // agree. Reuse borrowed text/collation, temporal, path and ID handling.
        _ => compare_cells(a, b, 0, work),
    }
}

impl NativeResultSpool {
    /// Continue a query-owned ordering under its original physical allowance.
    /// `explicit` comes from the bound GLA/stage, never from observed values or
    /// the spelling of its keys. Implicit canonical sorting, join keys, set
    /// equality and aggregate argument support keep their typed comparators.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sort_query_continuing<'a, A, B>(
        &'a self,
        cx: &'a QueryCx,
        source: &'a mut SpillFile<A>,
        destination: &'a mut SpillFile<B>,
        keys: &'a [GraphValueOrder],
        explicit: bool,
        run_rows: usize,
        max_runs: usize,
        page_bytes: usize,
        max_work_units: u64,
        prior_work: u64,
    ) -> crate::SendFuture<'a, Result<(Self, u64)>>
    where
        A: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'a,
        B: AsyncRead + AsyncWrite + AsyncSeek + Unpin + Send + 'a,
    {
        Box::pin(async move {
            cx.with_restriction_async(self.sort_inner(
                cx,
                source,
                destination,
                &mut QueryOrder { keys, explicit },
                run_rows,
                max_runs,
                page_bytes,
                max_work_units,
                prior_work,
            ))
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::lab::run_async_under_lab;
    use fgdb_types::context::SimulationCheckpointProbe;
    use fgdb_types::{
        CanonicalDate, CanonicalDecimal, CanonicalDuration, CanonicalF64, CanonicalLocalDateTime,
        CanonicalLocalTime, CanonicalTime, EId, PurposeContexts, VId,
    };

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }
    fn float(value: f64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
    }
    fn decimal(value: i128, scale: u32) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Decimal(
            CanonicalDecimal::from_scaled_half_even(value, scale).unwrap(),
        ))
    }
    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }
    fn bytes(values: Vec<GraphValue>) -> Vec<u8> {
        GraphValueRow::from_owned_values(values)
            .canonical_bytes()
            .unwrap()
    }
    fn compare_rows(
        a: &[u8],
        b: &[u8],
        keys: &[GraphValueOrder],
        columns: usize,
        work: &mut Work<'_>,
    ) -> Result<Ordering> {
        QueryOrder {
            keys,
            explicit: true,
        }
        .compare(a, b, columns, work)
    }

    #[test]
    fn borrowed_query_order_matches_native_values_without_changing_typed_identity() {
        let ((), report) = run_async_under_lab(0x50a7_0021, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            let mut values = vec![
                float(f64::NEG_INFINITY),
                int(i64::MIN),
                int(-9_007_199_254_740_993),
                float(-9_007_199_254_740_992.0),
                decimal(-1, 1),
                int(0),
                float(-0.0),
                decimal(1, 1),
                float(0.1),
                int(1),
                decimal(10, 1),
                float(1.0),
                float(9_007_199_254_740_992.0),
                int(9_007_199_254_740_993),
                int(i64::MAX),
                float(9_223_372_036_854_775_808.0),
                float(f64::INFINITY),
                float(f64::NAN),
                GraphValue::Scalar(CanonicalScalar::Null),
                GraphValue::Scalar(CanonicalScalar::Bool(false)),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("text").unwrap()),
                GraphValue::Scalar(CanonicalScalar::bytes(vec![1, 0, 2]).unwrap()),
                GraphValue::Vertex(VId(1)),
                GraphValue::Edge(EId(1)),
                GraphValue::map(vec![]).unwrap(),
                GraphValue::map(vec![("z".into(), int(1))]).unwrap(),
                GraphValue::map(vec![("a".into(), int(0)), ("b".into(), int(0))]).unwrap(),
                GraphValue::Vertices(vec![VId(1), VId(2)].into_boxed_slice()),
                list(vec![GraphValue::Vertex(VId(1)), GraphValue::Vertex(VId(2))]),
                GraphValue::Edges(vec![EId(1)].into_boxed_slice()),
                list(vec![GraphValue::Edge(EId(1))]),
                list(vec![int(1)]),
                list(vec![float(1.0)]),
                list(vec![GraphValue::Scalar(CanonicalScalar::Null)]),
            ];
            let midnight = CanonicalLocalTime::MIDNIGHT;
            let date = CanonicalDate::from_epoch_day(-1).unwrap();
            for scalar in [
                CanonicalScalar::Date(date),
                CanonicalScalar::LocalTime(midnight),
                CanonicalScalar::Time(CanonicalTime::new(midnight, 3600).unwrap()),
                CanonicalScalar::LocalDateTime(CanonicalLocalDateTime::new(date, midnight)),
                CanonicalScalar::Duration(CanonicalDuration::new(-1, 2, -3).unwrap()),
            ] {
                values.push(GraphValue::Scalar(scalar.clone()));
                values.push(list(vec![GraphValue::Scalar(scalar)]));
            }
            values.push(GraphValue::Path(fgdb_gql::algebra::GraphPath::new(
                VId(0),
                Box::new([]),
            )));
            for a in &values {
                for b in &values {
                    let left = bytes(vec![a.clone()]);
                    let right = bytes(vec![b.clone()]);
                    validate(&left, 1, &mut work).unwrap();
                    validate(&right, 1, &mut work).unwrap();
                    for descending in [false, true] {
                        for nulls_first in [false, true] {
                            let semantic = match (a.is_null(), b.is_null()) {
                                (true, false) if nulls_first => Ordering::Less,
                                (true, false) => Ordering::Greater,
                                (false, true) if nulls_first => Ordering::Greater,
                                (false, true) => Ordering::Less,
                                _ if descending => b.compare_orderability(a),
                                _ => a.compare_orderability(b),
                            };
                            let keys = [GraphValueOrder {
                                column: 0,
                                descending,
                                nulls_first,
                            }];
                            assert_eq!(
                                compare_rows(&left, &right, &keys, 1, &mut work).unwrap(),
                                semantic.then_with(|| a.cmp(b))
                            );
                        }
                    }
                    assert_eq!(compare(&left, &right, &[], 1, &mut work).unwrap(), a.cmp(b));
                }
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn semantic_ties_defer_to_later_keys_before_the_canonical_whole_row() {
        let ((), report) = run_async_under_lab(0x50a7_0022, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            for (a, b) in [
                (int(7), float(7.0)),
                (list(vec![int(7)]), list(vec![float(7.0)])),
                (
                    GraphValue::map(vec![("k".into(), int(7))]).unwrap(),
                    GraphValue::map(vec![("k".into(), float(7.0))]).unwrap(),
                ),
            ] {
                let left = bytes(vec![a, int(20)]);
                let right = bytes(vec![b, int(10)]);
                let keys = [GraphValueOrder::ascending(0), GraphValueOrder::ascending(1)];
                assert_eq!(
                    compare_rows(&left, &right, &keys, 2, &mut work).unwrap(),
                    Ordering::Greater
                );
                assert_eq!(
                    compare(&left, &right, &keys, 2, &mut work).unwrap(),
                    Ordering::Less
                );
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn every_borrowed_comparison_refusal_preserves_frames_and_original_limit() {
        let ((), report) = run_async_under_lab(0x50a7_0023, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let text = GraphValue::Scalar(
                CanonicalScalar::ucs_basic_text(&"payload".repeat(600)).unwrap(),
            );
            let left = bytes(vec![
                GraphValue::map(vec![("key".into(), list(vec![text.clone(), int(1)]))]).unwrap(),
            ]);
            let right = bytes(vec![
                GraphValue::map(vec![("key".into(), list(vec![text, float(1.0)]))]).unwrap(),
            ]);
            let frozen = (left.clone(), right.clone());
            let keys = [GraphValueOrder::ascending(0)];
            let mut work = Work {
                cx: &cx,
                used: 17,
                limit: u64::MAX,
            };
            let expected = compare_rows(&left, &right, &keys, 1, &mut work).unwrap();
            let total = work.used;
            let mut limit = 17;
            while limit < total {
                let mut work = Work {
                    cx: &cx,
                    used: 17,
                    limit,
                };
                match compare_rows(&left, &right, &keys, 1, &mut work) {
                    Err(NativeSpoolError::SortWorkLimit {
                        attempted,
                        limit: actual,
                    }) => {
                        assert_eq!(actual, limit);
                        assert!(attempted > limit && attempted <= total);
                        // Visit every positive charge boundary, not every byte
                        // inside one already checkpointed comparison chunk.
                        limit = attempted;
                    }
                    other => panic!("expected a terminal work refusal, got {other:?}"),
                }
            }
            let mut work = Work {
                cx: &cx,
                used: 17,
                limit: total,
            };
            assert_eq!(
                compare_rows(&left, &right, &keys, 1, &mut work).unwrap(),
                expected
            );
            let probe = Arc::new(SimulationCheckpointProbe::new(None));
            let observed = cx.with_checkpoint_probe(Arc::clone(&probe));
            let mut work = Work {
                cx: &observed,
                used: 17,
                limit: total,
            };
            assert_eq!(
                compare_rows(&left, &right, &keys, 1, &mut work).unwrap(),
                expected
            );
            for stop in 1..=probe.calls() {
                let probe = Arc::new(SimulationCheckpointProbe::new(Some(stop)));
                let interrupted = cx.with_checkpoint_probe(Arc::clone(&probe));
                let mut work = Work {
                    cx: &interrupted,
                    used: 17,
                    limit: total,
                };
                assert!(matches!(
                    compare_rows(&left, &right, &keys, 1, &mut work),
                    Err(NativeSpoolError::Spill(SpillError::Interrupted(_)))
                ));
                assert_eq!(probe.calls(), stop);
            }
            assert_eq!((left, right), frozen);
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }

    #[test]
    fn explicit_query_order_still_validates_an_unselected_hidden_tail() {
        let ((), report) = run_async_under_lab(0x50a7_0024, |root| async move {
            let cx = PurposeContexts::narrow_runtime_root(&root).query();
            let keys = [GraphValueOrder::ascending(0)];
            let mut order = QueryOrder {
                keys: &keys,
                explicit: true,
            };
            let mut work = Work {
                cx: &cx,
                used: 0,
                limit: u64::MAX,
            };
            let valid = bytes(vec![int(1), list(vec![int(2), float(3.0)])]);
            order.admit(2).unwrap();
            order.validate(&valid, 2, &mut work).unwrap();
            // The selected first key is intact, but the final hidden cell is
            // truncated. Sorting a singleton cannot bypass full admission.
            assert!(matches!(
                order.validate(&valid[..valid.len() - 1], 2, &mut work),
                Err(NativeSpoolError::Spill(SpillError::InvalidRun))
            ));
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}
