//! A leading row bag binds a pattern's slot zero by identity (fgdb-2acff).
//! Admit its visible closure at the statement's cut, then run the unchanged
//! pattern and identity join. An identity supplied by a parameter grants no
//! authority: roots, relations and far endpoints are masked before expansion.

use super::*;
use fgdb_gql::algebra::{GlaDirection, GlaOperator};
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
    // Property seeds stay unseeded here (fgdb-2npty): postings of a property
    // the scope hides would decide which roots are admitted and charged, so
    // hidden data would change the native work (FG-INV-20).
    let fgdb_gql::GraphSeeds::Identities(seeds) = seeds else {
        return None;
    };
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
        // Retain no hidden-identity cache: its size would expose hidden
        // topology through the scratch budget. Visible nodes are admitted
        // immediately, so a large reach cannot defer the signed node limit.
        control(SourceEvent::ScratchEntry)?;
        self.tables.admit_vertex(row, scope, node, control)?;
        self.vertices.insert(vid);
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
