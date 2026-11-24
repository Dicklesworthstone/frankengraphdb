//! Read-only access to the bound subset implemented by standing set circuits.
//! There is no parser tree, evaluator or alternative column-domain validator.

use super::*;

mod apply;
mod window;

pub use apply::{
    MaintainedApplyJoin, MaintainedCollectedCount, MaintainedCountJoin, MaintainedDistinctApply,
    MaintainedExistenceJoin, MaintainedFilteredAggregate, MaintainedWindowApply,
};

impl PreparedGraphSet {
    /// Retain a complete logical input through an already validated ordinary
    /// projection or unfiltered UNWIND. Only the specification supplies domains, including
    /// its same-domain NULL defaults; no caller can assert a narrower type.
    fn project_incremental_spec(
        mut self,
        spec: &crate::row_projection::RowProjectionSpec,
    ) -> Option<Self> {
        if self.types.as_slice() != spec.input_types() {
            return None;
        }
        if let Some((name, value)) = spec.unwind_value() {
            if !self
                .columns
                .iter()
                .map(String::as_str)
                .eq(spec.columns().take(self.types.len()))
            {
                return None;
            }
            let mut expanded = self.unwind(name.to_owned(), value).ok()?;
            expanded.types = spec.column_types().to_vec();
            return Some(expanded);
        }
        let (projection, filter) = spec.ordinary_parts()?;
        if !filter.is_empty() {
            self = self.filter(filter).ok()?;
        }
        let mut projected = self.project(projection.to_vec(), spec.quantifier()).ok()?;
        // General CASE inference widens to Any, while the immutable checked
        // specification may prove a copied value with a same-domain default.
        projected.types = spec.column_types().to_vec();
        Some(projected)
    }

    /// Peel exactly one finite relational window, preserving its entire input
    /// node. The ALL window runs after the node's own quantifier. A page without
    /// ORDER BY inherits a scope/filter child's selected order; pattern wrappers
    /// and product-free projections canonicalize rows as in the batch executor.
    /// Mixed product/UNWIND/window trees refuse: positional enumeration is not
    /// represented by the existing maintained stages' canonical bags.
    /// This is shape analysis, not derivative admission for any descendant.
    pub fn incremental_window(
        &self,
    ) -> Result<
        Option<(Self, crate::row_window::RowWindowSpec)>,
        crate::row_window::RowWindowBuildError,
    > {
        let Some(count) = self.count else {
            return Ok(None);
        };
        if !self.incremental_window_sequence_compatible() {
            return Ok(None);
        }
        let inherited = self
            .incremental_finite_order()
            .map_or(&[][..], |(order, _)| order);
        let order = if self.order.is_empty() {
            inherited.to_vec()
        } else {
            self.order.clone()
        };
        let spec = crate::row_window::RowWindowSpec::new(
            self.types.clone(),
            order,
            GraphSetQuantifier::All,
            self.offset,
            count,
        )?;
        let mut input = self.clone();
        input.order.clear();
        input.offset = 0;
        input.count = None;
        Ok(Some((input, spec)))
    }

    /// Noncanonical result order with a finite occurrence upper bound for the
    /// standing source/set/projection/filter subset. Scope and filter preserve
    /// order; a projection or binary set canonicalizes its output. Unsupported
    /// source shapes must still refuse in the host. No unbounded sort is admitted.
    pub fn incremental_finite_order(&self) -> Option<(&[GraphValueOrder], u64)> {
        if !self.order.is_empty() {
            return self.count.map(|count| (self.order.as_slice(), count));
        }
        match &self.node {
            SetNode::Scope(input) | SetNode::Filter { input, .. } => {
                let (order, bound) = input.incremental_finite_order()?;
                let bound = bound.saturating_sub(self.offset);
                Some((order, self.count.map_or(bound, |count| count.min(bound))))
            }
            _ => None,
        }
    }

