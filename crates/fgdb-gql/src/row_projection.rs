//! Exact selection/projection derivatives over complete native row bags.
//!
//! Expressions use the existing relational evaluator, not another interpreter.
//! Each changed input tuple is evaluated once, independent of its multiplicity.
//! UNWIND expands list elements, never input occurrences, through the same sink.
//! DISTINCT thresholds integrated projected support, never signed delta values.
//! Complete input counts remain available to reject invalid retractions even
//! when their projected images cancel or a predicate hides them. Selection
//! uses the same eager three-valued predicate kernel as snapshot execution.
//! No graph source or scheduler lives here.

use crate::algebra::{GraphValueRow, MAX_PATTERN_NAME_BYTES, MAX_PATTERN_VERTICES};
use crate::{
    GlaExecutionEvent, GraphIntegerError, GraphSetColumnType, GraphSetFilterError,
    GraphSetPredicateOp, GraphSetProjection, GraphSetProjectionError, GraphSetQuantifier,
    GraphSetValue,
};
use fgdb_delta_types::zset::ZSetUpdate;
use fgdb_delta_types::zset::incremental::{DistinctUpdate, IncrementalDistinct};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};
use std::collections::BTreeSet;

mod unwind;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowProjectionBuildError {
    InputWidth { observed: usize },
    ColumnCount { input: usize, columns: usize },
    Projection(GraphSetProjectionError),
    Filter(GraphSetFilterError),
}
impl core::fmt::Display for RowProjectionBuildError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "row projection definition: {self:?}")
    }
}
impl core::error::Error for RowProjectionBuildError {}
impl From<GraphSetProjectionError> for RowProjectionBuildError {
    fn from(error: GraphSetProjectionError) -> Self {
        Self::Projection(error)
    }
}
impl From<GraphSetFilterError> for RowProjectionBuildError {
    fn from(error: GraphSetFilterError) -> Self { Self::Filter(error) }
}

/// Frozen input schema and checked native expressions. All expressions refer
/// to the original input, never an earlier alias in this same projection.
/// Zero-column inputs are allowed for a relational singleton. Output names
/// and expressions obey exactly the existing GraphSetProjection admission.
#[derive(Clone, PartialEq, Eq)]
pub struct RowProjectionSpec {
    input: Box<[GraphSetColumnType]>,
    projection: Box<[GraphSetProjection]>,
    types: Box<[GraphSetColumnType]>,
    quantifier: GraphSetQuantifier,
    filter: Box<[GraphSetPredicateOp]>,
    // Only the checked UNWIND constructor expands the last evaluated cell.
    expand_last: bool,
}
impl RowProjectionSpec {
    pub fn new(
        input: Vec<GraphSetColumnType>,
        projection: Vec<GraphSetProjection>,
        quantifier: GraphSetQuantifier,
    ) -> Result<Self, RowProjectionBuildError> {
        if input.len() > MAX_PATTERN_VERTICES {
            return Err(RowProjectionBuildError::InputWidth {
                observed: input.len(),
            });
        }
        if projection.is_empty() {
            return Err(GraphSetProjectionError::Empty.into());
        }
        if projection.len() > MAX_PATTERN_VERTICES {
            return Err(GraphSetProjectionError::TooManyColumns {
                limit: MAX_PATTERN_VERTICES,
                observed: projection.len(),
            }
            .into());
        }
        let mut names = BTreeSet::new();
        let mut types = Vec::new();
        for (column, output) in projection.iter().enumerate() {
            GraphSetProjection::validate_output_name(output.name(), column)?;
            if !names.insert(output.name()) {
                return Err(GraphSetProjectionError::DuplicateName { column }.into());
            }
            types.push(GraphSetProjection::admit_output(
                output.value(),
                &input,
                column,
            )?);
        }
        Ok(Self {
            input: input.into(),
            projection: projection.into(),
            types: types.into(),
            quantifier,
            filter: Box::new([]),
            expand_last: false,
        })
    }

