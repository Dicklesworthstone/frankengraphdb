//! Sparse zero/one-hop distances over an admitted, canonically ordered row.
//!
//! The graph owners keep admission, source access, live control and result
//! quotas. This state retains only the distinct non-source neighbors. The
//! source is merged into their ordinal order without copying or sorting them,
//! so result conversion and retained-cursor pulls need no population scan.

#![forbid(unsafe_code)]

use core::mem::size_of;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InvalidRow,
    SizeOverflow,
    AllocationFailed,
    Workspace { requested: usize, limit: usize },
}

pub(crate) fn eligible(cutoff: Option<usize>) -> bool {
    #[cfg(test)]
    if FORCE_DENSE.with(core::cell::Cell::get) {
        return false;
    }
    cutoff.is_some_and(|cutoff| cutoff <= 1)
}

/// A canonical simple row has at most one incidence for every vertex. A
/// source loop is retained in scan work but cannot add a second result row.
pub(crate) fn capacity(nodes: usize, degree: usize, max_rows: usize) -> Result<usize, Error> {
    if nodes == 0 || degree > nodes {
        return Err(Error::InvalidRow);
    }
    Ok(degree.min(nodes - 1).min(max_rows.saturating_sub(1)))
}

pub(crate) fn workspace(capacity: usize) -> Result<usize, Error> {
    capacity
        .checked_mul(size_of::<usize>())
        .ok_or(Error::SizeOverflow)
}

/// Count the source row and possible output, plus owner-supplied physical
/// access work. Neither initialization nor output contributes a V-wide pass.
/// Lookup, raw history and direction costs belong in `source_work`.
pub(crate) fn work(
    nodes: usize,
    degree: usize,
    columns: usize,
    source_work: usize,
) -> Result<usize, Error> {
    if nodes == 0 || degree > nodes {
        return Err(Error::InvalidRow);
    }
    let rows = if degree == nodes { nodes } else { degree + 1 };
    columns
        .checked_add(4)
        .and_then(|per_row| rows.checked_mul(per_row))
        .and_then(|output| output.checked_add(degree))
        .and_then(|work| work.checked_add(source_work))
        .and_then(|work| work.checked_add(16))
        .ok_or(Error::SizeOverflow)
}

/// Only the completed result is retained by a public row cursor. Private
/// fields prevent callers from substituting unordered or duplicate ordinals.
pub(crate) struct Distances {
    source: usize,
    source_position: usize,
    neighbors: Vec<usize>,
}

impl Distances {
    pub(crate) fn len(&self) -> usize {
        // Search admits a valid source and at most nodes-1 other vertices.
        self.neighbors.len() + 1
    }

    pub(crate) fn get(&self, index: usize) -> Option<(usize, usize)> {
        if index == self.source_position {
            Some((self.source, 0))
        } else {
            let offset = if index < self.source_position {
                index
            } else {
                index.checked_sub(1)?
            };
            self.neighbors
                .get(offset)
                .copied()
                .map(|vertex| (vertex, 1))
        }
    }
}

pub(crate) struct Search {
    nodes: usize,
    maximum_neighbors: usize,
    previous: Option<usize>,
    distances: Distances,
    workspace: usize,
}

impl Search {
    /// Reserve bounded, uninitialized vector backing only; there is no graph
    /// population initialization. Owners checkpoint before this allocation.
    pub(crate) fn new(
        nodes: usize,
        source: usize,
        maximum_neighbors: usize,
        workspace_limit: usize,
    ) -> Result<Self, Error> {
        if source >= nodes || maximum_neighbors >= nodes {
            return Err(Error::InvalidRow);
        }
        let required = workspace(maximum_neighbors)?;
        if required > workspace_limit {
            return Err(Error::Workspace {
                requested: required,
                limit: workspace_limit,
            });
        }
        let mut neighbors = Vec::new();
        neighbors
            .try_reserve_exact(maximum_neighbors)
            .map_err(|_| Error::AllocationFailed)?;
        let actual = workspace(neighbors.capacity())?;
        if actual > workspace_limit {
            return Err(Error::Workspace {
                requested: actual,
                limit: workspace_limit,
            });
        }
        Ok(Self {
            nodes,
            maximum_neighbors,
            previous: None,
            distances: Distances {
                source,
                source_position: 0,
                neighbors,
            },
            workspace: actual,
        })
    }

    pub(crate) fn row_count(&self) -> usize {
        self.distances.len()
    }

    pub(crate) fn workspace_bytes(&self) -> usize {
        self.workspace
    }

    /// The row's live checkpoint precedes this step. Refuse a new result BEFORE
    /// retaining it, and never grow the reserved vector implicitly. Duplicate
    /// incidences and a source loop do not consume additional result rows.
    pub(crate) fn visit<E: From<Error>>(
        &mut self,
        neighbor: usize,
        admit: &mut impl FnMut(usize) -> Result<(), E>,
    ) -> Result<(), E> {
        if neighbor >= self.nodes || self.previous.is_some_and(|prior| prior > neighbor) {
            return Err(Error::InvalidRow.into());
        }
        if neighbor != self.distances.source && self.previous != Some(neighbor) {
            let count = self.row_count().checked_add(1).ok_or(Error::SizeOverflow)?;
            admit(count)?;
            if self.distances.neighbors.len() == self.maximum_neighbors {
                return Err(Error::InvalidRow.into());
            }
            self.distances.neighbors.push(neighbor);
            self.distances.source_position += usize::from(neighbor < self.distances.source);
        }
        self.previous = Some(neighbor);
        Ok(())
    }

    pub(crate) fn finish(self) -> Distances {
        self.distances
    }
}

