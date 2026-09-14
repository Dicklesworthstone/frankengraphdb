//! Fallible in-place ordering and linear ALL/DISTINCT merge. Rows move; their
//! scalar payloads are never cloned by a set operator.

use super::{GraphSetOperation, GraphSetQuantifier};
use crate::GlaExecutionEvent;
use core::cmp::Ordering;

pub(super) fn sort<T, E, C, F>(rows: &mut [T], control: &mut C, compare: &mut F) -> Result<(), E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    let len = rows.len();
    for root in (0..len / 2).rev() {
        sift(rows, root, len, control, compare)?;
    }
    for end in (1..len).rev() {
        control(GlaExecutionEvent::Work)?;
        rows.swap(0, end);
        sift(rows, 0, end, control, compare)?;
    }
    Ok(())
}
fn sift<T, E, C, F>(
    rows: &mut [T],
    mut root: usize,
    end: usize,
    control: &mut C,
    compare: &mut F,
) -> Result<(), E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    while root < end / 2 {
        let mut child = 2 * root + 1;
        if child + 1 < end && compare(&rows[child], &rows[child + 1], control)? == Ordering::Less {
            child += 1;
        }
        if compare(&rows[root], &rows[child], control)? != Ordering::Less {
            break;
        }
        control(GlaExecutionEvent::Work)?;
        rows.swap(root, child);
        root = child;
    }
    Ok(())
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
    mut left: Vec<T>,
    mut right: Vec<T>,
    operation: GraphSetOperation,
    quantifier: GraphSetQuantifier,
    control: &mut C,
    compare: &mut F,
) -> Result<Vec<T>, E>
where
    C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
{
    sort(&mut left, control, compare)?;
    sort(&mut right, control, compare)?;
    if quantifier == GraphSetQuantifier::Distinct {
        // DISTINCT EXCEPT must remove ALL occurrences of any right-side value.
        // Deduplicating only the difference of two bags would be incorrect.
        unique(&mut left, control, compare)?;
        unique(&mut right, control, compare)?;
    }
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    let mut result = Vec::new();
    loop {
        control(GlaExecutionEvent::Work)?;
        let order = match (left.peek(), right.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => compare(a, b, control)?,
        };
        let next = match (operation, order) {
            (GraphSetOperation::Union, Ordering::Less) => left.next(),
            (GraphSetOperation::Union, Ordering::Greater) => right.next(),
            (GraphSetOperation::Union, Ordering::Equal) => {
                if quantifier == GraphSetQuantifier::Distinct {
                    let _ = right.next();
                }
                left.next()
            }
            (GraphSetOperation::Intersect, Ordering::Equal) => {
                let _ = right.next();
                left.next()
            }
            (GraphSetOperation::Intersect, Ordering::Less) => {
                let _ = left.next();
                None
            }
            (GraphSetOperation::Intersect, Ordering::Greater) => {
                let _ = right.next();
                None
            }
            (GraphSetOperation::Except, Ordering::Less) => left.next(),
            (GraphSetOperation::Except, Ordering::Equal) => {
                let _ = left.next();
                let _ = right.next();
                None
            }
            (GraphSetOperation::Except, Ordering::Greater) => {
                let _ = right.next();
                None
            }
        };
        if let Some(row) = next {
            control(GlaExecutionEvent::ScratchEntry)?;
            result.push(row);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
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
