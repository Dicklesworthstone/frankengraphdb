//! Mutation clauses on the SAME MATCH lexer, scopes, parameter table and GLA
//! compiler. All syntax is checked before catalog callbacks. Neither selection
//! text nor parameter text is synthesized, and execution never sees this AST.

mod aggregate_input;
mod deletion;
mod edge_merge;
mod edge_upsert;
mod insertion;
mod integer;
mod merge;
mod mutation_query;
pub(in crate::graph_text) mod return_projection;
mod script;

use super::*;
use crate::GraphSymbolResolver;
use crate::graph_text::GraphPathFunction;
use crate::mutation_text::{
    MutationActionTemplate, MutationIntegerTemplateOp, MutationReturnTemplate,
};
use crate::set_text::ReadValueTemplate;
use crate::{
    GqlScalarParameter, GraphMutationAction, GraphMutationBuildError, GraphMutationTextError,
    GraphMutationTextErrorKind, GraphMutationValue, MAX_GRAPH_MUTATION_ACTIONS,
    PreparedGraphMutation, PreparedGraphMutationText,
};
use fgdb_types::CanonicalScalar;

#[derive(Clone, Copy)]
struct Projection<'a> {
    variable: Name<'a>,
    property: Option<Name<'a>>,
    path: Option<GraphPathFunction>,
    /// The scope index of the pattern comprehension whose list this input
    /// is (fgdb-7nwox); it then reads no variable.
    collect: Option<usize>,
}
enum Operand {
    Column(usize),
    Number(Number),
    Literal(GqlScalarParameter),
    Integer {
        program: Vec<MutationIntegerTemplateOp>,
        at: usize,
    },
}
enum ActionKind<'a> {
    Property {
        key: Name<'a>,
        value: Option<ReadValueTemplate>,
    },
    /// SET after WITH (fgdb-sl4ym): a value-language read of the WITH row.
    RowProperty {
        key: Name<'a>,
        value: crate::set_text::ReadValueTemplate,
        at: usize,
    },
    /// `SET n = {...}`: unset every property of n the map does not name.
    /// `current` is n's property-map column.
    Clear {
        keep: Vec<Name<'a>>,
        current: usize,
    },
    Label {
        label: Name<'a>,
        present: bool,
    },
    Delete {
        detach: bool,
    },
    /// DELETE after WITH of a value the row computes (TCK Delete5): its
    /// action has no target column.
    RowDelete {
        value: crate::set_text::ReadValueTemplate,
        detach: bool,
        at: usize,
    },
}
struct Action<'a> {
    target: usize,
    kind: ActionKind<'a>,
}

/// Each parsed action with its symbols resolved. Targets and value columns
/// already index the mutation's input row.
fn action_templates<'a>(
    parsed: Vec<Action<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<Vec<MutationActionTemplate>, GraphMutationTextError> {
    let mut actions = Vec::new();
    for action in parsed {
        let target = action.target;
        actions.push(match action.kind {
            ActionKind::Delete { detach: true } => {
                MutationActionTemplate::Bound(GraphMutationAction::DetachDelete { target })
            }
            ActionKind::Delete { detach: false } => {
                MutationActionTemplate::Bound(GraphMutationAction::Delete { target })
            }
            ActionKind::RowDelete { value, detach, at } => {
                MutationActionTemplate::DeleteValue { value, detach, at }
            }
            ActionKind::Label { label, present } => {
                let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                    unreachable!("symbol domain checked by the shared resolver")
                };
                MutationActionTemplate::Bound(GraphMutationAction::SetLabel {
                    target,
                    label,
                    present,
                })
            }
            ActionKind::RowProperty { key, value, at } => MutationActionTemplate::ValueProperty {
                target,
                key: property_key(symbol, key)?,
                value,
                at,
            },
            ActionKind::Clear { keep, current } => {
                let mut keys = Vec::with_capacity(keep.len());
                for key in keep {
                    keys.push(property_key(symbol, key)?);
                }
                MutationActionTemplate::Bound(GraphMutationAction::ClearProperties {
                    target,
                    current,
                    keep: keys,
                })
            }
            ActionKind::Property { key, value } => {
                let key = property_key(symbol, key)?;
                match value {
                    None => MutationActionTemplate::Bound(GraphMutationAction::RemoveProperty {
                        target,
                        key,
                    }),
                    Some(ReadValueTemplate::Column(column)) => {
                        MutationActionTemplate::Bound(GraphMutationAction::SetProperty {
                            target,
                            key,
                            value: GraphMutationValue::Column(column),
                        })
                    }
                    Some(ReadValueTemplate::Literal(value)) => {
                        MutationActionTemplate::Bound(GraphMutationAction::SetProperty {
                            target,
                            key,
                            value: GraphMutationValue::Literal(value),
                        })
                    }
                    // Replacement-map entries also reach this lowering. Keep
                    // declared lists (including typed NULL) on the native
                    // value binder, not the scalar-only ParameterProperty arm.
                    Some(
                        value @ ReadValueTemplate::Parameter {
                            null_type: crate::GraphSetColumnType::List,
                            ..
                        },
                    ) => MutationActionTemplate::CompositeProperty { target, key, value },
                    Some(ReadValueTemplate::Parameter {
                        index: parameter, ..
                    }) => MutationActionTemplate::ParameterProperty {
                        target,
                        key,
                        parameter,
                    },
                    Some(ReadValueTemplate::Integer { program, at }) => {
                        MutationActionTemplate::IntegerProperty {
                            target,
                            key,
                            program,
                            at,
                        }
                    }
                    Some(value) => MutationActionTemplate::CompositeProperty { target, key, value },
                }
            }
        });
    }
    Ok(actions)
}

fn property_key<'a>(
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    key: Name<'a>,
) -> Result<PropertyKeyId, GraphPatternTextError> {
    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
        unreachable!("symbol domain checked by the shared resolver")
    };
    Ok(key)
}

/// A write target after WITH (fgdb-sl4ym): a vertex the WITH row carries,
/// or for every action but DETACH DELETE a vertex or relationship. SET and
/// REMOVE also take a dynamically typed column, such as the element of an
/// UNWIND over collected vertices (`WITH collect(a) AS ns UNWIND ns AS n SET
/// n.p = 1`): the collector writes the element it holds, skips NULL, and
/// refuses any other value (NonElementTarget). A DELETE of such a column is
/// a computed delete instead (row_element_target).
fn row_write_target<'a>(
    row: &[(Name<'a>, crate::GraphSetColumnType)],
    name: Name<'a>,
    detach: bool,
) -> Result<(usize, crate::GraphSetColumnType), GraphPatternTextError> {
    use crate::GraphSetColumnType::{Any, Edge, Vertex};
    let target = row
        .iter()
        .position(|(alias, _)| alias.text == name.text)
        .ok_or_else(|| error(name.at, GraphPatternTextErrorKind::UnknownVariable))?;
    let kind = row[target].1;
    if kind != Vertex && (detach || (kind != Edge && kind != Any)) {
        return Err(error(
            name.at,
            GraphPatternTextErrorKind::Expected(if detach {
                "a vertex the WITH row carries"
            } else {
                "a vertex or relationship the WITH row carries"
            }),
        ));
    }
    Ok((target, kind))
}

fn build_error(at: usize, source: GraphMutationBuildError) -> GraphMutationTextError {
    GraphMutationTextError {
        offset: at,
        kind: GraphMutationTextErrorKind::Build(source),
    }
}
fn scalar(
    value: GqlParameterValue,
    at: usize,
) -> Result<GqlScalarParameter, GraphPatternTextError> {
    match value {
        GqlParameterValue::Scalar(value) => Ok(value),
        GqlParameterValue::Int64(value) => GqlScalarParameter::new(CanonicalScalar::Int(value))
            .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral)),
        GqlParameterValue::UInt64(_) => Err(error(
            at,
            GraphPatternTextErrorKind::Expected("signed or canonical scalar assignment"),
        )),
        GqlParameterValue::List(_) | GqlParameterValue::Map(_) => Err(error(
            at,
            GraphPatternTextErrorKind::Expected("scalar assignment, not a collection parameter"),
        )),
    }
}

