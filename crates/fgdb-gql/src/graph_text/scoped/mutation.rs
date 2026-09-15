//! Mutation clauses on the SAME MATCH lexer, scopes, parameter table and GLA
//! compiler. All syntax is checked before catalog callbacks. Neither selection
//! text nor parameter text is synthesized, and execution never sees this AST.

mod aggregate_input;
mod deletion;
mod insertion;
mod integer;
mod merge;
mod return_projection;

use super::*;
use crate::mutation_text::{MutationActionTemplate, MutationIntegerTemplateOp};
use crate::{GraphMutationAction, GraphMutationBuildError, GraphMutationTextError,
    GraphMutationTextErrorKind, GraphMutationValue, GqlScalarParameter,
    MAX_GRAPH_MUTATION_ACTIONS, PreparedGraphMutation, PreparedGraphMutationText};
use fgdb_types::CanonicalScalar;

#[derive(Clone, Copy)]
struct Projection<'a> { variable: Name<'a>, property: Option<Name<'a>> }
enum Operand {
    Column(usize), Number(Number), Literal(GqlScalarParameter),
    Integer { program: Vec<MutationIntegerTemplateOp>, at: usize },
}
enum ActionKind<'a> {
    Property { key: Name<'a>, value: Option<Operand> },
    Label { label: Name<'a>, present: bool },
    Delete,
}
struct Action<'a> { target: usize, kind: ActionKind<'a> }

fn build_error(at: usize, source: GraphMutationBuildError) -> GraphMutationTextError {
    GraphMutationTextError { offset: at, kind: GraphMutationTextErrorKind::Build(source) }
}
fn scalar(value: GqlParameterValue, at: usize) -> Result<GqlScalarParameter, GraphPatternTextError> {
    match value {
        GqlParameterValue::Scalar(value) => Ok(value),
        GqlParameterValue::Int64(value) => GqlScalarParameter::new(CanonicalScalar::Int(value))
            .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral)),
        GqlParameterValue::UInt64(_) => Err(error(at,
            GraphPatternTextErrorKind::Expected("signed or canonical scalar assignment"))),
    }
}

