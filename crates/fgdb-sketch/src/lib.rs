//! Deterministic graph-statistics sketches.
//!
//! Sketches are advisory summaries, never authoritative graph state. Every
//! implementation fixes its merge and deletion behavior explicitly and exposes
//! a versioned, bounded canonical codec for its logical state. These codecs are
//! embedded value encodings, not top-level Appendix A object-kind registrations.

#![forbid(unsafe_code)]

pub mod bottom_k;
pub mod count_min;
pub mod degree_histogram;
pub mod distinct;
pub mod exact_quantiles;
pub mod label_counts;
pub mod maintenance_log;
pub mod path_correlation;
pub mod zone_map;

#[cfg(test)]
pub(crate) mod graph_accuracy_fixtures {
    use fnx_generators::{GenerationReport, GraphGenerator};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum FixtureSource {
        FnxGenerators,
    }

    /// A deterministic, simple, undirected graph workload.
    #[derive(Clone, Debug)]
    pub(crate) struct GraphFixture {
        pub(crate) name: &'static str,
        pub(crate) source: FixtureSource,
        pub(crate) node_count: usize,
        pub(crate) edges: Vec<(u64, u64)>,
    }

    pub(crate) fn named_graph_fixtures() -> Vec<GraphFixture> {
        let mut generator = GraphGenerator::strict();
        vec![
            fnx_fixture(
                "fnx_path_graph_n1024",
                generator.path_graph(1_024).expect("pinned fnx path graph"),
            ),
            fnx_fixture(
                "fnx_star_graph_spokes1023",
                generator.star_graph(1_023).expect("pinned fnx star graph"),
            ),
            fnx_fixture(
                "fnx_cycle_graph_n1024",
                generator
                    .cycle_graph(1_024)
                    .expect("pinned fnx cycle graph"),
            ),
            fnx_fixture(
                "fnx_complete_bipartite_graph_48_64",
                generator
                    .complete_multipartite_graph(&[48, 64])
                    .expect("pinned fnx complete bipartite graph"),
            ),
        ]
    }

    fn fnx_fixture(name: &'static str, report: GenerationReport) -> GraphFixture {
        assert!(
            report.warnings.is_empty(),
            "strict pinned fnx fixture {name} emitted warnings: {:?}",
            report.warnings
        );
        let node_count = report.graph.node_count();
        let edges = report
            .graph
            .edges_ordered_indices()
            .into_iter()
            .map(|(left, right)| {
                (
                    u64::try_from(left).expect("fnx node index fits u64"),
                    u64::try_from(right).expect("fnx node index fits u64"),
                )
            })
            .collect();
        GraphFixture {
            name,
            source: FixtureSource::FnxGenerators,
            node_count,
            edges,
        }
    }

    pub(crate) fn canonical_edge_bytes(left: u64, right: u64) -> [u8; 16] {
        let (low, high) = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        // This is a test-only opaque observation key. Its byte order is part
        // of the fixture identity, not a durable fixed-integer encoding.
        let mut bytes = [0_u8; 16];
        bytes[..8].copy_from_slice(&low.to_be_bytes());
        bytes[8..].copy_from_slice(&high.to_be_bytes());
        bytes
    }

    #[test]
    fn named_graph_fixtures_have_frozen_nonempty_distinct_edges() {
        let fixtures = named_graph_fixtures();
        let expected = [
            (
                "fnx_path_graph_n1024",
                FixtureSource::FnxGenerators,
                1_024,
                1_023,
            ),
            (
                "fnx_star_graph_spokes1023",
                FixtureSource::FnxGenerators,
                1_024,
                1_023,
            ),
            (
                "fnx_cycle_graph_n1024",
                FixtureSource::FnxGenerators,
                1_024,
                1_024,
            ),
            (
                "fnx_complete_bipartite_graph_48_64",
                FixtureSource::FnxGenerators,
                112,
                3_072,
            ),
        ];
        assert_eq!(fixtures.len(), expected.len());

        for (fixture, (name, source, node_count, edge_count)) in fixtures.iter().zip(expected) {
            assert_eq!(fixture.name, name);
            assert_eq!(fixture.source, source);
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

        assert_eq!(
            fixtures
                .iter()
                .filter(|fixture| fixture.source == FixtureSource::FnxGenerators)
                .count(),
            fixtures.len(),
            "the accuracy harness must execute named fixtures from the pinned fnx generator"
        );
    }
}
