//! Indexed semi/anti joins over a compiler-owned GLA probe scope.
//! Probe bindings never escape into the outer row. One complete witness ends
//! the search, irrespective of its multiplicity; absence requires exhaustion.
//! No result bag, seen set, neighbor vector, or outer-root rescan is built.

use super::*;

#[derive(Clone, Copy)]
enum Binding {
    Copy { source: usize, nullable: bool },
    Expand(Expansion),
    Scan,
}

// A local scan owns its position, never the outer cursor's mutable position.
// Keep the domains distinct even though both identities use all 128 bits.
#[derive(Clone, Copy)]
enum Position {
    Copy(bool),
    Edge(Option<EId>),
    Vertex(Option<VId>),
}
impl Position {
    fn new(binding: Binding) -> Self {
        match binding {
            Binding::Copy { .. } => Self::Copy(false),
            Binding::Expand(_) => Self::Edge(None),
            Binding::Scan => Self::Vertex(None),
        }
    }
}

#[cfg(test)]
mod tests;

struct Step {
    binding: Binding,
    predicates: Vec<GlaOperator>,
}

pub(crate) struct Probe {
    anti: bool,
    outer_width: usize,
    steps: Vec<Step>,
}

impl Probe {
    /// Fixed-hop bodies may start from a correlation or an independent vertex
    /// scan. Captures only copy values; a nullable capture is not an anchor.
    /// Nested/optional scopes and variable-length atoms still refuse, including
    /// for LIMIT zero. Every local scan restarts only its own ordered position.
    pub(crate) fn compile(
        ops: &[GlaOperator], start: usize, outer_width: usize,
    ) -> Result<(Self, usize), EdgeScanBuildError> {
        let GlaOperator::Probe { group, end, anti } = &ops[start] else {
            return Err(EdgeScanBuildError { operator: start });
        };
        let end = *end as usize;
        if end <= start + 1 || !matches!(ops.get(end), Some(GlaOperator::ProbeEnd { group: close }) if close == group) {
            return Err(EdgeScanBuildError { operator: start });
        }
        if !matches!(ops.get(start + 1), Some(GlaOperator::ScanVertices
            | GlaOperator::BindVertex { .. } | GlaOperator::BindOuterVertex { .. })) {
            return Err(EdgeScanBuildError { operator: start + 1 });
        }
        let mut steps: Vec<Step> = Vec::new();
        let mut width = outer_width;
        let mut expansions = 0;
        for (at, op) in ops.iter().enumerate().take(end).skip(start + 1) {
            let bad = || EdgeScanBuildError { operator: at };
            let binding = match op {
                GlaOperator::ScanVertices => Some(Binding::Scan),
                GlaOperator::BindVertex { source } | GlaOperator::BindOuterVertex { source }
                    if (source.ordinal() as usize) < outer_width => {
                    Some(Binding::Copy { source: source.ordinal() as usize,
                        nullable: matches!(op, GlaOperator::BindOuterVertex { .. }) })
                }
                GlaOperator::Expand { source, relation, direction }
                    if (source.ordinal() as usize) < width && expansions < MAX_PATTERN_EDGES => {
                    expansions += 1;
                    Some(Binding::Expand(Expansion { source: source.ordinal() as usize,
                        relation: *relation, direction: *direction }))
                }
                GlaOperator::Select { slot, .. } if (slot.ordinal() as usize) < width => None,
                GlaOperator::VertexIdentity { left, right, .. }
                | GlaOperator::CompareProperties { left, right, .. }
                    if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width => None,
                GlaOperator::SelectBoolean { expression } => {
                    let mut vertices_valid = true;
                    let mut captures_valid = true;
                    let _ = expression.remap_elements(
                        |slot| { vertices_valid &= (slot.ordinal() as usize) < width; slot },
                        |capture| { captures_valid = false; capture },
                    );
                    if !vertices_valid || !captures_valid { return Err(bad()); }
                    None
                }
                _ => return Err(bad()),
            };
            if let Some(binding) = binding {
                if width >= MAX_PATTERN_BINDINGS { return Err(bad()); }
                width += 1;
                steps.push(Step { binding, predicates: Vec::new() });
            } else {
                steps.last_mut().ok_or_else(bad)?.predicates.push(op.clone());
            }
        }
        Ok((Self { anti: *anti, outer_width, steps }, end))
    }

