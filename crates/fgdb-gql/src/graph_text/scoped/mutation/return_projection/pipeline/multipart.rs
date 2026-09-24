//! Parse a complete multi-part read with one lexer and parameter table.
//! WITH is a real relational boundary. A later MATCH becomes a new governed
//! graph leaf joined BEFORE its projection/filter/page, never an AST evaluator.

use super::*;
use crate::row_join::RowJoinSpec;
use crate::set_text::TextToken;
use crate::set_text::multipart::{BoundContinuation, BoundReadInput, has_continuation};

struct UnresolvedContinuation<'a> {
    input: UnresolvedGraphText<'a>,
    join: RowJoinSpec,
}

pub(crate) struct UnresolvedReadInput<'a> {
    first: UnresolvedGraphText<'a>,
    continuations: Vec<UnresolvedContinuation<'a>>,
}

impl UnresolvedReadInput<'_> {
    pub(crate) fn column_schema(&self) -> (Vec<String>, Vec<GraphSetColumnType>) {
        self.continuations
            .last()
            .map_or(&self.first, |part| &part.input)
            .column_schema()
    }

    pub(crate) fn depth(&self) -> usize {
        // Each continuation contributes one Join plus its post-join stages.
        // Its hidden source is a depth-one graph leaf, not a second pipeline.
        self.first.depth()
            + self.continuations.iter().map(|part| {
                1 + usize::from(part.input.projection.is_some())
                    + part.input.pipeline.iter().filter(|stage| {
                        !matches!(stage, ReadStageTemplate::Page { .. })
                    }).count()
            }).sum::<usize>()
    }

    pub(crate) fn operand_count(&self) -> usize {
        usize::from(!self.first.singleton) + self.continuations.len()
    }

    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.first.parameter_schema()
    }

    pub(crate) fn parameter_offsets(&self) -> &[usize] {
        self.first.parameter_offsets()
    }

    pub(crate) fn resolve(
        self,
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<BoundReadInput, GraphPatternTextError> {
        let first = self.first.resolve(&mut resolve)?;
        let mut continuations = Vec::new();
        for part in self.continuations {
            continuations.push(BoundContinuation {
                input: part.input.resolve(&mut resolve)?,
                join: part.join,
            });
        }
        Ok(BoundReadInput { first, continuations })
    }
}

impl PreparedGraphText {
    pub(crate) fn unresolved_read_input<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        tokens: &[TextToken<'_>],
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        if !has_continuation(tokens) {
            return Ok(UnresolvedReadInput {
                first: Self::unresolved_for_composition(statement, declarations)?,
                continuations: Vec::new(),
            });
        }
        Parser::new_with_parameter_types(statement, declarations)?.multipart_read(statement)
    }
}

