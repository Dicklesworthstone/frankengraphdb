//! Sealed output shapes for the shared binding-row evaluator.
//!
//! A tuple is one correlated assignment projection, never a zip or product of
//! independently evaluated columns. The compiler alone constructs typed plans.

use super::{GlaOperator, MAX_PATTERN_VERTICES};
use crate::GlaExecutionEvent;
use fgdb_types::VId;
use std::collections::BTreeSet;

/// One nonempty, ordered projection of vertex bindings. Column positions match
/// the immutable prepared pattern's `columns()`; Debug never prints identities.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphBindingRow {
    values: Box<[VId]>,
}

impl GraphBindingRow {
    #[must_use]
    pub fn values(&self) -> &[VId] {
        &self.values
    }

    #[must_use]
    pub fn get(&self, column: usize) -> Option<VId> {
        self.values.get(column).copied()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl std::borrow::Borrow<[VId]> for GraphBindingRow {
    fn borrow(&self) -> &[VId] {
        self.values()
    }
}

impl core::fmt::Debug for GraphBindingRow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphBindingRow")
            .field("columns", &self.len())
            .field("values", &"[REDACTED]")
            .finish()
    }
}

/// Closed output domain of GLA. Applications cannot add an output collector or
/// turn a scalar plan into a tuple plan. Existing APIs default to `VId`.
pub trait GlaOutput: sealed::Projection + Clone + Ord {}
impl GlaOutput for VId {}
impl GlaOutput for GraphBindingRow {}

mod sealed {
    use super::*;

    pub trait Projection: Sized + Ord {
        fn collect<E>(
            operator: &GlaOperator,
            bindings: &[VId],
            projected: &mut BTreeSet<Self>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E>;
    }

    impl Projection for VId {
        fn collect<E>(
            operator: &GlaOperator,
            bindings: &[VId],
            projected: &mut BTreeSet<Self>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            let GlaOperator::Project { slot } = operator else {
                unreachable!("the private scalar-plan constructor owns its projection shape")
            };
            let value = bindings[slot.ordinal() as usize];
            if !projected.contains(&value) {
                control(GlaExecutionEvent::ScratchEntry)?;
                projected.insert(value);
            }
            Ok(())
        }
    }

    impl Projection for GraphBindingRow {
        fn collect<E>(
            operator: &GlaOperator,
            bindings: &[VId],
            projected: &mut BTreeSet<Self>,
            control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
        ) -> Result<(), E> {
            let GlaOperator::ProjectBindings { slots } = operator else {
                unreachable!("the private tuple-plan constructor owns its projection shape")
            };
            // The compiler caps columns at MAX_PATTERN_VERTICES. Form the
            // lookup key on the stack; duplicate path occurrences allocate no
            // new tuple. Every selected cell is a work/cancellation checkpoint.
            let mut key = [VId(0); MAX_PATTERN_VERTICES];
            for (column, slot) in slots.iter().enumerate() {
                control(GlaExecutionEvent::Work)?;
                key[column] = bindings[slot.ordinal() as usize];
            }
            let key = &key[..slots.len()];
            if projected.contains(key) {
                return Ok(());
            }
            // One set entry plus every owned cell is charged before growth.
            // A refusal drops the private prefix; it never releases a short row.
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for value in key {
                control(GlaExecutionEvent::ScratchEntry)?;
                values.push(*value);
            }
            projected.insert(GraphBindingRow {
                values: values.into_boxed_slice(),
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::sealed::Projection;
    use super::*;
    use crate::algebra::BindingSlot;

    #[test]
    fn tuple_collection_keeps_correlations_and_deduplicates_complete_rows() {
        let op = GlaOperator::ProjectBindings {
            slots: vec![BindingSlot(1), BindingSlot(0)],
        };
        let mut rows = BTreeSet::new();
        let mut scratch = 0;
        for bindings in [[VId(1), VId(3)], [VId(2), VId(3)], [VId(1), VId(3)]] {
            GraphBindingRow::collect(&op, &bindings, &mut rows, &mut |event| {
                scratch += usize::from(event == GlaExecutionEvent::ScratchEntry);
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(
            rows.iter().map(|r| r.values()).collect::<Vec<_>>(),
            vec![&[VId(3), VId(1)][..], &[VId(3), VId(2)][..]]
        );
        assert_eq!(scratch, 6, "two rows, each one entry plus two owned cells");
        assert!(!format!("{rows:?}").contains("VId"));
        let first = rows.first().unwrap();
        assert_eq!(first.get(0), Some(VId(3)));
        assert_eq!(first.get(2), None);
        assert_eq!(first.len(), 2);
        assert!(!first.is_empty());
    }

    #[test]
    fn every_tuple_checkpoint_refuses_before_publishing_an_incomplete_row() {
        let op = GlaOperator::ProjectBindings {
            slots: vec![BindingSlot(0), BindingSlot(1)],
        };
        for stop in 1..=5 {
            let mut rows = BTreeSet::new();
            let mut calls = 0;
            let result = GraphBindingRow::collect(&op, &[VId(1), VId(2)], &mut rows, &mut |_| {
                calls += 1;
                if calls == stop { Err(stop) } else { Ok(()) }
            });
            assert_eq!(result, Err(stop));
            assert_eq!(calls, stop);
            assert!(rows.is_empty());
        }
    }
}
