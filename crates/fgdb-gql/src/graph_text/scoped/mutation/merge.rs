//! Native bounded single-vertex MERGE and branch actions on the shared lexer.

use super::*;
use crate::insertion::{GraphInsertBuildError, GraphInsertVertex, PreparedGraphInsert};
use crate::mutation_text::{
    PreparedGraphVertexMergeRowsText, VertexMergeRowActionTemplate, VertexMergeRowActionTemplates,
    VertexMergeRowValueTemplate as RowValueTemplate, VertexMergeRowsReturnTemplate,
    VertexMergeRowsTextInput, VertexMergeValueTemplate, WriteReturnGroupTemplate,
};
use crate::set_text::{
    ReadPageNumber, ReadProjectionTemplate, ReadStageTemplate, ReadValueTemplate,
};
use crate::vertex_upsert_text::{
    PreparedGraphVertexUpsertQueryText, VertexReturnTemplate, VertexUpsertActionTemplate,
    VertexUpsertValueTemplate,
};
use crate::{
    GraphMutationValue, GraphSetColumnType, GraphSetProjection, GraphSetQuantifier,
    GraphSymbolResolver, GraphVertexMergeBuildError, GraphVertexMergeTextError,
    GraphVertexMergeTextErrorKind, GraphVertexReturnBinding, GraphVertexUpsertAction,
    GraphVertexUpsertBranch, GraphVertexUpsertTextError, GraphVertexUpsertTextErrorKind,
    PreparedGraphVertexMerge, PreparedGraphVertexMergeText, PreparedGraphVertexUpsert,
    PreparedGraphVertexUpsertQuery, PreparedGraphVertexUpsertText, ReverseSymbolCatalog,
};
use mutation_query::{ReturnLeaves, ReturnMapInput};

pub(super) struct MergeProperty<'a> {
    pub(super) key: Name<'a>,
    filter: Option<Number>,
    pub(super) value: VertexMergeValueTemplate,
}

/// A MERGE property map refusal, mapped by each MERGE form onto its own
/// error type: too many fields, a repeated key, or a malformed value.
pub(super) enum MergeMapError {
    Pattern(GraphPatternTextError),
    Value(crate::GraphSetTextError),
    TooManyFields { at: usize, observed: usize },
    DuplicateProperty { at: usize },
}
impl From<GraphPatternTextError> for MergeMapError {
    fn from(error: GraphPatternTextError) -> Self {
        Self::Pattern(error)
    }
}
impl From<crate::GraphSetTextError> for MergeMapError {
    fn from(error: crate::GraphSetTextError) -> Self {
        Self::Value(error)
    }
}

#[derive(Clone)]
enum ParsedUpsertAction<'a> {
    Property {
        key: Name<'a>,
        value: VertexUpsertValueTemplate,
    },
    /// The program's columns address `inputs` in order.
    Expression {
        key: Name<'a>,
        inputs: Vec<UpsertInput<'a>>,
        program: Vec<MutationIntegerTemplateOp>,
        at: usize,
    },
    /// A vertex MERGE step's right-hand side that reads its row and not the
    /// MERGE vertex (`prop.name`), by the shared value compiler.
    RowValue {
        key: Name<'a>,
        value: ReadValueTemplate,
        at: usize,
    },
    /// A relationship MERGE step's assignment to a vertex its row carries
    /// in `column` (`ON CREATE SET b.created = 1`, fgdb-at06q), by the
    /// shared value compiler over the row.
    RowVertex {
        column: usize,
        key: Name<'a>,
        value: ReadValueTemplate,
        at: usize,
    },
    Label {
        label: Name<'a>,
    },
    ClearProperties {
        keep: Vec<Name<'a>>,
    },
    /// A relationship MERGE step's `r = m` or `r += m` over a map its row
    /// computes; `r = a` copies the row element `a`'s properties
    /// (fgdb-at06q). It is the clause's only relationship assignment.
    RowProperties {
        value: ReadValueTemplate,
        replace: bool,
        catalog: crate::set_text::ElementCatalogSlot,
        at: usize,
    },
}

#[derive(Clone, Copy)]
enum UpsertReplacement<'k, 's> {
    Refuse,
    Created(&'k [Name<'s>]),
    Current,
}

/// One input of a MERGE action expression: a property of the MERGE vertex,
/// or a column of the row a vertex MERGE step runs over (fgdb-cpny9).
#[derive(Clone, Copy)]
enum UpsertInput<'a> {
    Property(Name<'a>),
    Column(usize),
}

type ParsedUpsertClauses<'a> = (
    Vec<ParsedUpsertAction<'a>>,
    Vec<ParsedUpsertAction<'a>>,
    Vec<ParsedUpsertAction<'a>>,
);

/// Property-only use of the shared row-action grammar by relationship MERGE.
/// Labels are refused before symbol resolution, and no source text executes.
pub(super) struct ParsedEdgeRowActions<'a>(ParsedUpsertClauses<'a>);

impl<'a> ParsedEdgeRowActions<'a> {
    pub(super) fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<VertexMergeRowActionTemplates, GraphPatternTextError> {
        Ok(VertexMergeRowActionTemplates {
            on_match: resolve_row_actions(self.0.0, symbol)?,
            on_create: resolve_row_actions(self.0.1, symbol)?,
            after: resolve_row_actions(self.0.2, symbol)?,
        })
    }
}

/// Pins the action resolver's argument to one inferred source lifetime. A bare
/// closure parameter here has no type at its definition (E0282), and spelling it
/// with `'_` would make the closure higher-ranked over a lifetime the symbol
/// cache cannot outlive.
fn upsert_action_resolver<'s, F>(resolver: F) -> F
where
    F: FnMut(
        Vec<ParsedUpsertAction<'s>>,
    ) -> Result<Vec<VertexUpsertActionTemplate>, GraphPatternTextError>,
{
    resolver
}

fn insert_error(at: usize, source: GraphInsertBuildError) -> GraphVertexMergeTextError {
    GraphVertexMergeTextError {
        offset: at,
        kind: GraphVertexMergeTextErrorKind::InsertBuild(source),
    }
}
fn merge_error(at: usize, source: GraphVertexMergeBuildError) -> GraphVertexMergeTextError {
    GraphVertexMergeTextError {
        offset: at,
        kind: GraphVertexMergeTextErrorKind::MergeBuild(source),
    }
}
fn upsert_merge_error(error: GraphVertexMergeTextError) -> GraphVertexUpsertTextError {
    GraphVertexUpsertTextError {
        offset: error.offset,
        kind: GraphVertexUpsertTextErrorKind::Merge(error.kind),
    }
}

