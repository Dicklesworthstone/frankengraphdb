//! Prove which sequence boundaries can be represented by ranked row bags.
//! This follows the ordinary relational executor's ordering contract; it does
//! not inspect data, evaluate expressions, or assume all bags are canonical.

use super::*;

impl PreparedGraphSet {
    /// The completed result's row comparator and a proven occurrence upper
    /// bound, when its sequence is expressible by a comparator on final cells.
    /// Empty keys mean canonical whole-row order. None means unknown sequence
    /// order, NOT canonical order: UNWIND, products and their order-preserving
    /// projections may enumerate differently. Explicit ORDER BY overrides that
    /// enumeration, with the same whole-row tie break as the snapshot executor.
    ///
    /// Scope and filter preserve ordering; ordinary canonicalizing projections
    /// and set operations reset it. A finite page gives an upper bound, filters
    /// cannot increase it, and offsets reduce it without arithmetic overflow.
    /// No claim that a graph source or descendant has a derivative is implied.
    pub fn incremental_result_order(&self) -> Option<(&[GraphValueOrder], Option<u64>)> {
        let inherited = self.incremental_node_order();
        let order = if self.order.is_empty() { inherited?.0 } else { &self.order };
        let remaining = inherited.and_then(|(_, bound)| bound)
            .map(|bound| bound.saturating_sub(self.offset));
        let bound = match (remaining, self.count) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        Some((order, bound))
    }

    /// Split exactly this node's ordered/page stage from its full definition.
    /// Nested pages, filters, DISTINCT and computed expressions remain in the
    /// cloned input. An explicit finite LIMIT works with explicit ORDER BY or
    /// a proven inherited/canonical order. OFFSET or ORDER BY without a LIMIT
    /// also works when the completed input already has a finite upper bound;
    /// no u64::MAX stand-in for an unbounded relation is manufactured.
    ///
    /// Unknown inherited enumeration refuses, including LIMIT 0. Callers must
    /// still compile the entire returned input: an empty final page must never
    /// bypass unsupported operators, expression failures or source admission.
    pub(super) fn split_incremental_window(&self) -> Option<(Self, &[GraphValueOrder], u64, u64)> {
        if self.unadorned_incremental_node() { return None; }
        let (order, bound) = self.incremental_result_order()?;
        let count = self.count.or(bound)?;
        let mut input = self.clone();
        input.order.clear();
        input.offset = 0;
        input.count = None;
        Some((input, order, self.offset, count))
    }

    fn incremental_node_order(&self) -> Option<(&[GraphValueOrder], Option<u64>)> {
        match &self.node {
            // A set pattern wrapper sorts the already-selected pattern bag,
            // even if the pattern's own output used a different ordering.
            SetNode::Pattern(_) => Some((&[], None)),
            SetNode::Binary { operation, left, right, .. } => {
                let left = left.incremental_result_order().and_then(|(_, bound)| bound);
                let right = right.incremental_result_order().and_then(|(_, bound)| bound);
                let bound = match operation {
                    GraphSetOperation::Union => left.zip(right).and_then(|(a, b)| a.checked_add(b)),
                    GraphSetOperation::Intersect => match (left, right) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    },
                    GraphSetOperation::Except => left,
                };
                Some((&[], bound))
            }
            SetNode::Values => Some((&[], Some(1))),
            SetNode::Scope(input) | SetNode::Filter { input, .. } =>
                input.incremental_result_order(),
            SetNode::Project { input, quantifier, .. }
                if *quantifier == GraphSetQuantifier::Distinct || !input.preserves_row_order() => {
                let bound = input.incremental_result_order().and_then(|(_, bound)| bound);
                Some((&[], bound))
            }
            // Reordering or collapsing output cells can destroy the relation
            // between their comparator and the original occurrence sequence.
            SetNode::Project { .. } | SetNode::Unwind { .. } | SetNode::CrossJoin { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests;
