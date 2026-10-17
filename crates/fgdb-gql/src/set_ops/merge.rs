//! Fallible in-place ordering and linear ALL/DISTINCT merge. Rows move; their
//! scalar payloads are never cloned by a set operator.

use super::{GraphSetOperation, GraphSetQuantifier};
use crate::GlaExecutionEvent;
use core::cmp::Ordering;

mod cursor;
pub(super) use cursor::SortedMerge;

/// One decision over complete canonically ordered input heads. Emit steps
/// consume the emitted head; every other named advance consumes and discards
/// that head. No row is copied or retained by the decision itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillSetMergeStep {
    Done,
    EmitLeft,
    EmitRight,
    EmitLeftAdvanceRight,
    AdvanceLeft,
    AdvanceRight,
    AdvanceBoth,
}

/// The resident and external merge share this exact ALL/DISTINCT transition.
/// DISTINCT inputs must each be unique before this function is driven; removing
/// duplicates only from the output would implement EXCEPT DISTINCT incorrectly.
pub(super) fn next_step<T: ?Sized, E, C, F>(
    left: Option<&T>,
    right: Option<&T>,
    operation: GraphSetOperation,
    quantifier: GraphSetQuantifier,
    control: &mut C,
    compare: &mut F,
) -> Result<SpillSetMergeStep, E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    control(GlaExecutionEvent::Work)?;
    let order = match (left, right) {
        (None, None) => return Ok(SpillSetMergeStep::Done),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (Some(a), Some(b)) => compare(a, b, control)?,
    };
    Ok(match (operation, order) {
        (GraphSetOperation::Union, Ordering::Less) => SpillSetMergeStep::EmitLeft,
        (GraphSetOperation::Union, Ordering::Greater) => SpillSetMergeStep::EmitRight,
        (GraphSetOperation::Union, Ordering::Equal) => {
            if quantifier == GraphSetQuantifier::Distinct {
                SpillSetMergeStep::EmitLeftAdvanceRight
            } else {
                SpillSetMergeStep::EmitLeft
            }
        }
        (GraphSetOperation::Intersect, Ordering::Equal) => SpillSetMergeStep::EmitLeftAdvanceRight,
        (GraphSetOperation::Intersect, Ordering::Less) => SpillSetMergeStep::AdvanceLeft,
        (GraphSetOperation::Intersect, Ordering::Greater) => SpillSetMergeStep::AdvanceRight,
        (GraphSetOperation::Except, Ordering::Less) => SpillSetMergeStep::EmitLeft,
        (GraphSetOperation::Except, Ordering::Equal) => SpillSetMergeStep::AdvanceBoth,
        (GraphSetOperation::Except, Ordering::Greater) => SpillSetMergeStep::AdvanceRight,
    })
}

pub(super) fn sort<T, E, C, F>(rows: &mut [T], control: &mut C, compare: &mut F) -> Result<(), E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    let mut sort = InPlaceSort::new(rows.len());
    loop {
        let mut remaining = usize::MAX;
        if sort
            .advance(rows, &mut remaining, control, compare)?
            .is_ready()
        {
            return Ok(());
        }
    }
}

/// Resumable form of the same in-place heapsort used by blocking consumers.
/// Only indices survive a yield; rows and payloads stay in their original
/// allocation. Each quantum unit permits one ascent comparison, heap descent
/// or extraction swap. Comparisons remain indivisible governed calls, not
/// wall-clock-bounded work.
///
/// The heap is entered only after the input is found not to be strictly
/// ascending. A strictly ascending input is the one sorted order of pairwise
/// distinct rows, exactly what the heap would leave, so its n - 1 ascent
/// comparisons are the whole sort and nothing moves. Any other input then runs
/// the former kernel unchanged, tied occurrences included: the comparisons
/// spent finding its first non-ascending pair are the only addition.
///
/// The owner must supply the same, otherwise unchanged slice on every advance
/// and discard both state and rows on failure. Pending never admits output.
/// Suspension adds no control events and repeats no comparison or move, so the
/// synchronous and cooperative paths have identical cumulative transcripts.
pub(crate) struct InPlaceSort {
    len: usize,
    // Leading adjacent pairs proved strictly ascending, while still checking.
    ascending: Option<usize>,
    build: usize,
    end: usize,
    root: Option<usize>,
}