impl<'a> Parser<'a> {
    fn vertex_merge_pattern(
        &mut self,
    ) -> Result<(Name<'a>, Vec<Name<'a>>, Vec<MergeProperty<'a>>), GraphVertexMergeTextError> {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let variable = self.name()?;
        let mut labels = Vec::new();
        while self.take(b':')? {
            let label = self.schema_name()?;
            if labels
                .iter()
                .any(|previous: &Name<'a>| previous.text == label.text)
            {
                return Err(insert_error(
                    label.at,
                    GraphInsertBuildError::DuplicateLabel { vertex: 0 },
                ));
            }
            labels.push(label);
        }
        let properties = self
            .merge_property_map(true)
            .map_err(|refusal| match refusal {
                MergeMapError::Pattern(error) => error.into(),
                MergeMapError::Value(error) => error.into(),
                MergeMapError::TooManyFields { at, observed } => insert_error(
                    at,
                    GraphInsertBuildError::TooManyFields {
                        limit: crate::insertion::MAX_GRAPH_INSERT_FIELDS,
                        observed,
                    },
                ),
                MergeMapError::DuplicateProperty { at } => insert_error(
                    at,
                    GraphInsertBuildError::DuplicateProperty { declaration: 0 },
                ),
            })?;
        self.punct(b')', ")")?;
        Ok((variable, labels, properties))
    }

    /// An optional `{key: value, ...}` MERGE property map: one or more
    /// distinct keys, each a non-null literal or a parameter. The same values
    /// are the MERGE's exact-equality predicates and its creation fields.
    pub(super) fn merge_property_map(
        &mut self,
        lists: bool,
    ) -> Result<Vec<MergeProperty<'a>>, MergeMapError> {
        let mut properties = Vec::new();
        if self.take(b'{')? {
            if self.is_punct(b'}') {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::Expected("at least one MERGE property"),
                )
                .into());
            }
            loop {
                if properties.len() >= crate::insertion::MAX_GRAPH_INSERT_FIELDS {
                    return Err(MergeMapError::TooManyFields {
                        at: self.current.at,
                        observed: properties.len() + 1,
                    });
                }
                let key = self.schema_name()?;
                if properties
                    .iter()
                    .any(|previous: &MergeProperty<'a>| previous.key.text == key.text)
                {
                    return Err(MergeMapError::DuplicateProperty { at: key.at });
                }
                self.punct(b':', ":")?;
                let (filter, value) = self.merge_property_value(lists)?;
                properties.push(MergeProperty { key, filter, value });
                if !self.take(b',')? {
                    break;
                }
            }
            self.punct(b'}', "}")?;
        }
        Ok(properties)
    }

    /// A relationship MERGE map over a row (fgdb-at06q): each value is a row
    /// column, a value computed from the row (`a.v`, `s + 1`), or, reading
    /// no row binding, a literal or parameter as merge_property_map takes
    /// it. The constants and the row values come back apart, each in text
    /// order; a key appears in at most one of them.
    #[allow(clippy::type_complexity)]
    pub(super) fn merge_row_property_map(
        &mut self,
        row: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<(Vec<MergeProperty<'a>>, Vec<(Name<'a>, RowValueTemplate)>), MergeMapError> {
        let mut constants: Vec<MergeProperty<'a>> = Vec::new();
        let mut computed: Vec<(Name<'a>, RowValueTemplate)> = Vec::new();
        if !self.take(b'{')? {
            return Ok((constants, computed));
        }
        if self.is_punct(b'}') {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("at least one MERGE property"),
            )
            .into());
        }
        loop {
            let count = constants.len() + computed.len();
            if count >= crate::insertion::MAX_GRAPH_INSERT_FIELDS {
                return Err(MergeMapError::TooManyFields {
                    at: self.current.at,
                    observed: count + 1,
                });
            }
            let key = self.schema_name()?;
            if constants
                .iter()
                .any(|previous| previous.key.text == key.text)
                || computed
                    .iter()
                    .any(|(previous, _)| previous.text == key.text)
            {
                return Err(MergeMapError::DuplicateProperty { at: key.at });
            }
            self.punct(b':', ":")?;
            // As in a vertex MERGE step: a bare column or hidden read is the
            // column itself, and a longer value that reads the row is
            // computed per row by the shared value compiler.
            let start = (self.lexer.clone(), self.current);
            let column = self.merge_row_column(row)?;
            match column {
                Some(column) if self.is_punct(b',') || self.is_punct(b'}') => {
                    computed.push((key, RowValueTemplate::Column(column)));
                }
                _ if column.is_some() || self.merge_value_reads_row(row)? => {
                    (self.lexer, self.current) = start;
                    let at = self.current.at;
                    computed.push((
                        key,
                        RowValueTemplate::Computed {
                            value: Box::new(self.merge_row_value(row)?),
                            at,
                        },
                    ));
                }
                _ => {
                    let (filter, value) = self.merge_property_value(true)?;
                    constants.push(MergeProperty { key, filter, value });
                }
            }
            if !self.take(b',')? {
                break;
            }
        }
        self.punct(b'}', "}")?;
        Ok((constants, computed))
    }

    /// One MERGE map value: a non-null literal or a parameter, as both the
    /// exact-equality filter and the creation field. A list, or a value
    /// computed from literals and parameters alone, is evaluated once at
    /// bind and its result is used the same way.
    fn merge_property_value(
        &mut self,
        lists: bool,
    ) -> Result<(Option<Number>, VertexMergeValueTemplate), MergeMapError> {
        let at = self.current.at;
        if (lists && self.set_value_is_list()?) || self.computed_value_follows()? {
            return Ok((
                None,
                VertexMergeValueTemplate::Stored {
                    value: Box::new(self.read_row_value(&[], 0)?),
                    at,
                },
            ));
        }
        let operand = self.mutation_operand(&mut Vec::new())?;
        Ok(match operand {
            Operand::Literal(value) => {
                if matches!(value.value(), CanonicalScalar::Null) {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("non-null MERGE property value"),
                    )
                    .into());
                }
                (
                    Some(Number::Literal(GqlParameterValue::Scalar(value.clone()))),
                    VertexMergeValueTemplate::Bound(value),
                )
            }
            Operand::Number(Number::Literal(value)) => {
                let scalar = scalar(value.clone(), at)?;
                if matches!(scalar.value(), CanonicalScalar::Null) {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected("non-null MERGE property value"),
                    )
                    .into());
                }
                (
                    Some(Number::Literal(value)),
                    VertexMergeValueTemplate::Bound(scalar),
                )
            }
            Operand::Number(Number::Parameter(index)) => (
                Some(Number::Parameter(index)),
                VertexMergeValueTemplate::Parameter { index, at },
            ),
            Operand::Column(_) | Operand::Integer { .. } => {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected(
                        "scalar literal or parameter MERGE property value",
                    ),
                )
                .into());
            }
        })
    }

    /// ON MATCH SET, ON CREATE SET and a trailing SET on `variable`. `row` is
    /// the row a vertex MERGE step runs over, whose columns the right-hand
    /// sides may read (fgdb-cpny9); a unique MERGE has none. With
    /// `row_vertices`, an assignment may also target a vertex the row
    /// carries (relationship MERGE, fgdb-at06q).
    /// `pattern_keys`: a vertex MERGE's property keys. ON CREATE can lower a
    /// replacement from those known keys; ON MATCH and trailing SET read the
    /// selected vertex's current keys. `None` keeps the relationship grammar's
    /// unsupported replacement outside this vertex action family.
    fn upsert_branch_actions(
        &mut self,
        variable: Name<'a>,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
        row_vertices: bool,
        pattern_keys: Option<&[Name<'a>]>,
    ) -> Result<ParsedUpsertClauses<'a>, GraphVertexUpsertTextError> {
        let mut on_match = Vec::new();
        let mut on_create = Vec::new();
        let mut after = Vec::new();
        let mut saw_match = false;
        let mut saw_create = false;
        while self.take_word("ON")? {
            let branch_at = self.current.at;
            let create = if self.take_word("MATCH")? {
                if saw_match {
                    return Err(GraphVertexUpsertTextError {
                        offset: branch_at,
                        kind: GraphVertexUpsertTextErrorKind::DuplicateBranch,
                    });
                }
                saw_match = true;
                false
            } else if self.take_word("CREATE")? {
                if saw_create {
                    return Err(GraphVertexUpsertTextError {
                        offset: branch_at,
                        kind: GraphVertexUpsertTextErrorKind::DuplicateBranch,
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
            let (target, branch, replacement) = if create {
                (
                    &mut on_create,
                    GraphVertexUpsertBranch::Create,
                    if row_vertices {
                        // A relationship row action has the same governed
                        // current-key reader on both branches. Its pattern
                        // map can itself be computed from the input row.
                        UpsertReplacement::Current
                    } else {
                        pattern_keys.map_or(UpsertReplacement::Refuse, UpsertReplacement::Created)
                    },
                )
            } else {
                (
                    &mut on_match,
                    GraphVertexUpsertBranch::Match,
                    if pattern_keys.is_some() || row_vertices {
                        UpsertReplacement::Current
                    } else {
                        UpsertReplacement::Refuse
                    },
                )
            };
            self.upsert_assignments(variable, target, branch, row, row_vertices, replacement)?;
        }
        // The common SET is a distinct clause, not a deduplicated extension
        // of both branches. It sees prior effects and cannot erase an earlier
        // failure or unauthorized write just by overwriting the same target.
        if self.take_word("SET")? {
            self.upsert_assignments(
                variable,
                &mut after,
                GraphVertexUpsertBranch::Match,
                row,
                row_vertices,
                if pattern_keys.is_some() || row_vertices {
                    UpsertReplacement::Current
                } else {
                    UpsertReplacement::Refuse
                },
            )?;
        }
        for (branch, actions) in [
            (GraphVertexUpsertBranch::Match, &on_match),
            (GraphVertexUpsertBranch::Create, &on_create),
        ] {
            let observed = actions.len() + after.len();
            if observed > crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
                return Err(GraphVertexUpsertTextError {
                    offset: self.current.at,
                    kind: GraphVertexUpsertTextErrorKind::UpsertBuild(
                        crate::GraphVertexUpsertBuildError::TooManyActions {
                            branch,
                            limit: crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS,
                            observed,
                        },
                    ),
                });
            }
        }
        Ok((on_match, on_create, after))
    }

    pub(super) fn edge_row_actions(
        &mut self,
        variable: Name<'a>,
        row: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ParsedEdgeRowActions<'a>, GraphVertexUpsertTextError> {
        let clauses = self.upsert_branch_actions(variable, Some(row), true, None)?;
        for actions in [&clauses.0, &clauses.1, &clauses.2] {
            for action in actions {
                if let ParsedUpsertAction::Label { label } = action {
                    return Err(error(
                        label.at,
                        GraphPatternTextErrorKind::Expected("a relationship property assignment"),
                    )
                    .into());
                }
            }
        }
        Ok(ParsedEdgeRowActions(clauses))
    }

    /// One simultaneous SET clause. The native scalar compiler owns operator
    /// precedence, lazy CASE/COALESCE, limits and parameter type admission.
    fn upsert_assignments(
        &mut self,
        variable: Name<'a>,
        target: &mut Vec<ParsedUpsertAction<'a>>,
        branch: GraphVertexUpsertBranch,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
        row_vertices: bool,
        replacement: UpsertReplacement<'_, 'a>,
    ) -> Result<(), GraphVertexUpsertTextError> {
        loop {
            self.upsert_capacity(target.len(), branch)?;
            let actual = self.name()?;
            if actual.text != variable.text {
                // `b.key = value` on a vertex the row carries. Computed
                // endpoints (head/list access/UNWIND) have an open type;
                // the native action evaluator checks their actual identity.
                // Its value reads the row and not the merged relationship.
                let carried = row.filter(|_| row_vertices).and_then(|row| {
                    row[..self.visible_width(row)]
                        .iter()
                        .position(|(name, kind)| {
                            name.text == actual.text
                                && matches!(
                                    kind,
                                    GraphSetColumnType::Vertex | GraphSetColumnType::Any
                                )
                        })
                        .map(|column| (row, column))
                });
                let Some((row, column)) = carried else {
                    return Err(error(
                        actual.at,
                        GraphPatternTextErrorKind::Expected("the MERGE variable"),
                    )
                    .into());
                };
                self.punct(b'.', "property assignment")?;
                let key = self.schema_name()?;
                self.punct(b'=', "=")?;
                let at = self.current.at;
                if self.value_reads(row, Some(variable.text))?.1 {
                    return Err(error(
                        at,
                        GraphPatternTextErrorKind::Expected(
                            "a value that does not read the MERGE relationship",
                        ),
                    )
                    .into());
                }
                let value = self.merge_row_value(row)?;
                target.push(ParsedUpsertAction::RowVertex {
                    column,
                    key,
                    value,
                    at,
                });
                if !self.take(b',')? {
                    break;
                }
                continue;
            }
            if let Some(row) = row
                && let Some(replace) = self.copied_map_follows(replacement)?
            {
                self.upsert_copied_map(variable, target, row, replace)?;
                if !self.take(b',')? {
                    break;
                }
                continue;
            }
            let action = if self.take(b':')? {
                ParsedUpsertAction::Label {
                    label: self.schema_name()?,
                }
            } else if self.take(b'+')? {
                // `n += {key: value, ...}` (fgdb-0s2pq): one ordinary property
                // assignment per entry, under the same action limit.
                self.upsert_map(variable, target, branch, row)?;
                if !self.take(b',')? {
                    break;
                }
                continue;
            } else if !matches!(replacement, UpsertReplacement::Refuse)
                && self.is_punct(b'=')
                && matches!(self.lexer.clone().next()?.kind, TokenKind::Punct(b'{'))
            {
                // Freeze every RHS before clearing the old keys. A newly
                // created vertex has exactly the pattern's keys; the other
                // branches ask the host for its visible keys at this clause.
                // Preserve the first-assignment restriction rather than
                // silently changing preceding same-clause assignments.
                if !target.is_empty() {
                    return Err(error(
                        self.current.at,
                        GraphPatternTextErrorKind::Expected(
                            "a replacing map as the first SET assignment",
                        ),
                    )
                    .into());
                }
                self.upsert_map(variable, target, branch, row)?;
                let assigned = |action: &ParsedUpsertAction<'a>| match action {
                    ParsedUpsertAction::Property { key, .. }
                    | ParsedUpsertAction::Expression { key, .. }
                    | ParsedUpsertAction::RowValue { key, .. } => Some(*key),
                    _ => None,
                };
                let written: Vec<Name<'a>> = target.iter().filter_map(assigned).collect();
                if let UpsertReplacement::Created(pattern_keys) = replacement {
                    for key in pattern_keys {
                        if written.iter().any(|written| written.text == key.text) {
                            continue;
                        }
                        self.upsert_capacity(target.len(), branch)?;
                        let null = crate::GqlScalarParameter::new(CanonicalScalar::Null)
                            .map_err(|_| error(key.at, GraphPatternTextErrorKind::ScalarLiteral))?;
                        target.push(ParsedUpsertAction::Property {
                            key: *key,
                            value: VertexUpsertValueTemplate::Bound(null),
                        });
                    }
                } else {
                    self.upsert_capacity(target.len(), branch)?;
                    target.push(ParsedUpsertAction::ClearProperties { keep: written });
                }
                if !self.take(b',')? {
                    break;
                }
                continue;
            } else {
                self.punct(b'.', "property or label assignment")?;
                let key = self.schema_name()?;
                self.punct(b'=', "=")?;
                self.upsert_value(variable, key, row)?
            };
            target.push(action);
            if !self.take(b',')? {
                break;
            }
        }
        // A copied map's names are known only per row: it is its clause's
        // only target assignment, so no entry competes with another.
        if let Some(at) = target.iter().find_map(|action| match action {
            ParsedUpsertAction::RowProperties { at, .. } => Some(*at),
            _ => None,
        }) && target.iter().any(|action| {
            !matches!(
                action,
                ParsedUpsertAction::RowProperties { .. } | ParsedUpsertAction::RowVertex { .. }
            )
        }) {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected(
                    "a copied property map as the clause's only target assignment",
                ),
            )
            .into());
        }
        // Additional explicit assignments of this simultaneous clause also
        // survive its replacement; clearing must never propose a competing
        // unset for one of those fields.
        if target
            .iter()
            .any(|action| matches!(action, ParsedUpsertAction::ClearProperties { .. }))
        {
            let written: Vec<_> = target
                .iter()
                .filter_map(|action| match action {
                    ParsedUpsertAction::Property { key, .. }
                    | ParsedUpsertAction::Expression { key, .. }
                    | ParsedUpsertAction::RowValue { key, .. } => Some(*key),
                    _ => None,
                })
                .collect();
            for action in target {
                if let ParsedUpsertAction::ClearProperties { keep } = action {
                    *keep = written;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Whether `= value` or `+= value` over anything but a literal map
    /// stands here: a copied property map (fgdb-at06q). `Some(true)`
    /// replaces; a replacement needs a branch that may replace.
    fn copied_map_follows(
        &self,
        replacement: UpsertReplacement<'_, 'a>,
    ) -> Result<Option<bool>, GraphPatternTextError> {
        let mut lexer = self.lexer.clone();
        let replace = match self.current.kind {
            TokenKind::Punct(b'=') if !matches!(replacement, UpsertReplacement::Refuse) => true,
            TokenKind::Punct(b'+') if matches!(lexer.next()?.kind, TokenKind::Punct(b'=')) => false,
            _ => return Ok(None),
        };
        Ok((!matches!(lexer.next()?.kind, TokenKind::Punct(b'{'))).then_some(replace))
    }

    /// `n = value` or `n += value` in a MERGE step (fgdb-at06q,
    /// TCK Merge6 [6], Merge7 [4]): a row element is its property map, as
    /// `properties(a)` is; any other value must compute a map from the row.
    /// Its entries are assigned when the step runs, so it must be the
    /// clause's only target assignment.
    fn upsert_copied_map(
        &mut self,
        variable: Name<'a>,
        target: &mut Vec<ParsedUpsertAction<'a>>,
        row: &[(Name<'a>, GraphSetColumnType)],
        replace: bool,
    ) -> Result<(), GraphVertexUpsertTextError> {
        let at = self.current.at;
        if !replace {
            self.punct(b'+', "+=")?;
        }
        self.punct(b'=', "=")?;
        if !target.is_empty() {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Expected(
                    "a copied property map as the clause's only target assignment",
                ),
            )
            .into());
        }
        let catalog = self.syntax.element_catalog.request();
        let element = match self.current.kind {
            TokenKind::Word(word)
                if matches!(
                    self.lexer.clone().next()?.kind,
                    TokenKind::End | TokenKind::Punct(b',' | b';') | TokenKind::Word(_)
                ) =>
            {
                row[..self.visible_width(row)]
                    .iter()
                    .position(|(name, kind)| {
                        name.text == word
                            && matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
                    })
            }
            _ => None,
        };
        let value = if let Some(column) = element {
            self.advance()?;
            ReadValueTemplate::ElementMetadata {
                kind: crate::GraphElementMetadata::Properties,
                value: Box::new(ReadValueTemplate::Column(column)),
                catalog: catalog.clone(),
            }
        } else {
            if self.value_reads(row, Some(variable.text))?.1 {
                return Err(error(
                    self.current.at,
                    GraphPatternTextErrorKind::Expected(
                        "a value that does not read the MERGE target",
                    ),
                )
                .into());
            }
            self.merge_row_value(row)?
        };
        target.push(ParsedUpsertAction::RowProperties {
            value,
            replace,
            catalog,
            at,
        });
        Ok(())
    }

    fn upsert_capacity(
        &self,
        actions: usize,
        branch: GraphVertexUpsertBranch,
    ) -> Result<(), GraphVertexUpsertTextError> {
        if actions >= crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS {
            return Err(GraphVertexUpsertTextError {
                offset: self.current.at,
                kind: GraphVertexUpsertTextErrorKind::UpsertBuild(
                    crate::GraphVertexUpsertBuildError::TooManyActions {
                        branch,
                        limit: crate::MAX_GRAPH_VERTEX_UPSERT_ACTIONS,
                        observed: actions + 1,
                    },
                ),
            });
        }
        Ok(())
    }

    /// `+= {key: value, ...}` after the MERGE vertex: each entry is exactly
    /// the assignment `n.key = value`, in entry order. `{}` assigns nothing
    /// (TCK Set5 [5]).
    fn upsert_map(
        &mut self,
        variable: Name<'a>,
        target: &mut Vec<ParsedUpsertAction<'a>>,
        branch: GraphVertexUpsertBranch,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<(), GraphVertexUpsertTextError> {
        self.punct(b'=', "=")?;
        self.punct(b'{', "literal property map after +=")?;
        if self.take(b'}')? {
            return Ok(());
        }
        let mut keys = std::collections::BTreeSet::new();
        loop {
            self.upsert_capacity(target.len(), branch)?;
            let key = self.schema_name()?;
            if !keys.insert(key.text) {
                return Err(error(
                    key.at,
                    GraphPatternTextErrorKind::Expected("distinct property keys in SET += map"),
                )
                .into());
            }
            self.punct(b':', ":")?;
            let action = self.upsert_value(variable, key, row)?;
            target.push(action);
            if self.take(b'}')? {
                break;
            }
            self.punct(b',', ", or }")?;
        }
        Ok(())
    }

    /// The right-hand side of `n.key = value`.
    fn upsert_value(
        &mut self,
        variable: Name<'a>,
        key: Name<'a>,
        row: Option<&[(Name<'a>, GraphSetColumnType)]>,
    ) -> Result<ParsedUpsertAction<'a>, GraphVertexUpsertTextError> {
        let at = self.current.at;
        // A vertex MERGE step's right-hand side that reads its row
        // and not the MERGE vertex (`prop.name`, `xs + [y]`) is the
        // shared value compiler's value over the row, written as
        // CREATE writes it (fgdb-cpny9).
        let row_value = match row {
            Some(row) => match self.value_reads(row, Some(variable.text))? {
                (true, false) => Some(self.merge_row_value(row)?),
                _ => None,
            },
            None => None,
        };
        Ok(if let Some(value) = row_value {
            ParsedUpsertAction::RowValue { key, value, at }
        } else if self.set_value_is_list()? {
            // A constant list (`['a', 'b']`, `$tags`) is a stored List
            // (fgdb-k3nub), evaluated once when the statement binds.
            // One built from the MERGE vertex's own fields is not yet.
            let mut inputs = Vec::new();
            let value = self.read_graph_value(&mut inputs, 0)?;
            if !inputs.is_empty() {
                return Err(GraphVertexUpsertTextError {
                    offset: at,
                    kind: GraphVertexUpsertTextErrorKind::Query(
                        GraphPatternTextErrorKind::Expected(
                            "a list value of literals and parameters",
                        ),
                    ),
                });
            }
            ParsedUpsertAction::Property {
                key,
                value: VertexUpsertValueTemplate::Composite { value, at },
            }
        } else {
            let mut inputs: Vec<UpsertInput<'a>> = Vec::new();
            let operand = self.resolved_predicate(&mut |parser| {
                let TokenKind::Word(word) = parser.current.kind else {
                    return Ok(None);
                };
                let next = parser.lexer.clone().next()?.kind;
                // A vertex MERGE step's row (fgdb-cpny9): `a.key` of an
                // element it carries is that row's hidden read, and a
                // scalar column reads by name.
                let column = match row {
                    Some(row) if word != variable.text => parser.merge_row_column(row)?,
                    _ => None,
                };
                let input = if let Some(column) = column {
                    UpsertInput::Column(column)
                } else {
                    if !matches!(next, TokenKind::Punct(b'.')) {
                        return Ok(None);
                    }
                    let source = parser.name()?;
                    if source.text != variable.text {
                        return Err(error(
                            source.at,
                            GraphPatternTextErrorKind::Expected("the MERGE variable"),
                        ));
                    }
                    parser.punct(b'.', ".")?;
                    UpsertInput::Property(parser.schema_name()?)
                };
                if let Some(column) = inputs.iter().position(|known| match (known, input) {
                    (UpsertInput::Property(old), UpsertInput::Property(new)) => {
                        old.text == new.text
                    }
                    (UpsertInput::Column(old), UpsertInput::Column(new)) => *old == new,
                    _ => false,
                }) {
                    return Ok(Some(column));
                }
                parser.capacity(
                    inputs.len(),
                    MAX_PATTERN_VERTICES,
                    crate::algebra::PatternLimitDimension::Columns,
                )?;
                inputs.push(input);
                Ok(Some(inputs.len() - 1))
            })?;
            match operand {
                Operand::Literal(value) => ParsedUpsertAction::Property {
                    key,
                    value: VertexUpsertValueTemplate::Bound(value),
                },
                Operand::Number(Number::Literal(value)) => ParsedUpsertAction::Property {
                    key,
                    value: VertexUpsertValueTemplate::Bound(scalar(value, at)?),
                },
                Operand::Number(Number::Parameter(index)) => ParsedUpsertAction::Property {
                    key,
                    value: VertexUpsertValueTemplate::Parameter { index, at },
                },
                Operand::Column(column) => ParsedUpsertAction::Expression {
                    key,
                    inputs,
                    program: vec![MutationIntegerTemplateOp::Bound(
                        crate::GraphIntegerOp::ScalarColumn(column),
                    )],
                    at,
                },
                Operand::Integer { program, at } => ParsedUpsertAction::Expression {
                    key,
                    inputs,
                    program,
                    at,
                },
            }
        })
    }
}

fn property_symbol<'a>(
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    name: Name<'a>,
) -> Result<fgdb_delta_types::PropertyKeyId, GraphPatternTextError> {
    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, name)? else {
        unreachable!("symbol domain checked by shared resolver")
    };
    Ok(key)
}

/// The symbols of one ON/SET clause of a vertex MERGE step (fgdb-cpny9).
fn resolve_row_actions<'a>(
    parsed: Vec<ParsedUpsertAction<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<Vec<VertexMergeRowActionTemplate>, GraphPatternTextError> {
    let mut actions = Vec::with_capacity(parsed.len());
    for action in parsed {
        actions.push(match action {
            ParsedUpsertAction::Property { key, value } => {
                VertexMergeRowActionTemplate::Vertex(VertexUpsertActionTemplate::Property {
                    key: property_symbol(symbol, key)?,
                    value,
                })
            }
            ParsedUpsertAction::Label { label } => {
                let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                    unreachable!("symbol domain checked by shared resolver")
                };
                VertexMergeRowActionTemplate::Vertex(VertexUpsertActionTemplate::Label {
                    label,
                    present: true,
                })
            }
            ParsedUpsertAction::ClearProperties { keep } => {
                let mut keep = keep
                    .into_iter()
                    .map(|key| property_symbol(symbol, key))
                    .collect::<Result<Vec<_>, _>>()?;
                keep.sort_unstable();
                keep.dedup();
                VertexMergeRowActionTemplate::Vertex(VertexUpsertActionTemplate::ClearProperties {
                    keep,
                })
            }
            ParsedUpsertAction::Expression {
                key,
                inputs,
                program,
                at,
            } => {
                let key = property_symbol(symbol, key)?;
                let mut resolved = Vec::with_capacity(inputs.len());
                for input in inputs {
                    resolved.push(match input {
                        UpsertInput::Property(name) => crate::GraphVertexMergeRowInput::Property(
                            property_symbol(symbol, name)?,
                        ),
                        UpsertInput::Column(column) => {
                            crate::GraphVertexMergeRowInput::Column(column)
                        }
                    });
                }
                VertexMergeRowActionTemplate::Expression {
                    key,
                    inputs: resolved,
                    program,
                    at,
                }
            }
            ParsedUpsertAction::RowValue { key, value, at } => {
                VertexMergeRowActionTemplate::Value {
                    key: property_symbol(symbol, key)?,
                    value: Box::new(value),
                    at,
                }
            }
            ParsedUpsertAction::RowVertex {
                column,
                key,
                value,
                at,
            } => VertexMergeRowActionTemplate::RowVertex {
                column,
                key: property_symbol(symbol, key)?,
                value: Box::new(value),
                at,
            },
            ParsedUpsertAction::RowProperties {
                value,
                replace,
                catalog,
                at,
            } => VertexMergeRowActionTemplate::RowProperties {
                value: Box::new(value),
                replace,
                catalog,
                at,
            },
        });
    }
    Ok(actions)
}

/// Bind the shared row-action grammar once. Relationship MERGE converts the
/// property-only subset into its own explicitly typed action definition.
pub(super) fn bind_row_actions(
    templates: &[VertexMergeRowActionTemplate],
    values: &[crate::GqlParameterValue],
) -> Result<Vec<crate::GraphVertexMergeRowAction>, GraphVertexUpsertTextError> {
    let mut actions = Vec::with_capacity(templates.len());
    for template in templates {
        actions.push(match template {
            VertexMergeRowActionTemplate::Vertex(template) => {
                crate::GraphVertexMergeRowAction::Vertex(match template {
                    VertexUpsertActionTemplate::Property { key, value } => match value {
                        VertexUpsertValueTemplate::Bound(value) => {
                            GraphVertexUpsertAction::SetProperty {
                                key: *key,
                                value: value.clone(),
                            }
                        }
                        VertexUpsertValueTemplate::Parameter { index, at } => {
                            GraphVertexUpsertAction::SetProperty {
                                key: *key,
                                value: scalar(values[*index].clone(), *at)?,
                            }
                        }
                        VertexUpsertValueTemplate::Composite { value, at } => {
                            constant_list_action(*key, value, values, *at)?
                        }
                    },
                    VertexUpsertActionTemplate::Label { label, present } => {
                        GraphVertexUpsertAction::SetLabel {
                            label: *label,
                            present: *present,
                        }
                    }
                    VertexUpsertActionTemplate::ClearProperties { keep } => {
                        GraphVertexUpsertAction::ClearProperties { keep: keep.clone() }
                    }
                    VertexUpsertActionTemplate::Expression { at, .. } => {
                        return Err(error(
                            *at,
                            GraphPatternTextErrorKind::Expected("a row expression"),
                        )
                        .into());
                    }
                })
            }
            VertexMergeRowActionTemplate::Expression {
                key,
                inputs,
                program,
                at,
            } => crate::GraphVertexMergeRowAction::Expression {
                key: *key,
                inputs: inputs.clone(),
                value: super::integer::bind_integer(program, values, *at)?,
            },
            VertexMergeRowActionTemplate::Value { key, value, at } => {
                crate::GraphVertexMergeRowAction::Value {
                    key: *key,
                    value: bind_row_value(value, values, *at)?,
                }
            }
            VertexMergeRowActionTemplate::RowProperties {
                value,
                replace,
                catalog,
                at,
            } => {
                let catalog = catalog.bind().ok_or_else(|| {
                    error(
                        *at,
                        GraphPatternTextErrorKind::Expected(
                            "a resolved property catalog for the copied map",
                        ),
                    )
                })?;
                let mut names: Vec<(Box<str>, fgdb_delta_types::PropertyKeyId)> = catalog
                    .properties
                    .iter()
                    .map(|(key, name)| (name.as_str().into(), *key))
                    .collect();
                names.sort_unstable();
                if names.windows(2).any(|names| names[0].0 == names[1].0) {
                    return Err(error(
                        *at,
                        GraphPatternTextErrorKind::Expected(
                            "distinct property names in the copied map's catalog",
                        ),
                    )
                    .into());
                }
                crate::GraphVertexMergeRowAction::Properties {
                    value: bind_row_value(value, values, *at)?,
                    replace: *replace,
                    names: names.into(),
                }
            }
            // Only relationship MERGE parses carried-vertex assignments.
            VertexMergeRowActionTemplate::RowVertex { at, .. } => {
                return Err(error(
                    *at,
                    GraphPatternTextErrorKind::Expected("a MERGE vertex property assignment"),
                )
                .into());
            }
        });
    }
    Ok(actions)
}

/// A row-reading right-hand side, bound by the shared value compiler.
pub(super) fn bind_row_value(
    value: &ReadValueTemplate,
    values: &[crate::GqlParameterValue],
    at: usize,
) -> Result<crate::GraphVertexMergeRowComputed, GraphVertexUpsertTextError> {
    Ok(crate::GraphVertexMergeRowComputed(
        return_projection::bind_read_value(value, values).map_err(|error| {
            GraphVertexUpsertTextError {
                offset: at,
                ..GraphVertexUpsertTextError::from(error)
            }
        })?,
    ))
}

/// A vertex MERGE branch's constant list value (fgdb-k3nub): a scalar result
/// is an ordinary assignment and a List the stored List; anything else
/// refuses (Parser::constant_written_value).
fn constant_list_action(
    key: fgdb_delta_types::PropertyKeyId,
    template: &ReadValueTemplate,
    values: &[crate::GqlParameterValue],
    at: usize,
) -> Result<GraphVertexUpsertAction, GraphVertexUpsertTextError> {
    let refused = || GraphVertexUpsertTextError {
        offset: at,
        kind: GraphVertexUpsertTextErrorKind::Expression(
            crate::GraphMutationTextErrorKind::IntegerOperand,
        ),
    };
    match Parser::constant_written_value(template, values)? {
        Some(fgdb_types::CanonicalPropertyValue::Scalar(scalar)) => {
            Ok(GraphVertexUpsertAction::SetProperty {
                key,
                value: crate::GqlScalarParameter::new(scalar).map_err(|_| refused())?,
            })
        }
        Some(value) => Ok(GraphVertexUpsertAction::SetStored { key, value }),
        None => Err(refused()),
    }
}

fn resolve_merge_template<'a>(
    statement: &str,
    relation: RelationId,
    syntax: Syntax<'a>,
    variable: Name<'a>,
    parsed_labels: Vec<Name<'a>>,
    parsed_properties: Vec<MergeProperty<'a>>,
    symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
) -> Result<PreparedGraphVertexMergeText, GraphVertexMergeTextError> {
    let mut labels = Vec::new();
    for label in parsed_labels {
        let GraphSymbol::Label(label_id) = symbol(GraphSymbolKind::Label, label)? else {
            unreachable!("symbol domain checked by shared resolver")
        };
        labels.push((label_id, label.at));
    }
    let mut properties = Vec::new();
    for property in parsed_properties {
        let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, property.key)? else {
            unreachable!("symbol domain checked by shared resolver")
        };
        properties.push((key, property.filter, property.value));
    }
    merge_template(
        statement,
        relation,
        (syntax.parameters, syntax.parameter_offsets),
        variable,
        &labels,
        properties,
    )
}