impl<'a> Parser<'a> {
    /// Whether a DELETE target after WITH names, bare, a carried vertex or,
    /// without DETACH a carried relationship (fgdb-sl4ym). Anything else is
    /// a value of the row (TCK Delete5).
    fn row_element_target(
        &self,
        row: &[(Name<'a>, crate::GraphSetColumnType)],
        detach: bool,
    ) -> Result<bool, GraphPatternTextError> {
        use crate::GraphSetColumnType::{Edge, Vertex};
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(false);
        };
        if matches!(
            self.lexer.clone().next()?.kind,
            TokenKind::Punct(b'.' | b'[' | b'(')
        ) {
            return Ok(false);
        }
        Ok(row.iter().any(|(name, kind)| {
            name.text == word && (*kind == Vertex || (!detach && *kind == Edge))
        }))
    }

    fn mutation_projection(
        &self,
        columns: &mut Vec<Projection<'a>>,
        variable: Name<'a>,
        property: Option<Name<'a>>,
    ) -> Result<usize, GraphPatternTextError> {
        let path = self.syntax.bare_path_function(variable.text);
        self.projection_slot(
            columns,
            Projection {
                variable,
                property,
                path,
                collect: None,
            },
        )
    }

    /// A source slot denotes the complete projection, not just its variable.
    /// In particular, a path, its length and its node/edge sequences are
    /// different values even though all refer to the same captured path.
    fn projection_slot(
        &self,
        columns: &mut Vec<Projection<'a>>,
        projection: Projection<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(at) = columns.iter().position(|column| {
            column.variable.text == projection.variable.text
                && column.property.map(|key| key.text) == projection.property.map(|key| key.text)
                && column.path == projection.path
                && column.collect == projection.collect
        }) {
            return Ok(at);
        }
        self.capacity(
            columns.len(),
            MAX_PATTERN_VERTICES,
            crate::algebra::PatternLimitDimension::Columns,
        )?;
        let at = columns.len();
        columns.push(projection);
        Ok(at)
    }

    /// A `.` after the current token that opens a property key, not the
    /// first dot of a slice's `..` (`list[null..3]`, List2 [9]).
    fn property_dot_follows(&self) -> Result<bool, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        Ok(matches!(lexer.next()?.kind, TokenKind::Punct(b'.'))
            && !matches!(lexer.next()?.kind, TokenKind::Punct(b'.')))
    }

