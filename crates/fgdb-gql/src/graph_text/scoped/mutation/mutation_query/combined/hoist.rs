//! Hidden identity-dependent keys shift summary columns, not expression locals.
//! Check every reference before changing any output; use the shared compiler's
//! complete expression remapper after admission. This is preparation only.

use crate::graph_text::aggregate::remap_output_columns;
use crate::mutation_text::{MutationIntegerTemplateOp, WriteReturnGroupTemplate};
use crate::set_text::{ReadProjectionTemplate, ReadValueTemplate};
use crate::{GraphIntegerOp, GraphPatternTextError, GraphPatternTextErrorKind};

fn columns_fit(value: &ReadValueTemplate, width: usize) -> bool {
    let fits = |value: &ReadValueTemplate| columns_fit(value, width);
    match value {
        ReadValueTemplate::Column(column) => *column < width,
        ReadValueTemplate::Integer { program, .. } => program.iter().all(|op| match op {
            MutationIntegerTemplateOp::Bound(
                GraphIntegerOp::Column(column)
                | GraphIntegerOp::ScalarColumn(column)
                | GraphIntegerOp::IdentityColumn { column, .. },
            ) => *column < width,
            _ => true,
        }),
        ReadValueTemplate::List(values) => values.iter().all(fits),
        ReadValueTemplate::Index { list, index } => fits(list) && fits(index),
        ReadValueTemplate::In { value, list } => fits(value) && fits(list),
        ReadValueTemplate::Equal { left, right }
        | ReadValueTemplate::Ordered { left, right, .. } => fits(left) && fits(right),
        ReadValueTemplate::Size(value)
        | ReadValueTemplate::Keys(value)
        | ReadValueTemplate::MapGet { map: value, .. }
        | ReadValueTemplate::ElementMetadata { value, .. }
        | ReadValueTemplate::Temporal {
            argument: value, ..
        }
        | ReadValueTemplate::Function {
            argument: value, ..
        } => fits(value),
        ReadValueTemplate::Comprehension { list, filter, map } => {
            fits(list) && filter.as_deref().is_none_or(fits) && map.as_deref().is_none_or(fits)
        }
        ReadValueTemplate::Quantifier {
            list, predicate, ..
        } => fits(list) && fits(predicate),
        ReadValueTemplate::Slice { list, from, to } => {
            fits(list) && from.as_deref().is_none_or(fits) && to.as_deref().is_none_or(fits)
        }
        ReadValueTemplate::Range { start, end, step } => {
            fits(start) && fits(end) && step.as_deref().is_none_or(fits)
        }
        ReadValueTemplate::Reduce { init, list, expr } => fits(init) && fits(list) && fits(expr),
        ReadValueTemplate::MapLiteral { values, guard, .. } => {
            values.iter().all(fits) && guard.as_deref().is_none_or(fits)
        }
        ReadValueTemplate::MapOverlay { base, values, .. } => fits(base) && values.iter().all(fits),
        ReadValueTemplate::Let { values, body } => values.iter().all(fits) && fits(body),
        ReadValueTemplate::Case { arms, otherwise } => {
            arms.iter()
                .all(|(condition, value)| fits(condition) && fits(value))
                && otherwise.as_deref().is_none_or(fits)
        }
        ReadValueTemplate::Literal(_)
        | ReadValueTemplate::Parameter { .. }
        | ReadValueTemplate::Local(_)
        | ReadValueTemplate::Random { .. } => true,
    }
}