/// The MERGE template for resolved labels (with their source offsets) and
/// properties: each property is an exact-equality predicate and the same
/// creation field.
fn merge_template(
    statement: &str,
    relation: RelationId,
    (parameters, parameter_offsets): (Vec<GqlParameterSpec>, Vec<usize>),
    variable: Name<'_>,
    resolved_labels: &[(LabelId, usize)],
    resolved: Vec<(PropertyKeyId, Option<Number>, VertexMergeValueTemplate)>,
) -> Result<PreparedGraphVertexMergeText, GraphVertexMergeTextError> {
    let at = statement.len();
    let mut builder = GraphPatternBuilder::new();
    built(variable.at, builder.vertex(variable.text))?;
    let mut labels = Vec::with_capacity(resolved_labels.len());
    for &(label, label_at) in resolved_labels {
        built(
            label_at,
            builder.filter(variable.text, VertexPredicate::HasLabel(label)),
        )?;
        labels.push(label);
    }
    let mut filters = Vec::new();
    let mut properties = Vec::new();
    for (key, filter, value) in resolved {
        if let Some(filter) = filter {
            filters.push(BoundFilter::Property {
                variable: variable.text.to_owned(),
                key,
                comparison: IntegerComparison::Equal,
                value: filter,
            });
        }
        properties.push((key, value));
    }
    labels.sort_unstable();
    properties.sort_by_key(|(key, _)| *key);
    let columns = vec![BoundColumn {
        alias: "_merge_target".to_owned(),
        variable: variable.text.to_owned(),
        key: None,
        path: None,
        collect: None,
    }];
    let projected = columns
        .iter()
        .map(BoundColumn::declaration)
        .collect::<Vec<_>>();
    built(at, builder.prepare_values(&projected, 0, None))?;
    let selection = PreparedGraphText {
        statement: statement.to_owned(),
        builder,
        filters,
        scopes: Vec::new(),
        columns,
        ordering: Vec::new(),
        parameters,
        parameter_offsets,
        offset: Number::Literal(GqlParameterValue::UInt64(0)),
        count: None,
        distinct: false,
        visible_columns: None,
        return_at: at,
        reverse_catalog: None,
    };
    Ok(PreparedGraphVertexMergeText {
        selection,
        relation,
        labels,
        properties,
    })
}

