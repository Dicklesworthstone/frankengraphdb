//! RETURN after SET/REMOVE/DETACH DELETE binds the statement's own selection
//! occurrences. A matched element's property is a source column plus the
//! mutation's proposal for that field; nothing is rescanned or re-matched.

mod combined;
mod row_maps;

use super::*;
use crate::mutation_text::{
    MutationReturnTemplate, PreparedGraphMutationQueryText, WriteReturnGroupTemplate,
};
use crate::set_text::{
    ReadPageNumber, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::{
    GraphMutationBinding, GraphSetColumnType, GraphSetProjection, GraphSetQuantifier,
    GraphSetTextError, PreparedGraphMutation, PreparedGraphMutationQuery,
};

#[derive(Clone, Copy)]
enum Binding<'a> {
    Input(usize),
    Property {
        target: usize,
        key: Name<'a>,
        current: usize,
    },
    Properties {
        target: usize,
        current: usize,
    },
    Labels {
        target: usize,
        current: usize,
    },
}
impl Binding<'_> {
    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Input(a), Self::Input(b)) => a == b,
            (
                Self::Property {
                    target: a, key: ak, ..
                },
                Self::Property {
                    target: b, key: bk, ..
                },
            ) => a == b && ak.text == bk.text,
            (Self::Properties { target: a, .. }, Self::Properties { target: b, .. }) => a == b,
            (Self::Labels { target: a, .. }, Self::Labels { target: b, .. }) => a == b,
            _ => false,
        }
    }
}

/// A map projection uses the same parsed source name for its NULL guard,
/// selected properties and wildcard. Write owners bind each read to their
/// final staged values; the shared grammar never synthesizes expression text.
#[derive(Clone, Copy)]
pub(super) enum ReturnMapInput<'a> {
    Identity,
    Guard,
    Property(Name<'a>),
    Properties,
}

/// Leaf resolution for one write statement's RETURN items: a leaf is a
/// column of the statement's private RETURN input row.
pub(super) trait ReturnLeaves<'a> {
    /// The binding column of the leaf at the current token, consuming it;
    /// `None` consumes nothing and leaves the token to the expression parser.
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError>;
    fn map_input(
        &mut self,
        _parser: &mut Parser<'a>,
        source: Name<'a>,
        _input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        Err(error(
            source.at,
            GraphPatternTextErrorKind::Expected("map projection over a readable write binding"),
        ))
    }
    /// Every binding column's type, in column order.
    fn types(&self) -> Vec<GraphSetColumnType>;
}

/// The parsed items of a non-star write RETURN.
pub(super) struct ReturnItems<'a> {
    pub projection: Vec<ReadProjectionTemplate>,
    pub grouping: Option<WriteReturnGroupTemplate>,
    pub output: Vec<(Name<'a>, GraphSetColumnType)>,
    /// The source spelling of every item, for ORDER BY.
    pub spellings: Vec<Name<'a>>,
}

impl<'a> Parser<'a> {
    /// The comma-separated items of a write RETURN. Output names follow a
    /// read RETURN: an alias, a leaf's own name, or the item's source text,
    /// with the source text replacing a name that would otherwise repeat.
    pub(super) fn write_return_items(
        &mut self,
        leaves: &mut impl ReturnLeaves<'a>,
    ) -> Result<ReturnItems<'a>, GraphSetTextError> {
        if self.write_items_have_combined_aggregate()? {
            return self.combined_write_return_items(leaves);
        }
        let mut items = ReturnItems {
            projection: Vec::new(),
            grouping: None,
            output: Vec::new(),
            spellings: Vec::new(),
        };
        let mut functions = Vec::new();
        loop {
            self.capacity(
                items.output.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let at = self.current.at;
            let aggregate = match self.current.kind {
                TokenKind::Word(word)
                    if matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) =>
                {
                    return_projection::with_aggregate::function(word)
                }
                _ => None,
            };
            let (value, aggregate) = if let Some(mut function) = aggregate {
                use crate::GraphAggregateFunction as F;
                self.name()?;
                self.punct(b'(', "(")?;
                let distinct = self.take_word("DISTINCT")?;
                if !distinct {
                    self.take_word("ALL")?;
                }
                let value = if self.take(b'*')? {
                    if function != F::Count || distinct {
                        return Err(error(
                            at,
                            GraphPatternTextErrorKind::Expected(
                                "COUNT(*) without DISTINCT, or one aggregate argument",
                            ),
                        )
                        .into());
                    }
                    function = F::CountRows;
                    ReadValueTemplate::Literal(
                        GqlScalarParameter::new(CanonicalScalar::Null).expect("canonical null"),
                    )
                } else {
                    // rand() in the argument is a non-constant expression.
                    let outer = core::mem::replace(&mut self.aggregate_argument, true);
                    let value = self.read_write_value(leaves, 0);
                    self.aggregate_argument = outer;
                    value?
                };
                self.punct(b')', "one aggregate argument")?;
                if matches!(function, F::SumInt | F::AverageInt)
                    && !matches!(
                        value.column_type(&leaves.types(), &self.syntax.parameters),
                        GraphSetColumnType::Scalar
                            | GraphSetColumnType::Stored
                            | GraphSetColumnType::Any
                    )
                {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected(
                            "a scalar argument for a numeric aggregate",
                        ),
                    )
                    .into());
                }
                if distinct {
                    function = match function {
                        F::Count => F::CountDistinct,
                        F::SumInt => F::SumIntDistinct,
                        F::AverageInt => F::AverageIntDistinct,
                        F::Collect => F::CollectDistinct,
                        other => other,
                    };
                }
                (value, Some(function))
            } else {
                (self.read_write_value(leaves, 0)?, None)
            };
            let end = self.current.at;
            items.spellings.push(self.source_name(at, end));
            // An unaliased item is named by its exact text, as in openCypher
            // (`n.p`, `count(*)`, `labels(n)`).
            let name = if self.take_word("AS")? {
                self.name()?
            } else {
                self.source_name(at, end)
            };
            if items.output.iter().any(|(old, _)| old.text == name.text) {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::DuplicateProjection),
                )
                .into());
            }
            let input_kind = value.column_type(&leaves.types(), &self.syntax.parameters);
            let kind = match aggregate {
                Some(
                    crate::GraphAggregateFunction::Collect
                    | crate::GraphAggregateFunction::CollectDistinct,
                ) => GraphSetColumnType::List,
                Some(crate::GraphAggregateFunction::Min | crate::GraphAggregateFunction::Max)
                | None => input_kind,
                Some(_) => GraphSetColumnType::Scalar,
            };
            functions.push(aggregate);
            items.projection.push(ReadProjectionTemplate {
                name: name.text.to_owned(),
                value,
            });
            items.output.push((name, kind));
            if !self.take(b',')? {
                break;
            }
        }
        if functions.iter().any(Option::is_some) {
            let mut inputs = core::mem::take(&mut items.projection);
            let keys = functions
                .iter()
                .enumerate()
                .filter_map(|(column, function)| function.is_none().then_some(column))
                .collect::<Vec<_>>();
            let mut aggregates = Vec::new();
            for (column, (input, function)) in inputs.iter_mut().zip(functions).enumerate() {
                let output = if let Some(function) = function {
                    let output = keys.len() + aggregates.len();
                    aggregates.push(crate::set_text::aggregate::ReadAggregateSpec {
                        name: format!("__write_summary_{}", aggregates.len()),
                        function,
                        column: (function != crate::GraphAggregateFunction::CountRows)
                            .then_some(column),
                    });
                    output
                } else {
                    keys.iter()
                        .position(|&key| key == column)
                        .expect("grouping key")
                };
                items.projection.push(ReadProjectionTemplate {
                    name: input.name.clone(),
                    value: ReadValueTemplate::Column(output),
                });
                input.name = format!("__write_input_{column}");
            }
            items.grouping = Some(WriteReturnGroupTemplate {
                inputs,
                keys,
                aggregates,
            });
        }
        Ok(items)
    }
}

