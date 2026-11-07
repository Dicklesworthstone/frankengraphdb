//! Indexed semi/anti joins over a compiler-owned GLA probe scope.
//! Probe bindings never escape into the outer row. One complete witness ends
//! the search, irrespective of its multiplicity; absence requires exhaustion.
//! Fixed hops keep no result bag or neighbor vector. Variable-length atoms
//! retain governed endpoint support and mode-aware traversal state only.

use super::*;
use crate::GraphWalkBounds;
use crate::algebra::GraphWalkSearch;
use std::collections::BTreeMap;

mod asynchronous;
mod walk;

#[derive(Clone, Copy)]
enum Binding {
    Copy {
        source: usize,
        nullable: bool,
    },
    Expand(Expansion),
    Scan,
    Walk {
        expansion: Expansion,
        bounds: GraphWalkBounds,
        search: GraphWalkSearch,
    },
}

// A local scan owns its position, never the outer cursor's mutable position.
// Keep the domains distinct even though both identities use all 128 bits.
#[derive(Clone, Copy)]
enum Position {
    Copy(bool),
    Edge(Option<EId>),
    Vertex(Option<VId>),
    Walk,
}
impl Position {
    fn new(binding: Binding) -> Self {
        match binding {
            Binding::Copy { .. } => Self::Copy(false),
            Binding::Expand(_) => Self::Edge(None),
            Binding::Scan => Self::Vertex(None),
            Binding::Walk { .. } => Self::Walk,
        }
    }
}

#[cfg(test)]
mod tests;

struct Step {
    binding: Binding,
    predicates: Vec<GlaOperator>,
    // Collection-only terminal semi/anti joins. Their bindings and capture
    // frames remain private; every accepted parent still contributes once.
    probes: Vec<Probe>,
}

#[derive(Clone, Copy)]
struct LocalCapture {
    slot: u32,
    // One variable-length segment remains a path, even at a one-hop bound.
    // Only a fixed single-edge capture can supply an Edge/property operand.
    single_edge: bool,
}

pub(crate) struct Probe {
    anti: bool,
    outer_width: usize,
    steps: Vec<Step>,
}

/// A bounded Collect scope over fixed hops and finite TRAIL atoms. A standalone
/// TRAIL may have one independent start. The shared probe kernel emits one
/// value per complete witness; only exhaustion emits a list.
pub(crate) struct Collection {
    pub(crate) group: usize,
    probe: Probe,
    value: Option<ValueProjection>,
    tuple: Option<Vec<ValueProjection>>,
}

impl Probe {
    /// Fixed and finite variable-length atoms may start from a correlation or
    /// an independent vertex scan. Captures only copy values; a nullable capture
    /// is not an anchor. Fixed relationship captures can feed local property
    /// predicates; neither paths nor relationship aliases escape the probe.
    /// Captured variable-length paths and nested/optional scopes still refuse,
    /// including at LIMIT zero. Local scans keep their own ordered positions.
    pub(crate) fn compile(
        ops: &[GlaOperator],
        start: usize,
        outer_width: usize,
    ) -> Result<(Self, usize), EdgeScanBuildError> {
        let GlaOperator::Probe { group, end, anti } = &ops[start] else {
            return Err(EdgeScanBuildError { operator: start });
        };
        let end = *end as usize;
        if end <= start + 1
            || !matches!(ops.get(end), Some(GlaOperator::ProbeEnd { group: close }) if close == group)
        {
            return Err(EdgeScanBuildError { operator: start });
        }
        let (steps, _) = Self::compile_body(ops, start, end, outer_width, false)?;
        Ok((
            Self {
                anti: *anti,
                outer_width,
                steps,
            },
            end,
        ))
    }

