//! Exact selection derivatives using the ordinary checked row predicate.
//!
//! A fixed predicate is linear over signed changes: filter(delta) is the exact
//! output change. Retain ALL input counts, including rejected rows, so an
//! invalid hidden retraction cannot be laundered into an empty result delta.
//! This is in-memory algebra, not another predicate interpreter or graph source.

use super::{GraphSetColumnType, GraphSetFilterError, GraphSetPredicateOp, RowPredicate};
use crate::GlaExecutionEvent;
use crate::algebra::{GraphValueRow, MAX_PATTERN_VERTICES};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowFilterBuildError {
    InputWidth { observed: usize },
    Predicate(GraphSetFilterError),
}
impl core::fmt::Display for RowFilterBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InputWidth { observed } => write!(f, "filter input width {observed} exceeds the native bound"),
            Self::Predicate(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for RowFilterBuildError {}

/// Frozen native schema and predicate. Only TRUE retains a tuple; FALSE and
/// UNKNOWN reject it. Comparisons, NULL, mixed types and eager Boolean rules
/// are owned by the same RowPredicate used by PreparedGraphSet::filter.
/// No expression, parameter, clock or catalog callback can change after binding.
#[derive(Clone, PartialEq, Eq)]
pub struct RowFilterSpec {
    types: Box<[GraphSetColumnType]>,
    predicate: RowPredicate,
}
impl RowFilterSpec {
    pub fn new(types: Vec<GraphSetColumnType>, code: &[GraphSetPredicateOp])
        -> Result<Self, RowFilterBuildError> {
        if types.len() > MAX_PATTERN_VERTICES {
            return Err(RowFilterBuildError::InputWidth { observed: types.len() });
        }
        let predicate = RowPredicate::prepare(&types, code).map_err(RowFilterBuildError::Predicate)?;
        Ok(Self { types: types.into(), predicate })
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] { &self.types }
    pub fn predicate(&self) -> &[GraphSetPredicateOp] { &self.predicate.code }
}
impl core::fmt::Debug for RowFilterSpec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowFilterSpec").field("columns", &self.types.len())
            .field("definition", &"[REDACTED]").finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowFilterError<E> {
    Delta(ZSetError<E>),
    InputSchema,
    NegativeMultiplicity,
    InvalidResult,
    ResultBudget { limit: u64 },
}
impl<E> From<ZSetError<E>> for RowFilterError<E> {
    fn from(error: ZSetError<E>) -> Self { Self::Delta(error) }
}
impl<E: core::fmt::Display> core::fmt::Display for RowFilterError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::InputSchema => f.write_str("filter input does not match its bounded schema"),
            Self::NegativeMultiplicity => f.write_str("negative integrated filter input"),
            Self::InvalidResult => f.write_str("invalid integrated filter result"),
            Self::ResultBudget { limit } => write!(f, "filter occurrence limit {limit} exceeded"),
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for RowFilterError<E> {}

fn charge<E>(control: &mut impl FnMut(ZSetEvent) -> Result<(), E>, event: ZSetEvent)
    -> Result<(), ZSetError<E>> { control(event).map_err(ZSetError::Control) }
fn reserve_row<E>(row: &GraphValueRow, control: &mut impl FnMut(ZSetEvent) -> Result<(), E>)
    -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::ScratchEntry)?;
    for value in row.values() {
        charge(control, ZSetEvent::Work)?;
        for _ in 0..=value.payload_units() { charge(control, ZSetEvent::ScratchEntry)?; }
    }
    Ok(())
}

/// An exact compressed bag selection. Evaluate only changed tuples, once per
/// tuple rather than per occurrence, including retractions. Rejected inputs
/// are retained for validation; no DISTINCT, ordering or page is introduced.
/// Logical work/payload events are not allocator-byte bounds. The retained
/// source and selected bags remain in memory; no spill is implied.
#[derive(PartialEq, Eq)]
pub struct IncrementalRowFilter {
    spec: RowFilterSpec,
    input: ZSet<GraphValueRow>,
    rows: ZSet<GraphValueRow>,
    total: ZWeight,
}
impl IncrementalRowFilter {
    pub fn new(spec: RowFilterSpec) -> Self {
        Self { spec, input: ZSet::new(), rows: ZSet::new(), total: ZWeight::ZERO }
    }
    pub fn spec(&self) -> &RowFilterSpec { &self.spec }
    pub fn rows(&self) -> &ZSet<GraphValueRow> { &self.rows }
    pub fn total(&self) -> &ZWeight { &self.total }