/// Admit a write RETURN's projection over its binding types before any
/// catalog callback: names, value types and lazy-branch references.
pub(super) fn admit_return(
    projection: &[ReadProjectionTemplate],
    types: &[GraphSetColumnType],
    parameters: &[GqlParameterSpec],
    grouping: Option<&WriteReturnGroupTemplate>,
    at: usize,
) -> Result<(), GraphSetTextError> {
    let values = insertion::shape_arguments(parameters);
    let grouping = grouping
        .map(|group| {
            group
                .bind(&values)?
                .prepare(types.to_vec())
                .map_err(|kind| GraphSetTextError {
                    offset: at,
                    kind: crate::GraphSetTextErrorKind::AggregateBuild(kind),
                })
        })
        .transpose()?;
    let types = grouping
        .as_ref()
        .map_or(types, |group| group.column_types());
    for (column, output) in projection.iter().enumerate() {
        let value = return_projection::bind_read_value(&output.value, &values)?;
        for result in [
            GraphSetProjection::validate_output_name(&output.name, column),
            GraphSetProjection::admit_output(&value, types, column).map(|_| ()),
        ] {
            result.map_err(|kind| GraphSetTextError {
                offset: at,
                kind: crate::GraphSetTextErrorKind::ProjectionBuild(kind),
            })?;
        }
    }
    Ok(())
}

impl WriteReturnGroupTemplate {
    pub(super) fn bind(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<crate::set_ops::WriteReturnGroupSpec, GraphSetTextError> {
        let inputs = self
            .inputs
            .iter()
            .map(|input| {
                Ok(GraphSetProjection::new(
                    &input.name,
                    return_projection::bind_read_value(&input.value, values)?,
                ))
            })
            .collect::<Result<Vec<_>, GraphSetTextError>>()?;
        Ok(crate::set_ops::WriteReturnGroupSpec {
            inputs,
            keys: self.keys.clone(),
            aggregates: self.aggregates.clone(),
        })
    }
}

/// The mutation leaves: the parsed RETURN plus the selection it extends.
struct MutationLeaves<'r, 'a> {
    returning: &'r mut ParsedMutationReturn<'a>,
    columns: &'r mut Vec<Projection<'a>>,
    scope: ReturnScope,
    /// The WITH row of a write after WITH (fgdb-sl4ym), the only scope.
    row: Option<&'r [(Name<'a>, GraphSetColumnType)]>,
}
impl<'a> ReturnLeaves<'a> for MutationLeaves<'_, 'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        match self.row {
            Some(row) => match self.returning.row_metadata(parser, row, self.scope)? {
                Some(column) => Ok(Some(column)),
                None => self.returning.row_leaf(parser, row, self.scope),
            },
            None => self.returning.leaf(parser, self.columns, self.scope),
        }
    }
    fn map_input(
        &mut self,
        parser: &mut Parser<'a>,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        match self.row {
            Some(row) => self
                .returning
                .row_map_input(parser, row, self.scope, source, input),
            None => self
                .returning
                .map_input(parser, self.columns, self.scope, source, input),
        }
    }
    fn types(&self) -> Vec<GraphSetColumnType> {
        self.returning.types()
    }
}

/// What the statement's actions make unreadable after it.
#[derive(Clone, Copy)]
pub(super) struct ReturnScope {
    /// A label action or DETACH DELETE changes `labels()`.
    pub labels_changed: bool,
    /// DETACH DELETE cascades to edges the statement cannot see.
    pub deleting: bool,
}

pub(super) struct ParsedMutationReturn<'a> {
    bindings: Vec<(Binding<'a>, Name<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    grouping: Option<WriteReturnGroupTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
    /// The row stages after a WITH head, when the head is a WITH.
    tail: Option<Vec<ReadStageTemplate>>,
}

