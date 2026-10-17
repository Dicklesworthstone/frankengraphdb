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
    /// Keyed residual joins replay only equal-key candidates through the native
    /// ON kernel. Every left/right occurrence owns its own witness; outer and
    /// presence decisions never reuse another occurrence's predicate result.
    /// This is a fixed choice before execution, never a retry after refusal.
    pub fn supports_merge_filtered(&self) -> bool {
        !self.keys().is_empty() && self.spec.predicate().is_some()
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
