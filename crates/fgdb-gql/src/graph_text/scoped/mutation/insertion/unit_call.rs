//! Unit write subqueries prepare a native scoped chain. The scheduler invokes
//! that chain once per outer occurrence and preserves the outer row, including
//! when a body's read produces no rows. No result or local binding is exported.

use super::chain::PreparedContinuation;
use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::graph_text::scoped::mutation::return_projection::pipeline::multipart::UnresolvedReadInput;
use crate::set_text::multipart::BoundReadInput;
use crate::set_text::{ElementKeySlot, ReadProjectionTemplate};
use crate::write_chain::{ChainTextStep, PreparedGraphUnitCallText};
use crate::{GraphWriteChainTextError, PreparedGraphUnitCall, PreparedGraphWriteChainText};

struct UnitCallPreparation<'a> {
    statement: &'a str,
    parser: Parser<'a>,
    input: BoundReadInput,
    outer: Vec<(Name<'a>, GraphSetColumnType)>,
    outer_columns: Vec<String>,
    outer_types: Vec<GraphSetColumnType>,
    imports: Vec<usize>,
    slots: Vec<ElementKeySlot>,
    next: Option<ChainContinuation<'a>>,
    steps: Vec<ChainTextStep>,
    close_at: usize,
}

impl PreparedGraphUnitCallText {
    pub(crate) fn prepare_step<'a>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut impl GraphSymbolResolver,
        from: Option<ChainContinuation<'a>>,
    ) -> Result<(Self, Option<ChainContinuation<'a>>), GraphWriteChainTextError> {
        // Suspended parsers are heap-owned. Nested calls use this work stack,
        // never recursive compiler invocations with live parser frames.
        let mut pending = vec![Self::prepare_at_depth(
            statement,
            declarations,
            resolve,
            from,
            0,
        )?];
        loop {
            let frame = pending
                .last_mut()
                .ok_or_else(|| expected(statement.len(), "an unfinished unit CALL"))?;
            if let Some(from) = frame.next_nested(relation, declarations, resolve)? {
                let statement = &frame.statement[..frame.close_at];
                let depth = pending.len();
                pending.push(Self::prepare_at_depth(
                    statement,
                    declarations,
                    resolve,
                    Some(from),
                    depth,
                )?);
                continue;
            }
            let frame = pending
                .pop()
                .ok_or_else(|| expected(statement.len(), "a completed unit CALL"))?;
            let (call, after) = frame.finish(resolve)?;
            if let Some(parent) = pending.last_mut() {
                parent.steps.push(ChainTextStep::UnitCall(Box::new(call)));
                parent.next = after;
            } else {
                return Ok((call, after));
            }
        }
    }

    fn prepare_at_depth<'a>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut impl GraphSymbolResolver,
        from: Option<ChainContinuation<'a>>,
        depth: usize,
    ) -> Result<UnitCallPreparation<'a>, GraphWriteChainTextError> {
        if depth >= crate::MAX_GRAPH_SET_DEPTH {
            return Err(crate::GraphSetTextError {
                offset: from.as_ref().map_or(0, |from| from.at),
                kind: crate::GraphSetTextErrorKind::SetBuild(crate::GraphSetBuildError::TooDeep {
                    limit: crate::MAX_GRAPH_SET_DEPTH,
                    observed: depth + 1,
                }),
            }
            .into());
        }
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        parser.chain_write = true;
        let (owned, incoming) = match from {
            Some(from) => {
                let owned = Some(Box::new(from.owned_columns()));
                parser = parser.resume_after(from.boundary, &from.parameters, from.at)?;
                (owned, Some(from.row))
            }
            None => (None, None),
        };
        let (mut source, mut outer) =
            parser.multipart_write_prefix(statement, declarations, incoming)?;
        let at = parser.current.at;
        parser.word("CALL")?;

        // Hidden property preimages belong to the preceding clause. Import
        // visible identities only; body reads observe its current staged cut.
        let visible = parser.visible_width(&outer);
        if visible != outer.len() {
            source.append_stages(vec![ReadStageTemplate::Project {
                at,
                projection: outer[..visible]
                    .iter()
                    .enumerate()
                    .map(|(column, (name, _))| ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value: ReadValueTemplate::Column(column),
                    })
                    .collect(),
                quantifier: crate::GraphSetQuantifier::All,
            }]);
            outer.truncate(visible);
        }
        parser.boundary_reads = None;
        parser.projected_sources = None;
        parser.existence_reads = None;
        parser.comprehension_reads = None;
        parser.grouped_comprehensions = None;
        parser.read_elements = true;
        parser.call_element_reads = true;

        let scoped = parser.take(b'(')?;
        let mut imports = Vec::new();
        if scoped {
            if parser.take(b'*')? {
                imports.extend(0..outer.len());
            } else if !parser.is_punct(b')') {
                imports = parser.unit_call_import_names(&outer)?;
            }
            parser.punct(b')', ") after CALL imports")?;
        }
        parser.punct(b'{', "{ before a unit CALL body")?;
        if !scoped && parser.unit_call_legacy_imports(&outer)? {
            parser.word("WITH")?;
            if parser.take(b'*')? {
                imports.extend(0..outer.len());
            } else {
                imports = parser.unit_call_import_names(&outer)?;
            }
        }
        let body_at = parser.current.at;
        let close_at = parser.unit_call_body_end(depth)?;
        let outer_columns: Vec<_> = outer.iter().map(|(name, _)| name.text.to_owned()).collect();
        let outer_types: Vec<_> = outer.iter().map(|(_, kind)| *kind).collect();
        source.share_parameters(&parser.syntax.parameters, &parser.syntax.parameter_offsets);
        let slots = source.element_key_slots();
        let mut input = source.resolve(resolve)?;
        input.first.owned = owned;
        let local = imports.iter().map(|&column| outer[column]).collect();
        let next = Some(ChainContinuation {
            at: body_at,
            row: local,
            boundary: None,
            parameters: parser.syntax.parameters.clone(),
        });
        Ok(UnitCallPreparation {
            statement,
            parser,
            input,
            outer,
            outer_columns,
            outer_types,
            imports,
            slots,
            next,
            steps: Vec::new(),
            close_at,
        })
    }

    pub(crate) fn parameter_schema(&self) -> Vec<GqlParameterSpec> {
        let mut schema: Vec<GqlParameterSpec> = Vec::new();
        let body_schema = self.body.parameter_schema();
        for spec in self
            .input
            .parameter_schema()
            .iter()
            .chain(body_schema.iter())
            .chain(
                self.output
                    .iter()
                    .flat_map(|output| output.parameter_schema()),
            )
        {
            if !schema.iter().any(|known| known.name == spec.name) {
                schema.push(spec.clone());
            }
        }
        schema
    }

    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
        expected_input: Option<&[GraphSetColumnType]>,
    ) -> Result<PreparedGraphUnitCall, GraphWriteChainTextError> {
        if let Some((_, types)) = self.input.first.owned.as_deref()
            && expected_input != Some(types.as_slice())
        {
            return Err(
                expected(self.input.first.return_at, "the previous write's WITH row").into(),
            );
        }
        let input = self
            .input
            .bind_parameters(&arguments.subset_for(self.input.parameter_schema()))?;
        let types = self
            .imports
            .iter()
            .map(|&column| self.outer_types[column])
            .collect();
        let body = self.body.bind_parameters_with_input(
            &arguments.subset_for(&self.body.parameter_schema()),
            Some(types),
        )?;
        let output = self
            .output
            .as_ref()
            .map(|output| output.bind_parameters(&arguments.subset_for(output.parameter_schema())))
            .transpose()?;
        Ok(PreparedGraphUnitCall {
            input,
            output,
            body,
            outer_columns: self.outer_columns.clone(),
            outer_types: self.outer_types.clone(),
            imports: self.imports.clone(),
            read_keys: self.read_keys.clone(),
            returning: self.returning,
        })
    }
}

