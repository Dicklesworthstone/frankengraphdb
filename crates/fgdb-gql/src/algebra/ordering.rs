//! Explicit ordering of complete projected value rows. Column positions refer
//! to the public output schema, not hidden bindings. Canonical whole-row order
//! breaks remaining ties, independently of input enumeration order.

use super::{GlaOperator, GlaPlan, GraphValueRow, MAX_PATTERN_VERTICES};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphValueOrder {
    pub column: usize,
    pub descending: bool,
    pub nulls_first: bool,
}

impl GraphValueOrder {
    #[must_use]
    pub const fn ascending(column: usize) -> Self {
        Self { column, descending: false, nulls_first: false }
    }

    #[must_use]
    pub const fn descending(column: usize) -> Self {
        Self { column, descending: true, nulls_first: false }
    }

    #[must_use]
    pub const fn with_nulls_first(mut self, nulls_first: bool) -> Self {
        self.nulls_first = nulls_first;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphOrderError {
    EmptyOrder,
    TooManyColumns { limit: usize, observed: usize },
    UnknownColumn { column: usize },
    DuplicateColumn { column: usize },
    InvalidPlanShape,
}

impl core::fmt::Display for GraphOrderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyOrder => f.write_str("ORDER BY requires a projected column"),
            Self::TooManyColumns { limit, observed } => write!(f, "ORDER BY has {observed} columns, limit {limit}"),
            Self::UnknownColumn { column } => write!(f, "ORDER BY references unknown output column {column}"),
            Self::DuplicateColumn { column } => write!(f, "ORDER BY repeats output column {column}"),
            Self::InvalidPlanShape => f.write_str("ORDER BY requires a compiler-owned value projection"),
        }
    }
}
impl core::error::Error for GraphOrderError {}

impl GlaPlan<GraphValueRow> {
    pub(super) fn set_value_order(&mut self, order: &[GraphValueOrder]) -> Result<(), GraphOrderError> {
        if order.is_empty() { return Err(GraphOrderError::EmptyOrder); }
        if order.len() > MAX_PATTERN_VERTICES {
            return Err(GraphOrderError::TooManyColumns { limit: MAX_PATTERN_VERTICES, observed: order.len() });
        }
        let order_at = self.operators.len().checked_sub(2).ok_or(GraphOrderError::InvalidPlanShape)?;
        if !matches!(self.operators.get(order_at),
            Some(GlaOperator::OrderByValues | GlaOperator::OrderByValueColumns { .. }))
            || !matches!(self.operators.last(), Some(GlaOperator::Limit { .. }))
        {
            return Err(GraphOrderError::InvalidPlanShape);
        }
        let previous = order_at.checked_sub(1).ok_or(GraphOrderError::InvalidPlanShape)?;
        let projection_at = if matches!(self.operators.get(previous), Some(GlaOperator::Distinct)) {
            order_at.checked_sub(2)
        } else {
            order_at.checked_sub(1)
        }.ok_or(GraphOrderError::InvalidPlanShape)?;
        let Some(GlaOperator::ProjectValues { columns }) = self.operators.get(projection_at) else {
            return Err(GraphOrderError::InvalidPlanShape);
        };
        for (at, column) in order.iter().enumerate() {
            if column.column >= columns.len() { return Err(GraphOrderError::UnknownColumn { column: column.column }); }
            if order[..at].iter().any(|previous| previous.column == column.column) {
                return Err(GraphOrderError::DuplicateColumn { column: column.column });
            }
        }
        // All checks precede mutation. DISTINCT, column order and pagination
        // are preserved; repeated application replaces the earlier ordering.
        self.operators[order_at] = GlaOperator::OrderByValueColumns { columns: std::sync::Arc::from(order) };
        Ok(())
    }
}
