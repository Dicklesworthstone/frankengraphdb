//! Edge-identity-preserving TRAIL traversal over one admitted adjacency index.
//!
//! Vertices may repeat; an EId may not. Used-edge membership is path-local,
//! never endpoint settlement or a post-filter over unrestricted WALK results.

use crate::algebra::GraphPath;
use crate::{GlaExecutionEvent, GraphWalkBounds};
use fgdb_types::{EId, VId};
use std::collections::BTreeMap;

struct Frame<'a> {
    vertex: VId,
    incoming: Option<EId>,
    neighbors: &'a [(EId, VId)],
    next: usize,
    emitted: bool,
}

/// Iterative, finite TRAIL expansion with real edge identities.
///
/// The host supplies an immutable, authorized relation/orientation index whose
/// neighbor slices are sorted by (EId, VId). Parallel edges remain distinct;
/// traversing an undirected edge backwards still uses the SAME EId. A self-loop
/// appears once in its undirected adjacency. No identities are synthesized.
/// Duplicate source occurrences retain their multiplicity.
///
/// Only one path frontier is retained, not a breadth-first layer or a copied
/// used-edge set per prefix. A closed trail may continue on unused edges.
/// Final predicates, DISTINCT, ranking and pagination belong to the host GLA
/// plan. Rejected endpoints do not prevent continuation through that vertex.
///
/// Both pull methods advance the same cursor in depth-first index order. The
/// endpoint method avoids allocating an owned path; the path method reserves
/// every copied identity before returning it. Every error permanently exhausts
/// the cursor and releases its frontier allocation. Controls are logical work
/// and scratch events, not allocator-byte or peak-memory measurements.
pub struct GraphTrailCursor<'a> {
    adjacency: Option<&'a BTreeMap<VId, Vec<(EId, VId)>>>,
    bounds: GraphWalkBounds,
    stack: Vec<Frame<'a>>,
}

impl<'a> GraphTrailCursor<'a> {
    pub fn new<E>(
        source: VId,
        bounds: GraphWalkBounds,
        adjacency: Option<&'a BTreeMap<VId, Vec<(EId, VId)>>>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, E> {
        control(GlaExecutionEvent::ScratchEntry)?;
        let neighbors = if bounds.maximum() == 0 {
            &[][..]
        } else {
            adjacency
                .and_then(|index| index.get(&source))
                .map_or(&[][..], Vec::as_slice)
        };
        Ok(Self {
            adjacency,
            bounds,
            stack: vec![Frame {
                vertex: source,
                incoming: None,
                neighbors,
                next: 0,
                emitted: false,
            }],
        })
    }

    pub fn next_endpoint_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        let result = self.advance(control);
        if result.is_err() || matches!(&result, Ok(None)) {
            self.stack = Vec::new();
        }
        result
    }

    pub fn next_with_control<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphPath>, E> {
        let result = (|| {
            if self.advance(control)?.is_none() {
                return Ok(None);
            }
            // Admit the owned result and all identity pairs before allocation.
            control(GlaExecutionEvent::ScratchEntry)?;
            for _ in 1..self.stack.len() {
                control(GlaExecutionEvent::Work)?;
                control(GlaExecutionEvent::ScratchEntry)?;
                control(GlaExecutionEvent::ScratchEntry)?;
            }
            let source = self.stack[0].vertex;
            let steps = self
                .stack
                .iter()
                .skip(1)
                .map(|frame| {
                    (
                        frame.incoming.expect("non-root trail frame has an edge"),
                        frame.vertex,
                    )
                })
                .collect::<Vec<_>>();
            Ok(Some(GraphPath::new(source, steps.into_boxed_slice())))
        })();
        if result.is_err() || matches!(&result, Ok(None)) {
            self.stack = Vec::new();
        }
        result
    }

    fn advance<E>(
        &mut self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<VId>, E> {
        while !self.stack.is_empty() {
            control(GlaExecutionEvent::Work)?;
            let depth = self.stack.len() - 1;
            let frame = &mut self.stack[depth];
            if !frame.emitted {
                frame.emitted = true;
                if depth as u32 >= self.bounds.minimum() {
                    return Ok(Some(frame.vertex));
                }
            }
            if depth as u32 == self.bounds.maximum() || frame.next == frame.neighbors.len() {
                let _ = self.stack.pop();
                continue;
            }
            let (edge, destination) = frame.neighbors[frame.next];
            frame.next += 1;
            let mut repeated = false;
            for prior in self.stack.iter().skip(1) {
                control(GlaExecutionEvent::Work)?;
                if prior.incoming == Some(edge) {
                    repeated = true;
                    break;
                }
            }
            if repeated {
                continue;
            }
            // No descendant allocation, path copy, or source re-read for a
            // repeated edge. Returning to a prior vertex is deliberately legal.
            control(GlaExecutionEvent::ScratchEntry)?;
            let neighbors = if depth as u32 + 1 == self.bounds.maximum() {
                &[][..]
            } else {
                self.adjacency
                    .and_then(|index| index.get(&destination))
                    .map_or(&[][..], Vec::as_slice)
            };
            self.stack.push(Frame {
                vertex: destination,
                incoming: Some(edge),
                neighbors,
                next: 0,
                emitted: false,
            });
        }
        Ok(None)
    }
}