    /// Validate the complete changed input before predicate evaluation. Even a
    /// constant FALSE predicate and a zero output allowance cannot hide invalid
    /// rows or over-retractions. The quota counts final selected occurrences,
    /// never an insertion-before-deletion transient prefix. All accepted state
    /// is unchanged until the returned guard commits, including on cancellation.
    pub fn prepare<E>(&mut self, changes: &ZSet<GraphValueRow>, limbs: LimbLimit,
        max_result_rows: Option<u64>, control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<RowFilterUpdate<'_>, RowFilterError<E>> {
        charge(control, ZSetEvent::Work)?;
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            if row.len() != self.spec.types.len() { return Err(RowFilterError::InputSchema); }
            for (value, kind) in row.values().iter().zip(self.spec.types.iter()) {
                charge(control, ZSetEvent::Work)?;
                if !kind.accepts(value) || !value.validate_bounds() { return Err(RowFilterError::InputSchema); }
            }
            let next = match self.input.weight(row) {
                Some(old) => old.checked_add(weight, limbs),
                None => weight.checked_clone(limbs),
            }.map_err(ZSetError::Arithmetic)?;
            if next < ZWeight::ZERO { return Err(RowFilterError::NegativeMultiplicity); }
            reserve_row(row, control)?;
            reserve_row(row, control)?;
        }
        let input = self.input.prepare_update(changes, limbs, control)?;
        let mut updates = Vec::new();
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            let keep = self.spec.predicate.evaluate(row, &mut |event| charge(control, match event {
                GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => ZSetEvent::Work,
            }))?;
            if keep {
                reserve_row(row, control)?;
                let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
                charge(control, ZSetEvent::ScratchEntry)?;
                updates.push((row.clone(), weight));
            }
        }
        let delta = ZSet::from_updates(updates, limbs, control)?;
        let change = delta.total_weight(limbs, control)?;
        charge(control, ZSetEvent::Work)?;
        let next_total = self.total.checked_add(&change, limbs).map_err(ZSetError::Arithmetic)?;
        if next_total < ZWeight::ZERO { return Err(RowFilterError::InvalidResult); }
        if let Some(limit) = max_result_rows {
            if next_total > ZWeight::from_i128(i128::from(limit)) {
                return Err(RowFilterError::ResultBudget { limit });
            }
        }
        for (row, _) in delta.iter() { reserve_row(row, control)?; reserve_row(row, control)?; }
        let sink = self.rows.prepare_update(&delta, limbs, control)?;
        for (row, _) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if sink.weight(row).is_some_and(|weight| weight < &ZWeight::ZERO) {
                return Err(RowFilterError::InvalidResult);
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(RowFilterUpdate { input, sink, total: &mut self.total, next_total, delta })
    }
}
impl core::fmt::Debug for IncrementalRowFilter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalRowFilter").field("spec", &self.spec)
            .field("support", &self.rows.len()).field("data", &"[REDACTED]").finish()
    }
}

#[must_use = "dropping a filter update preserves accepted input and output"]
pub struct RowFilterUpdate<'a> {
    input: ZSetUpdate<'a, GraphValueRow>,
    sink: ZSetUpdate<'a, GraphValueRow>,
    total: &'a mut ZWeight,
    next_total: ZWeight,
    delta: ZSet<GraphValueRow>,
}
impl RowFilterUpdate<'_> {
    pub fn delta(&self) -> &ZSet<GraphValueRow> { &self.delta }
    pub fn total(&self) -> &ZWeight { &self.next_total }
    pub fn commit(self) -> ZSet<GraphValueRow> {
        let Self { input, sink, total, next_total, delta } = self;
        input.commit();
        sink.commit();
        *total = next_total;
        delta
    }
}
impl core::fmt::Debug for RowFilterUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowFilterUpdate").field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]").finish()
    }
}

#[cfg(test)]
mod tests;