    /// Select complete input rows without renaming, deduplicating or changing
    /// their value domains. Names are inherited metadata, not new expressions:
    /// duplicate names and non-identifier aliases remain intact. Their byte
    /// bounds and the complete schema/predicate are checked before execution.
    /// A zero-column relation is legal and still has occurrence multiplicity.
    pub fn selection(
        input: Vec<GraphSetColumnType>,
        columns: Vec<String>,
        code: &[GraphSetPredicateOp],
    ) -> Result<Self, RowProjectionBuildError> {
        if input.len() > MAX_PATTERN_VERTICES {
            return Err(RowProjectionBuildError::InputWidth {
                observed: input.len(),
            });
        }
        if columns.len() != input.len() {
            return Err(RowProjectionBuildError::ColumnCount {
                input: input.len(),
                columns: columns.len(),
            });
        }
        for (column, name) in columns.iter().enumerate() {
            if name.len() > MAX_PATTERN_NAME_BYTES {
                return Err(GraphSetProjectionError::InvalidName { column }.into());
            }
        }
        GraphSetPredicateOp::validate_schema(&input, code)?;
        let projection = columns
            .into_iter()
            .enumerate()
            .map(|(column, name)| GraphSetProjection::new(name, GraphSetValue::Column(column)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ok(Self {
            types: input.clone().into(),
            input: input.into(),
            projection,
            quantifier: GraphSetQuantifier::All,
            filter: code.to_vec().into_boxed_slice(),
            expand_last: false,
        })
    }

    /// Set the input predicate, replacing any earlier selection. It sees the
    /// original input before output expressions and DISTINCT, not output aliases.
    /// FALSE and UNKNOWN skip expression evaluation but retain source counts.
    pub fn with_filter(
        mut self,
        code: &[GraphSetPredicateOp],
    ) -> Result<Self, RowProjectionBuildError> {
        GraphSetPredicateOp::validate_schema(&self.input, code)?;
        self.filter = code.to_vec().into_boxed_slice();
        Ok(self)
    }
    pub fn input_types(&self) -> &[GraphSetColumnType] {
        &self.input
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }
    pub fn columns(&self) -> impl ExactSizeIterator<Item = &str> {
        self.projection.iter().map(GraphSetProjection::name)
    }
    pub fn quantifier(&self) -> GraphSetQuantifier {
        self.quantifier
    }
}
impl core::fmt::Debug for RowProjectionSpec {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowProjectionSpec")
            .field("columns", &self.types.len())
            .field("quantifier", &self.quantifier)
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowProjectionError<E> {
    Delta(ZSetError<E>),
    InputSchema,
    NegativeMultiplicity,
    Expression {
        column: usize,
        error: GraphIntegerError,
    },
    InvalidResult,
    ResultBudget {
        limit: u64,
    },
}
impl<E> From<ZSetError<E>> for RowProjectionError<E> {
    fn from(error: ZSetError<E>) -> Self {
        Self::Delta(error)
    }
}
impl<E: core::fmt::Display> core::fmt::Display for RowProjectionError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Delta(error) => error.fmt(f),
            Self::InputSchema => f.write_str("projection input does not match its bounded schema"),
            Self::NegativeMultiplicity => f.write_str("negative integrated projection input"),
            Self::Expression { column, error } => write!(f, "projection column {column}: {error}"),
            Self::InvalidResult => f.write_str("invalid integrated projection result"),
            Self::ResultBudget { limit } => {
                write!(f, "projection occurrence limit {limit} exceeded")
            }
        }
    }
}
impl<E: core::error::Error + 'static> core::error::Error for RowProjectionError<E> {}

