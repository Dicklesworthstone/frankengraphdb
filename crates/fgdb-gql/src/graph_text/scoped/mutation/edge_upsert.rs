//! Native relationship MERGE branch actions over bound MATCH endpoints.
//!
//! Property maps and either orientation use the ordinary MERGE matcher. The
//! action owner still requires one endpoint pair and one matching relationship;
//! admitting these patterns does not silently choose among parallel matches.

use super::*;
use crate::edge_upsert_text::{
    EdgeReturnTemplate, EdgeUpsertActionTemplate, EdgeUpsertValueTemplate,
};
use mutation_query::ReturnLeaves;

#[path = "edge_upsert/returning.rs"]
mod returning;
use crate::{
    GraphEdgeMergeTextError, GraphEdgeUpsertAction, GraphEdgeUpsertBranch,
    GraphEdgeUpsertTextError, GraphEdgeUpsertTextErrorKind, GraphEdgeUpsertValue,
    PreparedGraphEdgeMergeText, PreparedGraphEdgeUpsert, PreparedGraphEdgeUpsertText,
};

// Each RHS has an explicit property-input schema for the chosen relationship.
// The shared scalar compiler emits only typed operands/bytecode; neither the
// endpoint selection nor a source-text evaluator is used to read these inputs.
struct ParsedEdgeUpsertAction<'a> {
    key: Name<'a>,
    value: ParsedEdgeValue,
    properties: Vec<Name<'a>>,
    at: usize,
}

/// A scalar right-hand side, or a constant list (fgdb-k3nub).
enum ParsedEdgeValue {
    Scalar(Operand),
    Composite(crate::set_text::ReadValueTemplate),
}

struct ParsedEdgeUpsert<'a> {
    relationship: Name<'a>,
    source: Name<'a>,
    destination: Name<'a>,
    relation: Name<'a>,
    properties: Vec<super::merge::MergeProperty<'a>>,
    undirected: bool,
    on_match: Vec<ParsedEdgeUpsertAction<'a>>,
    on_create: Vec<ParsedEdgeUpsertAction<'a>>,
    after: Vec<ParsedEdgeUpsertAction<'a>>,
}
fn merge_error(error: GraphEdgeMergeTextError) -> GraphEdgeUpsertTextError {
    GraphEdgeUpsertTextError {
        offset: error.offset,
        kind: GraphEdgeUpsertTextErrorKind::Merge(error.kind),
    }
}

