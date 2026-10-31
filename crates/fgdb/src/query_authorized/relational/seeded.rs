//! A leading row bag binds a pattern's slot zero by identity or a visible
//! property (fgdb-2acff, fgdb-2npty).
//! Admit its visible closure at the statement's cut, then run the unchanged
//! pattern and identity join. An identity supplied by a parameter grants no
//! authority: roots, relations and far endpoints are masked before expansion.

use super::*;
use fgdb_gql::algebra::{GlaDirection, GlaOperator, IntegerComparison};
use std::collections::BTreeSet;

/// Decline unsupported definitions before doing any source work. Once this
/// path starts, including on a limit or cancellation error, it never falls
/// back to a full scan with a fresh allowance.
pub(super) fn pattern_at<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    seeds: fgdb_gql::GraphSeeds<'_>,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
) -> Option<Governed<GraphValueRow>> {
    pattern_at_with_provenance(snapshot, at, pattern, seeds, scope, execution, policy, None)
}

/// Prepared retrieval may lend property reads for this pattern's admitted
/// visible closure. Retain those identities only after their ordinary source
/// admission, and charge the additional provenance storage on this SAME
/// statement allowance before insertion. Ordinary reads keep their unchanged
/// admission/accounting by leaving the sink absent.
#[allow(clippy::too_many_arguments)]
pub(super) fn pattern_at_with_provenance<Clock: FnMut() -> u64>(
    snapshot: &Arc<Snapshot>,
    at: CommitSeq,
    pattern: &PreparedGraphPattern<GraphValueRow>,
    seeds: fgdb_gql::GraphSeeds<'_>,
    scope: &PlannerPredicates,
    execution: &RefCell<Execution<'_, '_, Clock>>,
    policy: GqlQueryPolicy,
    provenance: Option<&RefCell<BTreeSet<ElementId>>>,
) -> Option<Governed<GraphValueRow>> {
    // A hidden field must never choose roots or change native accounting.
    // Keep the ordinary masked source for that definition, before any read.
    if let fgdb_gql::GraphSeeds::Property { key, .. } = seeds
        && !scope.allows_property(key)
    {
        return None;
    }
    let (root, rest) = pattern.plan().operators().split_first()?;
    let steps = match root {
        GlaOperator::ScanVertices => source::bound_edge_steps(rest, 1)?,
        GlaOperator::ScanEdges {
            relation,
            direction,
        } => {
            let mut steps = source::bound_edge_steps(rest, 2)?;
            steps.insert(
                0,
                source::BoundStep {
                    source: 0,
                    relation,
                    direction: *direction,
                    walk: None,
                },
            );
            steps
        }
        _ => return None,
    };
    Some(work_runs(execution, GqlQueryError::Interrupted, || {
        let mut usage = AdmissionUsage::default();
        let mut node = || {
            execution
                .borrow_mut()
                .node()
                .map_err(GqlQueryError::Interrupted)
        };
        // Mixed-scope index/history probes only poll. No hidden row or edge
        // changes a signed or native charge, including closure workspace.
        let mut scan = |_| {
            execution
                .borrow_mut()
                .poll()
                .map_err(GqlQueryError::Interrupted)
        };
        let mut control = |event| {
            execution
                .borrow_mut()
                .checkpoint()
                .map_err(GqlQueryError::Interrupted)?;
            usage.observe(policy, event)
        };
        let mut selected = Selection::new();
        let mut roots = BTreeSet::new();
        match seeds {
            fgdb_gql::GraphSeeds::Identities(seeds) => {
                for &seed in seeds {
                    if selected.vertex(
                        snapshot,
                        seed,
                        at,
                        scope,
                        &mut node,
                        &mut scan,
                        &mut control,
                    )? {
                        retain(&mut roots, seed, &mut control)?;
                    }
                }
            }
            fgdb_gql::GraphSeeds::Property { key, values } => {
                source::visit_property_seed_candidates(
                    snapshot,
                    key,
                    values,
                    &mut scan,
                    |seed, value, scan| {
                        if selected.property_vertex(
                            snapshot,
                            seed,
                            at,
                            scope,
                            key,
                            value,
                            &mut node,
                            scan,
                            &mut control,
                        )? {
                            retain(&mut roots, seed, &mut control)?;
                        }
                        Ok(())
                    },
                )?;
            }
        }
        let mut slots = vec![roots];
        for step in steps {
            let mut reached = BTreeSet::new();
            let mut frontier = BTreeSet::new();
            let mut expanded = BTreeSet::new();
            let (zero, maximum) = step.walk.unwrap_or((false, 1));
            for &near in &slots[step.source] {
                if zero {
                    retain(&mut reached, near, &mut control)?;
                }
                if maximum > 0 {
                    retain(&mut frontier, near, &mut control)?;
                }
            }
            // Breadth-first discovery expands a vertex at its shortest depth.
            // Reaching it later cannot add an edge needed within the maximum.
            // Keep every reached endpoint, including returns to an initial
            // vertex; the evaluator still owns minimum hops and path modes.
            for depth in 0..maximum {
                if frontier.is_empty() {
                    break;
                }
                let mut next = BTreeSet::new();
                for near in frontier {
                    if expanded.contains(&near) {
                        continue;
                    }
                    retain(&mut expanded, near, &mut control)?;
                    control(SourceEvent::Work)?;
                    snapshot.adjacency_index().visit(
                        &snapshot.blocks,
                        near,
                        step.direction,
                        at,
                        &mut scan,
                        |entry, block, row, scan| {
                            if !step.relation.matches(entry.relation)
                                || !scope.allows_relation(entry.relation)
                            {
                                return Ok(());
                            }
                            let far = match step.direction {
                                GlaDirection::Forward => entry.dst,
                                GlaDirection::Reverse => entry.src,
                                GlaDirection::Undirected if entry.src == near => entry.dst,
                                GlaDirection::Undirected => entry.src,
                            };
                            if !selected.vertex(
                                snapshot,
                                far,
                                at,
                                scope,
                                &mut node,
                                scan,
                                &mut control,
                            )? {
                                return Ok(());
                            }
                            // Both endpoints and the relation are visible.
                            // Only now may this incidence affect traversal,
                            // workspace, properties, or logical work counts.
                            control(SourceEvent::Work)?;
                            retain(&mut reached, far, &mut control)?;
                            if depth + 1 < maximum && !expanded.contains(&far) {
                                retain(&mut next, far, &mut control)?;
                            }
                            if let std::collections::btree_map::Entry::Vacant(slot) =
                                selected.edges.entry(entry.eid)
                            {
                                control(SourceEvent::ScratchEntry)?;
                                slot.insert((
                                    (entry.eid, entry.src, entry.relation, entry.dst),
                                    source::edge_properties_at(&snapshot.block_props, block, row),
                                ));
                            }
                            Ok(())
                        },
                    )?;
                }
                frontier = next;
            }
            slots.push(reached);
        }
        // Every edge was selected only after both its endpoints passed the
        // ordinary vertex admission. Seal once, then retain each original EId
        // and its borrowed properties through the existing edge admission.
        drop(slots);
        drop(selected.vertices);
        let mut tables = selected.tables;
        for edge in selected.edges.into_values() {
            tables.admit_closure_edge(edge, scope, true, &mut control)?;
        }
        tables.seal();
        if let Some(provenance) = provenance {
            let mut provenance = provenance.borrow_mut();
            for element in tables
                .vertices
                .iter()
                .map(|(id, _)| ElementId::Vertex(*id))
                .chain(
                    tables
                        .edges
                        .iter()
                        .map(|((id, ..), _)| ElementId::Edge(*id)),
                )
            {
                for _ in 0..=provenance.len().checked_ilog2().unwrap_or(0) {
                    control(SourceEvent::Work)?;
                }
                // Repeated elements still pay the full reservation: a prior
                // CALL cannot warm this set to widen a later source's budget.
                control(SourceEvent::ScratchEntry)?;
                provenance.insert(element);
            }
        }
        tables.metadata(pattern.plan(), scope, &mut control)?;
        execute_tables(
            tables,
            pattern,
            scope,
            policy,
            usage,
            WorkCheckpoint(execution),
        )
    }))
}