impl<'a> UnitCallPreparation<'a> {
    fn next_nested(
        &mut self,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut impl GraphSymbolResolver,
    ) -> Result<Option<ChainContinuation<'a>>, GraphWriteChainTextError> {
        let body_text = &self.statement[..self.close_at];
        while let Some(from) = self.next.take() {
            if self.steps.len() >= crate::MAX_GRAPH_SET_DEPTH {
                return Err(expected(from.at, "bounded unit CALL body step count").into());
            }
            match PreparedGraphWriteChainText::prepare_continuation(
                body_text,
                relation,
                declarations,
                resolve,
                from,
                true,
            )? {
                PreparedContinuation::UnitCall(from) => return Ok(Some(from)),
                PreparedContinuation::Write(step, after) => {
                    self.steps.push(step);
                    self.next = after;
                }
                PreparedContinuation::Read(_) => {
                    return Err(
                        expected(self.close_at, "a unit CALL body ending in a write").into(),
                    );
                }
            }
        }
        Ok(None)
    }

    fn finish(
        self,
        resolve: &mut impl GraphSymbolResolver,
    ) -> Result<(PreparedGraphUnitCallText, Option<ChainContinuation<'a>>), GraphWriteChainTextError>
    {
        let Self {
            statement,
            mut parser,
            input,
            outer,
            outer_columns,
            outer_types,
            imports,
            mut slots,
            steps,
            close_at,
            next: _,
        } = self;
        if steps.is_empty() {
            return Err(expected(close_at, "a nonempty unit CALL write body").into());
        }
        let body = PreparedGraphWriteChainText {
            statement: statement[..close_at].to_owned(),
            steps,
            read: None,
            read_keys: Vec::new(),
        };
        let mut parameters = parser.syntax.parameters.clone();
        for spec in body.parameter_schema() {
            if !parameters.iter().any(|known| known.name == spec.name) {
                parameters.push(spec);
            }
        }
        parser
            .syntax
            .parameter_offsets
            .resize(parameters.len(), close_at);
        parser.syntax.parameters.clone_from(&parameters);
        parser = parser.resume_after(None, &parameters, close_at)?;
        parser.projected_sources = None;
        parser.existence_reads = None;
        parser.comprehension_reads = None;
        parser.grouped_comprehensions = None;
        parser.punct(b'}', "} after a unit CALL body")?;
        let (output, continuation, returning) = if parser.at_chained_write()? {
            (
                None,
                Some(ChainContinuation {
                    at: parser.current.at,
                    row: outer,
                    boundary: None,
                    parameters,
                }),
                true,
            )
        } else if matches!(parser.current.kind, TokenKind::End) {
            parser.end()?;
            (None, None, false)
        } else {
            let pipeline = parser.write_row_pipeline(outer)?;
            let continuation = parser.continuation();
            if continuation.is_none() {
                parser.end()?;
            }
            let output = UnresolvedReadInput::from_first(UnresolvedGraphText {
                statement,
                syntax: parser.take_part_syntax()?,
                projection: None,
                pipeline,
                singleton: true,
                leading: Vec::new(),
                leading_types: Vec::new(),
                correlations: Vec::new(),
                row_fields: Vec::new(),
            });
            slots.extend(output.element_key_slots());
            let mut output = output.resolve(resolve)?;
            output.first.owned = Some(Box::new((outer_columns.clone(), outer_types.clone())));
            (Some(output), continuation, true)
        };
        let read_keys = slots
            .iter()
            .filter_map(|slot| match slot.bind() {
                crate::GraphElementKey::Property(key) => Some(key),
                crate::GraphElementKey::Unbound | crate::GraphElementKey::Missing => None,
            })
            .collect();
        Ok((
            PreparedGraphUnitCallText {
                input,
                output,
                body,
                outer_columns,
                outer_types,
                imports,
                read_keys,
                returning,
            },
            continuation,
        ))
    }
}