impl InPlaceSort {
    pub(crate) fn new(len: usize) -> Self {
        Self {
            len,
            ascending: Some(0),
            build: len / 2,
            end: len,
            root: None,
        }
    }

    pub(crate) fn advance<T, E, C, F>(
        &mut self,
        rows: &mut [T],
        remaining: &mut usize,
        control: &mut C,
        compare: &mut F,
    ) -> Result<core::task::Poll<()>, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
    {
        assert_eq!(rows.len(), self.len, "sort input changed across suspension");
        while let Some(pairs) = self.ascending {
            if pairs + 1 >= self.len {
                return Ok(core::task::Poll::Ready(()));
            }
            if *remaining == 0 {
                return Ok(core::task::Poll::Pending);
            }
            *remaining -= 1;
            self.ascending = (compare(&rows[pairs], &rows[pairs + 1], control)? == Ordering::Less)
                .then_some(pairs + 1);
        }
        loop {
            if let Some(root) = self.root {
                if root >= self.end / 2 {
                    self.root = None;
                    continue;
                }
                if *remaining == 0 {
                    return Ok(core::task::Poll::Pending);
                }
                *remaining -= 1;
                // root < end / 2 proves both the multiplication and the
                // left-child index, including odd heaps and usize boundaries.
                let mut child = 2 * root + 1;
                if child + 1 < self.end
                    && compare(&rows[child], &rows[child + 1], control)? == Ordering::Less
                {
                    child += 1;
                }
                if compare(&rows[root], &rows[child], control)? != Ordering::Less {
                    self.root = None;
                    continue;
                }
                control(GlaExecutionEvent::Work)?;
                rows.swap(root, child);
                self.root = Some(child);
            } else if self.build != 0 {
                self.build -= 1;
                self.root = Some(self.build);
            } else if self.end > 1 {
                if *remaining == 0 {
                    return Ok(core::task::Poll::Pending);
                }
                *remaining -= 1;
                control(GlaExecutionEvent::Work)?;
                self.end -= 1;
                rows.swap(0, self.end);
                self.root = Some(0);
            } else {
                return Ok(core::task::Poll::Ready(()));
            }
        }
    }
}
fn unique<T, E, C, F>(rows: &mut Vec<T>, control: &mut C, compare: &mut F) -> Result<(), E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    let mut kept = usize::from(!rows.is_empty());
    for at in 1..rows.len() {
        if compare(&rows[kept - 1], &rows[at], control)? != Ordering::Equal {
            control(GlaExecutionEvent::Work)?;
            rows.swap(kept, at);
            kept += 1;
        }
    }
    rows.truncate(kept);
    Ok(())
}

