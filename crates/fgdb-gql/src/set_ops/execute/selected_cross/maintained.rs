//! Compile a selected product into the existing maintained join and row sink.
//! This is bag lowering for the standing-circuit owner, not a rewrite of the
//! snapshot executor's left-major sequence or permission to erase a page.

use super::*;
use crate::row_join::RowJoinSpec;
use crate::row_projection::RowProjectionSpec;

/// Original complete children, a frozen join, and its original output schema.
/// A host must admit both children and publish these stages transactionally.
pub type MaintainedSelectedJoin<'a> = (
    &'a PreparedGraphSet,
    &'a PreparedGraphSet,
    RowJoinSpec,
    RowProjectionSpec,
);

impl PreparedGraphSet {
    /// Lower an unpaged WHERE-over-product bag for incremental maintenance.
    /// Reuses the snapshot probe's necessary-equality proof and the ordinary
    /// eager predicate. Both child queries remain intact; an empty child is
    /// never permission to omit the other child's admission or failures.
    ///
    /// Only same-domain Scalar/Vertex equalities become arrangement keys.
    /// Dynamic Any comparisons and all residual Boolean operations remain in
    /// ON. In particular an equality under NOT or only one OR arm is not an
    /// arrangement key. No authorization or source support is inferred here.
    ///
    /// This node's order/page, and any intervening order/page, DISTINCT,
    /// projection or computed expression, refuse. The circuit owner must
    /// handle such boundaries independently rather than dropping them. The
    /// returned output stage retains original names, including duplicates.
    ///
    /// The callback governs bounded definition work and retained metadata;
    /// no input rows are read. An error returns no prepared stages. None means
    /// this specialization cannot represent the complete shape, not that the
    /// original query is empty or should bypass its ordinary compiler.
    pub fn incremental_selected_join_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<MaintainedSelectedJoin<'_>>, E> {
        control(GlaExecutionEvent::Work)?;
        if !self.order.is_empty() || self.offset != 0 || self.count.is_some() {
            return Ok(None);
        }
        let Some((left, right, code, projection)) = self.filtered_cross_inputs() else {
            return Ok(None);
        };
        if projection.is_some() {
            return Ok(None);
        }
        let mut keys = required_keys(left.types.len(), code, None, control)?;
        // A necessary dynamic equality is still enforced by ON. It cannot
        // widen RowJoinSpec's declared key domains or change NULL semantics.
        keys.retain(|&(a, b)| {
            let l = left.types[a];
            l == right.types[b]
                && matches!(l, GraphSetColumnType::Scalar | GraphSetColumnType::Vertex)
        });
        for _ in 0..left.types.len() + right.types.len() + keys.len() {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let spec = if keys.is_empty() {
            RowJoinSpec::cross(&left.types, &right.types)
        } else {
            RowJoinSpec::new(&left.types, &right.types, &keys)
        };
        let Ok(spec) = spec else { return Ok(None) };
        for _ in code {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let Ok(spec) = spec.with_predicate(code) else { return Ok(None) };
        for name in &self.columns {
            control(GlaExecutionEvent::Work)?;
            for _ in 0..=name.len().div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }
        let output = RowProjectionSpec::selection(
            spec.column_types().collect(),
            self.columns.clone(),
            &[GraphSetPredicateOp::Truth(Some(true))],
        );
        let Ok(output) = output else { return Ok(None) };
        control(GlaExecutionEvent::Work)?;
        Ok(Some((left, right, spec, output)))
    }
}

#[cfg(test)]
mod tests;