    /// Conservative product-free profile used by incremental_window. Unwindowed
    /// circuits stay admitted under their canonical-bag contract. The ranked
    /// incremental_ordered_window accessor separately proves comparators and
    /// bounds at each scope, including explicitly ordered positional inputs.
    /// This finite structural walk examines no rows and clones no definitions.
    pub fn incremental_window_sequence_compatible(&self) -> bool {
        fn shape(query: &PreparedGraphSet, local_page: Option<&PreparedGraphSet>) -> (bool, bool) {
            let (nested_window, positional_source) = match &query.node {
                SetNode::Pattern(_) => (false, false),
                // Grouping completes and canonicalizes its output. Input
                // sequence requirements belong to the separately compiled
                // child; an outer window cannot erase that child admission.
                SetNode::Aggregate(_) => (false, false),
                // The executor admits and canonically sorts procedure rows
                // exactly like pattern rows. A page can preserve that order;
                // the host must still admit the complete CALL derivative.
                SetNode::ProcedureCall(_) => (false, false),
                SetNode::Owned { .. } => (true, true),
                // Arguments and Apply have positional enumeration, but an
                // unpaged invocation does not itself introduce a window.
                // Complete derivative admission still belongs to the host.
                SetNode::Arguments => (false, true),
                SetNode::Apply { left, right, .. } => {
                    let a = shape(left, None);
                    // A literal TRUE/LIMIT-one output observes only presence,
                    // never the identity or order of its selected witness.
                    // Ignore just that page; every real descendant window
                    // and the full existence derivative still need admission.
                    let b = if let Some(input) = apply::existence_input(query) {
                        shape(input, None)
                    } else {
                        // The maintained invocation selects its own bounded
                        // page before restoring outer occurrences. Its parent
                        // consumes that bag; no outer positional page is erased.
                        shape(right, apply::bounded_window_input(query))
                    };
                    (a.0 || b.0, true)
                }
                // An output projection keeps positional enumeration. Retain
                // every real order/page below it without inventing one for an
                // ordinary compiler-emitted, unpaged CALL output projection.
                SetNode::Project {
                    input,
                    preserve_order: true,
                    ..
                } => (shape(input, local_page).0, true),
                SetNode::Scope(input)
                | SetNode::Filter { input, .. }
                | SetNode::Project { input, .. } => shape(input, local_page),
                SetNode::Binary { left, right, .. } | SetNode::Join { left, right, .. } => {
                    let a = shape(left, local_page);
                    let b = shape(right, local_page);
                    (a.0 || b.0, a.1 || b.1)
                }
                SetNode::CrossJoin { left, right } => {
                    let a = shape(left, local_page);
                    let b = shape(right, local_page);
                    (a.0 || b.0, true)
                }
                SetNode::Unwind { input, .. } => (shape(input, local_page).0, true),
                SetNode::Values => (false, true),
            };
            (
                (!local_page.is_some_and(|page| core::ptr::eq(page, query))
                    && (query.count.is_some() || query.offset != 0 || !query.order.is_empty()))
                    || nested_window,
                positional_source,
            )
        }
        let (window, positional) = shape(self, None);
        !window || !positional
    }

    fn unadorned_incremental_node(&self) -> bool {
        self.order.is_empty() && self.offset == 0 && self.count.is_none()
    }

    /// Split a ranked finite window from its complete input definition. Only
    /// this node's order/page metadata is removed; child scopes, projections,
    /// filters and quantifiers stay untouched. The comparator can be explicit
    /// or inherited through scopes/filters. The finite occurrence bound can be
    /// an explicit LIMIT or follow from already-bounded upstream operators.
    ///
    /// Canonical-only pages and unknown implicit enumeration are not recognized
    /// by this ranked specialization. Unbounded sorting also refuses. Callers
    /// must admit the full returned input independently, including LIMIT 0;
    /// proving a comparator never authorizes unsupported descendants.
    pub fn incremental_ordered_window(&self) -> Option<(Self, &[GraphValueOrder], u64, u64)> {
        if self.incremental_result_order()?.0.is_empty() {
            return None;
        }
        self.split_incremental_window()
    }

