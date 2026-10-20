//! Sealed external execution metadata for a native relational join.
use super::*;
use crate::row_join::{RowJoinError, RowJoinKind, RowJoinSpec};
use fgdb_delta_types::ZSetError;

#[derive(Clone)]
pub struct SpillSetJoinPlan {
    pub(super) left: Box<AsyncSpillSetPlan>,
    pub(super) right: Box<AsyncSpillSetPlan>,
    pub(super) spec: RowJoinSpec,
    pub(super) columns: Vec<String>,
    pub(super) types: Vec<GraphSetColumnType>,
}
impl SpillSetJoinPlan {
    pub fn left(&self) -> &AsyncSpillSetPlan {
        &self.left
    }
    pub fn right(&self) -> &AsyncSpillSetPlan {
        &self.right
    }
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }
    pub fn kind(&self) -> RowJoinKind {
        self.spec.kind()
    }
    /// Positional canonical equality keys; each side is sorted independently
    /// in this tuple order. Repeated positions do not require duplicate sorts.
    pub fn keys(&self) -> &[(usize, usize)] {
        self.spec.keys()
    }
    /// Fixed eligibility, not an adaptive choice or a retry after refusal.
    /// Residual ON uses supports_merge_filtered() and per-occurrence witnesses.
    /// Equal NULL tuples still do not establish presence.
    pub fn supports_merge_presence(&self) -> bool {
        matches!(self.kind(), RowJoinKind::Semi | RowJoinKind::Anti)
            && !self.keys().is_empty()
            && self.spec.predicate().is_none()
    }
    /// Equality-only pair joins can replay one sorted key group instead of
    /// rescanning the complete opposite relation. Residual ON predicates need
    /// supports_merge_filtered(): a group witness cannot prove row absence.
    pub fn supports_merge_pairs(&self) -> bool {
        matches!(
            self.kind(),
            RowJoinKind::Inner | RowJoinKind::Left | RowJoinKind::Right | RowJoinKind::Full
        ) && !self.keys().is_empty()
            && self.spec.predicate().is_none()
    }
    /// Residual joins replay only equal-key candidates through the native ON
    /// kernel. An unkeyed definition may use necessary ON equalities, proved
    /// by the SAME Boolean analysis as resident and maintained selections.
    /// This bounded metadata inspection owns no input rows. The host repeats
    /// it under its execution control before retaining the actual probe keys.
    /// Every occurrence owns its own witness; this is never a retry strategy.
    pub fn supports_merge_filtered(&self) -> bool {
        if self.spec.predicate().is_none() {
            return false;
        }
        if !self.keys().is_empty() {
            return true;
        }
        match self.spec.predicate_probe_keys_with_control(&mut |_| {
            Ok::<_, core::convert::Infallible>(())
        }) {
            Ok(keys) => !keys.is_empty(),
            Err(never) => match never {},
        }
    }

    /// Physical candidate tuple in original left/right column positions.
    /// Explicit keys keep typed identity. Otherwise these are necessary ON
    /// equalities, NOT additional typed join conditions. Empty means no merge
    /// specialization; NOT and unsafe OR arms never manufacture a key.
    /// Reserve metadata on ScratchEntry before returning success and retain
    /// it through sorting and both witness sweeps. Refusal is terminal.
    pub fn probe_keys_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Vec<(usize, usize)>, SpillSetRowError<E>> {
        if self.keys().is_empty() {
            return self
                .spec
                .predicate_probe_keys_with_control(control)
                .map_err(SpillSetRowError::Control);
        }
        let mut keys = Vec::new();
        for &key in self.keys() {
            control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
            control(GlaExecutionEvent::ScratchEntry).map_err(SpillSetRowError::Control)?;
            keys.push(key);
        }
        Ok(keys)
    }

    /// Compare tuples obtained from THIS plan's probe_keys_with_control.
    /// Inferred tuples use native language orderability (ascending, NULL last)
    /// on both sides. This is a candidate SUPERSET, not predicate equivalence:
    /// e.g. Decimal(7), Int(7), Float(7) can share a group without all matching
    /// ON. Every candidate must still pass the complete native matcher. This
    /// permits reuse of the ordinary query-order sorter without rewriting
    /// payloads, defining another numeric law or promoting inferred keys into
    /// explicit typed conditions. Payload columns do not break tuple ties.
    /// Complete input schemas and key bounds are checked before comparison.
    pub fn compare_probe_keys_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        keys: &[(usize, usize)],
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<core::cmp::Ordering, SpillSetRowError<E>> {
        if !self.keys().is_empty() {
            return self.compare_keys_with_control(left, right, control);
        }
        self.validate_row_with_control(left, 0, control)?;
        self.validate_row_with_control(right, 1, control)?;
        for &(a, b) in keys {
            control(GlaExecutionEvent::Work).map_err(SpillSetRowError::Control)?;
            let left = left
                .values()
                .get(a)
                .ok_or_else(|| failure(RowJoinError::InputSchema { side: 0 }))?;
            let right = right
                .values()
                .get(b)
                .ok_or_else(|| failure(RowJoinError::InputSchema { side: 1 }))?;
            let order = left
                .compare_orderability_with_control(right, control)
                .map_err(SpillSetRowError::Control)?;
            if order != core::cmp::Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(core::cmp::Ordering::Equal)
    }
    pub fn compare_keys_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<core::cmp::Ordering, SpillSetRowError<E>> {
        self.spec
            .compare_keys_with_control(left, right, control)
            .map_err(failure)
    }
    pub fn validate_row_with_control<E>(
        &self,
        row: &GraphValueRow,
        side: usize,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<(), SpillSetRowError<E>> {
        self.spec
            .validate_input_with_control(side, row, control)
            .map_err(failure)
    }
    pub fn matches_pair_with_control<E>(
        &self,
        left: &GraphValueRow,
        right: &GraphValueRow,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<bool, SpillSetRowError<E>> {
        self.spec
            .test_pair_with_control(left, right, control)
            .map_err(failure)
    }
    pub fn project_pair_with_control<E>(
        &self,
        left: Option<&GraphValueRow>,
        right: Option<&GraphValueRow>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<GraphValueRow, SpillSetRowError<E>> {
        self.spec
            .project_output_with_control(left, right, control)
            .map_err(failure)
    }
}
impl core::fmt::Debug for SpillSetJoinPlan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpillSetJoinPlan")
            .field("kind", &self.kind())
            .field("columns", &self.columns.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
fn failure<E>(error: RowJoinError<E>) -> SpillSetRowError<E> {
    let native = match error {
        RowJoinError::Delta(ZSetError::Control(error) | ZSetError::Callback(error)) => {
            return SpillSetRowError::Control(error);
        }
        RowJoinError::Delta(ZSetError::Arithmetic(error)) => {
            RowJoinError::Delta(ZSetError::Arithmetic(error))
        }
        RowJoinError::Delta(ZSetError::WeightAdmission {
            required_limbs,
            limit,
        }) => RowJoinError::Delta(ZSetError::WeightAdmission {
            required_limbs,
            limit,
        }),
        RowJoinError::InputSchema { side } => RowJoinError::InputSchema { side },
        RowJoinError::NegativeMultiplicity { side } => RowJoinError::NegativeMultiplicity { side },
        RowJoinError::ResultBudget { limit } => RowJoinError::ResultBudget { limit },
        RowJoinError::InvalidResult => RowJoinError::InvalidResult,
    };
    SpillSetRowError::Native(GraphSetExecutionError::Join(native))
}
#[cfg(test)]
mod tests;