struct Selection<'a> {
    tables: Tables<'a>,
    vertices: BTreeSet<VId>,
    edges: BTreeMap<EId, Edge<'a>>,
}

impl<'a> Selection<'a> {
    fn new() -> Self {
        Self {
            tables: Tables::new(),
            vertices: BTreeSet::new(),
            edges: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn vertex(
        &mut self,
        snapshot: &'a Snapshot,
        vid: VId,
        at: CommitSeq,
        scope: &PlannerPredicates,
        node: &mut impl FnMut() -> Result<(), Fault>,
        scan: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<bool, Fault> {
        if self.vertices.contains(&vid) {
            return Ok(true);
        }
        // Match Tables::admit_found: one cancellation poll per identity,
        // independent of the hidden history index's descent shape.
        let row = source::find_vertex(snapshot, vid, at, &mut |_| Ok::<(), Fault>(()))?;
        scan(SourceEvent::Work)?;
        let Some(row) = row else {
            return Ok(false);
        };
        if !scope.allows_vertex(&row.labels) {
            return Ok(false);
        }
        self.admit(row, scope, node, control)
    }

    #[allow(clippy::too_many_arguments)]
    fn property_vertex(
        &mut self,
        snapshot: &'a Snapshot,
        vid: VId,
        at: CommitSeq,
        scope: &PlannerPredicates,
        key: PropertyKeyId,
        value: &CanonicalScalar,
        node: &mut impl FnMut() -> Result<(), Fault>,
        scan: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<bool, Fault> {
        if self.vertices.contains(&vid) {
            return Ok(true);
        }
        let row = source::find_vertex(snapshot, vid, at, &mut |_| Ok::<(), Fault>(()))?;
        scan(SourceEvent::Work)?;
        let Some(row) = row else {
            return Ok(false);
        };
        if !scope.allows_vertex(&row.labels) {
            return Ok(false);
        }
        // Historical postings are never authority. A visible vertex whose
        // old value matched must cost exactly as it does in a database that
        // only ever stored its current, nonmatching value. Inspect the field
        // only after both its key and this winning row have been authorized.
        let current = row
            .props
            .binary_search_by_key(&key, |(key, _)| *key)
            .ok()
            .and_then(|position| row.props[position].1.as_scalar());
        if !IntegerComparison::Equal.accepts_scalar_pair(current, Some(value)) {
            return Ok(false);
        }
        self.admit(row, scope, node, control)
    }

    fn admit(
        &mut self,
        row: &'a VertexRow,
        scope: &PlannerPredicates,
        node: &mut impl FnMut() -> Result<(), Fault>,
        control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
    ) -> Result<bool, Fault> {
        // Retain no hidden-identity cache: its size would expose hidden
        // topology through the scratch budget. Visible nodes are admitted
        // immediately, so a large reach cannot defer the signed node limit.
        control(SourceEvent::ScratchEntry)?;
        self.tables.admit_vertex(row, scope, node, control)?;
        self.vertices.insert(row.vid);
        Ok(true)
    }
}

fn retain(
    vertices: &mut BTreeSet<VId>,
    vertex: VId,
    control: &mut impl FnMut(SourceEvent) -> Result<(), Fault>,
) -> Result<(), Fault> {
    if !vertices.contains(&vertex) {
        control(SourceEvent::ScratchEntry)?;
        vertices.insert(vertex);
    }
    Ok(())
}
