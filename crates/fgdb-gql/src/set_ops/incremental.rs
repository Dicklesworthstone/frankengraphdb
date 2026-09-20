//! Read-only access to the bound subset implemented by standing set circuits.
//! There is no parser tree, evaluator or alternative column-domain validator.

use super::*;

impl PreparedGraphSet {
    fn unadorned_incremental_node(&self) -> bool {
        self.order.is_empty() && self.offset == 0 && self.count.is_none()
    }

    /// Borrow a complete bound pattern leaf without erasing a relational page
    /// or order wrapped around it. The pattern's OWN DISTINCT/order/page remain
    /// part of the returned definition. A set wrapper canonicalizes those
    /// already-selected rows; a downstream set circuit consumes their bag.
    /// This shape accessor does not promise that every pattern has a derivative.
    pub fn incremental_pattern(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        if !self.unadorned_incremental_node() { return None; }
        match &self.node { SetNode::Pattern(pattern) => Some(pattern), _ => None }
    }

    /// Borrow the exact binary operation and quantifier with its bound children.
    /// An outer ORDER BY, OFFSET or LIMIT (including zero) makes this node
    /// unavailable, never silently ignored. Children must be admitted in turn;
    /// returning Some here does not authorize unsupported descendant operators.
    pub fn incremental_binary(&self)
        -> Option<(GraphSetOperation, GraphSetQuantifier, &Self, &Self)> {
        if !self.unadorned_incremental_node() { return None; }
        match &self.node {
            SetNode::Binary { operation, quantifier, left, right } =>
                Some((*operation, *quantifier, left, right)),
            _ => None,
        }
    }

    /// Borrow a checked computed projection without dropping its quantifier or
    /// the child's complete semantics. Wrapper ordering and every finite page
    /// refuse, including LIMIT 0. The host must still admit the child and the
    /// expression schema; this accessor never evaluates a row.
    pub fn incremental_projection(&self)
        -> Option<(&Self, &[GraphSetProjection], GraphSetQuantifier)> {
        if !self.unadorned_incremental_node() { return None; }
        match &self.node {
            SetNode::Project { input, projection, quantifier } =>
                Some((input, projection, *quantifier)),
            _ => None,
        }
    }

    /// Borrow both complete operands of an unconditional Cartesian product.
    /// Every child must still be admitted and initialized, even when its peer
    /// is empty. A wrapper order/page is never discarded, including LIMIT 0.
    pub fn incremental_cross_join(&self) -> Option<(&Self, &Self)> {
        if !self.unadorned_incremental_node() { return None; }
        match &self.node {
            SetNode::CrossJoin { left, right } => Some((left, right)),
            _ => None,
        }
    }

    /// Transparent grouping only. A scope carrying a relational order/page
    /// cannot be peeled away. A projection has its own exact accessor; filters,
    /// UNWIND, singleton values and cross joins have no transparent fallback.
    pub fn incremental_scope(&self) -> Option<&Self> {
        if !self.unadorned_incremental_node() { return None; }
        match &self.node { SetNode::Scope(input) => Some(input), _ => None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, PreparedGraphText};

    fn pattern() -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare("MATCH (n) RETURN n AS id LIMIT 2", |_, _: &str| None)
            .unwrap().bind_parameters(&GqlParameters::new()).unwrap()
    }
    fn unavailable(query: &PreparedGraphSet) {
        assert!(query.incremental_pattern().is_none());
        assert!(query.incremental_binary().is_none());
        assert!(query.incremental_scope().is_none());
    }

    #[test]
    fn admitted_nodes_preserve_exact_children_and_refuse_all_wrapper_pages() {
        let pattern = pattern();
        let leaf = PreparedGraphSet::from(pattern.clone());
        assert_eq!(leaf.incremental_pattern().unwrap().canonical_bytes(), pattern.canonical_bytes());
        let scope = leaf.clone().nested().unwrap();
        assert_eq!(scope.incremental_scope().unwrap().canonical_bytes(), leaf.canonical_bytes());
        for operation in [GraphSetOperation::Union, GraphSetOperation::Intersect, GraphSetOperation::Except] {
            for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let binary = leaf.clone().combine(operation, quantifier, scope.clone()).unwrap();
                let (op, q, left, right) = binary.incremental_binary().unwrap();
                assert_eq!((op, q), (operation, quantifier));
                assert_eq!(left.canonical_bytes(), leaf.canonical_bytes());
                assert_eq!(right.canonical_bytes(), scope.canonical_bytes());
                for node in [&leaf, &scope, &binary] {
                    for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
                        unavailable(&(*node).clone().with_page(offset, count));
                    }
                    let mut ordered = (*node).clone();
                    ordered.order.push(GraphValueOrder { column: 0, descending: true, nulls_first: false });
                    unavailable(&ordered);
                }
            }
        }
    }

    #[test]
    fn unsupported_relational_stages_do_not_masquerade_as_a_pattern_leaf() {
        let leaf = PreparedGraphSet::from(pattern());
        unavailable(&PreparedGraphSet::singleton());
        unavailable(&leaf.clone().cross_join(leaf.clone()).unwrap());
        let child = Box::new(leaf.clone());
        // Private construction isolates shape admission from projection syntax;
        // ordinary callers still use the checked public constructors.
        let mut projected = leaf;
        projected.node = SetNode::Project { input: child, projection: vec![], quantifier: GraphSetQuantifier::All };
        unavailable(&projected);
    }

    #[test]
    fn projection_accessor_preserves_child_expressions_and_quantifier_but_not_wrapper_pages() {
        let leaf = PreparedGraphSet::from(pattern());
        for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
            let projection = vec![GraphSetProjection::new("renamed", GraphSetValue::Column(0))];
            let projected = leaf.clone().project(projection.clone(), quantifier).unwrap();
            let (child, expressions, observed) = projected.incremental_projection().unwrap();
            assert_eq!(child.canonical_bytes(), leaf.canonical_bytes());
            assert_eq!(expressions, projection);
            assert_eq!(observed, quantifier);
            assert_eq!(projected.columns(), &["renamed"]);
            unavailable(&projected); // Not an unprojected leaf/binary/scope.
            for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
                assert!(projected.clone().with_page(offset, count).incremental_projection().is_none());
            }
            let ordered = projected.with_order_by(&[GraphValueOrder {
                column: 0, descending: true, nulls_first: false,
            }]).unwrap();
            assert!(ordered.incremental_projection().is_none());
        }
        assert!(leaf.incremental_projection().is_none());
    }

    #[test]
    fn cross_accessor_retains_both_complete_children_and_refuses_wrapper_order_and_pages() {
        let left = PreparedGraphSet::from(pattern());
        let right = left.clone().nested().unwrap();
        let query = left.clone().cross_join(right.clone()).unwrap();
        let (a, b) = query.incremental_cross_join().unwrap();
        assert_eq!(a.canonical_bytes(), left.canonical_bytes());
        assert_eq!(b.canonical_bytes(), right.canonical_bytes());
        for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
            assert!(query.clone().with_page(offset, count).incremental_cross_join().is_none());
        }
        let ordered = query.with_order_by(&[GraphValueOrder::descending(0)]).unwrap();
        assert!(ordered.incremental_cross_join().is_none());
        assert!(left.incremental_cross_join().is_none());
        assert!(PreparedGraphSet::singleton().incremental_cross_join().is_none());
    }
}
