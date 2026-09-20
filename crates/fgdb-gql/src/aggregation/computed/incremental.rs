//! Row-local computed input for incremental aggregate consumers.
//!
//! This reuses the batch projection evaluator. It does not scan a graph, build
//! an accumulator or publish a result. One old/final binding is transformed at
//! a time; its caller retains and retracts the resulting aggregate contribution.

use super::*;
use crate::{GraphSetColumnType, GraphSetValue};
use core::convert::Infallible;

impl PreparedGraphAggregate {
    fn incremental_source_column_type(&self, column: usize) -> Option<GraphSetColumnType> {
        match self.input.value_columns().get(column)? {
            ValueProjection::Property { .. } => Some(GraphSetColumnType::Scalar),
            ValueProjection::Vertex { .. } => Some(GraphSetColumnType::Vertex),
            _ => None,
        }
    }

    /// Type of an aggregate INPUT column, after the optional computed input.
    /// This is not an index into input_pattern().value_columns() when a
    /// projection exists. Scalar programs retain their nullable scalar domain;
    /// individual scalar kinds and integer overflow are checked at execution.
    /// A relational owner uses the COMPLETE pipeline's schema, not its first
    /// graph leaf. This is group-schema admission, not admission of the input
    /// operators. List/path/edge columns remain outside this group profile.
    #[must_use]
    pub fn incremental_input_column_type(&self, column: usize) -> Option<GraphSetColumnType> {
        if let Some(relation) = &self.relational_input {
            return relation.column_types().get(column).copied().filter(|kind| {
                matches!(kind, GraphSetColumnType::Scalar | GraphSetColumnType::Vertex)
            });
        }
        let Some(projection) = &self.computed_input else {
            return self.incremental_source_column_type(column);
        };
        match projection.get(column)?.value() {
            GraphSetValue::Column(source) => self.incremental_source_column_type(*source),
            GraphSetValue::Literal(_) | GraphSetValue::Integer(_) => {
                Some(GraphSetColumnType::Scalar)
            }
            GraphSetValue::Value(GraphValue::Scalar(_)) => Some(GraphSetColumnType::Scalar),
            GraphSetValue::Value(GraphValue::Vertex(_)) => Some(GraphSetColumnType::Vertex),
            _ => None,
        }
    }

    /// Scalar/vertex aggregate-input schema. A relational owner must separately
    /// admit and maintain its COMPLETE input tree; this never authorizes a
    /// graph-source adapter to execute only input_pattern(). Source topology,
    /// result clauses and aggregate functions require independent admission.
    #[must_use]
    pub fn supports_incremental_input(&self) -> bool {
        if let Some(relation) = &self.relational_input {
            return relation.column_types().len() <= MAX_PATTERN_VERTICES
                && (0..relation.column_types().len())
                    .all(|column| self.incremental_input_column_type(column).is_some());
        }
        if (0..self.input.value_columns().len())
                .any(|column| self.incremental_source_column_type(column).is_none())
        {
            return false;
        }
        let width = self
            .computed_input
            .as_ref()
            .map_or(self.input.value_columns().len(), Vec::len);
        (0..width).all(|column| self.incremental_input_column_type(column).is_some())
    }

