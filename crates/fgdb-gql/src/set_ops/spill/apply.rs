//! Correlated external execution keeps the native lexical argument frame.

use super::*;

/// The admitted native row-extension semantics. A collected-list Apply needs
/// a separately bounded list builder and is refused during external admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpillSetApplyKind {
    Inner,
    Optional,
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
