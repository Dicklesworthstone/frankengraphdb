//! Native directed relationship MERGE over endpoints bound by the shared MATCH prefix.

use super::*;
use crate::mutation_text::{EdgeMergeReturnTemplate, EdgeMergeTextInput, WriteReturnGroupTemplate};
use crate::set_text::{
    BoundSetTextInput, ReadPageNumber, ReadProjectionTemplate, ReadStageTemplate,
};
use crate::{
    GraphEdgeMergeBinding, GraphEdgeMergeTextError, GraphEdgeMergeTextErrorKind,
    GraphSetColumnType, GraphSetProjection, GraphSetQuantifier, PreparedGraphEdgeMerge,
    PreparedGraphEdgeMergeQuery, PreparedGraphEdgeMergeQueryText, PreparedGraphEdgeMergeText,
};
use mutation_query::ReturnLeaves;

struct ParsedEdgeMerge<'a> {
    source: Name<'a>,
    destination: Name<'a>,
    relationship: Option<Name<'a>>,
    relation: Name<'a>,
    properties: Vec<super::merge::MergeProperty<'a>>,
    undirected: bool,
    actions: Option<super::merge::ParsedEdgeRowActions<'a>>,
}
/// A relationship MERGE over a WITH row: the pattern, its source and
/// destination row columns, and its RETURN.
type RowMerge<'a> = (
    ParsedEdgeMerge<'a>,
    usize,
    usize,
    Option<ParsedEdgeReturn<'a>>,
);

fn build_error(at: usize, source: crate::GraphEdgeMergeBuildError) -> GraphEdgeMergeTextError {
    GraphEdgeMergeTextError {
        offset: at,
        kind: GraphEdgeMergeTextErrorKind::Build(source),
    }
}

impl<'a> Parser<'a> {
    /// A plain SET may also write carried endpoints. Keep that existing
    /// mutation family intact unless every assignment targets this edge.
    fn edge_only_set(&self, relationship: Option<Name<'a>>) -> Result<bool, GraphPatternTextError> {
        let Some(relationship) = relationship.filter(|_| self.is_word("SET")) else {
            return Ok(false);
        };
        let mut lexer = self.lexer.clone();
        let (mut depth, mut target, mut property) = (0usize, true, false);
        loop {
            let token = lexer.next()?;
            if target {
                if !matches!(token.kind, TokenKind::Word(word) if word == relationship.text) {
                    return Ok(false);
                }
                let accessor = lexer.next()?;
                if !matches!(accessor.kind, TokenKind::Punct(b'.' | b'+')) {
                    return Ok(false);
                }
                target = false;
                property = matches!(accessor.kind, TokenKind::Punct(b'.'));
                continue;
            } else {
                match token.kind {
                    TokenKind::End | TokenKind::Punct(b';') => return Ok(true),
                    TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                    TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                    TokenKind::Punct(b',') if depth == 0 => target = true,
                    TokenKind::Word(word)
                        if depth == 0
                            && !property
                            && [
                                "ON", "RETURN", "WITH", "CREATE", "MERGE", "SET", "REMOVE",
                                "DELETE", "DETACH", "MATCH", "UNWIND", "OPTIONAL",
                            ]
                            .iter()
                            .any(|clause| word.eq_ignore_ascii_case(clause)) =>
                    {
                        return Ok(true);
                    }
                    _ => {}
                }
            }
            property = matches!(token.kind, TokenKind::Punct(b'.'));
        }
    }

    /// `MERGE (a)-[r:R {k: v}]->(b)`, its reverse arrow, or no arrow for an
    /// undirected MERGE. The relationship variable and the property map are
    /// optional; a named variable must be distinct from every MATCH binding
    /// (fgdb-at06q). The caller ends the statement or parses its RETURN.
    fn edge_merge_pattern(&mut self) -> Result<ParsedEdgeMerge<'a>, GraphEdgeMergeTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let left = self.edge_merge_endpoint()?;
        self.punct(b')', ")")?;

        let incoming = self.take(b'<')?;
        self.punct(b'-', "-")?;
        self.punct(b'[', "[")?;
        let relationship = if self.is_punct(b':') {
            None
        } else {
            let relationship = self.name()?;
            if self.insertion_match_kind(relationship.text).is_some() {
                return Err(error(
                    relationship.at,
                    GraphPatternTextErrorKind::Expected(
                        "a relationship variable distinct from MATCH bindings",
                    ),
                )
                .into());
            }
            Some(relationship)
        };
        self.punct(b':', ":")?;
        let relation = self.schema_name()?;
        let properties = self.merge_property_map().map_err(|refusal| match refusal {
            super::merge::MergeMapError::Pattern(error) => error.into(),
            super::merge::MergeMapError::TooManyFields { at, observed } => build_error(
                at,
                crate::GraphEdgeMergeBuildError::TooManyProperties {
                    limit: crate::MAX_GRAPH_EDGE_MERGE_PROPERTIES,
                    observed,
                },
            ),
            super::merge::MergeMapError::DuplicateProperty { at } => {
                build_error(at, crate::GraphEdgeMergeBuildError::DuplicateProperty)
            }
        })?;
        self.punct(b']', "]")?;
        self.punct(b'-', "-")?;
        let outgoing = self.take(b'>')?;
        if incoming && outgoing {
            return Err(error(
                relation.at,
                GraphPatternTextErrorKind::Expected("at most one relationship direction"),
            )
            .into());
        }

        self.punct(b'(', "(")?;
        let right = self.edge_merge_endpoint()?;
        self.punct(b')', ")")?;
        // An undirected MERGE creates left to right, as openCypher does.
        let (source, destination) = if incoming {
            (right, left)
        } else {
            (left, right)
        };
        Ok(ParsedEdgeMerge {
            source,
            destination,
            relationship,
            relation,
            properties,
            undirected: !incoming && !outgoing,
            actions: None,
        })
    }

    /// An endpoint of the MERGE pattern: a MATCH vertex, or after WITH (or
    /// in a later write of a chain) a name the row carries, which the row
    /// MERGE checks against that row (fgdb-cpny9).
    fn edge_merge_endpoint(&mut self) -> Result<Name<'a>, GraphPatternTextError> {
        if self.write_row_scope {
            self.name()
        } else {
            self.variable()
        }
    }
}