/// Admit one stored key once, then use exactly that value in the native
/// equality predicate and the ordinary insertion collector. Maps remain
/// unavailable as written properties, including maps nested inside a list.
fn stored_merge_field(
    builder: &mut GraphPatternBuilder,
    variable: &str,
    key: PropertyKeyId,
    value: &fgdb_types::CanonicalPropertyValue,
    at: usize,
) -> Result<GraphMutationValue, GraphVertexMergeTextError> {
    use crate::algebra::{GraphBooleanExpression, GraphBooleanOp, GraphBooleanOperand, GraphValue};
    let refused = || {
        error(
            at,
            GraphPatternTextErrorKind::Expected(
                "non-null scalar or bounded list MERGE property value",
            ),
        )
    };
    if let fgdb_types::CanonicalPropertyValue::Scalar(value) = value {
        if matches!(value, CanonicalScalar::Null) {
            return Err(refused().into());
        }
        let literal = crate::GqlScalarParameter::new(value.clone())
            .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
        let predicate =
            crate::algebra::ScalarPredicate::new(value.clone(), IntegerComparison::Equal)
                .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
        built(
            at,
            builder.filter(variable, VertexPredicate::ScalarProperty { key, predicate }),
        )?;
        return Ok(GraphMutationValue::Literal(literal));
    }
    // StoredLiteral checks the complete canonical size and nesting before
    // copying. Its comparison is the same governed three-valued comparison
    // used by MATCH, including exact Int/Float peers inside nested lists.
    let expression = GraphBooleanExpression::prepare(&[GraphBooleanOp::Compare {
        left: GraphBooleanOperand::Property { variable, key },
        comparison: IntegerComparison::Equal,
        right: GraphBooleanOperand::StoredLiteral(value),
    }])
    .map_err(|_| error(at, GraphPatternTextErrorKind::ScalarLiteral))?;
    let value = GraphValue::from_property_value(value).ok_or_else(refused)?;
    if value.to_written_property().is_none() {
        return Err(refused().into());
    }
    built(at, builder.filter_boolean(&expression))?;
    Ok(GraphMutationValue::Composite(crate::GraphSetValue::Value(
        value,
    )))
}

fn bind_merge_values(
    template: &PreparedGraphVertexMergeText,
    values: &[GqlParameterValue],
) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
    let mut selection = template.selection.clone();
    let variable = selection.columns[0].variable.clone();
    let mut fields = Vec::new();
    for (key, value_template) in &template.properties {
        let value = match value_template {
            VertexMergeValueTemplate::Stored { value, at } => {
                let value = Parser::constant_written_value(value, values)?.ok_or_else(|| {
                    error(
                        *at,
                        GraphPatternTextErrorKind::Expected(
                            "non-null scalar or bounded list MERGE property value",
                        ),
                    )
                })?;
                fields.push((
                    *key,
                    stored_merge_field(&mut selection.builder, &variable, *key, &value, *at)?,
                ));
                continue;
            }
            VertexMergeValueTemplate::Bound(value) => value.clone(),
            VertexMergeValueTemplate::Parameter { index, at } => {
                let value = scalar(values[*index].clone(), *at)?;
                if matches!(value.value(), CanonicalScalar::Null) {
                    return Err(error(
                        *at,
                        GraphPatternTextErrorKind::Expected("non-null MERGE property value"),
                    )
                    .into());
                }
                value
            }
        };
        fields.push((*key, GraphMutationValue::Literal(value)));
    }
    let selection = selection.bind_values(values)?;
    let creation = PreparedGraphInsert::prepare_standalone(
        template.relation,
        vec![GraphInsertVertex {
            labels: template.labels.clone(),
            properties: fields,
        }],
        Vec::new(),
    )
    .map_err(|source| GraphVertexMergeTextError {
        offset: template.selection.return_at,
        kind: GraphVertexMergeTextErrorKind::InsertBuild(source),
    })?;
    PreparedGraphVertexMerge::prepare(selection, template.relation, 0, creation)
        .map_err(|source| merge_error(template.selection.return_at, source))
}

impl PreparedGraphVertexMergeText {
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexMergeTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphVertexMergeTextError> {
        Self::prepare_with_parameter_types_and_resolver(
            statement,
            relation,
            declarations,
            &mut resolve,
        )
    }

    /// As [`Self::prepare_with_parameter_types`], with a host resolver whose
    /// pinned tzdb the statement's temporal constructors use (fgdb-txbg9).
    pub(crate) fn prepare_with_parameter_types_and_resolver<R>(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<Self, GraphVertexMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        let (variable, labels, properties) = parser.vertex_merge_pattern()?;
        parser.end()?;
        let syntax = parser.syntax;
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
        resolve_merge_template(
            statement,
            relation,
            syntax,
            variable,
            labels,
            properties,
            &mut symbol,
        )
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        self.selection.statement()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.selection.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
        let values = self.selection.checked_arguments(arguments)?;
        bind_merge_values(self, &values)
    }
}

impl PreparedGraphVertexUpsertText {
    pub fn prepare(
        statement: &str,
        relation: RelationId,
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        Self::prepare_definition(statement, relation, declarations, resolve, false)
            .map(|(upsert, _)| upsert)
    }

    /// The upsert and, in query mode, its terminal RETURN. A RETURN makes a
    /// MERGE without any SET clause a statement of this shape too.
    pub(crate) fn prepare_definition(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl GraphSymbolResolver,
        returning: bool,
    ) -> Result<(Self, Option<VertexReturnTemplate>), GraphVertexUpsertTextError> {
        let mut parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        let (variable, labels, properties) =
            parser.vertex_merge_pattern().map_err(upsert_merge_error)?;
        let pattern_keys: Vec<_> = properties.iter().map(|property| property.key).collect();
        let (parsed_match, parsed_create, parsed_after) =
            parser.upsert_branch_actions(variable, None, false, Some(&pattern_keys))?;
        if !returning
            && parsed_match.is_empty()
            && parsed_create.is_empty()
            && parsed_after.is_empty()
        {
            return Err(error(
                statement.len(),
                GraphPatternTextErrorKind::Expected("ON MATCH SET, ON CREATE SET or SET"),
            )
            .into());
        }
        let parsed_return = if returning {
            Some(parser.vertex_return(variable)?)
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
        // This standalone MERGE constructs its pattern directly, bypassing
        // the read relation's key-binding pass. Computed RETURN/WITH reads
        // still bind their property slots through the same catalog now.
        syntax.element_catalog.resolve(&mut resolve, statement);
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
        let mut resolve_actions = upsert_action_resolver(|parsed| {
            parsed
                .into_iter()
                .map(|action| {
                    Ok(match action {
                        ParsedUpsertAction::Property { key, value } => {
                            let GraphSymbol::Property(key) =
                                symbol(GraphSymbolKind::Property, key)?
                            else {
                                unreachable!("symbol domain checked")
                            };
                            VertexUpsertActionTemplate::Property { key, value }
                        }
                        ParsedUpsertAction::Expression {
                            key,
                            inputs,
                            program,
                            at,
                        } => {
                            let GraphSymbol::Property(key) =
                                symbol(GraphSymbolKind::Property, key)?
                            else {
                                unreachable!("symbol domain checked")
                            };
                            // A unique MERGE parses with no row, so every
                            // input is a property of its vertex.
                            let properties = inputs
                                .into_iter()
                                .map(|input| {
                                    let UpsertInput::Property(name) = input else {
                                        return Err(error(
                                            at,
                                            GraphPatternTextErrorKind::Expected(
                                                "the MERGE vertex variable",
                                            ),
                                        ));
                                    };
                                    let GraphSymbol::Property(key) =
                                        symbol(GraphSymbolKind::Property, name)?
                                    else {
                                        unreachable!("symbol domain checked")
                                    };
                                    Ok(key)
                                })
                                .collect::<Result<Vec<_>, GraphPatternTextError>>()?;
                            VertexUpsertActionTemplate::Expression {
                                key,
                                properties,
                                program,
                                at,
                            }
                        }
                        // A unique MERGE parses with no row to read.
                        ParsedUpsertAction::RowValue { at, .. }
                        | ParsedUpsertAction::RowVertex { at, .. }
                        | ParsedUpsertAction::RowProperties { at, .. } => {
                            return Err(error(
                                at,
                                GraphPatternTextErrorKind::Expected("the MERGE vertex variable"),
                            ));
                        }
                        ParsedUpsertAction::Label { label } => {
                            let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)?
                            else {
                                unreachable!("symbol domain checked")
                            };
                            VertexUpsertActionTemplate::Label {
                                label,
                                present: true,
                            }
                        }
                        ParsedUpsertAction::ClearProperties { keep } => {
                            let mut keep = keep
                                .into_iter()
                                .map(|key| property_symbol(&mut symbol, key))
                                .collect::<Result<Vec<_>, _>>()?;
                            keep.sort_unstable();
                            keep.dedup();
                            VertexUpsertActionTemplate::ClearProperties { keep }
                        }
                    })
                })
                .collect()
        });
        let on_match = resolve_actions(parsed_match)?;
        let on_create = resolve_actions(parsed_create)?;
        let after = resolve_actions(parsed_after)?;
        drop(resolve_actions);
        let mut returning = parsed_return
            .map(|parsed| parsed.resolve(&mut symbol))
            .transpose()?;
        let merge = resolve_merge_template(
            statement,
            relation,
            syntax,
            variable,
            labels,
            properties,
            &mut symbol,
        )
        .map_err(upsert_merge_error)?;
        drop(symbol);
        if let Some(returning) = &mut returning
            && (returning
                .bindings
                .contains(&GraphVertexReturnBinding::Properties)
                || returning
                    .bindings
                    .contains(&GraphVertexReturnBinding::Labels))
        {
            // The returned map is read after all clauses. Preserve the host's
            // complete trusted naming contract, including keys absent from
            // the MATCH and newly assigned by either branch. Probed words do
            // not establish a complete property catalog.
            returning.property_catalog = Some(std::sync::Arc::new(
                ReverseSymbolCatalog::from_resolver(&mut resolve, statement),
            ));
        }
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
    ) -> Result<PreparedGraphVertexUpsert, GraphVertexUpsertTextError> {
        let values = self.merge.selection.checked_arguments(arguments)?;
        let merge = bind_merge_values(&self.merge, &values).map_err(upsert_merge_error)?;
        let bind_actions = |templates: &[VertexUpsertActionTemplate]| -> Result<Vec<GraphVertexUpsertAction>, GraphVertexUpsertTextError> {
            templates.iter().map(|action| Ok(match action {
                VertexUpsertActionTemplate::Property { key, value } => match value {
                    VertexUpsertValueTemplate::Bound(value) =>
                        GraphVertexUpsertAction::SetProperty { key: *key, value: value.clone() },
                    VertexUpsertValueTemplate::Parameter { index, at } =>
                        GraphVertexUpsertAction::SetProperty {
                            key: *key,
                            value: scalar(values[*index].clone(), *at)?,
                        },
                    VertexUpsertValueTemplate::Composite { value, at } =>
                        constant_list_action(*key, value, &values, *at)?,
                },
                VertexUpsertActionTemplate::Expression { key, properties, program, at } =>
                    GraphVertexUpsertAction::SetExpression {
                        key: *key,
                        properties: properties.clone(),
                        value: super::integer::bind_integer(program, &values, *at)?,
                    },
                VertexUpsertActionTemplate::Label { label, present } =>
                    GraphVertexUpsertAction::SetLabel { label: *label, present: *present },
                VertexUpsertActionTemplate::ClearProperties { keep } =>
                    GraphVertexUpsertAction::ClearProperties { keep: keep.clone() },
            })).collect()
        };
        let on_match = bind_actions(&self.on_match)?;
        let on_create = bind_actions(&self.on_create)?;
        let after = bind_actions(&self.after)?;
        PreparedGraphVertexUpsert::prepare_with_trailing_actions(merge, on_match, on_create, after)
            .map_err(|source| GraphVertexUpsertTextError {
                offset: self.merge.selection.return_at,
                kind: GraphVertexUpsertTextErrorKind::UpsertBuild(source),
            })
    }
}

