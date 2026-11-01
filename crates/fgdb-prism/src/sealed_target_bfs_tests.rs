use super::*;
use crate::SealedProjectionError;
use crate::execute::KernelValues;
use std::cell::{Cell, RefCell};

struct Fixture {
    rows: Vec<Vec<usize>>,
    source_steps: Cell<usize>,
    fail_at: Cell<usize>,
    opened: RefCell<Vec<usize>>,
}

impl Fixture {
    fn new(rows: Vec<Vec<usize>>) -> Self {
        Self {
            rows,
            source_steps: Cell::new(0),
            fail_at: Cell::new(usize::MAX),
            opened: RefCell::new(Vec::new()),
        }
    }

    fn reset(&self, fail_at: usize) {
        self.source_steps.set(0);
        self.fail_at.set(fail_at);
        self.opened.borrow_mut().clear();
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
        self.rows.len()
    }
    fn degree(&self, source: usize) -> Option<usize> {
        self.rows.get(source).map(Vec::len)
    }
    fn open(&self, source: usize) -> Result<Self::Cursor<'_>> {
        self.step()?;
        self.opened.borrow_mut().push(source);
        Ok(Row {
            fixture: self,
            targets: self.rows[source].iter(),
        })
    }
}

fn distance(output: &KernelOutput) -> Option<usize> {
    let KernelValues::TargetHopDistance { target, distance } = output.values else {
        panic!("integer target distance")
    };
    assert_eq!(target, 1);
    distance
}

#[test]
fn target_in_the_first_cell_never_hides_later_neighbors_or_fallible_eof() {
    let fixture = Fixture::new(vec![vec![1, 2, 3], vec![4], vec![], vec![], vec![]]);
    let target = Target {
        source: 0,
        target: 1,
        cutoff: None,
    };
    let (result, _) = run(&fixture, target, usize::MAX, &mut || Ok(())).unwrap();
    assert_eq!(distance(&result), Some(1));
    assert_eq!(result.row_count, 1);
    assert_eq!(result.witness.edges_scanned, 3);
    assert_eq!(*fixture.opened.borrow(), vec![0]);
    assert_eq!(fixture.source_steps.get(), 5, "open, three neighbors, EOF");
    for stop in 1..=5 {
        fixture.reset(stop);
        assert!(matches!(run(&fixture, target, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop));
        assert_eq!(fixture.source_steps.get(), stop);
    }

    // Ordinals beyond the population are invalid even when they still fit
    // inside the bitmap's last word, and even after the target was discovered.
    let malformed = Fixture::new(vec![vec![1, 7], vec![]]);
    assert!(matches!(
        run(&malformed, target, usize::MAX, &mut || Ok(())),
        Err(super::super::Error::Execution(
            FnxExecutionError::InvalidUpstreamResult
        ))
    ));
}

#[test]
fn every_open_row_before_the_target_is_drained_even_at_the_same_depth() {
    let fixture = Fixture::new(vec![
        vec![2, 1],
        vec![3],
        vec![4, 5],
        vec![],
        vec![],
        vec![],
    ]);
    let target = Target {
        source: 0,
        target: 1,
        cutoff: None,
    };
    let (result, _) = run(&fixture, target, usize::MAX, &mut || Ok(())).unwrap();
    assert_eq!(distance(&result), Some(1));
    assert_eq!(*fixture.opened.borrow(), vec![0, 2]);
    assert_eq!(result.witness.edges_scanned, 4);
    let source_steps = fixture.source_steps.get();
    for stop in 1..=source_steps {
        fixture.reset(stop);
        assert!(matches!(run(&fixture, target, usize::MAX, &mut || Ok(())),
            Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop));
    }
}

#[test]
fn capacity_refusal_precedes_cursor_open_and_all_control_phases_remain_fallible() {
    let fixture = Fixture::new(vec![vec![2, 3], vec![], vec![1], vec![4], vec![]]);
    let target = Target {
        source: 0,
        target: 1,
        cutoff: Some(2),
    };
    let bytes = workspace::<Infallible>(fixture.node_count(), 0, 1, target.cutoff).unwrap();
    assert!(matches!(run(&fixture, target, bytes - 1, &mut || Ok(())),
        Err(super::super::Error::Execution(FnxExecutionError::LimitExceeded {
            resource: "kernel workspace bytes", requested, limit,
        })) if requested == bytes && limit == bytes - 1));
    assert_eq!(fixture.source_steps.get(), 0);
    let mut checks = 0;
    let (result, peak) = run(&fixture, target, bytes, &mut || {
        checks += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(distance(&result), Some(2));
    assert_eq!(peak, bytes);
    assert!(checks > result.witness.edges_scanned + result.witness.nodes_touched);
    for stop in 1..=checks {
        let mut seen = 0;
        fixture.reset(usize::MAX);
        assert!(matches!(run(&fixture, target, bytes, &mut || {
            seen += 1;
            if seen == stop { Err(SealedProjectionError::UnknownOrdinal(stop).into()) } else { Ok(()) }
        }), Err(super::super::Error::Source(SealedProjectionError::UnknownOrdinal(at))) if at == stop));
        assert_eq!(seen, stop);
    }
}

#[test]
fn zero_cutoff_and_self_target_do_not_allocate_or_open_a_row() {
    let fixture = Fixture::new(vec![vec![1], vec![]]);
    fixture.fail_at.set(1);
    for (source, cutoff, expected) in [
        (0, Some(0), None),
        (1, None, Some(0)),
        (1, Some(0), Some(0)),
    ] {
        let (result, bytes) = run(
            &fixture,
            Target {
                source,
                target: 1,
                cutoff,
            },
            0,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(distance(&result), expected);
        assert_eq!(bytes, 0);
        assert_eq!(fixture.source_steps.get(), 0);
    }
}