impl<'a> Parser<'a> {
    /// One `SET` assignment list of a relationship MERGE branch, appended to
    /// `target` under the branch's action limit. `r += {k: v, ...}` is the
    /// same simultaneous assignments as `r.k = v, ...`, one action apiece.
    fn edge_upsert_assignments(
        &mut self,
        relationship: Name<'a>,
        target: &mut Vec<ParsedEdgeUpsertAction<'a>>,
        branch: GraphEdgeUpsertBranch,
    ) -> Result<(), GraphEdgeUpsertTextError> {
        let capacity = |parser: &Self, count: usize| {
            if count < crate::MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Ok(());
            }
            Err(GraphEdgeUpsertTextError {
                offset: parser.current.at,
                kind: GraphEdgeUpsertTextErrorKind::UpsertBuild(
                    crate::GraphEdgeUpsertBuildError::TooManyActions {
                        branch,
                        limit: crate::MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                        observed: count + 1,
                    },
                ),
            })
        };
        loop {
            capacity(self, target.len())?;
            let actual = self.name()?;
            if actual.text != relationship.text {
                return Err(error(
                    actual.at,
                    GraphPatternTextErrorKind::Expected("the MERGE relationship variable"),
                )
                .into());
            }
            if self.take(b'+')? {
                self.punct(b'=', "=")?;
                let at = self.current.at;
                self.punct(b'{', "literal property map after +=")?;
                if self.is_punct(b'}') {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("a property in SET += map"),
                    )
                    .into());
                }
                let first = target.len();
                loop {
                    capacity(self, target.len())?;
                    let key = self.name()?;
                    if target[first..]
                        .iter()
                        .any(|action| action.key.text == key.text)
                    {
                        return Err(error(
                            key.at,
                            GraphPatternTextErrorKind::Expected(
                                "distinct property keys in SET += map",
                            ),
                        )
                        .into());
                    }
                    self.punct(b':', ":")?;
                    let action = self.edge_upsert_value(relationship, key)?;
                    target.push(action);
                    if self.take(b'}')? {
                        break;
                    }
                    self.punct(b',', ", or }")?;
                }
            } else {
                self.punct(b'.', "relationship property assignment")?;
                let key = self.name()?;
                self.punct(b'=', "=")?;
                let action = self.edge_upsert_value(relationship, key)?;
                target.push(action);
            }
            if !self.take(b',')? {
                break;
            }
        }
        Ok(())
    }

    /// The right-hand side of one relationship property assignment. It reads
    /// only the MERGE relationship's own fields, each through one input column.
    fn edge_upsert_value(
        &mut self,
        relationship: Name<'a>,
        key: Name<'a>,
    ) -> Result<ParsedEdgeUpsertAction<'a>, GraphEdgeUpsertTextError> {
        let at = self.current.at;
        if self.set_value_is_list()? {
            // A constant list (`['a', 'b']`, `$tags`) is a stored List
            // (fgdb-k3nub), evaluated once when the statement binds. One built
            // from the relationship's own fields is not yet.
            let mut inputs = Vec::new();
            let value = self
                .read_graph_value(&mut inputs, 0)
                .map_err(crate::GraphMutationTextError::from)?;
            if !inputs.is_empty() {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("a list value of literals and parameters"),
                )
                .into());
            }
            return Ok(ParsedEdgeUpsertAction {
                key,
                value: ParsedEdgeValue::Composite(value),
                properties: Vec::new(),
                at,
            });
        }
        let mut properties: Vec<Name<'a>> = Vec::new();
        let value = self.resolved_predicate(&mut |parser| {
            if !matches!(parser.current.kind, TokenKind::Word(_))
                || !matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'.'))
            {
                return Ok(None);
            }
            let source = parser.name()?;
            if source.text != relationship.text {
                return Err(error(
                    source.at,
                    GraphPatternTextErrorKind::Expected("the MERGE relationship variable"),
                ));
            }
            parser.punct(b'.', ".")?;
            let property = parser.name()?;
            if let Some(column) = properties.iter().position(|p| p.text == property.text) {
                return Ok(Some(column));
            }
            parser.capacity(
                properties.len(),
                MAX_PATTERN_VERTICES,
                crate::algebra::PatternLimitDimension::Columns,
            )?;
            let column = properties.len();
            properties.push(property);
            Ok(Some(column))
        })?;
        Ok(ParsedEdgeUpsertAction {
            key,
            value: ParsedEdgeValue::Scalar(value),
            properties,
            at,
        })
    }

    fn edge_upsert_pattern(
        &mut self,
        returning: bool,
    ) -> Result<ParsedEdgeUpsert<'a>, GraphEdgeUpsertTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let left = self.variable()?;
        self.punct(b')', ")")?;
        let incoming = self.take(b'<')?;
        self.punct(b'-', "-")?;
        self.punct(b'[', "[")?;
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
        self.punct(b':', ":")?;
        let relation = self.name()?;
        // Share literal/parameter, duplicate-key and field-count admission
        // with plain MERGE. The map is part of the match identity, not SET.
        let properties = self.merge_property_map().map_err(|refusal| match refusal {
            super::merge::MergeMapError::Pattern(error) => GraphEdgeUpsertTextError::from(error),
            super::merge::MergeMapError::TooManyFields { at, observed } => {
                merge_error(GraphEdgeMergeTextError {
                    offset: at,
                    kind: crate::GraphEdgeMergeTextErrorKind::Build(
                        crate::GraphEdgeMergeBuildError::TooManyProperties {
                            limit: crate::MAX_GRAPH_EDGE_MERGE_PROPERTIES,
                            observed,
                        },
                    ),
                })
            }
            super::merge::MergeMapError::DuplicateProperty { at } => {
                merge_error(GraphEdgeMergeTextError {
                    offset: at,
                    kind: crate::GraphEdgeMergeTextErrorKind::Build(
                        crate::GraphEdgeMergeBuildError::DuplicateProperty,
                    ),
                })
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
        let right = self.variable()?;
        self.punct(b')', ")")?;

        let mut on_match = Vec::new();
        let mut on_create = Vec::new();
        let mut after = Vec::new();
        let mut saw_match = false;
        let mut saw_create = false;
        while self.take_word("ON")? {
            let branch_at = self.current.at;
            let create = if self.take_word("MATCH")? {
                if saw_match {
                    return Err(GraphEdgeUpsertTextError {
                        offset: branch_at,
                        kind: GraphEdgeUpsertTextErrorKind::DuplicateBranch,
                    });
                }
                saw_match = true;
                false
            } else if self.take_word("CREATE")? {
                if saw_create {
                    return Err(GraphEdgeUpsertTextError {
                        offset: branch_at,
                        kind: GraphEdgeUpsertTextErrorKind::DuplicateBranch,
                    });
                }
                saw_create = true;
                true
            } else {
                return Err(error(
                    branch_at,
                    GraphPatternTextErrorKind::Expected("MATCH or CREATE after ON"),
                )
                .into());
            };
            self.word("SET")?;
            let (target, branch) = if create {
                (&mut on_create, GraphEdgeUpsertBranch::Create)
            } else {
                (&mut on_match, GraphEdgeUpsertBranch::Match)
            };
            self.edge_upsert_assignments(relationship, target, branch)?;
        }
        // A trailing SET reads the selected ON clause's native staged effects.
        // Do not flatten clauses or erase overwritten assignments: that would
        // also erase earlier expression failures, action charges and writes
        // that the authorized executor must individually admit.
        if self.take_word("SET")? {
            self.edge_upsert_assignments(relationship, &mut after, GraphEdgeUpsertBranch::Match)?;
        }
        for (branch, actions) in [
            (GraphEdgeUpsertBranch::Match, &on_match),
            (GraphEdgeUpsertBranch::Create, &on_create),
        ] {
            let observed = actions.len() + after.len();
            if observed > crate::MAX_GRAPH_EDGE_UPSERT_ACTIONS {
                return Err(GraphEdgeUpsertTextError {
                    offset: self.current.at,
                    kind: GraphEdgeUpsertTextErrorKind::UpsertBuild(
                        crate::GraphEdgeUpsertBuildError::TooManyActions {
                            branch,
                            limit: crate::MAX_GRAPH_EDGE_UPSERT_ACTIONS,
                            observed,
                        },
                    ),
                });
            }
        }

        if !returning && on_match.is_empty() && on_create.is_empty() && after.is_empty() {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("ON MATCH SET, ON CREATE SET or SET"),
            )
            .into());
        }
        // An undirected create uses the written left-to-right orientation;
        // the ordinary matcher accepts an existing edge either way round.
        let (source, destination) = if incoming {
            (right, left)
        } else {
            (left, right)
        };
        Ok(ParsedEdgeUpsert {
            relationship,
            source,
            destination,
            relation,
            properties,
            undirected: !incoming && !outgoing,
            on_match,
            on_create,
            after,
        })
    }
}

