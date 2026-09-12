//! Binding-dependent property predicates for the connected-pattern compiler.

use super::*;
use crate::algebra::IntegerComparison;
use fgdb_delta_types::PropertyKeyId;

#[derive(Clone)]
pub(super) enum PropertyComparison {
    Properties {
        left: usize,
        left_key: PropertyKeyId,
        right: usize,
        right_key: PropertyKeyId,
        comparison: IntegerComparison,
    },
    Boolean(crate::algebra::BoundBooleanExpression),
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
        self.property_comparisons.push(PropertyComparison::Properties {
            left,
            left_key,
            right,
            right_key,
            comparison,
        });
        self.predicate_count += 1;
        Ok(self)
    }

    /// Attach a bounded AND/OR/NOT expression to this positive MATCH scope.
    /// Operands can span variables, so this runs after binding, not in the
    /// single-vertex cache. Use a value projection even for vertex-only output.
    /// Every name and the combined predicate limit is checked before mutation.
    pub fn filter_boolean(
        &mut self,
        expression: &crate::algebra::GraphBooleanExpression,
    ) -> Result<&mut Self, PatternBuildError> {
        let observed = self.predicate_count.saturating_add(expression.predicate_count());
        if observed > MAX_PATTERN_PREDICATES {
            return Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Predicates,
                limit: MAX_PATTERN_PREDICATES,
                observed,
            });
        }
        let bound = expression.bind(|name| self.variable(name))?;
        self.property_comparisons.push(PropertyComparison::Boolean(bound));
        self.predicate_count = observed;
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
            operators.push(match predicate {
                PropertyComparison::Properties { left, left_key, right, right_key, comparison } => {
                    GlaOperator::CompareProperties {
                        left: slots[*left], left_key: *left_key,
                        right: slots[*right], right_key: *right_key, comparison: *comparison,
                    }
                }
                PropertyComparison::Boolean(expression) => GlaOperator::SelectBoolean {
                    expression: expression.remap(|slot| slots[slot.ordinal() as usize]),
                },
            });
        }
    }
}