fn charge<E>(
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    event: ZSetEvent,
) -> Result<(), ZSetError<E>> {
    control(event).map_err(ZSetError::Control)
}
fn reserve_row<E>(
    row: &GraphValueRow,
    control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
) -> Result<(), ZSetError<E>> {
    charge(control, ZSetEvent::ScratchEntry)?;
    for value in row.values() {
        charge(control, ZSetEvent::Work)?;
        for _ in 0..=value.payload_units() {
            charge(control, ZSetEvent::ScratchEntry)?;
        }
    }
    Ok(())
}

/// One in-memory exact selection/map/UNWIND/DISTINCT stage. Source tuples, projected support,
/// final rows and occurrence total publish together. Only changed keys are
/// visited; no repeated occurrence expansion or full-result differencing.
/// Logical events/payload units are not allocator-byte or spill bounds.
#[derive(PartialEq, Eq)]
pub struct IncrementalRowProjection {
    spec: RowProjectionSpec,
    input: ZSet<GraphValueRow>,
    distinct: Option<IncrementalDistinct<GraphValueRow>>,
    rows: ZSet<GraphValueRow>,
    total: ZWeight,
}
impl IncrementalRowProjection {
    pub fn new(spec: RowProjectionSpec) -> Self {
        let distinct =
            (spec.quantifier == GraphSetQuantifier::Distinct).then(IncrementalDistinct::new);
        Self {
            spec,
            input: ZSet::new(),
            distinct,
            rows: ZSet::new(),
            total: ZWeight::ZERO,
        }
    }
    pub fn spec(&self) -> &RowProjectionSpec {
        &self.spec
    }
    pub fn rows(&self) -> &ZSet<GraphValueRow> {
        &self.rows
    }
    pub fn total(&self) -> &ZWeight {
        &self.total
    }