    fn compile_body(
        ops: &[GlaOperator],
        start: usize,
        end: usize,
        outer_width: usize,
        compound_captures: bool,
    ) -> Result<(Vec<Step>, BTreeMap<u32, LocalCapture>), EdgeScanBuildError> {
        if outer_width > MAX_PATTERN_BINDINGS
            || !matches!(
                ops.get(start + 1),
                Some(
                    GlaOperator::ScanVertices
                        | GlaOperator::BindVertex { .. }
                        | GlaOperator::BindOuterVertex { .. }
                )
            )
        {
            return Err(EdgeScanBuildError {
                operator: start + 1,
            });
        }
        let mut steps: Vec<Step> = Vec::new();
        let mut width = outer_width;
        let mut expansions = 0;
        // A closing edge can bind a fresh slot equal to an earlier vertex.
        // Keep those proved aliases, including outer copies, so captures can
        // follow their declared route through cycles or reversed expansions.
        let mut aliases: [usize; MAX_PATTERN_BINDINGS] = std::array::from_fn(|slot| slot);
        // Canonical capture ordinals include the containing pattern's captures.
        // Translate only declarations inside THIS scope to compact local slots.
        // A reference to an outer or undeclared capture must never become NULL.
        let mut capture_slots = BTreeMap::<u32, LocalCapture>::new();
        let mut probe_groups = Vec::new();
        // Even a consumed singleton domain stops eager expansion lookahead.
        // Do not let a second constraint become an earlier route exclusion.
        let mut identity_domain_seen = false;
        let mut operators = ops.iter().enumerate().take(end).skip(start + 1);
        while let Some((at, op)) = operators.next() {
            let bad = || EdgeScanBuildError { operator: at };
            if let GlaOperator::Probe {
                group,
                end: child_end,
                ..
            } = op
                && compound_captures
            {
                // The canonical scope compiler emits nested constraints only
                // after a complete positive witness. A child uses the existing
                // probe compiler, which refuses deeper nesting and foreign
                // capture operands; no alternate matcher or truth cache runs.
                if *child_end as usize <= at + 1
                    || *child_end as usize >= end
                    || *group as usize >= crate::algebra::MAX_PATTERN_IDENTITIES
                    || probe_groups.contains(group)
                    || matches!(ops[start], GlaOperator::Collect { group: parent, .. } if parent == *group)
                {
                    return Err(bad());
                }
                let step = steps.last_mut().ok_or_else(bad)?;
                let (probe, child_end) = Self::compile(ops, at, width)?;
                step.probes.push(probe);
                probe_groups.push(*group);
                for (nested, _) in operators.by_ref() {
                    if nested == child_end {
                        break;
                    }
                }
                continue;
            }
            // Nothing except another complete sibling probe may follow the
            // terminal tail. This also rejects malformed/interleaved bindings.
            if !probe_groups.is_empty() {
                return Err(bad());
            }
            if let GlaOperator::DifferentEdges { segments } = op
                && let [segment] = segments.as_slice()
                && segment.ordinal() as usize == width - 1
                && let Some(Step {
                    binding: Binding::Walk { search, .. },
                    predicates,
                    ..
                }) = steps.last_mut()
                && predicates
                    .iter()
                    .all(|predicate| matches!(predicate, GlaOperator::VertexIdentity { .. }))
            {
                // One atom's identity domain is exactly a TRAIL. Its traversal
                // checks EIds before descent. Probes coalesce endpoint support;
                // collections emit every route occurrence from the same stack.
                // Only collections can retain this route for a later cross-atom
                // domain. Ordinary probes keep their endpoint-support profile.
                match search {
                    GraphWalkSearch::All => *search = GraphWalkSearch::Trail,
                    GraphWalkSearch::Trail | GraphWalkSearch::Acyclic => {}
                    _ => return Err(bad()),
                }
                identity_domain_seen = true;
                continue;
            }
            let mut predicate = None;
            let binding = match op {
                GlaOperator::ScanVertices => Some(Binding::Scan),
                GlaOperator::BindVertex { source } | GlaOperator::BindOuterVertex { source }
                    if (source.ordinal() as usize) < outer_width =>
                {
                    Some(Binding::Copy {
                        source: source.ordinal() as usize,
                        nullable: matches!(op, GlaOperator::BindOuterVertex { .. }),
                    })
                }
                GlaOperator::Expand {
                    source,
                    relation,
                    direction,
                } if (source.ordinal() as usize) < width && expansions < MAX_PATTERN_EDGES => {
                    expansions += 1;
                    Some(Binding::Expand(Expansion {
                        source: source.ordinal() as usize,
                        relation: *relation,
                        direction: *direction,
                    }))
                }
                GlaOperator::VarLengthExpand {
                    source,
                    relation,
                    direction,
                    bounds,
                    search,
                    edge_predicates,
                } if (source.ordinal() as usize) < width
                    && expansions < MAX_PATTERN_EDGES
                    && edge_predicates.is_empty() =>
                {
                    expansions += 1;
                    Some(Binding::Walk {
                        expansion: Expansion {
                            source: source.ordinal() as usize,
                            relation: *relation,
                            direction: *direction,
                        },
                        bounds: *bounds,
                        // At most one hop cannot repeat an EId. The positive
                        // compiler therefore need not emit a singleton domain
                        // for this ALL atom, even inside a mixed TRAIL scope.
                        search: if compound_captures
                            && *search == GraphWalkSearch::All
                            && bounds.maximum() <= 1
                        {
                            GraphWalkSearch::Trail
                        } else {
                            *search
                        },
                    })
                }
                GlaOperator::Select { slot, .. } if (slot.ordinal() as usize) < width => None,
                GlaOperator::VertexIdentity { left, right, equal }
                    if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width =>
                {
                    if *equal {
                        let keep = aliases[left.ordinal() as usize];
                        let merged = aliases[right.ordinal() as usize];
                        for alias in aliases.iter_mut().take(width) {
                            if *alias == merged {
                                *alias = keep;
                            }
                        }
                    }
                    None
                }
                GlaOperator::CompareProperties { left, right, .. }
                    if (left.ordinal() as usize) < width && (right.ordinal() as usize) < width =>
                {
                    None
                }
                GlaOperator::DifferentEdges { segments } => {
                    if segments.is_empty() || segments.len() > MAX_PATTERN_EDGES {
                        return Err(bad());
                    }
                    let has_walk = segments.iter().any(|slot| {
                        (slot.ordinal() as usize)
                            .checked_sub(outer_width)
                            .and_then(|local| steps.get(local))
                            .is_some_and(|step| matches!(step.binding, Binding::Walk { .. }))
                    });
                    if has_walk
                        && (identity_domain_seen
                            || segments
                                .last()
                                .is_none_or(|slot| slot.ordinal() as usize != width - 1)
                            || steps.last().is_none_or(|step| {
                                !step.predicates.iter().all(|predicate| {
                                    matches!(predicate, GlaOperator::VertexIdentity { .. })
                                })
                            }))
                    {
                        // Match eager for_expansion exactly: only identities
                        // may intervene before the first domain, and its last
                        // segment must be this expansion. A later constraint
                        // must not change which prefixes or failures are read.
                        return Err(bad());
                    }
                    for (at, slot) in segments.iter().enumerate() {
                        let step = (slot.ordinal() as usize)
                            .checked_sub(outer_width)
                            .and_then(|local| steps.get_mut(local))
                            .ok_or_else(bad)?;
                        if compound_captures && segments[..at].contains(slot) {
                            return Err(bad());
                        }
                        match &mut step.binding {
                            Binding::Expand(_) => {}
                            Binding::Walk { bounds, search, .. }
                                if compound_captures && bounds.is_bounded() =>
                            {
                                // A collection retains every live occurrence.
                                // DifferentEdges makes this atom a TRAIL even
                                // when an earlier fixed atom shares its domain.
                                match search {
                                    GraphWalkSearch::All
                                        if slot.ordinal() as usize == width - 1 =>
                                    {
                                        *search = GraphWalkSearch::Trail;
                                    }
                                    GraphWalkSearch::Trail => {}
                                    _ => return Err(bad()),
                                }
                            }
                            _ => return Err(bad()),
                        }
                    }
                    identity_domain_seen = true;
                    None
                }
                GlaOperator::CapturePath {
                    capture,
                    start,
                    segments,
                } => {
                    if segments.is_empty()
                        || segments.len() > MAX_PATTERN_EDGES
                        || (!compound_captures && segments.len() != 1)
                        || start.ordinal() as usize >= width
                        || *capture as usize >= crate::algebra::MAX_PATTERN_IDENTITIES
                        || capture_slots.contains_key(capture)
                    {
                        return Err(bad());
                    }
                    let mut endpoint = aliases[start.ordinal() as usize];
                    let mut single_edge = segments.len() == 1;
                    for (at, segment) in segments.iter().enumerate() {
                        let target = segment.ordinal() as usize;
                        let binding = target
                            .checked_sub(outer_width)
                            .and_then(|local| steps.get(local))
                            .map(|step| step.binding)
                            .ok_or_else(bad)?;
                        let expansion = match binding {
                            Binding::Expand(expansion) => expansion,
                            Binding::Walk {
                                expansion,
                                bounds,
                                search: GraphWalkSearch::Trail,
                            } if compound_captures && bounds.is_bounded() => {
                                single_edge = false;
                                expansion
                            }
                            _ => return Err(bad()),
                        };
                        if segments[..at].contains(segment) {
                            return Err(bad());
                        }
                        let from = aliases[expansion.source];
                        let to = aliases[target];
                        if from == endpoint {
                            endpoint = to;
                        } else if compound_captures && to == endpoint {
                            endpoint = from;
                        } else {
                            return Err(bad());
                        }
                    }
                    let local = capture_slots.len() as u32;
                    capture_slots.insert(
                        *capture,
                        LocalCapture {
                            slot: local,
                            single_edge,
                        },
                    );
                    predicate = Some(GlaOperator::CapturePath {
                        capture: local,
                        start: *start,
                        segments: segments.clone(),
                    });
                    None
                }
                GlaOperator::SelectBoolean { expression } => {
                    let mut vertices_valid = true;
                    let mut captures_valid = true;
                    let expression = expression.remap_elements(
                        |slot| {
                            vertices_valid &= (slot.ordinal() as usize) < width;
                            slot
                        },
                        |capture| match capture_slots.get(&capture) {
                            Some(local) if local.single_edge => local.slot,
                            _ => {
                                captures_valid = false;
                                capture
                            }
                        },
                    );
                    if !vertices_valid || !captures_valid {
                        return Err(bad());
                    }
                    predicate = Some(GlaOperator::SelectBoolean { expression });
                    None
                }
                _ => return Err(bad()),
            };
            if let Some(binding) = binding {
                // The positive compiler emits captures after all bindings.
                // A local capture frame can therefore be rebuilt per complete
                // candidate; it never becomes stale across another expansion.
                if width >= MAX_PATTERN_BINDINGS || !capture_slots.is_empty() {
                    return Err(bad());
                }
                if let Binding::Copy { source, .. } = binding {
                    aliases[width] = aliases[source];
                }
                width += 1;
                identity_domain_seen = false;
                steps.push(Step {
                    binding,
                    predicates: Vec::new(),
                    probes: Vec::new(),
                });
            } else {
                steps
                    .last_mut()
                    .ok_or_else(bad)?
                    .predicates
                    .push(predicate.unwrap_or_else(|| op.clone()));
            }
        }
        Ok((steps, capture_slots))
    }

