//! Exact derivatives of UNION, INTERSECT and EXCEPT over nonnegative bags.
//!
//! Signed input deltas remain Z-sets; only their integrated input multiplicities
//! must be nonnegative. For each changed key, output is f(new_left, new_right)
//! minus f(old_left, old_right). Both sides of a tick are considered together,
//! so DISTINCT and EXCEPT never expose transient absence or false insertions.
//! Only changed keys are visited. The exclusive-borrow guard lets downstream
//! sinks prepare before any retained input changes; drop/refusal rolls back.
//! This is in-process algebra, not storage, a scheduler or a durable format.

use super::{ZSet, ZSetError, ZSetEvent, admit, event};
use crate::{LimbLimit, ZWeight};
use std::collections::BTreeMap;

/// Per-key bag laws. DISTINCT tests integrated support, never delta signs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetOperation {
    UnionAll,
    UnionDistinct,
    IntersectAll,
    IntersectDistinct,
    ExceptAll,
    ExceptDistinct,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetInput { Left, Right }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetError<E> {
    Delta(ZSetError<E>),
    /// Over-retraction is invalid even when the output would remain empty.
    /// Neither the key nor its multiplicity is exposed in this diagnostic.
    NegativeMultiplicity { input: SetInput },
}
impl<E> From<ZSetError<E>> for SetError<E> {
    fn from(error: ZSetError<E>) -> Self { Self::Delta(error) }
}
impl<E: core::fmt::Display> core::fmt::Display for SetError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::NegativeMultiplicity { input } => write!(f, "negative integrated {input:?} set input"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for SetError<E> {}

/// Retains complete input counts, including duplicates hidden by DISTINCT,
/// suppressed INTERSECT rows, and EXCEPT's blocking right-hand support. No
/// output copy is retained: callers integrate the exact derivative into a sink.
#[derive(PartialEq, Eq)]
pub struct IncrementalSet<T: Ord> {
    operation: SetOperation,
    left: ZSet<T>,
    right: ZSet<T>,
}
impl<T: Ord> IncrementalSet<T> {
    pub fn new(operation: SetOperation) -> Self {
        Self { operation, left: ZSet::new(), right: ZSet::new() }
    }
    pub fn operation(&self) -> SetOperation { self.operation }
    pub fn left_counts(&self) -> &ZSet<T> { &self.left }
    pub fn right_counts(&self) -> &ZSet<T> { &self.right }
}
impl<T: Ord> core::fmt::Debug for IncrementalSet<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalSet")
            .field("operation", &self.operation)
            .field("left_support", &self.left.len())
            .field("right_support", &self.right.len())
            .field("data", &"[REDACTED]").finish()
    }
}

