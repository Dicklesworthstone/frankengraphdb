//! Binding-dependent property predicates for the connected-pattern compiler.

use super::*;
use crate::algebra::IntegerComparison;
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone, Copy)]
pub(super) struct PropertyComparison {
    left: usize,
    left_key: PropertyKeyId,
    right: usize,
    right_key: PropertyKeyId,
    comparison: IntegerComparison,
}

impl GraphPatternBuilder {
    /// Compare canonical properties of two variables in this positive MATCH
    /// scope. Both variables must be declared, but neither need be projected.
    /// This is a selection predicate, not a new edge or an implicit Cartesian
    /// product: ordinary connectivity and identity constraints still apply.
    ///
    /// The comparison executes inside its scope before OPTIONAL establishes a
    /// match and before an EXISTS witness or aggregate input is accepted. Null,
    /// missing and unlike scalar kinds reject every ordinary comparison. Use
    /// prepare_values (vertex columns are allowed) or its scoped variants;
    /// predicate-only identity execution cannot supply these property reads.
    pub fn compare_properties(
        &mut self,
        left: &str,
        left_key: PropertyKeyId,
        comparison: IntegerComparison,
        right: &str,
        right_key: PropertyKeyId,
    ) -> Result<&mut Self, PatternBuildError> {
        let left = self.variable(left)?;
        let right = self.variable(right)?;
        check_next(
            self.predicate_count,
            MAX_PATTERN_PREDICATES,
            PatternLimitDimension::Predicates,
        )?;
        self.property_comparisons.push(PropertyComparison {
            left,
            left_key,
            right,
            right_key,
            comparison,
        });
        self.predicate_count += 1;
        Ok(self)
    }

    pub(super) fn require_identity_projection(&self) -> Result<(), PatternBuildError> {
        if self.property_comparisons.is_empty() {
            Ok(())
        } else {
            Err(PatternBuildError::RequiresValueProjection)
        }
    }

    /// The positive compiler has finished introducing every declared variable.
    /// A scoped caller remaps BOTH sides before attaching its match boundary.
    /// No data-dependent scheduling or single-vertex predicate cache is used.
    pub(super) fn emit_property_comparisons(
        &self,
        slots: &[BindingSlot],
        operators: &mut Vec<GlaOperator>,
    ) {
        for predicate in &self.property_comparisons {
            operators.push(GlaOperator::CompareProperties {
                left: slots[predicate.left],
                left_key: predicate.left_key,
                right: slots[predicate.right],
                right_key: predicate.right_key,
                comparison: predicate.comparison,
            });
        }
    }
}