impl WriteReturnGroupTemplate {
    /// Admit the entire output vector before shifting its summary references.
    /// Keys keep their positions; parameters, locals, and aggregate INPUT
    /// expressions are not in this coordinate space and are never changed.
    pub(crate) fn remap_hidden_key_outputs(
        projection: &mut [ReadProjectionTemplate],
        keys: usize,
        width: usize,
        at: usize,
    ) -> Result<(), GraphPatternTextError> {
        if keys > width
            || width >= crate::algebra::MAX_PATTERN_VERTICES
            || projection
                .iter()
                .any(|output| !columns_fit(&output.value, width))
        {
            return Err(GraphPatternTextError {
                offset: at,
                kind: GraphPatternTextErrorKind::Expected(
                    "bounded grouped write output references",
                ),
            });
        }
        let columns: Vec<_> = (0..width)
            .map(|column| column + usize::from(column >= keys))
            .collect();
        for output in projection {
            remap_output_columns(&mut output.value, &columns);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlScalarParameter, GraphSetColumnType};
    use fgdb_types::CanonicalScalar;

    fn projection(value: ReadValueTemplate) -> ReadProjectionTemplate {
        ReadProjectionTemplate {
            name: "result".into(),
            value,
        }
    }

    fn transcript(outputs: &[ReadProjectionTemplate]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for output in outputs {
            output.value.append_template_transcript(&mut bytes);
        }
        bytes
    }

    #[test]
    fn hidden_key_remapping_keeps_keys_locals_and_parameters_distinct() {
        use MutationIntegerTemplateOp as Op;
        use ReadValueTemplate as V;
        let expression = |summary| V::Let {
            values: vec![V::List(vec![V::Column(0), V::Column(summary)])],
            body: Box::new(V::List(vec![
                V::Local(100),
                V::Parameter {
                    index: 100,
                    at: 4,
                    null_type: GraphSetColumnType::Scalar,
                },
                V::Integer {
                    program: vec![
                        Op::Bound(GraphIntegerOp::Column(0)),
                        Op::Bound(GraphIntegerOp::ScalarColumn(summary)),
                        Op::Bound(GraphIntegerOp::Local(100)),
                        Op::Parameter { index: 100, at: 4 },
                    ],
                    at: 4,
                },
                V::Case {
                    arms: vec![(
                        V::Literal(GqlScalarParameter::new(CanonicalScalar::Bool(false)).unwrap()),
                        V::Column(summary),
                    )],
                    otherwise: Some(Box::new(V::Comprehension {
                        list: Box::new(V::Column(summary)),
                        filter: None,
                        map: Some(Box::new(V::Local(0))),
                    })),
                },
            ])),
        };
        let mut outputs = vec![projection(expression(1))];
        WriteReturnGroupTemplate::remap_hidden_key_outputs(&mut outputs, 1, 2, 4).unwrap();
        assert_eq!(
            transcript(&outputs),
            transcript(&[projection(expression(2))])
        );
        // A second hidden key must shift the same summaries again, not a local.
        WriteReturnGroupTemplate::remap_hidden_key_outputs(&mut outputs, 2, 3, 4).unwrap();
        assert_eq!(
            transcript(&outputs),
            transcript(&[projection(expression(3))])
        );
    }

    #[test]
    fn malformed_late_or_lazy_references_refuse_without_partial_remapping() {
        use ReadValueTemplate as V;
        let invalid = [
            V::Column(2),
            V::Case {
                arms: vec![],
                otherwise: Some(Box::new(V::Column(usize::MAX))),
            },
            V::Integer {
                program: vec![MutationIntegerTemplateOp::Bound(
                    GraphIntegerOp::ScalarColumn(2),
                )],
                at: 7,
            },
            V::MapLiteral {
                keys: vec!["bad".into()].into_boxed_slice(),
                values: vec![V::Column(0)],
                guard: Some(Box::new(V::Column(2))),
            },
        ];
        for value in invalid {
            let mut outputs = vec![projection(V::Column(1)), projection(value)];
            let before = transcript(&outputs);
            let error = WriteReturnGroupTemplate::remap_hidden_key_outputs(&mut outputs, 1, 2, 7)
                .unwrap_err();
            assert_eq!(error.offset, 7);
            assert_eq!(transcript(&outputs), before);
        }
    }
}