impl SetOperation {
    fn weight<E>(
        self, left: &ZWeight, right: &ZWeight, limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZWeight, ZSetError<E>> {
        event(control, ZSetEvent::Work)?;
        admit(left, limbs)?;
        admit(right, limbs)?;
        let present = match self {
            Self::UnionDistinct => Some(!left.is_zero() || !right.is_zero()),
            Self::IntersectDistinct => Some(!left.is_zero() && !right.is_zero()),
            Self::ExceptDistinct => Some(!left.is_zero() && right.is_zero()),
            _ => None,
        };
        if let Some(present) = present {
            return Ok(if present { ZWeight::ONE } else { ZWeight::ZERO });
        }
        match self {
            Self::UnionAll => left.checked_add(right, limbs),
            Self::IntersectAll => left.min(right).checked_clone(limbs),
            Self::ExceptAll if left > right => {
                let inverse = right.checked_neg(limbs).map_err(ZSetError::Arithmetic)?;
                event(control, ZSetEvent::Work)?;
                left.checked_add(&inverse, limbs)
            }
            Self::ExceptAll => Ok(ZWeight::ZERO),
            _ => unreachable!("DISTINCT laws returned above"),
        }.map_err(ZSetError::Arithmetic)
    }
}

impl<T: Ord + Clone> IncrementalSet<T> {
    /// Prepare one complete tick. Limb admission and controls cover every
    /// visited weight and logical staging/output entry. They do not re-admit
    /// unrelated retained weights or account arbitrary key cloning/comparison,
    /// allocator bytes or collection allocation failure (the ZSet boundary).
    pub fn prepare<E>(
        &mut self, delta_left: &ZSet<T>, delta_right: &ZSet<T>, limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<SetUpdate<'_, T>, SetError<E>> {
        event(control, ZSetEvent::Work)?;
        let left = self.left.prepare_integration(delta_left, limbs, control)?;
        let right = self.right.prepare_integration(delta_right, limbs, control)?;
        let mut l = left.keys().peekable();
        let mut r = right.keys().peekable();
        let mut delta = ZSet::new();
        let zero = ZWeight::ZERO;
        loop {
            // Merge the changed-key sets in canonical order, visiting keys
            // changed on both sides once. No integrated relation is scanned.
            event(control, ZSetEvent::Work)?;
            let key = match (l.peek(), r.peek()) {
                (None, None) => break,
                (Some(_), None) => l.next(),
                (None, Some(_)) => r.next(),
                (Some(a), Some(b)) => match a.cmp(b) {
                    core::cmp::Ordering::Less => l.next(),
                    core::cmp::Ordering::Greater => r.next(),
                    core::cmp::Ordering::Equal => { r.next(); l.next() }
                },
            }.expect("at least one changed key");
            let old_left = self.left.weight(key).unwrap_or(&zero);
            let old_right = self.right.weight(key).unwrap_or(&zero);
            let next_left = left.get(key).unwrap_or(old_left);
            let next_right = right.get(key).unwrap_or(old_right);
            for (input, next) in [(SetInput::Left, next_left), (SetInput::Right, next_right)] {
                event(control, ZSetEvent::Work)?;
                if next < &ZWeight::ZERO { return Err(SetError::NegativeMultiplicity { input }); }
            }
            let before = self.operation.weight(old_left, old_right, limbs, control)?;
            let after = self.operation.weight(next_left, next_right, limbs, control)?;
            event(control, ZSetEvent::Work)?;
            let inverse = before.checked_neg(limbs).map_err(ZSetError::Arithmetic)?;
            event(control, ZSetEvent::Work)?;
            let change = after.checked_add(&inverse, limbs).map_err(ZSetError::Arithmetic)?;
            if !change.is_zero() {
                event(control, ZSetEvent::ScratchEntry)?;
                delta.accumulate(key.clone(), change, limbs, control)?;
            }
        }
        event(control, ZSetEvent::Work)?;
        Ok(SetUpdate { owner: self, left, right, delta })
    }

    pub fn apply<E>(
        &mut self, delta_left: &ZSet<T>, delta_right: &ZSet<T>, limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<T>, SetError<E>> {
        Ok(self.prepare(delta_left, delta_right, limbs, control)?.commit())
    }
}

/// Tentative input replacement and output delta. Neither is published until
/// commit; a downstream resource or semantic refusal can simply drop this.
#[must_use = "dropping a set update aborts it"]
pub struct SetUpdate<'a, T: Ord> {
    owner: &'a mut IncrementalSet<T>,
    left: BTreeMap<T, ZWeight>,
    right: BTreeMap<T, ZWeight>,
    delta: ZSet<T>,
}
impl<T: Ord> SetUpdate<'_, T> {
    pub fn delta(&self) -> &ZSet<T> { &self.delta }
}
impl<T: Ord + Clone> SetUpdate<'_, T> {
    /// No fallible arithmetic or callback remains. Arbitrary key code and
    /// standard-library allocation have the same boundary as ZSet::integrate.
    pub fn commit(self) -> ZSet<T> {
        let Self { owner, left, right, delta } = self;
        owner.left.publish(left);
        owner.right.publish(right);
        delta
    }
}
impl<T: Ord> core::fmt::Debug for SetUpdate<'_, T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SetUpdate").field("changed_support", &self.delta.len())
            .field("data", &"[REDACTED]").finish()
    }
}

#[cfg(test)]
mod tests;
