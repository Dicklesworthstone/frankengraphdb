//! Pull the canonical set merge without retaining a second result bag.
//!
//! Preparation still admits and sorts both complete child bags. This is the
//! merge operator's cursor, not a claim that graph sources are streamed or
//! spill-backed. Comparisons and moves use the caller's existing cumulative
//! control; retaining or delivering a row is charged by its actual consumer.

use super::{GraphSetOperation, GraphSetQuantifier, sort, unique};
use crate::GlaExecutionEvent;
use core::cmp::Ordering;
use std::iter::Peekable;
use std::vec::IntoIter;

struct Inputs<T> {
    left: Peekable<IntoIter<T>>,
    right: Peekable<IntoIter<T>>,
}

/// Own both admitted child bags and move one selected occurrence per pull.
/// No payload clone, output vector, result-row charge or separate meter exists.
/// EOF or the first refusal drops both unread suffixes and permanently fuses
/// this cursor. Already returned rows remain owned by the tentative consumer.
pub(crate) struct SortedMerge<T> {
    inputs: Option<Inputs<T>>,
    operation: GraphSetOperation,
    quantifier: GraphSetQuantifier,
}

impl<T> SortedMerge<T> {
    pub(crate) fn new<E, C, F>(
        mut left: Vec<T>,
        mut right: Vec<T>,
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
        control: &mut C,
        compare: &mut F,
    ) -> Result<Self, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
    {
        sort(&mut left, control, compare)?;
        sort(&mut right, control, compare)?;
        if quantifier == GraphSetQuantifier::Distinct {
            // Deduplicate BEFORE subtraction: EXCEPT DISTINCT removes every
            // left occurrence whenever the right side contains that value.
            unique(&mut left, control, compare)?;
            unique(&mut right, control, compare)?;
        }
        Ok(Self {
            inputs: Some(Inputs {
                left: left.into_iter().peekable(),
                right: right.into_iter().peekable(),
            }),
            operation,
            quantifier,
        })
    }

    pub(crate) fn next_with_control<E, C, F>(
        &mut self,
        control: &mut C,
        compare: &mut F,
    ) -> Result<Option<T>, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
    {
        let Some(inputs) = &mut self.inputs else {
            return Ok(None);
        };
        let result = Self::advance(inputs, self.operation, self.quantifier, control, compare);
        if !matches!(&result, Ok(Some(_))) {
            self.inputs = None;
        }
        result
    }

