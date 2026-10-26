//! Parse a complete multi-part read with one lexer and parameter table.
//! WITH is a real relational boundary. A later MATCH becomes a new governed
//! graph leaf joined BEFORE its projection/filter/page, never an AST evaluator.
//! OPTIONAL continuations use the same left-join kernel: correlation is part
//! of matching, and only new bindings are null-extended. Carried values stay
//! on the left, including property reads through their pinned identities.

use super::*;
use crate::row_join::{RowJoinKind, RowJoinSpec};
use crate::set_text::TextToken;
use crate::set_text::multipart::{
    BoundContinuation, BoundContinuationOperation, BoundReadInput, has_continuation,
};

struct UnresolvedContinuation<'a> {
    input: UnresolvedGraphText<'a>,
    operation: UnresolvedContinuationOperation<'a>,
}

enum UnresolvedContinuationOperation<'a> {
    Match(RowJoinSpec),
    Apply {
        body: Box<UnresolvedReadInput<'a>>,
        imports: Vec<usize>,
    },
}

pub(crate) struct UnresolvedReadInput<'a> {
    first: UnresolvedGraphText<'a>,
    continuations: Vec<UnresolvedContinuation<'a>>,
}

impl UnresolvedReadInput<'_> {
    pub(in crate::graph_text) fn from_first(
        first: UnresolvedGraphText<'_>,
    ) -> UnresolvedReadInput<'_> {
        UnresolvedReadInput {
            first,
            continuations: Vec::new(),
        }
    }

    pub(in crate::graph_text) fn append_stages(&mut self, stages: Vec<ReadStageTemplate>) {
        self.continuations
            .last_mut()
            .map_or(&mut self.first, |part| &mut part.input)
            .pipeline
            .extend(stages);
    }

    pub(in crate::graph_text) fn share_parameters(
        &mut self,
        parameters: &[GqlParameterSpec],
        offsets: &[usize],
    ) {
        self.first.syntax.parameters = parameters.to_vec();
        self.first.syntax.parameter_offsets = offsets.to_vec();
        for part in &mut self.continuations {
            part.input.syntax.parameters = parameters.to_vec();
            part.input.syntax.parameter_offsets = offsets.to_vec();
            if let UnresolvedContinuationOperation::Apply { body, .. } = &mut part.operation {
                body.share_parameters(parameters, offsets);
            }
        }
    }
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
            + self
                .continuations
                .iter()
                .map(|part| {
                    let source_depth = match &part.operation {
                        UnresolvedContinuationOperation::Match(_) => 1,
                        UnresolvedContinuationOperation::Apply { body, .. } => 1 + body.depth(),
                    };
                    source_depth
                        + usize::from(part.input.projection.is_some())
                        + 2 * usize::from(!part.input.row_fields.is_empty())
                        + part
                            .input
                            .pipeline
                            .iter()
                            .map(ReadStageTemplate::depth)
                            .sum::<usize>()
                })
                .sum::<usize>()
    }

    pub(crate) fn operand_count(&self) -> usize {
        usize::from(!self.first.singleton)
            + self
                .continuations
                .iter()
                .map(|part| match &part.operation {
                    UnresolvedContinuationOperation::Match(_) => 1,
                    UnresolvedContinuationOperation::Apply { body, .. } => body.operand_count(),
                })
                .sum::<usize>()
    }

    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.first.parameter_schema()
    }

    pub(crate) fn parameter_offsets(&self) -> &[usize] {
        self.first.parameter_offsets()
    }

    /// Every part reads the lender's reverse catalog for `labels(n)` and
    /// `type(r)`, not only the names its own text mentions.
    pub(crate) fn resolve<R: crate::graph_text::GraphSymbolResolver + ?Sized>(
        self,
        resolve: &mut R,
    ) -> Result<BoundReadInput, GraphPatternTextError> {
        let first = self
            .first
            .resolve(crate::graph_text::LentResolver(resolve))?;
        let mut continuations = Vec::new();
        for part in self.continuations {
            continuations.push(BoundContinuation {
                input: part
                    .input
                    .resolve(crate::graph_text::LentResolver(resolve))?,
                operation: match part.operation {
                    UnresolvedContinuationOperation::Match(join) => {
                        BoundContinuationOperation::Match(join)
                    }
                    UnresolvedContinuationOperation::Apply { body, imports } => {
                        BoundContinuationOperation::Apply {
                            body: Box::new(body.resolve(resolve)?),
                            imports,
                        }
                    }
                },
            });
        }
        Ok(BoundReadInput {
            first,
            continuations,
        })
    }
}

