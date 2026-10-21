//! Computed ORDER BY is projection, ranking and private-column removal over
//! the existing native scalar and relational owners. No graph is read twice.

use super::*;
use crate::set_text::ReadOrderProjection;

impl crate::set_text::CompoundOrderTemplate {
    pub(crate) fn prepare<'a>(
        text: &'a str,
        declarations: &[(&str, GqlParameterType)],
        columns: &'a [String],
        types: &[GraphSetColumnType],
    ) -> Result<Self, GraphSetTextError> {
        let mut parser = Parser::new_with_parameter_types(text, declarations)?;
        let schema: Vec<_> = columns
            .iter()
            .zip(types)
            .map(|(name, kind)| {
                (
                    Name {
                        text: name.as_str(),
                        at: 0,
                    },
                    *kind,
                )
            })
            .collect();
        let stage = parser
            .row_expression_page(&schema)?
            .ok_or_else(|| expected(parser.current.at, "ORDER BY over set output columns"))?;
        if !matches!(parser.current.kind, TokenKind::End) {
            return Err(expected(parser.current.at, "end of compound ordering"));
        }
        // Declarations may be used by another arm or enclosing page. The
        // compound owner performs the one complete unused/type check after
        // gathering actual uses from every leaf and page. Never validate a
        // partial declaration map as though it were the whole statement.
        Ok(Self {
            stage,
            parameters: parser.syntax.parameters,
            parameter_offsets: parser.syntax.parameter_offsets,
        })
    }

    pub(crate) fn bind(
        &self,
        input: PreparedGraphSet,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        // Signed client integers for SKIP/LIMIT parameters (fgdb-3dnpu).
        let admitted = arguments.admit_unsigned(&self.parameters);
        let arguments: &GqlParameters = &admitted;
        let mut values = Vec::with_capacity(self.parameters.len());
        for (spec, &at) in self.parameters.iter().zip(&self.parameter_offsets) {
            let value = arguments
                .get(&spec.name)
                .ok_or_else(|| error(at, GraphPatternTextErrorKind::MissingParameter))?;
            if !spec.parameter_type.accepts(value.parameter_type()) {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::ParameterTypeMismatch {
                        expected: spec.parameter_type,
                        found: value.parameter_type(),
                    },
                )
                .into());
            }
            values.push(value);
        }
        // Reuse the native page lowering, including its ordered projection
        // that removes private keys. Set arithmetic has already completed;
        // adding a key therefore cannot change UNION/DISTINCT multiplicity.
        // Execution still uses the same cumulative budget and checkpoints.
        bind_stages(input, std::slice::from_ref(&self.stage), &values)
    }
}

struct ExpressionPage {
    at: usize,
    keys: Vec<(ReadValueTemplate, GraphValueOrder)>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
}

impl ExpressionPage {
    fn finish(self, width: usize, visible: usize) -> Result<ReadStageTemplate, GraphSetTextError> {
        let mut values = Vec::<ReadValueTemplate>::new();
        let mut definitions = Vec::<Vec<u8>>::new();
        let mut order = Vec::<GraphValueOrder>::new();
        for (value, mut key) in self.keys {
            key.column = if let ReadValueTemplate::Column(column) = value {
                column
            } else {
                let mut bytes = Vec::new();
                value.append_template_transcript(&mut bytes);
                match definitions.iter().position(|previous| *previous == bytes) {
                    Some(column) => width + column,
                    None => {
                        let column = width + values.len();
                        if column >= MAX_PATTERN_VERTICES {
                            return Err(expected(self.at, "bounded ORDER BY evaluation columns"));
                        }
                        values.push(value);
                        definitions.push(bytes);
                        column
                    }
                }
            };
            // A later key on a column already sorted on cannot change the
            // order: rows equal there tie on both. openCypher keeps the first
            // direction (TCK WithOrderBy3 [7], `a + 2 ASC, a + 2 DESC`).
            if order.iter().any(|previous| previous.column == key.column) {
                continue;
            }
            order.push(key);
        }
        let computed = (!values.is_empty() || visible != width)
            .then_some(ReadOrderProjection { values, visible });
        Ok(ReadStageTemplate::Page {
            at: self.at,
            order,
            offset: self.offset,
            count: self.count,
            computed,
        })
    }
}

