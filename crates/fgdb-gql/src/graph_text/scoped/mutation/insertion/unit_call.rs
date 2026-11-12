//! Write subqueries prepare a native scoped chain. Unit bodies preserve each
//! outer occurrence; returning bodies export their typed result columns. Both
//! use the same bounded heap preparation frames and native write/read owners.

use super::chain::{PreparedContinuation, PreparedReadContinuation};
use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::graph_text::scoped::mutation::return_projection::pipeline::multipart::UnresolvedReadInput;
use crate::set_text::multipart::BoundReadInput;
use crate::set_text::{ElementKeySlot, ReadProjectionTemplate};
use crate::write_chain::{
    ChainTextStep, PreparedGraphCallArmText, PreparedGraphCallBody, PreparedGraphCallBodyText,
    PreparedGraphCallText,
};
use crate::{
    GraphWriteChainStep, GraphWriteChainTextError, PreparedGraphCallArm,
    PreparedGraphReturningCall, PreparedGraphUnitCall, PreparedGraphWriteChainText,
};

struct CallArmRange {
    start: usize,
    end: usize,
}

struct CallPreparation<'a> {
    statement: &'a str,
    parser: Parser<'a>,
    input: BoundReadInput,
    outer: Vec<(Name<'a>, GraphSetColumnType)>,
    outer_columns: Vec<String>,
    outer_types: Vec<GraphSetColumnType>,
    scoped_imports: Option<Vec<usize>>,
    imports: Vec<usize>,
    ranges: Vec<CallArmRange>,
    arm: usize,
    arms: Vec<PreparedGraphCallArmText>,
    quantifier: Option<crate::GraphSetQuantifier>,
    parameters: Vec<GqlParameterSpec>,
    slots: Vec<ElementKeySlot>,
    next: Option<ChainContinuation<'a>>,
    steps: Vec<ChainTextStep>,
    read: Option<BoundReadInput>,
    read_keys: Vec<PropertyKeyId>,
    arm_read_keys: Vec<PropertyKeyId>,
    exports: Option<Vec<(Name<'a>, GraphSetColumnType)>>,
    combined_exports: Option<Vec<(Name<'a>, GraphSetColumnType)>>,
    close_at: usize,
}

impl PreparedGraphCallText {
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
                let statement = &frame.statement[..frame.ranges[frame.arm].end];
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
                parent.steps.push(ChainTextStep::Call(Box::new(call)));
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
    ) -> Result<CallPreparation<'a>, GraphWriteChainTextError> {
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
        let (ranges, quantifier, close_at) = parser.unit_call_body_arms(depth)?;
        let outer_columns: Vec<_> = outer.iter().map(|(name, _)| name.text.to_owned()).collect();
        let outer_types: Vec<_> = outer.iter().map(|(_, kind)| *kind).collect();
        source.share_parameters(&parser.syntax.parameters, &parser.syntax.parameter_offsets);
        let slots = source.element_key_slots();
        let mut input = source.resolve(resolve)?;
        input.first.owned = owned;
        let parameters = parser.syntax.parameters.clone();
        let mut frame = CallPreparation {
            statement,
            parser,
            input,
            outer,
            outer_columns,
            outer_types,
            scoped_imports: scoped.then_some(imports),
            imports: Vec::new(),
            ranges,
            arm: 0,
            arms: Vec::new(),
            quantifier,
            parameters,
            slots,
            next: None,
            steps: Vec::new(),
            read: None,
            read_keys: Vec::new(),
            arm_read_keys: Vec::new(),
            exports: None,
            combined_exports: None,
            close_at,
        };
        frame.start_arm(declarations, resolve)?;
        Ok(frame)
    }

    pub(crate) fn parameter_schema(&self) -> Vec<GqlParameterSpec> {
        let mut schema: Vec<GqlParameterSpec> = Vec::new();
        let body_schema: Vec<_> = self
            .arms
            .iter()
            .flat_map(|arm| arm.body.parameter_schema())
            .collect();
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
    ) -> Result<GraphWriteChainStep, GraphWriteChainTextError> {
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
        let mut arms = Vec::new();
        for arm in &self.arms {
            let types: Vec<_> = arm
                .imports
                .iter()
                .map(|&column| self.outer_types[column])
                .collect();
            let own = arguments.subset_for(&arm.body.parameter_schema());
            let body = match &arm.body {
                PreparedGraphCallBodyText::Write(body) => PreparedGraphCallBody::Write(
                    body.bind_parameters_with_input(&own, Some(types))?,
                ),
                PreparedGraphCallBodyText::Read(body) => {
                    let imported = body.first.owned.as_deref();
                    if !imported
                        .is_some_and(|(_, expected)| expected.as_slice() == types.as_slice())
                    {
                        return Err(expected(
                            self.input.first.return_at,
                            "the CALL arm's independent import row",
                        )
                        .into());
                    }
                    let read = body.bind_parameters(&own)?;
                    if !imported
                        .is_some_and(|(columns, types)| read.has_one_owned_input(columns, types))
                        || read.check_parent_depth().is_err()
                    {
                        return Err(expected(
                            self.input.first.return_at,
                            "one complete import row for the CALL read arm",
                        )
                        .into());
                    }
                    PreparedGraphCallBody::Read(read)
                }
            };
            let alignment = if let Some((columns, types)) = &arm.exports {
                let (actual_columns, actual_types) = match &body {
                    PreparedGraphCallBody::Write(body) => (body.columns(), body.column_types()),
                    PreparedGraphCallBody::Read(body) => (body.columns(), body.column_types()),
                };
                if actual_columns != columns || actual_types != types {
                    return Err(expected(
                        self.input.first.return_at,
                        "the prepared CALL arm export schema",
                    )
                    .into());
                }
                let (common, _) = self.exports.as_ref().ok_or_else(|| {
                    expected(self.input.first.return_at, "RETURN in every CALL arm")
                })?;
                crate::set_text::union_column_order(common, columns).map_err(|()| {
                    crate::GraphSetTextError {
                        offset: self.input.first.return_at,
                        kind: crate::GraphSetTextErrorKind::DifferentColumnsInUnion,
                    }
                })?
            } else {
                Vec::new()
            };
            arms.push(PreparedGraphCallArm {
                body,
                imports: arm.imports.clone(),
                alignment,
            });
        }
        let output = self
            .output
            .as_ref()
            .map(|output| output.bind_parameters(&arguments.subset_for(output.parameter_schema())))
            .transpose()?;
        if let Some((columns, types)) = &self.exports {
            let joined_columns = self.outer_columns.iter().chain(columns).cloned().collect();
            let joined_types = self.outer_types.iter().chain(types).copied().collect();
            Ok(GraphWriteChainStep::ReturningCall(Box::new(
                PreparedGraphReturningCall {
                    input,
                    output,
                    arms,
                    quantifier: self.quantifier,
                    outer_types: self.outer_types.clone(),
                    export_columns: columns.clone(),
                    export_types: types.clone(),
                    joined_columns,
                    joined_types,
                    read_keys: self.read_keys.clone(),
                    returning: self.returning,
                },
            )))
        } else {
            Ok(GraphWriteChainStep::UnitCall(Box::new(
                PreparedGraphUnitCall {
                    input,
                    output,
                    arms,
                    outer_columns: self.outer_columns.clone(),
                    outer_types: self.outer_types.clone(),
                    read_keys: self.read_keys.clone(),
                    returning: self.returning,
                },
            )))
        }
    }
}

