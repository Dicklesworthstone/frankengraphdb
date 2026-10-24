//! Retractable binary64 support beside the existing exact integer reducer.
//!
//! Canonical float occurrence counts live in the parent's aggregate guard.
//! This patch keeps only the changed groups' exact finite sums and retractable
//! nonfinite-class counts. DISTINCT applies whole-tick support crossings, not
//! individual arrival order. Integer support, wide sums and rational results
//! retain their existing owner; a group returns to that domain when its last
//! float disappears. No input bag or unchanged group is rescanned.

use super::*;
use fgdb_types::{CanonicalF64, ExactBinary64Sum};
use std::collections::BTreeMap;

const SIGN: u64 = 1 << 63;
const FRACTION: u64 = (1 << 52) - 1;
// The exact kernel has 34 limbs. These conservative logical reservations cover
// its copies, sign propagation and division; they are not allocator-byte claims.
const COPY_UNITS: usize = 40;
const UPDATE_WORK: usize = 80;
const RENDER_WORK: usize = 160;

pub(super) fn supports(function: Function) -> bool {
    matches!(
        function,
        Function::SumInt
            | Function::SumIntDistinct
            | Function::AverageInt
            | Function::AverageIntDistinct
    )
}

pub(super) fn function<D: GroupDefinition, E>(
    definition: &D,
    index: usize,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<Function, GroupError<E>> {
    for (at, (function, _)) in definition.aggregate_specs().enumerate() {
        charge(control, ZSetEvent::Work)?;
        if at == index {
            return Ok(function);
        }
    }
    Err(GroupError::InvalidResult)
}

fn distinct(function: Function) -> bool {
    matches!(
        function,
        Function::SumIntDistinct | Function::AverageIntDistinct
    )
}

fn units<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    event: ZSetEvent,
    amount: usize,
) -> Result<(), GroupError<E>> {
    for _ in 0..amount {
        charge(control, event)?;
    }
    Ok(())
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Summary {
    // This accumulator NEVER receives a NaN or infinity: its special flags
    // would be irreversible. Separate counters below make removal exact.
    finite: ExactBinary64Sum,
    count: u64,
    positive_infinity: u64,
    negative_infinity: u64,
    nan: u64,
}

fn adjust<E>(old: u64, times: u64, remove: bool) -> Result<u64, GroupError<E>> {
    if remove {
        old.checked_sub(times)
            .ok_or(GroupError::NegativeMultiplicity)
    } else {
        old.checked_add(times).ok_or(GroupError::Arithmetic)
    }
}

impl Summary {
    fn apply<E>(
        &mut self,
        value: CanonicalF64,
        times: u64,
        remove: bool,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<(), GroupError<E>> {
        units(control, ZSetEvent::Work, UPDATE_WORK)?;
        self.count = adjust(self.count, times, remove)?;
        let bits = value.to_bits();
        if (bits >> 52) & 0x7ff == 0x7ff {
            let counter = if bits & FRACTION != 0 {
                &mut self.nan
            } else if bits & SIGN != 0 {
                &mut self.negative_infinity
            } else {
                &mut self.positive_infinity
            };
            *counter = adjust(*counter, times, remove)?;
        } else {
            // Flip the exact sign bit, not a rounded arithmetic result. The
            // canonical constructor also handles the negative-zero encoding.
            let signed = CanonicalF64::new(f64::from_bits(if remove { bits ^ SIGN } else { bits }));
            self.finite.add_repeated(signed, times);
        }
        Ok(())
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) struct State {
    groups: BTreeMap<Key, Summary>,
}

impl State {
    pub(super) fn prepare<D: GroupDefinition, E>(
        &mut self,
        definition: &D,
        aggregate: &IncrementalAggregate<Key>,
        delta: &Contributions,
        limbs: LimbLimit,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Update<'_>, GroupError<E>> {
        let mut steps = Vec::new();
        for ((key, _), change) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            let Some(GraphValue::Scalar(CanonicalScalar::Float(value))) = key.2.as_deref() else {
                continue;
            };
            let function = function(definition, key.1, control)?;
            if !supports(function) {
                continue;
            }
            let difference = if distinct(function) {
                let old = aggregate.get(key).map(AggregateValues::count_rows);
                let next = match old {
                    Some(old) => old.checked_add(change, limbs),
                    None => change.checked_clone(limbs),
                }
                .map_err(ZSetError::Arithmetic)?;
                if next < ZWeight::ZERO {
                    return Err(GroupError::NegativeMultiplicity);
                }
                i128::from(!next.is_zero()) - i128::from(old.is_some_and(|n| !n.is_zero()))
            } else {
                change.to_i128().ok_or(GroupError::Arithmetic)?
            };
            if difference != 0 {
                let times =
                    u64::try_from(difference.unsigned_abs()).map_err(|_| GroupError::Arithmetic)?;
                charge(control, ZSetEvent::ScratchEntry)?;
                steps.push((primary(&key.0, key.1), *value, times, difference < 0));
            }
        }
        let mut patch = BTreeMap::new();
        // Delete before insert, across the WHOLE tick. A replacement at the
        // u64 count ceiling must not overflow merely because its added value
        // sorts before the retracted value. Intermediate states remain private.
        for remove in [true, false] {
            for (key, value, times, retract) in &steps {
                charge(control, ZSetEvent::Work)?;
                if *retract != remove {
                    continue;
                }
                if !patch.contains_key(key) {
                    units(control, ZSetEvent::ScratchEntry, COPY_UNITS)?;
                    units(control, ZSetEvent::Work, COPY_UNITS)?;
                    patch.insert(
                        key.clone(),
                        self.groups.get(key).cloned().unwrap_or_default(),
                    );
                }
                patch
                    .get_mut(key)
                    .ok_or(GroupError::InvalidResult)?
                    .apply(*value, *times, remove, control)?;
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(Update { state: self, patch })
    }
}

pub(super) struct Update<'a> {
    state: &'a mut State,
    // A zero count shadows retained support until commit, then removes it.
    patch: BTreeMap<Key, Summary>,
}

impl Update<'_> {
    pub(super) fn render<E>(
        &self,
        key: &Key,
        function: Function,
        integers: Option<&AggregateValues>,
        pending: bool,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<Option<Value>, GroupError<E>> {
        charge(control, ZSetEvent::Work)?;
        let state =
            if pending { self.patch.get(key) } else { None }.or_else(|| self.state.groups.get(key));
        let Some(state) = state.filter(|state| state.count != 0) else {
            return Ok(None);
        };
        let average = matches!(
            function,
            Function::AverageInt | Function::AverageIntDistinct
        );
        let distinct = distinct(function);
        let sum = integers.and_then(|integers| {
            if distinct {
                integers.sum_distinct()
            } else {
                integers.sum()
            }
        });
        let sum = sum.map_or(Ok(0), |sum| sum.to_i128().ok_or(GroupError::Arithmetic))?;
        let denominator = if average {
            let integer_count = integers.map_or(Ok(0), |integers| {
                count(if distinct {
                    integers.count_distinct()
                } else {
                    integers.count_values()
                })
            })?;
            state
                .count
                .checked_add(integer_count)
                .ok_or(GroupError::Arithmetic)?
        } else {
            1
        };
        units(control, ZSetEvent::Work, RENDER_WORK)?;
        units(control, ZSetEvent::ScratchEntry, COPY_UNITS)?;
        let result =
            if state.nan != 0 || (state.positive_infinity != 0 && state.negative_infinity != 0) {
                CanonicalF64::new(f64::NAN)
            } else if state.positive_infinity != 0 {
                CanonicalF64::new(f64::INFINITY)
            } else if state.negative_infinity != 0 {
                CanonicalF64::new(f64::NEG_INFINITY)
            } else {
                let mut exact = state.finite.clone();
                exact.add_integer(sum);
                if average {
                    exact.mean(denominator).ok_or(GroupError::InvalidResult)?
                } else {
                    exact.sum()
                }
            };
        Ok(Some(Value::Value(GraphValue::Scalar(
            CanonicalScalar::Float(result),
        ))))
    }

    pub(super) fn commit(self) {
        for (key, value) in self.patch {
            if value.count == 0 {
                self.state.groups.remove(&key);
            } else {
                self.state.groups.insert(key, value);
            }
        }
    }
}
