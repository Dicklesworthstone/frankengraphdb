//! Complete relational joins using the existing checked native row kernel.
//!
//! A fresh incremental operator evaluates the exact snapshot from empty inputs.
//! This is the same bag/predicate/null-extension implementation maintained by
//! standing circuits, not a second matcher. Inputs and output remain resident;
//! work/scratch count logical payload events, not allocator or comparison bytes.

use super::*;
use crate::GlaExecutionEvent;
use crate::row_join::{IncrementalRowJoin, RowJoinError, RowJoinKind, RowJoinSpec};
use fgdb_delta_types::{LimbLimit, ZSet, ZSetError, ZSetEvent, ZWeight};

const LIMBS: LimbLimit = LimbLimit::new(4);

impl PreparedGraphSet {
    /// Join two complete, independently scoped relations with a frozen schema,
    /// kind, equality keys and optional ON predicate. `RowJoinSpec::new` builds
    /// keyed joins; `cross(...).with_predicate(...)` builds a pure theta join.
    /// Each declared input schema must match exactly, even for empty bags and
    /// semi/anti output. Neither Any coercion nor implicit DISTINCT is introduced.
    ///
    /// Child filters, projections, ordering and pages finish BEFORE matching.
    /// Both graph-source subtrees execute once, left-to-right,
    /// under the host's one pinned source and cumulative policy. Only TRUE ON
    /// predicates match; outer NULL extension follows matching. Semi/anti retain
    /// left multiplicities without multiplying by right witness counts.
    ///
    /// Output is in canonical complete-row order, with duplicates adjacent,
    /// before this node's optional ORDER BY/page. Names are left.<name> followed
    /// by right.<name>; semi/anti have left columns only. This differs from the
    /// legacy left-major cross_join sequence and does not rewrite that operator.
    /// Further filter/project/set/join/group stages consume the completed result.
    /// No textual JOIN grammar, selective range index, spill or WCOJ is implied.
    pub fn join(self, right: Self, spec: RowJoinSpec) -> Result<Self, GraphSetBuildError> {
        let operands = self.operands + right.operands;
        if operands > MAX_GRAPH_SET_OPERANDS {
            return Err(GraphSetBuildError::TooManyOperands {
                limit: MAX_GRAPH_SET_OPERANDS,
                observed: operands,
            });
        }
        let depth = 1 + self.depth.max(right.depth);
        check_depth(depth)?;
        for (side, (actual, declared)) in [
            (self.types.as_slice(), spec.left_types()),
            (right.types.as_slice(), spec.right_types()),
        ]
        .into_iter()
        .enumerate()
        {
            if actual != declared {
                return Err(GraphSetBuildError::JoinInputSchema { side });
            }
        }
        let mut columns: Vec<_> = self.columns.iter().map(|name| format!("left.{name}")).collect();
        if !matches!(spec.kind(), RowJoinKind::Semi | RowJoinKind::Anti) {
            columns.extend(right.columns.iter().map(|name| format!("right.{name}")));
        }
        Ok(Self {
            columns,
            types: spec.column_types().collect(),
            operands,
            depth,
            node: SetNode::Join {
                left: Box::new(self),
                right: Box::new(right),
                spec,
            },
            order: Vec::new(),
            offset: 0,
            count: None,
        })
    }
}

/// New application-node transcript. Existing node tags and bytes are unchanged.
/// Explicit tags never depend on enum layout. Predicate bytes come from the
/// ordinary row predicate transcript, including literal types and payloads.
pub(super) fn append_transcript(spec: &RowJoinSpec, bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(b"fgdb:relational-join:canonical-bag:v1\0");
    bytes.push(match spec.kind() {
        RowJoinKind::Inner => 0,
        RowJoinKind::Left => 1,
        RowJoinKind::Right => 2,
        RowJoinKind::Full => 3,
        RowJoinKind::Semi => 4,
        RowJoinKind::Anti => 5,
    });
    for types in [spec.left_types(), spec.right_types()] {
        bytes.extend_from_slice(&(types.len() as u64).to_be_bytes());
        for kind in types {
            bytes.push(match kind {
                GraphSetColumnType::Vertex => 0,
                GraphSetColumnType::Scalar => 1,
                GraphSetColumnType::Path => 2,
                GraphSetColumnType::Vertices => 3,
                GraphSetColumnType::Edges => 4,
                GraphSetColumnType::Edge => 5,
                GraphSetColumnType::List => 6,
                GraphSetColumnType::Any => 7,
            });
        }
    }
    bytes.extend_from_slice(&(spec.keys().len() as u64).to_be_bytes());
    for &(left, right) in spec.keys() {
        bytes.extend_from_slice(&(left as u64).to_be_bytes());
        bytes.extend_from_slice(&(right as u64).to_be_bytes());
    }
    bytes.push(u8::from(spec.predicate().is_some()));
    if let Some(code) = spec.predicate() {
        let types: Vec<_> = spec.left_types().iter().chain(spec.right_types()).copied().collect();
        filter::RowPredicate::prepare(&types, code)
            .expect("immutable RowJoinSpec already validated the complete predicate schema")
            .append_transcript(bytes);
    }
}