    /// Invalid individual input counts refuse before evaluating expressions.
    /// Predicates run once per changed tuple; output expressions run only for
    /// retained tuples, including retractions. Hidden tuples still participate
    /// in input admission and atomic publication. Errors contain no row data.
    /// The result limit checks FINAL occurrences after collision consolidation
    /// and DISTINCT, not an insertion-first transient prefix.
    pub fn prepare<E>(
        &mut self,
        changes: &ZSet<GraphValueRow>,
        limbs: LimbLimit,
        max_result_rows: Option<u64>,
        control: &mut impl FnMut(ZSetEvent) -> Result<(), E>,
    ) -> Result<RowProjectionUpdate<'_>, RowProjectionError<E>> {
        charge(control, ZSetEvent::Work)?;
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            if row.len() != self.spec.input.len() {
                return Err(RowProjectionError::InputSchema);
            }
            for (value, kind) in row.values().iter().zip(self.spec.input.iter()) {
                charge(control, ZSetEvent::Work)?;
                if !kind.accepts(value) || !value.validate_bounds() {
                    return Err(RowProjectionError::InputSchema);
                }
            }
            let next = match self.input.weight(row) {
                Some(old) => old.checked_add(weight, limbs),
                None => weight.checked_clone(limbs),
            }
            .map_err(ZSetError::Arithmetic)?;
            if next < ZWeight::ZERO {
                return Err(RowProjectionError::NegativeMultiplicity);
            }
            reserve_row(row, control)?;
            reserve_row(row, control)?;
        }
        let input = self.input.prepare_update(changes, limbs, control)?;
        let mut updates = Vec::new();
        for (row, weight) in changes.iter() {
            charge(control, ZSetEvent::Work)?;
            if !self.spec.filter.is_empty()
                && !GraphSetPredicateOp::evaluate_row_with_control(
                    &self.spec.filter,
                    row,
                    &mut |event| {
                        charge(
                            control,
                            match event {
                                GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                                GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                                    ZSetEvent::Work
                                }
                            },
                        )
                    },
                )?
            {
                continue;
            }
            let row = GraphSetProjection::evaluate_row_with_control(
                row,
                &self.spec.projection,
                &mut |event| {
                    charge(
                        control,
                        match event {
                            GlaExecutionEvent::ScratchEntry => ZSetEvent::ScratchEntry,
                            // This is one compressed value, not occurrence delivery.
                            GlaExecutionEvent::Work | GlaExecutionEvent::ResultRow => {
                                ZSetEvent::Work
                            }
                        },
                    )
                    .map_err(RowProjectionError::Delta)
                },
                |column, error| RowProjectionError::Expression { column, error },
            )?;
            if self.spec.expand_last {
                unwind::append(&row, weight, limbs, control, &mut updates)?;
            } else {
                let weight = weight.checked_clone(limbs).map_err(ZSetError::Arithmetic)?;
                charge(control, ZSetEvent::ScratchEntry)?;
                updates.push((row, weight));
            }
        }
        let mapped = ZSet::from_updates(updates, limbs, control)?;
        // DISTINCT can clone a mapped key into counts, its derivative and the
        // returned derivative. Reserve those payloads before generic operations.
        if self.distinct.is_some() {
            for (row, _) in mapped.iter() {
                for _ in 0..3 {
                    reserve_row(row, control)?;
                }
            }
        }
        let distinct = match &mut self.distinct {
            Some(operator) => Some(operator.prepare(&mapped, limbs, control)?),
            None => None,
        };
        let delta = match &distinct {
            Some(update) => update.delta().checked_clone(limbs, control)?,
            None => mapped,
        };
        let change = delta.total_weight(limbs, control)?;
        charge(control, ZSetEvent::Work)?;
        let next_total = self
            .total
            .checked_add(&change, limbs)
            .map_err(ZSetError::Arithmetic)?;
        if next_total < ZWeight::ZERO {
            return Err(RowProjectionError::InvalidResult);
        }
        if let Some(limit) = max_result_rows {
            if next_total > ZWeight::from_i128(i128::from(limit)) {
                return Err(RowProjectionError::ResultBudget { limit });
            }
        }
        for (row, _) in delta.iter() {
            reserve_row(row, control)?;
            reserve_row(row, control)?;
        }
        let sink = self.rows.prepare_update(&delta, limbs, control)?;
        for (row, _) in delta.iter() {
            charge(control, ZSetEvent::Work)?;
            if sink
                .weight(row)
                .is_some_and(|weight| weight < &ZWeight::ZERO)
            {
                return Err(RowProjectionError::InvalidResult);
            }
        }
        charge(control, ZSetEvent::Work)?;
        Ok(RowProjectionUpdate {
            input,
            distinct,
            sink,
            total: &mut self.total,
            next_total,
            delta,
        })
    }
}
impl core::fmt::Debug for IncrementalRowProjection {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IncrementalRowProjection")
            .field("spec", &self.spec)
            .field("support", &self.rows.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[must_use = "dropping a projection update preserves all accepted arrangements"]
pub struct RowProjectionUpdate<'a> {
    input: ZSetUpdate<'a, GraphValueRow>,
    distinct: Option<DistinctUpdate<'a, GraphValueRow>>,
    sink: ZSetUpdate<'a, GraphValueRow>,
    total: &'a mut ZWeight,
    next_total: ZWeight,
    delta: ZSet<GraphValueRow>,
}
impl RowProjectionUpdate<'_> {
    pub fn delta(&self) -> &ZSet<GraphValueRow> {
        &self.delta
    }
    pub fn total(&self) -> &ZWeight {
        &self.next_total
    }
    pub fn commit(self) -> ZSet<GraphValueRow> {
        let Self {
            input,
            distinct,
            sink,
            total,
            next_total,
            delta,
        } = self;
        input.commit();
        if let Some(update) = distinct {
            let _ = update.commit();
        }
        sink.commit();
        *total = next_total;
        delta
    }
}
impl core::fmt::Debug for RowProjectionUpdate<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RowProjectionUpdate")
            .field("delta_support", &self.delta.len())
            .field("data", &"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod filter_tests;