impl EdgeMergeTextInput {
    pub(crate) fn statement(&self) -> &str {
        match self {
            Self::Match(selection) => selection.statement(),
            Self::Row { statement, .. } => statement,
        }
    }
    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match self {
            Self::Match(selection) => selection.parameter_schema(),
            Self::Row { input, .. } => input.parameter_schema(),
        }
    }
    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphPatternTextError> {
        match self {
            Self::Match(selection) => selection.checked_arguments(arguments),
            Self::Row { input, .. } => input.checked_arguments(arguments),
        }
    }
    pub(crate) fn return_at(&self) -> usize {
        match self {
            Self::Match(selection) => selection.return_at,
            Self::Row { input, .. } => input.return_at,
        }
    }
}

impl PreparedGraphEdgeMergeText {
    /// Prepare `MATCH ... MERGE (a)-[r:R {k: v}]->(b)`, its reverse arrow,
    /// or the undirected `(a)-[:R]-(b)` (fgdb-at06q). Both endpoints must
    /// already be visible MATCH variables. Map values are non-null literals
    /// or parameters; they filter existing edges and fill a created one.
    /// Endpoint declarations, ON MATCH/ON CREATE clauses and RETURN refuse
    /// here rather than being partially ignored.
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        Self::prepare_with_parameter_types_and_resolver(
            statement,
            _relation,
            declarations,
            &mut resolve,
        )
    }

    /// As [`Self::prepare_with_parameter_types`], with a host resolver whose
    /// pinned tzdb the statement's temporal constructors use (fgdb-txbg9).
    pub(crate) fn prepare_with_parameter_types_and_resolver<R>(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<Self, GraphEdgeMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        Self::prepare_template(statement, declarations, resolve, false).map(|(merge, _)| merge)
    }

    /// The MERGE template, and with `returning` its RETURN template. RETURN
    /// leaves extend the same selection, after the two endpoint columns.
    pub(crate) fn prepare_template<R>(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
        returning: bool,
    ) -> Result<(Self, Option<EdgeMergeReturnTemplate>), GraphEdgeMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        parser.parse_match_prefix()?;
        if parser.is_word("WITH") {
            return Self::prepare_after_with(parser, statement, resolve, returning);
        }
        let parsed = parser.edge_merge_pattern()?;
        let mut projections = Vec::new();
        let source = parser.mutation_projection(&mut projections, parsed.source, None)?;
        let destination = parser.mutation_projection(&mut projections, parsed.destination, None)?;
        let parsed_return = if returning {
            Some(parser.edge_merge_return("RETURN", parsed.relationship, &mut projections, None)?)
        } else {
            None
        };
        parser.end()?;
        if let Some(parsed) = &parsed_return {
            mutation_query::admit_return(
                &parsed.projection,
                &parsed.types(),
                &parser.syntax.parameters,
                parsed.grouping.as_ref(),
                parsed.at,
            )?;
        }
        let syntax = parser.syntax;
        let at = syntax.return_at;

        // This direct MATCH builder does not use the read relation's binding
        // pass. Resolve computed output keys before preparing its projection.
        for (key, slot) in &syntax.element_keys {
            slot.resolve(
                match resolve.resolve_symbol(GraphSymbolKind::Property, key) {
                    Some(GraphSymbol::Property(key)) => Some(key),
                    _ => None,
                },
            );
        }
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) {
                return Ok(*value);
            }
            let value = resolve
                .resolve_symbol(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::WrongSymbolKind {
                        expected: kind,
                        found: value.kind(),
                    },
                ));
            }
            cache.insert(key, value);
            Ok(value)
        };
        let (builder, filters) = resolve_pattern(
            syntax.match_mode,
            &syntax.variables[..syntax.root_variables],
            &syntax.labels,
            &syntax.edges,
            syntax.filters,
            &mut symbol,
        )?;
        let mut scopes = Vec::new();
        for scope in syntax.scopes {
            scopes.push(scope.resolve(&mut symbol)?);
        }
        let GraphSymbol::Relation(found) = symbol(GraphSymbolKind::Relation, parsed.relation)?
        else {
            unreachable!("symbol domain checked by shared resolver")
        };
        let mut properties = Vec::new();
        for property in parsed.properties {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, property.key)?
            else {
                unreachable!("symbol domain checked by shared resolver")
            };
            properties.push((key, property.value));
        }
        let returning = parsed_return
            .map(|parsed| parsed.resolve(&mut symbol))
            .transpose()?;

        // The endpoints lead; RETURN leaves add MATCH bindings, their
        // properties and graph functions after them.
        let mut columns = Vec::with_capacity(projections.len());
        for (index, projection) in projections.into_iter().enumerate() {
            let key = match projection.property {
                Some(name) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("symbol domain checked by shared resolver")
                    };
                    Some(key)
                }
                None => None,
            };
            columns.push(BoundColumn {
                alias: format!("_edge_merge_{index}"),
                variable: projection.variable.text.to_owned(),
                key,
                path: projection.path,
                collect: None,
            });
        }
        drop(symbol);
        let clauses = scopes.iter().map(BoundScope::clause).collect::<Vec<_>>();
        let projected = columns
            .iter()
            .map(BoundColumn::declaration)
            .collect::<Vec<_>>();
        built(
            at,
            builder.prepare_values_with_clauses(&clauses, &projected, 0, None),
        )?;
        let reverse_catalog = columns
            .iter()
            .any(|column| {
                matches!(
                    column.path,
                    Some(
                        GraphPathFunction::Labels
                            | GraphPathFunction::Type
                            | GraphPathFunction::Properties
                    )
                )
            })
            .then(|| std::sync::Arc::new(ReverseSymbolCatalog::from_resolver(resolve, statement)));
        let selection = PreparedGraphText {
            statement: statement.to_owned(),
            builder,
            filters,
            scopes,
            columns,
            ordering: Vec::new(),
            parameters: syntax.parameters,
            parameter_offsets: syntax.parameter_offsets,
            offset: Number::Literal(GqlParameterValue::UInt64(0)),
            count: None,
            distinct: false,
            visible_columns: None,
            return_at: at,
            reverse_catalog,
        };
        Ok((
            Self {
                input: EdgeMergeTextInput::Match(Box::new(selection)),
                relation: found,
                source,
                destination,
                properties,
                undirected: parsed.undirected,
                row_actions: None,
            },
            returning,
        ))
    }

    /// `MATCH ... WITH ... MERGE (a)-[r:R {k: v}]->(b) [RETURN ...]`
    /// (fgdb-cpny9): the endpoints are vertices the WITH row carries, and that
    /// row is the whole scope, as after `MATCH ... WITH ... CREATE`. Each row
    /// resolves its pair as a MATCH-selected row does.
    fn prepare_after_with<'a, R>(
        mut parser: Parser<'a>,
        statement: &'a str,
        resolve: &mut R,
        returning: bool,
    ) -> Result<(Self, Option<EdgeMergeReturnTemplate>), GraphEdgeMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let (head, stages, schema) = parser.write_row_prefix()?;
        let ((parsed, source, destination, parsed_return), _) =
            Self::row_merge(&mut parser, &schema, Some(returning))?;
        let read = parser.finish_write_row_prefix(statement, head, stages)?;
        let input = read.resolve(crate::graph_text::LentResolver(&mut *resolve))?;
        Self::resolve_row_merge(
            statement,
            resolve,
            (parsed, source, destination, parsed_return),
            input,
        )
    }

    /// A later write of a chain (`CREATE (a), (b) MERGE (a)-[:R]->(b)`): a
    /// relationship MERGE over the previous write's rows, read through one
    /// owned leaf. As after `MATCH ... WITH`, that row is the whole scope.
    /// A RETURN after the pattern makes it a query step. A next write after
    /// it makes it a row step: the continuation carries the row's visible
    /// columns and the relationship, as `WITH *` would.
    #[allow(clippy::type_complexity)]
    pub(crate) fn prepare_chained<'a, R>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
        from: ChainContinuation<'a>,
    ) -> Result<
        (
            Self,
            Option<EdgeMergeReturnTemplate>,
            Option<(usize, ChainContinuation<'a>)>,
        ),
        GraphEdgeMergeTextError,
    >
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let (columns, types) = from.owned_columns();
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones())
            .resume_after(from.boundary, &from.parameters, from.at)?;
        parser.chain_write = true;
        let schema = from.row;
        let keep = parser.visible_width(&schema);
        let (merged, continues) = Self::row_merge(&mut parser, &schema, None)?;
        let continuation =
            Self::chain_continuation(&mut parser, &schema, keep, &merged, continues)?;
        // An owned continuation has no read-source resolver to bind keys
        // introduced by this MERGE's computed RETURN or WITH head.
        for (key, slot) in &parser.syntax.element_keys {
            slot.resolve(
                match resolve.resolve_symbol(GraphSymbolKind::Property, key) {
                    Some(GraphSymbol::Property(key)) => Some(key),
                    _ => None,
                },
            );
        }
        let input = BoundSetTextInput {
            selection: None,
            parameters: parser.syntax.parameters.clone(),
            parameter_offsets: parser.syntax.parameter_offsets.clone(),
            return_at: statement.len(),
            projection: None,
            quantifier: GraphSetQuantifier::All,
            pipeline: Vec::new(),
            singleton: false,
            leading: Vec::new(),
            correlations: Vec::new(),
            row_fields: Vec::new(),
            owned: Some(Box::new((columns, types))),
        };
        let (merge, returning) = Self::resolve_row_merge(statement, resolve, merged, input)?;
        Ok((merge, returning, continuation))
    }

    /// A chain whose first write is a relationship MERGE over a leading MATCH
    /// (`MATCH (a), (b) MERGE (a)-[:R]->(b) CREATE ...`, fgdb-cpny9), including
    /// a correlated MATCH after a leading UNWIND or WITH. The MATCH rows,
    /// through its WITH head or as `WITH *` carries them, are the ordinary
    /// read relation, as after `MATCH ... WITH ... MERGE`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn prepare_first_step<'a, R>(
        statement: &'a str,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<
        (
            Self,
            Option<EdgeMergeReturnTemplate>,
            Option<(usize, ChainContinuation<'a>)>,
        ),
        GraphEdgeMergeTextError,
    >
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        let (matched, source, schema) = if parser.is_word("UNWIND") || parser.is_word("WITH") {
            let (source, schema) =
                parser.write_graph_row_prefix(statement, declarations, resolve.time_zones())?;
            (None, Some(source), schema)
        } else {
            parser.parse_match_prefix()?;
            let (head, stages, schema) = if parser.is_word("WITH") {
                parser.write_row_prefix()?
            } else {
                parser.implicit_write_row_prefix()?
            };
            (Some((head, stages)), None, schema)
        };
        parser.chain_write = true;
        let keep = parser.visible_width(&schema);
        let (merged, continues) = Self::row_merge(&mut parser, &schema, None)?;
        let continuation =
            Self::chain_continuation(&mut parser, &schema, keep, &merged, continues)?;
        let read = if let Some(mut source) = source {
            // MERGE and its continuation may add parameters and element keys
            // after the graph-to-row boundary; bind their complete table.
            source
                .syntax
                .parameters
                .clone_from(&parser.syntax.parameters);
            source
                .syntax
                .parameter_offsets
                .clone_from(&parser.syntax.parameter_offsets);
            source
                .syntax
                .element_keys
                .extend(parser.syntax.element_keys.iter().cloned());
            source
        } else {
            let (head, stages) = matched.expect("MATCH prefix retained its projection head");
            parser.finish_write_row_prefix(statement, head, stages)?
        };
        let input = read.resolve(crate::graph_text::LentResolver(&mut *resolve))?;
        let (merge, returning) = Self::resolve_row_merge(statement, resolve, merged, input)?;
        Ok((merge, returning, continuation))
    }

    /// The row the next write of a chain reads after a relationship MERGE
    /// step: a WITH head's output row, or (`continues` without a head) the
    /// row's first `keep` columns and the relationship, as `WITH *` carries
    /// them.
    fn chain_continuation<'a>(
        parser: &mut Parser<'a>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        keep: usize,
        merged: &RowMerge<'a>,
        continues: bool,
    ) -> Result<Option<(usize, ChainContinuation<'a>)>, GraphEdgeMergeTextError> {
        if !continues {
            return Ok(None);
        }
        // The shared write-tail compiler carries final-state property cells
        // with their boundary metadata. They remain private input columns of
        // the following write, just as after CREATE's implicit WITH head.
        if let Some(continuation) = parser.continuation() {
            return Ok(Some((keep, continuation)));
        }
        let row = if let Some(head) = &merged.3 {
            head.output.clone()
        } else {
            let mut carried = schema[..keep].to_vec();
            if carried.len() >= MAX_PATTERN_VERTICES {
                return Err(error(
                    parser.current.at,
                    GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                        dimension: crate::algebra::PatternLimitDimension::Columns,
                        limit: MAX_PATTERN_VERTICES,
                        observed: carried.len() + 1,
                    }),
                )
                .into());
            }
            let name = merged.0.relationship.unwrap_or(Name {
                text: Parser::anonymous_name(carried.len()),
                at: parser.current.at,
            });
            carried.push((name, GraphSetColumnType::Edge));
            carried
        };
        Ok(Some((
            keep,
            ChainContinuation {
                at: parser.current.at,
                row,
                boundary: None,
                parameters: parser.syntax.parameters.clone(),
            },
        )))
    }

    /// A relationship MERGE pattern over a WITH row, its endpoint columns and
    /// RETURN, through the statement end. `returning` None takes a RETURN
    /// when one follows the pattern. In a chain, a next write directly after
    /// the pattern ends it instead (the `bool`): that write reads the row.
    fn row_merge<'a>(
        parser: &mut Parser<'a>,
        schema: &[(Name<'a>, GraphSetColumnType)],
        returning: Option<bool>,
    ) -> Result<(RowMerge<'a>, bool), GraphEdgeMergeTextError> {
        parser.write_row_scope = true;
        let mut parsed = parser.edge_merge_pattern()?;
        let endpoint = |name: Name<'_>| {
            schema
                .iter()
                .position(|(row, kind)| {
                    row.text == name.text && *kind == GraphSetColumnType::Vertex
                })
                .ok_or_else(|| {
                    error(
                        name.at,
                        GraphPatternTextErrorKind::Expected("a vertex the WITH row carries"),
                    )
                })
        };
        let source = endpoint(parsed.source)?;
        let destination = endpoint(parsed.destination)?;
        if let Some(relationship) = parsed.relationship
            && schema.iter().any(|(row, _)| row.text == relationship.text)
        {
            return Err(error(
                relationship.at,
                GraphPatternTextErrorKind::Expected(
                    "a relationship variable distinct from the WITH row",
                ),
            )
            .into());
        }
        // A chained ON/SET clause belongs to this occurrence-preserving
        // MERGE. A plain SET on a different carried element remains the
        // existing terminal mutation step. An anonymous relationship's ON
        // clauses can assign only the vertices the row carries: its
        // placeholder name is empty, which no identifier matches.
        let edge_set = parser.edge_only_set(parsed.relationship)?;
        if parser.chain_write && (parser.is_word("ON") || edge_set) {
            let relationship = parsed.relationship.unwrap_or(Name {
                text: "",
                at: parser.current.at,
            });
            parsed.actions = Some(parser.edge_row_actions(relationship, schema)?);
        }
        let continues = parser.chain_write && parser.at_chained_write()?;
        // In a chain, a WITH head after the pattern is a query step whose
        // output row the next write reads.
        let headed = !continues && parser.chain_write && parser.is_word("WITH");
        let returning =
            !continues && !headed && returning.unwrap_or_else(|| parser.is_word("RETURN"));
        let mut parsed_return = if headed {
            let head = parser.edge_merge_return(
                "WITH",
                parsed.relationship,
                &mut Vec::new(),
                Some(schema),
            )?;
            if !parser.at_chained_write()? {
                return Err(error(
                    parser.current.at,
                    GraphPatternTextErrorKind::Expected(
                        "a CREATE or MERGE after the WITH of a MERGE step",
                    ),
                )
                .into());
            }
            Some(head)
        } else if returning {
            Some(parser.edge_merge_return(
                "RETURN",
                parsed.relationship,
                &mut Vec::new(),
                Some(schema),
            )?)
        } else {
            None
        };
        let continues = continues || headed;
        if continues && parsed_return.is_none() {
            let keep = parser.visible_width(schema);
            let mut output = schema[..keep].to_vec();
            let name = parsed.relationship.unwrap_or(Name {
                text: Parser::anonymous_name(output.len()),
                at: parser.current.at,
            });
            output.push((name, GraphSetColumnType::Edge));
            // Identity-only continuations retain the compact row path. A
            // following property read needs the existing final-state query
            // owner, including its authorization and occurrence accounting.
            if !parser.write_chain_reads(&output)?.is_empty() {
                let mut head = ParsedEdgeReturn::identity_head(
                    parsed.relationship,
                    schema,
                    keep,
                    name,
                    parser.current.at,
                )?;
                head.tail =
                    Some(parser.edge_merge_tail(&mut head, &mut Vec::new(), output, true)?);
                parsed_return = Some(head);
            }
        }
        if !continues {
            parser.end()?;
        }
        parser.write_row_scope = false;
        if let Some(parsed) = &parsed_return {
            mutation_query::admit_return(
                &parsed.projection,
                &parsed.types(),
                &parser.syntax.parameters,
                parsed.grouping.as_ref(),
                parsed.at,
            )?;
        }
        Ok(((parsed, source, destination, parsed_return), continues))
    }

    /// Symbols, map and RETURN of a relationship MERGE over a row relation.
    fn resolve_row_merge<'a, R>(
        statement: &'a str,
        resolve: &mut R,
        (parsed, source, destination, parsed_return): RowMerge<'a>,
        input: BoundSetTextInput,
    ) -> Result<(Self, Option<EdgeMergeReturnTemplate>), GraphEdgeMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) {
                return Ok(*value);
            }
            let value = resolve
                .resolve_symbol(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::WrongSymbolKind {
                        expected: kind,
                        found: value.kind(),
                    },
                ));
            }
            cache.insert(key, value);
            Ok(value)
        };
        let GraphSymbol::Relation(found) = symbol(GraphSymbolKind::Relation, parsed.relation)?
        else {
            unreachable!("symbol domain checked by shared resolver")
        };
        let mut properties = Vec::new();
        for property in parsed.properties {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, property.key)?
            else {
                unreachable!("symbol domain checked by shared resolver")
            };
            properties.push((key, property.value));
        }
        let returning = parsed_return
            .map(|parsed| parsed.resolve(&mut symbol))
            .transpose()?;
        let row_actions = parsed
            .actions
            .map(|actions| actions.resolve(&mut symbol))
            .transpose()?;
        Ok((
            Self {
                input: EdgeMergeTextInput::Row {
                    statement: statement.to_owned(),
                    input: Box::new(input),
                },
                relation: found,
                source,
                destination,
                properties,
                undirected: parsed.undirected,
                row_actions,
            },
            returning,
        ))
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        self.input.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.input.parameter_schema()
    }

    /// Bind without lexing, catalog access or storage observation.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeMerge, GraphEdgeMergeTextError> {
        let values = self.input.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    /// The chain has already checked its parameter table; share that one
    /// admitted value vector between selection, actions and RETURN.
    pub(crate) fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphEdgeMerge, GraphEdgeMergeTextError> {
        let mut properties = Vec::with_capacity(self.properties.len());
        for (key, template) in &self.properties {
            let value = match template {
                crate::mutation_text::VertexMergeValueTemplate::Bound(value) => value.clone(),
                crate::mutation_text::VertexMergeValueTemplate::Parameter { index, at } => {
                    scalar(values[*index].clone(), *at)?
                }
            };
            properties.push((*key, value));
        }
        // A NULL argument refuses as NullProperty: it can neither match nor
        // be created.
        let merge = match &self.input {
            EdgeMergeTextInput::Match(selection) => PreparedGraphEdgeMerge::prepare(
                selection.bind_values(values)?,
                self.relation,
                self.source,
                self.destination,
                properties,
            ),
            EdgeMergeTextInput::Row { input, .. } => PreparedGraphEdgeMerge::prepare_relation(
                input.bind_values(values)?,
                self.relation,
                self.source,
                self.destination,
                properties,
            ),
        }
        .map_err(|source| build_error(self.input.return_at(), source))?;
        Ok(if self.undirected {
            merge.with_undirected()
        } else {
            merge
        })
    }

    /// Bind the property-only action templates to a distinct chain family.
    /// The plain MERGE definition and RETURN compiler keep their semantics.
    pub(crate) fn bind_row_upsert(
        &self,
        merge: &PreparedGraphEdgeMerge,
        values: &[GqlParameterValue],
    ) -> Result<Option<crate::PreparedGraphEdgeUpsertRows>, GraphEdgeMergeTextError> {
        use crate::{
            GraphEdgeMergeRowAction as EdgeAction, GraphEdgeMergeRowInput as EdgeInput,
            GraphEdgeUpsertAction, GraphEdgeUpsertValue, GraphVertexMergeRowAction as VertexAction,
            GraphVertexMergeRowInput as VertexInput, GraphVertexUpsertAction,
        };
        let Some(templates) = &self.row_actions else {
            return Ok(None);
        };
        let bind = |templates: &[crate::mutation_text::VertexMergeRowActionTemplate]| {
            templates
                .iter()
                .map(|template| {
                    // An assignment to a vertex the row carries (fgdb-at06q).
                    if let crate::mutation_text::VertexMergeRowActionTemplate::RowVertex {
                        column,
                        key,
                        value,
                        at,
                    } = template
                    {
                        return Ok(EdgeAction::RowVertex {
                            column: *column,
                            key: *key,
                            value: crate::GraphEdgeMergeRowComputed(super::merge::bind_row_value(
                                value, values, *at,
                            )?),
                        });
                    }
                    let Some(action) =
                        super::merge::bind_row_actions(core::slice::from_ref(template), values)?
                            .pop()
                    else {
                        unreachable!("one bound action per template")
                    };
                    Ok(match action {
                        VertexAction::Vertex(GraphVertexUpsertAction::SetProperty {
                            key,
                            value,
                        }) => EdgeAction::Edge(GraphEdgeUpsertAction {
                            key,
                            value: GraphEdgeUpsertValue::Literal(value),
                        }),
                        VertexAction::Vertex(GraphVertexUpsertAction::SetStored { key, value }) => {
                            EdgeAction::Edge(GraphEdgeUpsertAction {
                                key,
                                value: GraphEdgeUpsertValue::Stored(value),
                            })
                        }
                        VertexAction::Expression { key, inputs, value } => EdgeAction::Expression {
                            key,
                            inputs: inputs
                                .into_iter()
                                .map(|input| match input {
                                    VertexInput::Property(key) => EdgeInput::Property(key),
                                    VertexInput::Column(column) => EdgeInput::Column(column),
                                })
                                .collect(),
                            value,
                        },
                        VertexAction::Value { key, value } => EdgeAction::Value {
                            key,
                            value: crate::GraphEdgeMergeRowComputed(value),
                        },
                        VertexAction::Vertex(_) => {
                            return Err(GraphEdgeMergeTextError::from(error(
                                self.input.return_at(),
                                GraphPatternTextErrorKind::Expected(
                                    "a relationship property assignment",
                                ),
                            )));
                        }
                    })
                })
                .collect::<Result<Vec<_>, GraphEdgeMergeTextError>>()
        };
        crate::PreparedGraphEdgeUpsertRows::prepare(
            merge.clone(),
            crate::GraphEdgeMergeRowActions {
                on_match: bind(&templates.on_match)?,
                on_create: bind(&templates.on_create)?,
                after: bind(&templates.after)?,
            },
        )
        .map(Some)
        .map_err(|kind| GraphEdgeMergeTextError {
            offset: self.input.return_at(),
            kind: GraphEdgeMergeTextErrorKind::RowActionsBuild(kind),
        })
    }
}

