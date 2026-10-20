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

impl RowJoinSpec {
    /// Share the resident selected-product necessity proof with the maintained
    /// row adapter. Positions refer to the original validated left/right input,
    /// not its projected output. These are candidate-probe hints, NOT explicit
    /// typed keys or a replacement ON. The row adapter must use predicate
    /// equivalence for these hints and still run the complete native predicate.
    pub(crate) fn predicate_probe_keys_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<(usize, usize)>, E> {
        match self.predicate() {
            Some(code) => required_keys(self.left_types().len(), code, None, control),
            None => Ok(Vec::new()),
        }
    }
}

impl PreparedGraphSet {
    /// Lower an unpaged WHERE-over-product bag for incremental maintenance.
    /// Reuses the snapshot probe's necessary-equality proof and the ordinary
    /// eager predicate. Both child queries remain intact; an empty child is
    /// never permission to omit the other child's admission or failures.
    ///
    /// Only same-domain Vertex/Edge equalities become typed arrangement keys.
    /// Scalar and dynamic Any equalities remain in ON: Scalar admits Int/Float
    /// pairs whose WHERE equality differs from RowJoinSpec's typed identity.
    /// The row adapter may privately index necessary ON equalities using their
    /// predicate equivalence, while these explicit keys retain typed identity.
    /// An equality under NOT or only one OR arm is never an arrangement key.
    /// No authorization or source support is inferred here.
    ///
    /// One column-only ALL projection and transparent scopes may be crossed.
    /// The complete predicate is rebound to original left-then-right columns;
    /// the returned output stage restores reordered/repeated output cells and
    /// aliases. This node's order/page, and any intervening order/page,
    /// DISTINCT, second projection or computed expression, refuse. The owner
    /// must admit those boundaries separately, not drop them. This supplies a
    /// bag only: it does not establish a comparator for positional pagination.
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
        let slots = columns(projection, control)?;
        let mut keys = required_keys(left.types.len(), code, slots.as_deref(), control)?;
        // Necessary WHERE equality does not imply canonical typed equality:
        // Int(7) = Float(7) can be TRUE while their arrangement keys differ.
        // Keep such comparisons in ON instead of discarding candidates before
        // the native matcher sees them. Vertex/Edge identity is safe to index;
        // the declared domains, not currently observed rows, prove that fact.
        keys.retain(|&(a, b)| {
            let l = left.types[a];
            l == right.types[b]
                && matches!(l, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
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
        let mut rebound = Vec::new();
        for op in code {
            control(GlaExecutionEvent::Work)?;
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut op = op.clone();
            if let Some(slots) = slots.as_deref() {
                let remap = |operand: &mut GraphSetOperand| {
                    if let GraphSetOperand::Column(column) = operand {
                        *column = slots[*column];
                    }
                };
                match &mut op {
                    GraphSetPredicateOp::Compare { left, right, .. } => {
                        remap(left);
                        remap(right);
                    }
                    GraphSetPredicateOp::IsNull { operand, .. } => remap(operand),
                    _ => {}
                }
            }
            rebound.push(op);
        }
        let Ok(spec) = spec.with_predicate(&rebound) else {
            return Ok(None);
        };
        for name in &self.columns {
            control(GlaExecutionEvent::Work)?;
            for _ in 0..=name
                .len()
                .div_ceil(crate::algebra::GRAPH_VALUE_PAYLOAD_UNIT_BYTES)
            {
                control(GlaExecutionEvent::ScratchEntry)?;
            }
        }
        let output = match projection {
            Some(projection) => RowProjectionSpec::new(
                spec.column_types().collect(),
                projection.to_vec(),
                GraphSetQuantifier::All,
            ),
            None => RowProjectionSpec::selection(
                spec.column_types().collect(),
                self.columns.clone(),
                &[GraphSetPredicateOp::Truth(Some(true))],
            ),
        };
        let Ok(output) = output else { return Ok(None) };
        control(GlaExecutionEvent::Work)?;
        Ok(Some((left, right, spec, output)))
    }
}

#[cfg(test)]
mod tests;
