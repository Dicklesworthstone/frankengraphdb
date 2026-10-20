//! Sealed row projections for the shared pull operator. A leading vertex
//! identity makes canonical whole-row ordering and DISTINCT streamable without
//! a retained seen-set. Property-only output and alternate ordering refuse;
//! returning a different row order to avoid sorting would change semantics.

use super::{VertexScanEvent, VertexScanRow, seek};
use crate::GlaExecutionEvent;
use crate::algebra::{GlaOperator, GlaOutput, GraphValueRow, ValueProjection};
use crate::algebra_exec::ProjectedRows;
use fgdb_types::VId;

/// Closed pull-output shapes. The compiler proves that source identity order
/// is exactly the requested whole-row order and makes every row unique.
/// VId has no allocation; GraphValueRow shares the ordinary GLA property
/// collector, including nulls, canonical scalar variants and payload charges.
pub trait VertexScanOutput: GlaOutput + sealed::Projection + Send {}
impl VertexScanOutput for VId {}
impl VertexScanOutput for GraphValueRow {}

impl super::VertexScanPlan<GraphValueRow> {
    /// Compile the source of a blocking ORDER BY, without applying its window.
    /// Returns a complete terminal contract alongside the intermediate plan.
    /// The ordinary compiler still owns every predicate/probe and the native
    /// collector owns each value. Only the leading-identity ORDER proof is
    /// relaxed: any nonempty vertex-local value projection is legal here.
    ///
    /// The resulting cursor emits ALL occurrences in source identity order,
    /// not requested result order. Its ResultRows allowance/counter describes
    /// intermediate input, never the final query page. A host must explicitly
    /// budget that input and apply the returned tail before exposing results.
    /// Hidden key cells belong to these complete evaluation rows; the consumer
    /// removes them only after ordering and the query window.
    /// Ordinary compile(), and its ordered/unique-output proof, are unchanged.
    pub fn compile_sort_input(
        plan: &crate::algebra::GlaPlan<GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), super::VertexScanBuildError> {
        let tail = crate::scan_stream::ScanSortTail::compile(plan)
            .map_err(|operator| super::VertexScanBuildError { operator })?;
        // DISTINCT is intentionally deferred along with ordering. The row
        // collector emits every source occurrence; only the blocking consumer
        // may deduplicate complete rows before applying the retained window.
        let mut input = Self::compile_with_projection(plan, Some(&tail), |projection, order| {
            let GlaOperator::ProjectValues { columns } = projection else {
                return false;
            };
            matches!(
                order,
                GlaOperator::OrderByValues | GlaOperator::OrderByValueColumns { .. }
            ) && columns.iter().all(|column| match column {
                ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
                    slot.ordinal() == 0
                }
                _ => false,
            })
        })?;
        input.offset = 0;
        input.count = None;
        Ok((input, tail))
    }
}

impl<S: super::VertexScanSource, F> super::VertexScanCursor<S, F, GraphValueRow> {
    /// Pull one source row and complete its checked relational projection
    /// under this cursor's ORIGINAL work, scratch and cancellation controls.
    /// The host must compile this cursor from `pull.pattern()` and retain the
    /// same pull definition for the execution. No source is opened or retried.
    ///
    /// The source row stays private: only a successfully projected row spends
    /// ResultRows. Its zero-based ordinal therefore survives successive pulls
    /// without a second counter. Every projection event is charged immediately,
    /// before the VM performs the corresponding work or allocation. Private
    /// projection events never increment the public result count.
    ///
    /// `scan` preserves the host's native source/control error mapping;
    /// `arithmetic` retains the projection's row, column and typed data error.
    /// Either failure fuses the cursor and releases its source without demand.
    /// Earlier completed rows and all work/scratch charges remain recorded.
    /// This is logical admission, not a byte-memory or source-residency cap.
    pub fn next_projected<C, E>(
        &mut self,
        pull: &crate::GraphSetRowPull,
        mut scan: impl FnMut(crate::GqlQueryError<super::VertexScanError<S::Error>, C>) -> E,
        arithmetic: impl FnOnce(u64, usize, crate::GraphIntegerError) -> E,
    ) -> Option<Result<GraphValueRow, E>>
    where
        F: FnMut() -> Result<(), C>,
    {
        use super::VertexScanState;
        if self.state != VertexScanState::Open {
            return None;
        }
        let result: Result<Option<GraphValueRow>, E> = (|| {
            let Some(row) = self.advance_inner::<false, C>().map_err(&mut scan)? else {
                return Ok(None);
            };
            let ordinal = self.meter.rows.result_rows;
            self.meter.next_result_count().map_err(&mut scan)?;
            let projected = pull.project(
                &row,
                ordinal,
                &mut |event| {
                    self.meter
                        .event(match event {
                            GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                            GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                                VertexScanEvent::Work
                            }
                        })
                        .map_err(&mut scan)
                },
                |column, error| arithmetic(ordinal, column, error),
            )?;
            self.meter.emit().map_err(&mut scan)?;
            Ok(Some(projected))
        })();
        match result {
            Ok(Some(row)) => {
                if self.plan.count == Some(self.meter.rows.result_rows) {
                    self.state = VertexScanState::Exhausted;
                    self.source = None;
                }
                Some(Ok(row))
            }
            Ok(None) => {
                self.state = VertexScanState::Exhausted;
                self.source = None;
                None
            }
            Err(error) => {
                self.state = VertexScanState::Failed;
                self.source = None;
                Some(Err(error))
            }
        }
    }
}