impl<'a> ParsedMutationReturn<'a> {
    fn column(
        &mut self,
        binding: Binding<'a>,
        name: Name<'a>,
        kind: GraphSetColumnType,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(column) = self
            .bindings
            .iter()
            .position(|(old, _, _)| old.same(binding))
        {
            return Ok(column);
        }
        if self.bindings.len() == MAX_PATTERN_VERTICES {
            return Err(error(
                name.at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: self.bindings.len() + 1,
                }),
            ));
        }
        let column = self.bindings.len();
        self.bindings.push((binding, name, kind));
        Ok(column)
    }

    fn leaf(
        &mut self,
        parser: &mut Parser<'a>,
        columns: &mut Vec<Projection<'a>>,
        scope: ReturnScope,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        let name = Name {
            text: word,
            at: parser.current.at,
        };
        if (word.eq_ignore_ascii_case("properties") || word.eq_ignore_ascii_case("keys"))
            && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let mut lookahead = parser.lexer.clone();
            lookahead.next()?;
            if let TokenKind::Word(variable) = lookahead.next()?.kind
                && matches!(
                    parser.insertion_match_kind(variable),
                    Some(GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                )
                && !parser.elements.contains(&variable)
                && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
            {
                parser.advance()?;
                parser.punct(b'(', "(")?;
                let variable = parser.name()?;
                parser.punct(b')', ")")?;
                return self
                    .map_input(parser, columns, scope, variable, ReturnMapInput::Properties)
                    .map(Some);
            }
            // properties(MAP/NULL) and keys(MAP/NULL) belong to the shared
            // row evaluator, whose argument may contain post-write leaves.
            return Ok(None);
        }
        if matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'(')) {
            // Graph functions are ordinary source projections, exactly as in
            // a SET right-hand side; scalar functions stay with the shared
            // expression compiler, which resolves their arguments here.
            let endpoint =
                word.eq_ignore_ascii_case("startNode") || word.eq_ignore_ascii_case("endNode");
            let kind = if endpoint {
                GraphSetColumnType::Vertex
            } else {
                match Parser::path_function(name) {
                    // A label action or a delete changes labels(): read the
                    // frozen list and apply this statement's own actions
                    // (openCypher Set3, Remove2). A deleted vertex refuses.
                    Ok(GraphPathFunction::Labels) if scope.labels_changed => {
                        parser.advance()?;
                        parser.punct(b'(', "(")?;
                        let variable = parser.vertex_variable()?;
                        parser.punct(b')', ")")?;
                        let target = parser.mutation_projection(columns, variable, None)?;
                        let current = parser.projection_slot(
                            columns,
                            Projection {
                                variable,
                                property: None,
                                path: Some(GraphPathFunction::Labels),
                                collect: None,
                            },
                        )?;
                        return self
                            .column(
                                Binding::Labels { target, current },
                                name,
                                GraphSetColumnType::List,
                            )
                            .map(Some);
                    }
                    Ok(GraphPathFunction::Value) => GraphSetColumnType::Path,
                    Ok(GraphPathFunction::Length | GraphPathFunction::Type) => {
                        GraphSetColumnType::Scalar
                    }
                    Ok(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
                    Ok(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
                    Ok(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
                    Ok(GraphPathFunction::Labels) => GraphSetColumnType::List,
                    Ok(GraphPathFunction::Properties) => unreachable!("handled above"),
                    Err(_) => return Ok(None),
                }
            };
            let Operand::Column(column) = parser.mutation_operand(columns)? else {
                unreachable!("a native graph function lowers to a source projection")
            };
            return self.column(Binding::Input(column), name, kind).map(Some);
        }
        let Some(kind) = parser.insertion_match_kind(word) else {
            return Ok(None);
        };
        let name = parser.name()?;
        if !parser.take(b'.')? {
            let column = parser.mutation_projection(columns, name, None)?;
            return self.column(Binding::Input(column), name, kind).map(Some);
        }
        let key = parser.schema_name()?;
        self.map_input(parser, columns, scope, name, ReturnMapInput::Property(key))
            .map(Some)
    }

    /// A complete metadata call over an element retained by the incoming
    /// WITH. Its captured map/list is a preimage, not the answer: the mutation
    /// owner overlays final properties/labels before projection or grouping.
    /// Local/map-valued operands stay with the ordinary expression compiler.
    fn row_metadata(
        &mut self,
        parser: &mut Parser<'a>,
        row: &[(Name<'a>, GraphSetColumnType)],
        scope: ReturnScope,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        let labels = word.eq_ignore_ascii_case("labels");
        if !labels && !word.eq_ignore_ascii_case("properties") && !word.eq_ignore_ascii_case("keys")
        {
            return Ok(None);
        }
        let mut lookahead = parser.lexer.clone();
        if !matches!(lookahead.next()?.kind, TokenKind::Punct(b'(')) {
            return Ok(None);
        }
        let token = lookahead.next()?;
        let TokenKind::Word(name) = token.kind else {
            return Ok(None);
        };
        if parser.elements.contains(&name)
            || !matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
        {
            return Ok(None);
        }
        let visible = parser.visible_width(row);
        let Some(target) = row[..visible]
            .iter()
            .position(|(source, _)| source.text == name)
        else {
            return Ok(None);
        };
        let domain = row[target].1;
        if !matches!(
            domain,
            GraphSetColumnType::Vertex | GraphSetColumnType::Edge
        ) || labels && domain != GraphSetColumnType::Vertex
        {
            return Ok(None);
        }
        if !labels && domain == GraphSetColumnType::Edge && scope.deleting {
            return Err(error(
                token.at,
                GraphPatternTextErrorKind::Expected(
                    "a vertex property after DETACH DELETE, which may cascade to edges",
                ),
            ));
        }
        let kind = if labels {
            GraphSetColumnType::List
        } else {
            GraphSetColumnType::Any
        };
        let current = parser
            .boundary_reads
            .as_ref()
            .filter(|boundary| boundary.width == row.len() && target < boundary.visible)
            .and_then(|boundary| {
                if labels {
                    boundary
                        .functions
                        .iter()
                        .find_map(|&(source, function, column)| {
                            (source == name && function == GraphPathFunction::Labels)
                                .then_some(column)
                        })
                } else {
                    boundary.reads.iter().find_map(|&(source, field, column)| {
                        (source == name && field.text == "*").then_some(column)
                    })
                }
            })
            .filter(|&column| row.get(column).is_some_and(|(_, domain)| *domain == kind))
            .ok_or_else(|| {
                error(
                    token.at,
                    GraphPatternTextErrorKind::Expected(
                        "metadata carried by the incoming WITH row",
                    ),
                )
            })?;
        let binding = if labels {
            Binding::Labels { target, current }
        } else {
            Binding::Properties { target, current }
        };
        let column = self.column(
            binding,
            Name {
                text: name,
                at: token.at,
            },
            kind,
        )?;
        for _ in 0..4 {
            parser.advance()?;
        }
        Ok(Some(column))
    }

    /// A RETURN leaf after a write after WITH: a value of the WITH row. A
    /// property of an element the row carries is that element after the
    /// write: the row's hidden read of it before the write, overlaid by the
    /// write's own proposal for the field.
    fn row_leaf(
        &mut self,
        parser: &mut Parser<'a>,
        row: &[(Name<'a>, GraphSetColumnType)],
        scope: ReturnScope,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        if parser.elements.contains(&word)
            || matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            return Ok(None);
        }
        let visible = parser.visible_width(row);
        let Some(index) = row[..visible]
            .iter()
            .position(|(name, _)| name.text == word)
        else {
            return Ok(None);
        };
        let kind = row[index].1;
        if matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge) {
            let mut lookahead = parser.lexer.clone();
            if matches!(lookahead.next()?.kind, TokenKind::Punct(b'.')) {
                let token = lookahead.next()?;
                if let TokenKind::Word(text) | TokenKind::DelimitedKeyword(text) = token.kind {
                    let key = Name { text, at: token.at };
                    let column = self.row_map_input(
                        parser,
                        row,
                        scope,
                        row[index].0,
                        ReturnMapInput::Property(key),
                    )?;
                    for _ in 0..3 {
                        parser.advance()?;
                    }
                    return Ok(Some(column));
                }
            }
        }
        let name = parser.name()?;
        if parser.is_punct(b'.')
            && matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
        {
            return Err(error(
                parser.current.at,
                GraphPatternTextErrorKind::Expected(
                    "a WITH row value; a carried element's property after the write",
                ),
            ));
        }
        self.column(Binding::Input(index), name, kind).map(Some)
    }

    fn map_input(
        &mut self,
        parser: &mut Parser<'a>,
        columns: &mut Vec<Projection<'a>>,
        scope: ReturnScope,
        name: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        let kind = parser
            .insertion_match_kind(name.text)
            .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownVariable))?;
        if matches!(input, ReturnMapInput::Guard)
            && !matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
        {
            return Err(error(
                name.at,
                GraphPatternTextErrorKind::Expected("vertex or edge map source"),
            ));
        }
        if matches!(input, ReturnMapInput::Identity | ReturnMapInput::Guard) {
            let target = parser.mutation_projection(columns, name, None)?;
            return self.column(Binding::Input(target), name, kind);
        }
        match kind {
            GraphSetColumnType::Vertex => {}
            GraphSetColumnType::Edge if !scope.deleting => {}
            GraphSetColumnType::Edge => {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected(
                        "a vertex property after DETACH DELETE, which may cascade to edges",
                    ),
                ));
            }
            _ => {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("vertex or edge property"),
                ));
            }
        }
        let target = parser.mutation_projection(columns, name, None)?;
        match input {
            ReturnMapInput::Identity | ReturnMapInput::Guard => {
                unreachable!("identity guard handled above")
            }
            ReturnMapInput::Property(key) => {
                let current = parser.mutation_projection(columns, name, Some(key))?;
                self.column(
                    Binding::Property {
                        target,
                        key,
                        current,
                    },
                    key,
                    GraphSetColumnType::Stored,
                )
            }
            ReturnMapInput::Properties => {
                let current = parser.projection_slot(
                    columns,
                    Projection {
                        variable: name,
                        property: None,
                        path: Some(GraphPathFunction::Properties),
                        collect: None,
                    },
                )?;
                self.column(
                    Binding::Properties { target, current },
                    name,
                    GraphSetColumnType::Any,
                )
            }
        }
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, _, kind)| *kind).collect()
    }

    pub(super) fn admit(
        &self,
        parameters: &[GqlParameterSpec],
    ) -> Result<(), GraphMutationTextError> {
        Ok(admit_return(
            &self.projection,
            &self.types(),
            parameters,
            self.grouping.as_ref(),
            self.at,
        )?)
    }

    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<MutationReturnTemplate, GraphMutationTextError> {
        let mut bindings = Vec::new();
        for (binding, _, _) in self.bindings {
            bindings.push(match binding {
                Binding::Input(column) => GraphMutationBinding::Input(column),
                Binding::Property {
                    target,
                    key,
                    current,
                } => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphMutationBinding::Property {
                        target,
                        key,
                        current,
                    }
                }
                Binding::Properties { target, current } => {
                    GraphMutationBinding::Properties { target, current }
                }
                Binding::Labels { target, current } => {
                    GraphMutationBinding::Labels { target, current }
                }
            });
        }
        Ok(MutationReturnTemplate {
            bindings,
            projection: self.projection,
            grouping: self.grouping,
            quantifier: self.quantifier,
            order: self.order,
            offset: self.offset,
            count: self.count,
            at: self.at,
            tail: self.tail,
        })
    }
}