    fn mutation_operand(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<Operand, GraphPatternTextError> {
        let at = self.current.at;
        // A bound property wins over literal-looking names such as true/null.
        if matches!(self.current.kind, TokenKind::Word(_)) && self.property_dot_follows()? {
            let variable = self.property_variable()?;
            self.punct(b'.', ".")?;
            let key = self.schema_name()?;
            return self
                .mutation_projection(columns, variable, Some(key))
                .map(Operand::Column);
        }
        // Scalar functions have already been consumed by the shared precedence
        // compiler. Graph metadata functions are source columns, not a second
        // evaluator; reuse the native graph-function and variable-domain checks.
        if matches!(self.current.kind, TokenKind::Word(_))
            && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let name = self.name()?;
            // openCypher startNode(r)/endNode(r) of a directed single-hop edge
            // is the endpoint vertex the pattern already binds (fgdb-j687q):
            // the same column as naming that vertex.
            let start = name.text.eq_ignore_ascii_case("startNode");
            if start || name.text.eq_ignore_ascii_case("endNode") {
                self.punct(b'(', "(")?;
                let edge = self.edge_variable()?;
                self.punct(b')', ")")?;
                let vertex = self.edge_endpoint(edge, start)?;
                return self
                    .mutation_projection(columns, vertex, None)
                    .map(Operand::Column);
            }
            let function = Self::path_function(name)?;
            self.punct(b'(', "(")?;
            let variable = match function {
                GraphPathFunction::Labels => self.vertex_variable()?,
                GraphPathFunction::Type => self.edge_variable()?,
                GraphPathFunction::Properties => self.property_variable()?,
                _ => self.path_variable()?,
            };
            self.punct(b')', ")")?;
            return self
                .projection_slot(
                    columns,
                    Projection {
                        variable,
                        property: None,
                        path: Some(function),
                        collect: None,
                    },
                )
                .map(Operand::Column);
        }
        if let Some(value) = self.float_literal()? {
            return GqlScalarParameter::new(value)
                .map(Operand::Literal)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral));
        }
        let literal = match self.current.kind {
            TokenKind::Quoted(raw) => Some(super::super::literal::text_scalar(raw, at)?),
            TokenKind::Word(word) if word.eq_ignore_ascii_case("TRUE") => {
                Some(CanonicalScalar::Bool(true))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("FALSE") => {
                Some(CanonicalScalar::Bool(false))
            }
            TokenKind::Word(word) if word.eq_ignore_ascii_case("NULL") => {
                Some(CanonicalScalar::Null)
            }
            _ => None,
        };
        if let Some(value) = literal {
            self.advance()?;
            return GqlScalarParameter::new(value)
                .map(Operand::Literal)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral));
        }
        let expected = match self.current.kind {
            TokenKind::Parameter(name) => self
                .parameter_types
                .get(name)
                .copied()
                .unwrap_or(GqlParameterType::Int64),
            _ => GqlParameterType::Int64,
        };
        if expected == GqlParameterType::UInt64 {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected("signed or canonical scalar assignment"),
            ));
        }
        self.number(expected).map(Operand::Number)
    }

    /// SET shares the read value grammar, including temporal constructors,
    /// list/map access, quantifiers and reduce. It still lowers ordinary
    /// scalar operands to their existing mutation action variants. Preserve
    /// the mutation API's scalar diagnostics when the shared reader refuses.
    fn mutation_assignment(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
    ) -> Result<ReadValueTemplate, GraphMutationTextError> {
        self.read_graph_value(columns, 0).map_err(|source| {
            let kind = match source.kind {
                crate::GraphSetTextErrorKind::Pattern(kind) => {
                    GraphMutationTextErrorKind::Query(kind)
                }
                crate::GraphSetTextErrorKind::IntegerExpression(kind) => {
                    GraphMutationTextErrorKind::IntegerExpression(kind)
                }
                crate::GraphSetTextErrorKind::IntegerOperand => {
                    GraphMutationTextErrorKind::IntegerOperand
                }
                crate::GraphSetTextErrorKind::IntegerNesting { limit } => {
                    GraphMutationTextErrorKind::IntegerNesting { limit }
                }
                kind => GraphMutationTextErrorKind::Relation(kind),
            };
            GraphMutationTextError {
                offset: source.offset,
                kind,
            }
        })
    }

    /// `row` is the WITH row of a write after WITH (fgdb-sl4ym): targets are
    /// the elements it carries and values read it in the value language.
    fn mutation_actions(
        &mut self,
        row: Option<&[(Name<'a>, crate::GraphSetColumnType)]>,
    ) -> Result<(Vec<Projection<'a>>, Vec<Action<'a>>), GraphMutationTextError> {
        let mut columns = Vec::new();
        let mut actions = Vec::new();
        let mut deletion_mode = None;
        loop {
            let at = self.current.at;
            // (setting, deleting, detach)
            let (setting, deleting, detach) = if self.take_word("SET")? {
                (true, false, false)
            } else if self.take_word("REMOVE")? {
                (false, false, false)
            } else if self.take_word("DETACH")? {
                self.word("DELETE")?;
                (false, true, true)
            } else if self.take_word("DELETE")? {
                (false, true, false)
            } else {
                if actions.is_empty() {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("SET, REMOVE, DELETE or DETACH DELETE"),
                    )
                    .into());
                }
                break;
            };
            match deletion_mode {
                Some((previous, _)) if previous != deleting => {
                    return Err(build_error(
                        at,
                        GraphMutationBuildError::MixedDeletionAndUpdates,
                    ));
                }
                Some((true, previous)) if previous != detach => {
                    return Err(build_error(at, GraphMutationBuildError::MixedDeleteModes));
                }
                _ => {}
            }
            deletion_mode = Some((deleting, detach));
            loop {
                self.mutation_action_capacity(actions.len())?;
                // After WITH, a DELETE of anything but a carried vertex, or
                // without DETACH a carried relationship, named bare deletes a
                // value of the row: `friends[$i]`, `map.key`, a carried path
                // (TCK Delete5).
                if deleting
                    && let Some(row) = row
                    && !self.row_element_target(row, detach)?
                {
                    let at = self.current.at;
                    let value = self.insertion_source_value(&mut columns, row)?;
                    actions.push(Action {
                        target: 0,
                        kind: ActionKind::RowDelete { value, detach, at },
                    });
                    if !self.take(b',')? {
                        break;
                    }
                    continue;
                }
                // DETACH DELETE names a vertex. A plain DELETE, like SET and
                // REMOVE, names a vertex or one fixed-length relationship.
                // openCypher's property target is an atom, so `SET (n).name`
                // parenthesizes the variable (Set1 [3], [4]).
                let parenthesized = !deleting && self.parenthesized_property_target()?;
                let (variable, target, vertex) = match row {
                    Some(row) => {
                        let variable = self.name()?;
                        let (target, kind) = row_write_target(row, variable, detach)?;
                        // A dynamically typed target may hold a vertex: its
                        // label actions refuse a relationship when they run.
                        (variable, target, kind != crate::GraphSetColumnType::Edge)
                    }
                    None => {
                        // DELETE p and DETACH DELETE p name a matched path:
                        // its elements are the targets (TCK Delete3).
                        let path = deleting
                            && matches!(self.current.kind,
                                TokenKind::Word(word) if self.syntax.names_path(word));
                        let variable = if path {
                            self.name()?
                        } else if detach {
                            self.variable()?
                        } else {
                            self.property_variable()?
                        };
                        let target = self.mutation_projection(&mut columns, variable, None)?;
                        let vertex = self
                            .syntax
                            .variables
                            .iter()
                            .any(|name| name.text == variable.text);
                        (variable, target, vertex)
                    }
                };
                if parenthesized {
                    self.punct(b')', ")")?;
                }
                if setting && self.take(b'+')? {
                    self.mutation_literal_map(&mut columns, &mut actions, target, row)?;
                    if !self.take(b',')? {
                        break;
                    }
                    continue;
                }
                // openCypher `SET n = {...}` replaces n's properties (Set4).
                if setting && row.is_none() && self.is_punct(b'=') {
                    self.mutation_replace_map(&mut columns, &mut actions, target, variable)?;
                    if !self.take(b',')? {
                        break;
                    }
                    continue;
                }
                let kind = if deleting {
                    ActionKind::Delete { detach }
                } else if self.take(b':')? {
                    if !vertex {
                        return Err(error(
                            variable.at,
                            GraphPatternTextErrorKind::Expected("vertex label target"),
                        )
                        .into());
                    }
                    // `n:A:B` sets or removes each label, one action apiece
                    // in the statement's action allowance (openCypher Set3,
                    // Remove2).
                    let mut label = self.schema_name()?;
                    while self.take(b':')? {
                        actions.push(Action {
                            target,
                            kind: ActionKind::Label {
                                label,
                                present: setting,
                            },
                        });
                        self.mutation_action_capacity(actions.len())?;
                        label = self.schema_name()?;
                    }
                    ActionKind::Label {
                        label,
                        present: setting,
                    }
                } else {
                    self.punct(b'.', "property or label mutation")?;
                    let key = self.schema_name()?;
                    match row {
                        Some(row) if setting => {
                            self.punct(b'=', "=")?;
                            let at = self.current.at;
                            let value = self.insertion_source_value(&mut columns, row)?;
                            ActionKind::RowProperty { key, value, at }
                        }
                        _ if setting => {
                            self.punct(b'=', "=")?;
                            if self.set_value_is_list()? {
                                // A list value (`[1, 2]`, `n.tags + ['x']`)
                                // reads in the value language over the MATCH
                                // columns, a `+` chain through the list sum,
                                // and is written as the stored List it is
                                // (fgdb-k3nub).
                                let at = self.current.at;
                                let value = self.read_graph_value(&mut columns, 0)?;
                                ActionKind::RowProperty { key, value, at }
                            } else {
                                ActionKind::Property {
                                    key,
                                    value: Some(self.mutation_assignment(&mut columns)?),
                                }
                            }
                        }
                        _ => ActionKind::Property { key, value: None },
                    }
                };
                actions.push(Action { target, kind });
                if !self.take(b',')? {
                    break;
                }
            }
        }
        Ok((columns, actions))
    }

    /// A MERGE branch's constant list value (fgdb-k3nub), evaluated once
    /// when the statement binds: the stored value it writes, by CREATE's rule
    /// (to_written_property). `None` refuses a map at any depth, a graph
    /// element, or a value past the event budget or the stored bounds.
    /// Binding is not a budgeted query, so a literal like range(1, 100000000)
    /// refuses here instead of allocating.
    fn constant_written_value(
        template: &crate::set_text::ReadValueTemplate,
        values: &[crate::GqlParameterValue],
    ) -> Result<Option<fgdb_types::CanonicalPropertyValue>, crate::GraphSetTextError> {
        const CONSTANT_LIST_EVENTS: u32 = 1 << 20;
        let value = return_projection::bind_read_value(template, values)?;
        let row = crate::algebra::GraphValueRow::from_owned_values(Vec::new());
        let mut remaining = CONSTANT_LIST_EVENTS;
        Ok(crate::set_ops::evaluate_value(&value, &row, 0, &mut |_| {
            remaining = remaining.checked_sub(1).ok_or(())?;
            Ok::<_, ()>(())
        })
        .ok()
        .and_then(|value| value.to_written_property()))
    }

    /// Whether a SET value here is a list (fgdb-k3nub), read in the value
    /// language and written as a stored List: a list operand or a `+` chain
    /// with one (list_operand_here, list_chain_follows), or a parameter the
    /// statement declares as a List (`SET n.tags = $tags`).
    fn set_value_is_list(&self) -> Result<bool, GraphPatternTextError> {
        if let TokenKind::Parameter(name) = self.current.kind
            && matches!(
                self.parameter_types.get(name),
                Some(crate::GqlParameterType::List)
            )
        {
            return Ok(true);
        }
        Ok(self.list_operand_here(return_projection::Route::NONE)?
            || self.list_chain_follows(return_projection::Route::NONE)?)
    }

    /// `SET n = {k: v, ...}` (openCypher replace, Set4): the listed
    /// assignments, plus one action unsetting every other property n carries,
    /// read from n's frozen property map. `{}` unsets them all.
    fn mutation_replace_map(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
        actions: &mut Vec<Action<'a>>,
        target: usize,
        variable: Name<'a>,
    ) -> Result<(), GraphMutationTextError> {
        self.punct(b'=', "=")?;
        self.punct(b'{', "literal property map after =")?;
        let current = self.projection_slot(
            columns,
            Projection {
                variable,
                property: None,
                path: Some(GraphPathFunction::Properties),
                collect: None,
            },
        )?;
        let mut keep: Vec<Name<'a>> = Vec::new();
        if !self.take(b'}')? {
            loop {
                self.mutation_action_capacity(actions.len())?;
                let key = self.schema_name()?;
                if keep.iter().any(|old| old.text == key.text) {
                    return Err(error(
                        key.at,
                        GraphPatternTextErrorKind::Expected("distinct property keys in SET = map"),
                    )
                    .into());
                }
                self.punct(b':', ":")?;
                let value = self.mutation_assignment(columns)?;
                actions.push(Action {
                    target,
                    kind: ActionKind::Property {
                        key,
                        value: Some(value),
                    },
                });
                keep.push(key);
                if self.take(b'}')? {
                    break;
                }
                self.punct(b',', ", or }")?;
            }
        }
        self.mutation_action_capacity(actions.len())?;
        actions.push(Action {
            target,
            kind: ActionKind::Clear { keep, current },
        });
        Ok(())
    }

    /// `( name ) .` opens a property target; consumes only the parenthesis.
    fn parenthesized_property_target(&mut self) -> Result<bool, GraphPatternTextError> {
        if !self.is_punct(b'(') {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        let shape = matches!(lexer.next()?.kind, TokenKind::Word(_))
            && matches!(lexer.next()?.kind, TokenKind::Punct(b')'))
            && matches!(lexer.next()?.kind, TokenKind::Punct(b'.'));
        if shape {
            self.advance()?;
        }
        Ok(shape)
    }

    fn mutation_action_capacity(&self, count: usize) -> Result<(), GraphMutationTextError> {
        if count >= MAX_GRAPH_MUTATION_ACTIONS {
            return Err(build_error(
                self.current.at,
                GraphMutationBuildError::TooManyActions {
                    limit: MAX_GRAPH_MUTATION_ACTIONS,
                    observed: count + 1,
                },
            ));
        }
        Ok(())
    }

    // A literal map is syntax for simultaneous property assignments, not a
    // runtime map value. Every field uses the original expression compiler and
    // consumes one slot in the whole statement's existing action allowance.
    fn mutation_literal_map(
        &mut self,
        columns: &mut Vec<Projection<'a>>,
        actions: &mut Vec<Action<'a>>,
        target: usize,
        row: Option<&[(Name<'a>, crate::GraphSetColumnType)]>,
    ) -> Result<(), GraphMutationTextError> {
        self.punct(b'=', "=")?;
        let at = self.current.at;
        self.punct(b'{', "literal property map after +=")?;
        if self.is_punct(b'}') {
            return Err(build_error(at, GraphMutationBuildError::EmptyActions));
        }
        let mut keys = std::collections::BTreeSet::new();
        loop {
            self.mutation_action_capacity(actions.len())?;
            let key = self.schema_name()?;
            if !keys.insert(key.text) {
                return Err(error(
                    key.at,
                    GraphPatternTextErrorKind::Expected("distinct property keys in SET += map"),
                )
                .into());
            }
            self.punct(b':', ":")?;
            let at = self.current.at;
            // An entry's list value is a stored List, as in `SET n.p = [..]`.
            let listed = row.is_none() && self.set_value_is_list()?;
            let kind = if let Some(row) = row {
                let value = self.insertion_source_value(columns, row)?;
                ActionKind::RowProperty { key, value, at }
            } else if listed {
                let value = self.read_graph_value(columns, 0)?;
                ActionKind::RowProperty { key, value, at }
            } else {
                ActionKind::Property {
                    key,
                    value: Some(self.mutation_assignment(columns)?),
                }
            };
            // A field's map construction is never a written property: refuse
            // it here, before any catalog access. A list field is a stored
            // List (fgdb-k3nub). A subscripted or computed value stays a
            // per-row check.
            let map = |value: &ReadValueTemplate| {
                matches!(
                    value,
                    ReadValueTemplate::MapLiteral { .. } | ReadValueTemplate::MapOverlay { .. }
                )
            };
            let refused = match &kind {
                ActionKind::Property {
                    value: Some(value), ..
                } => map(value),
                ActionKind::RowProperty { value, .. } => map(value),
                _ => false,
            };
            if refused {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected("a scalar value in SET += map"),
                )
                .into());
            }
            actions.push(Action { target, kind });
            if self.take(b'}')? {
                break;
            }
            self.punct(b',', ", or }")?;
        }
        Ok(())
    }
}