impl<'a> CallPreparation<'a> {
    fn start_arm(
        &mut self,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut impl GraphSymbolResolver,
    ) -> Result<(), GraphWriteChainTextError> {
        let range = &self.ranges[self.arm];
        let mut parser =
            Parser::new_with_parameter_types(&self.statement[..range.end], declarations)?
                .with_zones(resolve.time_zones())
                .resume_after(None, &self.parameters, range.start)?;
        self.imports = if let Some(imports) = &self.scoped_imports {
            imports.clone()
        } else if parser.unit_call_legacy_imports(&self.outer)? {
            parser.word("WITH")?;
            if parser.take(b'*')? {
                (0..self.outer.len()).collect()
            } else {
                parser.unit_call_import_names(&self.outer)?
            }
        } else {
            Vec::new()
        };
        self.next = Some(ChainContinuation {
            at: parser.current.at,
            row: self
                .imports
                .iter()
                .map(|&column| self.outer[column])
                .collect(),
            boundary: None,
            parameters: self.parameters.clone(),
        });
        Ok(())
    }

    fn finish_arm(&mut self) -> Result<(), GraphWriteChainTextError> {
        let range = &self.ranges[self.arm];
        let body = if self.steps.is_empty() {
            let read = self
                .read
                .take()
                .ok_or_else(|| expected(range.start, "a nonempty native CALL arm"))?;
            self.arm_read_keys.append(&mut self.read_keys);
            PreparedGraphCallBodyText::Read(read)
        } else {
            PreparedGraphCallBodyText::Write(PreparedGraphWriteChainText {
                statement: self.statement[..range.end].to_owned(),
                steps: core::mem::take(&mut self.steps),
                read: self.read.take(),
                read_keys: core::mem::take(&mut self.read_keys),
            })
        };
        for spec in body.parameter_schema() {
            if let Some(known) = self
                .parameters
                .iter_mut()
                .find(|known| known.name == spec.name)
            {
                if known.parameter_type != spec.parameter_type {
                    return Err(error(
                        range.start,
                        GraphPatternTextErrorKind::ConflictingParameterTypes,
                    )
                    .into());
                }
                known.requires_positive |= spec.requires_positive;
                known.occurrences = known.occurrences.max(spec.occurrences);
            } else {
                self.parameters.push(spec);
            }
        }
        let exports = self.exports.take();
        if self.arms.is_empty() {
            self.combined_exports.clone_from(&exports);
        } else {
            match (&mut self.combined_exports, &exports) {
                (None, None) => {}
                (Some(common), Some(exported)) => {
                    let left: Vec<_> = common
                        .iter()
                        .map(|(name, _)| name.text.to_owned())
                        .collect();
                    let right: Vec<_> = exported
                        .iter()
                        .map(|(name, _)| name.text.to_owned())
                        .collect();
                    let alignment =
                        crate::set_text::union_column_order(&left, &right).map_err(|()| {
                            crate::GraphSetTextError {
                                offset: range.start,
                                kind: crate::GraphSetTextErrorKind::DifferentColumnsInUnion,
                            }
                        })?;
                    for (column, (_, left)) in common.iter_mut().enumerate() {
                        let right = exported[alignment[column]].1;
                        *left = left
                            .common_set_domain(right)
                            .ok_or(crate::GraphSetTextError {
                                offset: range.start,
                                kind: crate::GraphSetTextErrorKind::SetBuild(
                                    crate::GraphSetBuildError::ColumnType {
                                        column,
                                        left: *left,
                                        right,
                                    },
                                ),
                            })?;
                    }
                }
                _ => {
                    return Err(expected(range.start, "RETURN in all UNION arms or in none").into());
                }
            }
        }
        self.arms.push(PreparedGraphCallArmText {
            body,
            imports: core::mem::take(&mut self.imports),
            exports: exports.map(|exports| {
                exports
                    .into_iter()
                    .map(|(name, kind)| (name.text.to_owned(), kind))
                    .unzip()
            }),
        });
        Ok(())
    }