    /// Transform one complete owned source row through the SAME projection
    /// evaluator as batch aggregation. The caller reserves source cell/payload
    /// storage before constructing `values`; all additional storage and work
    /// here use `control`. No public result-row quota is spent on private input.
    ///
    /// None is a definition/source-schema refusal, not a dropped match. Every
    /// declared computed column executes, even if no group or aggregate reads
    /// it. InputExpression.row is zero because this call owns one local binding,
    /// not a graph-wide enumeration ordinal. Arithmetic and control errors are
    /// never turned into NULL, a missing input or a partial projected row.
    pub fn evaluate_incremental_input<C>(
        &self,
        values: Vec<GraphValue>,
        control: &mut impl FnMut(GlaExecutionEvent) -> Result<(), C>,
    ) -> Result<Option<GraphValueRow>, GqlQueryError<GraphAggregateError<Infallible>, C>> {
        let mut govern = |event| {
            control(event).map_err(GqlQueryError::<GraphAggregateError<Infallible>, C>::Interrupted)
        };
        govern(GlaExecutionEvent::Work)?;
        // One source binding is not a completed relation. Even identical
        // schemas cannot erase filters, DISTINCT, joins or local pages.
        if self.relational_input.is_some() {
            return Ok(None);
        }
        // Validate bounded widths before any per-cell loop or row construction.
        if values.len() != self.input.value_columns().len()
            || values.is_empty()
            || values.len() > MAX_PATTERN_VERTICES
        {
            return Ok(None);
        }
        for (column, value) in values.iter().enumerate() {
            govern(GlaExecutionEvent::Work)?;
            let accepts = match self.incremental_source_column_type(column) {
                Some(GraphSetColumnType::Scalar) => matches!(value, GraphValue::Scalar(_)),
                Some(GraphSetColumnType::Vertex) => {
                    matches!(value, GraphValue::Vertex(_)) || value.is_null()
                }
                _ => false,
            };
            if !accepts {
                return Ok(None);
            }
        }
        if let Some(projection) = &self.computed_input {
            for _ in projection {
                govern(GlaExecutionEvent::Work)?;
            }
        }
        if !self.supports_incremental_input() {
            return Ok(None);
        }
        govern(GlaExecutionEvent::ScratchEntry)?;
        let source = GraphValueRow::from_owned_values(values);
        let result = if let Some(projection) = &self.computed_input {
            GraphSetProjection::evaluate_row_with_control(
                &source,
                projection,
                &mut govern,
                |column, error| {
                    GqlQueryError::Source(GraphAggregateError::InputExpression {
                        row: 0,
                        column,
                        error,
                    })
                },
            )?
        } else {
            source
        };
        govern(GlaExecutionEvent::Work)?;
        Ok(Some(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphColumn, GraphPatternBuilder};
    use crate::{
        GraphIntegerBinary, GraphIntegerErrorKind, GraphIntegerExpression, GraphIntegerOp,
    };

    fn input() -> PreparedGraphPattern<GraphValueRow> {
        let mut source = GraphPatternBuilder::new();
        source.vertex("n").unwrap();
        source
            .prepare_values(
                &[
                    GraphColumn::vertex("id", "n"),
                    GraphColumn::property("quantity", "n", PropertyKeyId(1)),
                    GraphColumn::property("price", "n", PropertyKeyId(2)),
                    GraphColumn::property("category", "n", PropertyKeyId(3)),
                ],
                0,
                None,
            )
            .unwrap()
            .with_duplicates()
    }

    fn definition() -> PreparedGraphAggregate {
        let product = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(1),
            GraphIntegerOp::Column(2),
            GraphIntegerOp::Binary(GraphIntegerBinary::Multiply),
        ])
        .unwrap();
        let lower = GraphIntegerExpression::prepare_scalar(&[
            GraphIntegerOp::ScalarColumn(3),
            GraphIntegerOp::Lower,
        ])
        .unwrap();
        PreparedGraphAggregate::prepare_projected(
            input(),
            vec![
                GraphSetProjection::new("cost", GraphSetValue::Integer(product)),
                GraphSetProjection::new("bucket", GraphSetValue::Integer(lower)),
                GraphSetProjection::new("owner", GraphSetValue::Column(0)),
            ],
            &[1],
            &[
                GraphAggregate::sum_int("sum", 0),
                GraphAggregate::min("first", 2),
            ],
            0,
            None,
        )
        .unwrap()
    }

    fn source() -> Vec<GraphValue> {
        vec![
            GraphValue::Vertex(VId(u128::MAX)),
            GraphValue::Scalar(CanonicalScalar::Int(3)),
            GraphValue::Scalar(CanonicalScalar::Int(7)),
            GraphValue::Scalar(CanonicalScalar::ucs_basic_text("BOOKS").unwrap()),
        ]
    }