/// RETURN after MERGE: the merge variable, its properties after every
/// clause, and expressions over them. MERGE chooses one vertex, so there
/// is one input row and nothing else is in scope.
#[derive(Clone, Copy)]
enum VertexBinding<'a> {
    Vertex,
    Property(Name<'a>),
    Properties,
    Labels,
}

pub(super) struct ParsedVertexReturn<'a> {
    variable: Name<'a>,
    bindings: Vec<(VertexBinding<'a>, Name<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    grouping: Option<WriteReturnGroupTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
    tail: Option<Vec<ReadStageTemplate>>,
}

impl<'a> mutation_query::ReturnLeaves<'a> for ParsedVertexReturn<'a> {
    fn metadata_leaf(
        &mut self,
        parser: &mut Parser<'a>,
        source: Name<'a>,
    ) -> Result<Option<usize>, GraphPatternTextError> {
        if source.text == self.variable.text {
            self.leaf(parser)
        } else {
            Ok(None)
        }
    }
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        if (word.eq_ignore_ascii_case("properties") || word.eq_ignore_ascii_case("keys"))
            && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let mut lookahead = parser.lexer.clone();
            lookahead.next()?;
            if matches!(lookahead.next()?.kind, TokenKind::Word(variable)
                    if variable == self.variable.text && !parser.elements.contains(&variable))
                && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
            {
                parser.advance()?;
                parser.punct(b'(', "(")?;
                let variable = parser.name()?;
                parser.punct(b')', ")")?;
                return self
                    .map_input(parser, variable, ReturnMapInput::Properties)
                    .map(Some);
            }
            // Properties of MAP/NULL values stay with the shared value
            // evaluator; only the actual MERGE vertex needs a source cell.
            return Ok(None);
        }
        // labels() of the MERGE vertex reads its labels after every clause
        // (fgdb-iufb7); the host names them through the label catalog.
        if word.eq_ignore_ascii_case("labels")
            && matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            let mut lookahead = parser.lexer.clone();
            lookahead.next()?;
            if matches!(lookahead.next()?.kind, TokenKind::Word(variable)
                    if variable == self.variable.text && !parser.elements.contains(&variable))
                && matches!(lookahead.next()?.kind, TokenKind::Punct(b')'))
            {
                let name = parser.name()?;
                parser.punct(b'(', "(")?;
                parser.name()?;
                parser.punct(b')', ")")?;
                return self
                    .column(VertexBinding::Labels, name, GraphSetColumnType::List)
                    .map(Some);
            }
            return Ok(None);
        }
        if word != self.variable.text
            || matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
        {
            return Ok(None);
        }
        let name = parser.name()?;
        let input = if parser.take(b'.')? {
            ReturnMapInput::Property(parser.schema_name()?)
        } else {
            ReturnMapInput::Identity
        };
        self.map_input(parser, name, input).map(Some)
    }

    fn map_input(
        &mut self,
        _: &mut Parser<'a>,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        if source.text != self.variable.text {
            return Err(error(source.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        let (binding, alias, kind) = match input {
            ReturnMapInput::Identity | ReturnMapInput::Guard => {
                (VertexBinding::Vertex, source, GraphSetColumnType::Vertex)
            }
            ReturnMapInput::Property(key) => (
                VertexBinding::Property(key),
                key,
                GraphSetColumnType::Stored,
            ),
            ReturnMapInput::Properties => {
                (VertexBinding::Properties, source, GraphSetColumnType::Any)
            }
        };
        self.column(binding, alias, kind)
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, _, kind)| *kind).collect()
    }
}

impl<'a> ParsedVertexReturn<'a> {
    fn column(
        &mut self,
        binding: VertexBinding<'a>,
        alias: Name<'a>,
        kind: GraphSetColumnType,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(column) = self
            .bindings
            .iter()
            .position(|(old, _, _)| match (old, binding) {
                (VertexBinding::Vertex, VertexBinding::Vertex) => true,
                (VertexBinding::Properties, VertexBinding::Properties)
                | (VertexBinding::Labels, VertexBinding::Labels) => true,
                (VertexBinding::Property(old), VertexBinding::Property(key)) => {
                    old.text == key.text
                }
                _ => false,
            })
        {
            return Ok(column);
        }
        if self.bindings.len() == MAX_PATTERN_VERTICES {
            return Err(error(
                alias.at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: self.bindings.len() + 1,
                }),
            ));
        }
        self.bindings.push((binding, alias, kind));
        Ok(self.bindings.len() - 1)
    }

    fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<VertexReturnTemplate, GraphPatternTextError> {
        let mut bindings = Vec::new();
        for (binding, _, _) in self.bindings {
            bindings.push(match binding {
                VertexBinding::Vertex => GraphVertexReturnBinding::Vertex,
                VertexBinding::Properties => GraphVertexReturnBinding::Properties,
                VertexBinding::Labels => GraphVertexReturnBinding::Labels,
                VertexBinding::Property(key) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("shared catalog resolver checked the domain")
                    };
                    GraphVertexReturnBinding::Property(key)
                }
            });
        }
        Ok(VertexReturnTemplate {
            bindings,
            projection: self.projection,
            grouping: self.grouping,
            quantifier: self.quantifier,
            order: self.order,
            offset: self.offset,
            count: self.count,
            at: self.at,
            property_catalog: None,
            tail: self.tail,
        })
    }
}

impl<'a> Parser<'a> {
    fn vertex_return(
        &mut self,
        variable: Name<'a>,
    ) -> Result<ParsedVertexReturn<'a>, GraphVertexUpsertTextError> {
        let at = self.current.at;
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
        let mut returning = ParsedVertexReturn {
            variable,
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
        let (output, spellings) = if self.take(b'*')? {
            returning
                .bindings
                .push((VertexBinding::Vertex, variable, GraphSetColumnType::Vertex));
            returning.projection.push(ReadProjectionTemplate {
                name: variable.text.to_owned(),
                value: ReadValueTemplate::Column(0),
            });
            (vec![(variable, GraphSetColumnType::Vertex)], Vec::new())
        } else {
            let items = self.write_return_items(&mut returning)?;
            returning.projection = items.projection;
            returning.grouping = items.grouping;
            (items.output, items.spellings)
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
            let tail = self.vertex_return_tail(&mut returning, output)?;
            returning.tail = Some(tail);
        }
        Ok(returning)
    }

