//! The native WHERE-over-product seam, sealed for a bounded external host.
use super::*;
use crate::GraphSetPredicateOp;

#[derive(Clone)]
pub(super) struct Selection {
    code: Box<[GraphSetPredicateOp]>,
    columns: Option<Box<[usize]>>,
}

// Reuse the native eligibility proof: only unpaged transparent scopes and one
// column-only ALL projection may disappear. Original child plans still finish
// before any pair is examined. In particular, do not inspect the Boolean code
// to erase a child, or cross an expression, DISTINCT, order or page barrier.
pub(super) fn compile(
    query: &PreparedGraphSet,
    depth: usize,
    stages: &mut Vec<SpillSetStage>,
) -> Result<Option<SpillSetCrossPlan>, SpillSetBuildError> {
    let Some((left, right, code, projection)) = query.filtered_cross_inputs() else {
        return Ok(None);
    };
    // Preserve diagnostics in the ORIGINAL logical tree, not a shallower
    // synthetic product. The native seam proved this exact wrapper chain.
    let SetNode::Filter { input, .. } = &query.node else {
        unreachable!("the native selected-product seam requires a filter")
    };
    let mut input = input.as_ref();
    let mut child_depth = depth + 1;
    loop {
        child_depth += 1;
        match &input.node {
            SetNode::Scope(child) | SetNode::Project { input: child, .. } => input = child,
            SetNode::CrossJoin { .. } => break,
            _ => unreachable!("the native seam admitted only transparent wrappers"),
        }
    }
    if left.columns.len().saturating_add(right.columns.len()) > MAX_PATTERN_VERTICES {
        return Err(SpillSetBuildError::Unsupported {
            depth: child_depth - 1,
        });
    }
    let columns = projection.map(|projection| {
        projection
            .iter()
            .map(|column| match column.value() {
                GraphSetValue::Column(at) => *at,
                _ => unreachable!("the native seam admits only column copies"),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
    });
    let plan = SpillSetCrossPlan {
        left: Box::new(AsyncSpillSetPlan::compile_at(left, child_depth)?),
        right: Box::new(AsyncSpillSetPlan::compile_at(right, child_depth)?),
        columns: query.columns.clone(),
        types: query.types.clone(),
        selection: Some(Selection {
            code: code.into(),
            columns,
        }),
    };
    // The physical source already applied the filter and optional copies.
    // Restore only THIS node's order/page. Absent an explicit order, retain
    // the selected left-major sequence, including repeated projected columns.
    stages.push(SpillSetStage {
        input: query.types.clone(),
        columns: query.columns.clone(),
        types: query.types.clone(),
        operation: Operation::Identity,
        order: (!query.order.is_empty()).then(|| query.order.clone()),
        distinct: false,
        offset: query.offset,
        count: query.count,
    });
    Ok(Some(plan))
}

impl SpillSetCrossPlan {
    /// TRUE means only accepted pairs form this physical source's output.
    /// The host must not admit the unselected L*R count as retained rows.
    /// Candidate work remains metered; both original children still complete.
    pub fn has_selection(&self) -> bool {
        self.selection.is_some()
    }

    pub(super) fn validate_pair_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), SpillSetRowError<E>> {
        let mut event = |event| control(event).map_err(SpillSetRowError::Control);
        for (operand, input, row) in [(0, self.left(), left), (1, self.right(), right)] {
            event(GlaExecutionEvent::Work)?;
            if row.len() != input.column_types().len() {
                return Err(SpillSetRowError::Native(
                    GraphSetExecutionError::InputSchema { operand },
                ));
            }
            for (kind, value) in input.column_types().iter().zip(row.values()) {
                event(GlaExecutionEvent::Work)?;
                if !kind.accepts(value) {
                    return Err(SpillSetRowError::Native(
                        GraphSetExecutionError::InputSchema { operand },
                    ));
                }
            }
        }
        Ok(())
    }

    /// Test a borrowed candidate with the native eager three-valued predicate,
    /// then copy only TRUE pairs. FALSE/UNKNOWN allocate no output payload.
    /// A compiler-proved column projection maps both predicate and output to
    /// the original children; it neither evaluates expressions nor sorts rows.
    ///
    /// Reserve both authenticated inputs before this call. Charge the original
    /// cumulative allowance on every event and admit output memory BEFORE each
    /// ScratchEntry returns. Keep that guard through encoding/awaited append.
    /// No final ResultRow is charged and a failure never yields a partial row.
    pub fn select_pair_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<GraphValueRow>, SpillSetRowError<E>> {
        let Some(selection) = &self.selection else {
            return self.copy_pair_with_control(left, right, control).map(Some);
        };
        self.validate_pair_with_control(left, right, control)?;
        if !GraphSetPredicateOp::evaluate_projected_pair_with_control(
            &selection.code,
            left,
            right,
            selection.columns.as_deref(),
            &mut |event| control(event).map_err(SpillSetRowError::Control),
        )? {
            return Ok(None);
        }
        self.copy_output_with_control(left, right, control).map(Some)
    }

    pub(super) fn copy_output_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        let mut event = |event| control(event).map_err(SpillSetRowError::Control);
        let columns = self
            .selection
            .as_ref()
            .and_then(|selection| selection.columns.as_deref());
        let Some(columns) = columns else {
            return super::super::execute::copy_cross_pair(left, right, &mut event);
        };
        // Use the native per-value copier (also used by resident selected
        // products), with the same pre-allocation payload events. Never build
        // and then discard a full concatenated row for a narrow projection.
        event(GlaExecutionEvent::ScratchEntry)?;
        let mut values = Vec::new();
        for &column in columns {
            let value = if column < left.len() {
                &left.values()[column]
            } else {
                &right.values()[column - left.len()]
            };
            values.push(projection::copy_value(value, &mut event)?);
        }
        Ok(GraphValueRow::from_owned_values(values))
    }
}

#[cfg(test)]
mod tests;
