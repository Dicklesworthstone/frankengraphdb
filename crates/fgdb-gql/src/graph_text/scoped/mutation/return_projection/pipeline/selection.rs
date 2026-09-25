//! Computed WHERE operands lower to the existing Project/Filter/Project IR.
//! All expressions read the original row. Private cells cannot become aliases,
//! change multiplicity, escape into RETURN *, or move across an input page.
//! IN accepts a bounded list of expressions; list parameters and subquery RHSs
//! are not part of this profile. Text predicates use the shared scalar kernels.

use super::*;
use crate::algebra::IntegerComparison;
use crate::mutation_text::MutationIntegerTemplateOp;
use crate::{GqlListParameter, GraphIntegerOp};

#[derive(Default)]
struct Selection {
    code: Vec<ReadFilterOp>,
    values: Vec<ReadValueTemplate>,
}

// These are postfix construction instructions, not compiled jump offsets.
// Concatenating operands preserves CASE/COALESCE laziness when the shared
// scalar compiler subsequently constructs its checked execution program.
fn scalar_program(
    value: ReadValueTemplate,
    at: usize,
) -> Result<Vec<MutationIntegerTemplateOp>, GraphSetTextError> {
    Ok(match value {
        ReadValueTemplate::Column(column) => vec![MutationIntegerTemplateOp::Bound(
            GraphIntegerOp::ScalarColumn(column),
        )],
        ReadValueTemplate::Literal(value) => vec![MutationIntegerTemplateOp::Bound(
            GraphIntegerOp::Scalar(value.predicate(IntegerComparison::Equal)),
        )],
        ReadValueTemplate::Parameter { index, at } => {
            vec![MutationIntegerTemplateOp::Parameter { index, at }]
        }
        ReadValueTemplate::Integer { program, .. } => program,
        _ => return Err(expected(at, "scalar predicate expression")),
    })
}

