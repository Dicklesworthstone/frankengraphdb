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

    /// Transparent grouping only. A scope carrying a relational order/page
    /// cannot be peeled away. Projections, filters, UNWIND, singleton values and
    /// cross joins have no structural fallback through these accessors.
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
}