impl PreparedGraphText {
    pub(crate) fn deletion_feeds_later_match(&self) -> bool {
        !self.builder.is_single_vertex_selection() || !self.scopes.is_empty()
    }
}

impl PreparedGraphMutationText {
    /// Prepare simultaneous SET/REMOVE, plain DELETE or explicit DETACH DELETE
    /// after the existing MATCH/WALK/WHERE/OPTIONAL grammar. SET stores
    /// canonical NULL; REMOVE unsets a property. A plain DELETE stays
    /// non-detaching: the write host proves its vertices isolated. CREATE,
    /// RETURN and mixed deletion/update or DELETE/DETACH DELETE statements are
    /// not silently reinterpreted.
    ///
    /// `SET n += {p: expression, q: $value}` updates the named vertex or edge
    /// properties through the same simultaneous assignments as `SET n.p = ...`.
    /// All RHS values read the pre-statement graph; omitted properties remain
    /// unchanged and NULL remains a stored scalar, with REMOVE used for absence.
    /// Each field counts toward the statement's ordinary action limit. Keys are
    /// property identifiers and must be distinct within each literal map. Empty
    /// maps, dynamic map arguments and map replacement are refused. RHS values
    /// use the read value grammar, but only scalar results may be stored.
    ///
    /// RHS arithmetic supports checked nullable i64 +, -, *, /, %, unary signs,
    /// parentheses, ABS, NULLIF and COALESCE with at least two arguments. Division
    /// truncates toward zero. COALESCE skips unneeded arithmetic but the frozen
    /// GLA projection still performs all requested property reads. Plain scalar
    /// assignments retain their existing types; arithmetic never coerces them.
    /// Temporal constructors, collection access, list quantifiers and reduce
    /// use the shared bounded value evaluator over that same frozen row. A
    /// named timezone requires the resolver's pinned tzdb. A non-scalar result
    /// or a late expression error returns no mutation batch.
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphMutationTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    /// Types are supplied once for the whole read/write statement. Numeric and
    /// canonical RHS arguments share the WHERE schema and original offsets.
    /// Every (kind,name) resolves once across MATCH scopes and mutation actions.
    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphMutationTextError> {
        Self::prepare_definition(statement, relation, declarations, resolve, false)
            .map(|(mutation, _)| mutation)
    }

