//! Scoped semijoin/antijoin lowering reuses the positive-pattern compiler.

use super::*;
use crate::algebra::GraphExistence;

impl GraphPatternBuilder {
    /// Project outer values subject to correlated EXISTS / NOT EXISTS patterns.
    /// All constraints are AND-conjoined, before DISTINCT/order/pagination.
    /// Inner variable names that also occur outside are correlated identities;
    /// other names are local and cannot be returned. Every inner definition
    /// must be connected and have at least one outer correlation.
    pub fn prepare_values_with_existence(
        &self,
        constraints: &[GraphExistence<'_>],
        columns: &[GraphColumn<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern<GraphValueRow>, PatternBuildError> {
        let mut output = self.prepare_values(columns, offset, count)?;
        if constraints.is_empty() { return Ok(output); }
        check_total(constraints.len(), MAX_PATTERN_IDENTITIES, PatternLimitDimension::Identities)?;
        let mut edges = self.edges.len();
        let mut predicates = self.predicate_count;
        let mut identities = self.identities.len();
        for constraint in constraints {
            edges = edges.saturating_add(constraint.pattern.edges.len());
            predicates = predicates.saturating_add(constraint.pattern.predicate_count);
            identities = identities.saturating_add(constraint.pattern.identities.len());
            check_total(edges, MAX_PATTERN_EDGES, PatternLimitDimension::Edges)?;
            check_total(predicates, MAX_PATTERN_PREDICATES, PatternLimitDimension::Predicates)?;
            check_total(identities, MAX_PATTERN_IDENTITIES, PatternLimitDimension::Identities)?;
        }
        // Use the same compiler's slot map. No positional guesses from the
        // projection schema; a correlation need not be a returned column.
        let (mut operators, outer_slots) = self.compile()?;
        let width = (if self.edges.is_empty() { 1 } else { self.edges.len() + 1 }) as u32;
        for (group, constraint) in constraints.iter().enumerate() {
            let mut inner = (*constraint.pattern).clone();
            if inner.variables.is_empty() { return Err(PatternBuildError::EmptyPattern); }
            let correlation = |inner_at: usize| self.variables.iter()
                .position(|outer| outer.name == inner.variables[inner_at].name);
            let anchor = if inner.edges.is_empty() {
                if inner.variables.len() != 1 { return Err(PatternBuildError::Disconnected); }
                correlation(0).map(|outer| (0, outer))
            } else {
                inner.edges.iter().enumerate().find_map(|(at, edge)| {
                    correlation(edge.source).map(|outer| (at, outer))
                        .or_else(|| correlation(edge.destination).map(|outer| (at, outer)))
                })
            }.ok_or(PatternBuildError::Disconnected)?;
            let (edge_at, outer_at) = anchor;
            if !inner.edges.is_empty() {
                inner.edges.swap(0, edge_at);
                if inner.variables[inner.edges[0].source].name != self.variables[outer_at].name {
                    let edge = &mut inner.edges[0];
                    core::mem::swap(&mut edge.source, &mut edge.destination);
                    edge.direction = super::super::reverse(edge.direction);
                }
            }
            let (body, inner_slots) = inner.compile()?;
            let correlations: Vec<_> = inner.variables.iter().enumerate().filter_map(|(at, variable)| {
                self.variables.iter().position(|outer| outer.name == variable.name)
                    .map(|outer| (inner_slots[at], outer_slots[outer]))
            }).collect();
            let start = operators.len();
            operators.push(GlaOperator::Probe { group: group as u32, end: 0, anti: constraint.anti });
            // A real copied binding, not a row scan or a synthetic identifier.
            // Keeping inner slots separate also keeps inner labels out of the
            // outer node scan's mandatory-label conflict witness.
            operators.push(GlaOperator::BindVertex { source: outer_slots[outer_at] });
            let map = |slot: BindingSlot| BindingSlot(width + slot.ordinal());
            let mut available = 1_u32;
            emit_correlations(&mut operators, &correlations, 0, map);
            for (at, operator) in body.into_iter().enumerate() {
                match operator {
                    GlaOperator::ScanVertices if at == 0 => {}
                    GlaOperator::ScanEdges { relation, direction } if at == 0 => {
                        operators.push(GlaOperator::Expand { source: BindingSlot(width), relation, direction });
                        emit_correlations(&mut operators, &correlations, available, map);
                        available += 1;
                    }
                    GlaOperator::Expand { source, relation, direction } => {
                        operators.push(GlaOperator::Expand { source: map(source), relation, direction });
                        emit_correlations(&mut operators, &correlations, available, map);
                        available += 1;
                    }
                    GlaOperator::Select { slot, predicates } => operators.push(GlaOperator::Select { slot: map(slot), predicates }),
                    GlaOperator::VertexIdentity { left, right, equal } => operators.push(GlaOperator::VertexIdentity { left: map(left), right: map(right), equal }),
                    _ => unreachable!("the positive compiler emits only scan/select/expand/identity"),
                }
            }
            let end = operators.len() as u32;
            operators.push(GlaOperator::ProbeEnd { group: group as u32 });
            operators[start] = GlaOperator::Probe { group: group as u32, end, anti: constraint.anti };
        }
        let suffix = &output.logical.operators()[output.logical.operators().len() - 4..];
        operators.extend_from_slice(suffix);
        output.logical = GlaPlan::from_operators(operators);
        output.edge_count = edges;
        Ok(output)
    }
}

fn check_total(observed: usize, limit: usize, dimension: PatternLimitDimension) -> Result<(), PatternBuildError> {
    if observed > limit { Err(PatternBuildError::LimitExceeded { dimension, limit, observed }) }
    else { Ok(()) }
}

fn emit_correlations(
    operators: &mut Vec<GlaOperator>,
    correlations: &[(BindingSlot, BindingSlot)],
    available: u32,
    map: impl Fn(BindingSlot) -> BindingSlot,
) {
    for &(inner, outer) in correlations {
        if inner.ordinal() == available {
            operators.push(GlaOperator::VertexIdentity { left: map(inner), right: outer, equal: true });
        }
    }
}
