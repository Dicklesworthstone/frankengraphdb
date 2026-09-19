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
