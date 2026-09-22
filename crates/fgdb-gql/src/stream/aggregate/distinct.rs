//! Governed DISTINCT membership feeding exact numeric and collection cells.
//!
//! The source's row borrow ends on the next pull. Store a canonical value only
//! on its first occurrence, after admission, never a projected input bag or a
//! digest in place of equality. NULL contributes neither support nor a value.

use super::*;
use std::collections::BTreeSet;

pub(crate) struct DistinctState {
    // Only plain cells are constructed here, never a nested DISTINCT.
    pub(super) accumulator: NumericState,
    pub(super) scalars: BTreeSet<CanonicalScalar>,
    pub(super) vertices: BTreeSet<VId>,
    pub(super) values: BTreeSet<GraphValue>,
    pub(super) max_payload: usize,
}
impl DistinctState {
    pub(super) fn new(function: GraphAggregateFunction) -> Self {
        let accumulator = match function {
            GraphAggregateFunction::CountDistinct => NumericState::Count(0),
            GraphAggregateFunction::SumIntDistinct => NumericState::Sum(None),
            GraphAggregateFunction::AverageIntDistinct => {
                NumericState::Average { sum: 0, count: 0 }
            }
            GraphAggregateFunction::CollectDistinct => NumericState::Collect(Vec::new()),
            _ => unreachable!("the physical compiler checked the DISTINCT function"),
        };
        Self {
            accumulator,
            scalars: BTreeSet::new(),
            vertices: BTreeSet::new(),
            values: BTreeSet::new(),
            max_payload: 0,
        }
    }

    pub(super) fn update<E, C>(
        &mut self,
        input: Input<'_>,
        aggregate: usize,
        control: &mut impl FnMut(VertexScanEvent) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>>,
    ) -> Result<(), GqlQueryError<GraphAggregateError<E>, C>> {
        let input = input.normalized();
        if matches!(input, Input::Scalar(None | Some(CanonicalScalar::Null))) {
            return Ok(());
        }
        let units = input.payload_units();
        let largest = self.max_payload.max(units);
        let size = match input {
            Input::Vertex(_) => self.vertices.len(),
            Input::Scalar(_) => self.scalars.len(),
            Input::Value(_) => self.values.len(),
            Input::Identity => unreachable!("DISTINCT has a checked argument column"),
        };
        // Deterministic logical reservation for lookup and insertion, including
        // variable scalar payloads. This is not an assertion about the exact
        // comparison count or allocator-byte cost of std::BTreeSet.
        let levels = size.saturating_add(1).ilog2() as usize + 1;
        for _ in 0..levels
            .saturating_mul(24)
            .saturating_mul(largest.saturating_add(1))
        {
            control(VertexScanEvent::Work)?;
        }
        let present = match input {
            Input::Vertex(vid) => self.vertices.contains(&vid),
            Input::Scalar(Some(value)) => self.scalars.contains(value),
            Input::Value(value) => self.values.contains(value),
            _ => unreachable!("the nonnull argument was checked"),
        };
        if present {
            return Ok(());
        }
        // Membership reservation precedes contribution. Numeric cells check
        // errors before mutation; collection cells additionally govern their
        // separately owned list entry before appending it. No fallible work
        // follows the contribution, so a refusal leaves both sides unchanged.
        control(VertexScanEvent::ScratchEntry)?;
        for _ in 0..units {
            control(VertexScanEvent::ScratchEntry)?;
        }
        if let NumericState::Collect(values) = &mut self.accumulator {
            collection::push(values, input, control)?;
        } else {
            // Keep the existing numeric path's event sequence unchanged.
            self.accumulator.update(input, aggregate)?;
        }
        match input {
            Input::Vertex(vid) => {
                self.vertices.insert(vid);
            }
            Input::Scalar(Some(value)) => {
                self.scalars.insert(value.clone());
            }
            Input::Value(value) => {
                self.values.insert(value.clone());
            }
            _ => unreachable!("the nonnull argument was checked"),
        }
        self.max_payload = largest;
        Ok(())
    }

    /// Drop owned membership and move the result cell to ordinary finalization.
    /// SUM/AVG retain exactly the same empty, overflow and fraction semantics.
    pub(super) fn into_numeric(self) -> NumericState {
        self.accumulator
    }
}
