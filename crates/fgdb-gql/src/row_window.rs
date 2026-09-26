//! Incremental ORDER BY/OFFSET/LIMIT over complete native row bags.
//!
//! Uses the batch GLA comparator and exact weighted window kernel. DISTINCT
//! thresholds complete integrated tuple counts before pagination. Retained
//! ordering keys share immutable rows, so kernel key clones do not copy payloads.
//! This stage neither evaluates MATCH nor certifies source completeness. It is
//! session-local algebra, not durable subscription delivery or spill storage.
//! Controls account logical work and entries, not every internal B-tree
//! comparison or allocator bytes, as in the existing batch collector.

use crate::algebra::{GraphOrderError, GraphValueOrder, GraphValueRow, MAX_PATTERN_VERTICES};
use crate::{GlaExecutionEvent, GraphSetColumnType, GraphSetQuantifier};
use fgdb_delta_types::zset::incremental::topk::{IncrementalTopK, TopKError, TopKUpdate};
use fgdb_delta_types::zset::incremental::{DistinctUpdate, IncrementalDistinct};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use std::cmp::Ordering;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowWindowBuildError {
    InputWidth { observed: usize },
    Order(GraphOrderError),
}
impl core::fmt::Display for RowWindowBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "row window definition: {self:?}")
    }
}
impl core::error::Error for RowWindowBuildError {}

