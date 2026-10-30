//! FOREACH prepares native row sources and a checked local write chain. The
//! iteration itself belongs to the shared scheduler, inside the host's one
//! governed transaction; parsing never runs a statement or rewrites its text.

use super::*;
use crate::graph_text::parameters::UnresolvedGraphText;
use crate::graph_text::scoped::mutation::return_projection::pipeline::multipart::UnresolvedReadInput;
use crate::set_text::ReadProjectionTemplate;
use crate::write_chain::{ChainTextStep, PreparedGraphForeachText};
use crate::{GraphWriteChainTextError, PreparedGraphForeach, PreparedGraphWriteChainText};

impl PreparedGraphForeachText {
    pub(crate) fn prepare_step<'a>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut impl GraphSymbolResolver,
        from: Option<ChainContinuation<'a>>,
    ) -> Result<(Self, Option<ChainContinuation<'a>>), GraphWriteChainTextError> {
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
        parser.word("FOREACH")?;
        parser.punct(b'(', "(")?;
        let variable = parser.name()?;
        parser.word("IN")?;

        // A carried scalar alias keeps its value, but hidden element-property
        // columns belong to the preceding clause. Both the collection and
        // each body iteration read those properties from the current overlay.
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
        // An undeclared parameter in collection position has the established
        // list parameter shape. Explicitly declared scalar/map values retain
        // their type and use FOREACH's singleton coercion at execution time.
        if let TokenKind::Parameter(name) = parser.current.kind {
            let inferred = parser
                .syntax
                .parameters
                .iter()
                .find(|spec| spec.name == name)
                .map_or(GqlParameterType::List, |spec| spec.parameter_type);
            parser
                .parameter_types
                .entry(name.to_owned())
                .or_insert(inferred);
        }
        let value = parser.read_row_value(&outer, 0)?;
        parser.punct(b'|', "|")?;
        let body_at = parser.current.at;
        let close_at = parser.foreach_body_end()?;
        let outer_columns: Vec<_> = outer.iter().map(|(name, _)| name.text.to_owned()).collect();
        let outer_types: Vec<_> = outer.iter().map(|(_, kind)| *kind).collect();
        let imports: Vec<_> = outer
            .iter()
            .enumerate()
            .filter_map(|(column, (name, _))| (name.text != variable.text).then_some(column))
            .collect();
        if imports.len() >= crate::algebra::MAX_PATTERN_VERTICES {
            return Err(expected(at, "bounded FOREACH local row width").into());
        }

        // The outer input contains no collection expression. The scheduler
        // evaluates this separate singleton projection immediately before
        // each outer occurrence, so earlier iterations can change its list.
        let mut projection: Vec<_> = outer
            .iter()
            .enumerate()
            .map(|(column, (name, _))| ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(column),
            })
            .collect();
        let private_name = (0..=outer.len())
            .map(|suffix| format!("__fg_foreach_collection_{suffix}"))
            .find(|name| !outer_columns.contains(name))
            .expect("one more private candidate than visible names");
        projection.push(ReadProjectionTemplate {
            name: private_name,
            value,
        });
        let mut collection = UnresolvedReadInput::from_first(UnresolvedGraphText {
            statement,
            syntax: parser.take_part_syntax()?,
            projection: None,
            pipeline: vec![ReadStageTemplate::Project {
                at,
                projection,
                quantifier: crate::GraphSetQuantifier::All,
            }],
            singleton: true,
            leading: Vec::new(),
            leading_types: Vec::new(),
            correlations: Vec::new(),
            row_fields: Vec::new(),
        });
        // The prefix and collection share parameter indices established by
        // the one native parser, including parameters used in earlier parts.
        source.share_parameters(&parser.syntax.parameters, &parser.syntax.parameter_offsets);
        collection.share_parameters(&parser.syntax.parameters, &parser.syntax.parameter_offsets);
        let mut slots = source.element_key_slots();
        slots.extend(collection.element_key_slots());
        let mut input = source.resolve(resolve)?;
        input.first.owned = owned;
        let mut collection = collection.resolve(resolve)?;
        collection.first.owned = Some(Box::new((outer_columns.clone(), outer_types.clone())));

        let mut local: Vec<_> = imports.iter().map(|&column| outer[column]).collect();
        local.push((variable, GraphSetColumnType::Any));
        let mut next = Some(ChainContinuation {
            at: body_at,
            row: local,
            boundary: None,
            parameters: parser.syntax.parameters.clone(),
        });
        let body_text = &statement[..close_at];
        let mut steps = Vec::new();
        while let Some(from) = next {
            if steps.len() >= crate::MAX_GRAPH_SET_DEPTH {
                return Err(expected(from.at, "bounded FOREACH body step count").into());
            }
            let mut lexer = Lexer {
                text: body_text,
                at: from.at,
                tokens: 0,
            };
            match lexer.next()?.kind {
                TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("CREATE")
                        || word.eq_ignore_ascii_case("INSERT") =>
                {
                    let (step, returning, after) = PreparedGraphInsertText::prepare_chained(
                        body_text,
                        relation,
                        declarations,
                        crate::graph_text::LentResolver(resolve),
                        from,
                        // The insertion collector has no ambient element
                        // reader. Its native input projects CREATE fields
                        // from this iteration's staged graph before IDs.
                        true,
                    )?;
                    steps.push(ChainTextStep::Insert(Box::new((step, returning))));
                    next = after;
                }
                TokenKind::Word(word)
                    if word.eq_ignore_ascii_case("SET") || word.eq_ignore_ascii_case("REMOVE") =>
                {
                    let (step, returning, after) =
                        crate::mutation_text::PreparedGraphMutationRowsText::prepare_chained(
                            body_text,
                            relation,
                            declarations,
                            resolve,
                            from,
                            false,
                        )?;
                    steps.push(ChainTextStep::Mutation(Box::new((step, returning))));
                    next = after;
                }
                _ => {
                    return Err(
                        expected(from.at, "CREATE, INSERT, SET or REMOVE in FOREACH").into(),
                    );
                }
            }
        }
        let body = PreparedGraphWriteChainText {
            statement: body_text.to_owned(),
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
        // A later clause keeps types established even by a parameter used
        // only inside the local body. Its native parameter indices are local
        // to this tail, just as each ordinary write step's indices are.
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
        parser.punct(b')', ")")?;
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
            Self {
                input,
                collection,
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

    pub(crate) fn parameter_schema(&self) -> Vec<GqlParameterSpec> {
        let mut schema: Vec<GqlParameterSpec> = Vec::new();
        let body_schema = self.body.parameter_schema();
        for spec in self
            .input
            .parameter_schema()
            .iter()
            .chain(self.collection.parameter_schema())
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
    ) -> Result<PreparedGraphForeach, GraphWriteChainTextError> {
        if let Some((_, types)) = self.input.first.owned.as_deref()
            && expected_input != Some(types.as_slice())
        {
            return Err(
                expected(self.input.first.return_at, "the previous write's WITH row").into(),
            );
        }
        let own = |schema: &[GqlParameterSpec]| -> Result<GqlParameters, GraphPatternTextError> {
            let mut own = GqlParameters::new();
            for spec in schema {
                if let Some(value) = arguments.get(&spec.name) {
                    own.insert(spec.name.clone(), value).map_err(|_| {
                        error(
                            self.input.first.return_at,
                            GraphPatternTextErrorKind::UnexpectedArguments,
                        )
                    })?;
                }
            }
            Ok(own)
        };
        let input = self
            .input
            .bind_parameters(&own(self.input.parameter_schema())?)?;
        let collection = self
            .collection
            .bind_parameters(&own(self.collection.parameter_schema())?)?;
        let mut types: Vec<_> = self
            .imports
            .iter()
            .map(|&column| self.outer_types[column])
            .collect();
        types.push(GraphSetColumnType::Any);
        let body = self
            .body
            .bind_parameters_with_input(&own(&self.body.parameter_schema())?, Some(types))?;
        let output = self
            .output
            .as_ref()
            .map(|output| {
                Ok::<_, GraphWriteChainTextError>(
                    output.bind_parameters(&own(output.parameter_schema())?)?,
                )
            })
            .transpose()?;
        Ok(PreparedGraphForeach {
            input,
            collection,
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

impl Parser<'_> {
    /// Bound the local write-only body before preparing any statement. All
    /// unsupported families are rejected even when execution would see [] or
    /// NULL, and quoted strings/property names never become clause markers.
    fn foreach_body_end(&self) -> Result<usize, GraphPatternTextError> {
        let mut lexer = Lexer {
            text: self.lexer.text,
            at: self.current.at,
            tokens: 0,
        };
        let mut brackets = Vec::new();
        let mut previous = None;
        let mut clauses = 0;
        loop {
            let token = lexer.next()?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => {
                    return Err(error(
                        token.at,
                        GraphPatternTextErrorKind::Expected("closing FOREACH parenthesis"),
                    ));
                }
                TokenKind::Punct(b')') if brackets.is_empty() => {
                    if clauses == 0 {
                        return Err(error(
                            token.at,
                            GraphPatternTextErrorKind::Expected("nonempty FOREACH write body"),
                        ));
                    }
                    return Ok(token.at);
                }
                TokenKind::Punct(open @ (b'(' | b'[' | b'{')) => brackets.push(open),
                TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                    let expected = match close {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if brackets.pop() != Some(expected) {
                        return Err(error(
                            token.at,
                            GraphPatternTextErrorKind::Expected("balanced FOREACH body"),
                        ));
                    }
                }
                TokenKind::Word(word)
                    if brackets.is_empty()
                        && !matches!(previous, Some(TokenKind::Punct(b'.' | b':')))
                        && !matches!(previous, Some(TokenKind::Word(prior))
                        if ["AS", "STARTS", "ENDS"].iter().any(|word| prior.eq_ignore_ascii_case(word))) =>
                {
                    if ["CREATE", "INSERT", "SET", "REMOVE"]
                        .iter()
                        .any(|clause| word.eq_ignore_ascii_case(clause))
                    {
                        clauses += 1;
                    } else if [
                        "FOREACH", "MERGE", "DELETE", "DETACH", "MATCH", "OPTIONAL", "WITH",
                        "RETURN", "UNWIND", "CALL", "UNION",
                    ]
                    .iter()
                    .any(|clause| word.eq_ignore_ascii_case(clause))
                    {
                        return Err(error(
                            token.at,
                            GraphPatternTextErrorKind::Expected(
                                "CREATE, INSERT, SET or REMOVE in FOREACH",
                            ),
                        ));
                    }
                }
                _ => {}
            }
            previous = Some(token.kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::{GraphValue, GraphValueRow, PreparedGraphPattern};
    use crate::{
        GqlListParameter, GqlParameterValue, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
        GraphIntegerError, GraphWriteChainCursor, GraphWriteChainStep, GraphWriteChainTask,
    };

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "F") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            _ => None,
        }
    }

    #[test]
    fn prepares_native_sources_private_bodies_and_following_clauses() {
        for text in [
            "FOREACH (x IN [1,2] | CREATE (:F {p:x}))",
            "FOREACH (x IN [] | CREATE (:F)) RETURN 1 AS value",
            "WITH 7 AS x FOREACH (x IN [1,2] | CREATE (local:F {p:x}) SET local.q=x REMOVE local.p) RETURN x",
            "MATCH (n:F) FOREACH (x IN [1,2,3] | SET n.p=n.p+x) RETURN n.p",
            "MATCH (n:F) FOREACH (x IN [1,2] | CREATE (:F {p:n.p+x})) RETURN n",
            "MATCH (n:F) WITH collect(n) AS nodes FOREACH (n IN nodes | SET n.p=1) RETURN size(nodes)",
            "CREATE (n:F {p:0}) FOREACH (x IN range(1,n.p+1) | SET n.p=n.p+1) RETURN n.p",
            "CREATE (n:F {p:0}) WITH n,n.p AS frozen FOREACH (x IN [1,2,3] | SET n.p=n.p+x) RETURN n.p,frozen",
            "FOREACH (x IN [1] | CREATE (:F {p:x})) MATCH (n:F) RETURN n.p",
            "FOREACH (x IN [1] | CREATE (:F {p:x})) WITH 2 AS y CREATE (:F {p:y}) RETURN y",
            "FOREACH (x IN [1] | CREATE (:F {p:x})) FOREACH (y IN [2] | CREATE (:F {p:y})) RETURN 3 AS value",
            "MATCH (n:F) FOREACH (x IN ['a'] | SET n.p=x STARTS WITH 'a', n.q=x ENDS WITH 'a')",
        ] {
            let prepared = PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols)
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
            prepared
                .bind_parameters(&GqlParameters::new())
                .unwrap_or_else(|error| panic!("{text}: {error:?}"));
        }
    }

    #[test]
    fn local_names_and_unsupported_bodies_refuse_even_when_the_collection_is_empty() {
        for text in [
            "FOREACH (x IN [] | CREATE (local:F)) RETURN local",
            "FOREACH (x IN [] | CREATE (:F)) RETURN x",
            "FOREACH (x IN [] | SET missing.p=1)",
            "FOREACH (x IN [] |)",
            "FOREACH (x IN [] | FOREACH (y IN [] | CREATE (:F)))",
            "FOREACH (x IN [] | MERGE (:F))",
            "MATCH (n:F) FOREACH (x IN [] | DELETE n)",
            "MATCH (n:F) FOREACH (x IN [] | DETACH DELETE n)",
            "FOREACH (x IN [] | CREATE (:F) WITH x CREATE (:F))",
            "FOREACH (x IN [] | MATCH (n:F) SET n.p=1)",
            "FOREACH (x IN [] | CREATE (:F) RETURN x)",
        ] {
            assert!(
                PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols).is_err(),
                "{text}"
            );
        }
    }

    #[test]
    fn parameters_keep_their_types_across_collection_body_and_tail() {
        let text = "FOREACH (x IN $xs | CREATE (:F {p:x+$step})) WITH $step AS delta CREATE (:F {p:delta}) RETURN delta";
        let template = PreparedGraphWriteChainText::prepare_with_parameter_types(
            text,
            RelationId(1),
            &[("step", GqlParameterType::Int64)],
            symbols,
        )
        .unwrap();
        let schema = template.parameter_schema();
        assert_eq!(schema.len(), 2);
        assert!(
            schema
                .iter()
                .any(|spec| spec.name == "xs" && spec.parameter_type == GqlParameterType::List)
        );
        assert!(
            schema
                .iter()
                .any(|spec| spec.name == "step" && spec.parameter_type == GqlParameterType::Int64)
        );
        let mut arguments = GqlParameters::new().with_int64("step", 3).unwrap();
        arguments
            .insert(
                "xs",
                GqlParameterValue::List(
                    GqlListParameter::new(vec![
                        GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(1)),
                        GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(2)),
                    ])
                    .unwrap(),
                ),
            )
            .unwrap();
        template.bind_parameters(&arguments).unwrap();

        // A prior native row stage may infer the parameter before the loop
        // sees collection position. FOREACH must preserve that scalar type
        // and apply its singleton rule instead of redeclaring a list.
        let reused = PreparedGraphWriteChainText::prepare(
            "WITH $x AS seed FOREACH (item IN $x | CREATE (:F {p:item})) RETURN seed",
            RelationId(1),
            symbols,
        )
        .unwrap();
        assert_eq!(reused.parameter_schema().len(), 1);
        reused
            .bind_parameters(&GqlParameters::new().with_int64("x", 7).unwrap())
            .unwrap();
    }

    fn no_patterns(
        _: &PreparedGraphPattern<GraphValueRow>,
        _: GqlQueryPolicy,
    ) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<(), ()>> {
        panic!("source-free FOREACH must not request graph patterns")
    }

    #[test]
    fn scheduler_preserves_outer_occurrences_and_keeps_body_rows_private() {
        let text =
            "UNWIND [[],NULL,[NULL],[1,1],7,{a:1}] AS xs FOREACH (x IN xs | CREATE (:F)) RETURN xs";
        let chain = PreparedGraphWriteChainText::prepare(text, RelationId(1), symbols)
            .unwrap()
            .bind_parameters(&GqlParameters::new())
            .unwrap();
        let mut cursor = GraphWriteChainCursor::new(&chain, true);
        let mut rows = None;
        let mut iterations = Vec::new();
        let mut collections = 0;
        let policy = GqlQueryPolicy::new(1_000, 1_000, 1_000_000, 1_000_000);
        while let Some(task) = cursor
            .next(&mut rows, &mut |_| Ok::<_, GraphIntegerError>(()))
            .unwrap()
        {
            match task {
                GraphWriteChainTask::ForeachInput { foreach } => {
                    rows = Some(
                        foreach
                            .execute_input_governed(rows.take(), policy, no_patterns, || Ok(()))
                            .unwrap()
                            .value,
                    );
                }
                GraphWriteChainTask::ForeachCollection { foreach } => {
                    collections += 1;
                    assert_eq!(rows.as_ref().unwrap().len(), 1);
                    rows = Some(
                        foreach
                            .execute_collection_governed(
                                rows.take(),
                                policy,
                                no_patterns,
                                || Ok(()),
                            )
                            .unwrap()
                            .value,
                    );
                }
                GraphWriteChainTask::Write {
                    step,
                    delivers,
                    continues,
                } => {
                    assert!(matches!(step, GraphWriteChainStep::Insert(_)));
                    assert!(!delivers);
                    assert!(!continues);
                    let local = rows.take().unwrap();
                    assert_eq!(local.len(), 1);
                    iterations.push(local[0].values().last().unwrap().clone());
                    // A body's physical output never determines outer rows.
                    rows = Some(Vec::new());
                }
                GraphWriteChainTask::ForeachOutput { foreach, delivers } => {
                    assert!(delivers);
                    rows = Some(
                        foreach
                            .execute_output_governed(rows.take(), policy, no_patterns, || Ok(()))
                            .unwrap()
                            .value,
                    );
                }
            }
        }
        assert_eq!(collections, 6);
        assert_eq!(rows.unwrap().len(), 6);
        assert_eq!(iterations.len(), 5);
        assert_eq!(
            iterations
                .iter()
                .filter(|value| **value == GraphValue::Scalar(fgdb_types::CanonicalScalar::Null))
                .count(),
            1
        );
        assert_eq!(
            iterations
                .iter()
                .filter(|value| **value == GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(1)))
                .count(),
            2
        );
    }
}