impl<'a> Parser<'a> {
    fn unit_call_import_names(
        &mut self,
        outer: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Vec<usize>, GraphPatternTextError> {
        let mut imports = Vec::new();
        loop {
            let name = self.name()?;
            let column = outer
                .iter()
                .position(|(binding, _)| binding.text == name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownVariable))?;
            if imports.contains(&column) {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("unique CALL imports"),
                ));
            }
            imports.push(column);
            if !self.take(b',')? {
                return Ok(imports);
            }
        }
    }

    /// A legacy importing WITH names only outer bindings. Other WITH stages
    /// remain ordinary native row projections over the uncorrelated unit row.
    fn unit_call_legacy_imports(
        &self,
        outer: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("WITH") {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        loop {
            match lexer.next()?.kind {
                TokenKind::Punct(b'*') => return Ok(true),
                TokenKind::Word(name) if outer.iter().any(|(binding, _)| binding.text == name) => {}
                _ => return Ok(false),
            }
            let next = lexer.next()?;
            if matches!(next.kind, TokenKind::Punct(b',')) {
                continue;
            }
            return Ok(matches!(next.kind, TokenKind::Word(word)
                if ["MATCH", "OPTIONAL", "WITH", "UNWIND", "CALL", "CREATE", "INSERT", "MERGE", "SET", "REMOVE", "DELETE", "DETACH", "FOREACH"].iter().any(|clause| word.eq_ignore_ascii_case(clause))));
        }
    }

    fn unit_call_body_end(&self, outer_depth: usize) -> Result<usize, GraphPatternTextError> {
        let mut lexer = Lexer {
            text: self.lexer.text,
            at: self.current.at,
            tokens: 0,
        };
        let mut brackets = Vec::new();
        let mut scope_starts = std::collections::BTreeSet::new();
        let mut scopes = outer_depth + 1;
        let mut previous = None;
        let mut nonempty = false;
        loop {
            let token = lexer.next()?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => {
                    return Err(error(
                        token.at,
                        GraphPatternTextErrorKind::Expected("closing unit CALL brace"),
                    ));
                }
                TokenKind::Punct(b'}') if brackets.is_empty() => {
                    if !nonempty {
                        return Err(error(
                            token.at,
                            GraphPatternTextErrorKind::Expected("nonempty unit CALL write body"),
                        ));
                    }
                    return Ok(token.at);
                }
                TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => {
                    let scoped = scope_starts.remove(&token.at);
                    if scoped {
                        if scopes >= crate::MAX_GRAPH_SET_DEPTH {
                            return Err(error(
                                token.at,
                                GraphPatternTextErrorKind::Expected(
                                    "bounded combined CALL and FOREACH nesting",
                                ),
                            ));
                        }
                        scopes += 1;
                    }
                    brackets.push((open, scoped));
                }
                TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                    let open = match close {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    let Some((found, scoped)) = brackets.pop() else {
                        return Err(error(
                            token.at,
                            GraphPatternTextErrorKind::Expected("balanced unit CALL body"),
                        ));
                    };
                    if found != open {
                        return Err(error(
                            token.at,
                            GraphPatternTextErrorKind::Expected("balanced unit CALL body"),
                        ));
                    }
                    if scoped {
                        scopes -= 1;
                    }
                }
                TokenKind::Word(word)
                    if brackets.is_empty()
                        && !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["AS", "STARTS", "ENDS", "ON"].iter().any(|word| prior.eq_ignore_ascii_case(word)))
                        && ["RETURN", "UNION"]
                            .iter()
                            .any(|clause| word.eq_ignore_ascii_case(clause)) =>
                {
                    return Err(error(
                        token.at,
                        GraphPatternTextErrorKind::Expected(
                            "a unit CALL write body without RETURN or UNION",
                        ),
                    ));
                }
                TokenKind::Word(word)
                    if !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["AS", "ON", "STARTS", "ENDS"].iter().any(|not| prior.eq_ignore_ascii_case(not))) =>
                {
                    if word.eq_ignore_ascii_case("CALL")
                        && let Some(at) =
                            PreparedGraphWriteChainText::call_body_open(&mut lexer.clone())?
                    {
                        scope_starts.insert(at);
                    } else if word.eq_ignore_ascii_case("FOREACH") {
                        let next = lexer.clone().next()?;
                        if matches!(next.kind, TokenKind::Punct(b'(')) {
                            scope_starts.insert(next.at);
                        }
                    }
                }
                _ => {}
            }
            nonempty = true;
            previous = Some(token.kind);
        }
    }
}

