use super::*;
use crate::execute::KernelValues;
use crate::{
    Directedness, GraphView, ParallelEdgePolicy, ProjectionEdge, ProjectionLimits, ProjectionSpec,
    SealedProjectionError, SelfLoopPolicy, SnapshotBinding, SnapshotGraphView,
};
use fgdb_types::{CommitSeq, EId, VId, ids::ObjectId};
use std::cell::Cell;

struct Fixture {
    graph: SnapshotGraphView,
    source_steps: Cell<usize>,
    fail_at: Cell<usize>,
}

impl Fixture {
    fn new(direction: Directedness) -> Self {
        let vertices = [VId(u128::MAX), VId(1), VId(1u128 << 80), VId(0)];
        let edges: Vec<_> = [(0, 1), (1, 1), (1, 2), (2, 0), (0, 1)]
            .into_iter()
            .enumerate()
            .map(|(id, (source, target))| ProjectionEdge {
                eid: EId(id as u128),
                source: vertices[source],
                target: vertices[target],
                weight: -7.0,
            })
            .collect();
        Self {
            graph: SnapshotGraphView::build(
                SnapshotBinding {
                    root: ObjectId([0x6b; 32]),
                    as_of: CommitSeq(5),
                },
                &vertices,
                &edges,
                ProjectionSpec {
                    directedness: direction,
                    parallel_edges: ParallelEdgePolicy::Sum,
                    self_loops: SelfLoopPolicy::Keep,
                },
                ProjectionLimits {
                    max_vertices: 4,
                    max_input_edges: edges.len(),
                    max_adjacency_entries: edges.len() * 2,
                    max_workspace_bytes: 1 << 20,
                },
            )
            .unwrap(),
            source_steps: Cell::new(0),
            fail_at: Cell::new(usize::MAX),
        }
    }

    fn step(&self) -> Result<()> {
        let next = self.source_steps.get() + 1;
        self.source_steps.set(next);
        if next == self.fail_at.get() {
            return Err(SealedProjectionError::UnknownOrdinal(next).into());
        }
        Ok(())
    }
}

struct Row<'a> {
    fixture: &'a Fixture,
    targets: std::slice::Iter<'a, usize>,
}

impl Cursor for Row<'_> {
    fn next(&mut self) -> Result<Option<(usize, f64)>> {
        self.fixture.step()?;
        Ok(self.targets.next().map(|&target| (target, -7.0)))
    }
}

impl Rows for Fixture {
    type Cursor<'a> = Row<'a>;

    fn node_count(&self) -> usize {
        self.graph.node_count()
    }
    fn degree(&self, source: usize) -> Option<usize> {
        self.graph.neighbors_indices(source).map(<[_]>::len)
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        self.step()?;
        Ok(Row {
            fixture: self,
            targets: self.graph.neighbors_indices(source).unwrap().iter(),
        })
    }
}

#[test]
fn fallible_katz_preserves_every_score_bit_and_complete_row_failure() {
    for direction in [
        Directedness::Directed,
        Directedness::Reversed,
        Directedness::Undirected,
    ] {
        for options in [
            KatzOptions::default(),
            KatzOptions::new(0.25, -0.75, 200, 1e-10).unwrap(),
        ] {
            let fixture = Fixture::new(direction);
            let (actual, peak) = run(&fixture, options, usize::MAX, &mut || Ok(())).unwrap();
            let source_steps = fixture.source_steps.get();
            let (expected, expected_peak) =
                crate::katz::run(&fixture.graph, options, &mut || Ok::<_, Infallible>(())).unwrap();
            assert_eq!(actual.witness, expected.witness);
            assert_eq!(actual.row_count, expected.row_count);
            assert_eq!(peak, expected_peak);
            let iterations = actual.witness.nodes_touched / fixture.node_count();
            let KernelValues::Scores(actual) = actual.values else {
                panic!("scores")
            };
            let KernelValues::Scores(expected) = expected.values else {
                panic!("scores")
            };
            assert_eq!(
                actual.iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
                expected.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
            assert_eq!(peak, 2 * fixture.node_count() * std::mem::size_of::<f64>());
            let arcs = fixture.graph.outgoing_offsets().last().copied().unwrap();
            assert_eq!(
                source_steps,
                iterations * (2 * fixture.node_count() + arcs),
                "each opened row reaches EOF, even on the converged round"
            );

            // Includes opens, all yielded arcs and each fallible EOF. No
            // partially completed iteration or result can escape any failure.
            for stop in 1..=source_steps {
                fixture.source_steps.set(0);
                fixture.fail_at.set(stop);
                assert!(matches!(
                    run(&fixture, options, usize::MAX, &mut || Ok(())),
                    Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at)))
                        if at == stop
                ));
                assert_eq!(fixture.source_steps.get(), stop);
            }
        }
    }
}

#[test]
fn fallible_katz_admits_two_buffers_before_any_cursor_and_polls_every_scalar_phase() {
    let fixture = Fixture::new(Directedness::Directed);
    let options = KatzOptions::new(0.1, 1.0, 50, 1e-8).unwrap();
    let bytes = workspace::<Infallible>(fixture.node_count(), options).unwrap();
    assert!(matches!(
        run(&fixture, options, bytes - 1, &mut || Ok(())),
        Err(super::super::Error::Execution(
            FnxExecutionError::LimitExceeded {
                resource: "kernel workspace bytes",
                ..
            }
        ))
    ));
    assert_eq!(fixture.source_steps.get(), 0);
    let mut checks = 0;
    let (expected, peak) = run(&fixture, options, bytes, &mut || {
        checks += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(peak, bytes);
    assert!(checks > expected.witness.edges_scanned + expected.witness.nodes_touched);
    for stop in 1..=checks {
        let mut seen = 0;
        let actual = run(&fixture, options, bytes, &mut || {
            seen += 1;
            if seen == stop {
                Err(SealedProjectionError::UnknownOrdinal(stop).into())
            } else {
                Ok(())
            }
        });
        assert!(matches!(actual,
            Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop
        ));
        assert_eq!(seen, stop);
    }
}