    fn next_nested(
        &mut self,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut impl GraphSymbolResolver,
    ) -> Result<Option<ChainContinuation<'a>>, GraphWriteChainTextError> {
        loop {
            let end = self.ranges[self.arm].end;
            let body_text = &self.statement[..end];
            while let Some(from) = self.next.take() {
                if self.steps.len() >= crate::MAX_GRAPH_SET_DEPTH && from.at != end {
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
                    PreparedContinuation::Read(read) => {
                        let PreparedReadContinuation {
                            input,
                            keys,
                            output,
                        } = *read;
                        self.read = Some(input);
                        self.read_keys = keys;
                        self.exports = Some(output);
                    }
                    PreparedContinuation::Returned(exports) => self.exports = Some(exports),
                }
            }
            self.finish_arm()?;
            self.arm += 1;
            if self.arm == self.ranges.len() {
                return Ok(None);
            }
            self.start_arm(declarations, resolve)?;
        }
    }

    fn finish(
        self,
        resolve: &mut impl GraphSymbolResolver,
    ) -> Result<(PreparedGraphCallText, Option<ChainContinuation<'a>>), GraphWriteChainTextError>
    {
        let Self {
            statement,
            mut parser,
            input,
            outer,
            outer_columns,
            outer_types,
            mut slots,
            arms,
            quantifier,
            parameters,
            arm_read_keys,
            combined_exports: exports,
            close_at,
            ..
        } = self;
        if !arms
            .iter()
            .any(|arm| matches!(arm.body, PreparedGraphCallBodyText::Write(_)))
        {
            return Err(expected(close_at, "a CALL containing a native write arm").into());
        }
        let mut combined = outer;
        if let Some(exports) = &exports {
            for &(name, kind) in exports {
                if combined.iter().any(|(outer, _)| outer.text == name.text) {
                    return Err(
                        expected(name.at, "a CALL export distinct from outer bindings").into(),
                    );
                }
                parser.capacity(
                    combined.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                combined.push((name, kind));
            }
        }
        let (joined_columns, joined_types): (Vec<_>, Vec<_>) = combined
            .iter()
            .map(|(name, kind)| (name.text.to_owned(), *kind))
            .unzip();
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
                    row: combined,
                    boundary: None,
                    parameters,
                }),
                true,
            )
        } else if matches!(parser.current.kind, TokenKind::End) && exports.is_none() {
            parser.end()?;
            (None, None, false)
        } else {
            let pipeline = if matches!(parser.current.kind, TokenKind::End) {
                // A standalone returning CALL delivers its checked combined
                // row through the native projection owner, without inventing
                // a source-text RETURN or charging private body rows publicly.
                vec![ReadStageTemplate::Project {
                    at: close_at,
                    projection: combined
                        .iter()
                        .enumerate()
                        .map(|(column, (name, _))| ReadProjectionTemplate {
                            name: name.text.to_owned(),
                            value: ReadValueTemplate::Column(column),
                        })
                        .collect(),
                    quantifier: crate::GraphSetQuantifier::All,
                }]
            } else {
                parser.write_row_pipeline(combined)?
            };
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
            output.first.owned = Some(Box::new((joined_columns, joined_types)));
            (Some(output), continuation, true)
        };
        let mut read_keys: Vec<_> = slots
            .iter()
            .filter_map(|slot| match slot.bind() {
                crate::GraphElementKey::Property(key) => Some(key),
                crate::GraphElementKey::Unbound | crate::GraphElementKey::Missing => None,
            })
            .collect();
        read_keys.extend(arm_read_keys);
        Ok((
            PreparedGraphCallText {
                input,
                output,
                arms,
                quantifier,
                outer_columns,
                outer_types,
                read_keys,
                returning,
                exports: exports.map(|exports| {
                    exports
                        .into_iter()
                        .map(|(name, kind)| (name.text.to_owned(), kind))
                        .unzip()
                }),
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
                if ["MATCH", "OPTIONAL", "WITH", "UNWIND", "CALL", "CREATE", "INSERT", "MERGE", "SET", "REMOVE", "DELETE", "DETACH", "FOREACH", "RETURN"].iter().any(|clause| word.eq_ignore_ascii_case(clause))));
        }
    }

    fn unit_call_body_arms(
        &self,
        outer_depth: usize,
    ) -> Result<(Vec<CallArmRange>, Option<crate::GraphSetQuantifier>, usize), GraphPatternTextError>
    {
        let mut lexer = Lexer {
            text: self.lexer.text,
            at: self.current.at,
            tokens: 0,
        };
        let mut brackets = Vec::new();
        let mut scope_starts = std::collections::BTreeSet::new();
        let mut scopes = outer_depth + 1;
        let mut previous = None;
        let mut before_previous = None;
        let mut previous_delimited = false;
        let mut nonempty = false;
        let mut start = self.current.at;
        let mut arms = Vec::new();
        let mut union_quantifier = None;
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
                    arms.push(CallArmRange {
                        start,
                        end: token.at,
                    });
                    return Ok((arms, union_quantifier, token.at));
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
                        && word.eq_ignore_ascii_case("UNION")
                        && PreparedGraphWriteChainText::call_union_separator(
                            token,
                            previous,
                            before_previous,
                            previous_delimited,
                            &lexer,
                        )? =>
                {
                    if !nonempty {
                        return Err(expected(token.at, "a nonempty UNION arm"));
                    }
                    if arms.len() + 1 >= crate::MAX_GRAPH_SET_DEPTH {
                        return Err(expected(token.at, "bounded CALL UNION arm count"));
                    }
                    let next = lexer.clone().next()?;
                    let quantifier = match next.kind {
                        TokenKind::Word(word) if word.eq_ignore_ascii_case("ALL") => {
                            lexer.next()?;
                            crate::GraphSetQuantifier::All
                        }
                        TokenKind::Word(word) if word.eq_ignore_ascii_case("DISTINCT") => {
                            lexer.next()?;
                            crate::GraphSetQuantifier::Distinct
                        }
                        _ => crate::GraphSetQuantifier::Distinct,
                    };
                    if union_quantifier.is_some_and(|previous| previous != quantifier) {
                        return Err(expected(token.at, "one quantifier across CALL UNION arms"));
                    }
                    union_quantifier = Some(quantifier);
                    arms.push(CallArmRange {
                        start,
                        end: token.at,
                    });
                    start = lexer.clone().next()?.at;
                    nonempty = false;
                    previous = None;
                    before_previous = None;
                    previous_delimited = false;
                    continue;
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
            before_previous = previous;
            previous = Some(token.kind);
            previous_delimited = lexer.text.as_bytes().get(token.at) == Some(&b'`');
        }
    }
}

