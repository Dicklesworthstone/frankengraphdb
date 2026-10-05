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
pub trait VertexScanOutput: GlaOutput + sealed::Projection {}
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
    /// Ordinary compile(), and its ordered/unique-output proof, are unchanged.
    pub fn compile_sort_input(
        plan: &crate::algebra::GlaPlan<GraphValueRow>,
    ) -> Result<(Self, crate::scan_stream::ScanSortTail), super::VertexScanBuildError> {
        let tail = crate::scan_stream::ScanSortTail::compile(plan)
            .map_err(|operator| super::VertexScanBuildError { operator })?;
        // DISTINCT is intentionally deferred along with ordering. The row
        // collector emits every source occurrence; only the blocking consumer
        // may deduplicate complete rows before applying the retained window.
        let mut input = Self::compile_with_projection(plan, |projection, order| {
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

// Sealed: unnameable by design, so no other crate can implement it.
#[allow(unnameable_types)]
mod sealed {
    use super::*;

    pub trait Projection: Sized {
        fn accepts_projection(projection: &GlaOperator, order: &GlaOperator) -> bool;
        fn project<E>(
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
        fn project<E>(
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
        fn project<E>(
            vid: VId,
            row: VertexScanRow<'_>,
            projection: &GlaOperator,
            control: &mut impl FnMut(VertexScanEvent) -> Result<(), E>,
        ) -> Result<Self, E> {
            collect_one::<Self, E>(vid, row, projection, control)
        }
    }
}

fn collect_one<Row: GlaOutput, E>(
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
