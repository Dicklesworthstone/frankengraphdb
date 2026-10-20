//! Checked product factors of one original graph source. The source owner
//! admits the original plan once; these plans only evaluate that admitted cut.

use super::*;
use crate::GlaExecutionEvent;
use crate::algebra::{GRAPH_VALUE_PAYLOAD_UNIT_BYTES, GraphValueRow, ValueProjection};

/// Two independent vertex factors with the original source's admission and
/// witness domain. This is a physical execution request, not two graph reads.
///
/// The admitted source must contain each vertex identity exactly once, as the
/// native snapshot and transaction domains do. Both factor plans start with
/// their non-null vertex identity. Flattening in factor-0 major, factor-1 minor
/// order and restoring the original column positions is exactly the original
/// canonical graph row sequence. Unique identities make each complete pair
/// unique, so the original DISTINCT policy changes no multiplicity. An identity
/// may occur in both factors; no inequality is added. Duplicate imported rows
/// remain independent occurrences outside these graph factors.
#[derive(Debug)]
pub struct PreparedGraphPatternFactors<'p> {
    original: &'p PreparedGraphPattern<GraphValueRow>,
    factors: [PreparedGraphPattern<GraphValueRow>; 2],
    /// Each original output cell's factor and local column position.
    columns: Vec<(usize, usize)>,
}

#[cfg(test)]
mod tests;

impl<'p> PreparedGraphPatternFactors<'p> {
    #[must_use]
    pub fn original(&self) -> &'p PreparedGraphPattern<GraphValueRow> {
        self.original
    }

    /// Evaluate both plans against the SAME source admitted for `original()`.
    /// Do not independently admit either plan or narrow the original witnesses.
    /// The admitted vertex domain must have unique identities.
    #[must_use]
    pub fn factors(&self) -> &[PreparedGraphPattern<GraphValueRow>; 2] {
        &self.factors
    }

    pub(crate) fn columns(&self) -> &[(usize, usize)] {
        &self.columns
    }
}