    /// The mutation and, in query mode, its terminal RETURN, parsed in one
    /// pass so RETURN-only reads join the same frozen selection.
    pub(crate) fn prepare_definition(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
        returning: bool,
    ) -> Result<(Self, Option<MutationReturnTemplate>), GraphMutationTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        // UNWIND ... MATCH ... SET/REMOVE/DELETE (fgdb-0s2pq): the write reads
        // the rows of the UNWIND stages and the MATCH they correlate, the
        // read an `UNWIND ... MATCH ... CREATE` takes, as a WITH row.
        let unwound = if parser.is_word("UNWIND") {
            let (source, schema) =
                parser.write_graph_row_prefix(statement, declarations, resolve.time_zones())?;
            parser.write_row_scope = true;
            Some((source, schema))
        } else {
            None
        };
        // A WHERE the graph predicate cannot hold (a label test under OR or
        // NOT, a predicate over list values) reads as `MATCH ... WITH *
        // WHERE ...`.
        let (optional_root, deferred) = if unwound.is_none() {
            parser.defer_where = true;
            let optional = parser.parse_mutation_match_prefix();
            parser.defer_where = false;
            (optional?, parser.resume_deferred_where())
        } else {
            (false, false)
        };
        // MATCH ... WITH ... SET/REMOVE/DELETE (fgdb-sl4ym): the write reads
        // the read pipeline's row through its last WITH stage, and only it.
        let row_prefix =
            if unwound.is_none() && !optional_root && (deferred || parser.is_word("WITH")) {
                let prefix = if deferred {
                    parser.implicit_write_row_prefix()?
                } else {
                    parser.write_row_prefix()?
                };
                parser.write_row_scope = true;
                Some(prefix)
            } else {
                None
            };
        let row = match (&unwound, &row_prefix) {
            (Some((_, schema)), _) | (None, Some((_, _, schema))) => Some(schema.clone()),
            (None, None) => None,
        };
        let (mut projections, parsed_actions) = parser.mutation_actions(row.as_deref())?;
        let returning = if returning {
            let scope = mutation_query::ReturnScope {
                labels_changed: parsed_actions.iter().any(|action| {
                    !matches!(
                        action.kind,
                        ActionKind::Property { .. }
                            | ActionKind::RowProperty { .. }
                            | ActionKind::Clear { .. }
                    )
                }),
                // Only DETACH DELETE can retire an edge the statement does
                // not name. Computed targets can cascade too. A plain DELETE's
                // edges are its own targets, so reading one is the runtime
                // DeletedElementRead refusal.
                deleting: parsed_actions.iter().any(|action| {
                    matches!(
                        action.kind,
                        ActionKind::Delete { detach: true }
                            | ActionKind::RowDelete { detach: true, .. }
                    )
                }),
            };
            Some(parser.mutation_return(&mut projections, scope, row.as_deref())?)
        } else {
            None
        };
        parser.end()?;
        parser.write_row_scope = false;
        if let Some(returning) = &returning {
            returning.admit(&parser.syntax.parameters)?;
        }
        if unwound.is_some() || row_prefix.is_some() {
            if !projections.is_empty() {
                return Err(error(
                    statement.len(),
                    GraphPatternTextErrorKind::Expected("a write that reads only the WITH row"),
                )
                .into());
            }
            let read = match (unwound, row_prefix) {
                // The actions and RETURN may add parameters and element keys
                // after the read boundary; the read binds the complete table.
                (Some((mut source, _)), _) => {
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
                }
                (None, Some((head, stages, _))) => {
                    parser.finish_write_row_prefix(statement, head, stages)?
                }
                (None, None) => unreachable!("one row source was checked above"),
            };
            let mut row = read.resolve(crate::graph_text::LentResolver(&mut resolve))?;
            let metadata = row.selection.as_ref().is_some_and(|selection| {
                selection.columns.iter().any(|column| {
                    matches!(
                        column.path,
                        Some(
                            GraphPathFunction::Labels
                                | GraphPathFunction::Type
                                | GraphPathFunction::Properties
                        )
                    )
                })
            });
            if metadata && let Some(selection) = row.selection.as_mut() {
                selection.reverse_catalog = Some(std::sync::Arc::new(
                    ReverseSymbolCatalog::from_resolver(&mut resolve, statement),
                ));
            }
            let mut cache = BTreeMap::new();
            let mut symbol = |kind, name: Name<'_>| -> Result<GraphSymbol, GraphPatternTextError> {
                let key = (kind, name.text.to_owned());
                if let Some(value) = cache.get(&key) {
                    return Ok(*value);
                }
                let value = resolve.resolve_symbol(kind, name.text).ok_or_else(|| {
                    error(name.at, GraphPatternTextErrorKind::UnknownSymbol(kind))
                })?;
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
            let actions = action_templates(parsed_actions, &mut symbol)?;
            let returning = returning
                .map(|returning| returning.resolve(&mut symbol))
                .transpose()?;
            let selection = row.selection.clone().ok_or_else(|| {
                error(
                    statement.len(),
                    GraphPatternTextErrorKind::Expected("a MATCH before the WITH"),
                )
            })?;
            return Ok((
                Self {
                    selection,
                    relation,
                    actions,
                    optional_root: false,
                    row: Some(Box::new(row)),
                },
                returning,
            ));
        }
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
        let actions = action_templates(parsed_actions, &mut symbol)?;
        let returning = returning
            .map(|returning| returning.resolve(&mut symbol))
            .transpose()?;
        let mut columns = Vec::new();
        for (index, projection) in projections.into_iter().enumerate() {
            let key = if let Some(name) = projection.property {
                let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
                    unreachable!("symbol domain checked by the shared resolver")
                };
                Some(key)
            } else {
                None
            };
            // Generated aliases are immutable compiler metadata. No query text
            // is rewritten and user names/arguments never become syntax.
            columns.push(BoundColumn {
                alias: format!("_mutation_{index}"),
                variable: projection.variable.text.to_owned(),
                key,
                path: projection.path,
                collect: None,
            });
        }
        let clauses: Vec<_> = scopes.iter().map(BoundScope::clause).collect();
        let projected: Vec<_> = columns.iter().map(BoundColumn::declaration).collect();
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
            .then(|| {
                std::sync::Arc::new(ReverseSymbolCatalog::from_resolver(&mut resolve, statement))
            });
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
                selection,
                relation,
                actions,
                optional_root,
                row: None,
            },
            returning,
        ))
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        self.selection.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match &self.row {
            Some(row) => row.parameter_schema(),
            None => self.selection.parameter_schema(),
        }
    }

    /// The statement's validated argument row, in parameter order.
    pub(crate) fn checked_arguments(
        &self,
        arguments: &GqlParameters,
    ) -> Result<Vec<GqlParameterValue>, GraphMutationTextError> {
        Ok(match &self.row {
            Some(row) => row.checked_arguments(arguments)?,
            None => self.selection.checked_arguments(arguments)?,
        })
    }

    /// Only vertex field updates can use the frozen UNWIND input contract.
    /// Inspect resolved target columns, never aliases or current graph data.
    pub(crate) fn is_vertex_update_only(&self) -> bool {
        self.actions.iter().all(|action| {
            let target = match action {
                MutationActionTemplate::ParameterProperty { target, .. }
                | MutationActionTemplate::IntegerProperty { target, .. }
                | MutationActionTemplate::CompositeProperty { target, .. }
                | MutationActionTemplate::ValueProperty { target, .. }
                | MutationActionTemplate::Bound(
                    GraphMutationAction::SetProperty { target, .. }
                    | GraphMutationAction::RemoveProperty { target, .. }
                    | GraphMutationAction::ClearProperties { target, .. }
                    | GraphMutationAction::SetLabel { target, .. },
                ) => *target,
                MutationActionTemplate::Bound(
                    GraphMutationAction::DetachDelete { .. }
                    | GraphMutationAction::Delete { .. }
                    | GraphMutationAction::DeleteValue { .. },
                )
                | MutationActionTemplate::DeleteValue { .. } => return false,
            };
            self.selection
                .columns
                .get(target)
                .is_some_and(|column| column.key.is_none() && column.path.is_none())
        })
    }

    /// Whether sequential input-row execution could change a later row's
    /// MATCH. This is a conservative definition check, independent of current
    /// data and argument values, not a proof that aliases identify different
    /// elements. RHS-only projections do not restrict the MATCH domain.
    pub(crate) fn feeds_own_match(&self) -> bool {
        let writes_property = |read_kind, read| {
            self.actions.iter().any(|action| {
                let (target, key) = match action {
                    MutationActionTemplate::ParameterProperty { target, key, .. }
                    | MutationActionTemplate::IntegerProperty { target, key, .. }
                    | MutationActionTemplate::CompositeProperty { target, key, .. }
                    | MutationActionTemplate::ValueProperty { target, key, .. }
                    | MutationActionTemplate::Bound(
                        GraphMutationAction::SetProperty { target, key, .. }
                        | GraphMutationAction::RemoveProperty { target, key },
                    ) => (*target, *key),
                    // SET n = {map} clears every property it does not list,
                    // so it conservatively writes whatever key is read.
                    MutationActionTemplate::Bound(GraphMutationAction::ClearProperties {
                        target,
                        ..
                    }) => (*target, read),
                    MutationActionTemplate::Bound(
                        GraphMutationAction::SetLabel { .. }
                        | GraphMutationAction::DetachDelete { .. }
                        | GraphMutationAction::Delete { .. }
                        | GraphMutationAction::DeleteValue { .. },
                    )
                    | MutationActionTemplate::DeleteValue { .. } => return false,
                };
                if key != read {
                    return false;
                }
                // Action targets are resolved element columns, not RHS
                // projections. Unknown future target shapes stay conservative.
                let written_kind = self.selection.columns.get(target).and_then(|column| {
                    match (column.key, column.path) {
                        (None, None) => Some(crate::GraphSetColumnType::Vertex),
                        (None, Some(GraphPathFunction::Edge)) => {
                            Some(crate::GraphSetColumnType::Edge)
                        }
                        _ => None,
                    }
                });
                written_kind.is_none_or(|kind| kind == read_kind)
            })
        };
        let writes_label = |read| {
            self.actions.iter().any(|action| {
                matches!(action, MutationActionTemplate::Bound(
                    GraphMutationAction::SetLabel { label, .. }
                ) if *label == read)
            })
        };
        // Repeated deletion of a single matched vertex remains idempotent.
        // A cascade can remove another row's edge or independent anchor, so
        // a larger MATCH needs a frozen selection even without property SET.
        // The ordinary compiler forbids mixing deletion and updates.
        // A row-computed element (`DELETE friends[$i]`, fgdb-cpny9) is a
        // deletion too.
        if self.actions.iter().all(|action| {
            matches!(
                action,
                MutationActionTemplate::Bound(
                    GraphMutationAction::DetachDelete { .. }
                        | GraphMutationAction::Delete { .. }
                        | GraphMutationAction::DeleteValue { .. }
                ) | MutationActionTemplate::DeleteValue { .. }
            )
        }) {
            return self.selection.deletion_feeds_later_match();
        }
        let reads_written_field = |builder: &GraphPatternBuilder, filters: &[BoundFilter]| {
            builder.selects_on_any_field(writes_property, writes_label)
                || filters.iter().any(|filter| match filter {
                    BoundFilter::Property { key, .. } => {
                        writes_property(crate::GraphSetColumnType::Vertex, *key)
                    }
                    BoundFilter::Boolean(expression) => {
                        expression.selects_on_any_property(writes_property)
                    }
                    BoundFilter::PathLength { .. } => false,
                })
        };
        reads_written_field(&self.selection.builder, &self.selection.filters)
            || self
                .selection
                .scopes
                .iter()
                .any(|scope| reads_written_field(scope.builder(), scope.filters()))
    }

    /// Exact argument validation precedes all instantiation. No lexing,
    /// catalog lookup, storage observation or mutation occurs during binding.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphMutation, GraphMutationTextError> {
        let values = self.checked_arguments(arguments)?;
        let at = self.selection.return_at;
        let actions = Self::bind_actions(&self.actions, &values, at)?;
        if let Some(row) = &self.row {
            // The read relation's rows, each the WITH row its actions index.
            let input = row.bind_values(&values)?;
            return PreparedGraphMutation::prepare_relation(input, self.relation, actions)
                .map_err(|error| build_error(at, error));
        }
        let selection = self.selection.bind_values(&values)?;
        if !self.optional_root {
            return PreparedGraphMutation::prepare(selection, self.relation, actions)
                .map_err(|error| build_error(at, error));
        }
        // The unit relation left-joined with the pattern: its witnesses, or
        // one NULL-extended row. The left side is zero-width, so every action
        // still addresses the pattern's own column.
        let input = crate::PreparedGraphSet::from(selection);
        let spec = crate::row_join::RowJoinSpec::cross(&[], input.column_types())
            .map_err(|_| {
                error(
                    at,
                    GraphPatternTextErrorKind::Expected("a bounded OPTIONAL MATCH write source"),
                )
            })?
            .with_kind(crate::row_join::RowJoinKind::Left);
        let input = crate::PreparedGraphSet::singleton()
            .join(input, spec)
            .map_err(|error| build_error(at, GraphMutationBuildError::RelationalInput(error)))?;
        PreparedGraphMutation::prepare_relation(input, self.relation, actions)
            .map_err(|error| build_error(at, error))
    }

    /// Each action template, bound with the statement's checked arguments.
    fn bind_actions(
        templates: &[MutationActionTemplate],
        values: &[GqlParameterValue],
        at: usize,
    ) -> Result<Vec<GraphMutationAction>, GraphMutationTextError> {
        let mut actions = Vec::new();
        for action in templates {
            actions.push(match action {
                MutationActionTemplate::Bound(action) => action.clone(),
                MutationActionTemplate::ParameterProperty {
                    target,
                    key,
                    parameter,
                } => GraphMutationAction::SetProperty {
                    target: *target,
                    key: *key,
                    value: GraphMutationValue::Literal(scalar(values[*parameter].clone(), at)?),
                },
                MutationActionTemplate::IntegerProperty {
                    target,
                    key,
                    program,
                    at,
                } => GraphMutationAction::SetProperty {
                    target: *target,
                    key: *key,
                    value: GraphMutationValue::Expression(integer::bind_integer(
                        program, values, *at,
                    )?),
                },
                MutationActionTemplate::CompositeProperty { target, key, value } => {
                    GraphMutationAction::SetProperty {
                        target: *target,
                        key: *key,
                        value: GraphMutationValue::Composite(return_projection::bind_read_value(
                            value, values,
                        )?),
                    }
                }
                MutationActionTemplate::ValueProperty {
                    target,
                    key,
                    value,
                    at,
                } => GraphMutationAction::SetProperty {
                    target: *target,
                    key: *key,
                    value: GraphMutationValue::Composite(
                        return_projection::bind_read_value(value, values).map_err(|error| {
                            GraphMutationTextError {
                                offset: *at,
                                ..GraphMutationTextError::from(error)
                            }
                        })?,
                    ),
                },
                MutationActionTemplate::DeleteValue { value, detach, at } => {
                    GraphMutationAction::DeleteValue {
                        value: return_projection::bind_read_value(value, values).map_err(
                            |error| GraphMutationTextError {
                                offset: *at,
                                ..GraphMutationTextError::from(error)
                            },
                        )?,
                        detach: *detach,
                    }
                }
            });
        }
        Ok(actions)
    }
}