impl PreparedGraphWriteChainText {
    /// As in the native set framer, a separator introduces a branch head.
    /// UNION remains an identifier in operand, alias and property positions.
    /// Empty trailing/middle arms still count as separators so the structural
    /// scan refuses them before any catalog resolution. The native compiler
    /// subsequently validates each complete expression and clause sequence.
    fn call_union_separator(
        token: Token<'_>,
        previous: Option<TokenKind<'_>>,
        before_previous: Option<TokenKind<'_>>,
        previous_delimited: bool,
        lexer: &Lexer<'_>,
    ) -> Result<bool, GraphPatternTextError> {
        if lexer.text.as_bytes().get(token.at) == Some(&b'`') {
            return Ok(false);
        }
        let after_operand_prefix = matches!(
            before_previous,
            Some(TokenKind::Punct(
                b',' | b'+' | b'-' | b'*' | b'/' | b'%' | b'=' | b'<' | b'>' | b'!' | b'|' | b'&'
            ))
        ) || matches!(before_previous, Some(TokenKind::Word(word)) if [
            "RETURN", "WITH", "UNWIND", "WHERE", "BY", "SKIP", "LIMIT", "ALL", "DISTINCT",
            "AND", "OR", "XOR", "NOT", "IN", "IS", "WHEN", "THEN", "ELSE", "CASE",
            "CONTAINS", "STARTS", "ENDS", "SET", "REMOVE", "DELETE", "DETACH",
        ].iter().any(|prefix| word.eq_ignore_ascii_case(prefix)));
        // A nonreserved word such as DELETE can itself finish the preceding
        // expression (`RETURN delete UNION ...`). NOT and CASE still start
        // native prefix expressions, so `RETURN NOT union` keeps its operand.
        let completed_keyword_operand = after_operand_prefix
            && matches!(previous, Some(TokenKind::Word(word))
                if !crate::graph_text::RESERVED_NAMES.iter().any(|reserved| word.eq_ignore_ascii_case(reserved))
                    && !word.eq_ignore_ascii_case("NOT")
                    && !word.eq_ignore_ascii_case("CASE"));
        let previous_is_name = previous_delimited
            || completed_keyword_operand
            || matches!(before_previous, Some(TokenKind::Punct(b'.' | b':')))
            || matches!(before_previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
        if matches!(
            previous,
            Some(TokenKind::Punct(
                b'.' | b':'
                    | b','
                    | b'+'
                    | b'-'
                    | b'/'
                    | b'%'
                    | b'='
                    | b'<'
                    | b'>'
                    | b'!'
                    | b'|'
                    | b'&'
            ))
        ) || (!previous_is_name
            && matches!(previous, Some(TokenKind::Word(word)) if [
                "AS", "RETURN", "WITH", "UNWIND", "WHERE", "BY", "SKIP", "LIMIT", "ALL",
                "DISTINCT", "AND", "OR", "XOR", "NOT", "IN", "IS", "WHEN", "THEN", "ELSE",
                "CASE", "CONTAINS", "STARTS", "ENDS", "ON", "SET", "REMOVE", "DELETE",
                "DETACH",
            ].iter().any(|prefix| word.eq_ignore_ascii_case(prefix))))
        {
            return Ok(false);
        }
        // A standalone projection wildcard completes an expression; the
        // multiplication operator still requires its right-hand operand.
        if matches!(previous, Some(TokenKind::Punct(b'*')))
            && !matches!(before_previous, Some(TokenKind::Punct(b'.')))
            && !matches!(before_previous, Some(TokenKind::Word(word))
                if ["RETURN", "WITH", "ALL", "DISTINCT"].iter().any(|prefix| word.eq_ignore_ascii_case(prefix)))
        {
            return Ok(false);
        }
        let mut next = lexer.clone();
        let mut token = script::next_script_token(&mut next)?;
        if matches!(token.kind, TokenKind::Word(word)
            if word.eq_ignore_ascii_case("ALL") || word.eq_ignore_ascii_case("DISTINCT"))
        {
            token = script::next_script_token(&mut next)?;
        }
        Ok(
            matches!(token.kind, TokenKind::End | TokenKind::Punct(b'}' | b';'))
                || matches!(token.kind, TokenKind::Word(word) if [
                "UNION", "RETURN", "WITH", "UNWIND", "MATCH", "OPTIONAL", "CALL", "CREATE",
                "INSERT", "MERGE", "SET", "REMOVE", "DELETE", "DETACH", "FOREACH",
            ].iter().any(|clause| word.eq_ignore_ascii_case(clause))),
        )
    }

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
        let mut before_previous = None;
        let mut previous_delimited = false;
        // A RETURN's expressions may use write words as ordinary identifiers
        // (`RETURN delete + 1`, for example). Its branch cannot write again,
        // but a sibling UNION branch or enclosing CALL still can.
        let mut returning_at = None;
        let mut previous_case_expression = false;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            let after_case_expression = previous_case_expression;
            // CASE is also a legal alias/schema name. Only its expression
            // prefix prevents the next DELETE word from starting a clause.
            previous_case_expression = matches!(token.kind, TokenKind::Word(word)
                if word.eq_ignore_ascii_case("CASE"))
                && !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                && !matches!(previous, Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("AS"));
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
                    returning_at = returning_at.filter(|&depth| depth <= brackets.len());
                }
                TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("UNION")
                        && returning_at == Some(brackets.len())
                        && brackets.iter().all(|&open| open == b'{') =>
                {
                    // A completed expression may itself be named ON, STARTS
                    // or ENDS. Separator framing is independent of the guard
                    // that keeps those words from introducing write clauses.
                    if Self::call_union_separator(
                        token,
                        previous,
                        before_previous,
                        previous_delimited,
                        &lexer,
                    )? {
                        returning_at = None;
                    }
                }
                TokenKind::Word(word)
                    if brackets.iter().all(|&open| open == b'{')
                        && !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["AS", "ON", "STARTS", "ENDS"].iter().any(|not| prior.eq_ignore_ascii_case(not))) =>
                {
                    if returning_at.is_some() {
                        before_previous = previous;
                        previous = Some(token.kind);
                        previous_delimited = lexer.text.as_bytes().get(token.at) == Some(&b'`');
                        continue;
                    }
                    if word.eq_ignore_ascii_case("RETURN") {
                        returning_at = Some(brackets.len());
                        before_previous = previous;
                        previous = Some(token.kind);
                        previous_delimited = lexer.text.as_bytes().get(token.at) == Some(&b'`');
                        continue;
                    }
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
                        && !after_case_expression
                        && !matches!(previous, Some(TokenKind::Word(prior))
                            if ["ALL", "DISTINCT"].iter().any(|prefix| prior.eq_ignore_ascii_case(prefix)))
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
            before_previous = previous;
            previous = Some(token.kind);
            previous_delimited = lexer.text.as_bytes().get(token.at) == Some(&b'`');
        }
    }
}