    pub(crate) fn accepts<S: EdgeScanSource, C>(
        &self, outer: &[Option<VId>], source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<bool, S::Error, C> {
        Ok(self.witness(outer, source, control, record)? != self.anti)
    }

    fn witness<S: EdgeScanSource, C>(
        &self, outer: &[Option<VId>], source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<bool, S::Error, C> {
        debug_assert_eq!(outer.len(), self.outer_width);
        // Both frames have definition-bounded sizes. Reserve before allocating
        // and reuse them throughout backtracking, rather than copying prefixes.
        for _ in 0..self.outer_width + 3 * self.steps.len() {
            control(GlaExecutionEvent::ScratchEntry)?;
        }
        let mut ids = Vec::with_capacity(self.outer_width + self.steps.len());
        for id in outer {
            control(GlaExecutionEvent::Work)?;
            ids.push(*id);
        }
        let mut frames: Vec<_> = self.steps.iter().map(|step| Position::new(step.binding)).collect();
        let mut depth = 0;
        loop {
            control(GlaExecutionEvent::Work)?;
            if depth == self.steps.len() { return Ok(true); }
            let step = &self.steps[depth];
            let candidate = match step.binding {
                Binding::Copy { source: from, nullable } => {
                    if matches!(frames[depth], Position::Copy(true)) { None } else {
                        frames[depth] = Position::Copy(true);
                        let value = ids[from];
                        if value.is_none() && !nullable { None } else { Some(value) }
                    }
                }
                Binding::Scan => {
                    let Position::Vertex(after) = frames[depth] else { unreachable!("scan position"); };
                    let next = match source.next_probe_vertex(after, control) {
                        Ok(next) => next,
                        Err(EdgeExpansionSourceError::Unavailable) => {
                            return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                        }
                        Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
                    };
                    if let Some(vid) = next {
                        control(GlaExecutionEvent::Work)?;
                        if after.is_some_and(|prior| vid <= prior) {
                            return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
                        }
                        frames[depth] = Position::Vertex(Some(vid));
                        record()?;
                        // Histories may have no visible version at this cut.
                        // That is not a dangling edge and not a NULL witness.
                        if flatten(source.vertex(vid, control))?.is_none() { continue; }
                        Some(Some(vid))
                    } else { None }
                }
                Binding::Expand(expansion) => {
                    let Some(from) = ids[expansion.source] else {
                        if depth == 0 { return Ok(false); }
                        depth -= 1;
                        ids.pop();
                        continue;
                    };
                    let Position::Edge(after) = frames[depth] else { unreachable!("edge position"); };
                    let next = source.next_incident_edge(from, expansion.direction, after, control);
                    let next = match next {
                        Ok(next) => next,
                        Err(EdgeExpansionSourceError::Unavailable) => {
                            return Err(GqlQueryError::Source(EdgeScanError::ExpansionUnavailable));
                        }
                        Err(EdgeExpansionSourceError::Read(error)) => flatten(Err(error))?,
                    };
                    if let Some(eid) = next {
                        control(GlaExecutionEvent::Work)?;
                        if after.is_some_and(|prior| eid <= prior) {
                            return Err(GqlQueryError::Source(EdgeScanError::NonIncreasingIdentity));
                        }
                        frames[depth] = Position::Edge(Some(eid));
                        record()?;
                        let Some(edge) = flatten(source.edge(eid, control))? else { continue; };
                        if edge.relation != expansion.relation { continue; }
                        // Historical incidence is only a candidate index. The
                        // source's actual visible edge decides membership.
                        let to = match expansion.direction {
                            GlaDirection::Forward if edge.source == from => edge.target,
                            GlaDirection::Reverse if edge.target == from => edge.source,
                            GlaDirection::Undirected if edge.source == from => edge.target,
                            GlaDirection::Undirected if edge.target == from => edge.source,
                            _ => continue,
                        };
                        vertex(source, from, control)?;
                        if from != to { vertex(source, to, control)?; }
                        Some(Some(to))
                    } else { None }
                }
            };
            let Some(candidate) = candidate else {
                if depth == 0 { return Ok(false); }
                depth -= 1;
                ids.pop();
                continue;
            };
            ids.push(candidate);
            let mut passed = true;
            for predicate in &step.predicates {
                control(GlaExecutionEvent::Work)?;
                if !super::accepts(predicate, &ids, &[], source, control)? {
                    passed = false;
                    break;
                }
            }
            if !passed { ids.pop(); continue; }
            depth += 1;
            if depth < self.steps.len() { frames[depth] = Position::new(self.steps[depth].binding); }
        }
    }
}