impl PreparedGraphText {
    pub(crate) fn unresolved_read_input<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        tokens: &[TextToken<'_>],
        zones: Option<&crate::GqlTimeZones>,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        let multipart = has_continuation(tokens)
            || Parser::deferred_where_continues(statement, declarations, tokens, zones);
        let read = |deferred: &[usize],
                    implicit: &[usize]|
         -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
            if !multipart && implicit.is_empty() {
                return Ok(UnresolvedReadInput {
                    first: Self::unresolved_for_composition(
                        statement,
                        declarations,
                        zones,
                        deferred,
                    )?,
                    continuations: Vec::new(),
                });
            }
            let mut parser = Parser::new_with_parameter_types(statement, declarations)?
                .with_zones(zones.cloned());
            parser.deferred_wheres = deferred.to_vec();
            parser.implicit_withs = implicit.to_vec();
            parser.read_elements = true;
            parser.multipart_read(statement)
        };
        let error = match read(&[], &[]) {
            Err(error) => error,
            read => return read,
        };
        // A required MATCH's WHERE over imported rows that the graph predicate
        // refused, such as a computed graph operand against an imported
        // column (`WITH 0 AS e MATCH (n) WHERE toInteger(n.id) = e`, or the
        // same after a leading UNWIND), means what `MATCH ... WITH * WHERE ...`
        // means: the same rows, filtered after the join. So does a required
        // MATCH's WHERE whose existence predicate the graph predicate cannot
        // place (`MATCH (n) WHERE (n)-[:A]-() OR (n)-[:B]-()`): the row filter
        // reads each one from a hidden column (hoist_existence_reads). And a
        // later MATCH clause that reads a name its child scope cannot see, an earlier
        // clause's relationship (`MATCH (a)-[r]->(b) MATCH (b)-[s]->(c)
        // WHERE r.w < s.w`) or an imported column, means what it means after
        // an explicit `WITH *`: openCypher keeps relationship uniqueness per
        // clause, so the boundary changes no row. Re-read one refusal at a
        // time. A statement still refused reports its first error.
        let (mut deferred, mut implicit) = (Vec::new(), Vec::new());
        let mut offset = error.offset;
        loop {
            if let Some(at) = crate::set_text::multipart::continuation_where_at(tokens, offset)
                .or_else(|| crate::set_text::multipart::existence_where_at(tokens, offset))
                .filter(|at| !deferred.contains(at))
            {
                deferred.push(at);
            } else if let Some(at) = crate::set_text::multipart::implicit_with_at(tokens, offset)
                .filter(|at| !implicit.contains(at))
            {
                implicit.push(at);
            } else {
                break;
            }
            match read(&deferred, &implicit) {
                Ok(read) => return Ok(read),
                Err(next) => offset = next.offset,
            }
        }
        Err(error)
    }
}

// Validate expression-column domains before the first catalog callback. These
// typed NULL/empty-list witnesses only compile shape; no expression executes.
fn admit_head(
    head: &GraphProjectionHead<'_>,
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
) -> Result<(), GraphSetTextError> {
    admit_values(
        head.outputs.iter().map(|(name, value)| (name.at, value)),
        types,
        parameters,
    )
}

fn admit_values<'a>(
    outputs: impl IntoIterator<Item = (usize, &'a ReadValueTemplate)>,
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
) -> Result<(), GraphSetTextError> {
    let values: Vec<_> = parameters
        .iter()
        .map(|spec| match spec.parameter_type {
            GqlParameterType::Int64 => GqlParameterValue::Int64(0),
            GqlParameterType::UInt64 => GqlParameterValue::UInt64(0),
            GqlParameterType::Scalar(_) => GqlParameterValue::Scalar(
                GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
            ),
            GqlParameterType::List => GqlParameterValue::List(
                crate::GqlListParameter::new(Vec::new()).expect("bounded empty list"),
            ),
            GqlParameterType::Map => GqlParameterValue::Map(
                crate::GqlMapParameter::new(Vec::new()).expect("bounded empty map"),
            ),
        })
        .collect();
    for (column, (at, value)) in outputs.into_iter().enumerate() {
        let value = super::super::bind_read_value(value, &values)?;
        GraphSetProjection::admit_output(&value, types, column).map_err(|kind| {
            GraphSetTextError {
                offset: at,
                kind: GraphSetTextErrorKind::ProjectionBuild(kind),
            }
        })?;
    }
    Ok(())
}