    pub(crate) fn accepts<S: EdgeScanSource, C>(
        &self,
        outer: &[Option<VId>],
        source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<bool, S::Error, C> {
        Ok(self.witness(outer, source, control, record)? != self.anti)
    }

    fn witness<S: EdgeScanSource, C>(
        &self,
        outer: &[Option<VId>],
        source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<bool, S::Error, C> {
        self.visit_witnesses(
            outer,
            source,
            walk::Emission::Support,
            control,
            record,
            &mut |_, _, _| Ok(true),
        )
    }

    /// Visit complete witnesses in native identified-edge enumeration order.
    /// Returning true stops at this witness; false resumes the same indexed
    /// traversal. Neither the probe nor the collector builds a neighbor bag.
    fn visit_witnesses<S: EdgeScanSource, C, F>(
        &self,
        outer: &[Option<VId>],
        source: &S,
        emission: walk::Emission,
        control: &mut F,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
        visit: &mut impl FnMut(
            &[Option<VId>],
            &[Option<GraphPath>],
            &mut F,
        ) -> ScanResult<bool, S::Error, C>,
    ) -> ScanResult<bool, S::Error, C>
    where
        F: FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
    {
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
        let mut frames: Vec<_> = self
            .steps
            .iter()
            .map(|step| Position::new(step.binding))
            .collect();
        // Only active variable-length atoms allocate entries. Fixed-hop probes
        // keep their original frame and control-event sequence unchanged.
        let mut walks = BTreeMap::<usize, walk::Endpoints>::new();
        let mut depth = 0;
        loop {
            control(GlaExecutionEvent::Work)?;
            let step = &self.steps[depth];
            let candidate = match step.binding {
                Binding::Copy {
                    source: from,
                    nullable,
                } => {
                    if matches!(frames[depth], Position::Copy(true)) {
                        None
                    } else {
                        frames[depth] = Position::Copy(true);
                        let value = ids[from];
                        if value.is_none() && !nullable {
                            None
                        } else {
                            Some(value)
                        }
                    }
                }
                Binding::Scan => {
                    let Position::Vertex(after) = frames[depth] else {
                        unreachable!("scan position");
                    };
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
                            return Err(GqlQueryError::Source(
                                EdgeScanError::NonIncreasingIdentity,
                            ));
                        }
                        frames[depth] = Position::Vertex(Some(vid));
                        record()?;
                        // Histories may have no visible version at this cut.
                        // That is not a dangling edge and not a NULL witness.
                        if flatten(source.vertex_record(vid, control))?.is_none() {
                            continue;
                        }
                        Some(Some(vid))
                    } else {
                        None
                    }
                }
                Binding::Expand(expansion) => {
                    let Some(from) = ids[expansion.source] else {
                        if depth == 0 {
                            return Ok(false);
                        }
                        depth -= 1;
                        ids.pop();
                        continue;
                    };
                    let Position::Edge(after) = frames[depth] else {
                        unreachable!("edge position");
                    };
                    let next = source.next_incident_edge_for_relation(
                        from,
                        expansion.relation,
                        expansion.direction,
                        after,
                        control,
                    );
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
                            return Err(GqlQueryError::Source(
                                EdgeScanError::NonIncreasingIdentity,
                            ));
                        }
                        frames[depth] = Position::Edge(Some(eid));
                        record()?;
                        let Some(to) = resolve_target(source, eid, from, expansion, control)?
                        else {
                            continue;
                        };
                        Some(Some(to))
                    } else {
                        None
                    }
                }
                Binding::Walk {
                    expansion,
                    bounds,
                    search,
                } => {
                    match ids[expansion.source] {
                        None => None, // Even *0 cannot turn a NULL correlation into a vertex.
                        Some(from) => {
                            if let Some(walk) = walks.get_mut(&depth) {
                                walk.next(expansion, source, control, record)?.map(Some)
                            } else {
                                // Build outside the map's mutable entry borrow:
                                // earlier live routes supply this atom's domain.
                                let mut cursor = match emission {
                                    walk::Emission::Support => {
                                        walk::Endpoints::new(from, bounds, search, source, control)?
                                    }
                                    walk::Emission::TrailOccurrences => {
                                        debug_assert_eq!(search, GraphWalkSearch::Trail);
                                        walk::Endpoints::trail_occurrences(
                                            from, bounds, source, control,
                                        )?
                                    }
                                };
                                if emission == walk::Emission::TrailOccurrences {
                                    // Prune earlier segment identities before
                                    // descending this TRAIL, as eager GLA
                                    // does. Filtering only its endpoints
                                    // would still visit forbidden suffixes.
                                    if let Some(GlaOperator::DifferentEdges { segments }) =
                                        step.predicates.iter().find(|predicate| {
                                            !matches!(predicate, GlaOperator::VertexIdentity { .. })
                                        })
                                        && segments.last().is_some_and(|slot| {
                                            slot.ordinal() as usize == self.outer_width + depth
                                        })
                                    {
                                        for slot in &segments[..segments.len() - 1] {
                                            let local = slot.ordinal() as usize - self.outer_width;
                                            match frames[local] {
                                                Position::Edge(Some(eid)) => {
                                                    cursor.forbid(eid, control)?;
                                                }
                                                Position::Walk => {
                                                    let previous =
                                                        walks.get(&local).ok_or_else(|| {
                                                            GqlQueryError::Source(
                                                                EdgeScanError::BoundEdgeUnavailable,
                                                            )
                                                        })?;
                                                    cursor.forbid_route(previous, control)?;
                                                }
                                                _ => {
                                                    return Err(GqlQueryError::Source(
                                                        EdgeScanError::BoundEdgeUnavailable,
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                                control(GlaExecutionEvent::ScratchEntry)?;
                                walks
                                    .entry(depth)
                                    .or_insert(cursor)
                                    .next(expansion, source, control, record)?
                                    .map(Some)
                            }
                        }
                    }
                }
            };
            let Some(candidate) = candidate else {
                walks.remove(&depth);
                if depth == 0 {
                    return Ok(false);
                }
                depth -= 1;
                ids.pop();
                continue;
            };
            ids.push(candidate);
            let Some(paths) = self.test_candidate(
                &step.predicates,
                &ids,
                &frames,
                Some(&walks),
                source,
                control,
            )?
            else {
                ids.pop();
                continue;
            };
            let mut accepted = true;
            for probe in &step.probes {
                control(GlaExecutionEvent::Work)?;
                if !probe.accepts(&ids, source, control, record)? {
                    accepted = false;
                    break;
                }
            }
            if !accepted {
                ids.pop();
                continue;
            }
            depth += 1;
            if depth == self.steps.len() {
                control(GlaExecutionEvent::Work)?;
                if visit(&ids, &paths, control)? {
                    return Ok(true);
                }
                depth -= 1;
                ids.pop();
            } else {
                frames[depth] = Position::new(self.steps[depth].binding);
                walks.remove(&depth); // A new correlated prefix gets fresh atom-local history.
            }
        }
    }

    // One predicate/capture kernel for synchronous and awaitable probes. This
    // does not perform expansion: the caller supplies already selected frames.
    fn test_candidate<S: EdgeScanSource, C>(
        &self,
        predicates: &[GlaOperator],
        ids: &[Option<VId>],
        frames: &[Position],
        walks: Option<&BTreeMap<usize, walk::Endpoints>>,
        source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<Option<Vec<Option<GraphPath>>>, S::Error, C> {
        let mut passed = true;
        let mut paths = Vec::new();
        for predicate in predicates {
            control(GlaExecutionEvent::Work)?;
            if let GlaOperator::DifferentEdges { segments } = predicate {
                for (index, slot) in segments.iter().enumerate() {
                    let local = slot.ordinal() as usize - self.outer_width;
                    if !matches!(frames[local], Position::Edge(Some(_)) | Position::Walk) {
                        return Err(GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable));
                    }
                    for prior in &segments[..index] {
                        control(GlaExecutionEvent::Work)?;
                        let prior = prior.ordinal() as usize - self.outer_width;
                        let repeated = match (frames[local], frames[prior]) {
                            (Position::Edge(Some(eid)), Position::Edge(Some(previous))) => {
                                previous == eid
                            }
                            (Position::Walk, Position::Edge(Some(eid)))
                            | (Position::Edge(Some(eid)), Position::Walk) => {
                                let route = if matches!(frames[local], Position::Walk) {
                                    local
                                } else {
                                    prior
                                };
                                walks
                                    .and_then(|walks| walks.get(&route))
                                    .ok_or_else(|| {
                                        GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                                    })?
                                    .contains_edge(eid)
                            }
                            (Position::Walk, Position::Walk) => {
                                let routes = walks.ok_or_else(|| {
                                    GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                                })?;
                                let current = routes.get(&local).ok_or_else(|| {
                                    GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                                })?;
                                let previous = routes.get(&prior).ok_or_else(|| {
                                    GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                                })?;
                                current.shares_edge(previous, control)?
                            }
                            _ => {
                                return Err(GqlQueryError::Source(
                                    EdgeScanError::BoundEdgeUnavailable,
                                ));
                            }
                        };
                        if repeated {
                            passed = false;
                            break;
                        }
                    }
                    if !passed {
                        break;
                    }
                }
                if !passed {
                    break;
                }
                continue;
            }
            if let GlaOperator::CapturePath {
                capture,
                start,
                segments,
            } = predicate
            {
                // Retain each chosen EId, never reconstruct a relationship by
                // endpoints. The declared path can traverse an expansion in
                // reverse, including an edge closing onto an existing slot.
                let Some(Some(start)) = ids.get(start.ordinal() as usize) else {
                    return Err(GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable));
                };
                debug_assert_eq!(*capture as usize, paths.len());
                let local = segments[0].ordinal() as usize - self.outer_width;
                if segments.len() == 1 && matches!(self.steps[local].binding, Binding::Walk { .. })
                {
                    // This capture owns exactly one admitted finite TRAIL
                    // atom. The offered occurrence still owns its exact route;
                    // endpoint support and async probes cannot supply it.
                    debug_assert_eq!(segments.len(), 1);
                    let walk = walks.and_then(|walks| walks.get(&local)).ok_or_else(|| {
                        GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                    })?;
                    paths.push(Some(walk.capture(*start, control)?));
                    continue;
                }
                // Charge the path slot and every step before reserving payload.
                // A one-edge capture retains its original three scratch events.
                control(GlaExecutionEvent::ScratchEntry)?;
                let mut count = 0;
                for segment in segments {
                    let local = segment.ordinal() as usize - self.outer_width;
                    let length = if matches!(self.steps[local].binding, Binding::Walk { .. }) {
                        control(GlaExecutionEvent::Work)?;
                        walks
                            .and_then(|walks| walks.get(&local))
                            .ok_or_else(|| {
                                GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                            })?
                            .capture_len()
                    } else {
                        1
                    };
                    count += length;
                    for _ in 0..length {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        control(GlaExecutionEvent::ScratchEntry)?;
                    }
                }
                let mut endpoint = *start;
                let mut steps = Vec::with_capacity(count);
                for (at, segment) in segments.iter().enumerate() {
                    // The predicate checkpoint already accounts for step zero.
                    if at != 0 {
                        control(GlaExecutionEvent::Work)?;
                    }
                    let target = segment.ordinal() as usize;
                    let local = target - self.outer_width;
                    let expansion = match self.steps[local].binding {
                        Binding::Expand(expansion) => expansion,
                        Binding::Walk { .. } => {
                            endpoint = walks
                                .and_then(|walks| walks.get(&local))
                                .ok_or_else(|| {
                                    GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable)
                                })?
                                .append_capture(endpoint, &mut steps, control)?;
                            continue;
                        }
                        _ => unreachable!("the compiler admitted only local expansion segments"),
                    };
                    let (Some(Position::Edge(Some(eid))), Some(Some(from)), Some(Some(to))) = (
                        frames.get(local),
                        ids.get(expansion.source),
                        ids.get(target),
                    ) else {
                        return Err(GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable));
                    };
                    endpoint = if *from == endpoint {
                        *to
                    } else if *to == endpoint {
                        *from
                    } else {
                        return Err(GqlQueryError::Source(EdgeScanError::BoundEdgeUnavailable));
                    };
                    steps.push((*eid, endpoint));
                }
                paths.push(Some(GraphPath::new(*start, steps.into_boxed_slice())));
                continue;
            }
            if !super::accepts(predicate, ids, &paths, source, control)? {
                passed = false;
                break;
            }
        }
        Ok(passed.then_some(paths))
    }
}

impl Collection {
    pub(crate) fn compile(
        ops: &[GlaOperator],
        start: usize,
        outer_width: usize,
    ) -> Result<(Self, usize), EdgeScanBuildError> {
        let bad = || EdgeScanBuildError { operator: start };
        let GlaOperator::Collect { group, end, slots } = &ops[start] else {
            return Err(bad());
        };
        let end = *end as usize;
        if end <= start + 1 || *group as usize >= crate::algebra::MAX_PATTERN_IDENTITIES {
            return Err(bad());
        }
        let Some(GlaOperator::CollectEnd {
            group: close,
            value,
            tuple,
        }) = ops.get(end)
        else {
            return Err(bad());
        };
        if close != group {
            return Err(bad());
        }
        // Fixed scopes advance each local scan in VId order, then fixed hops
        // in EId order, like identified eager scopes. Disconnected components
        // keep separate positions even with copied outer predicate operands.
        // Correlated finite TRAIL atoms preserve nested DFS route occurrences,
        // interleaved with fixed EId choices. Each complete earlier occurrence
        // explores the following atoms before advancing. A single TRAIL may
        // have fixed hops and one independent declared start. Multiple TRAILs
        // currently retain their correlated root.
        // Fixed parents may have terminal child probes. Only direct parent
        // scans and expansion atoms determine the collection profile; child
        // finite walks keep their existing endpoint-support EXISTS semantics.
        // Children test each fixed/TRAIL occurrence in separate Support
        // frames. Rejection never advances or coalesces the paused parent
        // route; every accepted route still contributes one list element.
        let mut fixed = 0;
        let mut walks = 0;
        let mut scans = 0;
        let mut at = start + 1;
        while at < end {
            match &ops[at] {
                GlaOperator::Expand { .. } => fixed += 1,
                GlaOperator::ScanVertices => scans += 1,
                GlaOperator::VarLengthExpand { bounds, .. } if bounds.is_bounded() => walks += 1,
                GlaOperator::VarLengthExpand { .. } => {
                    return Err(EdgeScanBuildError { operator: at });
                }
                GlaOperator::Probe { end: child_end, .. } => {
                    if *child_end as usize <= at + 1 || *child_end as usize >= end {
                        return Err(EdgeScanBuildError { operator: at });
                    }
                    at = *child_end as usize;
                }
                _ => {}
            }
            at += 1;
        }
        if (fixed == 0 && walks == 0)
            || fixed + walks > MAX_PATTERN_EDGES
            || (walks != 0 && scans != 0 && (walks != 1 || scans > 1))
        {
            return Err(bad());
        }
        let (steps, captures) = Probe::compile_body(ops, start, end, outer_width, true)?;
        // Ordinary openCypher DifferentEdges lowers an ALL atom to TRAIL in
        // compile_body, including mixed domains. Other modes have different
        // multiplicity or enumeration order. Captures use the live route.
        if walks != 0
            && steps.iter().any(|step| {
                matches!(step.binding, Binding::Walk { search, .. }
                    if search != GraphWalkSearch::Trail)
            })
        {
            return Err(bad());
        }
        if *slots as usize != steps.len() {
            return Err(bad());
        }
        let width = outer_width + steps.len();
        let remap = |value: &ValueProjection| match *value {
            ValueProjection::Vertex { slot } if (slot.ordinal() as usize) < width => Ok(*value),
            ValueProjection::Path { capture, function }
                if matches!(function, GraphPathFunction::Value | GraphPathFunction::Edge) =>
            {
                captures
                    .get(&capture)
                    .filter(|capture| function != GraphPathFunction::Edge || capture.single_edge)
                    .map(|capture| ValueProjection::Path {
                        capture: capture.slot,
                        function,
                    })
                    .ok_or_else(bad)
            }
            _ => Err(bad()),
        };
        let value = value.as_ref().map(remap).transpose()?;
        let tuple = tuple
            .as_ref()
            .map(|columns| {
                if columns.is_empty() || columns.len() > crate::algebra::MAX_PATTERN_VERTICES {
                    return Err(bad());
                }
                columns.iter().map(remap).collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        if value.is_some() && tuple.is_some() {
            return Err(bad());
        }
        Ok((
            Self {
                group: *group as usize,
                probe: Probe {
                    anti: false,
                    outer_width,
                    steps,
                },
                value,
                tuple,
            },
            end,
        ))
    }

    pub(crate) fn evaluate<S: EdgeScanSource, C>(
        &self,
        outer: &[Option<VId>],
        source: &S,
        control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
        record: &mut impl FnMut() -> ScanResult<(), S::Error, C>,
    ) -> ScanResult<crate::algebra::GraphValue, S::Error, C> {
        use crate::algebra::GraphValue;
        use crate::algebra_exec::collect_witness_element;

        let mut values = Vec::new();
        let nodes = self.tuple.as_ref().map_or(1, |columns| columns.len() + 1);
        self.probe.visit_witnesses(
            outer,
            source,
            walk::Emission::TrailOccurrences,
            control,
            record,
            &mut |bindings, paths, control| {
                // The final value bound is known before any witness allocation.
                // Every tuple/path payload is then admitted by the same kernel
                // as the eager CollectEnd instruction, under the source meter.
                if values.len() >= (GraphValue::MAX_LIST_NODES - 1) / nodes {
                    return Err(crate::GraphIntegerError {
                        instruction: 0,
                        kind: crate::GraphIntegerErrorKind::CollectionBounds,
                    }
                    .into());
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                let value = if let Some(columns) = &self.tuple {
                    let mut fields = Vec::new();
                    for column in columns {
                        control(GlaExecutionEvent::Work)?;
                        control(GlaExecutionEvent::ScratchEntry)?;
                        fields.push(collect_witness_element(
                            Some(column),
                            bindings,
                            paths,
                            control,
                        )?);
                    }
                    GraphValue::List(fields.into_boxed_slice())
                } else {
                    collect_witness_element(self.value.as_ref(), bindings, paths, control)?
                };
                values.push(value);
                Ok(false)
            },
        )?;
        control(GlaExecutionEvent::ScratchEntry)?;
        Ok(GraphValue::List(values.into_boxed_slice()))
    }
}

// Both fixed and variable atoms resolve historical incidence through the SAME
// visible edge and endpoint readers. Candidate indexes never establish liveness.
fn resolve_target<S: EdgeScanSource, C>(
    source: &S,
    eid: EId,
    from: VId,
    expansion: Expansion,
    control: &mut impl FnMut(GlaExecutionEvent) -> ScanResult<(), S::Error, C>,
) -> ScanResult<Option<VId>, S::Error, C> {
    let Some(edge) = flatten(source.edge(eid, control))? else {
        return Ok(None);
    };
    if !expansion.relation.matches(edge.relation) {
        return Ok(None);
    }
    let to = match expansion.direction {
        GlaDirection::Forward if edge.source == from => edge.target,
        GlaDirection::Reverse if edge.target == from => edge.source,
        GlaDirection::Undirected if edge.source == from => edge.target,
        GlaDirection::Undirected if edge.target == from => edge.source,
        _ => return Ok(None),
    };
    vertex(source, from, control)?;
    if from != to {
        vertex(source, to, control)?;
    }
    Ok(Some(to))
}

#[cfg(test)]
mod captured_tests;