impl PreparedGraphEdgeUpsertText {
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeUpsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        _relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphEdgeUpsertTextError> {
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
    ) -> Result<Self, GraphEdgeUpsertTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        Self::prepare_definition(statement, declarations, resolve, false).map(|(upsert, _)| upsert)
    }

    // One grammar, parameter namespace and resolver cache for actions and
    // RETURN. Do not split source strings or reparse a synthetic MATCH query.
    pub(crate) fn prepare_definition<R: GraphSymbolResolver + ?Sized>(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
        returning: bool,
    ) -> Result<(Self, Option<EdgeReturnTemplate>), GraphEdgeUpsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        parser.parse_match_prefix()?;
        let parsed = parser.edge_upsert_pattern(returning)?;
        let parsed_return = if returning {
            Some(parser.edge_upsert_return(parsed.relationship)?)
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
        let mut projections = Vec::new();
        let source = parser.mutation_projection(&mut projections, parsed.source, None)?;
        let destination = parser.mutation_projection(&mut projections, parsed.destination, None)?;
        let syntax = parser.syntax;
        let at = syntax.return_at;

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
            unreachable!("symbol domain checked")
        };
        let mut properties = Vec::new();
        for property in parsed.properties {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, property.key)?
            else {
                unreachable!("symbol domain checked")
            };
            properties.push((key, property.value));
        }
        let on_match = resolve_actions(parsed.on_match, &mut symbol)?;
        let on_create = resolve_actions(parsed.on_create, &mut symbol)?;
        let after = resolve_actions(parsed.after, &mut symbol)?;
        let returning = parsed_return
            .map(|parsed| parsed.resolve(&mut symbol))
            .transpose()?;

        let columns = projections
            .into_iter()
            .enumerate()
            .map(|(index, projection)| BoundColumn {
                alias: format!("_edge_upsert_{index}"),
                variable: projection.variable.text.to_owned(),
                key: None,
                path: None,
                collect: None,
            })
            .collect::<Vec<_>>();
        let clauses = scopes.iter().map(BoundScope::clause).collect::<Vec<_>>();
        let projected = columns
            .iter()
            .map(BoundColumn::declaration)
            .collect::<Vec<_>>();
        built(
            at,
            builder.prepare_values_with_clauses(&clauses, &projected, 0, None),
        )?;
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
            reverse_catalog: None,
        };
        // Keep the map and orientation on the native MERGE definition. Its
        // binder checks nulls and resolved-key aliases before any execution;
        // its owner supplies the selected edge to the unchanged action engine.
        let merge = PreparedGraphEdgeMergeText {
            input: crate::mutation_text::EdgeMergeTextInput::Match(Box::new(selection)),
            relation: found,
            source,
            destination,
            properties,
            undirected: parsed.undirected,
        };
        Ok((
            Self {
                merge,
                on_match,
                on_create,
                after,
            },
            returning,
        ))
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphEdgeUpsert, GraphEdgeUpsertTextError> {
        let values = self.merge.input.checked_arguments(arguments)?;
        let merge = self.merge.bind_parameters(arguments).map_err(merge_error)?;
        let bind = |templates: &[EdgeUpsertActionTemplate]| -> Result<
            Vec<GraphEdgeUpsertAction<GraphEdgeUpsertValue>>,
            GraphEdgeUpsertTextError,
        > {
            templates
                .iter()
                .map(|action| {
                    let value = match &action.value {
                        EdgeUpsertValueTemplate::Bound(value) => {
                            GraphEdgeUpsertValue::Literal(value.clone())
                        }
                        EdgeUpsertValueTemplate::Parameter { index, at } => {
                            GraphEdgeUpsertValue::Literal(scalar(values[*index].clone(), *at)?)
                        }
                        EdgeUpsertValueTemplate::Expression {
                            properties,
                            program,
                            at,
                        } => GraphEdgeUpsertValue::Expression {
                            properties: properties.clone(),
                            value: integer::bind_integer(program, &values, *at)?,
                        },
                        // A constant list (fgdb-k3nub): a scalar result is a
                        // literal, a List the stored List; anything else
                        // refuses (Parser::constant_written_value).
                        EdgeUpsertValueTemplate::Composite { value, at } => {
                            let refused = || GraphEdgeUpsertTextError {
                                offset: *at,
                                kind: GraphEdgeUpsertTextErrorKind::Expression(
                                    crate::GraphMutationTextErrorKind::IntegerOperand,
                                ),
                            };
                            match Parser::constant_written_value(value, &values)
                                .map_err(crate::GraphMutationTextError::from)?
                            {
                                Some(fgdb_types::CanonicalPropertyValue::Scalar(scalar)) => {
                                    GraphEdgeUpsertValue::Literal(
                                        crate::GqlScalarParameter::new(scalar)
                                            .map_err(|_| refused())?,
                                    )
                                }
                                Some(stored) => GraphEdgeUpsertValue::Stored(stored),
                                None => return Err(refused()),
                            }
                        }
                    };
                    Ok(GraphEdgeUpsertAction {
                        key: action.key,
                        value,
                    })
                })
                .collect()
        };
        let on_match = bind(&self.on_match)?;
        let on_create = bind(&self.on_create)?;
        let after = bind(&self.after)?;
        PreparedGraphEdgeUpsert::prepare_with_clauses(merge, on_match, on_create, after).map_err(
            |source| GraphEdgeUpsertTextError {
                offset: self.merge.input.return_at(),
                kind: GraphEdgeUpsertTextErrorKind::UpsertBuild(source),
            },
        )
    }
}

