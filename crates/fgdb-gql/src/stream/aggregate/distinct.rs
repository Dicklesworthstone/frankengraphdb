//! Governed DISTINCT membership feeding the existing exact numeric cells.
//!
//! The source's row borrow ends on the next pull. Store a canonical value only
//! on its first occurrence, after admission, never a projected input bag or a
//! digest in place of equality. NULL contributes neither support nor a value.

use super::*;
use std::collections::BTreeSet;

pub(crate) struct DistinctState {
    // Only Count, Sum and Average are constructed here, never a nested DISTINCT.
    pub(super) accumulator: NumericState,
    pub(super) scalars: BTreeSet<CanonicalScalar>,
    pub(super) vertices: BTreeSet<VId>,
    pub(super) max_payload: usize,
}
impl DistinctState {
    pub(super) fn new(function: GraphAggregateFunction) -> Self {
        let accumulator = match function {
            GraphAggregateFunction::CountDistinct => NumericState::Count(0),
            GraphAggregateFunction::SumIntDistinct => NumericState::Sum(None),
            GraphAggregateFunction::AverageIntDistinct => NumericState::Average { sum: 0, count: 0 },
            _ => unreachable!("the physical compiler checked the DISTINCT function"),
        };
        Self {
            accumulator,
            scalars: BTreeSet::new(),
            vertices: BTreeSet::new(),
            max_payload: 0,
        }
    }

    pub(super) fn update<E, C>(
        &mut self,
        input: Input<'_>,
        aggregate: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), VertexAggregateError<E, C>>,
    ) -> Result<(), VertexAggregateError<E, C>> {
        if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
            return Ok(());
        }
        let units = input.payload_units();
        let largest = self.max_payload.max(units);
        let size = match input {
            Input::Vertex(_) => self.vertices.len(),
            Input::Scalar(_) => self.scalars.len(),
            Input::Identity => unreachable!("DISTINCT has a checked argument column"),
        };
        // Deterministic logical reservation for lookup and insertion, including
        // variable scalar payloads. This is not an assertion about the exact
        // comparison count or allocator-byte cost of std::BTreeSet.
        let levels = size.saturating_add(1).ilog2() as usize + 1;
        for _ in 0..levels.saturating_mul(24).saturating_mul(largest.saturating_add(1)) {
            control(VertexScanEvent::Work)?;
        }
        let present = match input {
            Input::Vertex(vid) => self.vertices.contains(&vid),
            Input::Scalar(Some(value)) => self.scalars.contains(value),
            _ => unreachable!("the nonnull argument was checked"),
        };
        if present {
            return Ok(());
        }
        // All callbacks precede mutation and payload cloning. The existing
        // numeric update checks overflow/domain failures before changing its
        // fields; an error cannot leave a witness without its contribution.
        control(VertexScanEvent::ScratchEntry)?;
        for _ in 0..units {
            control(VertexScanEvent::ScratchEntry)?;
        }
        self.accumulator.update(input, aggregate)?;
        match input {
            Input::Vertex(vid) => { self.vertices.insert(vid); }
            Input::Scalar(Some(value)) => { self.scalars.insert(value.clone()); }
            _ => unreachable!("the nonnull argument was checked"),
        }
        self.max_payload = largest;
        Ok(())
    }

    /// Drop owned membership and move the numeric cell to ordinary finalization.
    /// SUM/AVG retain exactly the same empty, overflow and fraction semantics.
    pub(super) fn into_numeric(self) -> NumericState {
        self.accumulator
    }
}