#[cfg(test)]
std::thread_local! {
    static FORCE_DENSE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn with_dense<T>(run: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            FORCE_DENSE.with(|mode| mode.set(self.0));
        }
    }
    let _reset = Reset(FORCE_DENSE.with(|mode| mode.replace(true)));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_small_directed_graph_and_source_matches_dense_shortest_paths() {
        let nodes = 4;
        for mask in 0u32..1 << (nodes * nodes) {
            let mut distance = [[usize::MAX; 4]; 4];
            for (source, row) in distance.iter_mut().enumerate() {
                row[source] = 0;
                for (target, value) in row.iter_mut().enumerate() {
                    if source != target && mask & (1 << (source * nodes + target)) != 0 {
                        *value = 1;
                    }
                }
            }
            for via in 0..nodes {
                for source in 0..nodes {
                    for target in 0..nodes {
                        distance[source][target] = distance[source][target]
                            .min(distance[source][via].saturating_add(distance[via][target]));
                    }
                }
            }
            for (source, distances) in distance.iter().enumerate() {
                for cutoff in [0, 1] {
                    let row = (0..nodes)
                        .filter(|&target| mask & (1 << (source * nodes + target)) != 0)
                        .collect::<Vec<_>>();
                    let degree = if cutoff == 0 { 0 } else { row.len() };
                    let mut search = Search::new(
                        nodes,
                        source,
                        capacity(nodes, degree, nodes).unwrap(),
                        usize::MAX,
                    )
                    .unwrap();
                    if cutoff == 1 {
                        for target in row {
                            search.visit::<Error>(target, &mut |_| Ok(())).unwrap();
                        }
                    }
                    let output = search.finish();
                    let actual = (0..output.len())
                        .map(|index| output.get(index).unwrap())
                        .collect::<Vec<_>>();
                    let expected = distances
                        .iter()
                        .enumerate()
                        .filter_map(|(vertex, &hops)| (hops <= cutoff).then_some((vertex, hops)))
                        .collect::<Vec<_>>();
                    assert_eq!(actual, expected);
                    assert_eq!(output.get(output.len()), None);
                }
            }
        }
    }

    #[test]
    fn enormous_population_needs_only_the_neighborhood_backing() {
        for nodes in [1, 1_000_000, usize::MAX] {
            let source = nodes - 1;
            let zero = Search::new(nodes, source, 0, 0).unwrap();
            assert_eq!(zero.workspace_bytes(), 0);
            assert_eq!(zero.finish().get(0), Some((source, 0)));
            assert!(work(nodes, 0, 2, 0).unwrap() < 32);
            if nodes > 3 {
                let bytes = workspace(2).unwrap();
                let mut search = Search::new(nodes, source, 2, bytes).unwrap();
                search.visit::<Error>(1, &mut |_| Ok(())).unwrap();
                search.visit::<Error>(2, &mut |_| Ok(())).unwrap();
                search.visit::<Error>(source, &mut |_| Ok(())).unwrap();
                assert_eq!(search.workspace_bytes(), bytes);
                let distances = search.finish();
                assert_eq!(distances.len(), 3);
                assert_eq!(distances.get(0), Some((1, 1)));
                assert_eq!(distances.get(1), Some((2, 1)));
                assert_eq!(distances.get(2), Some((source, 0)));
                assert!(matches!(
                    Search::new(nodes, source, 2, bytes - 1),
                    Err(Error::Workspace { requested, limit })
                        if requested == bytes && limit == bytes - 1
                ));
            }
        }
    }

    #[test]
    fn ordered_duplicates_loops_and_refusals_keep_the_exact_retained_prefix() {
        #[derive(Debug, Eq, PartialEq)]
        enum Refusal {
            State(Error),
            Rows(usize),
        }
        impl From<Error> for Refusal {
            fn from(error: Error) -> Self {
                Self::State(error)
            }
        }
        for limit in 1..=4 {
            let mut search = Search::new(6, 3, 3.min(limit - 1), usize::MAX).unwrap();
            let mut visited = Vec::new();
            let result = [1, 1, 2, 3, 3, 5].into_iter().try_for_each(|neighbor| {
                visited.push(neighbor);
                search.visit(neighbor, &mut |rows| {
                    if rows > limit {
                        Err(Refusal::Rows(rows))
                    } else {
                        Ok(())
                    }
                })
            });
            if limit < 4 {
                assert_eq!(result, Err(Refusal::Rows(limit + 1)));
                assert_eq!(search.row_count(), limit);
                assert_eq!(visited.last(), Some(&[1, 2, 5][limit - 1]));
            } else {
                assert_eq!(result, Ok(()));
                let output = search.finish();
                assert_eq!(
                    (0..output.len())
                        .map(|index| output.get(index).unwrap())
                        .collect::<Vec<_>>(),
                    [(1, 1), (2, 1), (3, 0), (5, 1)]
                );
            }
        }
        let mut search = Search::new(6, 3, 3, usize::MAX).unwrap();
        search.visit::<Error>(2, &mut |_| Ok(())).unwrap();
        for invalid in [1, 6, usize::MAX] {
            assert_eq!(
                search.visit::<Error>(invalid, &mut |_| Ok(())),
                Err(Error::InvalidRow)
            );
            assert_eq!(search.row_count(), 2);
        }
        assert!(matches!(Search::new(0, 0, 0, 0), Err(Error::InvalidRow)));
        assert_eq!(capacity(1, 2, 1), Err(Error::InvalidRow));
        assert_eq!(workspace(usize::MAX), Err(Error::SizeOverflow));
        assert_eq!(work(2, 1, usize::MAX, 0), Err(Error::SizeOverflow));
        assert!(!with_dense(|| eligible(Some(1))));
        assert!(eligible(Some(1)));
    }
}