// Resolution follows whole-statement grammar/type admission. Property keys
// remain in first-reference order so column ordinals keep their exact meaning.
fn resolve_actions<'a>(
    parsed: Vec<ParsedEdgeUpsertAction<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<Vec<EdgeUpsertActionTemplate>, GraphPatternTextError> {
    parsed
        .into_iter()
        .map(|action| {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, action.key)? else {
                unreachable!("symbol domain checked")
            };
            let operand = match action.value {
                ParsedEdgeValue::Composite(value) => {
                    return Ok(EdgeUpsertActionTemplate {
                        key,
                        value: EdgeUpsertValueTemplate::Composite {
                            value,
                            at: action.at,
                        },
                    });
                }
                ParsedEdgeValue::Scalar(operand) => operand,
            };
            let expression = match operand {
                Operand::Literal(value) => {
                    return Ok(EdgeUpsertActionTemplate {
                        key,
                        value: EdgeUpsertValueTemplate::Bound(value),
                    });
                }
                Operand::Number(Number::Literal(value)) => {
                    return Ok(EdgeUpsertActionTemplate {
                        key,
                        value: EdgeUpsertValueTemplate::Bound(scalar(value, action.at)?),
                    });
                }
                Operand::Number(Number::Parameter(index)) => {
                    return Ok(EdgeUpsertActionTemplate {
                        key,
                        value: EdgeUpsertValueTemplate::Parameter {
                            index,
                            at: action.at,
                        },
                    });
                }
                Operand::Column(column) => vec![MutationIntegerTemplateOp::Bound(
                    crate::GraphIntegerOp::ScalarColumn(column),
                )],
                Operand::Integer { program, .. } => program,
            };
            let properties = action
                .properties
                .into_iter()
                .map(|name| {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)?
                    else {
                        unreachable!("symbol domain checked")
                    };
                    Ok(key)
                })
                .collect::<Result<Vec<_>, GraphPatternTextError>>()?;
            Ok(EdgeUpsertActionTemplate {
                key,
                value: EdgeUpsertValueTemplate::Expression {
                    properties,
                    program: expression,
                    at: action.at,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod keyed_pattern_tests {
    use super::*;
    use fgdb_types::{CanonicalScalar, VId};

    const R: RelationId = RelationId(1);
    const P: PropertyKeyId = PropertyKeyId(1);
    const Q: PropertyKeyId = PropertyKeyId(2);

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            (GraphSymbolKind::Property, "p" | "alias") => Some(GraphSymbol::Property(P)),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(Q)),
            _ => None,
        }
    }

    #[test]
    fn keyed_upserts_preserve_maps_parameters_and_direction() {
        let arguments = GqlParameters::new().with_int64("key", 7).unwrap();
        let suffix = "ON CREATE SET r.q=r.p+1 ON MATCH SET r.q=r.p+2 SET r.p=r.q";
        let prepare = |pattern: &str| {
            PreparedGraphEdgeUpsertText::prepare(
                &format!("MATCH (a),(b) MERGE {pattern} {suffix}"),
                R,
                symbols,
            )
            .unwrap()
            .bind_parameters(&arguments)
            .unwrap()
        };
        let directed = prepare("(a)-[r:R {p:$key}]->(b)");
        let incoming = prepare("(b)<-[r:R {p:$key}]-(a)");
        assert_eq!(directed.canonical_bytes(), incoming.canonical_bytes());
        assert_eq!(directed.merge().properties().len(), 1);
        assert_eq!(directed.merge().properties()[0].0, P);
        assert_eq!(
            directed.merge().properties()[0].1.value(),
            &CanonicalScalar::Int(7)
        );
        assert_eq!(directed.on_match().len(), 1);
        assert_eq!(directed.on_create().len(), 1);
        assert_eq!(directed.after().len(), 1);
        let undirected = prepare("(a)-[r:R {p:$key}]-(b)");
        assert!(undirected.merge().undirected());
        assert_ne!(directed.canonical_bytes(), undirected.canonical_bytes());
        assert!(
            undirected
                .merge()
                .accepts_orientation((VId(1), VId(2)), (VId(2), VId(1)),)
        );
        assert!(
            !directed
                .merge()
                .accepts_orientation((VId(1), VId(2)), (VId(2), VId(1)),)
        );
    }

    #[test]
    fn returning_upsert_uses_the_same_keyed_pattern_and_parameter_namespace() {
        let text = "MATCH (a),(b) MERGE (a)-[r:R {p:$key}]-(b) \
                    ON CREATE SET r.q=r.p+$step \
                    ON MATCH SET r.q=r.q+$step RETURN r,r.p AS p,r.q AS q";
        let prepared = crate::PreparedGraphEdgeUpsertQueryText::prepare(text, R, symbols).unwrap();
        assert_eq!(prepared.parameter_schema().len(), 2);
        for key in [7, 9] {
            let arguments = GqlParameters::new()
                .with_int64("key", key)
                .unwrap()
                .with_int64("step", 2)
                .unwrap();
            let query = prepared.bind_parameters(&arguments).unwrap();
            assert!(query.upsert().merge().undirected());
            assert_eq!(
                query.upsert().merge().properties()[0].1.value(),
                &CanonicalScalar::Int(key)
            );
            assert_eq!(query.columns(), &["r", "p", "q"]);
        }
    }

    #[test]
    fn keyed_upserts_refuse_null_duplicate_aliases_and_binding_collisions() {
        for pattern in [
            "(a)-[r:R {p:null}]->(b)",
            "(a)-[r:R {p:1,p:2}]->(b)",
            "(a)-[r:R {p:1,alias:2}]->(b)",
            "(a)<-[r:R {p:1}]->(b)",
        ] {
            let text = format!("MATCH (a),(b) MERGE {pattern} SET r.q=2");
            let result = PreparedGraphEdgeUpsertText::prepare(&text, R, symbols)
                .and_then(|prepared| prepared.bind_parameters(&GqlParameters::new()));
            assert!(result.is_err(), "{text}");
        }
        let collision = "MATCH (a)-[r:R]->(b) \
                         MERGE (a)-[r:R {p:1}]->(b) SET r.q=2";
        assert!(PreparedGraphEdgeUpsertText::prepare(collision, R, symbols).is_err());
        let prepared = PreparedGraphEdgeUpsertText::prepare(
            "MATCH (a),(b) MERGE (a)-[r:R {p:$key}]->(b) SET r.q=2",
            R,
            symbols,
        )
        .unwrap();
        assert!(prepared.bind_parameters(&GqlParameters::new()).is_err());
    }
}