// Sealed: unnameable by design, so no other crate can implement it.
#[allow(unnameable_types)]
mod sealed {
    use super::*;

    pub trait Projection: Sized {
        fn accepts_projection(projection: &GlaOperator, order: &GlaOperator) -> bool;
        fn project<E: From<crate::GraphIntegerError>>(
            vid: VId,
            row: VertexScanRow<'_>,
            projection: &GlaOperator,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
        ) -> Result<Self, E>;
    }
    impl Projection for VId {
        fn accepts_projection(projection: &GlaOperator, order: &GlaOperator) -> bool {
            matches!(projection, GlaOperator::Project { slot } if slot.ordinal() == 0)
                && matches!(order, GlaOperator::OrderByVertexId)
        }
        fn project<E: From<crate::GraphIntegerError>>(
            vid: VId,
            _row: VertexScanRow<'_>,
            _projection: &GlaOperator,
            _control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
        ) -> Result<Self, E> {
            Ok(vid)
        }
    }
    impl Projection for GraphValueRow {
        fn accepts_projection(projection: &GlaOperator, order: &GlaOperator) -> bool {
            let GlaOperator::ProjectValues { columns } = projection else {
                return false;
            };
            matches!(order, GlaOperator::OrderByValues)
                && matches!(columns.first(), Some(ValueProjection::Vertex { slot }) if slot.ordinal() == 0)
                && columns.iter().all(|column| match column {
                    ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
                        slot.ordinal() == 0
                    }
                    _ => false,
                })
        }
        fn project<E: From<crate::GraphIntegerError>>(
            vid: VId,
            row: VertexScanRow<'_>,
            projection: &GlaOperator,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
        ) -> Result<Self, E> {
            collect_one::<Self, E>(vid, row, projection, control)
        }
    }
}

fn collect_one<Row: GlaOutput, E: From<crate::GraphIntegerError>>(
    vid: VId,
    row: VertexScanRow<'_>,
    projection: &GlaOperator,
    control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
) -> Result<Row, E> {
    // These callbacks are invoked serially by the sealed GLA collector. A
    // short RefCell borrow lets field lookups and collector payload copies
    // debit the SAME caller meter, without a second budget or deferred charge.
    let control = std::cell::RefCell::new(control);
    let mut projected = ProjectedRows::new(true);
    Row::collect_properties(
        projection,
        &[Some(vid)],
        &mut projected,
        &mut |asked, key| {
            debug_assert_eq!(asked, vid, "the checked projection names only slot zero");
            seek(
                row.properties,
                &key,
                |entry| entry.0,
                &mut **control.borrow_mut(),
            )
            .map(|found| found.map(|(_, value)| value))
        },
        &mut |event| {
            (**control.borrow_mut())(match event {
                GlaExecutionEvent::ScratchEntry => VertexScanEvent::ScratchEntry,
                GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => VertexScanEvent::Work,
            })
        },
    )?;
    let mut rows = projected.into_rows();
    let value = rows
        .next()
        .expect("a checked nonempty projection produces one complete row");
    debug_assert!(
        rows.next().is_none(),
        "a row-local projection cannot produce a second row"
    );
    Ok(value)
}