impl<'a> Parser<'a> {
    fn mutation_projection(&self, columns: &mut Vec<Projection<'a>>, variable: Name<'a>, property: Option<Name<'a>>)
        -> Result<usize, GraphPatternTextError> {
        if let Some(at) = columns.iter().position(|column|
            column.variable.text == variable.text
                && column.property.map(|key| key.text) == property.map(|key| key.text)) {
            return Ok(at);
        }
        self.capacity(columns.len(), MAX_PATTERN_VERTICES, crate::algebra::PatternLimitDimension::Columns)?;
        let at = columns.len();
        columns.push(Projection { variable, property });
        Ok(at)
    }

    fn mutation_operand(&mut self, columns: &mut Vec<Projection<'a>>) -> Result<Operand, GraphPatternTextError> {
        let at = self.current.at;
        // A bound property wins over literal-looking names such as true/null.
        if matches!(self.current.kind, TokenKind::Word(_))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'.')) {
            let variable = self.variable()?;
            self.punct(b'.', ".")?;
            let key = self.name()?;
            return self.mutation_projection(columns, variable, Some(key)).map(Operand::Column);
        }
        let literal = match self.current.kind {
            TokenKind::Quoted(raw) => Some(super::super::literal::text_scalar(raw, at)?),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("TRUE") => Some(CanonicalScalar::Bool(true)),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("FALSE") => Some(CanonicalScalar::Bool(false)),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NULL") => Some(CanonicalScalar::Null),
            _ => None,
        };
        if let Some(value) = literal {
            self.advance()?;
            return GqlScalarParameter::new(value).map(Operand::Literal)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral));
        }
        let expected = match self.current.kind {
            TokenKind::Parameter(name) => self.parameter_types.get(name).copied().unwrap_or(GqlParameterType::Int64),
            _ => GqlParameterType::Int64,
        };
        if expected == GqlParameterType::UInt64 {
            return Err(error(at, GraphPatternTextErrorKind::Expected("signed or canonical scalar assignment")));
        }
        self.number(expected).map(Operand::Number)
    }

    fn mutation_actions(&mut self) -> Result<(Vec<Projection<'a>>, Vec<Action<'a>>), GraphMutationTextError> {
        let mut columns = Vec::new();
        let mut actions = Vec::new();
        let mut deletion_mode = None;
        loop {
            let at = self.current.at;
            let (setting, deleting) = if self.take_word("SET")? {
                (true, false)
            } else if self.take_word("REMOVE")? {
                (false, false)
            } else if self.take_word("DETACH")? {
                self.word("DELETE")?;
                (false, true)
            } else {
                if actions.is_empty() {
                    return Err(error(at, GraphPatternTextErrorKind::Expected("SET, REMOVE or DETACH DELETE")).into());
                }
                break;
            };
            if deletion_mode.is_some_and(|previous| previous != deleting) {
                return Err(build_error(at, GraphMutationBuildError::MixedDeletionAndUpdates));
            }
            deletion_mode = Some(deleting);
            loop {
                if actions.len() >= MAX_GRAPH_MUTATION_ACTIONS {
                    return Err(build_error(self.current.at, GraphMutationBuildError::TooManyActions {
                        limit: MAX_GRAPH_MUTATION_ACTIONS, observed: actions.len() + 1,
                    }));
                }
                let variable = self.variable()?;
                let target = self.mutation_projection(&mut columns, variable, None)?;
                let kind = if deleting {
                    ActionKind::Delete
                } else if self.take(b':')? {
                    ActionKind::Label { label: self.name()?, present: setting }
                } else {
                    self.punct(b'.', "property or label mutation")?;
                    let key = self.name()?;
                    let value = if setting {
                        self.punct(b'=', "=")?;
                        Some(self.mutation_expression(&mut columns)?)
                    } else { None };
                    ActionKind::Property { key, value }
                };
                actions.push(Action { target, kind });
                if !self.take(b',')? { break; }
            }
        }
        self.end()?;
        Ok((columns, actions))
    }
}