impl PreparedGraphWriteChainText {
    /// Locate an inline CALL's opening brace, leaving the lexer just after it.
    /// This does not admit the import list; the scoped parser owns that grammar.
    fn call_body_open(lexer: &mut Lexer<'_>) -> Result<Option<usize>, GraphPatternTextError> {
        let mut token = script::next_script_token(lexer)?;
        if matches!(token.kind, TokenKind::Punct(b'(')) {
            let mut depth = 1_usize;
            while depth != 0 {
                token = script::next_script_token(lexer)?;
                match token.kind {
                    TokenKind::Punct(b'(') => depth += 1,
                    TokenKind::Punct(b')') => depth -= 1,
                    TokenKind::End | TokenKind::Punct(b';') => return Ok(None),
                    _ => {}
                }
            }
            token = script::next_script_token(lexer)?;
        }
        Ok(matches!(token.kind, TokenKind::Punct(b'{')).then_some(token.at))
    }

    /// Framing only: the lexer is immediately after CALL. A complete native
    /// compiler still owns every scope, write and expression. Read-only CALL
    /// bodies keep their established owner and refusal rules.
    pub(in crate::graph_text::scoped::mutation) fn call_has_write(
        mut lexer: Lexer<'_>,
    ) -> Result<bool, GraphPatternTextError> {
        if Self::call_body_open(&mut lexer)?.is_none() {
            return Ok(false);
        }
        let mut brackets = vec![b'{'];
        let mut previous = None;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => brackets.push(open),
                TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                    let open = match close {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if brackets.pop() != Some(open) || brackets.is_empty() {
                        return Ok(false);
                    }
                }
                TokenKind::Word(word)
                    if brackets.iter().all(|&open| open == b'{')
                        && !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["AS", "ON", "STARTS", "ENDS"].iter().any(|not| prior.eq_ignore_ascii_case(not))) =>
                {
                    if ["CREATE", "INSERT", "MERGE", "FOREACH"]
                        .iter()
                        .any(|clause| word.eq_ignore_ascii_case(clause))
                        && (matches!(
                            script::next_script_token(&mut lexer.clone())?.kind,
                            TokenKind::Punct(b'(')
                        ) || Self::named_merge_pattern(lexer.clone())?)
                    {
                        return Ok(true);
                    }
                    if ["SET", "REMOVE"]
                        .iter()
                        .any(|clause| word.eq_ignore_ascii_case(clause))
                        && Self::mutation_clause_target(lexer.clone())?
                    {
                        return Ok(true);
                    }
                    if ["DELETE", "DETACH"]
                        .iter()
                        .any(|clause| word.eq_ignore_ascii_case(clause))
                        && Self::deletion_clause_target(
                            lexer.clone(),
                            previous,
                            word.eq_ignore_ascii_case("DETACH"),
                        )?
                    {
                        return Ok(true);
                    }
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
    }
}
