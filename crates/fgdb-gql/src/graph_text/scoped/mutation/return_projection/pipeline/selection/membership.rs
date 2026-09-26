//! Runtime-list IN lowering. The private Boolean expression reads the original
//! row, then the ordinary three-valued filter consumes it. Binding does not
//! expand a parameter into predicate instructions or reinterpret query text.

use super::*;

impl<'a> Parser<'a> {
    pub(super) fn selection_list_membership(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        selection: &mut Selection,
        left: ReadValueTemplate,
        negate: bool,
        at: usize,
    ) -> Result<(), GraphSetTextError> {
        // The same inference rule as UNWIND: a new direct RHS parameter is a
        // List. Existing declarations and prior uses still share the native
        // parameter table and may not be silently changed to a different kind.
        if let TokenKind::Parameter(name) = self.current.kind {
            self.parameter_types
                .entry(name.to_owned())
                .or_insert(GqlParameterType::List);
        }
        let list = self.selection_value(schema)?;
        if let ReadValueTemplate::Parameter { index, at } = &list {
            let found = self.syntax.parameters[*index].parameter_type;
            if !matches!(
                found,
                GqlParameterType::List
                    | GqlParameterType::Scalar(fgdb_types::CanonicalScalarKind::Null)
            ) {
                return Err(GraphSetTextError {
                    offset: *at,
                    kind: GraphSetTextErrorKind::Pattern(
                        GraphPatternTextErrorKind::ParameterTypeMismatch {
                            expected: GqlParameterType::List,
                            found,
                        },
                    ),
                });
            }
        }
        let value = ReadValueTemplate::In {
            value: Box::new(left),
            list: Box::new(list),
        };
        let test = ReadFilterOp::Compare {
            left: self.selection_cell(schema, selection, value, at)?,
            comparison: IntegerComparison::Equal,
            right: ReadFilterOperand::Literal(
                GqlScalarParameter::new(CanonicalScalar::Bool(true))
                    .expect("canonical true is a bounded scalar"),
            ),
        };
        emit(&mut selection.code, test, at)?;
        if negate {
            emit(&mut selection.code, ReadFilterOp::Not, at)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::{GqlQueryError, GqlQueryPolicy, PreparedGraphSetText};
    use fgdb_types::VId;

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
    }

    fn int(value: i64) -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Int(value))
    }

    fn null() -> GraphValue {
        GraphValue::Scalar(CanonicalScalar::Null)
    }

    fn list(values: Vec<GraphValue>) -> GraphValue {
        GraphValue::List(values.into_boxed_slice())
    }

    fn expected(values: &[i64]) -> Vec<GraphValueRow> {
        values
            .iter()
            .map(|value| GraphValueRow::from_owned_values(vec![int(*value)]))
            .collect()
    }

    fn prepare(statement: &str, arguments: &GqlParameters) -> PreparedGraphSet {
        PreparedGraphSetText::prepare(statement, |_, _| None)
            .unwrap()
            .bind_parameters(arguments)
            .unwrap()
    }

    fn run(query: &PreparedGraphSet) -> Vec<GraphValueRow> {
        query
            .execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || Ok::<_, usize>(()),
            )
            .unwrap()
            .value
    }

    fn execute(statement: &str) -> Vec<GraphValueRow> {
        run(&prepare(statement, &GqlParameters::new()))
    }

    #[test]
    fn list_alias_membership_preserves_duplicates_and_three_valued_negation() {
        assert_eq!(
            execute(
                "UNWIND [1, 2, 2, 3, NULL] AS n WITH n, [2, NULL] AS allowed \
                 WHERE n IN allowed RETURN n"
            ),
            expected(&[2, 2])
        );
        assert!(
            execute(
                "UNWIND [1, 2, 3, NULL] AS n WITH n, [2, NULL] AS allowed \
                 WHERE n NOT IN allowed RETURN n"
            )
            .is_empty()
        );
        let rows = execute(
            "UNWIND [1, NULL] AS n WITH n, [] AS allowed \
             WHERE n NOT IN allowed RETURN n",
        );
        assert_eq!(
            rows,
            vec![
                GraphValueRow::from_owned_values(vec![int(1)]),
                GraphValueRow::from_owned_values(vec![null()]),
            ]
        );
        for operator in ["IN", "NOT IN"] {
            assert!(
                execute(&format!(
                    "UNWIND [1, NULL] AS n WITH n, NULL AS allowed \
                     WHERE n {operator} allowed RETURN n"
                ))
                .is_empty()
            );
        }
    }

    #[test]
    fn inferred_list_parameters_rebind_without_mutating_templates() {
        let template = PreparedGraphSetText::prepare(
            "UNWIND [1, 2, 2, 3, NULL] AS n WITH n WHERE n IN $ids RETURN n",
            |_, _| None,
        )
        .unwrap();
        assert_eq!(template.parameter_schema().len(), 1);
        assert_eq!(template.parameter_schema()[0].parameter_type, GqlParameterType::List);
        let frozen = template.canonical_template_bytes();
        let mut bound_bytes = Vec::new();
        for (members, wanted) in [
            (vec![int(2), null(), int(2)], expected(&[2, 2])),
            (vec![int(1), int(3)], expected(&[1, 3])),
            (vec![], vec![]),
            (vec![null()], vec![]),
        ] {
            let arguments = GqlParameters::new().with_list("ids", members).unwrap();
            let bound = template.bind_parameters(&arguments).unwrap();
            assert_eq!(run(&bound), wanted);
            assert_eq!(template.canonical_template_bytes(), frozen);
            bound_bytes.push(bound.canonical_bytes());
        }
        assert_ne!(bound_bytes[0], bound_bytes[1]);
        assert_ne!(bound_bytes[2], bound_bytes[3]);
        assert!(template.bind_parameters(&GqlParameters::new()).is_err());
        assert!(
            template
                .bind_parameters(&GqlParameters::new().with_int64("ids", 2).unwrap())
                .is_err()
        );
        let extra = GqlParameters::new()
            .with_list("ids", vec![int(2)])
            .unwrap()
            .with_int64("extra", 1)
            .unwrap();
        assert!(template.bind_parameters(&extra).is_err());
    }

    #[test]
    fn conflicting_parameter_contexts_refuse_before_catalog_access() {
        for statement in [
            "MATCH (n:X) WITH n, $ids AS old WHERE n IN $ids RETURN n",
            "MATCH (n:X) WITH n WHERE n IN $ids AND 1 = $ids RETURN n",
        ] {
            let mut calls = 0;
            assert!(
                PreparedGraphSetText::prepare(statement, |_, _| {
                    calls += 1;
                    None
                })
                .is_err()
            );
            assert_eq!(calls, 0, "{statement}");
        }
        let mut calls = 0;
        assert!(
            PreparedGraphSetText::prepare_with_parameter_types(
                "MATCH (n:X) WITH n WHERE n IN $ids RETURN n",
                &[("ids", GqlParameterType::Int64)],
                |_, _| {
                    calls += 1;
                    None
                },
            )
            .is_err()
        );
        assert_eq!(calls, 0);
    }

    #[test]
    fn nested_lists_and_indexed_rhs_use_native_value_equality() {
        let rows = execute(
            "UNWIND [[1, 2], [1, NULL], [2, 3], []] AS n \
             WITH n, [[1, 2], [9]] AS allowed WHERE n IN allowed RETURN n",
        );
        assert_eq!(
            rows,
            vec![GraphValueRow::from_owned_values(vec![list(vec![int(1), int(2)])])]
        );
        assert_eq!(
            execute(
                "UNWIND [1, 2, 3] AS n WITH n, [[2], [1]] AS groups \
                 WHERE n IN groups[0] RETURN n"
            ),
            expected(&[2])
        );
        assert_eq!(
            execute(
                "WITH [NULL, 1] AS n, [[NULL, 2]] AS allowed \
                 WHERE n NOT IN allowed RETURN 1 AS kept"
            ),
            expected(&[1])
        );
    }

    #[test]
    fn indexed_list_parameters_infer_the_container_and_keep_bounds_semantics() {
        let template = PreparedGraphSetText::prepare(
            "UNWIND [1, 2, 3] AS n WITH n WHERE n IN $groups[0] RETURN n",
            |_, _| None,
        )
        .unwrap();
        assert_eq!(template.parameter_schema()[0].parameter_type, GqlParameterType::List);
        let arguments = GqlParameters::new()
            .with_list("groups", vec![list(vec![int(2)]), list(vec![int(1)])])
            .unwrap();
        assert_eq!(run(&template.bind_parameters(&arguments).unwrap()), expected(&[2]));
        let empty = GqlParameters::new().with_list("groups", vec![]).unwrap();
        assert!(run(&template.bind_parameters(&empty).unwrap()).is_empty());
    }

    #[test]
    fn membership_keeps_pages_and_private_output_columns_in_scope() {
        assert!(
            execute(
                "UNWIND [3, 1, 2] AS n WITH n, [2, 3] AS allowed ORDER BY n LIMIT 1 \
                 WHERE n IN allowed RETURN n"
            )
            .is_empty()
        );
        assert_eq!(
            execute(
                "UNWIND [3, 1, 2] AS n WITH n, [2, 3] AS allowed \
                 WHERE n IN allowed ORDER BY n LIMIT 1 RETURN n"
            ),
            expected(&[2])
        );
        let arguments = GqlParameters::new().with_list("ids", vec![int(2)]).unwrap();
        let template = PreparedGraphSetText::prepare(
            "UNWIND [1, 2, 2] AS n WITH n AS __fg_where_0 \
             WHERE __fg_where_0 IN $ids RETURN *",
            |_, _| None,
        )
        .unwrap();
        assert_eq!(template.columns(), &["__fg_where_0".to_owned()]);
        assert_eq!(run(&template.bind_parameters(&arguments).unwrap()), expected(&[2, 2]));
        assert!(
            PreparedGraphSetText::prepare(
                "UNWIND [1] AS n WITH n WHERE n IN $ids RETURN __fg_where_0",
                |_, _| None,
            )
            .is_err()
        );
    }

    #[test]
    fn late_nonlist_and_candidate_failures_cannot_hide_behind_empty_lists_or_limit_zero() {
        for operator in ["IN", "NOT IN"] {
            for statement in [
                format!(
                    "UNWIND [[1], 7] AS allowed WITH allowed \
                     WHERE 1 {operator} allowed RETURN allowed LIMIT 0"
                ),
                format!(
                    "UNWIND [1, 0] AS n WITH n, [] AS allowed \
                     WHERE 1 / n {operator} allowed RETURN n LIMIT 0"
                ),
            ] {
                let query = prepare(&statement, &GqlParameters::new());
                assert!(matches!(
                    query.execute_governed(
                        policy(),
                        |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                        || Ok::<_, usize>(()),
                    ),
                    Err(GqlQueryError::Source(crate::GraphSetExecutionError::Projection { .. }))
                ));
            }
        }
    }

    #[test]
    fn every_membership_pipeline_checkpoint_preserves_the_interrupt() {
        let query = prepare(
            "UNWIND [1, 2, 3] AS n WITH n, [1, 2] AS allowed \
             WHERE n IN allowed RETURN n",
            &GqlParameters::new(),
        );
        let mut checkpoints = 0;
        let result = query
            .execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || {
                    checkpoints += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(result.value, expected(&[1, 2]));
        assert!(checkpoints > 0);
        for stop in 1..=checkpoints {
            let mut seen = 0;
            let result = query.execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || {
                    seen += 1;
                    if seen == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(seen, stop);
        }
    }

    #[test]
    fn runtime_list_size_is_not_a_predicate_instruction_count() {
        let count = crate::algebra::MAX_PATTERN_PREDICATES + 1;
        let arguments = GqlParameters::new()
            .with_list("ids", vec![int(1); count])
            .unwrap();
        let query = prepare(
            "UNWIND [1] AS n WITH n WHERE n IN $ids RETURN n LIMIT 0",
            &arguments,
        );
        assert!(run(&query).is_empty());
        assert!(matches!(
            query.execute_governed(
                GqlQueryPolicy::new(10_000, 10_000, 0, 1_000_000),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || Ok::<_, usize>(()),
            ),
            Err(GqlQueryError::Evaluator(_))
        ));
    }

    #[test]
    fn full_width_vertex_membership_traverses_the_graph_source_once() {
        let high = VId(u128::MAX);
        let vertices = [VId(0), VId(u64::MAX.into()), high];
        let arguments = GqlParameters::new()
            .with_list("ids", vec![GraphValue::Vertex(high), GraphValue::Vertex(high)])
            .unwrap();
        let query = prepare("MATCH (n) WITH n WHERE n IN $ids RETURN n", &arguments);
        let mut calls = 0;
        let result = query
            .execute_governed(
                policy(),
                |pattern, budget| {
                    calls += 1;
                    pattern.plan().execute_governed_with_properties(
                        3,
                        vertices,
                        [],
                        |_, _| Ok::<_, ()>(true),
                        |_, _| Ok(None),
                        budget,
                        || Ok::<_, ()>(()),
                    )
                },
                || Ok::<_, ()>(()),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            result.value,
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Vertex(high)])]
        );
    }
}