impl PreparedGraphMutationText {
    /// Prepare simultaneous SET/REMOVE or explicit DETACH DELETE after the
    /// existing MATCH/WALK/WHERE/OPTIONAL grammar. SET stores canonical NULL;
    /// REMOVE unsets a property. Bare DELETE, CREATE, RETURN and mixed deletion/
    /// update statements are not silently reinterpreted.
    ///
    /// RHS arithmetic supports checked nullable i64 +, -, *, /, %, unary signs,
    /// parentheses, ABS, NULLIF and COALESCE with at least two arguments. Division
    /// truncates toward zero. COALESCE skips unneeded arithmetic but the frozen
    /// GLA projection still performs all requested property reads. Plain scalar
    /// assignments retain their existing types; arithmetic never coerces them.
    pub fn prepare(
        statement: &str, relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphMutationTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// Types are supplied once for the whole read/write statement. Numeric and
    /// canonical RHS arguments share the WHERE schema and original offsets.
    /// Every (kind,name) resolves once across MATCH scopes and mutation actions.
    pub fn prepare_with_parameter_types(
        statement: &str, relation: RelationId, declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphMutationTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?;
        parser.parse_match_prefix()?;
        let (projections, parsed_actions) = parser.mutation_actions()?;
        let syntax = parser.syntax;
        let at = syntax.return_at;
        let mut cache = BTreeMap::new();
        let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
            let key = (kind, name.text.to_owned());
            if let Some(value) = cache.get(&key) { return Ok(*value); }
            let value = resolve(kind, name.text)
                .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind)))?;
            if value.kind() != kind {
                return Err(error(name.at, GraphPatternTextErrorKind::WrongSymbolKind { expected: kind, found: value.kind() }));
            }
            cache.insert(key, value);
            Ok(value)
        };
        let (builder, filters) = resolve_pattern(
            &syntax.variables[..syntax.root_variables], &syntax.labels, &syntax.edges,
            syntax.filters, &mut symbol,
        )?;
        let mut scopes = Vec::new();
        for scope in syntax.scopes { scopes.push(scope.resolve(&mut symbol)?); }
        let mut actions = Vec::new();
        for action in parsed_actions {
            let target = action.target;
            actions.push(match action.kind {
                ActionKind::Delete => MutationActionTemplate::Bound(GraphMutationAction::DetachDelete { target }),
                ActionKind::Label { label, present } => {
                    let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                        unreachable!("symbol domain checked by the shared resolver")
                    };
                    MutationActionTemplate::Bound(GraphMutationAction::SetLabel { target, label, present })
                }
                ActionKind::Property { key, value } => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("symbol domain checked by the shared resolver")
                    };
                    match value {
                        None => MutationActionTemplate::Bound(GraphMutationAction::RemoveProperty { target, key }),
                        Some(Operand::Column(column)) => MutationActionTemplate::Bound(GraphMutationAction::SetProperty {
                            target, key, value: GraphMutationValue::Column(column),
                        }),
                        Some(Operand::Literal(value)) => MutationActionTemplate::Bound(GraphMutationAction::SetProperty {
                            target, key, value: GraphMutationValue::Literal(value),
                        }),
                        Some(Operand::Number(Number::Literal(value))) => MutationActionTemplate::Bound(GraphMutationAction::SetProperty {
                            target, key, value: GraphMutationValue::Literal(scalar(value, at)?),
                        }),
                        Some(Operand::Number(Number::Parameter(parameter))) => MutationActionTemplate::ParameterProperty {
                            target, key, parameter,
                        },
                        Some(Operand::Integer { program, at }) => MutationActionTemplate::IntegerProperty {
                            target, key, program, at,
                        },
                    }
                }
            });
        }
        let mut columns = Vec::new();
        for (index, projection) in projections.into_iter().enumerate() {
            let key = if let Some(name) = projection.property {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                    unreachable!("symbol domain checked by the shared resolver")
                };
                Some(key)
            } else { None };
            // Generated aliases are immutable compiler metadata. No query text
            // is rewritten and user names/arguments never become syntax.
            columns.push(BoundColumn {
                alias: format!("_mutation_{index}"), variable: projection.variable.text.to_owned(), key,
            });
        }
        let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
        let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
        built(at, builder.prepare_values_with_clauses(&clauses, &projected, 0, None))?;
        let selection = PreparedGraphText {
            statement: statement.to_owned(), builder, filters, scopes, columns,
            ordering: Vec::new(), parameters: syntax.parameters,
            parameter_offsets: syntax.parameter_offsets,
            offset: Number::Literal(GqlParameterValue::UInt64(0)), count: None,
            distinct: false, return_at: at,
        };
        Ok(Self { selection, relation, actions })
    }

    #[must_use]
    pub fn statement(&self) -> &str { self.selection.statement() }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.selection.parameter_schema() }

    /// Exact argument validation precedes all instantiation. No lexing,
    /// catalog lookup, storage observation or mutation occurs during binding.
    pub fn bind_parameters(&self, arguments: &GqlParameters) -> Result<PreparedGraphMutation, GraphMutationTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        let selection = self.selection.bind_values(&values)?;
        let at = self.selection.return_at;
        let mut actions = Vec::new();
        for action in &self.actions {
            actions.push(match action {
                MutationActionTemplate::Bound(action) => action.clone(),
                MutationActionTemplate::ParameterProperty { target, key, parameter } => GraphMutationAction::SetProperty {
                    target: *target, key: *key,
                    value: GraphMutationValue::Literal(scalar(values[*parameter].clone(), at)?),
                },
                MutationActionTemplate::IntegerProperty { target, key, program, at } => GraphMutationAction::SetProperty {
                    target: *target, key: *key,
                    value: GraphMutationValue::Expression(integer::bind_integer(program, &values, *at)?),
                },
            });
        }
        PreparedGraphMutation::prepare(selection, self.relation, actions).map_err(|error| build_error(at, error))
    }
}