/// Frozen complete input schema, quantifier, ordering and occurrence window.
/// An empty ordering uses canonical whole-row order. A zero-column schema
/// admits only the relational identity tuple and cannot name an ORDER BY key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowWindowSpec {
    input: Box<[GraphSetColumnType]>,
    order: Arc<[GraphValueOrder]>,
    quantifier: GraphSetQuantifier,
    offset: u64,
    count: u64,
}
impl RowWindowSpec {
    pub fn new(
        input: Vec<GraphSetColumnType>,
        order: Vec<GraphValueOrder>,
        quantifier: GraphSetQuantifier,
        offset: u64,
        count: u64,
    ) -> Result<Self, RowWindowBuildError> {
        if input.len() > MAX_PATTERN_VERTICES {
            return Err(RowWindowBuildError::InputWidth {
                observed: input.len(),
            });
        }
        if order.len() > MAX_PATTERN_VERTICES {
            return Err(RowWindowBuildError::Order(
                GraphOrderError::TooManyColumns {
                    limit: MAX_PATTERN_VERTICES,
                    observed: order.len(),
                },
            ));
        }
        for (at, key) in order.iter().enumerate() {
            if key.column >= input.len() {
                return Err(RowWindowBuildError::Order(GraphOrderError::UnknownColumn {
                    column: key.column,
                }));
            }
            if order[..at]
                .iter()
                .any(|previous| previous.column == key.column)
            {
                return Err(RowWindowBuildError::Order(
                    GraphOrderError::DuplicateColumn { column: key.column },
                ));
            }
        }
        Ok(Self {
            input: input.into(),
            order: order.into(),
            quantifier,
            offset,
            count,
        })
    }
    pub fn input_types(&self) -> &[GraphSetColumnType] {
        &self.input
    }
    pub fn order(&self) -> &[GraphValueOrder] {
        &self.order
    }
    pub fn quantifier(&self) -> GraphSetQuantifier {
        self.quantifier
    }
    pub fn offset(&self) -> u64 {
        self.offset
    }
    pub fn count(&self) -> u64 {
        self.count
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowWindowError<E> {
    Delta(ZSetError<E>),
    InputSchema,
    NegativeMultiplicity,
    ResultBudget { limit: u64 },
}
impl<E> From<ZSetError<E>> for RowWindowError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E> From<TopKError<E>> for RowWindowError<E> {
    fn from(error: TopKError<E>) -> Self {
        match error {
            TopKError::Delta(error) => Self::Delta(error),
            TopKError::NegativeMultiplicity => Self::NegativeMultiplicity,
        }
    }
}
impl<E: core::fmt::Display> core::fmt::Display for RowWindowError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::InputSchema => {
                f.write_str("ordered-window input does not match its bounded schema")
            }
            Self::NegativeMultiplicity => f.write_str("negative integrated ordered-window input"),
            Self::ResultBudget { limit } => {
                write!(f, "ordered-window occurrence limit {limit} exceeded")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for RowWindowError<E> {}

#[derive(Clone)]
struct OrderedRow {
    row: Arc<GraphValueRow>,
    order: Arc<[GraphValueOrder]>,
}
impl Ord for OrderedRow {
    fn cmp(&self, other: &Self) -> Ordering {
        // Each live operator has one immutable definition. Still define an
        // honest total order across definitions for Eq/Ord consistency.
        self.order.cmp(&other.order).then_with(|| {
            self.row
                .compare_incremental_window_order(&other.row, &self.order)
        })
    }
}
impl PartialOrd for OrderedRow {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for OrderedRow {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for OrderedRow {}

fn charge<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    event: ZSetEvent,
) -> Result<(), RowWindowError<E>> {
    control(event).map_err(|error| RowWindowError::Delta(ZSetError::Control(error)))
}

fn copy_row<E>(
    row: &GraphValueRow,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<GraphValueRow, RowWindowError<E>> {
    charge(control, ZSetEvent::ScratchEntry)?;
    if row.is_empty() {
        return Ok(GraphValueRow::unit());
    }
    let mut values = Vec::new();
    for value in row.values() {
        charge(control, ZSetEvent::ScratchEntry)?;
        values.push(value.copy_with_control(&mut |event| {
            charge(
                control,
                match event {
                    GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                    GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => ZSetEvent::Work,
                },
            )
        })?);
    }
    Ok(GraphValueRow::from_owned_values(values))
}

/// Native row adapter for the exact bag window. Whole-row tie breaking means
/// equal ORDER BY values never collapse different tuples. Result deltas are
/// unordered Z-sets; `rows()` exports the current ordered, compressed page.
#[derive(PartialEq, Eq)]
pub struct IncrementalRowWindow {
    spec: RowWindowSpec,
    distinct: Option<IncrementalDistinct<OrderedRow>>,
    window: IncrementalTopK<OrderedRow>,
    total: ZWeight,
}
impl IncrementalRowWindow {
    pub fn new(spec: RowWindowSpec) -> Self {
        let distinct =
            (spec.quantifier == GraphSetQuantifier::Distinct).then(IncrementalDistinct::new);
        let window = IncrementalTopK::new(spec.offset, spec.count);
        Self {
            spec,
            distinct,
            window,
            total: ZWeight::ZERO,
        }
    }
    pub fn spec(&self) -> &RowWindowSpec {
        &self.spec
    }
    pub fn total(&self) -> &ZWeight {
        &self.total
    }
    pub fn rows(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&GraphValueRow, &ZWeight)> + ExactSizeIterator {
        self.window
            .rows()
            .iter()
            .map(|(key, weight)| (key.row.as_ref(), weight))
    }

    /// Validate every changed cell, including invisible tail rows and LIMIT 0.
    /// Invalid raw retractions refuse BEFORE DISTINCT can hide them. The quota
    /// applies to final selected occurrences, not transient insertion prefixes.
    /// All payload copies and returned native delta rows precede publication.
    pub fn prepare<E>(
        &mut self,
        changes: &ZSet<GraphValueRow>,
        limbs: LimbLimit,
        max_result_rows: Option<u64>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<RowWindowUpdate<'_>, RowWindowError<E>> {
        charge(control, ZSetEvent::Work)?;
        // Complete validation before any user payload is copied.
        for (row, _) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            if row.len() != self.spec.input.len() {
                return Err(RowWindowError::InputSchema);
            }
            for (value, kind) in row.values().iter().zip(self.spec.input.iter()) {
                charge(control, ZSetEvent::Work)?;
                if !kind.accepts(value) || !value.validate_bounds() {
                    return Err(RowWindowError::InputSchema);
                }
            }
        }
        let mut keyed = Vec::new();
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            let key = OrderedRow {
                row: Arc::new(copy_row(row, control)?),
                order: Arc::clone(&self.spec.order),
            };
            let counts = self
                .distinct
                .as_ref()
                .map_or_else(|| self.window.counts(), IncrementalDistinct::counts);
            let next = match counts.weight(&key) {
                Some(old) => old.checked_add(weight, limbs),
                None => weight.checked_clone(limbs),
            }
            .map_err(ZSetError::Arithmetic)?;
            if next < ZWeight::ZERO {
                return Err(RowWindowError::NegativeMultiplicity);
            }
            let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            keyed.push((key, weight));
        }
        let keyed = ZSet::from_updates(keyed, limbs, control)?;
        let distinct = match &mut self.distinct {
            Some(operator) => Some(operator.prepare(&keyed, limbs, control)?),
            None => None,
        };
        let changes = distinct.as_ref().map_or(&keyed, DistinctUpdate::delta);
        let window = self.window.prepare(changes, limbs, control)?;
        let next_total = window.rows().total_weight(limbs, control)?;
        if let Some(limit) = max_result_rows
            && next_total > ZWeight::from_i128(i128::from(limit))
        {
            return Err(RowWindowError::ResultBudget { limit });
        }
        let mut output = Vec::new();
        for (key, weight) in window.delta().iter() {
            charge(control, ZSetEvent::Work)?;
            let row = copy_row(&key.row, control)?;
            let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
            charge(control, ZSetEvent::ScratchEntry)?;
            output.push((row, weight));
        }
        let delta = ZSet::from_updates(output, limbs, control)?;
        charge(control, ZSetEvent::Work)?;
        Ok(RowWindowUpdate {
            distinct,
            window,
            total: &mut self.total,
            next_total,
            delta,
        })
    }

    pub fn apply<E>(
        &mut self,
        changes: &ZSet<GraphValueRow>,
        limbs: LimbLimit,
        max_result_rows: Option<u64>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<ZSet<GraphValueRow>, RowWindowError<E>> {
        Ok(self
            .prepare(changes, limbs, max_result_rows, control)?
            .commit())
    }
}
impl core::fmt::Debug for IncrementalRowWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalRowWindow")
            .field("spec", &self.spec)
            .field("output_support", &self.window.rows().len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a row-window update preserves counts, page and total"]
pub struct RowWindowUpdate<'a> {
    distinct: Option<DistinctUpdate<'a, OrderedRow>>,
    window: TopKUpdate<'a, OrderedRow>,
    total: &'a mut ZWeight,
    next_total: ZWeight,
    delta: ZSet<GraphValueRow>,
}
impl RowWindowUpdate<'_> {
    pub fn delta(&self) -> &ZSet<GraphValueRow> {
        &self.delta
    }
    pub fn total(&self) -> &ZWeight {
        &self.next_total
    }
    pub fn rows(
        &self,
    ) -> impl DoubleEndedIterator<Item = (&GraphValueRow, &ZWeight)> + ExactSizeIterator {
        self.window
            .rows()
            .iter()
            .map(|(key, weight)| (key.row.as_ref(), weight))
    }
    pub fn commit(self) -> ZSet<GraphValueRow> {
        let Self {
            distinct,
            window,
            total,
            next_total,
            delta,
        } = self;
        if let Some(update) = distinct {
            let _ = update.commit();
        }
        let _ = window.commit();
        *total = next_total;
        delta
    }
}
impl core::fmt::Debug for RowWindowUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowWindowUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;