#[cfg(test)]
mod projected_tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder, GraphValue};
    use crate::stream::{
        VertexScanCursor, VertexScanError, VertexScanPlan, VertexScanSource, VertexScanSourceError,
        VertexScanState,
    };
    use crate::{
        GlaLimitDimension, GqlQueryError, GqlQueryPolicy, GraphIntegerBinary, GraphIntegerError,
        GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp, GraphSetProjection,
        GraphSetQuantifier, GraphSetRowPull, GraphSetValue, PreparedGraphSet,
    };
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{CanonicalScalar, CommitSeq};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    const P: PropertyKeyId = PropertyKeyId(1);

    struct Source {
        rows: Vec<(
            VId,
            Vec<(PropertyKeyId, fgdb_types::CanonicalPropertyValue)>,
        )>,
        at: usize,
        fail: Option<VId>,
        reads: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }
    impl Source {
        fn new(values: [i64; 3]) -> Self {
            Self {
                rows: [VId(0), VId(9), VId(u128::MAX)]
                    .into_iter()
                    .zip(values)
                    .map(|(id, value)| (id, vec![(P, CanonicalScalar::Int(value).into())]))
                    .collect(),
                at: 0,
                fail: None,
                reads: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }
    impl Drop for Source {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl VertexScanSource for Source {
        type Error = &'static str;
        fn snapshot_seq(&self) -> CommitSeq {
            CommitSeq(23)
        }
        fn next_vertex<C>(
            &mut self,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VId>, VertexScanSourceError<Self::Error, C>> {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            let next = self.rows.get(self.at).map(|row| row.0);
            if next.is_some() {
                self.at += 1;
            }
            Ok(next)
        }
        fn vertex<C>(
            &self,
            id: VId,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), C>,
        ) -> Result<Option<VertexScanRow<'_>>, VertexScanSourceError<Self::Error, C>> {
            control(VertexScanEvent::Work).map_err(VertexScanSourceError::Control)?;
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.fail == Some(id) {
                return Err(VertexScanSourceError::Source("late source failure"));
            }
            Ok(self
                .rows
                .iter()
                .find(|row| row.0 == id)
                .map(|row| VertexScanRow {
                    labels: &[],
                    properties: &row.1,
                }))
        }
    }

    #[derive(Debug)]
    enum Failure {
        Scan(GqlQueryError<VertexScanError<&'static str>, usize>),
        Projection {
            row: u64,
            column: usize,
            error: GraphIntegerError,
        },
    }
    fn pull_next<F: FnMut() -> Result<(), usize>>(
        cursor: &mut VertexScanCursor<Source, F, GraphValueRow>,
        pull: &GraphSetRowPull,
    ) -> Option<Result<GraphValueRow, Failure>> {
        cursor.next_projected(pull, Failure::Scan, |row, column, error| {
            Failure::Projection { row, column, error }
        })
    }
    fn wide() -> GqlQueryPolicy {
        GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, u64::MAX)
    }
    fn definition(divide: bool, payload: &str) -> GraphSetRowPull {
        let mut builder = GraphPatternBuilder::new();
        builder.vertex("n").unwrap();
        let pattern = builder
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("p", "n", P),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates();
        let ops = if divide {
            vec![
                GraphIntegerOp::Literal(Some(10)),
                GraphIntegerOp::ScalarColumn(1),
                GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
            ]
        } else {
            vec![
                GraphIntegerOp::ScalarColumn(1),
                GraphIntegerOp::Literal(Some(1)),
                GraphIntegerOp::Binary(GraphIntegerBinary::Add),
            ]
        };
        PreparedGraphSet::from(pattern)
            .project(
                vec![
                    GraphSetProjection::new("id", GraphSetValue::Column(0)),
                    GraphSetProjection::new(
                        "value",
                        GraphSetValue::Integer(
                            GraphIntegerExpression::prepare_scalar(&ops).unwrap(),
                        ),
                    ),
                    GraphSetProjection::new(
                        "payload",
                        GraphSetValue::Value(GraphValue::Scalar(
                            CanonicalScalar::ucs_basic_text(payload).unwrap(),
                        )),
                    ),
                ],
                GraphSetQuantifier::All,
            )
            .unwrap()
            .row_pull()
            .unwrap()
    }

    #[test]
    fn projected_pulls_charge_the_vm_events_to_the_source_meter_before_delivery() {
        let payload = "visible-é".repeat(128);
        let pull = definition(false, &payload);
        let plan = VertexScanPlan::compile(pull.pattern().plan()).unwrap();
        // Separate the native scan's charges and the actual VM's event stream.
        // Omitting either set of events in next_projected must break equality.
        let mut raw = VertexScanCursor::new(Source::new([2, 5, 9]), plan.clone(), wide(), || {
            Ok::<_, usize>(())
        });
        let input = raw.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        let mut projection_work = 0;
        let mut projection_scratch = 0;
        for (ordinal, row) in input.iter().enumerate() {
            pull.project(
                row,
                ordinal as u64,
                &mut |event| {
                    match event {
                        GlaExecutionEvent::ScratchEntry => projection_scratch += 1,
                        _ => projection_work += 1,
                    }
                    Ok::<_, GraphIntegerError>(())
                },
                |_, error| error,
            )
            .unwrap();
        }
        assert!(projection_work > 0 && projection_scratch > 0);
        let source = Source::new([2, 5, 9]);
        let drops = source.drops.clone();
        let reads = source.reads.clone();
        let mut cursor = VertexScanCursor::new(source, plan, wide(), || Ok::<_, usize>(()));
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        let mut rows = Vec::new();
        while let Some(row) = pull_next(&mut cursor, &pull) {
            rows.push(row.unwrap());
            assert_eq!(cursor.row_stats().result_rows, rows.len() as u64);
        }
        for ((row, id), value) in rows
            .iter()
            .zip([VId(0), VId(9), VId(u128::MAX)])
            .zip([3, 6, 10])
        {
            assert_eq!(
                row.values(),
                &[
                    GraphValue::Vertex(id),
                    GraphValue::Scalar(CanonicalScalar::Int(value)),
                    GraphValue::Scalar(CanonicalScalar::ucs_basic_text(&payload).unwrap()),
                ]
            );
        }
        assert_eq!(rows.len(), 3);
        assert_eq!(cursor.snapshot_seq(), CommitSeq(23));
        assert_eq!(cursor.row_stats(), raw.row_stats());
        assert_eq!(
            cursor.evaluator_stats().scratch_entries,
            raw.evaluator_stats().scratch_entries + projection_scratch
        );
        // Each private source row has its existing completion event, and each
        // completed projected result has one additional final emit checkpoint.
        // Every evaluator event is one work unit (algebra_exec::charge), a
        // scratch entry included, so the projection's scratch entries count
        // here as well as above.
        assert_eq!(
            cursor.evaluator_stats().work_units,
            raw.evaluator_stats().work_units + projection_work + projection_scratch + 3
        );
        assert_eq!(cursor.state(), VertexScanState::Exhausted);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(pull_next(&mut cursor, &pull).is_none());
    }

    #[test]
    fn projection_scratch_cannot_hide_behind_a_scan_that_fits_the_limit() {
        let pull = definition(false, &"x".repeat(4096));
        let plan = VertexScanPlan::compile(pull.pattern().plan()).unwrap();
        let mut raw = VertexScanCursor::new(Source::new([2, 5, 9]), plan.clone(), wide(), || {
            Ok::<_, usize>(())
        });
        assert_eq!(
            raw.by_ref().collect::<Result<Vec<_>, _>>().unwrap().len(),
            3
        );
        let allowance = raw.evaluator_stats().scratch_entries;
        let policy = GqlQueryPolicy::new(u64::MAX, u64::MAX, u64::MAX, allowance);
        let source = Source::new([2, 5, 9]);
        let drops = source.drops.clone();
        let mut cursor = VertexScanCursor::new(source, plan, policy, || Ok::<_, usize>(()));
        assert!(matches!(
            pull_next(&mut cursor, &pull),
            Some(Err(Failure::Scan(GqlQueryError::Evaluator(error))))
                if error.dimension == GlaLimitDimension::ScratchEntries
        ));
        assert_eq!(cursor.row_stats().result_rows, 0);
        assert_eq!(cursor.state(), VertexScanState::Failed);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(cursor.next().is_none());
        assert!(pull_next(&mut cursor, &pull).is_none());
    }

    #[test]
    fn projection_limits_and_every_checkpoint_remain_cumulative_and_terminal() {
        let pull = definition(false, "payload");
        let plan = VertexScanPlan::compile(pull.pattern().plan()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let mut cursor =
            VertexScanCursor::new(Source::new([2, 5, 9]), plan.clone(), wide(), move || {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok::<_, usize>(())
            });
        let mut expected = Vec::new();
        while let Some(row) = pull_next(&mut cursor, &pull) {
            expected.push(row.unwrap());
        }
        let stats = cursor.evaluator_stats();
        let exact = GqlQueryPolicy::new(3, 3, stats.work_units, stats.scratch_entries);
        let mut cursor = VertexScanCursor::new(Source::new([2, 5, 9]), plan.clone(), exact, || {
            Ok::<_, usize>(())
        });
        let mut got = Vec::new();
        while let Some(row) = pull_next(&mut cursor, &pull) {
            got.push(row.unwrap());
        }
        assert_eq!(got, expected);
        for (policy, dimension) in [
            (
                GqlQueryPolicy::new(3, 3, stats.work_units - 1, stats.scratch_entries),
                GlaLimitDimension::WorkUnits,
            ),
            (
                GqlQueryPolicy::new(3, 3, stats.work_units, stats.scratch_entries - 1),
                GlaLimitDimension::ScratchEntries,
            ),
        ] {
            let mut cursor =
                VertexScanCursor::new(Source::new([2, 5, 9]), plan.clone(), policy, || {
                    Ok::<_, usize>(())
                });
            let error = loop {
                match pull_next(&mut cursor, &pull).expect("one-unit-short must refuse") {
                    Ok(_) => {}
                    Err(error) => break error,
                }
            };
            assert!(matches!(
                error,
                Failure::Scan(GqlQueryError::Evaluator(error)) if error.dimension == dimension
            ));
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert!(cursor.next().is_none());
        }
        for stop in 1..=calls.load(Ordering::SeqCst) {
            let source = Source::new([2, 5, 9]);
            let drops = source.drops.clone();
            let mut at = 0;
            let mut cursor = VertexScanCursor::new(source, plan.clone(), exact, || {
                at += 1;
                if at == stop { Err(stop) } else { Ok(()) }
            });
            let mut delivered = 0;
            let error = loop {
                match pull_next(&mut cursor, &pull).expect("checkpoint must be visited") {
                    Ok(_) => delivered += 1,
                    Err(error) => break error,
                }
            };
            assert!(matches!(error, Failure::Scan(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(cursor.row_stats().result_rows, delivered);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(pull_next(&mut cursor, &pull).is_none());
            assert!(cursor.next().is_none());
        }
    }

    #[test]
    fn late_projection_and_source_errors_keep_their_types_and_no_private_row_escapes() {
        let pull = definition(true, "payload");
        let plan = VertexScanPlan::compile(pull.pattern().plan()).unwrap();
        for source_failure in [false, true] {
            let mut source = Source::new([2, 0, 9]);
            if source_failure {
                source.fail = Some(VId(9));
            }
            let drops = source.drops.clone();
            let reads = source.reads.clone();
            let mut cursor =
                VertexScanCursor::new(source, plan.clone(), wide(), || Ok::<_, usize>(()));
            let first = pull_next(&mut cursor, &pull).unwrap().unwrap();
            assert_eq!(
                first.values()[1],
                GraphValue::Scalar(CanonicalScalar::Int(5))
            );
            let error = pull_next(&mut cursor, &pull).unwrap().unwrap_err();
            if source_failure {
                assert!(matches!(
                    error,
                    Failure::Scan(GqlQueryError::Source(VertexScanError::Source(
                        "late source failure"
                    )))
                ));
            } else {
                assert!(matches!(
                    error,
                    Failure::Projection { row: 1, column: 1, error }
                        if error.kind == GraphIntegerErrorKind::DivisionByZero
                ));
            }
            assert_eq!(cursor.row_stats().snapshot_records, 2);
            assert_eq!(cursor.row_stats().result_rows, 1);
            assert_eq!(cursor.state(), VertexScanState::Failed);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(pull_next(&mut cursor, &pull).is_none());
            assert!(cursor.next().is_none());
            assert_eq!(reads.load(Ordering::SeqCst), 2);
        }
        let source = Source::new([2, 5, 9]);
        let reads = source.reads.clone();
        let drops = source.drops.clone();
        let mut cursor = VertexScanCursor::new(source, plan, wide(), || -> Result<(), usize> {
            panic!("close must not drive the source or projection");
        });
        cursor.close();
        cursor.close();
        assert!(pull_next(&mut cursor, &pull).is_none());
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
