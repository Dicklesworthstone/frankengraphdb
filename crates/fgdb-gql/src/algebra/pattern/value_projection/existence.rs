//! Ordered inner/left/semi/anti joins lowered through the positive-pattern compiler.

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

    /// Compile ordered required MATCH, OPTIONAL, EXISTS and NOT EXISTS clauses.
    /// Required and OPTIONAL clauses export new variables. Required absence
    /// eliminates the incoming occurrence; OPTIONAL absence null-extends it.
    /// Later positive patterns cannot rebind a null. Explicit outer_vertex
    /// operands instead capture the original nullable value for predicates.
    /// Every clause may bind fixed relationships and the positive compiler's
    /// bounded path captures. Required and OPTIONAL clauses export them;
    /// absence leaves OPTIONAL captures null, like its vertices. Path predicates
    /// belong to their own clause, before absence is decided. EXISTS captures
    /// stay private. No clause correlates with an outer relationship or path
    /// alias. A complete optional witness remains a witness
    /// even if a later required clause rejects it. ALL/DISTINCT and pagination
    /// apply only to the final correlated projection.
    ///
    /// Shared pattern names are correlations; a child with none scans independently.
    /// Predicate-only captures neither anchor nor constrain a positive scan.
    /// Independent required MATCH preserves the product of actual occurrences,
    /// without inventing a row for an empty child. Independent OPTIONAL instead
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
        let mut path_captures = self.path_captures.len();
        let mut scope_count = 0;
        let mut pending: Vec<_> = clauses.iter().rev().collect();
        while let Some(clause) = pending.pop() {
            scope_count += 1;
            check_total(
                scope_count,
                MAX_PATTERN_IDENTITIES,
                PatternLimitDimension::Identities,
            )?;
            pending.extend(clause.nested.iter().rev());
            if clause.pattern.variables.iter().any(|variable| {
                self.path_captures
                    .iter()
                    .any(|capture| capture.name == variable.name)
            }) {
                return Err(PatternBuildError::DuplicateVariable);
            }
            edges = edges.saturating_add(clause.pattern.edges.len());
            predicates = predicates.saturating_add(clause.pattern.predicate_count);
            identities = identities.saturating_add(clause.pattern.identities.len());
            path_captures = path_captures.saturating_add(clause.pattern.path_captures.len());
            check_total(
                path_captures,
                MAX_PATTERN_IDENTITIES,
                PatternLimitDimension::PathCaptures,
            )?;
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
        struct ClauseFrames {
            next_group: usize,
            bindings: usize,
        }
        fn append_clauses(
            scope: &mut GraphPatternBuilder,
            scope_slots: &mut Vec<BindingSlot>,
            width: &mut u32,
            operators: &mut Vec<GlaOperator>,
            clauses: &[GraphMatchClause<'_>],
            first_group: usize,
            frames: &mut ClauseFrames,
        ) -> Result<(), PatternBuildError> {
            for (ordinal, clause) in clauses.iter().enumerate() {
                let group = first_group + ordinal;
                let mut inner = (*clause.pattern).clone();
                if inner.variables.is_empty() {
                    return Err(PatternBuildError::EmptyPattern);
                }
                // A shared edge name would mean an edge correlation, not a fresh
                // local binding. Refuse that unsupported contract rather than
                // silently shadowing a visible vertex or relationship.
                if inner.path_captures.iter().any(|capture| {
                    scope
                        .variables
                        .iter()
                        .any(|variable| variable.name == capture.name)
                        || scope
                            .path_captures
                            .iter()
                            .any(|outer| outer.name == capture.name)
                }) || inner.variables.iter().any(|variable| {
                    scope
                        .path_captures
                        .iter()
                        .any(|capture| capture.name == variable.name)
                }) {
                    return Err(PatternBuildError::DuplicateVariable);
                }
                let mut captures = Vec::new();
                for (at, variable) in inner.variables.iter().enumerate() {
                    if variable.outer {
                        let outer = scope
                            .variable(&variable.name)
                            .map_err(|_| PatternBuildError::UnknownOuterVertex)?;
                        captures.push((at, scope_slots[outer]));
                    }
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
                // Only positive pattern vertices may anchor traversal. A predicate
                // capture can be null and must not suppress independent witnesses.
                let (outer_at, root) = if inner
                    .path_captures
                    .iter()
                    .any(|capture| !capture.edge_identity)
                {
                    // CapturePath concatenates traversal segments in DECLARED
                    // order from the declared start. Re-rooting at a correlated
                    // destination reverses a segment; swapping a later edge also
                    // destroys a captured range's order. Keep the native root and
                    // edge order for these clauses. A shared native root can still
                    // be bound directly; other correlations are enforced at their
                    // ordinary binding slots, before any capture or path predicate.
                    // This uses the existing governed scan/expand family, not an
                    // endpoint-to-edge reconstruction or a second path evaluator.
                    let first =
                        inner.edges.first().map(|edge| edge.source).or_else(|| {
                            inner.variables.iter().position(|variable| !variable.outer)
                        });
                    (first.and_then(correlation), None)
                } else if let Some((edge_at, outer_at)) = edge_anchor {
                    inner.edges.swap(0, edge_at);
                    // Captures address declared edges, not positions chosen by a
                    // correlated traversal. Preserve that identity when a later
                    // edge is moved to the root; both swapped positions matter.
                    for capture in &mut inner.path_captures {
                        if capture.first_edge == 0 {
                            capture.first_edge = edge_at;
                        } else if capture.first_edge == edge_at {
                            capture.first_edge = 0;
                        }
                    }
                    if inner.variables[inner.edges[0].source].name != scope.variables[outer_at].name
                    {
                        let edge = &mut inner.edges[0];
                        core::mem::swap(&mut edge.source, &mut edge.destination);
                        edge.direction = super::super::reverse(edge.direction);
                    }
                    (Some(outer_at), None)
                } else if let Some((inner_at, outer_at)) = (0..inner.variables.len())
                    .filter(|&at| !inner.variables[at].outer)
                    .find_map(|at| correlation(at).map(|outer| (at, outer)))
                {
                    (Some(outer_at), Some(inner_at))
                } else {
                    (None, None)
                };
                let (body, inner_slots) = inner.compile_with_root(root)?;
                let inner_width = super::super::binding_width(&body);
                frames.bindings = frames.bindings.saturating_add(inner_width as usize);
                check_total(
                    frames.bindings,
                    super::super::MAX_PATTERN_BINDINGS,
                    PatternLimitDimension::Bindings,
                )?;
                let captures: Vec<_> = captures
                    .into_iter()
                    .map(|(at, outer)| (inner_slots[at], outer))
                    .collect();
                let correlations: Vec<_> = inner
                    .variables
                    .iter()
                    .enumerate()
                    .filter(|(_, variable)| !variable.outer)
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
                let required = clause.kind == GraphMatchKind::Required;
                let collect = clause.kind == GraphMatchKind::Collect;
                if optional {
                    operators.push(GlaOperator::Optional {
                        group: group as u32,
                        end: 0,
                        slots: 0,
                    });
                } else if collect {
                    operators.push(GlaOperator::Collect {
                        group: group as u32,
                        end: 0,
                        slots: 0,
                    });
                } else if !required {
                    operators.push(GlaOperator::Probe {
                        group: group as u32,
                        end: 0,
                        anti: clause.kind == GraphMatchKind::NotExists,
                    });
                }
                // Required MATCH is the ordinary positive continuation, not a probe
                // or a nullable scope. Emit it at this exact point; moving it into
                // a preceding OPTIONAL would change which rows are null-extended.
                let base = *width;
                let map = |slot: BindingSlot| BindingSlot(base + slot.ordinal());
                // A clause's captures follow every capture exported so far, so a
                // probe cannot replace a visible relationship. Sequential probes
                // reuse their private slots: the native capture continuation
                // restores them on success, absence and refusal. An OPTIONAL
                // clause's slots are written only inside its scope, so absence
                // leaves them null.
                let capture_base = scope.path_captures.len() as u32;
                let map_capture = |capture| capture_base + capture;
                let mut available = 0_u32;
                if let Some(outer_at) = outer_at {
                    // Copy only an actual correlation. An unrelated nullable outer
                    // variable cannot suppress an independent child's witnesses.
                    operators.push(GlaOperator::BindVertex {
                        source: scope_slots[outer_at],
                    });
                    emit_correlations(operators, &correlations, 0, map);
                    available = 1;
                }
                for (at, operator) in body.into_iter().enumerate() {
                    match operator {
                        GlaOperator::ScanVertices if at == 0 && outer_at.is_some() => {}
                        GlaOperator::ScanVertices => {
                            // A captured operand keeps null; a matched correlation
                            // must reject it. Neither is another graph scan. Do not
                            // emit NULL = NULL for captures: they are value copies,
                            // not additional positive identity constraints.
                            if let Some((_, outer)) = captures
                                .iter()
                                .find(|(inner, _)| inner.ordinal() == available)
                            {
                                operators.push(GlaOperator::BindOuterVertex { source: *outer });
                            } else if let Some((_, outer)) = correlations
                                .iter()
                                .find(|(inner, _)| inner.ordinal() == available)
                            {
                                operators.push(GlaOperator::BindVertex { source: *outer });
                            } else {
                                operators.push(GlaOperator::ScanVertices);
                            }
                            emit_correlations(operators, &correlations, available, map);
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
                            emit_correlations(operators, &correlations, available, map);
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
                            emit_correlations(operators, &correlations, available, map);
                            available += 1;
                        }
                        GlaOperator::VarLengthExpand {
                            source,
                            relation,
                            direction,
                            bounds,
                            search,
                        } => {
                            // A whole bounded walk appends one endpoint, not one
                            // slot per hop. Correlations constrain that endpoint
                            // after enumeration without filtering its transit nodes.
                            operators.push(GlaOperator::VarLengthExpand {
                                source: map(source),
                                relation,
                                direction,
                                bounds,
                                search,
                            });
                            emit_correlations(operators, &correlations, available, map);
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
                        GlaOperator::CapturePath {
                            capture,
                            start,
                            segments,
                        } => {
                            operators.push(GlaOperator::CapturePath {
                                capture: map_capture(capture),
                                start: map(start),
                                segments: segments.into_iter().map(map).collect(),
                            });
                        }
                        GlaOperator::SelectPathLength {
                            capture,
                            comparison,
                            value,
                        } => {
                            operators.push(GlaOperator::SelectPathLength {
                                capture: map_capture(capture),
                                comparison,
                                value,
                            });
                        }
                        GlaOperator::SelectPathNull {
                            capture,
                            function,
                            is_null,
                        } => {
                            operators.push(GlaOperator::SelectPathNull {
                                capture: map_capture(capture),
                                function,
                                is_null,
                            });
                        }
                        GlaOperator::DifferentEdges { segments } => {
                            operators.push(GlaOperator::DifferentEdges {
                                segments: segments.into_iter().map(map).collect(),
                            });
                        }
                        GlaOperator::SelectBoolean { expression } => {
                            operators.push(GlaOperator::SelectBoolean {
                                expression: expression.remap_elements(map, map_capture),
                            });
                        }
                        _ => unreachable!(
                            "the positive compiler emits only admitted pattern instructions"
                        ),
                    }
                }
                debug_assert_eq!(available, inner_width);
                if !clause.nested.is_empty() {
                    // Nested probes run after this positive witness is complete,
                    // but before this clause accepts it. Give them the entire
                    // visible ancestor chain, with this body's locals added only
                    // to their private scope. Correlations still copy/recheck the
                    // real slots; a nullable ancestor is never another root scan.
                    let mut nested_scope = scope.clone();
                    let mut nested_slots = scope_slots.clone();
                    for (at, variable) in inner.variables.iter().enumerate() {
                        if !nested_scope
                            .variables
                            .iter()
                            .any(|outer| outer.name == variable.name)
                        {
                            nested_scope.vertex(&variable.name)?;
                            nested_slots.push(map(inner_slots[at]));
                        }
                    }
                    nested_scope
                        .path_captures
                        .extend(inner.path_captures.iter().cloned());
                    let mut nested_width = base + available;
                    let first_nested = frames.next_group;
                    frames.next_group += clause.nested.len();
                    append_clauses(
                        &mut nested_scope,
                        &mut nested_slots,
                        &mut nested_width,
                        operators,
                        &clause.nested,
                        first_nested,
                        frames,
                    )?;
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
                } else if collect {
                    // Each witness contributes its local vertex, relationship or
                    // path, at the slot or capture this clause gave it.
                    let value = clause
                        .element
                        .map(|name| {
                            if let Some(at) = inner.variables.iter().position(|v| v.name == name) {
                                Ok(ValueProjection::Vertex {
                                    slot: map(inner_slots[at]),
                                })
                            } else if let Some(capture) =
                                inner.path_captures.iter().position(|c| c.name == name)
                            {
                                Ok(ValueProjection::Path {
                                    capture: map_capture(capture as u32),
                                    function: if inner.path_captures[capture].edge_identity {
                                        GraphPathFunction::Edge
                                    } else {
                                        GraphPathFunction::Value
                                    },
                                })
                            } else {
                                Err(PatternBuildError::UnknownVariable)
                            }
                        })
                        .transpose()?;
                    operators.push(GlaOperator::CollectEnd {
                        group: group as u32,
                        value,
                    });
                    operators[start] = GlaOperator::Collect {
                        group: group as u32,
                        end,
                        slots: available,
                    };
                } else if !required {
                    operators.push(GlaOperator::ProbeEnd {
                        group: group as u32,
                    });
                    operators[start] = GlaOperator::Probe {
                        group: group as u32,
                        end,
                        anti: clause.kind == GraphMatchKind::NotExists,
                    };
                }
                if optional || required {
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
                    // Exported in declaration order at capture_base + i, the
                    // numbering map_capture gave this clause's CapturePath.
                    scope
                        .path_captures
                        .extend(inner.path_captures.iter().cloned());
                    // Count actual producers, including independent scan roots and
                    // copied correlations. The definition-wide cap bounds all such
                    // frames, including transient probes, before execution begins.
                    *width += available;
                }
            }
            Ok(())
        }
        // Keep top-level group ordinals unchanged: collection projections
        // address their own top-level clause. Nested probes use the remaining
        // definition-wide group slots, never an ancestor's active group.
        let mut frames = ClauseFrames {
            next_group: clauses.len(),
            bindings: width as usize,
        };
        append_clauses(
            &mut scope,
            &mut scope_slots,
            &mut width,
            &mut operators,
            clauses,
            0,
            &mut frames,
        )?;
        // A collection column reads its own Collect clause's list.
        if columns.iter().any(|column| {
            matches!(column, GraphColumn::Collection { clause, .. }
                if clauses.get(*clause).map(|clause| clause.kind) != Some(GraphMatchKind::Collect))
        }) {
            return Err(PatternBuildError::UnknownVariable);
        }
        let variables = scope.checked_value_columns(columns)?;
        let projection = columns
            .iter()
            .zip(variables)
            .map(|(column, variable)| match column {
                GraphColumn::Collection { .. } => ValueProjection::Collected {
                    group: variable as u32,
                },
                GraphColumn::Vertex { .. } => ValueProjection::Vertex {
                    slot: scope_slots[variable],
                },
                GraphColumn::Property { key, .. } => ValueProjection::Property {
                    slot: scope_slots[variable],
                    key: *key,
                },
                GraphColumn::EdgeProperty { key, .. } => ValueProjection::EdgeProperty {
                    capture: variable as u32,
                    key: *key,
                },
                GraphColumn::Path {
                    function,
                    variable: name,
                    ..
                } => match function {
                    GraphPathFunction::Labels => ValueProjection::Labels {
                        slot: scope_slots[variable],
                    },
                    GraphPathFunction::Type => ValueProjection::Type {
                        capture: variable as u32,
                    },
                    GraphPathFunction::Properties if scope.variable(name).is_ok() => {
                        ValueProjection::Properties {
                            slot: scope_slots[variable],
                        }
                    }
                    GraphPathFunction::Properties => ValueProjection::EdgeProperties {
                        capture: variable as u32,
                    },
                    _ => ValueProjection::Path {
                        capture: variable as u32,
                        function: *function,
                    },
                },
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
            if !super::super::valid_column_name(column.name()) {
                return Err(PatternBuildError::InvalidColumnName);
            }
            if columns[..at]
                .iter()
                .any(|previous| previous.name() == column.name())
            {
                return Err(PatternBuildError::DuplicateProjection);
            }
            variables.push(match column {
                GraphColumn::Path {
                    variable, function, ..
                } => match function {
                    GraphPathFunction::Labels => self.variable(variable)?,
                    GraphPathFunction::Properties if self.variable(variable).is_ok() => {
                        self.variable(variable)?
                    }
                    GraphPathFunction::Type | GraphPathFunction::Properties => {
                        let capture = self.path_capture(variable)?;
                        if !self.path_captures[capture].edge_identity {
                            return Err(PatternBuildError::InvalidPathCapture);
                        }
                        capture
                    }
                    _ => {
                        let capture = self.path_capture(variable)?;
                        if (*function == GraphPathFunction::Edge)
                            != self.path_captures[capture].edge_identity
                        {
                            return Err(PatternBuildError::InvalidPathCapture);
                        }
                        capture
                    }
                },
                GraphColumn::EdgeProperty { variable, .. } => {
                    let capture = self.path_capture(variable)?;
                    if !self.path_captures[capture].edge_identity {
                        return Err(PatternBuildError::InvalidPathCapture);
                    }
                    capture
                }
                GraphColumn::Vertex { variable, .. } | GraphColumn::Property { variable, .. } => {
                    self.variable(variable)?
                }
                // Its clause index; the clause compiler checks the kind.
                GraphColumn::Collection { clause, .. } => *clause,
            });
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

#[cfg(test)]
mod edge_capture_tests;