impl PreparedGraphPattern<GraphValueRow> {
    // Definition-only shape recognition, like filtered_cross_inputs: bounded
    // by the already checked pattern inventory, with no allocation, payload
    // comparison or source/control callback. An ineligible physical request
    // must retain the incumbent source's exact refusal/admission path.
    fn independent_vertex_factor_shape(
        &self,
    ) -> Option<(&[GlaOperator], &[ValueProjection], bool)> {
        if self.variable_count != 2
            || self.edge_count != 0
            || self.logical.visible_columns.is_some()
        {
            return None;
        }
        let [
            body @ ..,
            GlaOperator::OrderByValues,
            GlaOperator::Limit {
                offset: 0,
                count: None,
            },
        ] = self.logical.operators.as_slice()
        else {
            return None;
        };
        let (body, distinct) = match body.split_last() {
            Some((GlaOperator::Distinct, body)) => (body, true),
            _ => (body, false),
        };
        let Some((GlaOperator::ProjectValues { columns }, body)) = body.split_last() else {
            return None;
        };
        if !matches!(
            columns.as_slice(),
            [
                ValueProjection::Vertex {
                    slot: BindingSlot(0)
                },
                ValueProjection::Vertex {
                    slot: BindingSlot(1)
                },
                ..
            ]
        ) || columns.len() != self.columns.len()
        {
            return None;
        }
        let mut roots = 0;
        for operator in body {
            match operator {
                GlaOperator::ScanVertices if roots < 2 => roots += 1,
                GlaOperator::Select { slot, predicates } if slot.0 < roots => {
                    for predicate in predicates {
                        if !matches!(predicate, VertexPredicate::HasLabel(_)) {
                            return None;
                        }
                    }
                }
                _ => return None,
            }
        }
        if roots != 2 {
            return None;
        }
        for column in columns {
            if !matches!(column, ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } if slot.0 < 2)
            {
                return None;
            }
        }
        Some((body, columns, distinct))
    }

    pub(crate) fn independent_vertex_factor_columns(&self) -> Option<&[ValueProjection]> {
        self.independent_vertex_factor_shape()
            .map(|(_, columns, _)| columns)
    }

    /// Prove the bounded independent-vertex product law and prepare its two
    /// ordinary GLA factors. The logical definition and its transcript remain
    /// unchanged. All retained definition cells and copied names are governed.
    ///
    /// Only two vertex scans with total label predicates qualify. The original
    /// output must begin with slot 0 then slot 1; subsequent cells may only be
    /// identities or stored properties of those slots. Edges, correlations,
    /// hidden columns, custom ordering and every page (including LIMIT 0)
    /// retain the ordinary source. Restricting predicates to labels avoids
    /// moving fallible expression evaluation across an empty product factor.
    pub fn independent_vertex_factors_with_control<E>(
        &self,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), E>,
    ) -> Result<Option<PreparedGraphPatternFactors<'_>>, E> {
        let Some((body, columns, distinct)) = self.independent_vertex_factor_shape() else {
            return Ok(None);
        };

        let mut factors = [Self::empty(), Self::empty()];
        let mut mapping = Vec::new();
        let mut widths = [0; 2];
        for column in columns {
            let factor = match column {
                ValueProjection::Vertex { slot } | ValueProjection::Property { slot, .. } => {
                    slot.0 as usize
                }
                _ => {
                    unreachable!("the checked factor projection contains only vertices/properties")
                }
            };
            control(GlaExecutionEvent::ScratchEntry)?;
            mapping.push((factor, widths[factor]));
            widths[factor] += 1;
        }
        for (factor, prepared) in factors.iter_mut().enumerate() {
            control(GlaExecutionEvent::ScratchEntry)?;
            let mut operators = vec![GlaOperator::ScanVertices];
            for operator in body {
                control(GlaExecutionEvent::Work)?;
                if let GlaOperator::Select { slot, predicates } = operator
                    && slot.0 as usize == factor
                {
                    let mut local = Vec::new();
                    for predicate in predicates {
                        control(GlaExecutionEvent::ScratchEntry)?;
                        local.push(predicate.clone());
                    }
                    control(GlaExecutionEvent::ScratchEntry)?;
                    operators.push(GlaOperator::Select {
                        slot: BindingSlot(0),
                        predicates: local,
                    });
                }
            }
            let mut projection = Vec::new();
            let mut names = Vec::new();
            for ((column, name), &(owner, _)) in columns.iter().zip(&self.columns).zip(&mapping) {
                control(GlaExecutionEvent::Work)?;
                if owner != factor {
                    continue;
                }
                let column = match column {
                    ValueProjection::Vertex { .. } => ValueProjection::Vertex {
                        slot: BindingSlot(0),
                    },
                    ValueProjection::Property { key, .. } => ValueProjection::Property {
                        slot: BindingSlot(0),
                        key: *key,
                    },
                    _ => unreachable!("the checked factor projection is total"),
                };
                control(GlaExecutionEvent::ScratchEntry)?;
                projection.push(column);
                for _ in 0..name.len().div_ceil(GRAPH_VALUE_PAYLOAD_UNIT_BYTES) {
                    control(GlaExecutionEvent::ScratchEntry)?;
                }
                control(GlaExecutionEvent::ScratchEntry)?;
                names.push(name.clone());
            }
            control(GlaExecutionEvent::ScratchEntry)?;
            operators.push(GlaOperator::ProjectValues {
                columns: projection,
            });
            if distinct {
                control(GlaExecutionEvent::ScratchEntry)?;
                operators.push(GlaOperator::Distinct);
            }
            for operator in [
                GlaOperator::OrderByValues,
                GlaOperator::Limit {
                    offset: 0,
                    count: None,
                },
            ] {
                control(GlaExecutionEvent::ScratchEntry)?;
                operators.push(operator);
            }
            let mut logical = GlaPlan::from_operators(operators);
            logical.reverse_catalog = self.logical.reverse_catalog.clone();
            *prepared = Self {
                logical,
                variable_count: 1,
                edge_count: 0,
                columns: names,
            };
        }
        Ok(Some(PreparedGraphPatternFactors {
            original: self,
            factors,
            columns: mapping,
        }))
    }
}