fn definition<E>(
    spec: &RowJoinSpec,
    control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
) -> Result<RowJoinSpec, E> {
    for _ in 0..spec.left_types().len() + spec.right_types().len() + spec.keys().len() {
        control(GlaExecutionEvent::ScratchEntry)?;
    }
    for op in spec.predicate().unwrap_or_default() {
        control(GlaExecutionEvent::Work)?;
        control(GlaExecutionEvent::ScratchEntry)?;
        let mut literal = |operand: &GraphSetOperand| {
            if let GraphSetOperand::Literal(value) = operand {
                for _ in 0..value.canonical_bytes().len().div_ceil(64) {
                    control(GlaExecutionEvent::Work)?;
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
            }
            Ok::<(), E>(())
        };
        match op {
            GraphSetPredicateOp::Compare { left, right, .. } => {
                literal(left)?;
                literal(right)?;
            }
            GraphSetPredicateOp::IsNull { operand, .. } => literal(operand)?,
            _ => {}
        }
    }
    Ok(spec.clone())
}

fn failure<E, C>(
    error: RowJoinError<GqlQueryError<GraphSetExecutionError<E>, C>>,
) -> GqlQueryError<GraphSetExecutionError<E>, C> {
    let error = match error {
        RowJoinError::Delta(ZSetError::Control(error) | ZSetError::Callback(error)) => return error,
        RowJoinError::Delta(ZSetError::Arithmetic(error)) => {
            RowJoinError::Delta(ZSetError::Arithmetic(error))
        }
        RowJoinError::Delta(ZSetError::WeightAdmission { required_limbs, limit }) => {
            RowJoinError::Delta(ZSetError::WeightAdmission { required_limbs, limit })
        }
        RowJoinError::InputSchema { side } => RowJoinError::InputSchema { side },
        RowJoinError::NegativeMultiplicity { side } => RowJoinError::NegativeMultiplicity { side },
        RowJoinError::ResultBudget { limit } => RowJoinError::ResultBudget { limit },
        RowJoinError::InvalidResult => RowJoinError::InvalidResult,
    };
    GqlQueryError::Source(GraphSetExecutionError::Join(error))
}
fn event(event: ZSetEvent) -> GlaExecutionEvent {
    match event {
        ZSetEvent::Work => GlaExecutionEvent::Work,
        ZSetEvent::ScratchEntry => GlaExecutionEvent::ScratchEntry,
    }
}
fn bag<E, C>(
    rows: Vec<GraphValueRow>,
    types: &[GraphSetColumnType],
    side: usize,
    control: &mut impl FnMut(GlaExecutionEvent) -> SetResult<(), E, C>,
) -> SetResult<ZSet<GraphValueRow>, E, C> {
    // Admission precedes ordered-map comparisons, even for an empty opposite
    // input or a constant FALSE predicate. Moving rows needs no payload clone.
    for row in &rows {
        control(GlaExecutionEvent::Work)?;
        if row.len() != types.len() {
            return Err(failure(RowJoinError::InputSchema { side }));
        }
        for (value, kind) in row.values().iter().zip(types) {
            control(GlaExecutionEvent::Work)?;
            if !kind.accepts(value) || !value.validate_bounds() {
                return Err(failure(RowJoinError::InputSchema { side }));
            }
            for _ in 0..value.payload_units() {
                control(GlaExecutionEvent::Work)?;
            }
        }
    }
    ZSet::from_updates(
        rows.into_iter().map(|row| (row, ZWeight::from_i128(1))),
        LIMBS,
        &mut |value| control(event(value)),
    )
    .map_err(|error| failure(RowJoinError::Delta(error)))
}

pub(super) fn execute<E, C>(
    spec: &RowJoinSpec,
    left: Vec<GraphValueRow>,
    right: Vec<GraphValueRow>,
    control: &mut impl FnMut(GlaExecutionEvent) -> SetResult<(), E, C>,
) -> SetResult<Vec<GraphValueRow>, E, C> {
    let mut operator = IncrementalRowJoin::new(definition(spec, control)?);
    let left = bag(left, spec.left_types(), 0, control)?;
    let right = bag(right, spec.right_types(), 1, control)?;
    // This private bag is not yet the final page. Only the parent result sink
    // spends max_result_rows; every retained/expanded row still spends scratch.
    let rows = operator
        .prepare(&left, &right, LIMBS, None, &mut |value| control(event(value)))
        .map_err(failure)?
        .commit();
    drop(operator);
    let mut output = Vec::new();
    for (row, count) in rows.into_updates() {
        control(GlaExecutionEvent::Work)?;
        let count = count.to_i128().and_then(|count| u64::try_from(count).ok())
            .filter(|count| *count != 0)
            .ok_or_else(|| GqlQueryError::Source(GraphSetExecutionError::AccountingOverflow {
                dimension: GqlBudgetDimension::ResultRows,
            }))?;
        for _ in 1..count {
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut values = Vec::new();
            for value in row.values() {
                values.push(projection::copy_value(value, control)?);
            }
            output.push(GraphValueRow::from_owned_values(values));
        }
        control(GlaExecutionEvent::ScratchEntry)?;
        output.push(row);
    }
    control(GlaExecutionEvent::Work)?;
    Ok(output)
}

#[cfg(test)]
mod tests;
