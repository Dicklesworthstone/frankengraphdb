//! Scoped left/semi/anti joins lowered through the positive-pattern compiler.

use super::*;
use crate::algebra::existence::GraphMatchKind;
use crate::algebra::{GraphExistence, GraphMatchClause};

impl GraphPatternBuilder {
    /// Project outer values subject to EXISTS / NOT EXISTS patterns.
    /// Shared names correlate; a child without shared names is independent.
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

    /// Compile ordered OPTIONAL, EXISTS and NOT EXISTS clauses.
    /// OPTIONAL exports new variables, nullable when its complete child has no
    /// match. Later clauses may correlate those variables but cannot rebind a
    /// null. EXISTS locals stay private. A complete optional witness remains a
    /// witness even if a later clause rejects it. ALL/DISTINCT and pagination
    /// apply only to the final correlated projection.
    ///
    /// Shared names are correlations; a child with none scans independently.
    /// Independent OPTIONAL preserves the product of actual occurrences and
    /// null-extends once on absence; independent EXISTS never multiplies rows.
    /// Definition-wide edge/predicate/identity/visible-name and binding-frame
    /// caps apply; clause count is capped at 64. No runtime input constructs a
    /// GLA scope. No result cache changes source-error or cancellation order.
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
        let mut width = super::super::binding_width(&operators);
        let mut definition_bindings = width as usize;
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
            let edge_anchor = inner.edges.iter().enumerate().find_map(|(at, edge)| {
                correlation(edge.source)
                    .map(|outer| (at, outer))
                    .or_else(|| correlation(edge.destination).map(|outer| (at, outer)))
            });
            // Keep the old anchor choice for correlated definitions. Without
            // any shared name, the child's own root supplies its first binding.
            let (outer_at, root) = if let Some((edge_at, outer_at)) = edge_anchor {
                inner.edges.swap(0, edge_at);
                if inner.variables[inner.edges[0].source].name != scope.variables[outer_at].name {
                    let edge = &mut inner.edges[0];
                    core::mem::swap(&mut edge.source, &mut edge.destination);
                    edge.direction = super::super::reverse(edge.direction);
                }
                (Some(outer_at), None)
            } else if let Some((inner_at, outer_at)) =
                (0..inner.variables.len()).find_map(|at| correlation(at).map(|outer| (at, outer)))
            {
                (Some(outer_at), Some(inner_at))
            } else {
                (None, None)
            };
            let (body, inner_slots) = inner.compile_with_root(root)?;
            let inner_width = super::super::binding_width(&body);
            definition_bindings = definition_bindings.saturating_add(inner_width as usize);
            check_total(
                definition_bindings,
                super::super::MAX_PATTERN_BINDINGS,
                PatternLimitDimension::Bindings,
            )?;
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
            let base = width;
            let map = |slot: BindingSlot| BindingSlot(base + slot.ordinal());
            let mut available = 0_u32;
            if let Some(outer_at) = outer_at {
                // Copy only an actual correlation. An unrelated nullable outer
                // variable cannot suppress an independent child's witnesses.
                operators.push(GlaOperator::BindVertex {
                    source: scope_slots[outer_at],
                });
                emit_correlations(&mut operators, &correlations, 0, map);
                available = 1;
            }
            for (at, operator) in body.into_iter().enumerate() {
                match operator {
                    GlaOperator::ScanVertices if at == 0 && outer_at.is_some() => {}
                    GlaOperator::ScanVertices => {
                        // An already visible variable is a bound correlation,
                        // not a fresh Cartesian dimension. A null correlation
                        // must fail the child, never be rebound by a new scan.
                        if let Some((_, outer)) = correlations
                            .iter()
                            .find(|(inner, _)| inner.ordinal() == available)
                        {
                            operators.push(GlaOperator::BindVertex { source: *outer });
                        } else {
                            operators.push(GlaOperator::ScanVertices);
                        }
                        emit_correlations(&mut operators, &correlations, available, map);
                        available += 1;
                    }
                    GlaOperator::ScanEdges {
                        relation,
                        direction,
                    } if at == 0 => {
                        if outer_at.is_none() {
                            // A fixed edge root owns two slots. In a scope,
                            // create its independent source then append its
                            // destination through the ordinary expansion path.
                            operators.push(GlaOperator::ScanVertices);
                            available += 1;
                        }
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
                    GlaOperator::VarLengthExpand {
                        source,
                        relation,
                        direction,
                        bounds,
                    } => {
                        // A whole bounded walk appends one endpoint, not one
                        // slot per hop. Correlations constrain that endpoint
                        // after enumeration without filtering its transit nodes.
                        operators.push(GlaOperator::VarLengthExpand {
                            source: map(source),
                            relation,
                            direction,
                            bounds,
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
                    GlaOperator::CompareProperties {
                        left,
                        left_key,
                        right,
                        right_key,
                        comparison,
                    } => {
                        operators.push(GlaOperator::CompareProperties {
                            left: map(left),
                            left_key,
                            right: map(right),
                            right_key,
                            comparison,
                        });
                    }
                    GlaOperator::SelectBoolean { expression } => {
                        operators.push(GlaOperator::SelectBoolean {
                            expression: expression.remap(map),
                        });
                    }
                    _ => unreachable!(
                        "the positive compiler emits only scan/select/expand/identity/compare"
                    ),
                }
            }
            debug_assert_eq!(available, inner_width);
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
                // Count actual producers, including independent scan roots and
                // copied correlations. The definition-wide cap bounds all such
                // frames, including transient probes, before execution begins.
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
