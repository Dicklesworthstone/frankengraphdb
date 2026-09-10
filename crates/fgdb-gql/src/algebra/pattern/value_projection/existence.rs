//! Scoped left/semi/anti joins lowered through the positive-pattern compiler.

use super::*;
use crate::algebra::existence::GraphMatchKind;
use crate::algebra::{GraphExistence, GraphMatchClause};

impl GraphPatternBuilder {
    /// Project outer values subject to correlated EXISTS / NOT EXISTS patterns.
    /// Inner local names never become outer projections or later correlations.
    pub fn prepare_values_with_existence(
        &self,
        constraints: &[GraphExistence<'_>],
        columns: &[GraphColumn<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern<GraphValueRow>, PatternBuildError> {
        check_total(
            constraints.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::Identities,
        )?;
        let clauses: Vec<_> = constraints
            .iter()
            .map(|constraint| {
                if constraint.anti {
                    GraphMatchClause::not_exists(constraint.pattern)
                } else {
                    GraphMatchClause::exists(constraint.pattern)
                }
            })
            .collect();
        self.prepare_values_with_clauses(&clauses, columns, offset, count)
    }

    /// Compile ordered correlated OPTIONAL, EXISTS and NOT EXISTS clauses.
    /// OPTIONAL exports new variables, nullable when its complete child has no
    /// match. Later clauses may correlate those variables but cannot rebind a
    /// null. EXISTS locals stay private. A complete optional witness remains a
    /// witness even if a later clause rejects it. ALL/DISTINCT and pagination
    /// apply only to the final correlated projection.
    ///
    /// Each positive connected child needs an already visible correlation.
    /// Definition-wide edge/predicate/identity/visible-variable caps apply;
    /// clause count is capped at 64. No runtime input constructs a GLA scope.
    pub fn prepare_values_with_clauses(
        &self,
        clauses: &[GraphMatchClause<'_>],
        columns: &[GraphColumn<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern<GraphValueRow>, PatternBuildError> {
        if clauses.is_empty() {
            return self.prepare_values(columns, offset, count);
        }
        check_total(
            clauses.len(),
            MAX_PATTERN_IDENTITIES,
            PatternLimitDimension::Identities,
        )?;
        let mut edges = self.edges.len();
        let mut predicates = self.predicate_count;
        let mut identities = self.identities.len();
        for clause in clauses {
            edges = edges.saturating_add(clause.pattern.edges.len());
            predicates = predicates.saturating_add(clause.pattern.predicate_count);
            identities = identities.saturating_add(clause.pattern.identities.len());
            check_total(edges, MAX_PATTERN_EDGES, PatternLimitDimension::Edges)?;
            check_total(
                predicates,
                MAX_PATTERN_PREDICATES,
                PatternLimitDimension::Predicates,
            )?;
            check_total(
                identities,
                MAX_PATTERN_IDENTITIES,
                PatternLimitDimension::Identities,
            )?;
        }
        // Scope metadata has only declared names; it is never recompiled as an
        // inner-join replacement for the sequence of nullable clauses.
        let mut scope = self.clone();
        let (mut operators, mut scope_slots) = self.compile()?;
        let mut width = (if self.edges.is_empty() {
            1
        } else {
            self.edges.len() + 1
        }) as u32;
        for (group, clause) in clauses.iter().enumerate() {
            let mut inner = (*clause.pattern).clone();
            if inner.variables.is_empty() {
                return Err(PatternBuildError::EmptyPattern);
            }
            let correlation = |inner_at: usize| {
                scope
                    .variables
                    .iter()
                    .position(|outer| outer.name == inner.variables[inner_at].name)
            };
            let anchor = if inner.edges.is_empty() {
                if inner.variables.len() != 1 {
                    return Err(PatternBuildError::Disconnected);
                }
                correlation(0).map(|outer| (0, outer))
            } else {
                inner.edges.iter().enumerate().find_map(|(at, edge)| {
                    correlation(edge.source)
                        .map(|outer| (at, outer))
                        .or_else(|| correlation(edge.destination).map(|outer| (at, outer)))
                })
            }
            .ok_or(PatternBuildError::Disconnected)?;
            let (edge_at, outer_at) = anchor;
            if !inner.edges.is_empty() {
                inner.edges.swap(0, edge_at);
                if inner.variables[inner.edges[0].source].name != scope.variables[outer_at].name {
                    let edge = &mut inner.edges[0];
                    core::mem::swap(&mut edge.source, &mut edge.destination);
                    edge.direction = super::super::reverse(edge.direction);
                }
            }
            let (body, inner_slots) = inner.compile()?;
            let correlations: Vec<_> = inner
                .variables
                .iter()
                .enumerate()
                .filter_map(|(at, variable)| {
                    scope
                        .variables
                        .iter()
                        .position(|outer| outer.name == variable.name)
                        .map(|outer| (inner_slots[at], scope_slots[outer]))
                })
                .collect();
            let start = operators.len();
            let optional = clause.kind == GraphMatchKind::Optional;
            operators.push(if optional {
                GlaOperator::Optional {
                    group: group as u32,
                    end: 0,
                    slots: 0,
                }
            } else {
                GlaOperator::Probe {
                    group: group as u32,
                    end: 0,
                    anti: clause.kind == GraphMatchKind::NotExists,
                }
            });
            // An explicit inner copy preserves outer labels/identities and
            // prevents a child predicate becoming a mandatory outer-scan label.
            operators.push(GlaOperator::BindVertex {
                source: scope_slots[outer_at],
            });
            let base = width;
            let map = |slot: BindingSlot| BindingSlot(base + slot.ordinal());
            let mut available = 1_u32;
            emit_correlations(&mut operators, &correlations, 0, map);
            for (at, operator) in body.into_iter().enumerate() {
                match operator {
                    GlaOperator::ScanVertices if at == 0 => {}
                    GlaOperator::ScanEdges {
                        relation,
                        direction,
                    } if at == 0 => {
                        operators.push(GlaOperator::Expand {
                            source: BindingSlot(base),
                            relation,
                            direction,
                        });
                        emit_correlations(&mut operators, &correlations, available, map);
                        available += 1;
                    }
                    GlaOperator::Expand {
                        source,
                        relation,
                        direction,
                    } => {
                        operators.push(GlaOperator::Expand {
                            source: map(source),
                            relation,
                            direction,
                        });
                        emit_correlations(&mut operators, &correlations, available, map);
                        available += 1;
                    }
                    GlaOperator::Select { slot, predicates } => {
                        operators.push(GlaOperator::Select {
                            slot: map(slot),
                            predicates,
                        })
                    }
                    GlaOperator::VertexIdentity { left, right, equal } => {
                        operators.push(GlaOperator::VertexIdentity {
                            left: map(left),
                            right: map(right),
                            equal,
                        })
                    }
                    _ => {
                        unreachable!("the positive compiler emits only scan/select/expand/identity")
                    }
                }
            }
            let end = operators.len() as u32;
            if optional {
                operators.push(GlaOperator::OptionalEnd {
                    group: group as u32,
                });
                operators[start] = GlaOperator::Optional {
                    group: group as u32,
                    end,
                    slots: available,
                };
                for (at, variable) in inner.variables.iter().enumerate() {
                    if !scope
                        .variables
                        .iter()
                        .any(|outer| outer.name == variable.name)
                    {
                        scope.vertex(&variable.name)?;
                        scope_slots.push(map(inner_slots[at]));
                    }
                }
                // At most total edges + one copied anchor per clause + the
                // root slot are live, bounded above by 129 slots, not by data.
                width += available;
            } else {
                operators.push(GlaOperator::ProbeEnd {
                    group: group as u32,
                });
                operators[start] = GlaOperator::Probe {
                    group: group as u32,
                    end,
                    anti: clause.kind == GraphMatchKind::NotExists,
                };
            }
        }
        let variables = scope.checked_value_columns(columns)?;
        let projection = columns
            .iter()
            .zip(variables)
            .map(|(column, variable)| {
                let slot = scope_slots[variable];
                match column {
                    GraphColumn::Vertex { .. } => ValueProjection::Vertex { slot },
                    GraphColumn::Property { key, .. } => {
                        ValueProjection::Property { slot, key: *key }
                    }
                }
            })
            .collect();
        operators.extend([
            GlaOperator::ProjectValues {
                columns: projection,
            },
            GlaOperator::Distinct,
            GlaOperator::OrderByValues,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan::from_operators(operators),
            variable_count: scope.variables.len(),
            edge_count: edges,
            columns: columns
                .iter()
                .map(|column| column.name().to_owned())
                .collect(),
        })
    }

    /// One projection validation path for positive and scoped value patterns.
    pub(super) fn checked_value_columns(
        &self,
        columns: &[GraphColumn<'_>],
    ) -> Result<Vec<usize>, PatternBuildError> {
        if self.variables.is_empty() {
            return Err(PatternBuildError::EmptyPattern);
        }
        if columns.is_empty() {
            return Err(PatternBuildError::EmptyProjection);
        }
        check_total(
            columns.len(),
            MAX_PATTERN_VERTICES,
            PatternLimitDimension::Columns,
        )?;
        let mut variables = Vec::new();
        for (at, column) in columns.iter().enumerate() {
            let bytes = column.name().as_bytes();
            if bytes.is_empty()
                || bytes.len() > MAX_PATTERN_NAME_BYTES
                || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
                || !bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
            {
                return Err(PatternBuildError::InvalidColumnName);
            }
            if columns[..at]
                .iter()
                .any(|previous| previous.name() == column.name())
            {
                return Err(PatternBuildError::DuplicateProjection);
            }
            variables.push(self.variable(column.variable())?);
        }
        Ok(variables)
    }
}

fn check_total(
    observed: usize,
    limit: usize,
    dimension: PatternLimitDimension,
) -> Result<(), PatternBuildError> {
    if observed > limit {
        Err(PatternBuildError::LimitExceeded {
            dimension,
            limit,
            observed,
        })
    } else {
        Ok(())
    }
}

fn emit_correlations(
    operators: &mut Vec<GlaOperator>,
    correlations: &[(BindingSlot, BindingSlot)],
    available: u32,
    map: impl Fn(BindingSlot) -> BindingSlot,
) {
    for &(inner, outer) in correlations {
        if inner.ordinal() == available {
            operators.push(GlaOperator::VertexIdentity {
                left: map(inner),
                right: outer,
                equal: true,
            });
        }
    }
}