pub(super) fn combine<T, E, C, F>(
    left: Vec<T>,
    right: Vec<T>,
    operation: GraphSetOperation,
    quantifier: GraphSetQuantifier,
    control: &mut C,
    compare: &mut F,
) -> Result<Vec<T>, E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    let mut merge = SortedMerge::new(left, right, operation, quantifier, control, compare)?;
    let mut result = Vec::new();
    while let Some(row) = merge.next_with_control(control, compare)? {
        control(GlaExecutionEvent::ScratchEntry)?;
        result.push(row);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum SortEvent {
        Compare((i32, usize), (i32, usize)),
        Work,
    }

    // An ascent scan, then the former blocking kernel, is an independent
    // transcript oracle. Equal keys carry distinct occurrence IDs to expose
    // any changed swap/tie order.
    fn reference_sort(rows: &mut [(i32, usize)], events: &mut Vec<SortEvent>) {
        fn compare(a: (i32, usize), b: (i32, usize), events: &mut Vec<SortEvent>) -> Ordering {
            events.push(SortEvent::Compare(a, b));
            events.push(SortEvent::Work);
            a.0.cmp(&b.0)
        }
        if (1..rows.len()).all(|at| compare(rows[at - 1], rows[at], events) == Ordering::Less) {
            return;
        }
        fn sift(
            rows: &mut [(i32, usize)],
            mut root: usize,
            end: usize,
            events: &mut Vec<SortEvent>,
        ) {
            while root < end / 2 {
                let mut child = 2 * root + 1;
                if child + 1 < end
                    && compare(rows[child], rows[child + 1], events) == Ordering::Less
                {
                    child += 1;
                }
                if compare(rows[root], rows[child], events) != Ordering::Less {
                    break;
                }
                events.push(SortEvent::Work);
                rows.swap(root, child);
                root = child;
            }
        }
        let len = rows.len();
        for root in (0..len / 2).rev() {
            sift(rows, root, len, events);
        }
        for end in (1..len).rev() {
            events.push(SortEvent::Work);
            rows.swap(0, end);
            sift(rows, 0, end, events);
        }
    }

    fn advance_traced(
        sort: &mut InPlaceSort,
        rows: &mut [(i32, usize)],
        remaining: &mut usize,
        events: &std::cell::RefCell<Vec<SortEvent>>,
    ) -> core::task::Poll<()> {
        sort.advance(
            rows,
            remaining,
            &mut |_| {
                events.borrow_mut().push(SortEvent::Work);
                Ok::<_, ()>(())
            },
            &mut |a, b, control| {
                events.borrow_mut().push(SortEvent::Compare(*a, *b));
                control(GlaExecutionEvent::Work)?;
                Ok(a.0.cmp(&b.0))
            },
        )
        .unwrap()
    }

    #[test]
    fn cooperative_sort_preserves_every_comparison_move_and_tied_occurrence() {
        for len in 0..=6 {
            for mut code in 0..3_usize.pow(len) {
                let input: Vec<_> = (0..len as usize)
                    .map(|at| {
                        let value = (code % 3) as i32 - 1;
                        code /= 3;
                        (value, at)
                    })
                    .collect();
                let mut expected = input.clone();
                let mut expected_events = Vec::new();
                reference_sort(&mut expected, &mut expected_events);
                assert!(expected.windows(2).all(|pair| pair[0].0 <= pair[1].0));
                for quantum in [1, 2, 5, 64, usize::MAX] {
                    let mut actual = input.clone();
                    let events = std::cell::RefCell::new(Vec::new());
                    let mut sort = InPlaceSort::new(actual.len());
                    let mut zero = 0;
                    let first = advance_traced(&mut sort, &mut actual, &mut zero, &events);
                    assert_eq!(first.is_ready(), len < 2);
                    assert_eq!(actual, input);
                    assert!(events.borrow().is_empty());
                    loop {
                        let mut remaining = quantum;
                        let result =
                            advance_traced(&mut sort, &mut actual, &mut remaining, &events);
                        if result.is_ready() {
                            break;
                        }
                        assert_eq!(remaining, 0);
                    }
                    assert_eq!(actual, expected);
                    assert_eq!(*events.borrow(), expected_events);
                    let mut zero = 0;
                    assert!(advance_traced(&mut sort, &mut actual, &mut zero, &events).is_ready());
                    assert_eq!(*events.borrow(), expected_events);
                }
            }
        }
    }

    #[test]
    fn a_strictly_ascending_input_is_sorted_by_its_ascent_comparisons_alone() {
        for len in 0..64_usize {
            let input: Vec<_> = (0..len).map(|at| (at as i32, at)).collect();
            for tie in [None, Some(len / 2)] {
                let mut input = input.clone();
                if let Some(at) = tie.filter(|&at| at > 0) {
                    input[at].0 = input[at - 1].0;
                }
                let mut rows = input.clone();
                let (mut comparisons, mut events) = (0, 0);
                sort(
                    &mut rows,
                    &mut |_| {
                        events += 1;
                        Ok::<_, ()>(())
                    },
                    &mut |a: &(i32, usize), b: &(i32, usize), control| {
                        comparisons += 1;
                        control(GlaExecutionEvent::Work)?;
                        Ok(a.0.cmp(&b.0))
                    },
                )
                .unwrap();
                let mut expected = input.clone();
                let mut oracle = Vec::new();
                reference_sort(&mut expected, &mut oracle);
                assert_eq!(rows, expected);
                if tie.is_some_and(|at| at > 0) {
                    // One tie sends the input through the former heap.
                    assert!(comparisons > len - 1, "{len}");
                } else {
                    assert_eq!(comparisons, len.saturating_sub(1));
                    assert_eq!(events, comparisons, "no move");
                    assert_eq!(rows, input);
                }
            }
        }
    }

    #[test]
    fn cooperative_sort_refuses_at_every_original_control_boundary() {
        let input: Vec<_> = (0..37).map(|at| ((at * 13 % 17) as i32, at)).collect();
        let mut expected = input.clone();
        let mut events = Vec::new();
        reference_sort(&mut expected, &mut events);
        let calls = events.iter().filter(|e| **e == SortEvent::Work).count();
        for quantum in [1, 3, 64] {
            for stop in 1..=calls {
                let mut rows = input.clone();
                let mut seen = 0;
                let mut sort = InPlaceSort::new(rows.len());
                let result = loop {
                    let mut remaining = quantum;
                    let result = sort.advance(
                        &mut rows,
                        &mut remaining,
                        &mut |_| {
                            seen += 1;
                            if seen == stop { Err(stop) } else { Ok(()) }
                        },
                        &mut |a, b, control| {
                            control(GlaExecutionEvent::Work)?;
                            Ok(a.0.cmp(&b.0))
                        },
                    );
                    if !matches!(result, Ok(core::task::Poll::Pending)) {
                        break result;
                    }
                };
                assert_eq!(result, Err(stop));
                assert_eq!(seen, stop);
                // A refused partially ordered buffer still owns every input
                // exactly once; no element was lost, cloned or overwritten.
                rows.sort_by_key(|row| row.1);
                assert_eq!(rows, input);
            }
        }
    }

    fn run(
        left: Vec<i32>,
        right: Vec<i32>,
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
    ) -> Vec<i32> {
        combine(
            left,
            right,
            operation,
            quantifier,
            &mut |_| Ok::<_, ()>(()),
            &mut |a, b, control| {
                control(GlaExecutionEvent::Work)?;
                Ok(a.cmp(b))
            },
        )
        .unwrap()
    }
    #[test]
    fn all_six_set_laws_match_independent_count_arithmetic() {
        // 27 multiplicity assignments per side, including empty inputs and
        // 2-vs-1 occurrences that distinguish EXCEPT ALL from EXCEPT DISTINCT.
        for a in 0..27 {
            for b in 0..27 {
                let counts = |code: i32| [code % 3, (code / 3) % 3, code / 9];
                let lc = counts(a);
                let rc = counts(b);
                let bag = |counts: [i32; 3]| {
                    (0..3)
                        .rev()
                        .flat_map(move |i| std::iter::repeat_n(i as i32 - 1, counts[i] as usize))
                        .collect::<Vec<_>>()
                };
                for operation in [
                    GraphSetOperation::Union,
                    GraphSetOperation::Intersect,
                    GraphSetOperation::Except,
                ] {
                    for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                        let mut expected = Vec::new();
                        for i in 0..3 {
                            let (mut l, mut r) = (lc[i], rc[i]);
                            if quantifier == GraphSetQuantifier::Distinct {
                                l = i32::from(l != 0);
                                r = i32::from(r != 0);
                            }
                            let n = match operation {
                                GraphSetOperation::Union
                                    if quantifier == GraphSetQuantifier::Distinct =>
                                {
                                    i32::from(l + r != 0)
                                }
                                GraphSetOperation::Union => l + r,
                                GraphSetOperation::Intersect => l.min(r),
                                GraphSetOperation::Except => (l - r).max(0),
                            };
                            expected.extend(std::iter::repeat_n(i as i32 - 1, n as usize));
                        }
                        assert_eq!(
                            run(bag(lc), bag(rc), operation, quantifier),
                            expected,
                            "{a}/{b}: {operation:?} {quantifier:?}"
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn every_comparison_move_and_retention_boundary_can_refuse() {
        for operation in [
            GraphSetOperation::Union,
            GraphSetOperation::Intersect,
            GraphSetOperation::Except,
        ] {
            for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let mut total = 0;
                let left = vec![3, 0, 2, 0, 1, 3];
                let right = vec![4, 0, 3, 3];
                let execute = |control: &mut dyn FnMut(GlaExecutionEvent) -> Result<(), usize>| {
                    combine(
                        left.clone(),
                        right.clone(),
                        operation,
                        quantifier,
                        &mut |e| control(e),
                        &mut |a, b, c| {
                            c(GlaExecutionEvent::Work)?;
                            Ok(a.cmp(b))
                        },
                    )
                };
                execute(&mut |_| {
                    total += 1;
                    Ok(())
                })
                .unwrap();
                for stop in 1..=total {
                    let mut seen = 0;
                    let result = execute(&mut |_| {
                        seen += 1;
                        if seen == stop { Err(stop) } else { Ok(()) }
                    });
                    assert_eq!(result, Err(stop));
                    assert_eq!(seen, stop);
                }
            }
        }
    }
}