impl<'a> Parser<'a> {
    /// `row` is the WITH row of a write after WITH (fgdb-sl4ym): RETURN then
    /// reads its values, and `*` is every column it carries.
    pub(super) fn mutation_return(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
        scope: ReturnScope,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<ParsedMutationReturn<'a>, GraphMutationTextError> {
        let at = self.current.at;
        // A WITH after the write is the head of a row pipeline whose rows
        // are this statement's own post-write values (openCypher Set, Remove
        // and Delete "... WITH n WHERE ... RETURN").
        let head = self.take_word("WITH")?;
        if !head {
            self.word("RETURN")?;
        }
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        let mut returning = ParsedMutationReturn {
            bindings: Vec::new(),
            projection: Vec::new(),
            grouping: None,
            quantifier,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
            tail: None,
        };
        let mut output: Vec<(Name<'a>, GraphSetColumnType)> = Vec::new();
        // The source spelling of every non-star output, for ORDER BY.
        let mut spellings = Vec::new();
        let mut star = true;
        if let Some(row) = row
            && self.take(b'*')?
        {
            // A WITH row's hidden reads are never columns of `*`.
            let visible = self.visible_width(row);
            for (index, &(name, kind)) in row[..visible].iter().enumerate() {
                let column = returning.column(Binding::Input(index), name, kind)?;
                returning.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                output.push((name, kind));
            }
        } else if self.take(b'*')? {
            let matched: Vec<_> = self
                .syntax
                .variables
                .iter()
                .copied()
                .chain(
                    self.syntax
                        .visible_edges()
                        .filter_map(|(edge, _)| edge.variable),
                )
                .chain(self.syntax.path)
                .chain(self.syntax.scope_paths.iter().copied())
                .filter(|name| !name.text.starts_with(Self::ANONYMOUS_PREFIX))
                .collect();
            for name in matched {
                let kind = self
                    .insertion_match_kind(name.text)
                    .expect("visible MATCH binding has a domain");
                let input = self.mutation_projection(columns, name, None)?;
                let column = returning.column(Binding::Input(input), name, kind)?;
                returning.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                output.push((name, kind));
            }
            if output.is_empty() {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("named MATCH bindings for RETURN *"),
                )
                .into());
            }
        } else {
            star = false;
            let items = self.write_return_items(&mut MutationLeaves {
                returning: &mut returning,
                columns: &mut *columns,
                scope,
                row,
            })?;
            returning.projection = items.projection;
            returning.grouping = items.grouping;
            output = items.output;
            spellings = items.spellings;
        }
        // openCypher orders RETURN *'s columns by name (fgdb-7jja3); each
        // still reads its own binding. WITH * keeps the row's order.
        if star && !head {
            let mut named: Vec<_> = core::mem::take(&mut returning.projection)
                .into_iter()
                .zip(core::mem::take(&mut output))
                .collect();
            named.sort_by(|(_, (a, _)), (_, (b, _))| a.text.cmp(b.text));
            (returning.projection, output) = named.into_iter().unzip();
        }
        // Pages address the completed projection, not its incoming WITH row.
        // Equal widths do not make those two column spaces interchangeable.
        let input_boundary = if row.is_some() {
            self.boundary_reads.take()
        } else {
            None
        };
        if let Some(ReadStageTemplate::Page {
            order,
            offset,
            count,
            ..
        }) = self.row_page_sourced(&output, &spellings)?
        {
            returning.order = order;
            returning.offset = offset;
            returning.count = count;
        }
        if head {
            if row.is_some() {
                self.boundary_reads = input_boundary;
            }
            let tail = self.write_tail(&mut returning, columns, scope, output, row)?;
            returning.tail = Some(tail);
        }
        Ok(returning)
    }

    /// The stages after a write's WITH head. A direct alias or identity key
    /// carries its post-write properties through the WITH's WHERE/pages,
    /// terminal RETURN or first UNWIND expression. Hidden grouping keys are
    /// functions of kept identities; no summary becomes an element binding.
    /// A row-fed write uses its admitted incoming field slots, never graph
    /// projection indices or a fresh read. UNWIND drops the hidden cells.
    fn write_tail(
        &mut self,
        returning: &mut ParsedMutationReturn<'a>,
        columns: &mut Vec<Projection<'a>>,
        scope: ReturnScope,
        mut schema: Vec<(Name<'a>, GraphSetColumnType)>,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<Vec<ReadStageTemplate>, GraphMutationTextError> {
        let visible = schema.len();
        let mut reads = Vec::new();
        let input_boundary = self.boundary_reads.take();
        for (alias, key) in self.write_tail_reads(&schema)? {
            let Some(at) = schema[..visible]
                .iter()
                .position(|(name, _)| name.text == alias.text)
            else {
                continue;
            };
            let Some(binding) = WriteReturnGroupTemplate::carried_binding(
                returning.grouping.as_ref(),
                &returning.projection[at].value,
            ) else {
                continue;
            };
            let (Binding::Input(input), _, GraphSetColumnType::Vertex | GraphSetColumnType::Edge) =
                returning.bindings[binding]
            else {
                continue;
            };
            let column = if let Some(row) = row {
                let &(source, kind) = row
                    .get(input)
                    .ok_or_else(|| error(key.at, GraphPatternTextErrorKind::UnknownVariable))?;
                if kind == GraphSetColumnType::Edge && scope.deleting {
                    return Err(error(
                        key.at,
                        GraphPatternTextErrorKind::Expected(
                            "a vertex property after DETACH DELETE, which may cascade to edges",
                        ),
                    )
                    .into());
                }
                let current = input_boundary
                    .as_ref()
                    .filter(|boundary| boundary.width == row.len() && input < boundary.visible)
                    .and_then(|boundary| {
                        boundary.reads.iter().find_map(|&(name, field, column)| {
                            (name == source.text && field.text == key.text && column < row.len())
                                .then_some(column)
                        })
                    })
                    .ok_or_else(|| {
                        error(
                            key.at,
                            GraphPatternTextErrorKind::Expected(
                                "a property carried by the incoming WITH row",
                            ),
                        )
                    })?;
                returning.column(
                    Binding::Property {
                        target: input,
                        key,
                        current,
                    },
                    key,
                    GraphSetColumnType::Stored,
                )?
            } else {
                let variable = columns
                    .get(input)
                    .ok_or_else(|| error(key.at, GraphPatternTextErrorKind::UnknownVariable))?
                    .variable;
                returning.map_input(
                    self,
                    columns,
                    scope,
                    variable,
                    ReturnMapInput::Property(key),
                )?
            };
            let column = match &mut returning.grouping {
                Some(group) => group.hoist_property(&mut returning.projection, column, key.at)?,
                None => column,
            };
            push_tail_read(
                &mut returning.projection,
                &mut schema,
                &mut reads,
                (alias, key),
                column,
            )?;
        }
        Ok(self.finish_write_tail(schema, visible, reads)?)
    }

    /// Resolve a write tail's hidden reads through the read pipeline's own
    /// boundary columns, then parse its remaining stages.
    pub(super) fn finish_write_tail(
        &mut self,
        schema: Vec<(Name<'a>, GraphSetColumnType)>,
        visible: usize,
        reads: Vec<(&'a str, Name<'a>, usize)>,
    ) -> Result<Vec<ReadStageTemplate>, GraphSetTextError> {
        if !reads.is_empty() {
            self.boundary_reads = Some(BoundaryReads {
                visible,
                width: schema.len(),
                reads,
                functions: Vec::new(),
                grouped: Vec::new(),
                scoped: Vec::new(),
            });
        }
        self.write_row_pipeline(schema)
    }

    /// `alias.key` reads of a carried vertex or edge in the WITH's WHERE,
    /// pages and terminal RETURN, or through the first UNWIND expression up
    /// to its top-level AS. A later WITH/MATCH/CALL/set operation ends the
    /// scope. Chained keys and other names are not carried property reads.
    pub(super) fn write_tail_reads(
        &self,
        outputs: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Vec<(Name<'a>, Name<'a>)>, GraphPatternTextError> {
        self.write_property_reads(outputs, false)
    }

    /// The next write needs property values used by expressions or RETURN,
    /// not the old value of a field merely named as a SET target. Avoid
    /// turning an identity-only continuation into an unnecessary read.
    pub(super) fn write_chain_reads(
        &self,
        outputs: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Vec<(Name<'a>, Name<'a>)>, GraphPatternTextError> {
        self.write_property_reads(outputs, true)
    }

    fn write_property_reads(
        &self,
        outputs: &[(Name<'a>, GraphSetColumnType)],
        skip_set_targets: bool,
    ) -> Result<Vec<(Name<'a>, Name<'a>)>, GraphPatternTextError> {
        let mut reads: Vec<(Name<'a>, Name<'a>)> = Vec::new();
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        // The three tokens before `token`, oldest first.
        let mut back: [Option<Token<'a>>; 3] = [None; 3];
        let mut depth = 0_usize;
        let mut unwind = false;
        let (mut setting, mut target) = (false, false);
        loop {
            let previous = back[2].map(|token| token.kind);
            let after_dot = matches!(previous, Some(TokenKind::Punct(b'.')));
            let after_alias = matches!(previous, Some(TokenKind::Word(word))
                if word.eq_ignore_ascii_case("AS"));
            if skip_set_targets && depth == 0 && !after_dot && !after_alias {
                match token.kind {
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("SET") => {
                        setting = true;
                        target = true;
                    }
                    TokenKind::Word(word)
                        if [
                            "ON", "RETURN", "WITH", "CREATE", "MERGE", "REMOVE", "DELETE", "DETACH",
                        ]
                        .iter()
                        .any(|clause| word.eq_ignore_ascii_case(clause)) =>
                    {
                        setting = false;
                        target = false;
                    }
                    TokenKind::Punct(b',') if setting => target = true,
                    TokenKind::Punct(b'=') if setting => target = false,
                    _ => {}
                }
            }
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word)
                    if unwind && depth == 0 && !after_dot && word.eq_ignore_ascii_case("AS") =>
                {
                    break;
                }
                TokenKind::Word(word)
                    if !unwind
                        && depth == 0
                        && !after_dot
                        && !after_alias
                        && word.eq_ignore_ascii_case("UNWIND") =>
                {
                    // The expression still reads the head's final-state
                    // cells. Nothing after its AS may request another one.
                    unwind = true;
                }
                // `ON MATCH` is a later MERGE's clause, not a new MATCH: its
                // right-hand sides read the carried row too (fgdb-cpny9).
                TokenKind::Word(word)
                    if !unwind
                        && depth == 0
                        && !after_dot
                        && !after_alias
                        && return_projection::pipeline::ends_boundary_scope(word, previous) =>
                {
                    break;
                }
                TokenKind::Word(key) | TokenKind::DelimitedKeyword(key) if after_dot && !target => {
                    if let Some(Token {
                        kind: TokenKind::Word(alias),
                        at,
                    }) = back[1]
                        && !matches!(
                            back[0],
                            Some(Token {
                                kind: TokenKind::Punct(b'.'),
                                ..
                            })
                        )
                        && outputs.iter().any(|(name, kind)| {
                            name.text == alias
                                && matches!(
                                    kind,
                                    GraphSetColumnType::Vertex | GraphSetColumnType::Edge
                                )
                        })
                        && !reads
                            .iter()
                            .any(|(read, known)| read.text == alias && known.text == key)
                    {
                        reads.push((
                            Name { text: alias, at },
                            Name {
                                text: key,
                                at: token.at,
                            },
                        ));
                    }
                }
                _ => {}
            }
            back = [back[1], back[2], Some(token)];
            token = lexer.next()?;
        }
        Ok(reads)
    }
}

impl MutationReturnTemplate {
    pub(super) fn bind(
        &self,
        mutation: PreparedGraphMutation,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphMutationQuery, GraphMutationTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphMutationQuery::prepare_with_grouping(
            mutation,
            self.bindings.clone(),
            projection,
            self.quantifier,
            self.grouping
                .as_ref()
                .map(|group| group.bind(values))
                .transpose()?,
        )
        .map_err(|kind| GraphMutationTextError {
            offset: self.at,
            kind: GraphMutationTextErrorKind::ReturnBuild(kind),
        })?;
        if !self.order.is_empty() {
            query = query
                .with_order_by(&self.order)
                .map_err(|kind| crate::GraphSetTextError {
                    offset: self.at,
                    kind: crate::GraphSetTextErrorKind::OrderBuild(kind),
                })?;
        }
        let query = query.with_page(
            return_projection::pipeline::page_value(&self.offset, values),
            self.count
                .as_ref()
                .map(|count| return_projection::pipeline::page_value(count, values)),
        );
        let Some(stages) = &self.tail else {
            return Ok(query);
        };
        // The tail reads the head's own output columns, hidden reads included.
        let owned =
            crate::PreparedGraphSet::owned(query.columns().to_vec(), query.column_types().to_vec());
        let tail = return_projection::bind_stages(owned, stages, values)?;
        Ok(query.with_tail(tail))
    }
}

/// One hoisted write-tail read: a hidden head column holding `column`, the
/// write's binding for `alias.key` after the statement, under a reserved
/// boundary name no user output takes.
pub(super) fn push_tail_read<'a>(
    projection: &mut Vec<ReadProjectionTemplate>,
    schema: &mut Vec<(Name<'a>, GraphSetColumnType)>,
    reads: &mut Vec<(&'a str, Name<'a>, usize)>,
    (alias, key): (Name<'a>, Name<'a>),
    column: usize,
) -> Result<(), GraphPatternTextError> {
    let name = return_projection::pipeline::BOUNDARY_READ_NAMES
        .iter()
        .copied()
        .find(|name| schema.iter().all(|(output, _)| output.text != *name))
        .ok_or_else(|| {
            error(
                key.at,
                GraphPatternTextErrorKind::Expected(
                    "at most 16 carried-element property reads after a WITH",
                ),
            )
        })?;
    projection.push(ReadProjectionTemplate {
        name: name.to_owned(),
        value: ReadValueTemplate::Column(column),
    });
    schema.push((
        Name {
            text: name,
            at: key.at,
        },
        GraphSetColumnType::Stored,
    ));
    reads.push((alias.text, key, schema.len() - 1));
    Ok(())
}

/// Single-statement framing on the script lexer: one terminal semicolon is
/// accepted, and a second statement, refuses (naming `shape`) before any
/// catalog callback.
pub(super) fn statement_body<'s>(
    statement: &'s str,
    shape: &'static str,
) -> Result<&'s str, GraphPatternTextError> {
    if statement.len() > MAX_GRAPH_TEXT_BYTES {
        return Err(error(
            MAX_GRAPH_TEXT_BYTES,
            GraphPatternTextErrorKind::DefinitionTooLarge,
        ));
    }
    let mut lexer = Lexer {
        text: statement,
        at: 0,
        tokens: 0,
    };
    loop {
        let token = script::next_script_token(&mut lexer)?;
        if matches!(token.kind, TokenKind::End) {
            return Ok(statement);
        }
        if matches!(token.kind, TokenKind::Punct(b';')) {
            let next = script::next_script_token(&mut lexer)?;
            if !matches!(next.kind, TokenKind::End) {
                return Err(error(next.at, GraphPatternTextErrorKind::Expected(shape)));
            }
            return Ok(&statement[..token.at]);
        }
    }
}

impl PreparedGraphMutationQueryText {
    /// Classify write text with native token framing: a MATCH-led (or
    /// UNWIND-led, fgdb-0s2pq) statement with a top-level SET, REMOVE, DELETE
    /// or DETACH DELETE clause followed by RETURN. SET/REMOVE also admit the
    /// parser's `(name).property` target. A CREATE, INSERT or MERGE clause, or
    /// a second statement, keeps its owner. Quoted values, property keys and
    /// aliases never become clause words. This validates nothing and resolves
    /// no symbol; prepare owns that.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphMutationTextError> {
        if statement.len() > crate::MAX_GRAPH_WRITE_SCRIPT_BYTES {
            return Err(error(
                0,
                GraphPatternTextErrorKind::Expected("bounded native write text"),
            )
            .into());
        }
        let mut lexer = Lexer {
            text: statement,
            at: 0,
            tokens: 0,
        };
        let first = script::next_script_token(&mut lexer)?;
        if !matches!(first.kind, TokenKind::Word(word) if ["MATCH", "OPTIONAL", "UNWIND"].iter().any(|lead| word.eq_ignore_ascii_case(lead)))
        {
            return Ok(false);
        }
        let mut mutating = false;
        let mut depth = 0_usize;
        let mut previous = first.kind;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word) if depth == 0 => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias {
                        let mut lookahead = lexer.clone();
                        let next = script::next_script_token(&mut lookahead)?.kind;
                        let after = script::next_script_token(&mut lookahead)?.kind;
                        let clause = |name: &str| word.eq_ignore_ascii_case(name);
                        if ["CREATE", "INSERT", "MERGE"]
                            .iter()
                            .any(|name| clause(name))
                            && matches!(next, TokenKind::Punct(b'('))
                        {
                            return Ok(false);
                        }
                        let target = matches!(next, TokenKind::Word(_));
                        // Match parenthesized_property_target's exact prefix,
                        // not every parenthesized expression after a keyword.
                        let property_action = (clause("SET") || clause("REMOVE"))
                            && (target
                                && matches!(after, TokenKind::Punct(b'.' | b':' | b'+' | b'='))
                                || matches!(next, TokenKind::Punct(b'('))
                                    && matches!(after, TokenKind::Word(_))
                                    && matches!(
                                        script::next_script_token(&mut lookahead)?.kind,
                                        TokenKind::Punct(b')')
                                    )
                                    && matches!(
                                        script::next_script_token(&mut lookahead)?.kind,
                                        TokenKind::Punct(b'.')
                                    ));
                        if property_action
                            || clause("DETACH")
                                && matches!(next, TokenKind::Word(word) if word.eq_ignore_ascii_case("DELETE"))
                            || clause("DELETE") && target
                        {
                            mutating = true;
                        }
                        if mutating && clause("RETURN") {
                            return Ok(true);
                        }
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
    }

    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphMutationTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphMutationTextError> {
        let body = statement_body(statement, "one SET/REMOVE RETURN statement")?;
        let (mutation, returning) = PreparedGraphMutationText::prepare_definition(
            body,
            relation,
            declarations,
            resolve,
            true,
        )?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        Ok(Self {
            statement: statement.to_owned(),
            mutation,
            returning,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.mutation.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphMutationQuery, GraphMutationTextError> {
        let mutation = self.mutation.bind_parameters(arguments)?;
        let values = self.mutation.checked_arguments(arguments)?;
        self.returning.bind(mutation, &values)
    }
}

#[cfg(test)]
mod framing_tests {
    use super::PreparedGraphMutationQueryText;

    #[test]
    fn only_a_mutation_clause_followed_by_return_routes_here() {
        for (text, expected) in [
            ("MATCH (n) SET n.p = 1 RETURN n.p", true),
            ("MATCH (n) SET n:L RETURN n", true),
            ("MATCH (n) SET n += {p: 1} RETURN n", true),
            ("MATCH (n) SET n = {p: 1} RETURN n", true),
            ("MATCH (n) WITH n SET n.p = 1 RETURN n", true),
            ("MATCH (n) REMOVE n.p RETURN n", true),
            ("MATCH (n) SET (n).p = 1 RETURN n.p", true),
            ("MATCH (n) REMOVE (n).p RETURN n.p", true),
            ("MATCH ()-[r:R]->() SET (r).p = 1 RETURN r.p", true),
            ("MATCH ()-[r:R]->() REMOVE (r).p RETURN r", true),
            (
                "MATCH (n) WITH n AS kept SET (kept).p = 1 RETURN kept",
                true,
            ),
            ("OPTIONAL MATCH (n) SET (n).p = 1 RETURN n", true),
            (
                "mAtCh (n) sEt /* target */ ( /* name */ n ).p = 1 RETURN n.p",
                true,
            ),
            ("MATCH (n) SET (n).p = 1", false),
            ("MATCH (n) REMOVE (n).p", false),
            ("MATCH (n) SET (n) RETURN n", false),
            ("MATCH (n) SET ((n)).p = 1 RETURN n", false),
            ("MATCH (n) RETURN 'SET (n).p = 1 RETURN n'", false),
            ("MATCH (n) /* REMOVE (n).p */ RETURN n", false),
            ("MATCH (n) RETURN (n).SET", false),
            ("MATCH (n) SET (n).p = 1 CREATE (m) RETURN m", false),
            ("MATCH (n) SET (n).p = 1; MATCH (m) RETURN m", false),
            ("MATCH (n) DETACH DELETE n RETURN n", true),
            ("MATCH (n) DELETE n RETURN 42 AS num", true),
            ("MATCH ()-[r]->() DELETE r RETURN type(r)", true),
            ("OPTIONAL MATCH (n) SET n.p = 1 RETURN n", true),
            ("MATCH (n) SET (n).p = 1 RETURN n", true),
            ("MATCH ()-[r]->() SET (r).p = 1 RETURN r", true),
            // An UNWIND-led MATCH mutation (fgdb-0s2pq).
            (
                "UNWIND $rows AS row MATCH (n {id: row.id}) SET n.p = row.p RETURN n.p",
                true,
            ),
            (
                "UNWIND [1, 2] AS i MATCH (n {id: i}) REMOVE n.p RETURN n",
                true,
            ),
            (
                "UNWIND $rows AS row MATCH (n {id: row.id}) SET n.p = row.p",
                false,
            ),
            (
                "UNWIND $rows AS row MERGE (n {id: row.id}) SET n.p = row.p RETURN n",
                false,
            ),
            ("MATCH (n) SET (n).p = 1", false),
            ("MATCH (n) SET (n + 1) RETURN n", false),
            ("MATCH (n) DELETE n", false),
            ("MATCH (n) WHERE n.delete = 1 RETURN n.p AS delete", false),
            ("MATCH (n) SET n.p = 1", false),
            ("MATCH (n) RETURN n", false),
            ("MATCH (set) WITH set RETURN set", false),
            ("MATCH (n) WHERE n.SET = 1 RETURN n", false),
            ("MATCH (n) SET n.p = 1 CREATE (m) RETURN m", false),
            ("MATCH (n) SET n.p = 1; MATCH (m) RETURN m", false),
            ("CREATE (n) SET n.p = 1 RETURN n", false),
            ("MATCH (n) RETURN n.p AS SET", false),
        ] {
            assert_eq!(
                PreparedGraphMutationQueryText::has_return_clause(text).unwrap(),
                expected,
                "{text}"
            );
        }
    }
}