/// Where the next write of a chain starts, and the row it reads: the previous
/// write's WITH tail output, whose hidden boundary reads stay resolvable.
/// `parameters` are the types the statement gave its parameters so far, so a
/// parameter keeps one type across the chain (fgdb-cpny9).
pub(crate) struct ChainContinuation<'a> {
    at: usize,
    row: Vec<(Name<'a>, crate::GraphSetColumnType)>,
    boundary: Option<BoundaryReads<'a>>,
    parameters: Vec<GqlParameterSpec>,
}

impl<'a> ChainContinuation<'a> {
    /// The row the next write reads, as owned-leaf columns.
    fn owned_columns(&self) -> (Vec<String>, Vec<crate::GraphSetColumnType>) {
        self.row
            .iter()
            .map(|(name, kind)| (name.text.to_owned(), *kind))
            .unzip()
    }
}

/// One ON-less SET, REMOVE or DELETE step of a chain over `row`, through the
/// end of its RETURN, or through the WITH head (implicit before a write
/// that directly follows) whose rows the next write reads.
type ChainMutation<'a> = (
    Vec<Projection<'a>>,
    Vec<Action<'a>>,
    Option<mutation_query::ParsedMutationReturn<'a>>,
    Option<ChainContinuation<'a>>,
);

impl<'a> Parser<'a> {
    fn chain_mutation(
        &mut self,
        row: &[(Name<'a>, crate::GraphSetColumnType)],
    ) -> Result<ChainMutation<'a>, GraphMutationTextError> {
        self.write_row_scope = true;
        self.chain_write = true;
        let (mut projections, parsed_actions) = self.mutation_actions(Some(row))?;
        let returning =
            if self.is_word("RETURN") || self.is_word("WITH") || self.at_chained_write()? {
                let scope = mutation_query::ReturnScope {
                    labels_changed: parsed_actions.iter().any(|action| {
                        !matches!(
                            action.kind,
                            ActionKind::Property { .. }
                                | ActionKind::RowProperty { .. }
                                | ActionKind::Clear { .. }
                        )
                    }),
                    deleting: parsed_actions.iter().any(|action| {
                        matches!(
                            action.kind,
                            ActionKind::Delete { detach: true }
                                | ActionKind::RowDelete { detach: true, .. }
                        )
                    }),
                };
                Some(self.mutation_return(&mut projections, scope, Some(row))?)
            } else {
                None
            };
        let continuation = self.continuation();
        if continuation.is_none() {
            self.end()?;
        }
        self.write_row_scope = false;
        Ok((projections, parsed_actions, returning, continuation))
    }
}

impl crate::mutation_text::PreparedGraphMutationRowsText {
    /// The SET, REMOVE or DELETE of a chain over the row the previous write
    /// left (`CREATE (n:P) SET n.name = $name RETURN n.name`, fgdb-cpny9).
    /// A RETURN or the end of the statement may follow it, or a WITH head or
    /// another write, which then reads its post-write rows. Its actions are
    /// the WITH-row actions of `MATCH ... WITH ... SET` (fgdb-sl4ym), over
    /// that row instead of a read pipeline.
    pub(crate) fn prepare_chained<'a, R>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
        from: ChainContinuation<'a>,
    ) -> Result<
        (
            Self,
            Option<MutationReturnTemplate>,
            Option<ChainContinuation<'a>>,
        ),
        GraphMutationTextError,
    >
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let (columns, types) = from.owned_columns();
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones())
            .resume_after(from.boundary, &from.parameters, from.at)?;
        let at = parser.current.at;
        let row = from.row;
        let (projections, parsed_actions, returning, continuation) = parser.chain_mutation(&row)?;
        if let Some(returning) = &returning {
            returning.admit(&parser.syntax.parameters)?;
        }
        if !projections.is_empty() {
            return Err(error(
                statement.len(),
                GraphPatternTextErrorKind::Expected("a write that reads only the WITH row"),
            )
            .into());
        }
        // This owned-row step builds its bound input directly, bypassing
        // UnresolvedGraphText::resolve. Its computed element fields still
        // need the same catalog binding before actions or RETURN are bound.
        for (key, slot) in &parser.syntax.element_keys {
            slot.resolve(
                match resolve.resolve_symbol(GraphSymbolKind::Property, key) {
                    Some(GraphSymbol::Property(key)) => Some(key),
                    _ => None,
                },
            );
        }
        let input = crate::set_text::BoundSetTextInput {
            selection: None,
            parameters: parser.syntax.parameters.clone(),
            parameter_offsets: parser.syntax.parameter_offsets.clone(),
            return_at: statement.len(),
            projection: None,
            quantifier: crate::GraphSetQuantifier::All,
            pipeline: Vec::new(),
            singleton: false,
            leading: Vec::new(),
            correlations: Vec::new(),
            row_fields: Vec::new(),
            owned: Some(Box::new((columns, types))),
        };
        let (step, returning) =
            Self::resolve_chain_step(resolve, relation, at, input, parsed_actions, returning)?;
        Ok((step, returning, continuation))
    }

    /// A SET, REMOVE or DELETE over a leading MATCH as the first write of a
    /// chain another write follows (`MATCH (a)-[r:OLD]->(b) DELETE r CREATE
    /// (a)-[:NEW]->(b)`). The MATCH and its WITH stages are its row source,
    /// as for a relationship MERGE first step; its rows reach the next write
    /// through its WITH head, implicit before a write that directly follows.
    pub(crate) fn prepare_first_step<'a, R>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<
        (
            Self,
            Option<MutationReturnTemplate>,
            Option<ChainContinuation<'a>>,
        ),
        GraphMutationTextError,
    >
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        // A WHERE the graph predicate cannot hold is the row filter of the
        // implicit `WITH *` below.
        parser.write_match_prefix()?;
        let (head, stages, schema) = if parser.is_word("WITH") {
            parser.write_row_prefix()?
        } else {
            parser.implicit_write_row_prefix()?
        };
        let at = parser.current.at;
        let (projections, parsed_actions, returning, continuation) =
            parser.chain_mutation(&schema)?;
        if let Some(returning) = &returning {
            returning.admit(&parser.syntax.parameters)?;
        }
        if !projections.is_empty() {
            return Err(error(
                statement.len(),
                GraphPatternTextErrorKind::Expected("a write that reads only the MATCH row"),
            )
            .into());
        }
        let read = parser.finish_write_row_prefix(statement, head, stages)?;
        let mut input = read.resolve(crate::graph_text::LentResolver(&mut *resolve))?;
        // Graph metadata the row reads names catalog symbols, as in
        // `MATCH ... WITH ... SET`.
        let metadata = input.selection.as_ref().is_some_and(|selection| {
            selection.columns.iter().any(|column| {
                matches!(
                    column.path,
                    Some(
                        GraphPathFunction::Labels
                            | GraphPathFunction::Type
                            | GraphPathFunction::Properties
                    )
                )
            })
        });
        if metadata && let Some(selection) = input.selection.as_mut() {
            selection.reverse_catalog = Some(std::sync::Arc::new(
                ReverseSymbolCatalog::from_resolver(&mut *resolve, statement),
            ));
        }
        let (step, returning) =
            Self::resolve_chain_step(resolve, relation, at, input, parsed_actions, returning)?;
        Ok((step, returning, continuation))
    }

    /// Bind a chain mutation step's actions and its RETURN or WITH head to
    /// the catalog, over its resolved input.
    fn resolve_chain_step<'a, R>(
        resolve: &mut R,
        relation: RelationId,
        at: usize,
        input: crate::set_text::BoundSetTextInput,
        parsed_actions: Vec<Action<'a>>,
        returning: Option<mutation_query::ParsedMutationReturn<'a>>,
    ) -> Result<(Self, Option<MutationReturnTemplate>), GraphMutationTextError>
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
        let actions = action_templates(parsed_actions, &mut symbol)?;
        let returning = returning
            .map(|returning| returning.resolve(&mut symbol))
            .transpose()?;
        Ok((
            Self {
                relation,
                actions,
                input,
                at,
            },
            returning,
        ))
    }

    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.input.parameter_schema()
    }

    /// The previous write's row this step reads.
    pub(crate) fn owned_column_types(&self) -> Option<&[crate::GraphSetColumnType]> {
        self.input
            .owned
            .as_deref()
            .map(|(_, types)| types.as_slice())
    }

    /// The mutation over the owned rows, with these exact arguments.
    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphMutation, GraphMutationTextError> {
        let values = self.input.checked_arguments(arguments)?;
        self.bind_values(&values)
    }

    fn bind_values(
        &self,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphMutation, GraphMutationTextError> {
        let actions = PreparedGraphMutationText::bind_actions(&self.actions, values, self.at)?;
        let input = self.input.bind_values(values)?;
        // A first step reads its MATCH; a later one only the previous
        // write's rows.
        if input.reads_graph() {
            PreparedGraphMutation::prepare_relation(input, self.relation, actions)
        } else {
            PreparedGraphMutation::prepare_owned(input, self.relation, actions)
        }
        .map_err(|error| build_error(self.at, error))
    }

    /// The mutation and its RETURN over the owned rows.
    pub(crate) fn bind_query(
        &self,
        returning: &MutationReturnTemplate,
        arguments: &GqlParameters,
    ) -> Result<crate::PreparedGraphMutationQuery, GraphMutationTextError> {
        let values = self.input.checked_arguments(arguments)?;
        returning.bind(self.bind_values(&values)?, &values)
    }
}

