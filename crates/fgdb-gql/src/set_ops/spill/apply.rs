//! Correlated external execution keeps the native lexical argument frame.

use super::*;

/// The admitted native per-occurrence correlation semantics. Collection keeps
/// mapped NULLs and duplicates; its saved NULL input skips the complete body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillSetApplyKind {
    Inner,
    Optional,
    Collect { null_input: usize },
}

/// A compiler-owned correlated relation over complete child plans.
///
/// Complete `left()` once under the enclosing argument frame. For each selected
/// left occurrence, copy `imports()` in their declared order and execute the
/// complete `right()` under that new frame. Restore the enclosing frame after
/// the body, including on failure or cancellation. Never cache equal input
/// rows: repeated occurrences run the body and charge work independently.
///
/// Emit left-major pairs in each completed body's order. Inner emits no row
/// for an empty body; Optional emits exactly one null extension. A body page,
/// DISTINCT or aggregate is local to that invocation. No enclosing LIMIT,
/// including zero, may hide a reached body failure. An empty left input never
/// executes the body. The enclosing Identity stage owns this Apply's clauses.
/// Collect emits one bounded list in body occurrence order, including mapped
/// NULLs; an empty body emits an empty list. Its saved NULL input emits NULL
/// without preparing arguments or executing the body.
///
/// Hosts keep one cumulative allowance, one pinned read/authority cut, and the
/// same logical source-ordinal range on every body invocation. They must admit
/// and release per-invocation scratch metadata without treating the static
/// `stage_count()` as a bound on the number of body invocations.
#[derive(Clone)]
pub struct SpillSetApplyPlan {
    pub(super) left: Box<AsyncSpillSetPlan>,
    pub(super) right: Box<AsyncSpillSetPlan>,
    pub(super) imports: Vec<usize>,
    pub(super) kind: SpillSetApplyKind,
    pub(super) columns: Vec<String>,
    pub(super) types: Vec<GraphSetColumnType>,
}

impl SpillSetApplyPlan {
    pub fn left(&self) -> &AsyncSpillSetPlan {
        &self.left
    }
    pub fn right(&self) -> &AsyncSpillSetPlan {
        &self.right
    }
    pub fn kind(&self) -> SpillSetApplyKind {
        self.kind
    }
    pub fn imports(&self) -> &[usize] {
        &self.imports
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    /// Test the saved input before preparing arguments or opening a collected
    /// body. A NULL list is not an empty body and must not observe graph reads,
    /// arithmetic failures or body work. Static child admission still occurs
    /// before execution, even when every input will take this short circuit.
    pub fn collect_input_is_null<E>(
        &self,
        left: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, SpillSetRowError<E>> {
        validate_row(left, self.left.column_types(), 0, control)?;
        control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
        let SpillSetApplyKind::Collect { null_input } = self.kind else {
            return Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ));
        };
        left.get(null_input)
            .map(GraphValue::is_null)
            .ok_or(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ))
    }

    /// The single NULL extension for a saved NULL list. No body has executed.
    /// Reserve the left input and admit a separate output on ScratchEntry;
    /// retain both guards through encoding. This never charges ResultRow.
    pub fn null_collect<E>(
        &self,
        left: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        if !self.collect_input_is_null(left, control)? {
            return Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ));
        }
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let null = GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
            fgdb_types::CanonicalScalar::Null,
        )]);
        super::super::execute::copy_cross_pair(left, &null, control)
            .map_err(SpillSetRowError::Control)
    }

    /// Start one non-NULL occurrence's private output after its complete body
    /// has finished. The host owns a growing memory reservation beside this
    /// collector and retains it across awaited body reads, finish and encoding.
    /// Every ScratchEntry must be admitted before returning success. An error
    /// drops the private prefix; a final LIMIT cannot suppress that error.
    pub fn start_collection<E>(
        &self,
        row_index: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<SpillSetCollection<'_>, SpillSetRowError<E>> {
        control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
        if !matches!(self.kind, SpillSetApplyKind::Collect { .. })
            || self.right.column_types().len() != 1
        {
            return Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 1 },
            ));
        }
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        Ok(SpillSetCollection {
            plan: self,
            row_index,
            members: Vec::new(),
            // The final List itself is one node at depth zero.
            remaining: GraphValue::MAX_LIST_NODES - 1,
        })
    }

    /// Copy the typed argument projection for one completed left occurrence.
    /// Repeated import positions retain repeated cells, including NULLs and
    /// collection/element identities. An empty import list still owns one
    /// zero-column argument occurrence; it is not an absent frame.
    ///
    /// Reserve the input before this call and admit argument storage on each
    /// ScratchEntry before returning success. Retain that reservation through
    /// the entire body invocation, including nested calls and awaited I/O.
    /// Arguments leaves make independently charged copies of this same frame.
    pub fn prepare_arguments<E>(
        &self,
        left: &GraphValueRow,
        row_index: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        validate_row(left, self.left.column_types(), 0, control)?;
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let mut values = Vec::with_capacity(self.imports.len());
        for (column, &input) in self.imports.iter().enumerate() {
            let value =
                projection::evaluate_value(&GraphSetValue::Column(input), left, column, control)
                    .map_err(|error| match error {
                        projection::ProjectionFailure::Control(error) => {
                            SpillSetRowError::Control(error)
                        }
                        projection::ProjectionFailure::Arithmetic { column, error } => {
                            SpillSetRowError::Native(GraphSetExecutionError::Projection {
                                row: row_index,
                                column,
                                error,
                            })
                        }
                    })?;
            values.push(value);
        }
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        Ok(GraphValueRow::from_owned_values(values))
    }

    /// Copy one pair with the same native occurrence copier as resident Apply.
    /// InputSchema operand zero is this left row and one is this body row;
    /// neither is a graph-source ordinal. Reserve both inputs before calling,
    /// admit a separate output reservation on ScratchEntry, and retain all
    /// three guards through the output's awaited encode/append. No ResultRow
    /// event is charged and no failed copy authorizes a prefix.
    pub fn copy_pair<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        validate_row(left, self.left.column_types(), 0, control)?;
        validate_row(right, self.right.column_types(), 1, control)?;
        super::super::execute::copy_cross_pair(left, right, control)
            .map_err(SpillSetRowError::Control)
    }

    /// The single absence row for an Optional body that completed empty.
    /// Call only after its complete evaluation, never after the first missing
    /// source witness. Every body column receives NULL, even element/list
    /// domains. Admission and ownership match `copy_pair`; no temporary null
    /// vector or uncharged payload is allocated.
    pub fn null_extend<E>(
        &self,
        left: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        validate_row(left, self.left.column_types(), 0, control)?;
        control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let null = GraphValue::Scalar(fgdb_types::CanonicalScalar::Null);
        let mut values = Vec::new();
        for value in left
            .values()
            .iter()
            .chain(std::iter::repeat_n(&null, self.right.column_types().len()))
        {
            values.push(projection::copy_value(value, control).map_err(SpillSetRowError::Control)?);
        }
        Ok(GraphValueRow::from_owned_values(values))
    }
}