fn join_spec(
    left: &[GraphSetColumnType],
    inputs: &[Projection<'_>],
    keys: &[(usize, usize)],
    comparisons: &[(usize, usize, IntegerComparison)],
    at: usize,
) -> Result<RowJoinSpec, GraphSetTextError> {
    let right: Vec<_> = inputs.iter().map(projection_type).collect();
    let typed_keys = !keys.is_empty()
        && keys.iter().all(|&(l, r)| {
            left[l] == right[r]
                && (left[l].is_scalar_like()
                    || matches!(
                        left[l],
                        GraphSetColumnType::Vertex | GraphSetColumnType::Edge
                    ))
        });
    let spec = if typed_keys {
        RowJoinSpec::new(left, &right, keys)
    } else {
        RowJoinSpec::cross(left, &right).and_then(|spec| {
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
    let spec = spec.map_err(|_| expected(at, "bounded, type-compatible MATCH correlations"))?;
    if comparisons.is_empty() {
        return Ok(spec);
    }
    // Keep typed equality keys selective. An Any-domain equality may already
    // be ON bytecode; append to it, never overwrite it with the theta test.
    let mut code = spec.predicate().unwrap_or_default().to_vec();
    for &(l, r, comparison) in comparisons {
        let conjunction = !code.is_empty();
        code.push(GraphSetPredicateOp::Compare {
            left: GraphSetOperand::Column(l),
            comparison,
            right: GraphSetOperand::Column(left.len() + r),
        });
        if conjunction {
            code.push(GraphSetPredicateOp::And);
        }
    }
    spec.with_predicate(&code)
        .map_err(|_| expected(at, "bounded, type-compatible MATCH comparisons"))
}

impl<'a> Parser<'a> {
    /// A separable ON operand over the incoming row uses the existing private
    /// input projection. In particular it must not re-match a carried vertex:
    /// that vertex can be NULL, and only the new bindings are null-extended.
    pub(in crate::graph_text) fn computed_imported_operand(
        &mut self,
    ) -> Result<usize, GraphPatternTextError> {
        let at = self.current.at;
        let schema: Vec<_> = self
            .read_row_bindings
            .iter()
            .map(|name| (*name, GraphSetColumnType::Any))
            .collect();
        let (operand, slots) = self
            .row_operand_with_lets(&schema)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        let value = self
            .scalar_let_value(operand, slots, at)
            .map_err(|source| error(source.offset, GraphPatternTextErrorKind::BooleanExpression))?;
        if let ReadValueTemplate::Column(column) = value {
            return Ok(column);
        }
        let column = self.read_row_bindings.len() + self.read_row_fields.len();
        self.capacity(
            column,
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        self.read_row_fields.push(value);
        Ok(column)
    }

    fn multipart_read(
        mut self,
        statement: &'a str,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        let (mut result, _) = self.multipart_parts(statement, false)?;
        self.end()?;
        result.share_parameters(&self.syntax.parameters, &self.syntax.parameter_offsets);
        Ok(result)
    }

    // Leave RETURN to the exact aggregate compiler, with the same live lexer,
    // argument indices and statement-wide counters. No synthetic query text or
    // second MATCH parser is introduced. The tuple crosses only this private
    // module boundary; the aggregate owner immediately restores a typed head.
    #[allow(clippy::type_complexity)]
    pub(in crate::graph_text) fn multipart_aggregate_prefix(
        &mut self,
        statement: &'a str,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>, usize), GraphSetTextError> {
        let (result, schema) = self.multipart_parts(statement, true)?;
        let depth = result.depth();
        Ok((result, schema, depth))
    }

    fn multipart_parts(
        &mut self,
        statement: &'a str,
        aggregate: bool,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        self.multipart_parts_from(statement, aggregate, Vec::new(), 0)
    }

    /// A terminal read after a write uses exactly the ordinary multipart
    /// joins, scopes and projections. Its first relation is supplied by the
    /// write host, not a unit row or an Apply argument frame. RETURN shares
    /// the row-domain aggregate owner used by the other write families.
    pub(in crate::graph_text::scoped::mutation) fn write_read_continuation(
        mut self,
        statement: &'a str,
        incoming: RowSchema<'a>,
    ) -> Result<UnresolvedReadInput<'a>, GraphSetTextError> {
        self.read_elements = true;
        self.call_element_reads = true;
        let (mut result, schema) =
            self.multipart_parts_with_input(statement, true, incoming, 0, true)?;
        let stages = self.write_row_pipeline(schema)?;
        result.append_stages(stages);
        // The graph part has already lent its syntax. Terminal row expressions
        // add property slots afterwards; resolve them with that final part so
        // carried and optional element reads bind once with the template.
        result
            .continuations
            .last_mut()
            .map_or(&mut result.first, |part| &mut part.input)
            .syntax
            .element_keys
            .append(&mut self.syntax.element_keys);
        self.end()?;
        result.share_parameters(&self.syntax.parameters, &self.syntax.parameter_offsets);
        self.admit_multipart_size(&result)?;
        Ok(result)
    }

    fn multipart_parts_from(
        &mut self,
        statement: &'a str,
        aggregate: bool,
        incoming: RowSchema<'a>,
        call_depth: usize,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        self.multipart_parts_with_input(statement, aggregate, incoming, call_depth, false)
    }

    fn multipart_parts_with_input(
        &mut self,
        statement: &'a str,
        aggregate: bool,
        mut incoming: RowSchema<'a>,
        call_depth: usize,
        owned: bool,
    ) -> Result<(UnresolvedReadInput<'a>, RowSchema<'a>), GraphSetTextError> {
        self.refuse_call_write(call_depth)?;
        let mut terminal = false;
        let first = if self.is_word("MATCH") && incoming.is_empty() && !owned {
            let (input, next, terminal, _) =
                self.multipart_graph_part(statement, &incoming, None, aggregate, call_depth)?;
            if terminal {
                return Ok((UnresolvedReadInput::from_first(input), next));
            }
            incoming = next;
            input
        } else {
            // A values-only leading pipeline is a unit relation followed by
            // the existing row stages, not a manufactured graph carrier.
            let arguments =
                (!owned && !incoming.is_empty()).then(|| ReadStageTemplate::Arguments {
                    columns: incoming
                        .iter()
                        .map(|(name, _)| name.text.to_owned())
                        .collect(),
                    types: incoming.iter().map(|(_, kind)| *kind).collect(),
                });
            let (mut pipeline, next, done) = self.multipart_row_tail(incoming, aggregate)?;
            self.refuse_call_write(call_depth)?;
            if let Some(arguments) = arguments {
                pipeline.insert(0, arguments);
            }
            incoming = next;
            terminal = done;
            if !terminal
                && !(self.is_word("MATCH") || self.is_word("OPTIONAL") || self.inline_call()?)
            {
                return Err(expected(
                    self.current.at,
                    "MATCH, OPTIONAL MATCH or inline CALL after row stages",
                ));
            }
            let syntax = self.take_part_syntax()?;
            UnresolvedGraphText {
                statement,
                syntax,
                projection: None,
                pipeline,
                singleton: true,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
                row_fields: Vec::new(),
            }
        };
        let mut result = UnresolvedReadInput {
            first,
            continuations: Vec::new(),
        };
        while !terminal {
            self.refuse_call_write(call_depth)?;
            if self.inline_call()? {
                let (part, next, done) =
                    self.multipart_call_part(statement, &incoming, aggregate, call_depth)?;
                result.continuations.push(part);
                incoming = next;
                terminal = done;
            } else {
                let kind = if self.take_word("OPTIONAL")? {
                    RowJoinKind::Left
                } else {
                    RowJoinKind::Inner
                };
                if !self.is_word("MATCH") {
                    return Err(expected(self.current.at, "MATCH after WITH or OPTIONAL"));
                }
                let (input, next, done, join) = self.multipart_graph_part(
                    statement,
                    &incoming,
                    Some(kind),
                    aggregate,
                    call_depth,
                )?;
                result.continuations.push(UnresolvedContinuation {
                    input,
                    operation: UnresolvedContinuationOperation::Match(
                        join.expect("a continuation has an incoming relation"),
                    ),
                });
                incoming = next;
                terminal = done;
            }
            self.admit_multipart_size(&result)?;
        }
        Ok((result, incoming))
    }

    fn admit_multipart_size(
        &self,
        result: &UnresolvedReadInput<'_>,
    ) -> Result<(), GraphSetTextError> {
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
        Ok(())
    }

    fn inline_call(&self) -> Result<bool, GraphPatternTextError> {
        Ok(self.is_word("CALL")
            && matches!(
                self.lexer.clone().next()?.kind,
                TokenKind::Punct(b'{' | b'(')
            ))
    }

    fn refuse_call_write(&self, call_depth: usize) -> Result<(), GraphSetTextError> {
        if call_depth != 0
            && [
                "CREATE", "INSERT", "MERGE", "SET", "DELETE", "DETACH", "REMOVE",
            ]
            .iter()
            .any(|clause| self.is_word(clause))
        {
            return Err(GraphSetTextError {
                offset: self.current.at,
                kind: GraphSetTextErrorKind::CallBodyWrite,
            });
        }
        Ok(())
    }

    fn multipart_row_tail(
        &mut self,
        schema: RowSchema<'a>,
        aggregate: bool,
    ) -> Result<(Vec<ReadStageTemplate>, RowSchema<'a>, bool), GraphSetTextError> {
        let (mut stages, mut schema, _) = self.row_pipeline_prefix(schema)?;
        if !self.is_word("RETURN") {
            return Ok((stages, schema, false));
        }
        if !aggregate {
            let at = self.current.at;
            self.advance()?;
            let distinct = self.take_word("DISTINCT")?;
            if !distinct {
                self.take_all_quantifier()?;
            }
            let (mut projection, next) = self.row_projection(&schema, true)?;
            let page = self.return_expression_page(&schema, &mut projection, distinct)?;
            self.boundary_reads = None;
            stages.push(ReadStageTemplate::Project {
                at,
                projection,
                quantifier: if distinct {
                    GraphSetQuantifier::Distinct
                } else {
                    GraphSetQuantifier::All
                },
            });
            stages.extend(page);
            schema = next;
        }
        Ok((stages, schema, true))
    }

    fn multipart_call_part(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<(UnresolvedContinuation<'a>, RowSchema<'a>, bool), GraphSetTextError> {
        let at = self.current.at;
        // Admission precedes recursive descent and catalog access. This is a
        // parser stack bound, in addition to the whole relational depth bound.
        if call_depth >= 8 {
            return Err(expected(at, "at most eight nested inline CALL bodies"));
        }
        self.word("CALL")?;
        let mut imports = Vec::new();
        let mut schema = Vec::new();
        let scoped = self.take(b'(')?;
        if scoped && !self.take(b')')? {
            loop {
                let name = self.name()?;
                let Some(column) = incoming
                    .iter()
                    .position(|(binding, _)| binding.text == name.text)
                else {
                    return Err(expected(name.at, "an existing CALL import"));
                };
                if imports.contains(&column) {
                    return Err(expected(name.at, "unique CALL imports"));
                }
                self.capacity(
                    imports.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                imports.push(column);
                schema.push((name, incoming[column].1));
                if !self.take(b',')? {
                    break;
                }
            }
            self.punct(b')', ")")?;
        }
        self.punct(b'{', "{")?;
        if !scoped && let Some((columns, names)) = self.legacy_call_imports(incoming)? {
            imports = columns;
            schema = names;
        }
        self.read_row_bindings.clear();
        self.boundary_reads = None;
        self.projected_sources = None;
        let previous_reads = core::mem::replace(&mut self.call_element_reads, true);
        let body = self.multipart_parts_from(statement, false, schema, call_depth + 1);
        self.call_element_reads = previous_reads;
        let (body, exports) = body?;
        self.punct(b'}', "} after a read-only CALL body ending in RETURN")?;
        self.read_row_bindings.clear();
        self.boundary_reads = None;
        self.projected_sources = None;
        let mut combined = incoming.clone();
        for &(name, kind) in &exports {
            if combined
                .iter()
                .any(|(binding, _)| binding.text == name.text)
            {
                return Err(expected(
                    name.at,
                    "a CALL export distinct from outer bindings",
                ));
            }
            self.capacity(
                combined.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            combined.push((name, kind));
        }
        let projection = combined
            .iter()
            .enumerate()
            .map(|(column, (name, _))| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(column),
            })
            .collect();
        let types = combined.iter().map(|(_, kind)| *kind).collect();
        // A returning CALL exports real typed identities, not graph slots.
        // Its caller's WHERE/RETURN/aggregate reads them through the same
        // governed element reader as the body; dropped names remain absent.
        self.call_element_reads = true;
        let (pipeline, next, terminal) = self.multipart_row_tail(combined, aggregate)?;
        let mut syntax = self.take_part_syntax()?;
        syntax.return_at = at;
        Ok((
            UnresolvedContinuation {
                input: UnresolvedGraphText {
                    statement,
                    syntax,
                    projection: Some(projection),
                    pipeline,
                    singleton: true,
                    leading: Vec::new(),
                    leading_types: types,
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                },
                operation: UnresolvedContinuationOperation::Apply {
                    body: Box::new(body),
                    imports,
                },
            },
            next,
            terminal,
        ))
    }

    /// The legacy importing WITH accepts only a list of outer names. Inspect
    /// the same lexer without consuming it: the ordinary WITH owner still
    /// builds its projection. Expressions remain ordinary local row stages;
    /// they cannot acquire an unlisted outer binding.
    fn legacy_call_imports(
        &self,
        incoming: &RowSchema<'a>,
    ) -> Result<Option<(Vec<usize>, RowSchema<'a>)>, GraphSetTextError> {
        if !self.is_word("WITH") {
            return Ok(None);
        }
        let mut lexer = self.lexer.clone();
        let mut imports = Vec::new();
        let mut schema = Vec::new();
        loop {
            let token = lexer.next()?;
            let TokenKind::Word(text) = token.kind else {
                return Ok(None);
            };
            let Some(column) = incoming.iter().position(|(name, _)| name.text == text) else {
                return Ok(None);
            };
            if imports.contains(&column) {
                return Err(expected(token.at, "unique CALL imports"));
            }
            self.capacity(
                imports.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            imports.push(column);
            schema.push((Name { text, at: token.at }, incoming[column].1));
            let next = lexer.next()?;
            if matches!(next.kind, TokenKind::Punct(b',')) {
                continue;
            }
            return Ok(matches!(next.kind, TokenKind::Word(word)
                if ["RETURN", "MATCH", "OPTIONAL", "WITH", "UNWIND", "CALL"].iter().any(|clause| word.eq_ignore_ascii_case(clause)))
                .then_some((imports, schema)));
        }
    }

    /// Carried vertices whose properties the part starting at this MATCH
    /// reads (`a.p`) although its own pattern does not bind them, in
    /// first-read order. Predicate reads end at the next top-level WITH,
    /// RETURN or set operation. RETURN reads the carried identity directly;
    /// re-matching it would discard a NULL carried from an earlier OPTIONAL.
    /// A part that names a path, a node binding of the same name
    /// and a comprehension or quantifier local are left to the ordinary
    /// compiler. Nothing is consumed.
    fn carried_property_reads(
        &self,
        incoming: &RowSchema<'a>,
    ) -> Result<Vec<Name<'a>>, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let mut previous = self.current.kind;
        let mut depth = 0_usize;
        let mut reads: Vec<Name<'a>> = Vec::new();
        let mut excluded: Vec<&'a str> = Vec::new();
        loop {
            let token = lexer.next()?;
            let after_dot = matches!(previous, TokenKind::Punct(b'.'));
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => {
                    let Some(outer) = depth.checked_sub(1) else {
                        break;
                    };
                    depth = outer;
                }
                TokenKind::Word(word)
                    if depth == 0
                        && !after_dot
                        && (["RETURN", "UNION", "EXCEPT", "INTERSECT"]
                            .iter()
                            .any(|clause| word.eq_ignore_ascii_case(clause))
                            || (word.eq_ignore_ascii_case("WITH")
                                && !matches!(previous, TokenKind::Word(previous)
                                    if previous.eq_ignore_ascii_case("STARTS")
                                        || previous.eq_ignore_ascii_case("ENDS")))) =>
                {
                    break;
                }
                TokenKind::Word(word) if !after_dot => {
                    let next = lexer.clone().next()?.kind;
                    if depth == 0 && matches!(next, TokenKind::Punct(b'=')) {
                        if matches!(previous, TokenKind::Word(clause)
                            if clause.eq_ignore_ascii_case("MATCH"))
                        {
                            return Ok(Vec::new());
                        }
                    } else if (matches!(previous, TokenKind::Punct(b'('))
                        && matches!(next, TokenKind::Punct(b')' | b':' | b'{')))
                        || (matches!(previous, TokenKind::Punct(b'[' | b'('))
                            && matches!(next, TokenKind::Word(next)
                                if next.eq_ignore_ascii_case("IN")))
                    {
                        // A later part's own binding, `(m)`, `(m:L)` or
                        // `(m {..})`, or a local `[x IN` / `(x IN`.
                        excluded.push(word);
                    } else if matches!(next, TokenKind::Punct(b'.'))
                        && incoming.iter().any(|(name, kind)| {
                            name.text == word && *kind == GraphSetColumnType::Vertex
                        })
                        && reads.iter().all(|read| read.text != word)
                    {
                        reads.push(Name {
                            text: word,
                            at: token.at,
                        });
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
        reads.retain(|read| !excluded.contains(&read.text));
        Ok(reads)
    }

    pub(in crate::graph_text) fn take_part_syntax(
        &mut self,
    ) -> Result<Syntax<'a>, GraphPatternTextError> {
        // Reset only graph-local bindings. The live lexer, declaration map and
        // definition-wide predicate/edge/identity counters are never reset.
        let mut next: Syntax<'a> = Parser::new("")?.syntax;
        next.parameters.clone_from(&self.syntax.parameters);
        next.parameter_offsets
            .clone_from(&self.syntax.parameter_offsets);
        Ok(core::mem::replace(&mut self.syntax, next))
    }

    #[allow(clippy::type_complexity)]
    fn multipart_graph_part(
        &mut self,
        statement: &'a str,
        incoming: &RowSchema<'a>,
        kind: Option<RowJoinKind>,
        aggregate: bool,
        call_depth: usize,
    ) -> Result<
        (
            UnresolvedGraphText<'a>,
            RowSchema<'a>,
            bool,
            Option<RowJoinSpec>,
        ),
        GraphSetTextError,
    > {
        let at = self.current.at;
        let optional = kind == Some(RowJoinKind::Left);
        self.read_row_bindings = incoming.iter().map(|(name, _)| *name).collect();
        self.read_comparisons = Some(Vec::new());
        // A carried vertex whose property a required part reads (`WITH a
        // MATCH (b) WHERE a.p < b.p`) re-matches by identity, exactly as the
        // name-reuse form `MATCH (a), (b)` does: the key loop below
        // correlates it with the carried column (fgdb-dhcfo). Not in an
        // OPTIONAL part, where a read through the re-matched vertex would be
        // NULL on a null-extended row although the carried vertex is not.
        if kind == Some(RowJoinKind::Inner) {
            for name in self.carried_property_reads(incoming)? {
                self.capacity(
                    self.syntax.variables.len(),
                    crate::algebra::MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Vertices,
                )?;
                self.syntax.variables.push(name);
            }
        }
        // `OPTIONAL MATCH (a) OPTIONAL MATCH (b)` left-joins clause by
        // clause: this part ends before the next clause, at an implicit
        // `WITH *` (multipart_head), never wrapping it in one left join.
        self.optional_part = optional;
        // The first part's WHERE over values is that part's implicit `WITH *
        // WHERE` (fgdb-1c29d, fgdb-qo3jv). A continuation's WHERE stays its
        // own: an OPTIONAL one belongs to the left join, not after it.
        self.defer_where = kind.is_none();
        let parsed = self.parse_match_prefix();
        self.defer_where = false;
        let deferred = self.deferred_where.take();
        self.optional_part = false;
        let row_fields = core::mem::take(&mut self.read_row_fields);
        let row_comparisons = self.read_comparisons.take().unwrap_or_default();
        parsed?;
        self.refuse_call_write(call_depth)?;
        // Wrapping several independently scoped clauses in one left join
        // would move their null-extension boundary. Keep this continuation a
        // positive pattern; another WITH may begin the next required/optional
        // part. Existing required-part scoped semantics are unchanged.
        if optional && !self.syntax.scopes.is_empty() {
            return Err(expected(
                at,
                "WITH between an OPTIONAL continuation and scoped clauses",
            ));
        }
        let mut inputs = Vec::new();
        let mut keys = Vec::new();
        for (left, &(name, kind)) in incoming.iter().enumerate() {
            // An imported name cannot silently become an independent local
            // inside EXISTS/OPTIONAL. This subset correlates root vertices;
            // the ordinary scoped compiler can then capture that root value.
            let in_root = self.syntax.variables[..self.syntax.root_variables]
                .iter()
                .any(|variable| variable.text == name.text);
            if !in_root
                && self.syntax.scopes.iter().any(|scope| {
                    scope
                        .body
                        .variables
                        .iter()
                        .chain(&scope.body.captures)
                        .any(|variable| variable.text == name.text)
                })
            {
                return Err(expected(name.at, "imported vertices in the root MATCH"));
            }
            if let Some(right) = self
                .syntax
                .variables
                .iter()
                .position(|var| var.text == name.text)
            {
                if right >= self.syntax.root_variables {
                    return Err(expected(name.at, "imported vertices in the root MATCH"));
                }
                if !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Any) {
                    return Err(expected(name.at, "a vertex-valued imported binding"));
                }
                let right =
                    self.mutation_projection(&mut inputs, self.syntax.variables[right], None)?;
                keys.push((left, right));
            }
            if self.syntax.names_path(name.text) {
                return Err(expected(name.at, "new path bindings after WITH"));
            }
            // A carried relationship correlates by identity, as a carried
            // vertex does (openCypher: `WITH r MATCH (a)-[r]->(b)` matches
            // that one relationship again). It must be one root edge.
            let edge = self
                .syntax
                .visible_edge(name.text)
                .map(|(edge, scoped)| (edge.variable, edge.walk.is_some() || scoped));
            if let Some((variable, unsupported)) = edge {
                let Some(variable) = variable.filter(|_| !unsupported) else {
                    return Err(expected(
                        name.at,
                        "a carried relationship as one root edge of the next MATCH",
                    ));
                };
                if kind != GraphSetColumnType::Edge {
                    return Err(expected(name.at, "an edge-valued imported binding"));
                }
                let right = self.mutation_projection(&mut inputs, variable, None)?;
                keys.push((left, right));
            }
        }
        // The current property-map correlation record has no scope tag. Do
        // not accidentally move an OPTIONAL/EXISTS predicate onto the outer
        // required join. Root-only maps remain supported without ambiguity.
        if !self.read_correlations.is_empty() && !self.syntax.scopes.is_empty() {
            return Err(expected(at, "root-only imported property-map correlations"));
        }
        if !row_comparisons.is_empty() && !self.syntax.scopes.is_empty() {
            return Err(expected(at, "root-only imported MATCH comparisons"));
        }
        for (variable, key, left) in core::mem::take(&mut self.read_correlations) {
            let right = self.mutation_projection(&mut inputs, variable, key)?;
            if !keys.contains(&(left, right)) {
                keys.push((left, right));
            }
        }
        let mut comparisons = Vec::new();
        for comparison in row_comparisons {
            let right =
                self.mutation_projection(&mut inputs, comparison.variable, comparison.property)?;
            comparisons.push((comparison.row, right, comparison.comparison));
        }
        // An aggregate terminal addresses completed row aliases, not graph
        // slots. A continuation's own RETURN reads every visible binding, so
        // it passes through the `WITH *` boundary openCypher gives it: the
        // incoming columns plus this MATCH's bindings, nothing invented. The
        // first part still needs its explicit boundary before the next MATCH.
        // An UNWIND directly after a continuation reads that same row (TCK
        // Graph8 [4]: `OPTIONAL MATCH (n) UNWIND keys(n) AS x RETURN ...`).
        let before_call = self.inline_call()?;
        let implicit_star = kind.is_some()
            && deferred.is_none()
            && ((aggregate && self.is_word("RETURN")) || self.is_word("UNWIND") || before_call);
        if aggregate
            && !implicit_star
            && deferred.is_none()
            && !self.is_word("WITH")
            && !before_call
            && !(kind.is_none() && self.is_word("UNWIND"))
            && !self.implicit_withs.contains(&self.current.at)
        {
            return Err(expected(
                self.current.at,
                "WITH before a multipart aggregate RETURN",
            ));
        }
        let mut head = if let Some((lexer, at_where)) = deferred {
            // The row pipeline reads the WHERE after the implicit `WITH *`,
            // then this part ends at the next MATCH, as after an explicit one.
            // A continuation's `WITH *` also keeps its incoming columns.
            let head = if kind.is_none() {
                self.implicit_with_head()?
            } else {
                let outputs = self.star_outputs(incoming, &mut inputs)?;
                GraphProjectionHead {
                    with: true,
                    plain: false,
                    inputs,
                    outputs,
                }
            };
            self.lexer = lexer;
            self.current = at_where;
            head
        } else if kind.is_none() {
            if before_call {
                self.implicit_with_head()?
            } else {
                self.graph_projection_head()?
            }
        } else if implicit_star {
            // The same graph-to-row aggregate head owns carried properties
            // in WITH and in an implicit WITH * before aggregate RETURN.
            self.with_graph_head(incoming, inputs, optional)?
        } else {
            self.multipart_head(incoming, inputs, optional)?
        };
        // graph_projection_head numbers its graph inputs from zero; a
        // continuation head places them after the incoming row columns.
        let offset = if kind.is_none() { 0 } else { incoming.len() };
        if !implicit_star {
            self.hoist_boundary_reads(&mut head, offset)?;
        }
        let graph_page = if head.with {
            None
        } else {
            self.graph_expression_page(&mut head, incoming, optional)?
        };
        if head.inputs.is_empty() {
            // A constant projection still emits once per graph occurrence.
            self.mutation_projection(&mut head.inputs, self.syntax.variables[0], None)?;
        }
        let mut types: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
        types.extend(head.inputs.iter().map(projection_type));
        admit_head(&head, &types, &self.syntax.parameters)?;
        let schema: RowSchema<'a> = head
            .outputs
            .iter()
            .map(|(name, value)| (*name, value.column_type(&types, &self.syntax.parameters)))
            .collect();
        // A continuation joins its incoming relation even when its schema is
        // empty; column count is not evidence that there was no earlier part.
        let join = if let Some(kind) = kind {
            let mut left: Vec<_> = incoming.iter().map(|(_, kind)| *kind).collect();
            if !row_fields.is_empty() {
                // Private ON operands have the same pre-catalog domain
                // admission as public outputs, against the real input schema.
                admit_values(
                    row_fields.iter().map(|value| (at, value)),
                    &left,
                    &self.syntax.parameters,
                )?;
            }
            let field_types: Vec<_> = row_fields
                .iter()
                .map(|value| value.column_type(&left, &self.syntax.parameters))
                .collect();
            left.extend(field_types);
            Some(join_spec(&left, &head.inputs, &keys, &comparisons, at)?.with_kind(kind))
        } else {
            None
        };
        let mut terminal = !head.with;
        let (mut pipeline, mut next) = if head.with {
            let (pipeline, schema, _) = self.row_pipeline_prefix(schema)?;
            (pipeline, schema)
        } else {
            (graph_page.into_iter().collect(), schema)
        };
        if !terminal && self.is_word("RETURN") && !aggregate {
            let at = self.current.at;
            self.advance()?;
            let distinct = self.take_word("DISTINCT")?;
            if !distinct {
                self.take_all_quantifier()?;
            }
            let (mut projection, schema) = self.row_projection(&next, true)?;
            let page = self.return_expression_page(&next, &mut projection, distinct)?;
            // The terminal RETURN is the last reader of any hidden boundary read.
            self.boundary_reads = None;
            pipeline.push(ReadStageTemplate::Project {
                at,
                projection,
                quantifier: if distinct {
                    GraphSetQuantifier::Distinct
                } else {
                    GraphSetQuantifier::All
                },
            });
            if let Some(page) = page {
                pipeline.push(page);
            }
            next = schema;
            terminal = true;
        }
        if aggregate && self.is_word("RETURN") {
            terminal = true;
        }
        if !row_fields.is_empty() {
            self.capacity(
                incoming.len() + head.inputs.len() + row_fields.len() - 1,
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
        }
        // The surrounding set parser owns terminal ORDER BY/SKIP/LIMIT. Every
        // page before a continuation was already consumed by row_pipeline_prefix.
        self.syntax.columns = head
            .inputs
            .into_iter()
            .map(|input| Column {
                variable: input.variable,
                property: input.property,
                path: input.path,
                alias: input.variable,
                collect: input.collect,
            })
            .collect();
        let projection = Some(
            head.outputs
                .into_iter()
                .map(|(name, value)| ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value,
                })
                .collect(),
        );
        let syntax = self.take_part_syntax()?;
        Ok((
            UnresolvedGraphText {
                statement,
                syntax,
                projection,
                pipeline,
                singleton: false,
                leading: Vec::new(),
                leading_types: incoming.iter().map(|(_, kind)| *kind).collect(),
                correlations: keys,
                row_fields,
            },
            next,
            terminal,
            join,
        ))
    }

    fn multipart_head(
        &mut self,
        incoming: &RowSchema<'a>,
        mut inputs: Vec<Projection<'a>>,
        optional: bool,
    ) -> Result<GraphProjectionHead<'a>, GraphSetTextError> {
        // An OPTIONAL part that stopped at the next clause, or any part that
        // stopped at an implicit boundary (implicit_withs): `WITH *`.
        let implicit = (optional && (self.is_word("OPTIONAL") || self.is_word("MATCH")))
            || self.implicit_withs.contains(&self.current.at);
        if !implicit && self.with_has_aggregate()? {
            return self.with_graph_head(incoming, inputs, optional);
        }
        let with = implicit || self.take_word("WITH")?;
        if !with {
            self.word("RETURN")?;
        }
        if !implicit {
            self.syntax.distinct = self.take_word("DISTINCT")?;
            if !self.syntax.distinct {
                self.take_all_quantifier()?;
            }
        }
        let width = incoming.len();
        let mut outputs = Vec::new();
        if implicit || self.take(b'*')? {
            outputs = self.star_outputs(incoming, &mut inputs)?;
            // openCypher orders RETURN *'s columns by name (fgdb-7jja3);
            // WITH *, written or implicit, keeps the row's order.
            if !with {
                outputs.sort_by(|(a, _), (b, _)| a.text.cmp(b.text));
            }
        } else {
            loop {
                self.capacity(
                    outputs.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let at = self.current.at;
                let implicit = if with {
                    self.implicit_with_binding()?
                } else {
                    None
                };
                let value = self.read_resolved_item(&mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else { return Ok(None); };
                    let next = parser.lexer.clone().next()?;
                    if (word.eq_ignore_ascii_case("properties") || word.eq_ignore_ascii_case("keys"))
                        && let Some(argument) = super::property_map_source(word, parser.current.at, &parser.lexer)?
                        && parser.visible_graph_bindings().any(|name| name.text == argument.text) {
                        if optional && incoming.iter().any(|(binding, _)| binding.text == argument.text) {
                            return Err(error(argument.at, GraphPatternTextErrorKind::Expected("project carried graph maps before OPTIONAL MATCH")));
                        }
                        return parser.property_map_function(&mut inputs).map(|column| Some(width + column));
                    }
                    if matches!(next.kind, TokenKind::Punct(b'(')) {
                        if ["labels", "type", "length", "path_length", "nodes", "edges", "relationships"]
                            .iter().any(|name| word.eq_ignore_ascii_case(name))
                        {
                            // A graph-function read of an imported value is
                            // not a read of the nullable right-hand copy. Until
                            // that read has its own source, require a saved
                            // left-hand expression instead of returning NULL.
                            if optional {
                                let mut lookahead = parser.lexer.clone();
                                lookahead.next()?; // opening parenthesis
                                let argument = lookahead.next()?;
                                if matches!(argument.kind, TokenKind::Word(name)
                                    if incoming.iter().any(|(binding, _)| binding.text == name))
                                {
                                    return Err(error(argument.at, GraphPatternTextErrorKind::Expected(
                                        "project carried graph functions before OPTIONAL MATCH",
                                    )));
                                }
                            }
                            let Operand::Column(column) = parser.mutation_operand(&mut inputs)? else {
                                unreachable!("graph function resolves a source column");
                            };
                            return Ok(Some(width + column));
                        }
                        return Ok(None);
                    }
                    if let Some(column) = incoming.iter().position(|(name, _)| name.text == word)
                    {
                        parser.advance()?;
                        return Ok(Some(column));
                    }
                    let graph = parser.visible_graph_bindings().any(|name| name.text == word);
                    if !graph { return Ok(None); }
                    let variable = parser.any_variable()?;
                    let property = if parser.take(b'.')? {
                        parser.require_property_variable(variable)?;
                        Some(parser.schema_name()?)
                    } else { None };
                    let column = parser.mutation_projection(&mut inputs, variable, property)?;
                    Ok(Some(width + column))
                }, 0)?;
                let end = self.current.at;
                let alias = if self.take_word("AS")? {
                    self.name()?
                } else if with {
                    implicit
                        .filter(|(_, boundary)| {
                            *boundary == end && matches!(value, ReadValueTemplate::Column(_))
                        })
                        .map(|(name, _)| name)
                        .ok_or_else(|| expected(at, "AS alias for a computed row value"))?
                } else {
                    // An unaliased RETURN item is named by its exact text.
                    self.source_name(at, end)
                };
                if outputs
                    .iter()
                    .any(|(name, _): &(Name<'a>, ReadValueTemplate)| name.text == alias.text)
                {
                    return Err(error(
                        alias.at,
                        GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                    )
                    .into());
                }
                outputs.push((alias, value));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok(GraphProjectionHead {
            with,
            plain: false,
            inputs,
            outputs,
        })
    }

    /// `WITH *`: every incoming column, then each visible graph binding the
    /// incoming row does not already carry.
    fn star_outputs(
        &mut self,
        incoming: &RowSchema<'a>,
        inputs: &mut Vec<Projection<'a>>,
    ) -> Result<Vec<(Name<'a>, ReadValueTemplate)>, GraphSetTextError> {
        let width = incoming.len();
        let mut outputs: Vec<_> = incoming
            .iter()
            .enumerate()
            .map(|(index, &(name, _))| (name, ReadValueTemplate::Column(index)))
            .collect();
        let visible: Vec<_> = self.visible_graph_bindings().collect();
        for variable in visible {
            if incoming.iter().any(|(name, _)| name.text == variable.text) {
                continue;
            }
            self.capacity(
                outputs.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let column = self.mutation_projection(inputs, variable, None)?;
            outputs.push((variable, ReadValueTemplate::Column(width + column)));
        }
        Ok(outputs)
    }
}

impl BoundReadInput {
    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let values = self.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        self.first.checked_arguments(arguments)
    }

    /// Only a caller that checked the complete shared argument map may enter.
    /// Both ordinary multipart reads and exact aggregation bind this same tree.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut input = self.first.bind_values(values)?;
        for part in &self.continuations {
            let source = &part.input;
            if let BoundContinuationOperation::Apply { body, imports } = &part.operation {
                input = input
                    .apply(body.bind_values(values)?, imports.clone())
                    .map_err(|kind| GraphSetTextError {
                        offset: source.return_at,
                        kind: GraphSetTextErrorKind::SetBuild(kind),
                    })?;
                if let Some(projection) = &source.projection {
                    input = super::super::bind_projection(
                        input,
                        projection,
                        source.quantifier,
                        values,
                        source.return_at,
                    )?;
                }
                input = super::super::bind_stages(input, &source.pipeline, values)?;
                continue;
            }
            let BoundContinuationOperation::Match(join) = &part.operation else {
                unreachable!("Apply handled above")
            };
            let imported_width = input.column_types().len();
            input = source.bind_row_fields(input, values)?;
            let left_width = input.column_types().len();
            let right = source
                .selection
                .as_ref()
                .expect("continuations are graph sources")
                .bind_values(values)?;
            input = input
                .join(right.into(), join.clone())
                .map_err(|kind| GraphSetTextError {
                    offset: source.return_at,
                    kind: GraphSetTextErrorKind::SetBuild(kind),
                })?;
            if !source.row_fields.is_empty() {
                // ON may address private computed LEFT keys. Remove them
                // before public projection/DISTINCT, restoring the original
                // incoming + graph ordinals without changing bag occurrences.
                let projection = (0..imported_width)
                    .chain(left_width..input.column_types().len())
                    .enumerate()
                    .map(|(output, column)| {
                        GraphSetProjection::new(
                            format!("__fg_match_input_{output}"),
                            GraphSetValue::Column(column),
                        )
                    })
                    .collect();
                input =
                    input
                        .project_ordered_output(projection)
                        .map_err(|kind| GraphSetTextError {
                            offset: source.return_at,
                            kind: GraphSetTextErrorKind::ProjectionBuild(kind),
                        })?;
            }
            if let Some(projection) = &source.projection {
                input = super::super::bind_projection(
                    input,
                    projection,
                    source.quantifier,
                    values,
                    source.return_at,
                )?;
            }
            input = super::super::bind_stages(input, &source.pipeline, values)?;
        }
        Ok(input)
    }
}