    /// A directly carried MERGE vertex, including a grouping key, reads its
    /// final staged fields through the existing write owner. Hidden property
    /// keys do not change identity grouping or aggregate argument slots.
    /// A MIN/MAX/COLLECT representative never acquires a new graph read.
    fn vertex_return_tail(
        &mut self,
        returning: &mut ParsedVertexReturn<'a>,
        mut schema: Vec<(Name<'a>, GraphSetColumnType)>,
    ) -> Result<Vec<ReadStageTemplate>, GraphVertexUpsertTextError> {
        let visible = schema.len();
        let mut reads = Vec::new();
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
            if !matches!(
                returning.bindings[binding],
                (VertexBinding::Vertex, _, GraphSetColumnType::Vertex)
            ) {
                continue;
            }
            let column = returning.column(
                VertexBinding::Property(key),
                key,
                GraphSetColumnType::Scalar,
            )?;
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

impl VertexReturnTemplate {
    fn bind(
        &self,
        upsert: PreparedGraphVertexUpsert,
        values: &[GqlParameterValue],
    ) -> Result<PreparedGraphVertexUpsertQuery, GraphVertexUpsertTextError> {
        let mut projection = Vec::new();
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = PreparedGraphVertexUpsertQuery::prepare_with_grouping(
            upsert,
            self.bindings.clone(),
            projection,
            self.quantifier,
            self.grouping
                .as_ref()
                .map(|group| group.bind(values))
                .transpose()?,
            self.property_catalog.clone(),
        )
        .map_err(|kind| GraphVertexUpsertTextError {
            offset: self.at,
            kind: GraphVertexUpsertTextErrorKind::ReturnBuild(kind),
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

impl PreparedGraphVertexUpsertQueryText {
    /// Classify write text with native token framing: a statement that
    /// starts with `MERGE (` and has a top-level RETURN, in one statement.
    /// Quoted values, property keys and aliases never become clause words.
    /// This validates nothing and resolves no symbol; prepare owns that.
    pub fn has_return_clause(statement: &str) -> Result<bool, GraphVertexUpsertTextError> {
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
        let opening = script::next_script_token(&mut lexer.clone())?;
        if !matches!(first.kind, TokenKind::Word(word) if word.eq_ignore_ascii_case("MERGE"))
            || !matches!(opening.kind, TokenKind::Punct(b'('))
        {
            return Ok(false);
        }
        let mut depth = 0_usize;
        let mut previous = first.kind;
        loop {
            let token = script::next_script_token(&mut lexer)?;
            match token.kind {
                TokenKind::End | TokenKind::Punct(b';') => return Ok(false),
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TokenKind::Word(word) if depth == 0 && word.eq_ignore_ascii_case("RETURN") => {
                    let alias = matches!(previous, TokenKind::Punct(b'.'))
                        || matches!(previous, TokenKind::Word(word) if word.eq_ignore_ascii_case("AS"));
                    if !alias {
                        return Ok(true);
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
    ) -> Result<Self, GraphVertexUpsertTextError> {
        Self::prepare_with_parameter_types(statement, relation, &[], resolve)
    }

    pub fn prepare_with_parameter_types(
        statement: &str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: impl GraphSymbolResolver,
    ) -> Result<Self, GraphVertexUpsertTextError> {
        let body = mutation_query::statement_body(statement, "one MERGE RETURN statement")?;
        let (upsert, returning) = PreparedGraphVertexUpsertText::prepare_definition(
            body,
            relation,
            declarations,
            resolve,
            true,
        )?;
        let returning = returning.expect("query mode prepares a RETURN definition");
        Ok(Self {
            statement: statement.to_owned(),
            upsert,
            returning,
        })
    }

    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        self.upsert.merge.selection.parameter_schema()
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphVertexUpsertQuery, GraphVertexUpsertTextError> {
        let upsert = self.upsert.bind_parameters(arguments)?;
        let values = self.upsert.merge.selection.checked_arguments(arguments)?;
        self.returning.bind(upsert, &values)
    }
}

impl<'a> Parser<'a> {
    /// `MERGE ([v][:L ...] [{k: value, ...}])` as a chained write over `row`.
    /// The variable is optional and must be new to the row. A value may read
    /// the row: a scalar column by name, or a carried element's hidden read.
    #[allow(clippy::type_complexity)]
    fn vertex_merge_row_pattern(
        &mut self,
        row: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<
        (
            Option<Name<'a>>,
            Vec<Name<'a>>,
            Vec<(Name<'a>, RowValueTemplate)>,
        ),
        GraphVertexMergeTextError,
    > {
        self.word("MERGE")?;
        self.punct(b'(', "(")?;
        let variable = if matches!(self.current.kind, TokenKind::Word(_)) {
            let name = self.name()?;
            if row.iter().any(|(carried, _)| carried.text == name.text) {
                return Err(error(
                    name.at,
                    GraphPatternTextErrorKind::Expected("a MERGE variable the row does not carry"),
                )
                .into());
            }
            Some(name)
        } else {
            None
        };
        let mut labels: Vec<Name<'a>> = Vec::new();
        while self.take(b':')? {
            let label = self.schema_name()?;
            if labels.iter().any(|previous| previous.text == label.text) {
                return Err(insert_error(
                    label.at,
                    GraphInsertBuildError::DuplicateLabel { vertex: 0 },
                ));
            }
            labels.push(label);
        }
        let mut properties: Vec<(Name<'a>, RowValueTemplate)> = Vec::new();
        if self.take(b'{')? {
            loop {
                if properties.len() >= crate::insertion::MAX_GRAPH_INSERT_FIELDS {
                    return Err(insert_error(
                        self.current.at,
                        GraphInsertBuildError::TooManyFields {
                            limit: crate::insertion::MAX_GRAPH_INSERT_FIELDS,
                            observed: properties.len() + 1,
                        },
                    ));
                }
                let key = self.schema_name()?;
                if properties
                    .iter()
                    .any(|(previous, _)| previous.text == key.text)
                {
                    return Err(insert_error(
                        key.at,
                        GraphInsertBuildError::DuplicateProperty { declaration: 0 },
                    ));
                }
                self.punct(b':', ":")?;
                // A bare column or hidden read is the column itself. A longer
                // value that reads the row (`prop.login`, `y + 1`) is computed
                // per row by the shared value compiler; a value that reads no
                // row binding stays a bound literal or parameter.
                let start = (self.lexer.clone(), self.current);
                let column = self.merge_row_column(row)?;
                let value = match column {
                    Some(column) if self.is_punct(b',') || self.is_punct(b'}') => {
                        RowValueTemplate::Column(column)
                    }
                    _ if column.is_some() || self.merge_value_reads_row(row)? => {
                        (self.lexer, self.current) = start;
                        let at = self.current.at;
                        RowValueTemplate::Computed {
                            value: Box::new(self.merge_row_value(row)?),
                            at,
                        }
                    }
                    _ => {
                        let (_, value) =
                            self.merge_property_value(true)
                                .map_err(|refusal| match refusal {
                                    MergeMapError::Pattern(error) => {
                                        GraphVertexMergeTextError::from(error)
                                    }
                                    MergeMapError::Value(error) => error.into(),
                                    MergeMapError::TooManyFields { at, observed } => insert_error(
                                        at,
                                        GraphInsertBuildError::TooManyFields {
                                            limit: crate::insertion::MAX_GRAPH_INSERT_FIELDS,
                                            observed,
                                        },
                                    ),
                                    MergeMapError::DuplicateProperty { at } => insert_error(
                                        at,
                                        GraphInsertBuildError::DuplicateProperty { declaration: 0 },
                                    ),
                                })?;
                        RowValueTemplate::Value(value)
                    }
                };
                properties.push((key, value));
                if !self.take(b',')? {
                    break;
                }
            }
            self.punct(b'}', "}")?;
        }
        self.punct(b')', ")")?;
        if self.is_punct(b'-') || self.is_punct(b'<') {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected(
                    "a vertex MERGE, or a relationship MERGE between named row vertices",
                ),
            )
            .into());
        }
        Ok((variable, labels, properties))
    }

    /// A MERGE map value that reads the carried row: a column by name, or
    /// `a.key` of a carried element through its hidden read. A graph
    /// element column is no property value.
    fn merge_row_column(
        &mut self,
        row: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = self.current.kind else {
            return Ok(None);
        };
        match self.lexer.clone().next()?.kind {
            TokenKind::Punct(b'.') => return self.boundary_read(row.len()),
            TokenKind::Punct(b'(') => return Ok(None),
            _ => {}
        }
        let visible = self.visible_width(row);
        let Some(column) = row[..visible]
            .iter()
            .position(|(name, _)| name.text == word)
        else {
            return Ok(None);
        };
        if !matches!(
            row[column].1,
            GraphSetColumnType::Scalar
                | GraphSetColumnType::Stored
                | GraphSetColumnType::List
                | GraphSetColumnType::Any
        ) {
            return Err(error(
                self.current.at,
                GraphPatternTextErrorKind::Expected("a scalar or list row value in a MERGE map"),
            ));
        }
        self.advance()?;
        Ok(Some(column))
    }

    /// Whether the MERGE map value at the cursor names a visible row binding.
    fn merge_value_reads_row(
        &self,
        row: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<bool, GraphPatternTextError> {
        Ok(self.value_reads(row, None)?.0)
    }

    /// Whether the value at the cursor names a visible row binding, and
    /// whether it names `variable`, read with native tokens and without
    /// consuming the parser's own. The value ends at a `,` or closing
    /// bracket outside its own brackets, at a clause keyword, or at the end.
    /// A property key after `.` names no binding.
    fn value_reads(
        &self,
        row: &[(Name<'a>, GraphSetColumnType)],
        variable: Option<&str>,
    ) -> Result<(bool, bool), GraphPatternTextError> {
        const CLAUSES: [&str; 18] = [
            "ON", "SET", "RETURN", "WITH", "MERGE", "CREATE", "DELETE", "DETACH", "REMOVE",
            "UNWIND", "MATCH", "OPTIONAL", "FOREACH", "CALL", "UNION", "ORDER", "SKIP", "LIMIT",
        ];
        let visible = &row[..self.visible_width(row)];
        let mut lexer = self.lexer.clone();
        let mut token = self.current;
        let mut depth = 0usize;
        let mut key = false;
        let (mut reads_row, mut reads_variable) = (false, false);
        loop {
            match token.kind {
                TokenKind::End => break,
                TokenKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TokenKind::Punct(b')' | b']' | b'}' | b',') if depth == 0 => break,
                TokenKind::Punct(b')' | b']' | b'}') => depth -= 1,
                TokenKind::Word(word)
                    if depth == 0
                        && CLAUSES
                            .iter()
                            .any(|clause| clause.eq_ignore_ascii_case(word)) =>
                {
                    break;
                }
                TokenKind::Word(word) if !key => {
                    reads_row |= visible.iter().any(|(name, _)| name.text == word);
                    reads_variable |= variable == Some(word);
                }
                _ => {}
            }
            key = matches!(token.kind, TokenKind::Punct(b'.'));
            token = lexer.next()?;
        }
        Ok((reads_row, reads_variable))
    }

    /// A MERGE map value computed from the row, read as a WITH item over it:
    /// its columns by name and type, so a map column's `prop.login` reads
    /// the entry, and `a.key` of a carried element through its hidden read.
    fn merge_row_value(
        &mut self,
        row: &[(Name<'a>, GraphSetColumnType)],
    ) -> Result<ReadValueTemplate, crate::GraphSetTextError> {
        self.read_row_value(row, 0)
    }
}

/// One RETURN leaf of a vertex MERGE step before symbol resolution.
#[derive(Clone, Copy)]
enum RowsBinding<'a> {
    Input(usize),
    Vertex,
    Property(Name<'a>),
    /// A property of the element in a row column, after the statement.
    InputProperty(usize, Name<'a>),
}

/// RETURN after a vertex MERGE step (fgdb-cpny9): the row it read (names
/// and hidden reads), the merged vertex and that vertex's properties after
/// the statement.
struct ParsedVertexRowsReturn<'r, 'a> {
    variable: Option<Name<'a>>,
    row: &'r [(Name<'a>, GraphSetColumnType)],
    /// With ON/SET clauses, the property names they write. A carried
    /// element's properties read their final values (InputProperty), but its
    /// labels(), keys() and properties() would read a hidden read from
    /// before the clauses, so RETURN refuses them.
    written: Option<Vec<&'a str>>,
    bindings: Vec<(RowsBinding<'a>, GraphSetColumnType)>,
    projection: Vec<ReadProjectionTemplate>,
    grouping: Option<WriteReturnGroupTemplate>,
    quantifier: GraphSetQuantifier,
    order: Vec<crate::algebra::GraphValueOrder>,
    offset: ReadPageNumber,
    count: Option<ReadPageNumber>,
    at: usize,
    tail: Option<Vec<ReadStageTemplate>>,
    /// The output row: each item's name and type.
    output: Vec<(Name<'a>, GraphSetColumnType)>,
}

impl<'a> ParsedVertexRowsReturn<'_, 'a> {
    /// Carry future reads as final-state MERGE bindings. The following
    /// write receives these cells as its preimage; a terminal mutation still
    /// overlays its own assignments before evaluating RETURN.
    fn hoist_continuation_reads(
        &mut self,
        requested: Vec<(Name<'a>, Name<'a>)>,
    ) -> Result<Option<BoundaryReads<'a>>, GraphPatternTextError> {
        let visible = self.output.len();
        let mut reads = Vec::new();
        for (alias, key) in requested {
            let Some(at) = self.output[..visible]
                .iter()
                .position(|(name, _)| name.text == alias.text)
            else {
                continue;
            };
            let Some(binding) = WriteReturnGroupTemplate::carried_binding(
                self.grouping.as_ref(),
                &self.projection[at].value,
            ) else {
                continue;
            };
            let (binding, GraphSetColumnType::Vertex | GraphSetColumnType::Edge) =
                self.bindings[binding]
            else {
                continue;
            };
            let property = match binding {
                RowsBinding::Vertex => RowsBinding::Property(key),
                RowsBinding::Input(column) => RowsBinding::InputProperty(column, key),
                RowsBinding::Property(_) | RowsBinding::InputProperty(_, _) => continue,
            };
            let column = self.column(property, GraphSetColumnType::Stored, key.at)?;
            let column = match &mut self.grouping {
                Some(group) => group.hoist_property(&mut self.projection, column, key.at)?,
                None => column,
            };
            mutation_query::push_tail_read(
                &mut self.projection,
                &mut self.output,
                &mut reads,
                (alias, key),
                column,
            )?;
        }
        Ok((!reads.is_empty()).then(|| BoundaryReads {
            visible,
            width: self.output.len(),
            reads,
            functions: Vec::new(),
            grouped: Vec::new(),
            scoped: Vec::new(),
        }))
    }

    fn column(
        &mut self,
        binding: RowsBinding<'a>,
        kind: GraphSetColumnType,
        at: usize,
    ) -> Result<usize, GraphPatternTextError> {
        if let Some(column) = self
            .bindings
            .iter()
            .position(|(old, _)| match (old, binding) {
                (RowsBinding::Input(old), RowsBinding::Input(new)) => *old == new,
                (RowsBinding::Vertex, RowsBinding::Vertex) => true,
                (RowsBinding::Property(old), RowsBinding::Property(new)) => old.text == new.text,
                (
                    RowsBinding::InputProperty(old_column, old),
                    RowsBinding::InputProperty(new_column, new),
                ) => *old_column == new_column && old.text == new.text,
                _ => false,
            })
        {
            return Ok(column);
        }
        if self.bindings.len() == MAX_PATTERN_VERTICES {
            return Err(error(
                at,
                GraphPatternTextErrorKind::Build(PatternBuildError::LimitExceeded {
                    dimension: crate::algebra::PatternLimitDimension::Columns,
                    limit: MAX_PATTERN_VERTICES,
                    observed: self.bindings.len() + 1,
                }),
            ));
        }
        self.bindings.push((binding, kind));
        Ok(self.bindings.len() - 1)
    }

    fn resolve(
        self,
        symbol: &mut impl FnMut(GraphSymbolKind, Name<'a>) -> Result<GraphSymbol, GraphPatternTextError>,
    ) -> Result<VertexMergeRowsReturnTemplate, GraphPatternTextError> {
        let mut bindings = Vec::with_capacity(self.bindings.len());
        for (binding, _) in self.bindings {
            bindings.push(match binding {
                RowsBinding::Input(column) => crate::GraphVertexMergeRowsBinding::Input(column),
                RowsBinding::Vertex => crate::GraphVertexMergeRowsBinding::Vertex,
                RowsBinding::Property(key) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("symbol domain checked by shared resolver")
                    };
                    crate::GraphVertexMergeRowsBinding::Property(key)
                }
                RowsBinding::InputProperty(column, key) => {
                    let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                        unreachable!("symbol domain checked by shared resolver")
                    };
                    crate::GraphVertexMergeRowsBinding::InputProperty { column, key }
                }
            });
        }
        Ok(VertexMergeRowsReturnTemplate {
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

impl<'a> ReturnLeaves<'a> for ParsedVertexRowsReturn<'_, 'a> {
    fn leaf(&mut self, parser: &mut Parser<'a>) -> Result<Option<usize>, GraphPatternTextError> {
        let TokenKind::Word(word) = parser.current.kind else {
            return Ok(None);
        };
        let at = parser.current.at;
        // After ON/SET, labels(), keys() and properties() of a carried
        // element would read its state before the clauses.
        if self.written.is_some()
            && ["labels", "keys", "properties"]
                .iter()
                .any(|function| word.eq_ignore_ascii_case(function))
        {
            let mut lexer = parser.lexer.clone();
            if matches!(lexer.next()?.kind, TokenKind::Punct(b'('))
                && matches!(lexer.next()?.kind, TokenKind::Word(argument)
                    if !self.variable.is_some_and(|variable| variable.text == argument))
            {
                return Err(error(
                    at,
                    GraphPatternTextErrorKind::Expected(
                        "no carried-element metadata in RETURN after MERGE ON/SET",
                    ),
                ));
            }
        }
        // Functions stay with the shared compiler; their arguments return here.
        if matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'('))
            || parser.elements.contains(&word)
        {
            return Ok(None);
        }
        if self.variable.is_some_and(|variable| variable.text == word) {
            parser.advance()?;
            if parser.take(b'.')? {
                let key = parser.schema_name()?;
                return self
                    .column(RowsBinding::Property(key), GraphSetColumnType::Stored, at)
                    .map(Some);
            }
            return self
                .column(RowsBinding::Vertex, GraphSetColumnType::Vertex, at)
                .map(Some);
        }
        let visible = parser.visible_width(self.row);
        if matches!(parser.lexer.clone().next()?.kind, TokenKind::Punct(b'.')) {
            // `a.key` of a vertex or relationship the row carries reads its
            // value after the statement, ON/SET effects included: the MERGE
            // may have merged that same element.
            if let Some(column) = self.row[..visible].iter().position(|(name, kind)| {
                name.text == word
                    && matches!(kind, GraphSetColumnType::Vertex | GraphSetColumnType::Edge)
            }) {
                parser.advance()?;
                parser.punct(b'.', ".")?;
                let key = parser.schema_name()?;
                return self
                    .column(
                        RowsBinding::InputProperty(column, key),
                        GraphSetColumnType::Stored,
                        at,
                    )
                    .map(Some);
            }
            // Otherwise a carried element's hidden read, if the row has one.
            let Some(column) = parser.boundary_read(self.row.len())? else {
                return Ok(None);
            };
            let kind = self.row[column].1;
            return self.column(RowsBinding::Input(column), kind, at).map(Some);
        }
        let Some(column) = self.row[..visible]
            .iter()
            .position(|(name, _)| name.text == word)
        else {
            return Ok(None);
        };
        parser.advance()?;
        let kind = self.row[column].1;
        self.column(RowsBinding::Input(column), kind, at).map(Some)
    }

    fn map_input(
        &mut self,
        _: &mut Parser<'a>,
        source: Name<'a>,
        input: ReturnMapInput<'a>,
    ) -> Result<usize, GraphPatternTextError> {
        if !self
            .variable
            .is_some_and(|variable| variable.text == source.text)
        {
            return Err(error(source.at, GraphPatternTextErrorKind::UnknownVariable));
        }
        match input {
            ReturnMapInput::Identity | ReturnMapInput::Guard => {
                self.column(RowsBinding::Vertex, GraphSetColumnType::Vertex, source.at)
            }
            ReturnMapInput::Property(key) => self.column(
                RowsBinding::Property(key),
                GraphSetColumnType::Stored,
                key.at,
            ),
            ReturnMapInput::Properties => Err(error(
                source.at,
                GraphPatternTextErrorKind::Expected(
                    "named vertex properties in RETURN after a chained MERGE",
                ),
            )),
        }
    }

    fn types(&self) -> Vec<GraphSetColumnType> {
        self.bindings.iter().map(|(_, kind)| *kind).collect()
    }
}

impl<'a> Parser<'a> {
    /// `RETURN ...` after a vertex MERGE step, over `row` and the merged
    /// vertex. Named items only.
    /// `keyword` is RETURN, or WITH for the head of a step another write
    /// follows (fgdb-cpny9): the same items, whose output row the next
    /// write reads.
    fn vertex_rows_return<'r>(
        &mut self,
        keyword: &'static str,
        variable: Option<Name<'a>>,
        row: &'r [(Name<'a>, GraphSetColumnType)],
        written: Option<Vec<&'a str>>,
    ) -> Result<ParsedVertexRowsReturn<'r, 'a>, GraphVertexMergeTextError> {
        let at = self.current.at;
        self.word(keyword)?;
        let quantifier = if self.take_word("DISTINCT")? {
            GraphSetQuantifier::Distinct
        } else {
            self.take_all_quantifier()?;
            GraphSetQuantifier::All
        };
        let star = self.is_punct(b'*').then_some(self.current.at);
        let mut returning = ParsedVertexRowsReturn {
            variable,
            row,
            written,
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
        if let Some(star_at) = star {
            // `WITH *` / `RETURN *` (TCK Match8 [2]): every named binding the
            // row carries, then the merged vertex when it is named. RETURN *
            // orders its columns by name; WITH * keeps the row's order
            // (fgdb-7jja3).
            self.advance()?;
            let visible = self.visible_width(row);
            let mut items: Vec<_> = row[..visible]
                .iter()
                .enumerate()
                .map(|(index, &(name, kind))| (RowsBinding::Input(index), name, kind))
                .chain(variable.map(|name| (RowsBinding::Vertex, name, GraphSetColumnType::Vertex)))
                .filter(|(_, name, _)| !name.text.starts_with(Self::ANONYMOUS_PREFIX))
                .collect();
            if items.is_empty() {
                return Err(error(
                    star_at,
                    GraphPatternTextErrorKind::Expected("named bindings for * after a MERGE"),
                )
                .into());
            }
            if keyword == "RETURN" {
                items.sort_by(|(_, a, _), (_, b, _)| a.text.cmp(b.text));
            }
            for (binding, name, kind) in items {
                let column = returning.column(binding, kind, name.at)?;
                returning.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                returning.output.push((name, kind));
            }
            if let Some(ReadStageTemplate::Page {
                order,
                offset,
                count,
                ..
            }) = self.row_page_sourced(&returning.output, &[])?
            {
                returning.order = order;
                returning.offset = offset;
                returning.count = count;
            }
            return Ok(returning);
        }
        let items = self.write_return_items(&mut returning)?;
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
        returning.output = items.output;
        Ok(returning)
    }
}

impl VertexMergeRowsReturnTemplate {
    fn bind(
        &self,
        merge: crate::PreparedGraphVertexMergeRows,
        values: &[GqlParameterValue],
    ) -> Result<crate::PreparedGraphVertexMergeRowsQuery, GraphVertexMergeTextError> {
        let mut projection = Vec::with_capacity(self.projection.len());
        for output in &self.projection {
            projection.push(GraphSetProjection::new(
                &output.name,
                return_projection::bind_read_value(&output.value, values)?,
            ));
        }
        let mut query = crate::PreparedGraphVertexMergeRowsQuery::prepare_with_grouping(
            merge,
            self.bindings.clone(),
            projection,
            self.quantifier,
            self.grouping
                .as_ref()
                .map(|group| group.bind(values))
                .transpose()?,
        )
        .map_err(|kind| GraphVertexMergeTextError {
            offset: self.at,
            kind: GraphVertexMergeTextErrorKind::ReturnBuild(kind),
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

impl PreparedGraphVertexMergeRowsText {
    /// A vertex MERGE step of a chain. `from` is the row the previous write
    /// left, or `None` when this MERGE is the statement's first write. The
    /// continuation is returned when another write follows: its row is this
    /// row's visible columns and the merged vertex, as `WITH *` would carry.
    pub(crate) fn prepare_step<'a, R>(
        statement: &'a str,
        relation: RelationId,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
        from: Option<ChainContinuation<'a>>,
    ) -> Result<(Self, Option<ChainContinuation<'a>>), GraphVertexMergeTextError>
    where
        R: GraphSymbolResolver + ?Sized,
    {
        let parser = Parser::new_with_parameter_types(statement, declarations)?
            .with_zones(resolve.time_zones());
        // `leading` is a first write's source-free prefix, if any: its UNWIND
        // stages and the WITH stages after them (`WITH 42 AS v MERGE ...`);
        // `matched` its MATCH prefix (a WITH head, or every MATCH binding as
        // `WITH *` carries them) and the row stages after it.
        // `unwound_match` is the leading-row compiler's MATCH relation: a
        // first write's `UNWIND ... MATCH ...`, or a later MATCH over owned
        // rows. Its input schema stays separate from its output schema.
        let mut matched = None;
        let mut unwound_match = None;
        let mut matched_owned = None;
        let (mut parser, row, owned, leading) = match from {
            Some(from) => {
                let (columns, types) = from.owned_columns();
                let mut parser = parser.resume_after(from.boundary, &from.parameters, from.at)?;
                if parser.is_word("MATCH") {
                    let (source, row) = parser.owned_match_row_prefix(statement, from.row)?;
                    unwound_match = Some(source);
                    matched_owned = Some(Box::new((columns, types)));
                    (parser, row, None, None)
                } else {
                    (parser, from.row, Some(types), None)
                }
            }
            None => {
                let mut parser = parser;
                if parser.is_word("UNWIND") || parser.is_word("WITH") {
                    let (unwound, schema) = if parser.is_word("UNWIND") {
                        parser.insertion_unwind_prefix()?
                    } else {
                        (Vec::new(), Vec::new())
                    };
                    if parser.is_word("MATCH") {
                        // `UNWIND $rows AS row MATCH (y {k: row.k}) MERGE ...`
                        // (fgdb-0s2pq): the rows are the ordinary read of the
                        // UNWIND stages and the MATCH, correlated as an
                        // `UNWIND ... MATCH ... CREATE` reads them.
                        let (source, schema) = parser.write_graph_row_prefix(
                            statement,
                            declarations,
                            resolve.time_zones(),
                        )?;
                        unwound_match = Some(source);
                        (parser, schema, None, None)
                    } else {
                        let (stages, schema) = if parser.is_word("WITH") {
                            parser.leading_row_stages(schema)?
                        } else {
                            (Vec::new(), schema)
                        };
                        (parser, schema, None, Some((unwound, stages)))
                    }
                } else if parser.is_word("MATCH") {
                    // A WHERE the graph predicate cannot hold is the row
                    // filter of the implicit `WITH *` below.
                    parser.write_match_prefix()?;
                    let (head, stages, schema) = if parser.is_word("WITH") {
                        parser.write_row_prefix()?
                    } else {
                        parser.implicit_write_row_prefix()?
                    };
                    matched = Some((head, stages));
                    (parser, schema, None, None)
                } else {
                    (parser, Vec::new(), None, None)
                }
            }
        };
        parser.chain_write = true;
        let keep = parser.visible_width(&row);
        let (variable, parsed_labels, parsed_properties) = parser.vertex_merge_row_pattern(&row)?;
        // ON MATCH SET, ON CREATE SET and SET name the MERGE vertex; their
        // right-hand sides may read the row.
        let parsed_actions = if parser.is_word("ON") || parser.is_word("SET") {
            let Some(variable) = variable else {
                return Err(error(
                    parser.current.at,
                    GraphPatternTextErrorKind::Expected("a named MERGE vertex before ON or SET"),
                )
                .into());
            };
            let pattern_keys: Vec<_> = parsed_properties.iter().map(|(key, _)| *key).collect();
            Some(parser.upsert_branch_actions(
                variable,
                Some(row.as_slice()),
                false,
                Some(&pattern_keys),
            )?)
        } else {
            None
        };
        let mut parsed_return = None;
        let continuation = if parser.at_chained_write()? {
            let mut carried = row[..keep].to_vec();
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
            let name = variable.unwrap_or(Name {
                text: Parser::anonymous_name(carried.len()),
                at: parser.current.at,
            });
            carried.push((name, GraphSetColumnType::Vertex));
            let requested = parser.write_chain_reads(&carried)?;
            let boundary = if requested.is_empty() && !parser.at_chained_read()? {
                None
            } else {
                // Keep identity-only MERGE chains on the compact row path.
                // A property-consuming next write instead uses the same
                // final-state projection owner as an explicit WITH.
                let mut head = ParsedVertexRowsReturn {
                    variable,
                    row: &row,
                    written: None,
                    bindings: Vec::new(),
                    projection: Vec::new(),
                    grouping: None,
                    quantifier: GraphSetQuantifier::All,
                    order: Vec::new(),
                    offset: ReadPageNumber::Literal(0),
                    count: None,
                    at: parser.current.at,
                    tail: None,
                    output: carried,
                };
                for (input, &(name, kind)) in row[..keep].iter().enumerate() {
                    let column = head.column(RowsBinding::Input(input), kind, name.at)?;
                    head.projection.push(ReadProjectionTemplate {
                        name: name.text.to_owned(),
                        value: ReadValueTemplate::Column(column),
                    });
                }
                let column =
                    head.column(RowsBinding::Vertex, GraphSetColumnType::Vertex, name.at)?;
                head.projection.push(ReadProjectionTemplate {
                    name: name.text.to_owned(),
                    value: ReadValueTemplate::Column(column),
                });
                let boundary = head.hoist_continuation_reads(requested)?;
                carried = head.output.clone();
                parsed_return = Some(head);
                boundary
            };
            Some(ChainContinuation {
                at: parser.current.at,
                row: carried,
                boundary,
                parameters: parser.syntax.parameters.clone(),
            })
        } else {
            // A carried element the MERGE also merged would read stale
            // metadata through its hidden read, so after ON/SET clauses
            // RETURN (or a WITH head) refuses labels(), keys() and
            // properties() of a carried element. Its properties read final
            // values.
            let written = parsed_actions.as_ref().map(|(on_match, on_create, after)| {
                on_match
                    .iter()
                    .chain(on_create)
                    .chain(after)
                    .filter_map(|action| match action {
                        ParsedUpsertAction::Property { key, .. }
                        | ParsedUpsertAction::Expression { key, .. }
                        | ParsedUpsertAction::RowValue { key, .. } => Some(key.text),
                        // Dynamic map keys resolve at execution. Metadata
                        // reads below retain the same post-action rules.
                        ParsedUpsertAction::RowVertex { .. }
                        | ParsedUpsertAction::RowProperties { .. }
                        | ParsedUpsertAction::Label { .. }
                        | ParsedUpsertAction::ClearProperties { .. } => None,
                    })
                    .collect()
            });
            if parser.is_word("WITH") {
                // A WITH head: its output row is what the next write reads.
                let mut head = parser.vertex_rows_return("WITH", variable, &row, written)?;
                let requested = parser.write_chain_reads(&head.output)?;
                let boundary = head.hoist_continuation_reads(requested)?;
                let continuation = if parser.at_chained_write()? {
                    Some(ChainContinuation {
                        at: parser.current.at,
                        row: head.output.clone(),
                        boundary,
                        parameters: parser.syntax.parameters.clone(),
                    })
                } else {
                    // WHERE, UNWIND and further WITH stages read the complete
                    // frozen head before a later MATCH or write. Use the same
                    // typed row pipeline as CREATE and relationship MERGE;
                    // its continuation carries the resulting names and types,
                    // not the earlier WITH's schema (fgdb-vy1rf).
                    parser.boundary_reads = boundary;
                    head.tail = Some(parser.write_row_pipeline(head.output.clone())?);
                    let continuation = parser.continuation();
                    if continuation.is_none() {
                        parser.end()?;
                    }
                    continuation
                };
                parsed_return = Some(head);
                continuation
            } else {
                if parser.is_word("RETURN") {
                    let returning = parser.vertex_rows_return("RETURN", variable, &row, written)?;
                    parsed_return = Some(returning);
                }
                parser.end()?;
                None
            }
        };
        if let Some(parsed) = &parsed_return {
            mutation_query::admit_return(
                &parsed.projection,
                &parsed.types(),
                &parser.syntax.parameters,
                parsed.grouping.as_ref(),
                parsed.at,
            )?;
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
        let mut labels = Vec::with_capacity(parsed_labels.len());
        for label in parsed_labels {
            let GraphSymbol::Label(label) = symbol(GraphSymbolKind::Label, label)? else {
                unreachable!("symbol domain checked by shared resolver")
            };
            labels.push(label);
        }
        let mut properties = Vec::with_capacity(parsed_properties.len());
        for (key, value) in parsed_properties {
            let GraphSymbol::Property(key) = symbol(GraphSymbolKind::Property, key)? else {
                unreachable!("symbol domain checked by shared resolver")
            };
            properties.push((key, value));
        }
        labels.sort_unstable();
        properties.sort_by_key(|(key, _)| *key);
        let actions = match parsed_actions {
            Some((on_match, on_create, after)) => {
                let mut resolve_actions =
                    |parsed: Vec<ParsedUpsertAction<'a>>| resolve_row_actions(parsed, &mut symbol);
                let on_match = resolve_actions(on_match)?;
                let on_create = resolve_actions(on_create)?;
                let after = resolve_actions(after)?;
                Some(Box::new(VertexMergeRowActionTemplates {
                    on_match,
                    on_create,
                    after,
                }))
            }
            None => None,
        };
        let returning = parsed_return
            .map(|parsed| parsed.resolve(&mut symbol))
            .transpose()?;
        let parameters = parser.syntax.parameters.clone();
        let parameter_offsets = parser.syntax.parameter_offsets.clone();
        // Owned, unit and row-only inputs construct their bound relation
        // directly. Graph-backed inputs below resolve the same slots through
        // their normal read preparation instead.
        if owned.is_some() || leading.is_some() || matched.is_none() && unwound_match.is_none() {
            parser
                .syntax
                .element_catalog
                .resolve(&mut *resolve, statement);
            for (key, slot) in &parser.syntax.element_keys {
                slot.resolve(
                    match resolve.resolve_symbol(GraphSymbolKind::Property, key) {
                        Some(GraphSymbol::Property(key)) => Some(key),
                        _ => None,
                    },
                );
            }
        }
        let input = match (owned, leading, matched) {
            (Some(types), _, _) => VertexMergeRowsTextInput::Owned(types),
            (None, Some((leading, pipeline)), _) => {
                VertexMergeRowsTextInput::Relation(Box::new(crate::set_text::BoundSetTextInput {
                    selection: None,
                    parameters: parameters.clone(),
                    parameter_offsets: parameter_offsets.clone(),
                    return_at: statement.len(),
                    projection: None,
                    quantifier: GraphSetQuantifier::All,
                    pipeline,
                    singleton: true,
                    leading,
                    correlations: Vec::new(),
                    row_fields: Vec::new(),
                    owned: None,
                }))
            }
            // The MATCH rows are the ordinary read relation, as after `MATCH
            // ... WITH ... CREATE`.
            (None, None, Some((head, stages))) => {
                let read = parser.finish_write_row_prefix(statement, head, stages)?;
                VertexMergeRowsTextInput::Relation(Box::new(
                    read.resolve(crate::graph_text::LentResolver(&mut *resolve))?,
                ))
            }
            (None, None, None) => match unwound_match {
                // The MERGE and its tail may add parameters and element keys
                // after the read boundary; the read binds the complete table.
                Some(mut source) => {
                    source.syntax.parameters.clone_from(&parameters);
                    source
                        .syntax
                        .parameter_offsets
                        .clone_from(&parameter_offsets);
                    source
                        .syntax
                        .element_keys
                        .extend(parser.syntax.element_keys.iter().cloned());
                    let mut input =
                        source.resolve(crate::graph_text::LentResolver(&mut *resolve))?;
                    input.owned = matched_owned;
                    VertexMergeRowsTextInput::Relation(Box::new(input))
                }
                None => VertexMergeRowsTextInput::Unit,
            },
        };
        Ok((
            Self {
                relation,
                labels,
                properties,
                input,
                keep,
                returning,
                actions,
                parameters,
                parameter_offsets,
                end: statement.len(),
            },
            continuation,
        ))
    }

    pub(crate) fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    /// The column types of the previous write's row this step reads; `None`
    /// for a statement's first write.
    pub(crate) fn owned_column_types(&self) -> Option<&[GraphSetColumnType]> {
        match &self.input {
            VertexMergeRowsTextInput::Owned(types) => Some(types),
            VertexMergeRowsTextInput::Relation(input) => {
                input.owned.as_deref().map(|(_, types)| types.as_slice())
            }
            VertexMergeRowsTextInput::Unit => None,
        }
    }

    /// Bind with exactly the arguments this step names: a chain step that
    /// hands its rows on, or the statement's RETURN.
    pub(crate) fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<crate::GraphWriteChainStep, GraphVertexMergeTextError> {
        use crate::vertex_merge::{GraphVertexMergeRowValue, GraphVertexMergeRowsInput};
        let values = crate::graph_text::checked_parameter_values(
            &self.parameters,
            &self.parameter_offsets,
            self.end,
            arguments,
        )?;
        let mut properties = Vec::with_capacity(self.properties.len());
        for (key, value) in &self.properties {
            properties.push((
                *key,
                match value {
                    RowValueTemplate::Column(column) => GraphVertexMergeRowValue::Column(*column),
                    RowValueTemplate::Computed { value, at } => GraphVertexMergeRowValue::Computed(
                        return_projection::bind_read_value(value, &values).map_err(|error| {
                            GraphVertexMergeTextError {
                                offset: *at,
                                ..GraphVertexMergeTextError::from(error)
                            }
                        })?,
                    ),
                    RowValueTemplate::Value(VertexMergeValueTemplate::Bound(value)) => {
                        GraphVertexMergeRowValue::Bound(value.clone())
                    }
                    RowValueTemplate::Value(VertexMergeValueTemplate::Stored { value, at }) => {
                        GraphVertexMergeRowValue::Computed(
                            return_projection::bind_read_value(value, &values).map_err(
                                |error| GraphVertexMergeTextError {
                                    offset: *at,
                                    ..GraphVertexMergeTextError::from(error)
                                },
                            )?,
                        )
                    }
                    RowValueTemplate::Value(VertexMergeValueTemplate::Parameter { index, at }) => {
                        let value = scalar(values[*index].clone(), *at)?;
                        if matches!(value.value(), CanonicalScalar::Null) {
                            return Err(error(
                                *at,
                                GraphPatternTextErrorKind::Expected(
                                    "non-null MERGE property value",
                                ),
                            )
                            .into());
                        }
                        GraphVertexMergeRowValue::Bound(value)
                    }
                },
            ));
        }
        let input = match &self.input {
            VertexMergeRowsTextInput::Unit => GraphVertexMergeRowsInput::Unit,
            VertexMergeRowsTextInput::Owned(types) => {
                GraphVertexMergeRowsInput::Owned(types.clone())
            }
            VertexMergeRowsTextInput::Relation(input) => {
                GraphVertexMergeRowsInput::Relation(Box::new(input.bind_values(&values)?))
            }
        };
        let bind_actions =
            |templates: &[VertexMergeRowActionTemplate]| bind_row_actions(templates, &values);
        let actions = match &self.actions {
            Some(templates) => Some(Box::new(crate::GraphVertexMergeRowActions {
                on_match: bind_actions(&templates.on_match)?,
                on_create: bind_actions(&templates.on_create)?,
                after: bind_actions(&templates.after)?,
            })),
            None => None,
        };
        let merge = crate::PreparedGraphVertexMergeRows {
            relation: self.relation,
            labels: self.labels.clone(),
            properties,
            input,
            keep: self.keep,
            actions,
        };
        Ok(match &self.returning {
            Some(returning) => crate::GraphWriteChainStep::VertexMergeQuery(Box::new(
                returning.bind(merge, &values)?,
            )),
            None => crate::GraphWriteChainStep::VertexMerge(Box::new(merge)),
        })
    }
}

impl crate::PreparedGraphVertexMergeRows {
    /// The single MERGE of one property tuple (values in property-key order,
    /// as [`Self::tuple`] reads them).
    pub fn merge_for(
        &self,
        tuple: &[CanonicalScalar],
    ) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
        let variable = Name { text: "n", at: 0 };
        let mut resolved = Vec::with_capacity(self.properties.len());
        for ((key, _), value) in self.properties.iter().zip(tuple) {
            let value = crate::GqlScalarParameter::new(value.clone())
                .map_err(|_| error(0, GraphPatternTextErrorKind::ScalarLiteral))?;
            resolved.push((
                *key,
                Some(Number::Literal(GqlParameterValue::Scalar(value.clone()))),
                VertexMergeValueTemplate::Bound(value),
            ));
        }
        let labels: Vec<_> = self.labels.iter().map(|label| (*label, 0)).collect();
        let template = merge_template(
            "",
            self.relation,
            (Vec::new(), Vec::new()),
            variable,
            &labels,
            resolved,
        )?;
        bind_merge_values(&template, &[])
    }

    /// The single MERGE for a scalar or list tuple. Scalar-only callers keep
    /// the existing scalar plan and canonical bytes through merge_for.
    /// A collection's native Boolean predicate stays inside the selection;
    /// creation never substitutes canonical storage identity for query equality.
    pub fn merge_for_stored(
        &self,
        tuple: &[fgdb_types::CanonicalPropertyValue],
    ) -> Result<PreparedGraphVertexMerge, GraphVertexMergeTextError> {
        if tuple.len() != self.properties.len() {
            return Err(error(
                0,
                GraphPatternTextErrorKind::Expected("one MERGE value per property key"),
            )
            .into());
        }
        if tuple
            .iter()
            .any(|value| matches!(value.as_scalar(), Some(CanonicalScalar::Null)))
        {
            return Err(error(
                0,
                GraphPatternTextErrorKind::Expected("non-null MERGE property value"),
            )
            .into());
        }
        if tuple.iter().all(|value| value.as_scalar().is_some()) {
            let scalars = tuple
                .iter()
                .filter_map(|value| value.as_scalar().cloned())
                .collect::<Vec<_>>();
            return self.merge_for(&scalars);
        }
        let mut builder = GraphPatternBuilder::new();
        built(0, builder.vertex("n"))?;
        for label in &self.labels {
            built(0, builder.filter("n", VertexPredicate::HasLabel(*label)))?;
        }
        let mut fields = Vec::with_capacity(tuple.len());
        for ((key, _), value) in self.properties.iter().zip(tuple) {
            fields.push((*key, stored_merge_field(&mut builder, "n", *key, value, 0)?));
        }
        let selection = built(
            0,
            builder.prepare_values(&[GraphColumn::vertex("_merge_target", "n")], 0, None),
        )?;
        let creation = PreparedGraphInsert::prepare_standalone(
            self.relation,
            vec![GraphInsertVertex {
                labels: self.labels.clone(),
                properties: fields,
            }],
            Vec::new(),
        )
        .map_err(|source| insert_error(0, source))?;
        PreparedGraphVertexMerge::prepare(selection, self.relation, 0, creation)
            .map_err(|source| merge_error(0, source))
    }
}

#[cfg(test)]
mod return_framing_tests {
    use super::PreparedGraphVertexUpsertQueryText;

    #[test]
    fn only_a_merge_with_a_top_level_return_routes_here() {
        for (text, expected) in [
            ("MERGE (n:L {k: 1}) RETURN n", true),
            (
                "MERGE (n:L {k: 1}) ON CREATE SET n.p = 1 SET n.q = 2 RETURN n.p",
                true,
            ),
            ("MERGE (n:L {k: 1}) SET n.p = 1", false),
            ("MERGE (n:L {k: 'RETURN'}) SET n.p = 1", false),
            ("MATCH (n) RETURN n", false),
            ("MERGE (n:L {k: 1}); MATCH (m) RETURN m", false),
            ("MERGE (n:L {k: 1}) SET n.RETURN = 1", false),
        ] {
            assert_eq!(
                PreparedGraphVertexUpsertQueryText::has_return_clause(text).unwrap(),
                expected,
                "{text}"
            );
        }
    }
}