impl core::fmt::Debug for SpillSetApplyPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillSetApplyPlan")
            .field("kind", &self.kind)
            .field("columns", &self.columns.len())
            .field("imports", &self.imports.len())
            .finish_non_exhaustive()
    }
}

/// One private collected result, bounded by the ordinary GraphValue depth and
/// node limits before any member payload is copied. It owns only the mapped
/// members, not the body relation. The host supplies their memory reservation.
/// Consuming push/finish makes every control or value refusal terminal: a
/// partially built list can neither escape nor be retried as a shorter result.
pub struct SpillSetCollection<'plan> {
    plan: &'plan SpillSetApplyPlan,
    row_index: usize,
    members: Vec<GraphValue>,
    remaining: usize,
}

impl SpillSetCollection<'_> {
    /// Append one complete one-column body row in its existing occurrence
    /// order. Mapped NULLs, duplicates and compact element lists are values,
    /// never absence, deduplication keys or implicit nested-row wrappers.
    pub fn push<E>(
        mut self,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Self, SpillSetRowError<E>> {
        validate_row(right, self.plan.right.column_types(), 1, control)?;
        let value = right.get(0).ok_or(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand: 1 },
        ))?;
        admit_collection_value(
            value,
            1,
            &mut self.remaining,
            self.row_index,
            self.plan.left.column_types().len(),
            control,
        )?;
        // This entry covers the member slot before vector growth; the native
        // evaluator separately admits every cell and payload before copying.
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let value = projection::evaluate_value(
            &GraphSetValue::Column(0),
            right,
            self.plan.left.column_types().len(),
            control,
        )
        .map_err(|error| match error {
            projection::ProjectionFailure::Control(error) => SpillSetRowError::Control(error),
            projection::ProjectionFailure::Arithmetic { column, error } => {
                SpillSetRowError::Native(GraphSetExecutionError::Projection {
                    row: self.row_index,
                    column,
                    error,
                })
            }
        })?;
        self.members.push(value);
        Ok(self)
    }

    /// Complete exactly one non-NULL outer occurrence, after the host verifies
    /// the complete body row count. Copy the outer cells under controls and
    /// move the already admitted list into the result without another payload
    /// clone. Keep the collector and output reservation through final encoding.
    pub fn finish<E>(
        self,
        left: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        if self.plan.collect_input_is_null(left, control)? {
            return Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand: 0 },
            ));
        }
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let mut values = Vec::new();
        for value in left.values() {
            values.push(projection::copy_value(value, control).map_err(SpillSetRowError::Control)?);
        }
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        let collected = GraphValue::List(self.members.into_boxed_slice());
        debug_assert!(collected.validate_bounds());
        values.push(collected);
        control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
        Ok(GraphValueRow::from_owned_values(values))
    }
}