impl core::fmt::Debug for GraphTrailCursor<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphTrailCursor")
            .field("bounds", &self.bounds)
            .field("frontier_depth", &self.stack.len())
            .field("graph", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    type Index = BTreeMap<VId, Vec<(EId, VId)>>;
    type Route = (VId, Vec<(EId, VId)>);

    fn collect<E>(
        source: VId,
        bounds: GraphWalkBounds,
        index: &Index,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<Route>, E> {
        let mut cursor = GraphTrailCursor::new(source, bounds, Some(index), control)?;
        let mut result = Vec::new();
        while let Some(path) = cursor.next_with_control(control)? {
            result.push((path.start(), path.steps().to_vec()));
        }
        Ok(result)
    }

    #[test]
    fn exhaustive_trails_equal_unpruned_walks_filtered_by_actual_edge_ids() {
        // The oracle retains every walk, including invalid prefixes, then
        // filters its complete EId list. No DFS/frontier code is shared.
        let edges = [(1, 0, 0), (2, 0, 1), (3, 0, 1), (4, 1, 0), (5, 1, 1)];
        for mask in 0..32 {
            for direction in 0..3 {
                let mut index = Index::new();
                for (at, &(edge, from, to)) in edges.iter().enumerate() {
                    if mask & (1 << at) == 0 {
                        continue;
                    }
                    let (from, to) = if direction == 1 {
                        (to, from)
                    } else {
                        (from, to)
                    };
                    index
                        .entry(VId(from))
                        .or_default()
                        .push((EId(edge), VId(to)));
                    if direction == 2 && from != to {
                        index
                            .entry(VId(to))
                            .or_default()
                            .push((EId(edge), VId(from)));
                    }
                }
                for values in index.values_mut() {
                    values.sort();
                }
                for source in [VId(0), VId(1), VId(2)] {
                    for maximum in 0..=3 {
                        for minimum in 0..=maximum {
                            let bounds = GraphWalkBounds::new(minimum, maximum).unwrap();
                            let mut all = vec![(source, Vec::<(EId, VId)>::new())];
                            let mut expected = Vec::new();
                            for depth in 0..=maximum {
                                if depth >= minimum {
                                    expected.extend(
                                        all.iter()
                                            .filter(|(_, steps)| {
                                                steps
                                                    .iter()
                                                    .map(|step| step.0)
                                                    .collect::<BTreeSet<_>>()
                                                    .len()
                                                    == steps.len()
                                            })
                                            .cloned(),
                                    );
                                }
                                let mut next = Vec::new();
                                for (start, steps) in all {
                                    let endpoint = steps.last().map_or(start, |step| step.1);
                                    for &step in index.get(&endpoint).into_iter().flatten() {
                                        let mut child = steps.clone();
                                        child.push(step);
                                        next.push((start, child));
                                    }
                                }
                                all = next;
                            }
                            expected.sort();
                            let mut actual =
                                collect(source, bounds, &index, &mut |_| Ok::<_, ()>(())).unwrap();
                            actual.sort();
                            assert_eq!(
                                actual, expected,
                                "mask={mask} direction={direction} {source:?} {bounds:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn closed_trails_continue_and_undirected_edges_cannot_be_reused() {
        let index = Index::from([
            (VId(1), vec![(EId(11), VId(2)), (EId(13), VId(3))]),
            (VId(2), vec![(EId(12), VId(1))]),
        ]);
        assert_eq!(
            collect(
                VId(1),
                GraphWalkBounds::new(3, 3).unwrap(),
                &index,
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap(),
            vec![(
                VId(1),
                vec![(EId(11), VId(2)), (EId(12), VId(1)), (EId(13), VId(3))]
            )]
        );
        let one = Index::from([
            (VId(1), vec![(EId(11), VId(2))]),
            (VId(2), vec![(EId(11), VId(1))]),
        ]);
        assert!(
            collect(
                VId(1),
                GraphWalkBounds::new(2, 3).unwrap(),
                &one,
                &mut |_| Ok::<_, ()>(())
            )
            .unwrap()
            .is_empty()
        );
        let parallel = Index::from([
            (VId(1), vec![(EId(11), VId(2)), (EId(12), VId(2))]),
            (VId(2), vec![(EId(11), VId(1)), (EId(12), VId(1))]),
        ]);
        let paths = collect(
            VId(1),
            GraphWalkBounds::new(2, 2).unwrap(),
            &parallel,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(paths.len(), 2);
        assert!(
            paths
                .iter()
                .all(|(_, path)| path[0].0 != path[1].0 && path[1].1 == VId(1))
        );
    }

    #[test]
    fn endpoint_pulls_keep_multiplicity_without_owned_path_copies() {
        let index = Index::from([(VId(1), vec![(EId(7), VId(1)), (EId(8), VId(1))])]);
        let bounds = GraphWalkBounds::new(0, 3).unwrap();
        let mut path_scratch = 0;
        let paths = collect(VId(1), bounds, &index, &mut |event| {
            path_scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, ()>(())
        })
        .unwrap();
        let mut endpoint_scratch = 0;
        let mut control = |event| {
            endpoint_scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
            Ok::<_, ()>(())
        };
        let mut cursor = GraphTrailCursor::new(VId(1), bounds, Some(&index), &mut control).unwrap();
        let mut endpoints = Vec::new();
        while let Some(endpoint) = cursor.next_endpoint_with_control(&mut control).unwrap() {
            endpoints.push(endpoint);
        }
        assert_eq!(
            endpoints,
            paths
                .iter()
                .map(|(start, steps)| steps.last().map_or(*start, |s| s.1))
                .collect::<Vec<_>>()
        );
        assert_eq!(endpoints, vec![VId(1); 5]);
        assert!(endpoint_scratch < path_scratch);
        assert_eq!(cursor.stack.capacity(), 0);
    }

    #[test]
    fn every_control_refusal_is_terminal_including_result_copy_and_membership() {
        let index = Index::from([
            (VId(1), vec![(EId(7), VId(1)), (EId(8), VId(2))]),
            (VId(2), vec![(EId(8), VId(1)), (EId(9), VId(1))]),
        ]);
        let bounds = GraphWalkBounds::new(0, 3).unwrap();
        for endpoint_only in [false, true] {
            let mut total = 0;
            let mut control = |_| {
                total += 1;
                Ok::<_, usize>(())
            };
            let mut cursor =
                GraphTrailCursor::new(VId(1), bounds, Some(&index), &mut control).unwrap();
            loop {
                let more = if endpoint_only {
                    cursor
                        .next_endpoint_with_control(&mut control)
                        .unwrap()
                        .is_some()
                } else {
                    cursor.next_with_control(&mut control).unwrap().is_some()
                };
                if !more {
                    break;
                }
            }
            for stop in 1..=total {
                let mut calls = 0;
                let mut control = |_| {
                    calls += 1;
                    if calls == stop { Err(stop) } else { Ok(()) }
                };
                let result = GraphTrailCursor::new(VId(1), bounds, Some(&index), &mut control);
                let Ok(mut cursor) = result else {
                    assert_eq!(stop, 1);
                    continue;
                };
                loop {
                    let result = if endpoint_only {
                        cursor
                            .next_endpoint_with_control(&mut control)
                            .map(|v| v.is_some())
                    } else {
                        cursor.next_with_control(&mut control).map(|v| v.is_some())
                    };
                    if result.is_err() {
                        assert_eq!(result, Err(stop));
                        break;
                    }
                    assert_eq!(result, Ok(true), "must encounter the selected boundary");
                }
                assert_eq!(calls, stop);
                assert_eq!(cursor.stack.capacity(), 0);
                assert_eq!(
                    cursor
                        .next_endpoint_with_control(&mut |_| -> Result<(), usize> {
                            panic!("refused cursor resumed")
                        })
                        .unwrap(),
                    None
                );
                assert_eq!(
                    cursor
                        .next_with_control(&mut |_| -> Result<(), usize> {
                            panic!("refused path copy resumed")
                        })
                        .unwrap(),
                    None
                );
            }
        }
    }

    #[test]
    fn finite_depth_is_iterative_and_repeated_edge_prefixes_stop_early() {
        let maximum = crate::MAX_GRAPH_WALK_HOPS;
        let mut chain = Index::new();
        for at in 0..maximum {
            chain.insert(
                VId(u128::from(at)),
                vec![(EId(u128::from(at)), VId(u128::from(at) + 1))],
            );
        }
        let paths = collect(
            VId(0),
            GraphWalkBounds::new(maximum, maximum).unwrap(),
            &chain,
            &mut |_| Ok::<_, ()>(()),
        )
        .unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].1.len(), maximum as usize);
        let looped = Index::from([(VId(1), vec![(EId(7), VId(1))])]);
        let mut calls = 0;
        assert!(
            collect(
                VId(1),
                GraphWalkBounds::new(maximum, maximum).unwrap(),
                &looped,
                &mut |_| {
                    calls += 1;
                    assert!(
                        calls < 20,
                        "must reject repeated prefixes before the hop bound"
                    );
                    Ok::<_, ()>(())
                }
            )
            .unwrap()
            .is_empty()
        );
        for source in [VId(0), VId(u128::MAX)] {
            assert_eq!(
                collect(
                    source,
                    GraphWalkBounds::new(0, 0).unwrap(),
                    &Index::new(),
                    &mut |_| Ok::<_, ()>(())
                )
                .unwrap(),
                vec![(source, vec![])]
            );
        }
    }
}