fn projection_type(input: &Projection<'_>) -> GraphSetColumnType {
    if input.property.is_some() {
        return GraphSetColumnType::Scalar;
    }
    match input.path {
        None => GraphSetColumnType::Vertex,
        Some(GraphPathFunction::Value) => GraphSetColumnType::Path,
        Some(GraphPathFunction::Length | GraphPathFunction::Type) => GraphSetColumnType::Scalar,
        Some(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
        Some(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
        Some(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
        Some(GraphPathFunction::Labels) => GraphSetColumnType::List,
    }
}

// Validate expression-column domains before the first catalog callback. These
// typed NULL/empty-list witnesses only compile shape; no expression executes.
fn admit_head(
    head: &GraphProjectionHead<'_>,
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
) -> Result<(), GraphSetTextError> {
    let values: Vec<_> = parameters.iter().map(|spec| match spec.parameter_type {
        GqlParameterType::Int64 => GqlParameterValue::Int64(0),
        GqlParameterType::UInt64 => GqlParameterValue::UInt64(0),
        GqlParameterType::Scalar(_) => GqlParameterValue::Scalar(
            GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
        ),
        GqlParameterType::List => GqlParameterValue::List(
            crate::GqlListParameter::new(Vec::new()).expect("bounded empty list"),
        ),
    }).collect();
    for (column, (name, value)) in head.outputs.iter().enumerate() {
        let value = super::super::bind_read_value(value, &values)?;
        GraphSetProjection::admit_output(&value, types, column).map_err(|kind| {
            GraphSetTextError { offset: name.at, kind: GraphSetTextErrorKind::ProjectionBuild(kind) }
        })?;
    }
    Ok(())
}

fn join_spec(
    incoming: &RowSchema<'_>,
    inputs: &[Projection<'_>],
    keys: &[(usize, usize)],
    at: usize,
) -> Result<RowJoinSpec, GraphSetTextError> {
    let left: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
    let right: Vec<_> = inputs.iter().map(projection_type).collect();
    let typed_keys = !keys.is_empty() && keys.iter().all(|&(l, r)| {
        left[l] == right[r]
            && matches!(left[l], GraphSetColumnType::Scalar | GraphSetColumnType::Vertex)
    });
    let spec = if typed_keys {
        RowJoinSpec::new(&left, &right, keys)
    } else {
        RowJoinSpec::cross(&left, &right).and_then(|spec| {
            if keys.is_empty() {
                return Ok(spec);
            }
            // UNWIND has the Any domain. The checked ordinary comparison
            // predicate handles it without coercion or a guessed scalar type.
            let mut code = Vec::new();
            for &(l, r) in keys {
                code.push(GraphSetPredicateOp::Compare {
                    left: GraphSetOperand::Column(l),
                    comparison: IntegerComparison::Equal,
                    right: GraphSetOperand::Column(left.len() + r),
                });
                if code.len() > 1 {
                    code.push(GraphSetPredicateOp::And);
                }
            }
            spec.with_predicate(&code)
        })
    };
    spec.map_err(|_| expected(at, "bounded, type-compatible MATCH correlations"))
}

impl<'a> Parser<'a> {
    fn multipart_read(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        let mut incoming = Vec::new();
        let first = if self.is_word("MATCH") {
            let (input, next, terminal, _) = self.multipart_graph_part(statement, &incoming)?;
            if terminal {
                return Err(expected(self.current.at, "WITH before the next MATCH"));
            }
            incoming = next;
            input
        } else {
            // A values-only leading pipeline is a unit relation followed by
            // the existing row stages, not a manufactured graph carrier.
            let (pipeline, next, _) = self.row_pipeline_prefix(Vec::new())?;
            incoming = next;
            if !self.is_word("MATCH") {
                return Err(expected(self.current.at, "MATCH after the leading row stages"));
            }
            let syntax = self.take_part_syntax()?;
            UnresolvedGraphText {
                statement, syntax, projection: None, pipeline, singleton: true,
                leading: Vec::new(), leading_types: Vec::new(), correlations: Vec::new(),
            }
        };
        let mut result = UnresolvedReadInput { first, continuations: Vec::new() };
        loop {
            if !self.is_word("MATCH") {
                return Err(expected(self.current.at, "required MATCH after WITH"));
            }
            let (input, next, terminal, join) = self.multipart_graph_part(statement, &incoming)?;
            result.continuations.push(UnresolvedContinuation {
                input,
                join: join.expect("a continuation has an incoming relation"),
            });
            if result.depth() > crate::MAX_GRAPH_SET_DEPTH {
                return Err(GraphSetTextError {
                    offset: self.current.at,
                    kind: GraphSetTextErrorKind::SetBuild(crate::GraphSetBuildError::TooDeep {
                        limit: crate::MAX_GRAPH_SET_DEPTH,
                        observed: result.depth(),
                    }),
                });
            }
            if result.operand_count() > crate::MAX_GRAPH_SET_OPERANDS {
                return Err(expected(self.current.at, "bounded graph-source count"));
            }
            incoming = next;
            if terminal {
                self.end()?;
                break;
            }
        }
        // All parts used ONE global argument-index table. Later declarations
        // and occurrences are now frozen into every source without reparsing.
        let parameters = self.syntax.parameters;
        let offsets = self.syntax.parameter_offsets;
        result.first.syntax.parameters.clone_from(&parameters);
        result.first.syntax.parameter_offsets.clone_from(&offsets);
        for part in &mut result.continuations {
            part.input.syntax.parameters.clone_from(&parameters);
            part.input.syntax.parameter_offsets.clone_from(&offsets);
        }
        Ok(result)
    }

    fn take_part_syntax(&mut self) -> Result<Syntax<'a>, GraphPatternTextError> {
        // Reset only graph-local bindings. The live lexer, declaration map and
        // definition-wide predicate/edge/identity counters are never reset.
        let mut next: Syntax<'a> = Parser::new("")?.syntax;
        next.parameters.clone_from(&self.syntax.parameters);
        next.parameter_offsets.clone_from(&self.syntax.parameter_offsets);
        Ok(core::mem::replace(&mut self.syntax, next))
    }

    #[allow(clippy::type_complexity)]
    fn multipart_graph_part(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
    ) -> Result<(UnresolvedGraphText<'a>, RowSchema<'a>, bool, Option<RowJoinSpec>), GraphSetTextError> {
        let at = self.current.at;
        self.read_row_bindings = incoming.iter().map(|(name, _)| *name).collect();
        self.parse_match_prefix()?;
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        for (left, &(name, kind)) in incoming.iter().enumerate() {
            // An imported name cannot silently become an independent local
            // inside EXISTS/OPTIONAL. This subset correlates root vertices;
            // the ordinary scoped compiler can then capture that root value.
            let in_root = self.syntax.variables[..self.syntax.root_variables]
                .iter().any(|variable| variable.text == name.text);
            if !in_root && self.syntax.scopes.iter().any(|scope| {
                scope.body.variables.iter().chain(&scope.body.captures)
                    .any(|variable| variable.text == name.text)
            }) {
                return Err(expected(name.at, "imported vertices in the root MATCH"));
            }
            if let Some(right) = self.syntax.variables.iter().position(|var| var.text == name.text) {
                if right >= self.syntax.root_variables {
                    return Err(expected(name.at, "imported vertices in the root MATCH"));
                }
                if !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Any) {
                    return Err(expected(name.at, "a vertex-valued imported binding"));
                }
                let right = self.mutation_projection(&mut inputs, self.syntax.variables[right], None)?;
                keys.push((left, right));
            }
            if self.syntax.path.is_some_and(|path| path.text == name.text)
                || self.syntax.edges.iter().any(|edge| {
                    edge.variable.is_some_and(|edge| edge.text == name.text)
                })
            {
                return Err(expected(name.at, "new path and relationship bindings after WITH"));
            }
        }
        // The current property-map correlation record has no scope tag. Do
        // not accidentally move an OPTIONAL/EXISTS predicate onto the outer
        // required join. Root-only maps remain supported without ambiguity.
        if !self.read_correlations.is_empty() && !self.syntax.scopes.is_empty() {
            return Err(expected(at, "root-only imported property-map correlations"));
        }
        for (variable, key, left) in core::mem::take(&mut self.read_correlations) {
            let right = self.mutation_projection(&mut inputs, variable, Some(key))?;
            if !keys.contains(&(left, right)) {
                keys.push((left, right));
            }
        }
        let mut head = if incoming.is_empty() {
            self.graph_projection_head()?
        } else {
            self.multipart_head(incoming, inputs)?
        };
        if head.inputs.is_empty() {
            // A constant projection still emits once per graph occurrence.
            self.mutation_projection(&mut head.inputs, self.syntax.variables[0], None)?;
        }
        let mut types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
        types.extend(head.inputs.iter().map(projection_type));
        admit_head(&head, &types, &self.syntax.parameters)?;
        let schema: RowSchema<'a> = head.outputs.iter().map(|(name, value)| {
            (*name, value.column_type(&types, &self.syntax.parameters))
        }).collect();
        let join = if incoming.is_empty() { None } else {
            Some(join_spec(incoming, &head.inputs, &keys, at)?)
        };
        let mut terminal = !head.with;
        let (mut pipeline, mut next) = if head.with {
            let (pipeline, schema, _) = self.row_pipeline_prefix(schema)?;
            (pipeline, schema)
        } else {
            (Vec::new(), schema)
        };
        if !terminal && self.is_word("RETURN") {
            let at = self.current.at;
            self.advance()?;
            let distinct = self.take_word("DISTINCT")?;
            if !distinct { self.take_word("ALL")?; }
            let (projection, schema) = self.row_projection(&next)?;
            pipeline.push(ReadStageTemplate::Project {
                at, projection,
                quantifier: if distinct { GraphSetQuantifier::Distinct } else { GraphSetQuantifier::All },
            });
            next = schema;
            terminal = true;
        }
        // The surrounding set parser owns terminal ORDER BY/SKIP/LIMIT. Every
        // page before a continuation was already consumed by row_pipeline_prefix.
        self.syntax.columns = head.inputs.into_iter().map(|input| Column {
            variable: input.variable, property: input.property, path: input.path,
            alias: input.variable,
        }).collect();
        let projection = Some(head.outputs.into_iter().map(|(name, value)| {
            ReadProjectionTemplate { name: name.text.to_owned(), value }
        }).collect());
        let syntax = self.take_part_syntax()?;
        Ok((UnresolvedGraphText {
            statement, syntax, projection, pipeline, singleton: false,
            leading: Vec::new(), leading_types: incoming.iter().map(|(_, kind)| *kind).collect(),
            correlations: keys,
        }, next, terminal, join))
    }

    fn multipart_head(
        &mut self,
        incoming: &RowSchema<'a>,
        mut inputs: Vec<Projection<'a>>,
    ) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        let with = self.take_word("WITH")?;
        if !with { self.word("RETURN")?; }
        self.syntax.distinct = self.take_word("DISTINCT")?;
        if !self.syntax.distinct { self.take_word("ALL")?; }
        let width = incoming.len();
        let mut outputs = Vec::new();
        if self.take(b'*')? {
            for (index, &(name, _)) in incoming.iter().enumerate() {
                outputs.push((name, ReadValueTemplate::Column(index)));
            }
            let visible: Vec<_> = self.visible_graph_bindings().collect();
            for variable in visible {
                if incoming.iter().any(|(name, _)| name.text == variable.text) { continue; }
                self.capacity(outputs.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                let column = self.mutation_projection(&mut inputs, variable, None)?;
                outputs.push((variable, ReadValueTemplate::Column(width + column)));
            }
        } else {
            loop {
                self.capacity(outputs.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
                let at = self.current.at;
                let value = self.read_resolved_value(&mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else { return Ok(None); };
                    let next = parser.lexer.clone().next()?;
                    if matches!(next.kind, TokenKind::Punct(b'(')) {
                        if ["labels", "type", "length", "path_length", "nodes", "edges", "relationships"]
                            .iter().any(|name| word.eq_ignore_ascii_case(name))
                        {
                            let Operand::Column(column) = parser.mutation_operand(&mut inputs)? else {
                                unreachable!("graph function resolves a source column");
                            };
                            return Ok(Some(width + column));
                        }
                        return Ok(None);
                    }
                    if !matches!(next.kind, TokenKind::Punct(b'.'))
                        && let Some(column) = incoming.iter().position(|(name, _)| name.text == word)
                    {
                        parser.advance()?;
                        return Ok(Some(column));
                    }
                    let graph = parser.visible_graph_bindings().any(|name| name.text == word);
                    if !graph { return Ok(None); }
                    let variable = parser.any_variable()?;
                    let property = if parser.take(b'.')? {
                        parser.require_property_variable(variable)?;
                        Some(parser.name()?)
                    } else { None };
                    let column = parser.mutation_projection(&mut inputs, variable, property)?;
                    Ok(Some(width + column))
                }, 0)?;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if let ReadValueTemplate::Column(column) = &value {
                    if *column < width { incoming[*column].0 } else {
                        let input = inputs[*column - width];
                        input.property.unwrap_or(input.variable)
                    }
                } else { return Err(expected(at, "AS alias for a computed row value")); };
                if outputs.iter().any(|(name, _): &(Name<'a>, ReadValueTemplate)| name.text == alias.text) {
                    return Err(error(alias.at, GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection)).into());
                }
                outputs.push((alias, value));
                if !self.take(b',')? { break; }
            }
        }
        Ok(GraphProjectionHead { with, inputs, outputs })
    }
}

impl BoundReadInput {
    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        if self.continuations.is_empty() { return self.first.bind_parameters(arguments); }
        let values = self.first.checked_arguments(arguments)?;
        let mut input = self.first.bind_values(&values)?;
        for part in &self.continuations {
            let source = &part.input;
            let right = source.selection.as_ref().expect("continuations are graph sources")
                .bind_values(&values)?;
            input = input.join(right.into(), part.join.clone()).map_err(|kind| {
                GraphSetTextError { offset: source.return_at, kind: GraphSetTextErrorKind::SetBuild(kind) }
            })?;
            if let Some(projection) = &source.projection {
                input = super::super::bind_projection(input, projection, source.quantifier, &values, source.return_at)?;
            }
            input = super::super::bind_stages(input, &source.pipeline, &values)?;
        }
        Ok(input)
    }
}