    /// Borrow one intermediate group over its COMPLETE relational input.
    /// The host must independently admit that input and the native group
    /// kernel, then retain the checked ordinary-row derivative. This accessor
    /// never exposes an executable first-leaf substitute. Every outer order
    /// or page, including LIMIT zero, must be peeled by its own window owner.
    pub fn incremental_group(&self) -> Option<&PreparedGraphSetAggregate> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Aggregate(summary) => Some(summary),
            _ => None,
        }
    }

    /// Borrow a complete bound pattern leaf without erasing a relational page
    /// or order wrapped around it. The pattern's OWN DISTINCT/order/page remain
    /// part of the returned definition. A set wrapper canonicalizes those
    /// already-selected rows; a downstream set circuit consumes their bag.
    /// This shape accessor does not promise that every pattern has a derivative.
    pub fn incremental_pattern(&self) -> Option<&PreparedGraphPattern<GraphValueRow>> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Pattern(pattern) => Some(pattern),
            _ => None,
        }
    }

    /// Borrow a procedure leaf with no runtime arguments, preserving its
    /// catalog-bound graph projection and complete YIELD metadata. Hosts must
    /// still validate the procedure, graph laws and derivative independently.
    /// Nonempty argument programs, including named envelopes, are not erased;
    /// they remain unsupported by this zero-argument accessor. An outer order
    /// or page must be peeled by its own owner, including LIMIT zero.
    pub fn incremental_procedure(&self) -> Option<&PreparedProcedureCall> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::ProcedureCall(call) if call.arguments.is_empty() => Some(call),
            _ => None,
        }
    }

    /// Borrow the exact binary operation and quantifier with its bound children.
    /// An outer ORDER BY, OFFSET or LIMIT (including zero) makes this node
    /// unavailable, never silently ignored. Children must be admitted in turn;
    /// returning Some here does not authorize unsupported descendant operators.
    pub fn incremental_binary(
        &self,
    ) -> Option<(GraphSetOperation, GraphSetQuantifier, &Self, &Self)> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Binary {
                operation,
                quantifier,
                left,
                right,
            } => Some((*operation, *quantifier, left, right)),
            _ => None,
        }
    }

    /// Borrow a checked computed projection without dropping its quantifier or
    /// the child's complete semantics. Wrapper ordering and every finite page
    /// refuse, including LIMIT 0. The host must still admit the child and the
    /// expression schema; this accessor never evaluates a row.
    pub fn incremental_projection(
        &self,
    ) -> Option<(&Self, &[GraphSetProjection], GraphSetQuantifier)> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Project {
                input,
                projection,
                quantifier,
                ..
            } => Some((input, projection, *quantifier)),
            _ => None,
        }
    }

    /// Capture stored vertex properties in their maintained source before a
    /// nonempty literal UNWIND. Each expanded copy then carries the value from
    /// the same commit as its vertex. The final projection restores the public
    /// slots and quantifier. Dynamic or empty expansions keep their ordinary
    /// admission path, where a property read may never be reached.
    pub fn incremental_captured_unwind_projection<E>(
        &self,
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<(Self, crate::row_projection::RowProjectionSpec)>, E> {
        use crate::GlaExecutionEvent::{ScratchEntry, Work};
        let Some((expanded, outputs, quantifier)) = self.incremental_projection() else {
            return Ok(None);
        };
        let Some((source, alias, list @ GraphSetValue::List(items))) =
            expanded.incremental_unwind()
        else {
            return Ok(None);
        };
        if items.is_empty() {
            return Ok(None);
        }
        for item in items {
            control(Work)?;
            if !matches!(item, GraphSetValue::Literal(_)) {
                return Ok(None);
            }
        }
        let width = source.columns.len();
        let mut fields = Vec::new();
        for output in outputs {
            control(Work)?;
            match output.value() {
                GraphSetValue::Column(_) => {}
                GraphSetValue::MapGet {
                    map,
                    property: GraphElementKey::Property(key),
                    ..
                } => {
                    let GraphSetValue::Column(column) = map.as_ref() else {
                        return Ok(None);
                    };
                    if source.types.get(*column) != Some(&GraphSetColumnType::Vertex)
                        || output.name() == alias
                    {
                        return Ok(None);
                    }
                    control(ScratchEntry)?;
                    fields.push((*column, *key, output.name()));
                }
                _ => return Ok(None),
            }
        }
        if fields.is_empty() || width + fields.len() + 1 > crate::algebra::MAX_PATTERN_VERTICES {
            return Ok(None);
        }
        let Some(captured) = source.with_vertex_fields(&fields, control)? else {
            return Ok(None);
        };
        let Ok(input) = captured.unwind(alias.to_owned(), list.clone()) else {
            return Ok(None);
        };
        let mut next_field = width;
        let mut projected = Vec::with_capacity(outputs.len());
        for output in outputs {
            control(ScratchEntry)?;
            let column = match output.value() {
                GraphSetValue::Column(column) if *column == width => width + fields.len(),
                GraphSetValue::Column(column) => *column,
                _ => {
                    next_field += 1;
                    next_field - 1
                }
            };
            projected.push(GraphSetProjection::new(
                output.name(),
                GraphSetValue::Column(column),
            ));
        }
        Ok(crate::row_projection::RowProjectionSpec::new(
            input.types.clone(),
            projected,
            quantifier,
        )
        .ok()
        .map(|spec| (input, spec)))
    }

    fn with_vertex_fields<E>(
        &self,
        fields: &[(usize, fgdb_delta_types::PropertyKeyId, &str)],
        control: &mut impl FnMut(crate::GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<Self>, E> {
        use crate::GlaExecutionEvent::{ScratchEntry, Work};
        if !self.unadorned_incremental_node()
            || self.columns.len() + fields.len() > crate::algebra::MAX_PATTERN_VERTICES
        {
            return Ok(None);
        }
        for &(_, _, name) in fields {
            for existing in &self.columns {
                control(Work)?;
                if existing == name {
                    return Ok(None);
                }
            }
        }
        match &self.node {
            SetNode::Pattern(pattern) => {
                Ok(pattern.with_scanned_vertex_fields(fields).map(Into::into))
            }
            SetNode::Scope(input) => Ok(input
                .with_vertex_fields(fields, control)?
                .and_then(|input| input.nested().ok())),
            SetNode::Project {
                input,
                projection,
                quantifier: GraphSetQuantifier::All,
                preserve_order,
            } => {
                for p in projection {
                    control(Work)?;
                    if !matches!(p.value(), GraphSetValue::Column(_)) {
                        return Ok(None);
                    }
                }
                let mut inner_fields = Vec::with_capacity(fields.len());
                for &(col, key, name) in fields {
                    control(ScratchEntry)?;
                    let Some(GraphSetValue::Column(slot)) =
                        projection.get(col).map(GraphSetProjection::value)
                    else {
                        return Ok(None);
                    };
                    inner_fields.push((*slot, key, name));
                }
                let width = input.columns.len();
                let Some(child) = input.with_vertex_fields(&inner_fields, control)? else {
                    return Ok(None);
                };
                let mut columns = projection.clone();
                for (i, &(_, _, name)) in fields.iter().enumerate() {
                    control(ScratchEntry)?;
                    columns.push(GraphSetProjection::new(
                        name,
                        GraphSetValue::Column(width + i),
                    ));
                }
                Ok(if *preserve_order {
                    child.project_ordered_output(columns)
                } else {
                    child.project(columns, GraphSetQuantifier::All)
                }
                .ok())
            }
            _ => Ok(None),
        }
    }

    /// Borrow both complete operands of an unconditional Cartesian product.
    /// Every child must still be admitted and initialized, even when its peer
    /// is empty. A wrapper order/page is never discarded, including LIMIT 0.
    pub fn incremental_cross_join(&self) -> Option<(&Self, &Self)> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::CrossJoin { left, right } => Some((left, right)),
            _ => None,
        }
    }

    /// Borrow a checked join of complete relations, not their first leaves.
    /// The frozen definition includes both schemas, kind, keys and ON program.
    /// Ordering/pages at this scope (including LIMIT 0) must be peeled by their
    /// own admission path; they are never discarded by this accessor.
    pub fn incremental_join(&self) -> Option<(&Self, &Self, &crate::row_join::RowJoinSpec)> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Join { left, right, spec } => Some((left, right, spec)),
            _ => None,
        }
    }

    /// Borrow a list-expansion stage with its original input and appended alias.
    /// No element is evaluated here. Every wrapper order/page still refuses;
    /// the caller must admit the complete child and native expression schema.
    pub fn incremental_unwind(&self) -> Option<(&Self, &str, &GraphSetValue)> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Unwind { input, value } => Some((input, self.columns.last()?.as_str(), value)),
            _ => None,
        }
    }

    /// Transparent grouping only. A scope carrying a relational order/page
    /// cannot be peeled away. A projection has its own exact accessor; filters,
    /// UNWIND, singleton values and cross joins have no transparent fallback.
    pub fn incremental_scope(&self) -> Option<&Self> {
        if !self.unadorned_incremental_node() {
            return None;
        }
        match &self.node {
            SetNode::Scope(input) => Some(input),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlParameters, PreparedGraphText};

    fn pattern() -> PreparedGraphPattern<GraphValueRow> {
        PreparedGraphText::prepare("MATCH (n) RETURN n AS id LIMIT 2", |_, _: &str| None)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
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
        assert_eq!(
            leaf.incremental_pattern().unwrap().canonical_bytes(),
            pattern.canonical_bytes()
        );
        let scope = leaf.clone().nested().unwrap();
        assert_eq!(
            scope.incremental_scope().unwrap().canonical_bytes(),
            leaf.canonical_bytes()
        );
        for operation in [
            GraphSetOperation::Union,
            GraphSetOperation::Intersect,
            GraphSetOperation::Except,
        ] {
            for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let binary = leaf
                    .clone()
                    .combine(operation, quantifier, scope.clone())
                    .unwrap();
                let (op, q, left, right) = binary.incremental_binary().unwrap();
                assert_eq!((op, q), (operation, quantifier));
                assert_eq!(left.canonical_bytes(), leaf.canonical_bytes());
                assert_eq!(right.canonical_bytes(), scope.canonical_bytes());
                for node in [&leaf, &scope, &binary] {
                    for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
                        unavailable(&(*node).clone().with_page(offset, count));
                    }
                    let mut ordered = (*node).clone();
                    ordered.order.push(GraphValueOrder {
                        column: 0,
                        descending: true,
                        nulls_first: false,
                    });
                    unavailable(&ordered);
                }
            }
        }
    }

    #[test]
    fn finite_windows_preserve_exact_input_and_inherited_filter_scope_order() {
        let leaf = PreparedGraphSet::from(pattern());
        let order = vec![GraphValueOrder {
            column: 0,
            descending: true,
            nulls_first: false,
        }];
        let first = leaf
            .clone()
            .with_order_by(&order)
            .unwrap()
            .with_page(0, Some(7));
        let frozen = first.canonical_bytes();
        let (input, spec) = first.incremental_window().unwrap().unwrap();
        assert_eq!(input.canonical_bytes(), leaf.canonical_bytes());
        assert_eq!(spec.order(), order);
        assert_eq!(spec.count(), 7);
        assert_eq!(spec.quantifier(), GraphSetQuantifier::All);
        let filtered = first
            .clone()
            .filter(&[GraphSetPredicateOp::Truth(Some(true))])
            .unwrap();
        assert_eq!(
            filtered.incremental_finite_order(),
            Some((order.as_slice(), 7))
        );
        let second = filtered.clone().nested().unwrap().with_page(1, Some(2));
        let (input, spec) = second.incremental_window().unwrap().unwrap();
        assert_eq!(
            input.canonical_bytes(),
            filtered.clone().nested().unwrap().canonical_bytes()
        );
        assert_eq!(spec.order(), order);
        assert_eq!((spec.offset(), spec.count()), (1, 2));
        let projected = filtered
            .project(
                vec![GraphSetProjection::new("id", GraphSetValue::Column(0))],
                GraphSetQuantifier::All,
            )
            .unwrap();
        assert!(projected.incremental_finite_order().is_none());
        assert!(
            leaf.clone()
                .with_order_by(&order)
                .unwrap()
                .incremental_window()
                .unwrap()
                .is_none()
        );
        assert_eq!(first.canonical_bytes(), frozen);
        assert_eq!(
            leaf.clone()
                .with_page(0, Some(0))
                .incremental_window()
                .unwrap()
                .unwrap()
                .1
                .count(),
            0
        );
        let product = leaf.clone().cross_join(leaf.clone()).unwrap();
        assert!(product.incremental_window_sequence_compatible());
        assert!(
            !product
                .clone()
                .with_page(0, Some(0))
                .incremental_window_sequence_compatible()
        );
        assert!(
            product
                .with_page(0, Some(2))
                .incremental_window()
                .unwrap()
                .is_none()
        );
        assert!(
            !leaf
                .clone()
                .cross_join(leaf.clone().with_page(0, Some(1)))
                .unwrap()
                .incremental_window_sequence_compatible()
        );
        let expanded = leaf
            .unwind(
                "item".into(),
                GraphSetValue::List(vec![GraphSetValue::Column(0)]),
            )
            .unwrap();
        assert!(expanded.incremental_window_sequence_compatible());
        assert!(
            !expanded
                .with_page(0, Some(0))
                .incremental_window_sequence_compatible()
        );
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
        projected.node = SetNode::Project {
            input: child,
            projection: vec![],
            quantifier: GraphSetQuantifier::All,
            preserve_order: false,
        };
        unavailable(&projected);
    }

    #[test]
    fn projection_accessor_preserves_child_expressions_and_quantifier_but_not_wrapper_pages() {
        let leaf = PreparedGraphSet::from(pattern());
        for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
            let projection = vec![GraphSetProjection::new("renamed", GraphSetValue::Column(0))];
            let projected = leaf
                .clone()
                .project(projection.clone(), quantifier)
                .unwrap();
            let (child, expressions, observed) = projected.incremental_projection().unwrap();
            assert_eq!(child.canonical_bytes(), leaf.canonical_bytes());
            assert_eq!(expressions, projection);
            assert_eq!(observed, quantifier);
            assert_eq!(projected.columns(), &["renamed"]);
            unavailable(&projected); // Not an unprojected leaf/binary/scope.
            for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
                assert!(
                    projected
                        .clone()
                        .with_page(offset, count)
                        .incremental_projection()
                        .is_none()
                );
            }
            let ordered = projected
                .with_order_by(&[GraphValueOrder {
                    column: 0,
                    descending: true,
                    nulls_first: false,
                }])
                .unwrap();
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
            assert!(
                query
                    .clone()
                    .with_page(offset, count)
                    .incremental_cross_join()
                    .is_none()
            );
        }
        let ordered = query
            .with_order_by(&[GraphValueOrder::descending(0)])
            .unwrap();
        assert!(ordered.incremental_cross_join().is_none());
        assert!(left.incremental_cross_join().is_none());
        assert!(
            PreparedGraphSet::singleton()
                .incremental_cross_join()
                .is_none()
        );
    }

    #[test]
    fn maintained_vertex_fields_cross_only_nonempty_literal_expansions() {
        use fgdb_delta_types::PropertyKeyId;
        use fgdb_types::CanonicalScalar;
        let source = PreparedGraphSet::from(
            PreparedGraphText::prepare("MATCH (a) RETURN a", |_, _: &str| None)
                .unwrap()
                .bind_parameters(&GqlParameters::new())
                .unwrap(),
        );
        let field = GraphSetValue::MapGet {
            map: Box::new(GraphSetValue::Column(0)),
            key: "score".into(),
            property: GraphElementKey::Property(PropertyKeyId(5)),
        };
        let after = |items| {
            source
                .clone()
                .unwind("token".into(), GraphSetValue::List(items))
                .unwrap()
                .project(
                    vec![
                        GraphSetProjection::new("token", GraphSetValue::Column(1)),
                        GraphSetProjection::new("score", field.clone()),
                        GraphSetProjection::new("owner", GraphSetValue::Column(0)),
                    ],
                    GraphSetQuantifier::Distinct,
                )
                .unwrap()
        };
        let seven = crate::GqlScalarParameter::new(CanonicalScalar::Int(7)).unwrap();
        let query = after(vec![GraphSetValue::Literal(seven); 2]);
        let (expanded, spec) = query
            .incremental_captured_unwind_projection(&mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(spec.quantifier(), GraphSetQuantifier::Distinct);
        assert_eq!(spec.column_types(), query.column_types());
        assert_eq!(
            spec.columns().collect::<Vec<_>>(),
            ["token", "score", "owner"]
        );
        assert_eq!(
            expanded.column_types(),
            &[
                GraphSetColumnType::Vertex,
                GraphSetColumnType::Stored,
                GraphSetColumnType::Any
            ]
        );
        assert_eq!(
            query
                .incremental_captured_unwind_projection(&mut |_| Err("stop"))
                .unwrap_err(),
            "stop"
        );
        for items in [vec![], vec![GraphSetValue::Column(0)]] {
            assert!(
                after(items)
                    .incremental_captured_unwind_projection(&mut |_| Ok::<_, ()>(()))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn unwind_accessor_keeps_original_input_expression_alias_and_wrapper_refusals() {
        let leaf = PreparedGraphSet::from(pattern());
        let value = GraphSetValue::List(vec![GraphSetValue::Column(0), GraphSetValue::Column(0)]);
        let query = leaf
            .clone()
            .unwind("element".into(), value.clone())
            .unwrap();
        let (input, alias, expression) = query.incremental_unwind().unwrap();
        assert_eq!(input.canonical_bytes(), leaf.canonical_bytes());
        assert_eq!(alias, "element");
        assert_eq!(expression, &value);
        assert_eq!(
            query.column_types(),
            &[GraphSetColumnType::Vertex, GraphSetColumnType::Any]
        );
        for (offset, count) in [(1, None), (0, Some(0)), (0, Some(2))] {
            assert!(
                query
                    .clone()
                    .with_page(offset, count)
                    .incremental_unwind()
                    .is_none()
            );
        }
        assert!(
            query
                .with_order_by(&[GraphValueOrder::descending(0)])
                .unwrap()
                .incremental_unwind()
                .is_none()
        );
        assert!(leaf.incremental_unwind().is_none());
    }

    #[test]
    fn terminal_window_split_preserves_every_child_scope_and_requires_explicit_order() {
        let leaf = PreparedGraphSet::from(pattern());
        let input = leaf
            .clone()
            .combine(
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                leaf.clone().with_page(1, Some(2)),
            )
            .unwrap()
            .nested()
            .unwrap();
        let order = [GraphValueOrder::descending(0)];
        for count in [0, 1, u64::MAX] {
            let query = input
                .clone()
                .with_order_by(&order)
                .unwrap()
                .with_page(3, Some(count));
            let frozen = query.canonical_bytes();
            let (child, keys, skip, take) = query.incremental_ordered_window().unwrap();
            assert_eq!(child.canonical_bytes(), input.canonical_bytes());
            assert_eq!(keys, order);
            assert_eq!((skip, take), (3, count));
            assert_eq!(query.canonical_bytes(), frozen);
            assert!(child.incremental_ordered_window().is_none());
        }
        assert!(
            input
                .clone()
                .with_page(0, Some(0))
                .incremental_ordered_window()
                .is_none()
        );
        assert!(
            input
                .clone()
                .with_page(1, None)
                .incremental_ordered_window()
                .is_none()
        );
        assert!(
            input
                .with_order_by(&order)
                .unwrap()
                .incremental_ordered_window()
                .is_none()
        );
    }
}