impl core::fmt::Debug for SpillSetCollection<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SpillSetCollection([REDACTED])")
    }
}

fn collection_overflow<E>(row: usize, column: usize) -> SpillSetRowError<E> {
    SpillSetRowError::Native(GraphSetExecutionError::Projection {
        row,
        column,
        error: crate::GraphIntegerError {
            instruction: 0,
            kind: crate::GraphIntegerErrorKind::Overflow,
        },
    })
}

// The same structural bounds as GraphValue::validate_bounds, with depth one
// for each member and one node budget shared by the final collected List.
// This borrowed walk allocates no validation stack and stops before copying
// an inadmissible member; recursion is bounded by MAX_LIST_DEPTH + 1.
fn admit_collection_value<E>(
    value: &GraphValue,
    depth: usize,
    remaining: &mut usize,
    row: usize,
    column: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), SpillSetRowError<E>> {
    control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
    if depth > GraphValue::MAX_LIST_DEPTH || *remaining == 0 {
        return Err(collection_overflow(row, column));
    }
    *remaining -= 1;
    match value {
        GraphValue::List(values) => {
            for value in values {
                admit_collection_value(value, depth + 1, remaining, row, column, control)?;
            }
        }
        GraphValue::Map { keys, values } => {
            if keys.len() != values.len() {
                return Err(collection_overflow(row, column));
            }
            for pair in keys.windows(2) {
                control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
                if pair[0].as_bytes() >= pair[1].as_bytes() {
                    return Err(collection_overflow(row, column));
                }
            }
            for value in values {
                admit_collection_value(value, depth + 1, remaining, row, column, control)?;
            }
        }
        GraphValue::Scalar(scalar) => {
            scalar
                .canonical_encoded_len()
                .map_err(|_| collection_overflow(row, column))?;
        }
        _ => {}
    }
    Ok(())
}

/// One imported row from the nearest enclosing Apply, under local aliases.
/// Every occurrence reads the frame without consuming it. It does not open a
/// graph source or obtain ambient/host-owned rows. Its own Identity stage still
/// owns its ordering and page, including a page that discards this occurrence.
#[derive(Clone)]
pub struct SpillSetArgumentsPlan {
    pub(super) columns: Vec<String>,
    pub(super) types: Vec<GraphSetColumnType>,
}

impl SpillSetArgumentsPlan {
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }

    /// Validate and copy the current frame with the native occurrence copier.
    /// A missing frame is ArgumentsUnavailable, never an empty relation. NULL
    /// cells and a present zero-column frame each still produce one row.
    ///
    /// The frame's reservation belongs to the enclosing Apply. Admit a separate
    /// row reservation on ScratchEntry and retain it through the returned
    /// cursor's last output use, including awaited encoding or scratch append.
    /// Errors and cancellation are terminal; neither consumes the frame.
    pub fn start<E>(
        &self,
        frame: Option<&GraphValueRow>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<SpillSetRows, SpillSetRowError<E>> {
        let frame = frame.ok_or(SpillSetRowError::Native(
            GraphSetExecutionError::ArgumentsUnavailable,
        ))?;
        validate_row(frame, &self.types, 0, control)?;
        let row = super::super::execute::copy_cross_pair(&GraphValueRow::unit(), frame, control)
            .map_err(SpillSetRowError::Control)?;
        Ok(SpillSetRows {
            state: StageRows::One(Some(row)),
        })
    }
}

impl core::fmt::Debug for SpillSetArgumentsPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillSetArgumentsPlan")
            .field("columns", &self.columns.len())
            .finish_non_exhaustive()
    }
}

fn validate_row<E>(
    row: &GraphValueRow,
    types: &[GraphSetColumnType],
    operand: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<(), SpillSetRowError<E>> {
    control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
    if row.len() != types.len() {
        return Err(SpillSetRowError::Native(
            GraphSetExecutionError::InputSchema { operand },
        ));
    }
    for (kind, value) in types.iter().zip(row.values()) {
        control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
        if !kind.accepts(value) {
            return Err(SpillSetRowError::Native(
                GraphSetExecutionError::InputSchema { operand },
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