impl<'a> Parser<'a> {
    fn expression_page(
        &mut self,
        mut value: impl FnMut(&mut Self) -> Result<ReadValueTemplate, GraphSetTextError>,
    ) -> Result<Option<ExpressionPage>, GraphSetTextError> {
        let at = self.current.at;
        let mut present = false;
        let mut keys = Vec::new();
        if self.take_word("ORDER")? {
            present = true;
            self.word("BY")?;
            loop {
                self.capacity(
                    keys.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                let value = value(self)?;
                let descending = self.take_word("DESC")? || self.take_word("DESCENDING")?;
                if !descending && !self.take_word("ASC")? {
                    self.take_word("ASCENDING")?;
                }
                let nulls_first = if self.take_word("NULLS")? {
                    if self.take_word("FIRST")? {
                        true
                    } else {
                        self.word("LAST")?;
                        false
                    }
                } else {
                    false
                };
                keys.push((
                    value,
                    GraphValueOrder {
                        column: 0,
                        descending,
                        nulls_first,
                    },
                ));
                if !self.take(b',')? {
                    break;
                }
            }
        }
        let offset = if self.take_word("SKIP")? {
            present = true;
            self.row_page_number()?
        } else {
            ReadPageNumber::Literal(0)
        };
        let count = if self.take_word("LIMIT")? {
            present = true;
            Some(self.row_page_number()?)
        } else {
            None
        };
        Ok(present.then_some(ExpressionPage {
            at,
            keys,
            offset,
            count,
        }))
    }

    pub(super) fn row_expression_page(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Option<ReadStageTemplate>, GraphSetTextError> {
        self.expression_page(|parser| parser.read_row_value(schema, 0))?
            .map(|page| page.finish(schema.len(), schema.len()))
            .transpose()
    }

    /// ORDER BY sees returned aliases first, then the non-DISTINCT input
    /// scope. Retained input cells are private and removed after ranking.
    pub(super) fn return_expression_page(
        &mut self,
        schema: &[(Name<'a>, GraphSetColumnType)],
        projection: &mut Vec<ReadProjectionTemplate>,
        distinct: bool,
    ) -> Result<Option<ReadStageTemplate>, GraphSetTextError> {
        let visible = projection.len();
        let page = self.expression_page(|parser| {
            parser.read_resolved_value(
                &mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else {
                        return Ok(None);
                    };
                    let next = parser.lexer.clone().next()?;
                    if matches!(next.kind, TokenKind::Punct(b'(')) {
                        return Ok(None);
                    }
                    if !matches!(next.kind, TokenKind::Punct(b'.'))
                        && let Some(column) = projection[..visible]
                            .iter()
                            .position(|column| column.name == word)
                    {
                        parser.advance()?;
                        return Ok(Some(column));
                    }
                    let at = parser.current.at;
                    let input = if let Some(column) = parser.boundary_read(schema.len())? {
                        column
                    } else {
                        let Some(column) = schema[..parser.visible_width(schema)]
                            .iter()
                            .position(|(name, _)| name.text == word)
                        else {
                            return Ok(None);
                        };
                        parser.advance()?;
                        column
                    };
                    if let Some(column) = projection.iter().position(|column| {
                    matches!(column.value, ReadValueTemplate::Column(index) if index == input)
                }) {
                    return Ok(Some(column));
                }
                    if distinct {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "projected ORDER BY expression or alias under DISTINCT",
                            ),
                        ));
                    }
                    parser.capacity(
                        projection.len(),
                        MAX_PATTERN_VERTICES,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    let column = projection.len();
                    let mut suffix = column;
                    let name = loop {
                        let name = format!("__fg_order_input_{suffix}");
                        if projection.iter().all(|prior| prior.name != name) {
                            break name;
                        }
                        suffix += 1;
                    };
                    projection.push(ReadProjectionTemplate {
                        name,
                        value: ReadValueTemplate::Column(input),
                    });
                    Ok(Some(column))
                },
                0,
            )
        })?;
        page.map(|page| page.finish(projection.len(), visible))
            .transpose()
    }

    /// Graph leaves retain sort-only property/function reads in the SAME
    /// native source projection. Aliases name the already computed output;
    /// every scalar operation still compiles through read_resolved_value.
    pub(in crate::graph_text::scoped::mutation::return_projection) fn graph_expression_page(
        &mut self,
        head: &mut GraphProjectionHead<'a>,
        incoming: &[(Name<'a>, GraphSetColumnType)],
        optional: bool,
    ) -> Result<Option<ReadStageTemplate>, GraphSetTextError> {
        let visible = head.outputs.len();
        let distinct = self.syntax.distinct;
        let offset = incoming.len();
        let page = self.expression_page(|parser| {
            parser.read_resolved_value(
                &mut |parser| {
                    let TokenKind::Word(word) = parser.current.kind else {
                        return Ok(None);
                    };
                    let next = parser.lexer.clone().next()?;
                    if !matches!(next.kind, TokenKind::Punct(b'.' | b'('))
                        && let Some(column) = head.outputs[..visible]
                            .iter()
                            .position(|(name, _)| name.text == word)
                    {
                        parser.advance()?;
                        return Ok(Some(column));
                    }
                    let at = parser.current.at;
                    let graph_function = matches!(next.kind, TokenKind::Punct(b'('))
                        && [
                            "labels",
                            "type",
                            "length",
                            "path_length",
                            "nodes",
                            "edges",
                            "relationships",
                        ]
                        .iter()
                        .any(|name| word.eq_ignore_ascii_case(name));
                    let input = if graph_function {
                        if optional {
                            let mut lookahead = parser.lexer.clone();
                            lookahead.next()?;
                            let argument = lookahead.next()?;
                            if matches!(argument.kind, TokenKind::Word(name)
                            if incoming.iter().any(|(binding, _)| binding.text == name))
                            {
                                return Err(error(
                                    argument.at,
                                    GraphPatternTextErrorKind::Expected(
                                        "project carried graph functions before OPTIONAL MATCH",
                                    ),
                                ));
                            }
                        }
                        let Operand::Column(input) = parser.mutation_operand(&mut head.inputs)?
                        else {
                            unreachable!("graph functions resolve a source column");
                        };
                        offset + input
                    } else if !matches!(next.kind, TokenKind::Punct(b'('))
                        && let Some(column) =
                            incoming.iter().position(|(name, _)| name.text == word)
                    {
                        parser.advance()?;
                        column
                    } else {
                        // `RETURN r AS rel ORDER BY rel.id`: a property of an
                        // alias that projects a graph element of this part
                        // bare is that element's property (With3 [1]). A
                        // name that is itself a binding keeps meaning it.
                        let bound = parser
                            .visible_graph_bindings()
                            .any(|name| name.text == word);
                        let aliased = if !bound && matches!(next.kind, TokenKind::Punct(b'.')) {
                            head.outputs[..visible].iter().find_map(|(name, value)| {
                                let ReadValueTemplate::Column(index) = value else {
                                    return None;
                                };
                                if name.text != word {
                                    return None;
                                }
                                let Some(source) = index.checked_sub(offset) else {
                                    // A carried element this part matched
                                    // again by identity (`WITH r MATCH ()-[r]->()`).
                                    let (carried, kind) = incoming[*index];
                                    return matches!(
                                        kind,
                                        GraphSetColumnType::Vertex | GraphSetColumnType::Edge
                                    )
                                    .then(|| {
                                        parser
                                            .visible_graph_bindings()
                                            .find(|binding| binding.text == carried.text)
                                    })
                                    .flatten();
                                };
                                let input = head.inputs[source];
                                (input.property.is_none()
                                    && matches!(input.path, None | Some(GraphPathFunction::Edge)))
                                .then_some(input.variable)
                            })
                        } else {
                            None
                        };
                        if matches!(next.kind, TokenKind::Punct(b'('))
                            || (!bound && aliased.is_none())
                        {
                            return Ok(None);
                        }
                        let variable = match aliased {
                            Some(variable) => {
                                parser.advance()?;
                                variable
                            }
                            None => parser.any_variable()?,
                        };
                        let property = if parser.take(b'.')? {
                            parser.require_property_variable(variable)?;
                            Some(parser.schema_name()?)
                        } else {
                            None
                        };
                        offset + parser.mutation_projection(&mut head.inputs, variable, property)?
                    };
                    // A repeated carried field ranks the already projected
                    // value. In particular DISTINCT a.p ORDER BY a.p must
                    // not append the a identity: that would split duplicates.
                    if input < offset && parser.is_punct(b'.') {
                        let key = parser.lexer.clone().next()?;
                        if let TokenKind::Word(key) | TokenKind::DelimitedKeyword(key) = key.kind
                            && let Some(column) = head.outputs[..visible].iter().position(|(_, value)| {
                                matches!(value, ReadValueTemplate::MapGet { map, key: known, .. }
                                    if matches!(map.as_ref(), ReadValueTemplate::Column(index) if *index == input)
                                        && known.as_ref() == key)
                            })
                        {
                            parser.punct(b'.', ".")?;
                            parser.map_key()?;
                            return Ok(Some(column));
                        }
                    }
                    if let Some(column) = head.outputs.iter().position(|(_, value)| {
                    matches!(value, ReadValueTemplate::Column(index) if *index == input)
                }) {
                    return Ok(Some(column));
                }
                    // A property/function of a returned graph element is fully
                    // determined by it. It cannot distinguish duplicate outputs.
                    let determined = input.checked_sub(offset).is_some_and(|source| {
                        let source = head.inputs[source];
                        head.outputs[..visible].iter().any(|(_, value)| {
                            let ReadValueTemplate::Column(index) = value else {
                                return false;
                            };
                            let Some(index) = index.checked_sub(offset) else {
                                // Multipart MATCH correlates each carried root
                                // vertex by identity before this projection.
                                // Returning that incoming vertex therefore
                                // determines reads from its matched graph slot.
                                return incoming.get(*index).is_some_and(|(name, kind)| {
                                    *kind == GraphSetColumnType::Vertex
                                        && name.text == source.variable.text
                                });
                            };
                            let returned = head.inputs[index];
                            returned.variable.text == source.variable.text
                                && returned.property.is_none()
                                && matches!(
                                    returned.path,
                                    None | Some(GraphPathFunction::Edge | GraphPathFunction::Value)
                                )
                        })
                    });
                    if distinct && !determined {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "projected ORDER BY expression or alias under DISTINCT",
                            ),
                        ));
                    }
                    parser.capacity(
                        head.outputs.len(),
                        MAX_PATTERN_VERTICES,
                        crate::algebra::PatternLimitDimension::Columns,
                    )?;
                    let Some(&name) = BOUNDARY_READ_NAMES
                        .iter()
                        .find(|name| head.outputs.iter().all(|(prior, _)| prior.text != **name))
                    else {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "at most 16 private graph ORDER BY inputs",
                            ),
                        ));
                    };
                    let column = head.outputs.len();
                    head.outputs
                        .push((Name { text: name, at }, ReadValueTemplate::Column(input)));
                    Ok(Some(column))
                },
                0,
            )
        })?;
        page.map(|page| page.finish(head.outputs.len(), visible))
            .transpose()
    }
}