impl<'a> Parser<'a> {
    pub(super) fn row_selection(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        stages: &mut Vec<ReadStageTemplate>,
        depth: &mut usize,
        at: usize,
    ) -> Result<(), GraphSetTextError> {
        let mut selection = Selection::default();
        self.selection_or(schema, 0, &mut selection)?;
        let types: Vec<_> = schema.iter().map(|(_, kind)| *kind).collect();
        let mut expanded_types = types.clone();
        let mut projection = Vec::new();
        let mut restore = Vec::new();
        if !selection.values.is_empty() {
            // NULL is a scalar shape witness, never a substituted execution
            // value. Lists need a list witness for SIZE/index schema admission.
            // The shared compiler checks every composed operator/branch below;
            // no witness is retained in the prepared definition or executed.
            let null = GqlScalarParameter::new(CanonicalScalar::Null)
                .expect("canonical null is a bounded scalar");
            let witnesses: Vec<_> = self
                .syntax
                .parameters
                .iter()
                .map(|spec| {
                    if spec.parameter_type == GqlParameterType::List {
                        GqlParameterValue::List(
                            GqlListParameter::new(Vec::new()).expect("empty list is bounded"),
                        )
                    } else {
                        GqlParameterValue::Scalar(null.clone())
                    }
                })
                .collect();
            for (column, (name, _)) in schema.iter().enumerate() {
                let output = ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                };
                restore.push(output.clone());
                projection.push(output);
            }
            let mut suffix = 0;
            for value in selection.values {
                let column = projection.len();
                let shape = bind_read_value(&value, &witnesses)?;
                let kind =
                    GraphSetProjection::admit_output(&shape, &types, column).map_err(|kind| {
                        GraphSetTextError {
                            offset: at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        }
                    })?;
                expanded_types.push(kind);
                let name = loop {
                    let candidate = format!("__fg_where_{suffix}");
                    suffix += 1;
                    if !projection.iter().any(|output| output.name == candidate) {
                        break candidate;
                    }
                };
                projection.push(ReadProjectionTemplate { name, value });
            }
        }
        let shape = bind_filter(&selection.code, None)?;
        GraphSetPredicateOp::validate_schema(&expanded_types, &shape).map_err(|kind| {
            GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::FilterBuild(kind),
            }
        })?;
        if !projection.is_empty() {
            append_stage(
                stages,
                ReadStageTemplate::Project {
                    at,
                    projection,
                    quantifier: GraphSetQuantifier::All,
                },
                depth,
            )?;
        }
        append_stage(
            stages,
            ReadStageTemplate::Filter {
                at,
                code: selection.code,
            },
            depth,
        )?;
        if !restore.is_empty() {
            append_stage(
                stages,
                ReadStageTemplate::Project {
                    at,
                    projection: restore,
                    quantifier: GraphSetQuantifier::All,
                },
                depth,
            )?;
        }
        Ok(())
    }

    fn selection_or(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
        selection: &mut Selection,
    ) -> Result<(), GraphSetTextError> {
        self.selection_and(schema, depth, selection)?;
        while self.is_word("OR") {
            let at = self.current.at;
            self.advance()?;
            self.selection_and(schema, depth, selection)?;
            emit(&mut selection.code, ReadFilterOp::Or, at)?;
        }
        Ok(())
    }

    fn selection_and(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
        selection: &mut Selection,
    ) -> Result<(), GraphSetTextError> {
        self.selection_predicate(schema, depth, selection)?;
        while self.is_word("AND") {
            let at = self.current.at;
            self.advance()?;
            self.selection_predicate(schema, depth, selection)?;
            emit(&mut selection.code, ReadFilterOp::And, at)?;
        }
        Ok(())
    }

    fn selection_predicate(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        depth: usize,
        selection: &mut Selection,
    ) -> Result<(), GraphSetTextError> {
        let at = self.current.at;
        if depth > MAX_FILTER_NESTING {
            return Err(expected(at, "bounded row predicate nesting"));
        }
        let alias = matches!(self.current.kind, TokenKind::Word(word)
            if schema.iter().any(|(name, _)| name.text == word));
        if self.is_word("NOT") && !alias {
            self.advance()?;
            self.selection_predicate(schema, depth + 1, selection)?;
            return emit(&mut selection.code, ReadFilterOp::Not, at);
        }
        if self.is_punct(b'(') && !self.selection_parenthesized_operand()? {
            self.advance()?;
            self.selection_or(schema, depth + 1, selection)?;
            self.punct(b')', ")")?;
            return Ok(());
        }
        let left = self.selection_value(schema)?;
        if self.is_word("IN") || self.is_word("BETWEEN") || self.is_word("NOT") {
            return self.selection_membership(schema, selection, left, at);
        }
        let text_op = if self.take_word("STARTS")? {
            self.word("WITH")?;
            Some(GraphIntegerOp::StartsWith)
        } else if self.take_word("ENDS")? {
            self.word("WITH")?;
            Some(GraphIntegerOp::EndsWith)
        } else if self.take_word("CONTAINS")? {
            Some(GraphIntegerOp::Contains)
        } else {
            None
        };
        if let Some(op) = text_op {
            let right = self.selection_value(schema)?;
            self.selection_text_parameters(&left, &right, at)?;
            let mut program = scalar_program(left, at)?;
            program.extend(scalar_program(right, at)?);
            program.push(MutationIntegerTemplateOp::Bound(op));
            let value = ReadValueTemplate::Integer { program, at };
            let test = ReadFilterOp::Compare {
                left: self.selection_cell(schema, selection, value, at)?,
                comparison: IntegerComparison::Equal,
                right: ReadFilterOperand::Literal(
                    GqlScalarParameter::new(CanonicalScalar::Bool(true))
                        .expect("canonical true is a bounded scalar"),
                ),
            };
            return emit(&mut selection.code, test, at);
        }
        let op = if self.take_word("IS")? {
            let negate = self.take_word("NOT")?;
            self.word("NULL")?;
            ReadFilterOp::IsNull {
                operand: self.selection_cell(schema, selection, left, at)?,
                is_null: !negate,
            }
        } else if matches!(
            self.current.kind,
            TokenKind::Punct(b'=' | b'!' | b'<' | b'>')
        ) {
            let comparison = self.comparison()?;
            let right = self.selection_value(schema)?;
            ReadFilterOp::Compare {
                left: self.selection_cell(schema, selection, left, at)?,
                comparison,
                right: self.selection_cell(schema, selection, right, at)?,
            }
        } else {
            match left {
                ReadValueTemplate::Literal(value) => {
                    let truth = match value.value() {
                        CanonicalScalar::Null => None,
                        CanonicalScalar::Bool(value) => Some(*value),
                        _ => return Err(expected(at, "Boolean row predicate")),
                    };
                    ReadFilterOp::Truth(truth)
                }
                value => {
                    // Double NOT checks the Boolean domain without changing
                    // TRUE/FALSE/UNKNOWN. Comparing an unchecked cell to TRUE
                    // would silently turn non-Boolean inputs into false.
                    let mut program = scalar_program(value, at)?;
                    program.push(MutationIntegerTemplateOp::Bound(GraphIntegerOp::Not));
                    program.push(MutationIntegerTemplateOp::Bound(GraphIntegerOp::Not));
                    let value = ReadValueTemplate::Integer { program, at };
                    ReadFilterOp::Compare {
                        left: self.selection_cell(schema, selection, value, at)?,
                        comparison: IntegerComparison::Equal,
                        right: ReadFilterOperand::Literal(
                            GqlScalarParameter::new(CanonicalScalar::Bool(true))
                                .expect("canonical true is a bounded scalar"),
                        ),
                    }
                }
            }
        };
        emit(&mut selection.code, op, at)
    }

    fn selection_text_parameters(
        &mut self,
        left: &ReadValueTemplate,
        right: &ReadValueTemplate,
        at: usize,
    ) -> Result<(), GraphSetTextError> {
        use fgdb_types::CanonicalScalarKind;
        let expected_type = GqlParameterType::Scalar(CanonicalScalarKind::Text);
        let operands = [left, right];
        for operand in operands {
            let ReadValueTemplate::Parameter { index, .. } = operand else {
                continue;
            };
            let local_uses = operands
                .iter()
                .filter(|value| {
                    matches!(value,
                        ReadValueTemplate::Parameter { index: other, .. } if other == index)
                })
                .count();
            let spec = &mut self.syntax.parameters[*index];
            // A direct parameter used only by this text predicate has a known
            // required kind. Never reinterpret a declaration or an earlier
            // numeric use; all other contexts still share one frozen schema.
            if spec.parameter_type == GqlParameterType::Int64
                && spec.occurrences == local_uses
                && !self.parameter_types.contains_key(&spec.name)
            {
                spec.parameter_type = expected_type;
                self.parameter_types
                    .insert(spec.name.clone(), expected_type);
            }
            if !matches!(
                spec.parameter_type,
                GqlParameterType::Scalar(CanonicalScalarKind::Text | CanonicalScalarKind::Null)
            ) {
                return Err(GraphSetTextError {
                    offset: at,
                    kind: GraphSetTextErrorKind::Pattern(
                        GraphPatternTextErrorKind::ParameterTypeMismatch {
                            expected: expected_type,
                            found: spec.parameter_type,
                        },
                    ),
                });
            }
        }
        Ok(())
    }

    fn selection_membership(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        selection: &mut Selection,
        left: ReadValueTemplate,
        at: usize,
    ) -> Result<(), GraphSetTextError> {
        let negate = self.take_word("NOT")?;
        // A computed candidate is admitted/evaluated once, including for an
        // empty IN list. Neither duplicate members nor negation duplicate rows.
        let left = self.selection_cell(schema, selection, left, at)?;
        if self.take_word("IN")? {
            self.punct(b'[', "bounded IN expression list")?;
            if self.take(b']')? {
                emit(&mut selection.code, ReadFilterOp::Truth(Some(false)), at)?;
            } else {
                let mut first = true;
                loop {
                    let value = self.selection_value(schema)?;
                    let right = self.selection_cell(schema, selection, value, at)?;
                    emit(
                        &mut selection.code,
                        ReadFilterOp::Compare {
                            left: left.clone(),
                            comparison: IntegerComparison::Equal,
                            right,
                        },
                        at,
                    )?;
                    if !first {
                        emit(&mut selection.code, ReadFilterOp::Or, at)?;
                    }
                    first = false;
                    if self.take(b']')? {
                        break;
                    }
                    self.punct(b',', ", or ]")?;
                }
            }
        } else if self.take_word("BETWEEN")? {
            let value = self.selection_value(schema)?;
            let lower = self.selection_cell(schema, selection, value, at)?;
            self.word("AND")?;
            let value = self.selection_value(schema)?;
            let upper = self.selection_cell(schema, selection, value, at)?;
            emit(
                &mut selection.code,
                ReadFilterOp::Compare {
                    left: left.clone(),
                    comparison: IntegerComparison::GreaterOrEqual,
                    right: lower,
                },
                at,
            )?;
            emit(
                &mut selection.code,
                ReadFilterOp::Compare {
                    left,
                    comparison: IntegerComparison::LessOrEqual,
                    right: upper,
                },
                at,
            )?;
            emit(&mut selection.code, ReadFilterOp::And, at)?;
        } else {
            return Err(expected(self.current.at, "IN or BETWEEN after NOT"));
        }
        if negate {
            emit(&mut selection.code, ReadFilterOp::Not, at)?;
        }
        Ok(())
    }

    fn selection_value(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ReadValueTemplate, GraphSetTextError> {
        // The shared resolved-expression entry point stops before comparisons
        // and Boolean connectors. Parentheses and CASE retain their own inner
        // expressions. This callback resolves ONLY the current WITH aliases.
        // Hidden boundary reads resolve only as `alias.property`, never by
        // their private names.
        let visible = self
            .boundary_reads
            .as_ref()
            .filter(|boundary| boundary.width == schema.len())
            .map_or(schema.len(), |boundary| boundary.visible);
        self.read_resolved_value(
            &mut |parser| {
                if let Some(column) = parser.boundary_read(schema.len())? {
                    return Ok(Some(column));
                }
                if let TokenKind::Word(word) = parser.current.kind
                    && let Some(column) = schema[..visible]
                        .iter()
                        .position(|(name, _)| name.text == word)
                {
                    parser.advance()?;
                    return Ok(Some(column));
                }
                Ok(None)
            },
            0,
        )
    }

    fn selection_cell(
        &self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        selection: &mut Selection,
        value: ReadValueTemplate,
        at: usize,
    ) -> Result<ReadFilterOperand, GraphSetTextError> {
        Ok(match value {
            ReadValueTemplate::Column(column) => ReadFilterOperand::Column(column),
            ReadValueTemplate::Literal(value) => ReadFilterOperand::Literal(value),
            ReadValueTemplate::Parameter { index, at }
                if self.syntax.parameters[index].parameter_type != GqlParameterType::List =>
            {
                ReadFilterOperand::Parameter { index, at }
            }
            value => {
                let column = schema.len() + selection.values.len();
                if column >= MAX_PATTERN_VERTICES {
                    return Err(GraphSetTextError {
                        offset: at,
                        kind: GraphSetTextErrorKind::ProjectionBuild(
                            crate::GraphSetProjectionError::TooManyColumns {
                                limit: MAX_PATTERN_VERTICES,
                                observed: column + 1,
                            },
                        ),
                    });
                }
                selection.values.push(value);
                ReadFilterOperand::Column(column)
            }
        })
    }

    fn selection_parenthesized_operand(&self) -> Result<bool, GraphPatternTextError> {
        // Distinguish (a + 1) > b from (a = b OR c IS NULL), without consuming
        // tokens, changing parameter accounting, or reparsing the statement.
        let mut lexer = self.lexer.clone();
        let mut depth = 1;
        loop {
            match lexer.next()?.kind {
                TokenKind::Punct(b'(') => depth += 1,
                TokenKind::Punct(b')') => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                TokenKind::End => return Ok(false),
                _ => {}
            }
        }
        Ok(match lexer.next()?.kind {
            TokenKind::Punct(
                b'+' | b'-' | b'*' | b'/' | b'%' | b'|' | b'[' | b'=' | b'!' | b'<' | b'>',
            ) => true,
            TokenKind::Word(word) => ["IS", "IN", "BETWEEN", "NOT", "STARTS", "ENDS", "CONTAINS"]
                .iter()
                .any(|operator| word.eq_ignore_ascii_case(operator)),
            _ => false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueRow};
    use crate::{GqlQueryError, GqlQueryPolicy, PreparedGraphSetText};

    fn policy() -> GqlQueryPolicy {
        GqlQueryPolicy::new(10_000, 10_000, 1_000_000, 1_000_000)
    }

    fn prepare(statement: &str) -> PreparedGraphSet {
        PreparedGraphSetText::prepare(statement, |_, _| None)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap()
    }

    fn execute(statement: &str) -> Vec<GraphValueRow> {
        prepare(statement)
            .execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || Ok::<_, usize>(()),
            )
            .unwrap()
            .value
    }

    fn row(value: i64) -> GraphValueRow {
        GraphValueRow::from_owned_values(vec![GraphValue::Scalar(CanonicalScalar::Int(value))])
    }

    #[test]
    fn arithmetic_operands_preserve_boolean_precedence_and_nulls() {
        assert_eq!(
            execute(
                "UNWIND [1, 2, 3, NULL] AS n WITH n \
                 WHERE (n + 1) * 2 >= 6 AND (n = 2 OR n IS NULL) OR n = 1 RETURN n"
            ),
            vec![row(1), row(2)]
        );
        assert_eq!(
            execute("UNWIND [1, 2, 3] AS n WITH n WHERE NOT (n + 1 = 3 OR n = 1) RETURN n"),
            vec![row(3)]
        );
    }

    #[test]
    fn computed_filters_stay_between_their_written_pages() {
        assert_eq!(
            execute(
                "UNWIND [3, 1, 2] AS n WITH n ORDER BY n LIMIT 1 \
                 WHERE n + 0 > 1 ORDER BY n DESC LIMIT 1 RETURN n"
            ),
            Vec::<GraphValueRow>::new()
        );
        assert_eq!(
            execute(
                "UNWIND [3, 1, 2] AS n WITH n WHERE n + 0 > 1 \
                 ORDER BY n LIMIT 1 RETURN n"
            ),
            vec![row(2)]
        );
    }

    #[test]
    fn case_and_list_operands_use_existing_checked_projection_semantics() {
        assert_eq!(
            execute(
                "UNWIND [0, 2, 5] AS n WITH n \
                 WHERE CASE WHEN n = 0 THEN 0 ELSE 10 / n END > 1 RETURN n"
            ),
            vec![row(2), row(5)]
        );
        let rows = execute(
            "UNWIND [[1, 2], [3], []] AS xs WITH xs \
             WHERE SIZE(xs) = 2 AND xs[0] = 1 RETURN xs",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0],
            GraphValueRow::from_owned_values(vec![GraphValue::List(
                vec![
                    GraphValue::Scalar(CanonicalScalar::Int(1)),
                    GraphValue::Scalar(CanonicalScalar::Int(2)),
                ]
                .into_boxed_slice(),
            )])
        );
    }

    #[test]
    fn boolean_aliases_keep_only_true_and_reject_nonboolean_rows() {
        assert_eq!(
            execute("UNWIND [TRUE, FALSE, NULL] AS keep WITH keep WHERE keep RETURN keep"),
            vec![GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                CanonicalScalar::Bool(true),
            )])]
        );
        for count in [0, 1] {
            let query = prepare(&format!(
                "UNWIND [1] AS n WITH n WHERE n RETURN n LIMIT {count}"
            ));
            assert!(matches!(
                query.execute_governed(
                    policy(),
                    |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                    || Ok::<_, usize>(()),
                ),
                Err(GqlQueryError::Source(
                    crate::GraphSetExecutionError::Projection { .. }
                ))
            ));
        }
    }

    #[test]
    fn private_cells_do_not_escape_or_capture_user_aliases() {
        assert_eq!(
            execute(
                "UNWIND [1, 2, 2] AS n WITH n AS __fg_where_0 \
                 WHERE __fg_where_0 + 1 > 2 RETURN *"
            ),
            vec![row(2), row(2)]
        );
        let error = PreparedGraphSetText::prepare(
            "UNWIND [1] AS n WITH n WHERE n + 1 > 0 RETURN __fg_where_0",
            |_, _| None,
        )
        .unwrap_err();
        assert!(matches!(
            error.kind,
            GraphSetTextErrorKind::Pattern(GraphPatternTextErrorKind::UnknownVariable)
        ));
        let mut calls = 0;
        let result = PreparedGraphSetText::prepare(
            "MATCH (n) WITH n.p AS value WHERE n.p + 1 > 1 RETURN value",
            |_, _| {
                calls += 1;
                None
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn plain_filters_keep_their_original_ir_and_vertex_identity_domain() {
        for (statement, schema) in [
            (
                "n = 1 OR n IS NULL",
                vec![(Name { text: "n", at: 0 }, GraphSetColumnType::Scalar)],
            ),
            (
                "a = b",
                vec![
                    (Name { text: "a", at: 0 }, GraphSetColumnType::Vertex),
                    (Name { text: "b", at: 0 }, GraphSetColumnType::Vertex),
                ],
            ),
        ] {
            let mut legacy = Parser::new(statement).unwrap();
            let mut expected = Vec::new();
            legacy.row_disjunction(&schema, 0, &mut expected).unwrap();
            let mut parser = Parser::new(statement).unwrap();
            let mut stages = Vec::new();
            let mut depth = 2;
            parser
                .row_selection(&schema, &mut stages, &mut depth, 0)
                .unwrap();
            assert_eq!(depth, 3);
            let [ReadStageTemplate::Filter { code, .. }] = stages.as_slice() else {
                panic!("plain filters must not gain projection stages");
            };
            let transcript = |code: &[ReadFilterOp]| {
                let mut bytes = Vec::new();
                for op in code {
                    op.append_template_transcript(&mut bytes);
                }
                bytes
            };
            assert_eq!(transcript(code), transcript(&expected));
        }
    }

    #[test]
    fn computed_predicates_rebind_without_mutating_the_template() {
        let prepared = PreparedGraphSetText::prepare(
            "UNWIND [1, 2, 3] AS n WITH n WHERE n + $step > $minimum RETURN n",
            |_, _| None,
        )
        .unwrap();
        let frozen = prepared.canonical_template_bytes();
        for (step, minimum, expected) in [(1, 3, vec![row(3)]), (3, 4, vec![row(2), row(3)])] {
            let arguments = GqlParameters::new()
                .with_int64("step", step)
                .unwrap()
                .with_int64("minimum", minimum)
                .unwrap();
            let result = prepared
                .bind_parameters(&arguments)
                .unwrap()
                .execute_governed(
                    policy(),
                    |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                    || Ok::<_, usize>(()),
                )
                .unwrap();
            assert_eq!(result.value, expected);
            assert_eq!(prepared.canonical_template_bytes(), frozen);
        }
    }

    #[test]
    fn late_errors_and_each_cancellation_refuse_without_a_result_prefix() {
        let query = prepare("UNWIND [1, 0] AS n WITH n WHERE 10 / n > 1 RETURN n LIMIT 0");
        assert!(matches!(
            query.execute_governed(
                policy(),
                |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                || Ok::<_, usize>(()),
            ),
            Err(GqlQueryError::Source(
                crate::GraphSetExecutionError::Projection { .. }
            ))
        ));
        let query = prepare("UNWIND [1, 2, 3] AS n WITH n WHERE n + 1 > 2 RETURN n");
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
        assert_eq!(result.value, vec![row(2), row(3)]);
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
    fn membership_preserves_null_logic_and_input_occurrences() {
        assert_eq!(
            execute("UNWIND [1, 2, 2, 3, NULL] AS n WITH n WHERE n IN [2, NULL] RETURN n"),
            vec![row(2), row(2)]
        );
        assert!(
            execute("UNWIND [1, 2, 3, NULL] AS n WITH n WHERE n NOT IN [2, NULL] RETURN n")
                .is_empty()
        );
        assert_eq!(
            execute("UNWIND [1, 2, 2] AS n WITH n WHERE n IN [n, n] RETURN n"),
            vec![row(1), row(2), row(2)]
        );
        assert!(execute("UNWIND [1, NULL] AS n WITH n WHERE n IN [] RETURN n").is_empty());
        assert_eq!(
            execute("UNWIND [1, NULL] AS n WITH n WHERE n NOT IN [] RETURN n").len(),
            2
        );
    }

    #[test]
    fn ranges_and_computed_members_keep_boolean_boundaries() {
        assert_eq!(
            execute(
                "UNWIND [1, 2, 3, NULL] AS n WITH n \
                 WHERE (n + 0) BETWEEN 2 AND 3 AND n < 3 RETURN n"
            ),
            vec![row(2)]
        );
        assert_eq!(
            execute(
                "UNWIND [1, 2, 3, NULL] AS n WITH n \
                 WHERE (n + 0) NOT BETWEEN 2 AND 3 OR n IN [n + 1, 2] RETURN n"
            ),
            vec![row(1), row(2)]
        );
        assert_eq!(
            execute("UNWIND [1, 2, NULL] AS n WITH n WHERE n NOT BETWEEN NULL AND 1 RETURN n"),
            vec![row(2)]
        );
    }

    #[test]
    fn text_predicates_compose_with_functions_case_and_boolean_filters() {
        let mut rows = execute(
            "UNWIND ['Alpha', 'beta', 'alphabet', NULL] AS word WITH word \
             WHERE LOWER(word) STARTS WITH 'al' AND word CONTAINS 'ph' \
             OR word ENDS WITH 'ta' RETURN word",
        );
        let mut expected: Vec<_> = ["Alpha", "alphabet", "beta"]
            .into_iter()
            .map(|value| {
                GraphValueRow::from_owned_values(vec![GraphValue::Scalar(
                    CanonicalScalar::ucs_basic_text(value).unwrap(),
                )])
            })
            .collect();
        rows.sort();
        expected.sort();
        assert_eq!(rows, expected);
        assert_eq!(
            execute(
                "UNWIND ['Alpha', 'beta', NULL] AS word WITH word \
                 WHERE (CASE WHEN word IS NULL THEN '' ELSE LOWER(word) END) \
                 STARTS WITH 'al' RETURN word"
            )
            .len(),
            1
        );
        assert_eq!(
            execute(
                "UNWIND ['éclair', 'plain'] AS word WITH word \
                 WHERE word STARTS WITH 'é' AND word CONTAINS 'cl' RETURN word"
            )
            .len(),
            1
        );
    }

    #[test]
    fn text_parameters_are_inferred_once_and_never_reinterpret_numeric_uses() {
        use fgdb_types::CanonicalScalarKind;
        for condition in ["word STARTS WITH $prefix", "$prefix STARTS WITH $prefix"] {
            let statement =
                format!("UNWIND ['alpha'] AS word WITH word WHERE {condition} RETURN word");
            let prepared = PreparedGraphSetText::prepare(&statement, |_, _| None).unwrap();
            let schema = prepared.parameter_schema();
            assert_eq!(schema.len(), 1);
            assert_eq!(
                schema[0].parameter_type,
                GqlParameterType::Scalar(CanonicalScalarKind::Text)
            );
            let mut arguments = GqlParameters::new();
            arguments
                .insert(
                    "prefix",
                    GqlParameterValue::Scalar(
                        GqlScalarParameter::new(CanonicalScalar::ucs_basic_text("al").unwrap())
                            .unwrap(),
                    ),
                )
                .unwrap();
            let result = prepared
                .bind_parameters(&arguments)
                .unwrap()
                .execute_governed(
                    policy(),
                    |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                    || Ok::<_, usize>(()),
                )
                .unwrap();
            assert_eq!(result.value.len(), 1);
            assert!(
                prepared
                    .bind_parameters(&GqlParameters::new().with_int64("prefix", 1).unwrap())
                    .is_err()
            );
        }
        let mut parser = Parser::new("$prefix STARTS WITH 'a'").unwrap();
        parser
            .parameter_types
            .insert("prefix".into(), GqlParameterType::Int64);
        let mut depth = 2;
        assert!(
            parser
                .row_selection(&[], &mut Vec::new(), &mut depth, 0)
                .is_err()
        );
        assert!(
            PreparedGraphSetText::prepare(
                "UNWIND [1] AS n WITH n WHERE n = $value AND 'a' STARTS WITH $value RETURN n",
                |_, _| None,
            )
            .is_err()
        );
    }

    #[test]
    fn empty_membership_does_not_hide_expression_errors_or_numeric_text_misuse() {
        for operator in ["IN", "NOT IN"] {
            let query = prepare(&format!(
                "UNWIND [0] AS n WITH n WHERE 10 / n {operator} [] RETURN n LIMIT 0"
            ));
            assert!(matches!(
                query.execute_governed(
                    policy(),
                    |_, _| Err::<_, GqlQueryError<usize, usize>>(GqlQueryError::Source(1)),
                    || Ok::<_, usize>(()),
                ),
                Err(GqlQueryError::Source(
                    crate::GraphSetExecutionError::Projection { .. }
                ))
            ));
        }
        let mut calls = 0;
        let result = PreparedGraphSetText::prepare(
            "MATCH (n) WITH n WHERE 1 STARTS WITH 'a' RETURN n",
            |_, _| {
                calls += 1;
                None
            },
        );
        assert!(result.is_err());
        assert_eq!(calls, 0);
    }

    #[test]
    fn membership_predicate_limit_is_enforced_without_widening_the_ir() {
        let members = vec!["1"; crate::algebra::MAX_PATTERN_PREDICATES].join(",");
        let allowed = format!("UNWIND [1] AS n WITH n WHERE n IN [{members}] RETURN n");
        assert_eq!(execute(&allowed), vec![row(1)]);
        let refused = format!("UNWIND [1] AS n WITH n WHERE n IN [{members},1] RETURN n");
        assert!(matches!(
            PreparedGraphSetText::prepare(&refused, |_, _| None)
                .unwrap_err()
                .kind,
            GraphSetTextErrorKind::FilterBuild(GraphSetFilterError::TooManyPredicates { .. })
        ));
    }
}