impl<'a> Parser<'a> {
    /// At `MERGE`: whether its pattern is a relationship, `(a)-` or `(a)<-`,
    /// read with native tokens and without consuming the parser's own.
    pub(super) fn at_relationship_merge(&self) -> Result<bool, GraphPatternTextError> {
        if !self.is_word("MERGE") {
            return Ok(false);
        }
        let mut lexer = self.lexer.clone();
        Ok(matches!(lexer.next()?.kind, TokenKind::Punct(b'('))
            && matches!(lexer.next()?.kind, TokenKind::Word(_))
            && matches!(lexer.next()?.kind, TokenKind::Punct(b')'))
            && matches!(lexer.next()?.kind, TokenKind::Punct(b'-' | b'<')))
    }

    /// A terminal graph read continues the write's private occurrence bag.
    /// ON MATCH is consumed by the MERGE owner before this boundary is asked.
    pub(super) fn at_chained_read(&self) -> Result<bool, GraphPatternTextError> {
        Ok(self.is_word("MATCH")
            || (self.is_word("OPTIONAL")
                && matches!(self.lexer.clone().next()?.kind,
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("MATCH"))))
    }

    /// A next write or terminal graph read over the carried row (fgdb-cpny9).
    /// The chain compiler dispatches the read to its native relational owner.
    pub(super) fn at_chained_write(&self) -> Result<bool, GraphPatternTextError> {
        Ok(self.at_chained_read()?
            || self.is_word("CREATE")
            || (self.is_word("MERGE")
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'(')))
            || self.is_word("SET")
            || self.is_word("REMOVE")
            || self.is_word("DELETE")
            || (self.is_word("DETACH")
                && matches!(self.lexer.clone().next()?.kind,
                    TokenKind::Word(word) if word.eq_ignore_ascii_case("DELETE"))))
    }

    /// The continuation a chained WITH tail left, at the next write.
    fn continuation(&mut self) -> Option<ChainContinuation<'a>> {
        let row = self.chained_row.take()?;
        Some(ChainContinuation {
            at: self.current.at,
            row,
            boundary: self.boundary_reads.take(),
            parameters: self.syntax.parameters.clone(),
        })
    }

    /// Start a later write of a chain at its own clause, in the scope the
    /// previous write's tail left: its hidden reads and parameter types.
    fn resume_after(
        mut self,
        boundary: Option<BoundaryReads<'a>>,
        parameters: &[GqlParameterSpec],
        at: usize,
    ) -> Result<Self, GraphPatternTextError> {
        for spec in parameters {
            self.parameter_types
                .entry(spec.name.clone())
                .or_insert(spec.parameter_type);
        }
        self.lexer.at = at;
        self.lexer.tokens = 0;
        self.current = self.lexer.next()?;
        self.boundary_reads = boundary;
        Ok(self)
    }
}