    fn advance<E, C, F>(
        inputs: &mut Inputs<T>,
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
        control: &mut C,
        compare: &mut F,
    ) -> Result<Option<T>, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
        F: FnMut(&T, &T, &mut C) -> Result<Ordering, E>,
    {
        loop {
            control(GlaExecutionEvent::Work)?;
            let order = match (inputs.left.peek(), inputs.right.peek()) {
                (None, None) => return Ok(None),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (Some(a), Some(b)) => compare(a, b, control)?,
            };
            let next = match (operation, order) {
                (GraphSetOperation::Union, Ordering::Less) => inputs.left.next(),
                (GraphSetOperation::Union, Ordering::Greater) => inputs.right.next(),
                (GraphSetOperation::Union, Ordering::Equal) => {
                    if quantifier == GraphSetQuantifier::Distinct {
                        let _ = inputs.right.next();
                    }
                    inputs.left.next()
                }
                (GraphSetOperation::Intersect, Ordering::Equal) => {
                    let _ = inputs.right.next();
                    inputs.left.next()
                }
                (GraphSetOperation::Intersect, Ordering::Less) => {
                    let _ = inputs.left.next();
                    None
                }
                (GraphSetOperation::Intersect, Ordering::Greater) => {
                    let _ = inputs.right.next();
                    None
                }
                (GraphSetOperation::Except, Ordering::Less) => inputs.left.next(),
                (GraphSetOperation::Except, Ordering::Equal) => {
                    let _ = inputs.left.next();
                    let _ = inputs.right.next();
                    None
                }
                (GraphSetOperation::Except, Ordering::Greater) => {
                    let _ = inputs.right.next();
                    None
                }
            };
            if next.is_some() {
                return Ok(next);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn compare<E, C>(a: &i32, b: &i32, control: &mut C) -> Result<Ordering, E>
    where
        C: FnMut(GlaExecutionEvent) -> Result<(), E>,
    {
        control(GlaExecutionEvent::Work)?;
        Ok(a.cmp(b))
    }

    fn cursor(operation: GraphSetOperation, quantifier: GraphSetQuantifier) -> SortedMerge<i32> {
        SortedMerge::new(
            vec![3, 0, 2, 0, 1, 3],
            vec![4, 0, 3, 3],
            operation,
            quantifier,
            &mut |_| Ok::<_, usize>(()),
            &mut compare,
        )
        .unwrap()
    }

    #[test]
    fn a_pull_does_not_merge_or_charge_retention_for_the_unread_suffix() {
        let mut cursor = cursor(GraphSetOperation::Union, GraphSetQuantifier::All);
        let mut events = Vec::new();
        assert_eq!(
            cursor
                .next_with_control(
                    &mut |event| {
                        events.push(event);
                        Ok::<_, ()>(())
                    },
                    &mut compare,
                )
                .unwrap(),
            Some(0)
        );
        assert_eq!(events, vec![GlaExecutionEvent::Work; 2]);
        // Pausing does not replace or reset the caller's control state.
        assert_eq!(
            cursor
                .next_with_control(
                    &mut |event| {
                        events.push(event);
                        Ok::<_, ()>(())
                    },
                    &mut compare,
                )
                .unwrap(),
            Some(0)
        );
        assert_eq!(events, vec![GlaExecutionEvent::Work; 4]);
    }

    #[test]
    fn every_pull_refusal_fuses_and_preserves_the_successful_prefix() {
        for operation in [
            GraphSetOperation::Union,
            GraphSetOperation::Intersect,
            GraphSetOperation::Except,
        ] {
            for quantifier in [GraphSetQuantifier::All, GraphSetQuantifier::Distinct] {
                let mut normal = cursor(operation, quantifier);
                let mut expected = Vec::new();
                let mut total = 0;
                while let Some(row) = normal
                    .next_with_control(
                        &mut |_| {
                            total += 1;
                            Ok::<_, usize>(())
                        },
                        &mut compare,
                    )
                    .unwrap()
                {
                    expected.push(row);
                }
                for stop in 1..=total {
                    let mut interrupted = cursor(operation, quantifier);
                    let mut seen = 0;
                    let mut prefix = Vec::new();
                    loop {
                        match interrupted.next_with_control(
                            &mut |_| {
                                seen += 1;
                                if seen == stop { Err(stop) } else { Ok(()) }
                            },
                            &mut compare,
                        ) {
                            Ok(Some(row)) => prefix.push(row),
                            result => {
                                assert_eq!(result, Err(stop));
                                break;
                            }
                        }
                    }
                    assert_eq!(prefix, expected[..prefix.len()]);
                    assert_eq!(seen, stop);
                    assert_eq!(
                        interrupted
                            .next_with_control(
                                &mut |_| {
                                    seen += 1;
                                    Ok::<_, usize>(())
                                },
                                &mut compare,
                            )
                            .unwrap(),
                        None
                    );
                    assert_eq!(seen, stop, "a failed cursor must never call control again");
                }
                let mut after_eof = 0;
                assert_eq!(
                    normal
                        .next_with_control(
                            &mut |_| {
                                after_eof += 1;
                                Ok::<_, usize>(())
                            },
                            &mut compare,
                        )
                        .unwrap(),
                    None
                );
                assert_eq!(after_eof, 0);
            }
        }
    }

    // Deliberately not Clone: a selected row must move from its child bag.
    struct OwnedRow {
        key: i32,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for OwnedRow {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[test]
    fn early_drop_and_refusal_release_unread_payloads_without_delivering_them() {
        for refuse in [false, true] {
            let drops = Rc::new(Cell::new(0));
            let make = |key| OwnedRow {
                key,
                drops: Rc::clone(&drops),
            };
            let mut cursor = SortedMerge::new(
                vec![make(1), make(3)],
                vec![make(2), make(4)],
                GraphSetOperation::Union,
                GraphSetQuantifier::All,
                &mut |_| Ok::<_, ()>(()),
                &mut |a, b, control| {
                    control(GlaExecutionEvent::Work)?;
                    Ok(a.key.cmp(&b.key))
                },
            )
            .unwrap();
            let first = cursor
                .next_with_control(&mut |_| Ok::<_, ()>(()), &mut |a, b, control| {
                    control(GlaExecutionEvent::Work)?;
                    Ok(a.key.cmp(&b.key))
                })
                .unwrap()
                .unwrap();
            assert_eq!(first.key, 1);
            assert_eq!(drops.get(), 0);
            if refuse {
                assert!(
                    cursor
                        .next_with_control(&mut |_| Err::<(), _>(()), &mut |a, b, control| {
                            control(GlaExecutionEvent::Work)?;
                            Ok(a.key.cmp(&b.key))
                        })
                        .is_err()
                );
                assert_eq!(drops.get(), 3, "refusal releases both suffixes immediately");
            }
            drop(cursor);
            assert_eq!(drops.get(), 3);
            drop(first);
            assert_eq!(drops.get(), 4);
        }
    }
}