/// A RETURN leaf after relationship MERGE, before catalog resolution.
#[derive(Clone, Copy)]
enum EdgeBinding<'a> {
    Input(usize),
    Edge,
    Property(Name<'a>),
    Endpoint {
        start: bool,
    },
    EndpointProperty {
        start: bool,
        key: Name<'a>,
    },
    /// A property of the element in a row column, after the statement.
    InputProperty {
        column: usize,
        key: Name<'a>,
    },
}
impl EdgeBinding<'_> {
    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Self::Input(old), Self::Input(new)) => old == new,
            (
                Self::InputProperty { column, key },
                Self::InputProperty {
                    column: other,
                    key: other_key,
                },
            ) => column == other && key.text == other_key.text,
            (Self::Edge, Self::Edge) => true,
            (Self::Property(old), Self::Property(new)) => old.text == new.text,
            (Self::Endpoint { start: old }, Self::Endpoint { start: new }) => old == new,
            (
                Self::EndpointProperty { start, key },
                Self::EndpointProperty {
                    start: other,
                    key: other_key,
                },
            ) => start == other && key.text == other_key.text,
            _ => false,
        }
    }
}

/// RETURN after relationship MERGE (fgdb-at06q): MATCH bindings and their
/// properties are frozen selection columns; the relationship, its properties
/// after the statement and its own endpoints are supplied by the write owner.
struct ParsedEdgeReturn<'a> {
    relationship: Option<Name<'a>>,
    bindings: Vec<(EdgeBinding<'a>, Name<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    grouping: Option<crate::mutation_text::WriteReturnGroupTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
    tail: Option<Vec<ReadStageTemplate>>,
    /// A chain step's WITH head output row: each item's name and type.
    output: Vec<(Name<'a>, GraphSetColumnType)>,
}

impl<'a> ParsedEdgeReturn<'a> {
    /// An implicit WITH * over one MERGE occurrence. Hidden properties are
    /// added by the same compiler as an explicit WITH head.
    fn identity_head(
        relationship: Option<Name<'a>>,
        row: &[(Name<'a>, GraphSetColumnType)],
        keep: usize,
        edge: Name<'a>,
        at: usize,
    ) -> Result<Self, GraphPatternTextError> {
        let mut head = Self {
            relationship,
            bindings: Vec::new(),
            projection: Vec::new(),
            grouping: None,
            quantifier: GraphSetQuantifier::All,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
            tail: None,
            output: Vec::new(),
        };
        for (input, &(name, kind)) in row[..keep].iter().enumerate() {
            let column = head.column(EdgeBinding::Input(input), name, kind)?;
            head.projection.push(ReadProjectionTemplate {
                name: name.text.to_owned(),
                value: ReadValueTemplate::Column(column),
            });
        }
        let column = head.column(EdgeBinding::Edge, edge, GraphSetColumnType::Edge)?;
        head.projection.push(ReadProjectionTemplate {
            name: edge.text.to_owned(),
            value: ReadValueTemplate::Column(column),
        });
        Ok(head)
    }

    fn column(
        &mut self,
        binding: EdgeBinding<'a>,
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
        self.bindings.push((binding, name, kind));
        Ok(self.bindings.len() - 1)
    }

    fn is_relationship(&self, word: &str) -> bool {
        self.relationship.is_some_and(|name| name.text == word)
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, _, kind)| *kind).collect()
    }

    fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<EdgeMergeReturnTemplate, GraphPatternTextError> {
        let mut property = |name| {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                unreachable!("symbol domain checked by shared resolver")
            };
            Ok::<_, GraphPatternTextError>(key)
        };
        let mut bindings = Vec::with_capacity(self.bindings.len());
        for (binding, _, _) in self.bindings {
            bindings.push(match binding {
                EdgeBinding::Input(column) => GraphEdgeMergeBinding::Input(column),
                EdgeBinding::Edge => GraphEdgeMergeBinding::Edge,
                EdgeBinding::Property(key) => GraphEdgeMergeBinding::Property(property(key)?),
                EdgeBinding::Endpoint { start } => GraphEdgeMergeBinding::Endpoint { start },
                EdgeBinding::EndpointProperty { start, key } => {
                    GraphEdgeMergeBinding::EndpointProperty {
                        start,
                        key: property(key)?,
                    }
                }
                EdgeBinding::InputProperty { column, key } => {
                    GraphEdgeMergeBinding::InputProperty {
                        column,
                        key: property(key)?,
                    }
                }
            });
        }
        Ok(EdgeMergeReturnTemplate {
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

/// The relationship MERGE leaves: the parsed RETURN plus the selection it
/// extends with MATCH bindings, or the WITH row that is the whole scope.
struct EdgeMergeLeaves<'r, 'a> {
    returning: &'r mut ParsedEdgeReturn<'a>,
    columns: &'r mut Vec<Projection<'a>>,
    row: Option<&'r [(Name<'a>, GraphSetColumnType)]>,
}
impl<'a> ReturnLeaves<'a> for EdgeMergeLeaves<'_, 'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        let call = matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('));
        let start = word.eq_ignore_ascii_case("startNode");
        if call && (start || word.eq_ignore_ascii_case("endNode")) {
            // startNode(r) / endNode(r) of the MERGE relationship are its own
            // endpoints, which an undirected match may hold either way round.
            let mut lookahead = parser.lexer.clone();
            lookahead.next()?;
            if matches!(lookahead.next()?.kind, TokenKind::Word(variable)
                    if self.returning.is_relationship(variable))
                && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
            {
                let name = parser.name()?;
                parser.punct(b'(', "(")?;
                parser.name()?;
                parser.punct(b')', ")")?;
                if parser.take(b'.')? {
                    let key = parser.schema_name()?;
                    return self
                        .returning
                        .column(
                            EdgeBinding::EndpointProperty { start, key },
                            key,
                            GraphSetColumnType::Stored,
                        )
                        .map(Some);
                }
                return self
                    .returning
                    .column(
                        EdgeBinding::Endpoint { start },
                        name,
                        GraphSetColumnType::Vertex,
                    )
                    .map(Some);
            }
        }
        if !call && self.returning.is_relationship(word) {
            let name = parser.name()?;
            if parser.take(b'.')? {
                let key = parser.schema_name()?;
                return self
                    .returning
                    .column(EdgeBinding::Property(key), key, GraphSetColumnType::Stored)
                    .map(Some);
            }
            return self
                .returning
                .column(EdgeBinding::Edge, name, GraphSetColumnType::Edge)
                .map(Some);
        }
        if let Some(row) = self.row {
            // After WITH the row is the whole scope (fgdb-cpny9): a carried
            // name is its column. `a.key` of a vertex or relationship the row
            // carries reads its value after the statement; otherwise it is a
            // carried element's hidden read.
            if call {
                return Ok(None);
            }
            let mut lookahead = parser.lexer.clone();
            if matches!(lookahead.next()?.kind, TokenKind::Punct(b'.')) {
                if let Some(column) = row.iter().position(|(name, kind)| {
                    name.text == word
                        && matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                }) {
                    parser.name()?;
                    parser.punct(b'.', ".")?;
                    let key = parser.schema_name()?;
                    return self
                        .returning
                        .column(
                            EdgeBinding::InputProperty { column, key },
                            key,
                            GraphSetColumnType::Stored,
                        )
                        .map(Some);
                }
                let key = lookahead.next()?;
                if let TokenKind::Word(text) = key.kind
                    && let Some(column) = parser.boundary_read(row.len())?
                {
                    let name = Name { text, at: key.at };
                    return self
                        .returning
                        .column(EdgeBinding::Input(column), name, GraphSetColumnType::Stored)
                        .map(Some);
                }
            }
            let Some(column) = row.iter().position(|(name, _)| name.text == word) else {
                return Ok(None);
            };
            let name = parser.name()?;
            return self
                .returning
                .column(EdgeBinding::Input(column), name, row[column].1)
                .map(Some);
        }
        if call {
            // Graph functions over MATCH bindings are ordinary selection
            // projections, as in a SET right-hand side. Scalar functions stay
            // with the shared expression compiler.
            let name = Name {
                text: word,
                at: parser.current.at,
            };
            let kind = if start || word.eq_ignore_ascii_case("endNode") {
                GraphSetColumnType::Vertex
            } else {
                match Parser::path_function(name) {
                    Ok(GraphPathFunction::Value) => GraphSetColumnType::Path,
                    Ok(GraphPathFunction::Length | GraphPathFunction::Type) => {
                        GraphSetColumnType::Scalar
                    }
                    Ok(GraphPathFunction::Nodes) => GraphSetColumnType::Vertices,
                    Ok(GraphPathFunction::Edges) => GraphSetColumnType::Edges,
                    Ok(GraphPathFunction::Edge) => GraphSetColumnType::Edge,
                    Ok(GraphPathFunction::Labels) => GraphSetColumnType::List,
                    Ok(GraphPathFunction::Properties) => GraphSetColumnType::Any,
                    Err(_) => return Ok(None),
                }
            };
            let Operand::Column(column) = parser.mutation_operand(self.columns)? else {
                unreachable!("a native graph function lowers to a selection projection")
            };
            return self
                .returning
                .column(EdgeBinding::Input(column), name, kind)
                .map(Some);
        }
        let Some(kind) = parser.insertion_match_kind(word) else {
            return Ok(None);
        };
        let name = parser.name()?;
        if !parser.take(b'.')? {
            let column = parser.mutation_projection(self.columns, name, None)?;
            return self
                .returning
                .column(EdgeBinding::Input(column), name, kind)
                .map(Some);
        }
        // MERGE changes no MATCH element, so a frozen property is final.
        let key = parser.schema_name()?;
        let column = parser.mutation_projection(self.columns, name, Some(key))?;
        self.returning
            .column(EdgeBinding::Input(column), key, GraphSetColumnType::Stored)
            .map(Some)
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.returning.types()
    }
}

impl<'a> Parser<'a> {
    /// `keyword` is RETURN, or WITH for the head of a chain step another
    /// write follows (fgdb-cpny9): the same items, whose output row the next
    /// write reads.
    fn edge_merge_return(
        &mut self,
        keyword: &'static str,
        relationship: Option<Name<'a>>,
        columns: &mut Vec<Projection<'a>>,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<ParsedEdgeReturn<'a>, GraphEdgeMergeTextError> {
        let at = self.current.at;
        // Under RETURN, a WITH head continues to this statement's own tail.
        let head = keyword == "RETURN" && self.take_word("WITH")?;
        if !head {
            self.word(keyword)?;
        }
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        if self.is_punct(b'*') {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("named RETURN items after relationship MERGE"),
            )
            .into());
        }
        let mut returning = ParsedEdgeReturn {
            relationship,
            bindings: Vec::new(),
            projection: Vec::new(),
            grouping: None,
            quantifier,
            order: Vec::new(),
            offset: ReadPageNumber::Literal(0),
            count: None,
            at,
            tail: None,
            output: Vec::new(),
        };
        let items = self.write_return_items(&mut EdgeMergeLeaves {
            returning: &mut returning,
            columns: &mut *columns,
            row,
        })?;
        returning.projection = items.projection;
        returning.grouping = items.grouping;
        if let Some(ReadStageTemplate::Page {
            order,
            offset,
            count,
            ..
        }) = self.row_page_sourced(&items.output, &items.spellings)?
        {
            returning.order = order;
            returning.offset = offset;
            returning.count = count;
        }
        if head || keyword == "WITH" && self.chain_write {
            returning.tail =
                Some(self.edge_merge_tail(&mut returning, columns, items.output, row.is_some())?);
        } else {
            returning.output = items.output;
        }
        Ok(returning)
    }

    /// A carried edge, MATCH element or actual MERGE endpoint has one frozen
    /// post-write property binding. A direct grouping key retains that same
    /// identity; its fields can be hidden keys without splitting its group.
    /// Aggregate representatives and dropped identities never acquire reads.
    /// Over a WITH row (`row`), an input binding is a row column, read after
    /// the statement.
    fn edge_merge_tail(
        &mut self,
        returning: &mut ParsedEdgeReturn<'a>,
        columns: &mut Vec<Projection<'a>>,
        mut schema: Vec<(Name<'a>, GraphSetColumnType)>,
        row: bool,
    ) -> Result<Vec<ReadStageTemplate>, GraphEdgeMergeTextError> {
        let visible = schema.len();
        let mut reads = Vec::new();
        let requested = if self.chain_write && self.at_chained_write()? {
            self.write_chain_reads(&schema)?
        } else {
            self.write_tail_reads(&schema)?
        };
        for (alias, key) in requested {
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
            let (binding, _, GraphSetColumnType::Vertex | GraphSetColumnType::Edge) =
                returning.bindings[binding]
            else {
                continue;
            };
            let property = match binding {
                EdgeBinding::Edge => EdgeBinding::Property(key),
                EdgeBinding::Endpoint { start } => EdgeBinding::EndpointProperty { start, key },
                EdgeBinding::Input(column) if row => EdgeBinding::InputProperty { column, key },
                EdgeBinding::Input(input) => {
                    let variable = columns[input].variable;
                    let column = self.mutation_projection(columns, variable, Some(key))?;
                    EdgeBinding::Input(column)
                }
                EdgeBinding::Property(_)
                | EdgeBinding::EndpointProperty { .. }
                | EdgeBinding::InputProperty { .. } => continue,
            };
            let column = returning.column(property, key, GraphSetColumnType::Stored)?;
            let column = match &mut returning.grouping {
                Some(group) => group.hoist_property(&mut returning.projection, column, key.at)?,
                None => column,
            };
            mutation_query::push_tail_read(
                &mut returning.projection,
                &mut schema,
                &mut reads,
                (alias, key),
                column,
            )?;
        }
        Ok(self.finish_write_tail(schema, visible, reads)?)
    }
}

impl EdgeMergeReturnTemplate {
    pub(super) fn bind(
        &self,
        merge: PreparedGraphEdgeMerge,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphEdgeMergeQuery, GraphEdgeMergeTextError> {
        let mut projection = Vec::with_capacity(self.projection.len());
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphEdgeMergeQuery::prepare_with_grouping(
            merge,
            self.bindings.clone(),
            projection,
            self.quantifier,
            self.grouping
                .as_ref()
                .map(|group| group.bind(values))
                .transpose()?,
        )
        .map_err(|kind| GraphEdgeMergeTextError {
            offset: self.at,
            kind: GraphEdgeMergeTextErrorKind::ReturnBuild(kind),
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
        let owned =
            crate::PreparedGraphSet::owned(query.columns().to_vec(), query.column_types().to_vec());
        let tail = return_projection::bind_stages(owned, stages, values)?;
        Ok(query.with_tail(tail))
    }
}

impl PreparedGraphEdgeMergeQueryText {
    /// Classify write text with native token framing (fgdb-at06q): a MATCH
    /// prefix with no other clause, then `MERGE (x)` followed by a
    /// relationship arrow, and RETURN or WITH directly after the pattern's second
    /// node. A vertex MERGE, a WITH/DELETE/SET before the MERGE, ON branches
    /// or a second MERGE keep their own owners and diagnostics. Quoted values,
    /// property keys and aliases never become clause words. This validates
    /// nothing and resolves no symbol; prepare owns that.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphEdgeMergeTextError> {
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
        if !matches!(first.kind, TokenKind::Word(word)
                if word.eq_ignore_ascii_case("MATCH") || word.eq_ignore_ascii_case("OPTIONAL"))
        {
            return Ok(false);
        }
        // Clauses the MATCH prefix cannot carry before the MERGE. A WITH row
        // may precede it (fgdb-cpny9): `MATCH ... WITH ... MERGE ... RETURN`.
        const PREFIX_ENDS: [&str; 9] = [
            "UNWIND", "CREATE", "INSERT", "DELETE", "DETACH", "SET", "REMOVE", "CALL", "RETURN",
        ];
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
                        if PREFIX_ENDS
                            .iter()
                            .any(|name| word.eq_ignore_ascii_case(name))
                        {
                            return Ok(false);
                        }
                        if word.eq_ignore_ascii_case("MERGE")
                            && matches!(
                                script::next_script_token(&mut lexer.clone())?.kind,
                                TokenKind::Punct(b'(')
                            )
                        {
                            return Self::relationship_then_return(&mut lexer);
                        }
                    }
                }
                _ => {}
            }
            previous = token.kind;
        }
    }

    /// After `MERGE`: the pattern's first node group must be followed by a
    /// relationship arrow, and RETURN or WITH must directly follow its second.
    fn relationship_then_return(lexer: &mut Lexer<'_>) -> Result<bool, GraphEdgeMergeTextError> {
        let mut depth = 0_usize;
        let mut nodes = 0_u8;
        loop {
            let token = script::next_script_token(lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(close @ (b')' | b']' | b'}')) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && close == b')' {
                        nodes += 1;
                        let next = script::next_script_token(&mut lexer.clone())?.kind;
                        if nodes > 1 {
                            return Ok(matches!(next, TokenKind::Word(word)
                                if word.eq_ignore_ascii_case("RETURN")
                                    || word.eq_ignore_ascii_case("WITH")));
                        }
                        if !matches!(next, TokenKind::Punct(b'-' | b'<')) {
                            return Ok(false);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// The host resolver's pinned tzdb and reverse catalog serve the
    /// statement; the relation coordinate is the MERGE's own, by name.
    pub fn prepare_with_parameter_types(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphEdgeMergeTextError> {
        let body = mutation_query::statement_body(statement, "one relationship MERGE RETURN")?;
        let (merge, returning) =
            PreparedGraphEdgeMergeText::prepare_template(body, declarations, &mut resolve, true)?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        Ok(Self {
            statement: statement.to_owned(),
            merge,
            returning,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.merge.parameter_schema()
    }

    /// Bind without lexing, catalog access or storage observation.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeMergeQuery, GraphEdgeMergeTextError> {
        let merge = self.merge.bind_parameters(arguments)?;
        let values = self.merge.input.checked_arguments(arguments)?;
        self.returning.bind(merge, &values)
    }
}