#[cfg(test)]
mod graph_function_tests {
    use super::*;
    use crate::{GraphSetColumnType, PreparedGraphSetText};

    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(RelationId(1))),
            (GraphSymbolKind::Property, "cost") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            _ => None,
        }
    }

    /// openCypher Set1 [3], [4]: a parenthesized variable is the same property
    /// target as the bare one, for SET and REMOVE, vertex and relationship.
    /// A label or map mutation still takes only the bare variable.
    #[test]
    fn a_parenthesized_property_target_is_the_bare_target() {
        let bound = |text: &str| {
            crate::PreparedGraphMutationText::prepare(text, RelationId(1), symbols)
                .and_then(|prepared| prepared.bind_parameters(&crate::GqlParameters::new()))
                .map_err(|error| format!("{text}: {error:?}"))
        };
        for (parenthesized, bare) in [
            ("MATCH (n) SET (n).cost = 1", "MATCH (n) SET n.cost = 1"),
            (
                "MATCH (a)-[r:R]->() SET (r).cost = 2, (a).cost = 3",
                "MATCH (a)-[r:R]->() SET r.cost = 2, a.cost = 3",
            ),
            ("MATCH (n) REMOVE (n).cost", "MATCH (n) REMOVE n.cost"),
        ] {
            assert_eq!(bound(parenthesized).unwrap(), bound(bare).unwrap());
        }
        for refused in [
            "MATCH (n) SET (n):L",
            "MATCH (n) SET (n) += {cost: 1}",
            "MATCH (n) SET (m).cost = 1",
            "MATCH (n) SET ((n)).cost = 1",
            "MATCH (n) DELETE (n).cost",
        ] {
            assert!(bound(refused).is_err(), "{refused}");
        }
    }

    #[test]
    fn source_slots_distinguish_path_values_and_each_metadata_function() {
        let mut parser =
            Parser::new("MATCH p = (a)-[:R]->{1,2}(b) RETURN path_length(p) + PATH_LENGTH(p)")
                .unwrap();
        parser.parse_match_prefix().unwrap();
        parser.word("RETURN").unwrap();
        let mut columns = Vec::new();
        assert!(matches!(
            parser.mutation_expression(&mut columns).unwrap(),
            Operand::Integer { .. }
        ));
        parser.end().unwrap();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].path, Some(GraphPathFunction::Length));
        let path = parser.syntax.path.unwrap();
        let entity = parser
            .mutation_projection(&mut columns, path, None)
            .unwrap();
        assert_eq!(entity, 1);
        assert_eq!(columns[entity].path, Some(GraphPathFunction::Value));
        for (function, expected) in [
            (GraphPathFunction::Nodes, 2),
            (GraphPathFunction::Edges, 3),
            (GraphPathFunction::Length, 0),
        ] {
            assert_eq!(
                parser
                    .projection_slot(
                        &mut columns,
                        Projection {
                            variable: path,
                            property: None,
                            path: Some(function),
                            collect: None,
                        },
                    )
                    .unwrap(),
                expected
            );
        }
        assert_eq!(columns.len(), 4);
    }

    #[test]
    fn computed_returns_and_lists_bind_native_graph_function_columns() {
        let text = "MATCH p = (a)-[:R]->{1,2}(b) RETURN \
                    path_length(p) + 1 AS score, p AS route, \
                    nodes(p) AS vertices, edges(p) AS relationships, \
                    [path_length(p), path_length(p) + 1] AS lengths";
        let prepared = PreparedGraphSetText::prepare(text, symbols).unwrap();
        let expected = [
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Path,
            GraphSetColumnType::Vertices,
            GraphSetColumnType::Edges,
            GraphSetColumnType::List,
        ];
        assert_eq!(prepared.column_types(), &expected);
        let bound = prepared.bind_parameters(&GqlParameters::new()).unwrap();
        assert_eq!(bound.column_types(), &expected);
        let prepared = PreparedGraphSetText::prepare(
            "MATCH (a)-[e:R]->(b) RETURN labels(a) AS names, type(e) AS relation, \
             type(e) || '!' AS decorated, e AS edge",
            symbols,
        )
        .unwrap();
        let expected = [
            GraphSetColumnType::List,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Scalar,
            GraphSetColumnType::Edge,
        ];
        assert_eq!(prepared.column_types(), &expected);
        assert_eq!(
            prepared
                .bind_parameters(&GqlParameters::new())
                .unwrap()
                .column_types(),
            &expected
        );
    }

    #[test]
    fn path_length_assignments_share_the_existing_typed_parameter_contract() {
        let prepared = PreparedGraphMutationText::prepare(
            "MATCH p = (a)-[:R]->{1,2}(b) SET b.cost = path_length(p) + $extra",
            RelationId(1),
            symbols,
        )
        .unwrap();
        assert_eq!(prepared.parameter_schema().len(), 1);
        assert_eq!(prepared.parameter_schema()[0].name, "extra");
        let arguments = GqlParameters::new().with_int64("extra", 1).unwrap();
        assert!(prepared.bind_parameters(&arguments).is_ok());
    }

    #[test]
    fn graph_function_domains_refuse_before_catalog_access() {
        for expression in [
            "nodes(a)",
            "edges(a)",
            "path_length(a)",
            "labels(p)",
            "type(a)",
            "nodes(missing)",
            "path_length(p, p)",
        ] {
            let text =
                format!("MATCH p = (a)-[:R]->{{1,2}}(b) RETURN {expression} AS value LIMIT 0");
            let mut calls = 0;
            let result = PreparedGraphSetText::prepare(&text, |kind, name| {
                calls += 1;
                symbols(kind, name)
            });
            assert!(result.is_err(), "{expression}");
            assert_eq!(calls, 0, "{expression}");
        }
    }

    fn assignment_policy() -> crate::GraphMutationPolicy {
        crate::GraphMutationPolicy::new(crate::GqlQueryPolicy::new(100, 100, 100_000, 100_000), 100)
    }

    fn execute_assignment(
        mutation: &PreparedGraphMutation,
        values: &[CanonicalScalar],
        policy: crate::GraphMutationPolicy,
        checkpoint: impl FnMut() -> Result<(), ()>,
    ) -> Result<crate::GraphMutationBatch, crate::GqlQueryError<crate::GraphMutationError<()>, ()>>
    {
        // Property sources hand stored values (fgdb-k3nub).
        let values: Vec<fgdb_types::CanonicalPropertyValue> =
            values.iter().cloned().map(Into::into).collect();
        mutation.execute_governed(
            policy,
            |plan, policy| {
                plan.plan().execute_governed_with_properties(
                    values.len() as u64,
                    [],
                    (0..values.len()).map(|i| {
                        (
                            fgdb_types::VId(1),
                            RelationId(1),
                            fgdb_types::VId(i as u128 + 10),
                        )
                    }),
                    |_, _| Ok::<_, ()>(true),
                    |vid, _| Ok(Some(&values[vid.0 as usize - 10])),
                    policy,
                    || Ok::<_, ()>(()),
                )
            },
            checkpoint,
        )
    }

    #[test]
    fn set_composite_rhs_executes_the_native_value_language() {
        for (expression, expected) in [
            ("head([b.cost, 100])", CanonicalScalar::Int(7)),
            ("[b.cost, 100][0]", CanonicalScalar::Int(7)),
            ("{answer: b.cost}.answer", CanonicalScalar::Int(7)),
            (
                "size([x IN [1, 2, 3] WHERE x > 1]) + b.cost",
                CanonicalScalar::Int(9),
            ),
            (
                "reduce(total = 0, x IN [b.cost, 2] | total + x)",
                CanonicalScalar::Int(9),
            ),
            ("any(x IN [1, null] WHERE x > 1)", CanonicalScalar::Null),
            (
                "toString(date({year: 2024, month: 2, day: 29}))",
                CanonicalScalar::ucs_basic_text("2024-02-29").unwrap(),
            ),
        ] {
            for assignment in [
                format!("SET a.cost = {expression}"),
                format!("SET a += {{cost: {expression}}}"),
            ] {
                let text = format!("MATCH (a)-[:R]->(b) {assignment}");
                let prepared =
                    PreparedGraphMutationText::prepare(&text, RelationId(1), symbols).unwrap();
                let bound = prepared.bind_parameters(&GqlParameters::new()).unwrap();
                let batch = execute_assignment(
                    &bound,
                    &[CanonicalScalar::Int(7)],
                    assignment_policy(),
                    || Ok(()),
                )
                .unwrap();
                assert_eq!(
                    batch.intents(),
                    &[crate::GraphMutationIntent::Property {
                        vertex: fgdb_types::VId(1),
                        key: PropertyKeyId(1),
                        value: Some(expected.clone().into()),
                    }],
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn set_temporal_rhs_retains_its_storable_scalar_type() {
        let prepared = PreparedGraphMutationText::prepare(
            "MATCH (a)-[:R]->(b) SET a.cost = date({year: $year, month: 2, day: 29})",
            RelationId(1),
            symbols,
        )
        .unwrap();
        let bound = prepared
            .bind_parameters(&GqlParameters::new().with_int64("year", 2024).unwrap())
            .unwrap();
        let batch = execute_assignment(
            &bound,
            &[CanonicalScalar::Int(7)],
            assignment_policy(),
            || Ok(()),
        )
        .unwrap();
        let [
            crate::GraphMutationIntent::Property {
                value: Some(fgdb_types::CanonicalPropertyValue::Scalar(CanonicalScalar::Date(date))),
                ..
            },
        ] = batch.intents()
        else {
            panic!("SET must retain a Date, not its text rendering");
        };
        assert_eq!(date.to_string(), "2024-02-29");
    }

    #[test]
    fn set_composite_rhs_rebinds_without_catalog_access() {
        let mut calls = 0;
        let prepared = PreparedGraphMutationText::prepare(
            "MATCH (a)-[:R]->(b) SET a.cost = head([b.cost, 100]) + $extra",
            RelationId(1),
            |kind, name: &str| {
                calls += 1;
                symbols(kind, name)
            },
        )
        .unwrap();
        let before = calls;
        assert!(before > 0);
        assert_eq!(prepared.parameter_schema().len(), 1);
        assert_eq!(prepared.parameter_schema()[0].occurrences, 1);
        for extra in [1, 4] {
            let bound = prepared
                .bind_parameters(&GqlParameters::new().with_int64("extra", extra).unwrap())
                .unwrap();
            let batch = execute_assignment(
                &bound,
                &[CanonicalScalar::Int(7)],
                assignment_policy(),
                || Ok(()),
            )
            .unwrap();
            assert_eq!(
                batch.intents(),
                &[crate::GraphMutationIntent::Property {
                    vertex: fgdb_types::VId(1),
                    key: PropertyKeyId(1),
                    value: Some(CanonicalScalar::Int(7 + extra).into()),
                }]
            );
        }
        assert_eq!(calls, before);
        assert!(prepared.bind_parameters(&GqlParameters::new()).is_err());
    }

    #[test]
    fn set_composite_rhs_keeps_conflict_and_non_scalar_refusals() {
        let bind = |expression| {
            PreparedGraphMutationText::prepare(
                &format!("MATCH (a)-[:R]->(b) SET a.cost = {expression}"),
                RelationId(1),
                symbols,
            )
            .unwrap()
            .bind_parameters(&GqlParameters::new())
        };
        let bound = bind("head([b.cost])").unwrap();
        for values in [
            [CanonicalScalar::Int(7), CanonicalScalar::Int(9)],
            [CanonicalScalar::Int(9), CanonicalScalar::Int(7)],
        ] {
            assert!(matches!(
                execute_assignment(&bound, &values, assignment_policy(), || Ok(())),
                Err(crate::GqlQueryError::Source(
                    crate::GraphMutationError::ConflictingAssignment { .. }
                ))
            ));
        }
        // A list value is a stored List (fgdb-k3nub), a list selected out of
        // a list included; a map is still no property value.
        let stored = |values: Vec<i64>| {
            fgdb_types::CanonicalPropertyValue::List(
                fgdb_types::CanonicalList::try_new(
                    values
                        .into_iter()
                        .map(|value| CanonicalScalar::Int(value).into())
                        .collect(),
                )
                .unwrap(),
            )
        };
        for (expression, expected) in [("[1, 2]", vec![1, 2]), ("[[1]][0]", vec![1])] {
            let batch = execute_assignment(
                &bind(expression).unwrap(),
                &[CanonicalScalar::Int(7)],
                assignment_policy(),
                || Ok(()),
            )
            .unwrap();
            // A stored collection is GitHub main's StoredProperty intent (b75ef707).
            assert_eq!(
                batch.intents(),
                &[crate::GraphMutationIntent::StoredProperty {
                    target: fgdb_delta_types::ElementId::Vertex(fgdb_types::VId(1)),
                    key: PropertyKeyId(1),
                    value: stored(expected),
                }],
                "{expression}"
            );
        }
        let bound = bind("[{k: 1}][0]").unwrap();
        assert!(matches!(
            execute_assignment(
                &bound,
                &[CanonicalScalar::Int(7)],
                assignment_policy(),
                || Ok(())
            ),
            Err(crate::GqlQueryError::Source(
                crate::GraphMutationError::NonScalarValue { .. }
            ))
        ));
    }

    #[test]
    fn set_composite_rhs_charges_work_and_observes_cancellation() {
        let bound = PreparedGraphMutationText::prepare(
            "MATCH (a)-[:R]->(b) SET a.cost = reduce(s = 0, x IN range(1, 20) | s + x)",
            RelationId(1),
            symbols,
        )
        .unwrap()
        .bind_parameters(&GqlParameters::new())
        .unwrap();
        let values = [CanonicalScalar::Int(7)];
        let measured = execute_assignment(&bound, &values, assignment_policy(), || Ok(())).unwrap();
        let stats = measured.stats();
        let exact = crate::GraphMutationPolicy::new(
            crate::GqlQueryPolicy::new(
                100,
                100,
                stats.evaluator.work_units,
                stats.evaluator.scratch_entries,
            ),
            1,
        );
        assert_eq!(
            execute_assignment(&bound, &values, exact, || Ok(()))
                .unwrap()
                .stats(),
            stats
        );
        let mut short = exact;
        short.query.evaluator.max_work_units -= 1;
        assert!(matches!(
            execute_assignment(&bound, &values, short, || Ok(())),
            Err(crate::GqlQueryError::Evaluator(_))
        ));
        let mut checkpoints = 0;
        assert!(matches!(
            execute_assignment(&bound, &values, assignment_policy(), || {
                checkpoints += 1;
                if checkpoints == 10 { Err(()) } else { Ok(()) }
            }),
            Err(crate::GqlQueryError::Interrupted(()))
        ));
        assert_eq!(checkpoints, 10);
    }
}
