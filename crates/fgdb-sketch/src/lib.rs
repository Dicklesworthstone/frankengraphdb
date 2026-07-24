//! Deterministic graph-statistics sketches.
//!
//! Sketches are advisory summaries, never authoritative graph state. Every
//! implementation fixes its merge and deletion behavior explicitly and exposes
//! a canonical logical state for registry-generated durable encoders.

#![forbid(unsafe_code)]

pub mod bottom_k;
pub mod count_min;
pub mod degree_histogram;
pub mod distinct;
pub mod exact_quantiles;
pub mod maintenance_log;
pub mod zone_map;

#[cfg(test)]
pub(crate) mod graph_accuracy_fixtures {
    /// A deterministic, simple, undirected graph workload.
    ///
    /// These topology-only fixtures mirror the named basic graph shapes used by
    /// `fnx-generators`. They deliberately do not claim parity with fnx's
    /// randomized generator implementations.
    #[derive(Clone, Debug)]
    pub(crate) struct GraphFixture {
        pub(crate) name: &'static str,
        pub(crate) node_count: usize,
        pub(crate) edges: Vec<(u64, u64)>,
    }

    pub(crate) fn named_graph_fixtures() -> Vec<GraphFixture> {
        vec![
            path_graph(1_024),
            star_graph(1_024),
            cycle_graph(1_024),
            complete_bipartite_graph(48, 64),
        ]
    }

    fn path_graph(node_count: usize) -> GraphFixture {
        let edges = (1..node_count)
            .map(|node| ((node - 1) as u64, node as u64))
            .collect();
        GraphFixture {
            name: "path_graph_n1024",
            node_count,
            edges,
        }
    }

    fn star_graph(node_count: usize) -> GraphFixture {
        let edges = (1..node_count).map(|node| (0, node as u64)).collect();
        GraphFixture {
            name: "star_graph_n1024",
            node_count,
            edges,
        }
    }

    fn cycle_graph(node_count: usize) -> GraphFixture {
        let mut edges = (1..node_count)
            .map(|node| ((node - 1) as u64, node as u64))
            .collect::<Vec<_>>();
        edges.push(((node_count - 1) as u64, 0));
        GraphFixture {
            name: "cycle_graph_n1024",
            node_count,
            edges,
        }
    }

    fn complete_bipartite_graph(left: usize, right: usize) -> GraphFixture {
        let mut edges = Vec::with_capacity(left * right);
        for left_node in 0..left {
            for right_node in 0..right {
                edges.push((left_node as u64, (left + right_node) as u64));
            }
        }
        GraphFixture {
            name: "complete_bipartite_graph_48_64",
            node_count: left + right,
            edges,
        }
    }

    pub(crate) fn canonical_edge_bytes(left: u64, right: u64) -> [u8; 16] {
        let (low, high) = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&low.to_be_bytes());
        bytes[8..].copy_from_slice(&high.to_be_bytes());
        bytes
    }

    #[test]
    fn named_graph_fixtures_have_frozen_nonempty_distinct_edges() {
        let fixtures = named_graph_fixtures();
        let expected = [
            ("path_graph_n1024", 1_024, 1_023),
            ("star_graph_n1024", 1_024, 1_023),
            ("cycle_graph_n1024", 1_024, 1_024),
            ("complete_bipartite_graph_48_64", 112, 3_072),
        ];
        assert_eq!(fixtures.len(), expected.len());

        for (fixture, (name, node_count, edge_count)) in fixtures.iter().zip(expected) {
            assert_eq!(fixture.name, name);
            assert_eq!(fixture.node_count, node_count);
            assert_eq!(fixture.edges.len(), edge_count);
            assert!(!fixture.edges.is_empty());
            assert!(fixture.edges.iter().all(|&(left, right)| {
                left < node_count as u64 && right < node_count as u64 && left != right
            }));

            let mut canonical_edges = fixture
                .edges
                .iter()
                .map(|&(left, right)| canonical_edge_bytes(left, right))
                .collect::<Vec<_>>();
            canonical_edges.sort_unstable();
            canonical_edges.dedup();
            assert_eq!(canonical_edges.len(), edge_count);
        }
    }
}