    #[test]
    fn projected_schema_and_row_use_output_positions_not_source_positions() {
        let query = definition();
        assert!(query.supports_incremental_input());
        assert_eq!(
            query.incremental_input_column_type(0),
            Some(GraphSetColumnType::Scalar)
        );
        assert_eq!(
            query.incremental_input_column_type(2),
            Some(GraphSetColumnType::Vertex)
        );
        assert_eq!(query.incremental_input_column_type(3), None);
        let row = query
            .evaluate_incremental_input(source(), &mut |_| Ok::<_, ()>(()))
            .unwrap()
            .unwrap();
        assert_eq!(
            row.values(),
            &[
                GraphValue::Scalar(CanonicalScalar::Int(21)),
                GraphValue::Scalar(CanonicalScalar::ucs_basic_text("books").unwrap()),
                GraphValue::Vertex(VId(u128::MAX)),
            ]
        );
        assert!(
            query
                .evaluate_incremental_input(vec![], &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
        let mut wrong = source();
        wrong[0] = GraphValue::Scalar(CanonicalScalar::Int(1));
        assert!(
            query
                .evaluate_incremental_input(wrong, &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn every_checkpoint_aborts_private_projection_and_allows_retry() {
        let query = definition();
        let before = query.canonical_bytes();
        let mut calls = 0;
        let expected = query
            .evaluate_incremental_input(source(), &mut |_| {
                calls += 1;
                Ok::<_, usize>(())
            })
            .unwrap()
            .unwrap();
        for stop in 1..=calls {
            let mut seen = 0;
            let result = query.evaluate_incremental_input(source(), &mut |_| {
                seen += 1;
                if seen == stop { Err(stop) } else { Ok(()) }
            });
            assert!(matches!(result, Err(GqlQueryError::Interrupted(at)) if at == stop));
            assert_eq!(seen, stop);
            assert_eq!(query.canonical_bytes(), before);
            assert_eq!(
                query
                    .evaluate_incremental_input(source(), &mut |_| Ok::<_, ()>(()))
                    .unwrap()
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn hidden_computed_errors_are_eager_but_coalesce_branches_remain_lazy() {
        let fallback = GraphIntegerExpression::prepare(&[
            GraphIntegerOp::Column(1),
            GraphIntegerOp::Literal(Some(1)),
            GraphIntegerOp::Literal(Some(0)),
            GraphIntegerOp::Binary(GraphIntegerBinary::Divide),
            GraphIntegerOp::Coalesce,
        ])
        .unwrap();
        let query = PreparedGraphAggregate::prepare_projected(
            input(),
            vec![GraphSetProjection::new(
                "unused",
                GraphSetValue::Integer(fallback),
            )],
            &[],
            &[GraphAggregate::count_rows("count")],
            0,
            None,
        )
        .unwrap();
        assert!(
            query
                .evaluate_incremental_input(source(), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_some()
        );
        let mut null = source();
        null[1] = GraphValue::Scalar(CanonicalScalar::Null);
        let error = query.evaluate_incremental_input(null, &mut |_| Ok::<_, ()>(()));
        assert!(
            matches!(error, Err(GqlQueryError::Source(GraphAggregateError::InputExpression {
            row: 0, column: 0, error,
        })) if error.kind == GraphIntegerErrorKind::DivisionByZero)
        );
    }

    #[test]
    fn unsupported_collection_inputs_fail_closed_even_when_only_counting_rows() {
        let query = PreparedGraphAggregate::prepare_projected(
            input(),
            vec![GraphSetProjection::new(
                "unused",
                GraphSetValue::List(vec![GraphSetValue::Column(1)]),
            )],
            &[],
            &[GraphAggregate::count_rows("count")],
            0,
            None,
        )
        .unwrap();
        assert!(!query.supports_incremental_input());
        assert!(
            query
                .evaluate_incremental_input(source(), &mut |_| Ok::<_, ()>(()))
                .unwrap()
                .is_none()
        );
    }
}
